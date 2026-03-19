use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::sync::Mutex;
use std::sync::LazyLock;
use std::time::Instant;

use super::{InputCapture, InputEvent, InputInjector};
use crate::core::protocol::{
    KeyEvent, MouseButton, MouseButtonEvent, MouseMoveEvent, MouseScrollEvent,
};

// --- Global state (mirrors Windows implementation pattern) ---

static SUPPRESS: AtomicBool = AtomicBool::new(false);
static EVENT_SENDER: OnceLock<std::sync::mpsc::Sender<InputEvent>> = OnceLock::new();

/// Virtual cursor position tracking for remote mouse control.
static VIRTUAL_X: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static VIRTUAL_Y: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Track which mouse buttons are currently held using a bitmask.
/// bit 0 (0x01) = left, bit 1 (0x02) = right, bit 2 (0x04) = other/middle.
/// Used to post drag events instead of move events during a drag.
static HELD_BUTTON: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

const HELD_LEFT: u8 = 0x01;
const HELD_RIGHT: u8 = 0x02;
const HELD_OTHER: u8 = 0x04;

/// Remote screen bounds for clamping virtual position.
static REMOTE_LEFT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static REMOTE_TOP: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static REMOTE_RIGHT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1920);
static REMOTE_BOTTOM: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1080);

/// Anchor position to lock the local cursor when suppressed.
/// The cursor is warped back here on every mouse move to prevent visible movement.
static ANCHOR_X: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static ANCHOR_Y: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Whether any peers are currently connected.
/// When false, non-suppressed mouse moves are dropped in the event tap callback
/// entirely — there is nothing to forward them to, and the async wakeups were
/// the primary source of idle CPU usage on macOS.
static PEERS_CONNECTED: AtomicBool = AtomicBool::new(false);

/// Timestamp (ms since epoch) of the last non-suppressed mouse-move sent to the
/// channel. Used to throttle edge-detection events to ~60 Hz when peers are
/// connected but the cursor is not suppressed, preventing the async runtime
/// from being woken 200+ times/second by trackpad events.
static LAST_MOVE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Monotonic epoch for cheap elapsed-ms calculations in the hot event tap path.
static MONO_EPOCH: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);

/// Notify the event tap whether any peers are connected.
/// Call from engine's add_peer / remove_peer so the hot path can bail out early.
pub fn set_peers_connected(connected: bool) {
    PEERS_CONNECTED.store(connected, Ordering::Relaxed);
}

// CGEvent delta fields
const KCG_MOUSE_EVENT_DELTA_X: u32 = 4;
const KCG_MOUSE_EVENT_DELTA_Y: u32 = 5;

// Field to stamp on injected events so the event tap can identify them.
// Unlike an AtomicBool flag, this travels WITH the event through the async
// CGEventPost pipeline, eliminating the race condition.
const KCG_EVENT_SOURCE_USER_DATA: u32 = 42;
const SHAREFLOW_EVENT_MARKER: i64 = 0x53464C57; // "SFLW"

// Click state field — macOS apps ignore clicks with count=0.
const KCG_MOUSE_EVENT_CLICK_STATE: u32 = 1;

// CGEventSource state IDs.
const KCG_EVENT_SOURCE_STATE_COMBINED_SESSION: i32 = 0;
const KCG_EVENT_SOURCE_STATE_HID_SYSTEM: i32 = 1;

pub fn set_suppress(suppress: bool) {
    if suppress {
        // Capture current cursor position as the anchor point.
        // The cursor will be warped back here on every move while suppressed.
        unsafe {
            let event = CGEventCreate(std::ptr::null());
            if !event.is_null() {
                let loc = CGEventGetLocation(event);
                ANCHOR_X.store(loc.x as i32, Ordering::SeqCst);
                ANCHOR_Y.store(loc.y as i32, Ordering::SeqCst);
                CFRelease(event);
            }
        }
    }
    if !suppress {
        // When focus returns to the local Mac, clear any stuck modifier keys.
        // If a modifier key-down was injected (e.g. Control from the remote machine)
        // but the key-up was lost during a screen transition, the HID system
        // thinks the modifier is still held. On macOS, a stuck Control key causes
        // every left-click to become a right-click (Control+Click = Right-Click).
        reset_injected_modifiers();
    }
    SUPPRESS.store(suppress, Ordering::SeqCst);
}

/// Release all injected modifier keys and reset injected modifier flags.
/// Posts synthetic key-up events for all modifier keys to clean up HID state.
fn reset_injected_modifiers() {
    // Atomically get and clear the flags
    let flags = {
        let mut state = MODIFIER_STATE.lock().unwrap_or_else(|e| e.into_inner());
        let f = state.injected_flags;
        state.injected_flags = 0;
        f
    };

    if flags == 0 {
        return;
    }
    log::info!("Resetting stuck modifier flags: 0x{:X}", flags);

    // List of all modifier virtual keycodes to release
    let modifier_vks: &[u16] = &[
        0x38, // Left Shift
        0x3C, // Right Shift
        0x3B, // Left Control
        0x3E, // Right Control
        0x3A, // Left Option
        0x3D, // Right Option
        0x37, // Left Command
        0x36, // Right Command
    ];

    unsafe {
        let source = create_event_source();
        for &vk in modifier_vks {
            if let Some((indep, dep)) = modifier_flags_for_vk(vk) {
                // Only release modifiers that were actually held
                if flags & (indep | dep) != 0 {
                    let event = CGEventCreateKeyboardEvent(source, vk, false);
                    if !event.is_null() {
                        CGEventSetType(event, KCG_EVENT_FLAGS_CHANGED);
                        CGEventSetFlags(event, 0);
                        CGEventSetIntegerValueField(event, KCG_EVENT_SOURCE_USER_DATA, SHAREFLOW_EVENT_MARKER);
                        CGEventPost(KCG_HID_EVENT_TAP, event);
                        CFRelease(event);
                    }
                }
            }
        }
        if !source.is_null() {
            CFRelease(source);
        }
    }
}

/// Initialize remote mouse control: set virtual position to the entry point on the remote screen.
pub fn init_remote_mouse(virtual_x: i32, virtual_y: i32, rs_x: i32, rs_y: i32, rs_w: i32, rs_h: i32) {
    VIRTUAL_X.store(virtual_x, Ordering::SeqCst);
    VIRTUAL_Y.store(virtual_y, Ordering::SeqCst);
    REMOTE_LEFT.store(rs_x, Ordering::SeqCst);
    REMOTE_TOP.store(rs_y, Ordering::SeqCst);
    REMOTE_RIGHT.store(rs_x + rs_w, Ordering::SeqCst);
    REMOTE_BOTTOM.store(rs_y + rs_h, Ordering::SeqCst);
}

// --- CoreGraphics FFI types and functions ---

type CGEventTapProxy = *mut c_void;
type CGEventRef = *mut c_void;
type CFMachPortRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type CFStringRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CGEventMask = u64;
type CGDirectDisplayID = u32;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

// CGEventType constants
const KCG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
const KCG_EVENT_LEFT_MOUSE_UP: u32 = 2;
const KCG_EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
const KCG_EVENT_RIGHT_MOUSE_UP: u32 = 4;
const KCG_EVENT_MOUSE_MOVED: u32 = 5;
const KCG_EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
const KCG_EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
const KCG_EVENT_KEY_DOWN: u32 = 10;
const KCG_EVENT_KEY_UP: u32 = 11;
const KCG_EVENT_FLAGS_CHANGED: u32 = 12;
const KCG_EVENT_SCROLL_WHEEL: u32 = 22;
const KCG_EVENT_OTHER_MOUSE_DOWN: u32 = 25;
const KCG_EVENT_OTHER_MOUSE_UP: u32 = 26;
const KCG_EVENT_OTHER_MOUSE_DRAGGED: u32 = 27;
const KCG_EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFFFFFE;

