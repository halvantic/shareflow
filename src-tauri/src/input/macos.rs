use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

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

/// Remote screen bounds for clamping virtual position.
static REMOTE_LEFT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static REMOTE_TOP: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static REMOTE_RIGHT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1920);
static REMOTE_BOTTOM: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1080);

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
    SUPPRESS.store(suppress, Ordering::SeqCst);
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
        _ => vk, // Pass through unknown
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

/// Track previous modifier flags for detecting individual modifier key changes.
static mut PREV_FLAGS: u64 = 0;

extern "C" fn event_tap_callback(
    _proxy: CGEventTapProxy,
    event_type: u32,
    event: CGEventRef,
    _user_info: *mut c_void,
) -> CGEventRef {
    // Re-enable tap if it was disabled by timeout
    if event_type == KCG_EVENT_TAP_DISABLED_BY_TIMEOUT {
        unsafe {
            if let Some(tap) = TAP_REF.as_ref() {
                CGEventTapEnable(*tap, true);
            }
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
                    // Use raw deltas for accurate tracking when cursor is suppressed
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
                    return std::ptr::null_mut();
                }
                let loc = CGEventGetLocation(event);
                let _ = sender.send(InputEvent::MouseMove(MouseMoveEvent {
                    x: loc.x as i32,
                    y: loc.y as i32,
                }));
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
                // Normalize to Windows WHEEL_DELTA convention (120 per notch)
                let _ = sender.send(InputEvent::MouseScroll(MouseScrollEvent {
                    dx: dx as i32 * 120,
                    dy: dy as i32 * 120,
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
                    _ => flags > PREV_FLAGS,
                };

                PREV_FLAGS = flags;
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

/// Global reference to the event tap for re-enabling after timeout.
static mut TAP_REF: Option<CFMachPortRef> = None;

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
    TAP_REF = Some(tap);

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
        }
        Self
    }
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
        unsafe {
            let point = CGPoint {
                x: x as f64,
                y: y as f64,
            };
            CGWarpMouseCursorPosition(point);

            // Post a mouse-moved event to re-sync the event stream after warp.
            // Without this, macOS dissociates cursor and event state, causing
            // subsequent click/key events to silently fail.
            let source = create_event_source();
            let move_event = CGEventCreateMouseEvent(
                source,
                KCG_EVENT_MOUSE_MOVED,
                point,
                0,
            );
            if !move_event.is_null() {
                CGEventSetIntegerValueField(move_event, KCG_EVENT_SOURCE_USER_DATA, SHAREFLOW_EVENT_MARKER);
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
            // Get actual current cursor position using a generic event
            let source = create_event_source();
            let dummy = CGEventCreate(source);
            let pos = if !dummy.is_null() {
                let p = CGEventGetLocation(dummy);
                CFRelease(dummy);
                p
            } else {
                CGPoint { x: 0.0, y: 0.0 }
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

            let event = CGEventCreateMouseEvent(source, event_type, pos, cg_button);
            if !event.is_null() {
                CGEventSetIntegerValueField(event, KCG_EVENT_SOURCE_USER_DATA, SHAREFLOW_EVENT_MARKER);
                // Set click count to 1 — some macOS apps ignore clicks with count=0
                if pressed {
                    CGEventSetIntegerValueField(event, KCG_MOUSE_EVENT_CLICK_STATE, 1);
                }
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

        unsafe {
            // Use CombinedSession source for keyboard — more reliable than HIDSystem
            // when no physical keyboard activity has occurred on this Mac yet.
            let source = CGEventSourceCreate(KCG_EVENT_SOURCE_STATE_COMBINED_SESSION);
            let event = CGEventCreateKeyboardEvent(source, mac_vk, pressed);
            if !event.is_null() {
                CGEventSetIntegerValueField(event, KCG_EVENT_SOURCE_USER_DATA, SHAREFLOW_EVENT_MARKER);
                // Post to session tap (not HID tap). Session-level injection is
                // more reliable — it doesn't require a fully warmed HID state and
                // avoids the "need to wake keyboard first" issue.
                CGEventPost(KCG_SESSION_EVENT_TAP, event);
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