// CGEventTapLocation
const KCG_HID_EVENT_TAP: u32 = 0;
#[allow(dead_code)]
const KCG_SESSION_EVENT_TAP: u32 = 1;
// CGEventTapPlacement
const KCG_HEAD_INSERT_EVENT_TAP: u32 = 0;
// CGEventTapOptions
const KCG_EVENT_TAP_OPTION_DEFAULT: u32 = 0;

// CGEventField constants
const KCG_MOUSE_EVENT_BUTTON_NUMBER: u32 = 3;
const KCG_KEYBOARD_EVENT_KEYCODE: u32 = 9;
const KCG_SCROLL_WHEEL_EVENT_DELTA_AXIS_1: u32 = 11;
const KCG_SCROLL_WHEEL_EVENT_DELTA_AXIS_2: u32 = 12;

// CGScrollEventUnit
const KCG_SCROLL_EVENT_UNIT_LINE: u32 = 1;

// CGEventFlags for modifier tracking
const KCG_EVENT_FLAG_MASK_SHIFT: u64 = 0x00020000;
const KCG_EVENT_FLAG_MASK_CONTROL: u64 = 0x00040000;
const KCG_EVENT_FLAG_MASK_ALTERNATE: u64 = 0x00080000; // Option/Alt
const KCG_EVENT_FLAG_MASK_COMMAND: u64 = 0x00100000;
const KCG_EVENT_FLAG_MASK_ALPHA_SHIFT: u64 = 0x00010000; // Caps Lock

// NX device-dependent modifier flags (lower 16 bits of CGEventFlags).
// These distinguish left vs right modifier keys.
const NX_DEVICELCTLKEYMASK: u64 = 0x00000001;
const NX_DEVICELSHIFTKEYMASK: u64 = 0x00000002;
const NX_DEVICERSHIFTKEYMASK: u64 = 0x00000004;
const NX_DEVICELCMDKEYMASK: u64 = 0x00000008;
const NX_DEVICERCMDKEYMASK: u64 = 0x00000010;
const NX_DEVICELALTKEYMASK: u64 = 0x00000020;
const NX_DEVICERALTKEYMASK: u64 = 0x00000040;
const NX_DEVICERCTLKEYMASK: u64 = 0x00002000;

type CGEventTapCallBack = extern "C" fn(
    proxy: CGEventTapProxy,
    event_type: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef;

extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: CGEventMask,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;

    fn CFMachPortCreateRunLoopSource(
        allocator: CFAllocatorRef,
        port: CFMachPortRef,
        order: i64,
    ) -> CFRunLoopSourceRef;

    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRun();

    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventGetFlags(event: CGEventRef) -> u64;

    fn CGEventCreateMouseEvent(
        source: *const c_void,
        mouse_type: u32,
        mouse_cursor_position: CGPoint,
        mouse_button: u32,
    ) -> CGEventRef;

    fn CGEventCreateKeyboardEvent(
        source: *const c_void,
        virtual_key: u16,
        key_down: bool,
    ) -> CGEventRef;

    fn CGEventCreateScrollWheelEvent2(
        source: *const c_void,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CGEventRef;

    fn CGEventCreate(source: *const c_void) -> CGEventRef;
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGEventSetDoubleValueField(event: CGEventRef, field: u32, value: f64);
    fn CGEventSetType(event: CGEventRef, event_type: u32);
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGEventPost(tap: u32, event: CGEventRef);
    fn CGEventSourceCreate(state_id: i32) -> *mut c_void;
    fn CFRelease(cf: *const c_void);
    fn CGWarpMouseCursorPosition(new_cursor_position: CGPoint) -> i32;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);

    fn CGGetActiveDisplayList(
        max_displays: u32,
        active_displays: *mut CGDirectDisplayID,
        display_count: *mut u32,
    ) -> i32;
    fn CGDisplayBounds(display: CGDirectDisplayID) -> CGRect;
    fn CGDisplayIsMain(display: CGDirectDisplayID) -> bool;

    fn CGDisplayRegisterReconfigurationCallback(
        callback: unsafe extern "C" fn(display: CGDirectDisplayID, flags: u32, user_info: *mut c_void),
        user_info: *mut c_void,
    ) -> i32;

    fn AXIsProcessTrusted() -> bool;

    static kCFRunLoopCommonModes: CFStringRef;
}

// --- macOS Virtual Keycode <-> PS/2 Scancode mapping ---

/// Convert macOS virtual keycode to PS/2 scancode (used in our protocol).
fn mac_vk_to_scancode(vk: u16) -> u16 {
    match vk {
        0x00 => 0x1E, // A
        0x01 => 0x1F, // S
        0x02 => 0x20, // D
        0x03 => 0x21, // F
        0x04 => 0x23, // H
        0x05 => 0x22, // G
        0x06 => 0x2C, // Z
        0x07 => 0x2D, // X
        0x08 => 0x2E, // C
        0x09 => 0x2F, // V
        0x0B => 0x30, // B
        0x0C => 0x10, // Q
        0x0D => 0x11, // W
        0x0E => 0x12, // E
        0x0F => 0x13, // R
        0x10 => 0x15, // Y
        0x11 => 0x14, // T
        0x12 => 0x02, // 1
        0x13 => 0x03, // 2
        0x14 => 0x04, // 3
        0x15 => 0x05, // 4
        0x16 => 0x07, // 6
        0x17 => 0x06, // 5
        0x18 => 0x0D, // =
        0x19 => 0x0A, // 9
        0x1A => 0x08, // 7
        0x1B => 0x0C, // -
        0x1C => 0x09, // 8
        0x1D => 0x0B, // 0
        0x1E => 0x1B, // ]
        0x1F => 0x18, // O
        0x20 => 0x16, // U
        0x21 => 0x1A, // [
        0x22 => 0x17, // I
        0x23 => 0x19, // P
        0x24 => 0x1C, // Return
        0x25 => 0x26, // L
        0x26 => 0x24, // J
        0x27 => 0x28, // '
        0x28 => 0x25, // K
        0x29 => 0x27, // ;
        0x2A => 0x2B, // backslash
        0x2B => 0x33, // ,
        0x2C => 0x35, // /
        0x2D => 0x31, // N
        0x2E => 0x32, // M
        0x2F => 0x34, // .
        0x30 => 0x0F, // Tab
        0x31 => 0x39, // Space
        0x32 => 0x29, // `
        0x33 => 0x0E, // Backspace
        0x35 => 0x01, // Escape
        0x37 => 0x15B, // Command -> Windows/Super
        0x38 => 0x2A, // Left Shift
        0x39 => 0x3A, // Caps Lock
        0x3A => 0x38, // Left Option -> Left Alt
        0x3B => 0x1D, // Left Control
        0x3C => 0x36, // Right Shift
        0x3D => 0x138, // Right Option -> Right Alt
        0x3E => 0x11D, // Right Control
        0x41 => 0x53, // Keypad .
        0x43 => 0x37, // Keypad *
        0x45 => 0x4E, // Keypad +
        0x47 => 0x45, // Keypad Clear -> Num Lock
        0x4B => 0x135, // Keypad /
        0x4C => 0x11C, // Keypad Enter
        0x4E => 0x4A, // Keypad -
        0x52 => 0x52, // Keypad 0
        0x53 => 0x4F, // Keypad 1
        0x54 => 0x50, // Keypad 2
        0x55 => 0x51, // Keypad 3
        0x56 => 0x4B, // Keypad 4
        0x57 => 0x4C, // Keypad 5
        0x58 => 0x4D, // Keypad 6
        0x59 => 0x47, // Keypad 7
        0x5B => 0x48, // Keypad 8
        0x5C => 0x49, // Keypad 9
        0x60 => 0x3F, // F5
        0x61 => 0x40, // F6
        0x62 => 0x41, // F7
        0x63 => 0x3D, // F3
        0x64 => 0x42, // F8
        0x65 => 0x43, // F9
        0x67 => 0x57, // F11
        0x6D => 0x44, // F10
        0x6F => 0x58, // F12
        0x73 => 0x147, // Home
        0x74 => 0x149, // Page Up
        0x75 => 0x153, // Forward Delete
        0x76 => 0x3E, // F4
        0x77 => 0x14F, // End
        0x78 => 0x3C, // F2
        0x79 => 0x151, // Page Down
        0x7A => 0x3B, // F1
        0x7B => 0x14B, // Left Arrow
        0x7C => 0x14D, // Right Arrow
        0x7D => 0x150, // Down Arrow
        0x7E => 0x148, // Up Arrow
        _ => {
            log::debug!("Unknown macOS VK 0x{:X}, passing through as-is", vk);
            0 // Return 0 (no valid scancode) for unmapped keys
        }
    }
}

/// Convert PS/2 scancode to macOS virtual keycode (for injection).
fn scancode_to_mac_vk(sc: u16) -> Option<u16> {
    match sc {
        0x1E => Some(0x00), // A
        0x1F => Some(0x01), // S
        0x20 => Some(0x02), // D
        0x21 => Some(0x03), // F
        0x23 => Some(0x04), // H
        0x22 => Some(0x05), // G
        0x2C => Some(0x06), // Z
        0x2D => Some(0x07), // X
        0x2E => Some(0x08), // C
        0x2F => Some(0x09), // V
        0x30 => Some(0x0B), // B
        0x10 => Some(0x0C), // Q
        0x11 => Some(0x0D), // W
        0x12 => Some(0x0E), // E
        0x13 => Some(0x0F), // R
        0x15 => Some(0x10), // Y
        0x14 => Some(0x11), // T
        0x02 => Some(0x12), // 1
        0x03 => Some(0x13), // 2
        0x04 => Some(0x14), // 3
        0x05 => Some(0x15), // 4
        0x07 => Some(0x16), // 6
        0x06 => Some(0x17), // 5
        0x0D => Some(0x18), // =
        0x0A => Some(0x19), // 9
        0x08 => Some(0x1A), // 7
        0x0C => Some(0x1B), // -
        0x09 => Some(0x1C), // 8
        0x0B => Some(0x1D), // 0
        0x1B => Some(0x1E), // ]
        0x18 => Some(0x1F), // O
        0x16 => Some(0x20), // U
        0x1A => Some(0x21), // [
        0x17 => Some(0x22), // I
        0x19 => Some(0x23), // P
        0x1C => Some(0x24), // Return
        0x26 => Some(0x25), // L
        0x24 => Some(0x26), // J
        0x28 => Some(0x27), // '
        0x25 => Some(0x28), // K
        0x27 => Some(0x29), // ;
        0x2B => Some(0x2A), // backslash
        0x33 => Some(0x2B), // ,
        0x35 => Some(0x2C), // /
        0x31 => Some(0x2D), // N
        0x32 => Some(0x2E), // M
        0x34 => Some(0x2F), // .
        0x0F => Some(0x30), // Tab
        0x39 => Some(0x31), // Space
        0x29 => Some(0x32), // `
        0x0E => Some(0x33), // Backspace
        0x01 => Some(0x35), // Escape
        0x15B => Some(0x37), // Windows/Super -> Command
        0x2A => Some(0x38), // Left Shift
        0x3A => Some(0x39), // Caps Lock
        0x38 => Some(0x3A), // Left Alt -> Left Option
        0x1D => Some(0x3B), // Left Control
        0x36 => Some(0x3C), // Right Shift
        0x136 => Some(0x3C), // Right Shift (with erroneous extended flag)
        0x138 => Some(0x3D), // Right Alt -> Right Option
        0x11D => Some(0x3E), // Right Control
        0x53 => Some(0x41), // Keypad .
        0x37 => Some(0x43), // Keypad *
        0x4E => Some(0x45), // Keypad +
        0x45 => Some(0x47), // Num Lock -> Keypad Clear
        0x135 => Some(0x4B), // Keypad /
        0x11C => Some(0x4C), // Keypad Enter
        0x4A => Some(0x4E), // Keypad -
        0x52 => Some(0x52), // Keypad 0
        0x4F => Some(0x53), // Keypad 1
        0x50 => Some(0x54), // Keypad 2
        0x51 => Some(0x55), // Keypad 3
        0x4B => Some(0x56), // Keypad 4
        0x4C => Some(0x57), // Keypad 5
        0x4D => Some(0x58), // Keypad 6
        0x47 => Some(0x59), // Keypad 7
        0x48 => Some(0x5B), // Keypad 8
        0x49 => Some(0x5C), // Keypad 9
        0x3F => Some(0x60), // F5
        0x40 => Some(0x61), // F6
        0x41 => Some(0x62), // F7
        0x3D => Some(0x63), // F3
        0x42 => Some(0x64), // F8
        0x43 => Some(0x65), // F9
        0x57 => Some(0x67), // F11
        0x44 => Some(0x6D), // F10
        0x58 => Some(0x6F), // F12
        0x147 => Some(0x73), // Home
        0x149 => Some(0x74), // Page Up
        0x153 => Some(0x75), // Forward Delete
        0x3E => Some(0x76), // F4
        0x14F => Some(0x77), // End
        0x3C => Some(0x78), // F2
        0x151 => Some(0x79), // Page Down
        0x3B => Some(0x7A), // F1
        0x14B => Some(0x7B), // Left Arrow
        0x14D => Some(0x7C), // Right Arrow
        0x150 => Some(0x7D), // Down Arrow
        0x148 => Some(0x7E), // Up Arrow
        _ => None,
    }
}

// --- Event Tap Callback ---

/// Modifier state protected by mutex to prevent races between event tap callback
/// and reset_injected_modifiers() in the async runtime.
struct ModifierState {
    prev_flags: u64,
    injected_flags: u64,
}

static MODIFIER_STATE: LazyLock<Mutex<ModifierState>> = LazyLock::new(|| {
    Mutex::new(ModifierState {
        prev_flags: 0,
        injected_flags: 0,
    })
});

/// Map macOS virtual keycode to (device-independent flag, device-dependent flag).
/// Returns None for non-modifier keys.
fn modifier_flags_for_vk(vk: u16) -> Option<(u64, u64)> {
    match vk {
        0x38 => Some((KCG_EVENT_FLAG_MASK_SHIFT, NX_DEVICELSHIFTKEYMASK)),     // Left Shift
        0x3C => Some((KCG_EVENT_FLAG_MASK_SHIFT, NX_DEVICERSHIFTKEYMASK)),     // Right Shift
        0x3B => Some((KCG_EVENT_FLAG_MASK_CONTROL, NX_DEVICELCTLKEYMASK)),     // Left Control
        0x3E => Some((KCG_EVENT_FLAG_MASK_CONTROL, NX_DEVICERCTLKEYMASK)),     // Right Control
        0x3A => Some((KCG_EVENT_FLAG_MASK_ALTERNATE, NX_DEVICELALTKEYMASK)),   // Left Option
        0x3D => Some((KCG_EVENT_FLAG_MASK_ALTERNATE, NX_DEVICERALTKEYMASK)),   // Right Option
        0x37 => Some((KCG_EVENT_FLAG_MASK_COMMAND, NX_DEVICELCMDKEYMASK)),     // Left Command
        0x36 => Some((KCG_EVENT_FLAG_MASK_COMMAND, NX_DEVICERCMDKEYMASK)),     // Right Command
        _ => None,
    }
}

/// Last known cursor position, updated by move_mouse() and the event tap callback.
/// Used by press_mouse_button() instead of a dummy CGEvent (which returns 0,0).
static LAST_CURSOR_X: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static LAST_CURSOR_Y: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn event_tap_callback(
    _proxy: CGEventTapProxy,
    event_type: u32,
    event: CGEventRef,
    _user_info: *mut c_void,
) -> CGEventRef {
    // Re-enable tap if it was disabled by timeout
    if event_type == KCG_EVENT_TAP_DISABLED_BY_TIMEOUT {
        if let Some(tap) = TAP_REF.get() {
            unsafe { CGEventTapEnable(tap.0, true); }
        }
        return event;
    }

    // Skip our own injected events — identified by a marker field value
    // stamped on the event itself. This is race-free unlike an AtomicBool
    // flag, because CGEventPost is asynchronous.
    unsafe {
        if CGEventGetIntegerValueField(event, KCG_EVENT_SOURCE_USER_DATA) == SHAREFLOW_EVENT_MARKER {
            return event;
        }
    }

    let sender = match EVENT_SENDER.get() {
        Some(s) => s,
        None => return event,
    };

    let suppress = SUPPRESS.load(Ordering::SeqCst);

    unsafe {
        match event_type {
            KCG_EVENT_MOUSE_MOVED
            | KCG_EVENT_LEFT_MOUSE_DRAGGED
            | KCG_EVENT_RIGHT_MOUSE_DRAGGED
            | KCG_EVENT_OTHER_MOUSE_DRAGGED => {
                if suppress {
                    // Suppressed = controlling a remote machine.
                    // Use raw deltas for accurate tracking.
                    let dx = CGEventGetIntegerValueField(event, KCG_MOUSE_EVENT_DELTA_X) as i32;
                    let dy = CGEventGetIntegerValueField(event, KCG_MOUSE_EVENT_DELTA_Y) as i32;
                    if dx != 0 || dy != 0 {
                        let mut vx = VIRTUAL_X.load(Ordering::SeqCst) + dx;
                        let mut vy = VIRTUAL_Y.load(Ordering::SeqCst) + dy;

                        // Clamp to remote screen bounds
                        let left = REMOTE_LEFT.load(Ordering::SeqCst);
                        let top = REMOTE_TOP.load(Ordering::SeqCst);
                        let right = REMOTE_RIGHT.load(Ordering::SeqCst);
                        let bottom = REMOTE_BOTTOM.load(Ordering::SeqCst);
                        vx = vx.clamp(left, right - 1);
                        vy = vy.clamp(top, bottom - 1);

                        VIRTUAL_X.store(vx, Ordering::SeqCst);
                        VIRTUAL_Y.store(vy, Ordering::SeqCst);

                        let _ = sender.send(InputEvent::MouseMove(MouseMoveEvent {
                            x: vx,
                            y: vy,
                        }));
                    }
                    // Warp the cursor back to the anchor point to prevent
                    // visible movement on the local Mac screen. Returning
                    // null_mut() alone only prevents app delivery — the
                    // HID-level cursor has already moved visually.
                    let ax = ANCHOR_X.load(Ordering::SeqCst);
                    let ay = ANCHOR_Y.load(Ordering::SeqCst);
                    CGWarpMouseCursorPosition(CGPoint {
                        x: ax as f64,
                        y: ay as f64,
                    });
                    return std::ptr::null_mut();
                }
                // Not suppressed — cursor moves locally.
                // Only forward to the async runtime if peers are connected
                // (for edge-switch detection) and at most 60 Hz.
                // This is the primary idle-CPU fix: at rest with no peers, the
                // async runtime is never woken by trackpad/mouse events.
                let loc = CGEventGetLocation(event);
                LAST_CURSOR_X.store(loc.x as i32, Ordering::SeqCst);
                LAST_CURSOR_Y.store(loc.y as i32, Ordering::SeqCst);
                if PEERS_CONNECTED.load(Ordering::Relaxed) {
                    let now_ms = MONO_EPOCH.elapsed().as_millis() as u64;
                    let last_ms = LAST_MOVE_MS.load(Ordering::Relaxed);
                    if now_ms.wrapping_sub(last_ms) >= 16 {
                        LAST_MOVE_MS.store(now_ms, Ordering::Relaxed);
                        let _ = sender.send(InputEvent::MouseMove(MouseMoveEvent {
                            x: loc.x as i32,
                            y: loc.y as i32,
                        }));
                    }
                }
            }

            KCG_EVENT_LEFT_MOUSE_DOWN => {
                let _ = sender.send(InputEvent::MouseButton(MouseButtonEvent {
                    button: MouseButton::Left,
                    pressed: true,
                }));
            }
            KCG_EVENT_LEFT_MOUSE_UP => {
                let _ = sender.send(InputEvent::MouseButton(MouseButtonEvent {
                    button: MouseButton::Left,
                    pressed: false,
                }));
            }
            KCG_EVENT_RIGHT_MOUSE_DOWN => {
                let _ = sender.send(InputEvent::MouseButton(MouseButtonEvent {
                    button: MouseButton::Right,
                    pressed: true,
                }));
            }
            KCG_EVENT_RIGHT_MOUSE_UP => {
                let _ = sender.send(InputEvent::MouseButton(MouseButtonEvent {
                    button: MouseButton::Right,
                    pressed: false,
                }));
            }
            KCG_EVENT_OTHER_MOUSE_DOWN => {
                let btn_num = CGEventGetIntegerValueField(event, KCG_MOUSE_EVENT_BUTTON_NUMBER);
                let button = match btn_num {
                    2 => MouseButton::Middle,
                    3 => MouseButton::Button4,
                    4 => MouseButton::Button5,
                    _ => MouseButton::Middle,
                };
                let _ = sender.send(InputEvent::MouseButton(MouseButtonEvent {
                    button,
                    pressed: true,
                }));
            }
            KCG_EVENT_OTHER_MOUSE_UP => {
                let btn_num = CGEventGetIntegerValueField(event, KCG_MOUSE_EVENT_BUTTON_NUMBER);
                let button = match btn_num {
                    2 => MouseButton::Middle,
                    3 => MouseButton::Button4,
                    4 => MouseButton::Button5,
                    _ => MouseButton::Middle,
                };
                let _ = sender.send(InputEvent::MouseButton(MouseButtonEvent {
                    button,
                    pressed: false,
                }));
            }

            KCG_EVENT_SCROLL_WHEEL => {
                let dy = CGEventGetIntegerValueField(event, KCG_SCROLL_WHEEL_EVENT_DELTA_AXIS_1);
                let dx = CGEventGetIntegerValueField(event, KCG_SCROLL_WHEEL_EVENT_DELTA_AXIS_2);
                // Normalize to Windows WHEEL_DELTA convention (120 per notch).
                // Use saturating_mul to prevent i32 overflow on high-res trackpads.
                let _ = sender.send(InputEvent::MouseScroll(MouseScrollEvent {
                    dx: (dx as i32).saturating_mul(120),
                    dy: (dy as i32).saturating_mul(120),
                }));
            }

            KCG_EVENT_KEY_DOWN => {
                let vk = CGEventGetIntegerValueField(event, KCG_KEYBOARD_EVENT_KEYCODE) as u16;
                let scancode = mac_vk_to_scancode(vk);
                let _ = sender.send(InputEvent::Key(KeyEvent {
                    scancode,
                    pressed: true,
                }));
            }
            KCG_EVENT_KEY_UP => {
                let vk = CGEventGetIntegerValueField(event, KCG_KEYBOARD_EVENT_KEYCODE) as u16;
                let scancode = mac_vk_to_scancode(vk);
                let _ = sender.send(InputEvent::Key(KeyEvent {
                    scancode,
                    pressed: false,
                }));
            }

            KCG_EVENT_FLAGS_CHANGED => {
                // Modifier keys don't produce key down/up — they produce flag changes.
                // Detect which modifier changed by comparing with previous flags.
                let flags = CGEventGetFlags(event);
                let vk = CGEventGetIntegerValueField(event, KCG_KEYBOARD_EVENT_KEYCODE) as u16;
                let scancode = mac_vk_to_scancode(vk);

                // Determine if this is a press or release based on flag state
                let pressed = match vk {
                    0x38 | 0x3C => (flags & KCG_EVENT_FLAG_MASK_SHIFT) != 0,
                    0x3B | 0x3E => (flags & KCG_EVENT_FLAG_MASK_CONTROL) != 0,
                    0x3A | 0x3D => (flags & KCG_EVENT_FLAG_MASK_ALTERNATE) != 0,
                    0x37 | 0x36 => (flags & KCG_EVENT_FLAG_MASK_COMMAND) != 0,
                    0x39 => (flags & 0x00010000) != 0, // Caps Lock
                    _ => {
                        // Fall back to flag comparison if specific modifier not recognized
                        let state = MODIFIER_STATE.lock().unwrap_or_else(|e| e.into_inner());
                        flags > state.prev_flags
                    }
                };

                // Update previous flags (use try_lock to avoid blocking event tap)
                if let Ok(mut state) = MODIFIER_STATE.try_lock() {
                    state.prev_flags = flags;
                }

                let _ = sender.send(InputEvent::Key(KeyEvent { scancode, pressed }));
            }

            _ => {}
        }
    }

    // Return null to suppress the event, or the event itself to pass through
    if suppress {
        std::ptr::null_mut()
    } else {
        event
    }
}

/// Wrapper to allow CFMachPortRef (a raw pointer) in a static OnceLock.
/// Safety: The event tap is created once on a single thread and only read
/// afterwards (to re-enable after timeout), so this is safe in practice.
struct TapRef(CFMachPortRef);
unsafe impl Send for TapRef {}
unsafe impl Sync for TapRef {}

/// Global reference to the event tap for re-enabling after timeout.
static TAP_REF: OnceLock<TapRef> = OnceLock::new();

// --- Input Capture ---

pub struct MacOSInputCapture {
    capturing: bool,
}

impl MacOSInputCapture {
    pub fn new() -> Self {
        Self {
            capturing: false,
        }
    }

    /// Create capture and set up the event tap + channel (mirrors Windows pattern).
    pub fn new_with_channel() -> (Self, Option<std::sync::mpsc::Receiver<InputEvent>>) {
        // Check accessibility permission
        let trusted = unsafe { AXIsProcessTrusted() };
        if !trusted {
            log::error!(
                "Accessibility permission not granted. \
                 Go to System Settings > Privacy & Security > Accessibility \
                 and add ShareFlow."
            );
            return (Self::new(), None);
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let _ = EVENT_SENDER.set(tx);

        // Spawn thread for CGEventTap with CFRunLoop
        std::thread::spawn(|| {
            unsafe {
                run_event_tap();
            }
        });

        (
            Self {
                capturing: true,
            },
            Some(rx),
        )
    }
}

unsafe fn run_event_tap() {
    // Events we want to capture
    let event_mask: CGEventMask = (1 << KCG_EVENT_MOUSE_MOVED)
        | (1 << KCG_EVENT_LEFT_MOUSE_DOWN)
        | (1 << KCG_EVENT_LEFT_MOUSE_UP)
        | (1 << KCG_EVENT_RIGHT_MOUSE_DOWN)
        | (1 << KCG_EVENT_RIGHT_MOUSE_UP)
        | (1 << KCG_EVENT_LEFT_MOUSE_DRAGGED)
        | (1 << KCG_EVENT_RIGHT_MOUSE_DRAGGED)
        | (1 << KCG_EVENT_OTHER_MOUSE_DOWN)
        | (1 << KCG_EVENT_OTHER_MOUSE_UP)
        | (1 << KCG_EVENT_OTHER_MOUSE_DRAGGED)
        | (1 << KCG_EVENT_SCROLL_WHEEL)
        | (1 << KCG_EVENT_KEY_DOWN)
        | (1 << KCG_EVENT_KEY_UP)
        | (1 << KCG_EVENT_FLAGS_CHANGED);

    let tap = CGEventTapCreate(
        KCG_HID_EVENT_TAP,
        KCG_HEAD_INSERT_EVENT_TAP,
        KCG_EVENT_TAP_OPTION_DEFAULT,
        event_mask,
        event_tap_callback,
        std::ptr::null_mut(),
    );

    if tap.is_null() {
        log::error!(
            "Failed to create CGEventTap. Ensure Accessibility permission is granted."
        );
        return;
    }

    // Store tap reference for re-enabling after timeout
    let _ = TAP_REF.set(TapRef(tap));

    let run_loop_source =
        CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
    if run_loop_source.is_null() {
        log::error!("Failed to create run loop source for event tap");
        CFRelease(tap);
        return;
    }

    let run_loop = CFRunLoopGetCurrent();
    CFRunLoopAddSource(run_loop, run_loop_source, kCFRunLoopCommonModes);
    CGEventTapEnable(tap, true);

    log::info!("macOS event tap started — capturing input events");
    CFRunLoopRun();

    // Cleanup (won't normally reach here)
    CFRelease(run_loop_source);
    CFRelease(tap);
}

impl InputCapture for MacOSInputCapture {
    fn start_capture(
        &mut self,
        _callback: Box<dyn Fn(InputEvent) + Send>,
    ) -> Result<(), String> {
        self.capturing = true;
        Ok(())
    }

    fn stop_capture(&mut self) -> Result<(), String> {
        self.capturing = false;
        Ok(())
    }

    fn is_capturing(&self) -> bool {
        self.capturing
    }
}

// --- Input Injection ---

/// Whether the HID keyboard system has been primed with a warm-up event.
/// On macOS, CGEventPost to the HID tap can silently drop the first few
/// keyboard events if no physical keyboard activity has occurred since boot.
/// We prime it by posting a harmless Shift key down+up on first use.
static KEYBOARD_PRIMED: AtomicBool = AtomicBool::new(false);

/// Multi-click tracking state for detecting double/triple clicks.
/// macOS synthetic CGEvents must have kCGMouseEventClickState set explicitly;
/// the OS does not auto-detect multi-clicks from timing on injected events.
struct ClickState {
    last_button: Option<MouseButton>,
    last_press_time: Option<Instant>,
    last_x: i32,
    last_y: i32,
    click_count: i64,
}

impl ClickState {
    fn new() -> Self {
        Self {
            last_button: None,
            last_press_time: None,
            last_x: 0,
            last_y: 0,
            click_count: 0,
        }
    }

    /// Compute click count for a new mouse-down event.
    /// Increments if the same button is pressed within the double-click
    /// time window (~500ms) and within a small distance, otherwise resets to 1.
    fn press(&mut self, button: MouseButton, x: i32, y: i32) -> i64 {
        const MULTI_CLICK_TIME_MS: u128 = 500;
        const MULTI_CLICK_DIST: i32 = 5;

        let now = Instant::now();
        let same_button = self.last_button == Some(button);
        let within_time = self
            .last_press_time
            .map(|t| now.duration_since(t).as_millis() < MULTI_CLICK_TIME_MS)
            .unwrap_or(false);
        let within_distance = (x - self.last_x).abs() <= MULTI_CLICK_DIST
            && (y - self.last_y).abs() <= MULTI_CLICK_DIST;

        if same_button && within_time && within_distance {
            self.click_count += 1;
        } else {
            self.click_count = 1;
        }

        self.last_button = Some(button);
        self.last_press_time = Some(now);
        self.last_x = x;
        self.last_y = y;

        self.click_count
    }

    /// Return the current click count (for setting on mouse-up events).
    fn current_count(&self) -> i64 {
        self.click_count.max(1)
    }
}

static CLICK_STATE: OnceLock<Mutex<ClickState>> = OnceLock::new();

fn get_click_state() -> &'static Mutex<ClickState> {
    CLICK_STATE.get_or_init(|| Mutex::new(ClickState::new()))
}

pub struct MacOSInputInjector;

impl MacOSInputInjector {
    pub fn new() -> Self {
        // Verify accessibility permission is available for injection
        let trusted = unsafe { AXIsProcessTrusted() };
        if !trusted {
            log::error!(
                "Accessibility permission not granted — input injection (clicks, keys, scroll) \
                 will NOT work. Go to System Settings > Privacy & Security > Accessibility \
                 and add ShareFlow."
            );
        } else {
            log::info!("Accessibility permission verified for input injection");
            // Prime the keyboard immediately at injector creation.
            Self::prime_keyboard();
        }
        Self
    }

    /// Send a harmless Shift key down+up to warm the HID keyboard event pipeline.
    fn prime_keyboard() {
        if KEYBOARD_PRIMED.swap(true, Ordering::SeqCst) {
            return; // Already primed
        }
        unsafe {
            let source = create_event_source();
            // Shift key (vk 0x38) — produces no visible output.
            let down = CGEventCreateKeyboardEvent(source, 0x38, true);
            if !down.is_null() {
                CGEventSetIntegerValueField(down, KCG_EVENT_SOURCE_USER_DATA, SHAREFLOW_EVENT_MARKER);
                CGEventPost(KCG_HID_EVENT_TAP, down);
                CFRelease(down);
            }
            let up = CGEventCreateKeyboardEvent(source, 0x38, false);
            if !up.is_null() {
                CGEventSetIntegerValueField(up, KCG_EVENT_SOURCE_USER_DATA, SHAREFLOW_EVENT_MARKER);
                CGEventPost(KCG_HID_EVENT_TAP, up);
                CFRelease(up);
            }
            if !source.is_null() {
                CFRelease(source);
            }
            log::info!("HID keyboard primed with warm-up event");
        }
    }
}

/// Re-prime the HID keyboard pipeline for focus transitions.
/// Resets the KEYBOARD_PRIMED flag so that prime_keyboard() will fire again,
/// ensuring the warm-up Shift event is sent each time the local Mac receives
/// focus from a remote peer.  Call this immediately after switch_to_local() on
/// macOS, before the first key injection arrives from the remote machine.
pub fn reprime_keyboard_for_focus() {
    KEYBOARD_PRIMED.store(false, Ordering::SeqCst);
    MacOSInputInjector::prime_keyboard();
}

/// Create a CGEventSource for injection.  Returns null on failure.
/// Uses HIDSystemState so injected events appear to originate from hardware,
/// which is required for reliable click/key/scroll injection on macOS.
unsafe fn create_event_source() -> *mut c_void {
    let source = CGEventSourceCreate(KCG_EVENT_SOURCE_STATE_HID_SYSTEM);
    if source.is_null() {
        log::warn!("CGEventSourceCreate(HIDSystem) returned null, trying CombinedSession");
        let fallback = CGEventSourceCreate(KCG_EVENT_SOURCE_STATE_COMBINED_SESSION);
        if fallback.is_null() {
            log::error!("CGEventSourceCreate failed entirely — check Accessibility permissions");
        }
        return fallback;
    }
    source
}

impl InputInjector for MacOSInputInjector {
    fn move_mouse(&self, x: i32, y: i32) -> Result<(), String> {
        // Compute delta from previous position before updating.
        // Both integer and double delta fields must be set on the synthetic
        // event so apps that read relative deltas (window dragging, Photoshop,
        // 3D viewports) see correct movement rather than zero.
        let dx = x - LAST_CURSOR_X.load(Ordering::SeqCst);
        let dy = y - LAST_CURSOR_Y.load(Ordering::SeqCst);

        LAST_CURSOR_X.store(x, Ordering::SeqCst);
        LAST_CURSOR_Y.store(y, Ordering::SeqCst);
        unsafe {
            let point = CGPoint {
                x: x as f64,
                y: y as f64,
            };
            CGWarpMouseCursorPosition(point);

            // Post a mouse event to re-sync the event stream after warp.
            // Use drag event type when a button is held, otherwise macOS
            // won't show live window dragging.
            let held = HELD_BUTTON.load(Ordering::SeqCst);
            let (event_type, cg_button) = if held & HELD_LEFT != 0 {
                (KCG_EVENT_LEFT_MOUSE_DRAGGED, 0u32)
            } else if held & HELD_RIGHT != 0 {
                (KCG_EVENT_RIGHT_MOUSE_DRAGGED, 1)
            } else if held & HELD_OTHER != 0 {
                (KCG_EVENT_OTHER_MOUSE_DRAGGED, 2)
            } else {
                (KCG_EVENT_MOUSE_MOVED, 0)
            };
            let source = create_event_source();
            let move_event = CGEventCreateMouseEvent(
                source,
                event_type,
                point,
                cg_button,
            );
            if !move_event.is_null() {
                CGEventSetIntegerValueField(move_event, KCG_EVENT_SOURCE_USER_DATA, SHAREFLOW_EVENT_MARKER);
                // Set relative delta fields — required for apps that read deltas
                // instead of absolute position (window drag, creative apps, 3D).
                CGEventSetIntegerValueField(move_event, KCG_MOUSE_EVENT_DELTA_X, dx as i64);
                CGEventSetIntegerValueField(move_event, KCG_MOUSE_EVENT_DELTA_Y, dy as i64);
                CGEventSetDoubleValueField(move_event, KCG_MOUSE_EVENT_DELTA_X, dx as f64);
                CGEventSetDoubleValueField(move_event, KCG_MOUSE_EVENT_DELTA_Y, dy as f64);
                CGEventPost(KCG_HID_EVENT_TAP, move_event);
                CFRelease(move_event);
            }
            if !source.is_null() {
                CFRelease(source);
            }
        }
        Ok(())
    }

    fn press_mouse_button(
        &self,
        button: MouseButton,
        pressed: bool,
    ) -> Result<(), String> {
        unsafe {
            let source = create_event_source();
            // Use tracked cursor position instead of a dummy CGEvent
            // (CGEventGetLocation on a newly-created event returns the position
            // passed to CGEventCreate, not the actual cursor position).
            let pos = CGPoint {
                x: LAST_CURSOR_X.load(Ordering::SeqCst) as f64,
                y: LAST_CURSOR_Y.load(Ordering::SeqCst) as f64,
            };

            let (event_type, cg_button) = match (button, pressed) {
                (MouseButton::Left, true) => (KCG_EVENT_LEFT_MOUSE_DOWN, 0u32),
                (MouseButton::Left, false) => (KCG_EVENT_LEFT_MOUSE_UP, 0),
                (MouseButton::Right, true) => (KCG_EVENT_RIGHT_MOUSE_DOWN, 1),
                (MouseButton::Right, false) => (KCG_EVENT_RIGHT_MOUSE_UP, 1),
                (MouseButton::Middle, true) => (KCG_EVENT_OTHER_MOUSE_DOWN, 2),
                (MouseButton::Middle, false) => (KCG_EVENT_OTHER_MOUSE_UP, 2),
                (MouseButton::Button4, true) => (KCG_EVENT_OTHER_MOUSE_DOWN, 3),
                (MouseButton::Button4, false) => (KCG_EVENT_OTHER_MOUSE_UP, 3),
                (MouseButton::Button5, true) => (KCG_EVENT_OTHER_MOUSE_DOWN, 4),
                (MouseButton::Button5, false) => (KCG_EVENT_OTHER_MOUSE_UP, 4),
            };

            // Track held buttons via bitmask so move_mouse can post drag events
            if pressed {
                let bit = match button {
                    MouseButton::Left => HELD_LEFT,
                    MouseButton::Right => HELD_RIGHT,
                    _ => HELD_OTHER,
                };
                HELD_BUTTON.fetch_or(bit, Ordering::SeqCst);
            } else {
                let bit = match button {
                    MouseButton::Left => HELD_LEFT,
                    MouseButton::Right => HELD_RIGHT,
                    _ => HELD_OTHER,
                };
                HELD_BUTTON.fetch_and(!bit, Ordering::SeqCst);
            }

            let event = CGEventCreateMouseEvent(source, event_type, pos, cg_button);
            if !event.is_null() {
                CGEventSetIntegerValueField(event, KCG_EVENT_SOURCE_USER_DATA, SHAREFLOW_EVENT_MARKER);
                // Explicitly set modifier flags to our tracked state. Without this,
                // CGEventCreateMouseEvent inherits flags from the HID system state
                // which can include a stale Control flag. On macOS, Control+Click
                // is converted to Right-Click by Cocoa, so a stuck Control modifier
                // causes every left click to behave as a right click.
                let mod_flags = {
                    let state = MODIFIER_STATE.lock().unwrap_or_else(|e| e.into_inner());
                    state.injected_flags
                };
                CGEventSetFlags(event, mod_flags);
                // Set click count for multi-click detection (double-click, triple-click).
                // macOS requires this field set explicitly on synthetic events —
                // it does NOT auto-detect multi-clicks from timing on CGEventPost'd events.
                let click_count = {
                    let state = get_click_state();
                    let mut cs = state.lock().unwrap_or_else(|e| e.into_inner());
                    if pressed {
                        cs.press(button, pos.x as i32, pos.y as i32)
                    } else {
                        // mouse-up must carry the same click count as the preceding mouse-down
                        cs.current_count()
                    }
                };
                CGEventSetIntegerValueField(event, KCG_MOUSE_EVENT_CLICK_STATE, click_count);
                // For Other-type mouse buttons, explicitly set the button number field
                if cg_button >= 2 {
                    CGEventSetIntegerValueField(event, KCG_MOUSE_EVENT_BUTTON_NUMBER, cg_button as i64);
                }
                CGEventPost(KCG_HID_EVENT_TAP, event);
                CFRelease(event);
            } else {
                log::error!("CGEventCreateMouseEvent returned null for type={} button={}", event_type, cg_button);
            }
            if !source.is_null() {
                CFRelease(source);
            }
        }
        Ok(())
    }

    fn scroll(&self, dx: i32, dy: i32) -> Result<(), String> {
        unsafe {
            // Convert from WHEEL_DELTA convention (120 per notch) to lines
            let line_dy = if dy.abs() >= 120 { dy / 120 } else { dy.signum() };
            let line_dx = if dx.abs() >= 120 { dx / 120 } else { dx.signum() };
            let source = create_event_source();
            let event = CGEventCreateScrollWheelEvent2(
                source,
                KCG_SCROLL_EVENT_UNIT_LINE,
                2,
                line_dy,
                line_dx,
                0,
            );
            if !event.is_null() {
                CGEventSetIntegerValueField(event, KCG_EVENT_SOURCE_USER_DATA, SHAREFLOW_EVENT_MARKER);
                CGEventPost(KCG_HID_EVENT_TAP, event);
                CFRelease(event);
            } else {
                log::error!("CGEventCreateScrollWheelEvent2 returned null");
            }
            if !source.is_null() {
                CFRelease(source);
            }
        }
        Ok(())
    }

    fn send_key(&self, scancode: u16, pressed: bool) -> Result<(), String> {
        let mac_vk = match scancode_to_mac_vk(scancode) {
            Some(vk) => vk,
            None => {
                log::warn!("Unknown scancode for macOS: 0x{:X}", scancode);
                return Ok(());
            }
        };

        log::debug!(
            "Injecting key: scancode=0x{:X} mac_vk=0x{:X} pressed={}",
            scancode, mac_vk, pressed
        );

        // Caps Lock (vk 0x39) requires special handling on macOS:
        // It uses kCGEventFlagsChanged (type 12) with the alpha-shift flag,
        // not regular key down/up events. Without this, Caps Lock acts as a
        // held modifier instead of a latching toggle.
        if mac_vk == 0x39 {
            return self.inject_caps_lock(pressed);
        }

        // Modifier keys (Shift, Control, Option, Command) use
        // kCGEventFlagsChanged on macOS, not regular key down/up.
        // Without this, apps ignore the modifier state entirely.
        if modifier_flags_for_vk(mac_vk).is_some() {
            return self.inject_modifier(mac_vk, pressed);
        }

        unsafe {
            let source = create_event_source();
            let event = CGEventCreateKeyboardEvent(source, mac_vk, pressed);
            if !event.is_null() {
                CGEventSetIntegerValueField(event, KCG_EVENT_SOURCE_USER_DATA, SHAREFLOW_EVENT_MARKER);
                // Apply currently held modifier flags so modified key combos
                // (e.g. Shift+A) carry the correct flag state.
                let mod_flags = {
                    let state = MODIFIER_STATE.lock().unwrap_or_else(|e| e.into_inner());
                    state.injected_flags
                };
                if mod_flags != 0 {
                    CGEventSetFlags(event, mod_flags);
                }
                CGEventPost(KCG_HID_EVENT_TAP, event);
                CFRelease(event);
            } else {
                log::error!("CGEventCreateKeyboardEvent returned null for vk=0x{:X}", mac_vk);
            }
            if !source.is_null() {
                CFRelease(source);
            }
        }
        Ok(())
    }
}

impl MacOSInputInjector {
    /// Inject a modifier key (Shift, Control, Option, Command) as a
    /// kCGEventFlagsChanged event. macOS apps expect modifiers to arrive
    /// as flag-change events — regular key down/up events are ignored for
    /// modifier keys by most Cocoa applications.
    fn inject_modifier(&self, mac_vk: u16, pressed: bool) -> Result<(), String> {
        let (indep_flag, dep_flag) = modifier_flags_for_vk(mac_vk)
            .expect("inject_modifier called for non-modifier vk");

        // Update cumulative flags atomically under lock to prevent races
        let flags = {
            let mut state = MODIFIER_STATE.lock().unwrap_or_else(|e| e.into_inner());
            if pressed {
                state.injected_flags |= indep_flag | dep_flag;
            } else {
                // Clear the device-dependent flag. Only clear the device-independent
                // flag if no other key sharing it is still held (e.g. Left Shift
                // released while Right Shift is still down).
                state.injected_flags &= !(dep_flag);
                // Check if any device-dependent bit for the same modifier family remains.
                let still_held = match indep_flag {
                    KCG_EVENT_FLAG_MASK_SHIFT => state.injected_flags & (NX_DEVICELSHIFTKEYMASK | NX_DEVICERSHIFTKEYMASK) != 0,
                    KCG_EVENT_FLAG_MASK_CONTROL => state.injected_flags & (NX_DEVICELCTLKEYMASK | NX_DEVICERCTLKEYMASK) != 0,
                    KCG_EVENT_FLAG_MASK_ALTERNATE => state.injected_flags & (NX_DEVICELALTKEYMASK | NX_DEVICERALTKEYMASK) != 0,
                    KCG_EVENT_FLAG_MASK_COMMAND => state.injected_flags & (NX_DEVICELCMDKEYMASK | NX_DEVICERCMDKEYMASK) != 0,
                    _ => false,
                };
                if !still_held {
                    state.injected_flags &= !(indep_flag);
                }
            }
            state.injected_flags
        };

        unsafe {
            let source = create_event_source();
            let event = CGEventCreateKeyboardEvent(source, mac_vk, pressed);
            if !event.is_null() {
                CGEventSetType(event, KCG_EVENT_FLAGS_CHANGED);
                CGEventSetFlags(event, flags);
                CGEventSetIntegerValueField(event, KCG_EVENT_SOURCE_USER_DATA, SHAREFLOW_EVENT_MARKER);
                CGEventPost(KCG_HID_EVENT_TAP, event);
                CFRelease(event);
            } else {
                log::error!("CGEventCreateKeyboardEvent returned null for modifier vk=0x{:X}", mac_vk);
            }
            if !source.is_null() {
                CFRelease(source);
            }
        }
        Ok(())
    }

    /// Inject Caps Lock toggle using a kCGEventFlagsChanged event.
    /// On macOS, Caps Lock doesn't use normal key down/up — it toggles via
    /// a flags-changed event with the alpha-shift bit set/cleared.
    /// We only act on key-down (pressed=true) and perform a full toggle cycle,
    /// since Windows sends separate down/up but macOS toggles on a single event.
    fn inject_caps_lock(&self, pressed: bool) -> Result<(), String> {
        // Only toggle on key-down; ignore key-up to avoid double-toggling.
        if !pressed {
            return Ok(());
        }

        unsafe {
            let source = create_event_source();
            // Create a keyboard event for Caps Lock (vk 0x39), then change its
            // type to kCGEventFlagsChanged and set the alpha-shift flag.
            let down = CGEventCreateKeyboardEvent(source, 0x39, true);
            if !down.is_null() {
                CGEventSetType(down, KCG_EVENT_FLAGS_CHANGED);
                CGEventSetFlags(down, KCG_EVENT_FLAG_MASK_ALPHA_SHIFT);
                CGEventSetIntegerValueField(down, KCG_EVENT_SOURCE_USER_DATA, SHAREFLOW_EVENT_MARKER);
                CGEventPost(KCG_HID_EVENT_TAP, down);
                CFRelease(down);
            }
            // Post the release (flags cleared) to complete the toggle cycle.
            let up = CGEventCreateKeyboardEvent(source, 0x39, false);
            if !up.is_null() {
                CGEventSetType(up, KCG_EVENT_FLAGS_CHANGED);
                CGEventSetFlags(up, 0);
                CGEventSetIntegerValueField(up, KCG_EVENT_SOURCE_USER_DATA, SHAREFLOW_EVENT_MARKER);
                CGEventPost(KCG_HID_EVENT_TAP, up);
                CFRelease(up);
            }
            if !source.is_null() {
                CFRelease(source);
            }
        }
        Ok(())
    }
}

// --- Screen Detection ---

pub fn get_screens_macos() -> Vec<crate::core::protocol::ScreenInfo> {
    let mut displays: [CGDirectDisplayID; 32] = [0; 32];
    let mut count: u32 = 0;

    unsafe {
        let result = CGGetActiveDisplayList(32, displays.as_mut_ptr(), &mut count);
        if result != 0 {
            log::error!("CGGetActiveDisplayList failed: {}", result);
            return vec![crate::core::protocol::ScreenInfo {
                id: "main".to_string(),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                primary: true,
            }];
        }
    }

    let mut screens = Vec::new();
    for i in 0..count as usize {
        let display_id = displays[i];
        unsafe {
            let bounds = CGDisplayBounds(display_id);
            let is_main = CGDisplayIsMain(display_id);

            screens.push(crate::core::protocol::ScreenInfo {
                id: format!("display-{}", display_id),
                x: bounds.origin.x as i32,
                y: bounds.origin.y as i32,
                width: bounds.size.width as i32,
                height: bounds.size.height as i32,
                primary: is_main,
            });
        }
    }

    if screens.is_empty() {
        screens.push(crate::core::protocol::ScreenInfo {
            id: "main".to_string(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        });
    }

    screens
}

// --- Display reconfiguration monitoring ---

/// Global channel to notify the async runtime when displays change.
static DISPLAY_CHANGE_SENDER: OnceLock<std::sync::mpsc::Sender<()>> = OnceLock::new();

/// CGDisplayReconfigurationCallback — fires on display connect/disconnect/resize/wake.
/// The kCGDisplayBeginConfigurationFlag (1 << 0) fires at the START of the change;
/// we only act once the reconfiguration is complete (flag not set).
unsafe extern "C" fn display_reconfig_callback(
    _display: CGDirectDisplayID,
    flags: u32,
    _user_info: *mut c_void,
) {
    const K_CG_DISPLAY_BEGIN_CONFIGURATION_FLAG: u32 = 1;
    if flags & K_CG_DISPLAY_BEGIN_CONFIGURATION_FLAG != 0 {
        return; // Reconfiguration starting — wait for the completion callback.
    }
    if let Some(tx) = DISPLAY_CHANGE_SENDER.get() {
        let _ = tx.send(());
    }
}

/// Start monitoring for display configuration changes (resolution, wake, etc.).
/// Returns a receiver that fires whenever displays are reconfigured.
pub fn start_display_change_monitor() -> std::sync::mpsc::Receiver<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = DISPLAY_CHANGE_SENDER.set(tx);

    unsafe {
        CGDisplayRegisterReconfigurationCallback(display_reconfig_callback, std::ptr::null_mut());
    }
    log::info!("Display reconfiguration monitor registered");

    rx
}
