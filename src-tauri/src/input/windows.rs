use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::OnceLock;

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    VIRTUAL_KEY,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, GetSystemMetrics, PostThreadMessageW, SetCursorPos,
    SetWindowsHookExW, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSLLHOOKSTRUCT, MSG,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
    WM_MBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SYSKEYDOWN, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

use super::{InputCapture, InputEvent, InputInjector};
use crate::core::protocol::{
    KeyEvent, MouseButton, MouseButtonEvent, MouseMoveEvent, MouseScrollEvent,
};

// Global state for the hook callbacks (Windows hooks require static/global context).
static HOOK_ACTIVE: AtomicBool = AtomicBool::new(false);
/// When true, captured events are NOT passed to the local OS.
static SUPPRESS: AtomicBool = AtomicBool::new(false);
/// Channel sender for forwarding events from hooks to the async runtime.
static EVENT_SENDER: OnceLock<std_mpsc::Sender<InputEvent>> = OnceLock::new();
/// Thread ID of the hook thread, needed to post WM_QUIT to stop it.
static HOOK_THREAD_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Marker value set in dwExtraInfo to identify our own synthetic events.
const SHAREFLOW_EXTRA_INFO: usize = 0x53464C57;

/// Track whether we've logged the first keyboard event (confirms hooks work).
static FIRST_KEY_LOGGED: AtomicBool = AtomicBool::new(false);

/// Track whether the Win key was pressed while suppressing, so we eat the
/// matching key-up even if suppress turns off between down and up — a bare
/// Win key-up reaching the shell opens the Start Menu.
static WIN_KEY_SUPPRESSED: AtomicBool = AtomicBool::new(false);

/// Virtual cursor position tracking for warp-to-center remote mouse control.
static VIRTUAL_X: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static VIRTUAL_Y: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static WARP_CENTER_X: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static WARP_CENTER_Y: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Remote screen bounds for clamping virtual position.
static REMOTE_LEFT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static REMOTE_TOP: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static REMOTE_RIGHT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1920);
static REMOTE_BOTTOM: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1080);

pub struct WindowsInputCapture {
    thread_handle: Option<std::thread::JoinHandle<()>>,
    event_receiver: Option<std_mpsc::Receiver<InputEvent>>,
}

impl WindowsInputCapture {
    pub fn new() -> Self {
        Self {
            thread_handle: None,
            event_receiver: None,
        }
    }
}

impl InputCapture for WindowsInputCapture {
    fn start_capture(
        &mut self,
        _callback: Box<dyn Fn(InputEvent) + Send>,
    ) -> Result<(), String> {
        if HOOK_ACTIVE.load(Ordering::SeqCst) {
            return Err("Already capturing".into());
        }

        // Create a std channel for hook → async bridge.
        let (tx, rx) = std_mpsc::channel();
        let _ = EVENT_SENDER.set(tx);
        self.event_receiver = Some(rx);

        HOOK_ACTIVE.store(true, Ordering::SeqCst);

        let handle = std::thread::spawn(|| {
            // Store thread ID so we can post WM_QUIT to stop cleanly.
            let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            HOOK_THREAD_ID.store(tid, Ordering::SeqCst);

            unsafe {
                let mouse_hook =
                    SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), None, 0)
                        .expect("Failed to set mouse hook");
                let kb_hook =
                    SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), None, 0)
                        .expect("Failed to set keyboard hook");

                log::info!("Low-level hooks installed on thread {}", tid);

                // Message loop — required for low-level hooks.
                let mut msg = MSG::default();
                loop {
                    let ret = GetMessageW(&mut msg, None, 0, 0);
                    if !ret.as_bool() {
                        break; // WM_QUIT received
                    }
                }

                // Cleanup
                let _ = UnhookWindowsHookEx(mouse_hook);
                let _ = UnhookWindowsHookEx(kb_hook);
                log::info!("Hooks removed");
            }
        });

        self.thread_handle = Some(handle);
        log::info!("Windows input capture started");
        Ok(())
    }

    fn stop_capture(&mut self) -> Result<(), String> {
        HOOK_ACTIVE.store(false, Ordering::SeqCst);
        SUPPRESS.store(false, Ordering::SeqCst);
        WIN_KEY_SUPPRESSED.store(false, Ordering::SeqCst);

        // Post WM_QUIT to the hook thread to break its message loop.
        let tid = HOOK_THREAD_ID.load(Ordering::SeqCst);
        if tid != 0 {
            unsafe {
                let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }

        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }

        log::info!("Windows input capture stopped");
        Ok(())
    }

    fn is_capturing(&self) -> bool {
        HOOK_ACTIVE.load(Ordering::SeqCst)
    }
}

impl WindowsInputCapture {
    /// Take the event receiver channel.
    /// Used by the async runtime to receive input events from hooks.
    pub fn take_event_receiver(&mut self) -> Option<std_mpsc::Receiver<InputEvent>> {
        self.event_receiver.take()
    }
}

/// Enable or disable input suppression.
/// When suppressing, captured events are consumed and not passed to the local OS.
pub fn set_suppress(suppress: bool) {
    let prev = SUPPRESS.swap(suppress, Ordering::SeqCst);
    if prev != suppress {
        crate::diag(format!("Input suppression: {} → {}", prev, suppress));
    }
}

#[allow(dead_code)]
pub fn is_suppressing() -> bool {
    SUPPRESS.load(Ordering::SeqCst)
}

/// Initialize remote mouse control: set virtual position and warp cursor to screen center.
pub fn init_remote_mouse(virtual_x: i32, virtual_y: i32, rs_x: i32, rs_y: i32, rs_w: i32, rs_h: i32) {
    VIRTUAL_X.store(virtual_x, Ordering::SeqCst);
    VIRTUAL_Y.store(virtual_y, Ordering::SeqCst);
    REMOTE_LEFT.store(rs_x, Ordering::SeqCst);
    REMOTE_TOP.store(rs_y, Ordering::SeqCst);
    REMOTE_RIGHT.store(rs_x + rs_w, Ordering::SeqCst);
    REMOTE_BOTTOM.store(rs_y + rs_h, Ordering::SeqCst);
    unsafe {
        let screen_w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let screen_h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        let virt_x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let virt_y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let cx = virt_x + screen_w / 2;
        let cy = virt_y + screen_h / 2;
        WARP_CENTER_X.store(cx, Ordering::SeqCst);
        WARP_CENTER_Y.store(cy, Ordering::SeqCst);
        warp_cursor_to_center(cx, cy);
    }
}

unsafe fn warp_cursor_to_center(cx: i32, cy: i32) {
    // SetCursorPos uses exact pixel coordinates — no rounding errors.
    // Unlike SendInput with MOUSEEVENTF_ABSOLUTE (which normalizes to 0-65535
    // and can land 1px off), SetCursorPos is pixel-perfect.
    let _ = SetCursorPos(cx, cy);
}

unsafe extern "system" fn mouse_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code >= 0 && HOOK_ACTIVE.load(Ordering::SeqCst) {
        let data = &*(lparam.0 as *const MSLLHOOKSTRUCT);

        // Skip our own synthetic events (warp, injection) — let them pass
        // through to the OS but don't send to engine
        if data.dwExtraInfo == SHAREFLOW_EXTRA_INFO {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        let suppress = SUPPRESS.load(Ordering::SeqCst);

        // Warp-to-center mouse tracking when controlling remote machine
        if suppress && wparam.0 as u32 == WM_MOUSEMOVE {
            let cx = WARP_CENTER_X.load(Ordering::SeqCst);
            let cy = WARP_CENTER_Y.load(Ordering::SeqCst);
            let dx = data.pt.x - cx;
            let dy = data.pt.y - cy;

            if dx != 0 || dy != 0 {
                let mut vx = VIRTUAL_X.load(Ordering::SeqCst) + dx;
                let mut vy = VIRTUAL_Y.load(Ordering::SeqCst) + dy;

                // Clamp to remote screen bounds to prevent drift
                let left = REMOTE_LEFT.load(Ordering::SeqCst);
                let top = REMOTE_TOP.load(Ordering::SeqCst);
                let right = REMOTE_RIGHT.load(Ordering::SeqCst);
                let bottom = REMOTE_BOTTOM.load(Ordering::SeqCst);
                vx = vx.clamp(left, right - 1);
                vy = vy.clamp(top, bottom - 1);

                VIRTUAL_X.store(vx, Ordering::SeqCst);
                VIRTUAL_Y.store(vy, Ordering::SeqCst);

                if let Some(tx) = EVENT_SENDER.get() {
                    let _ = tx.send(InputEvent::MouseMove(MouseMoveEvent {
                        x: vx,
                        y: vy,
                    }));
                }

                warp_cursor_to_center(cx, cy);
            }

            return LRESULT(1);
        }

        let event = match wparam.0 as u32 {
            WM_MOUSEMOVE => Some(InputEvent::MouseMove(MouseMoveEvent {
                x: data.pt.x,
                y: data.pt.y,
            })),
            WM_LBUTTONDOWN => Some(InputEvent::MouseButton(MouseButtonEvent {
                button: MouseButton::Left,
                pressed: true,
            })),
            WM_LBUTTONUP => Some(InputEvent::MouseButton(MouseButtonEvent {
                button: MouseButton::Left,
                pressed: false,
            })),
            WM_RBUTTONDOWN => Some(InputEvent::MouseButton(MouseButtonEvent {
                button: MouseButton::Right,
                pressed: true,
            })),
            WM_RBUTTONUP => Some(InputEvent::MouseButton(MouseButtonEvent {
                button: MouseButton::Right,
                pressed: false,
            })),
            WM_MBUTTONDOWN => Some(InputEvent::MouseButton(MouseButtonEvent {
                button: MouseButton::Middle,
                pressed: true,
            })),
            WM_MBUTTONUP => Some(InputEvent::MouseButton(MouseButtonEvent {
                button: MouseButton::Middle,
                pressed: false,
            })),
            WM_MOUSEWHEEL => {
                let delta = (data.mouseData >> 16) as i16 as i32;
                Some(InputEvent::MouseScroll(MouseScrollEvent { dx: 0, dy: delta }))
            }
            WM_XBUTTONDOWN => {
                let xbutton = (data.mouseData >> 16) as u16;
                let button = if xbutton == 1 {
                    MouseButton::Button4
                } else {
                    MouseButton::Button5
                };
                Some(InputEvent::MouseButton(MouseButtonEvent {
                    button,
                    pressed: true,
                }))
            }
            WM_XBUTTONUP => {
                let xbutton = (data.mouseData >> 16) as u16;
                let button = if xbutton == 1 {
                    MouseButton::Button4
                } else {
                    MouseButton::Button5
                };
                Some(InputEvent::MouseButton(MouseButtonEvent {
                    button,
                    pressed: false,
                }))
            }
            _ => None,
        };

        if let Some(event) = event {
            if let Some(tx) = EVENT_SENDER.get() {
                let _ = tx.send(event);
            }

            if suppress {
                return LRESULT(1);
            }
        }
    }

    CallNextHookEx(None, code, wparam, lparam)
}

unsafe extern "system" fn keyboard_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code >= 0 && HOOK_ACTIVE.load(Ordering::SeqCst) {
        let data = &*(lparam.0 as *const KBDLLHOOKSTRUCT);

        // Skip our own synthetic events — let them pass through but don't send to engine
        if data.dwExtraInfo == SHAREFLOW_EXTRA_INFO {
            return CallNextHookEx(None, code, wparam, lparam);
        }

        let pressed = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);

        // Log the first keyboard event to confirm hooks are active.
        if !FIRST_KEY_LOGGED.swap(true, Ordering::Relaxed) {
            crate::diag(format!("Keyboard hook active — first key event (vk=0x{:X})", data.vkCode));
        }

        // Handle extended scancodes (arrow keys, Windows key, Right Ctrl/Alt, etc.)
        let mut scancode = data.scanCode as u16;
        if (data.flags.0 & 1) != 0 {
            // LLKHF_EXTENDED flag — set bit 8 for our protocol
            scancode |= 0x100;
        }

        // Win key (VK_LWIN=0x5B / VK_RWIN=0x5C) — must suppress both down AND
        // up to prevent the shell from opening the Start Menu.  Track across
        // suppress-mode transitions so a key-up after suppress turns off is
        // still eaten if the matching key-down was suppressed.
        let is_win_key = data.vkCode == 0x5B || data.vkCode == 0x5C;
        if is_win_key {
            if SUPPRESS.load(Ordering::SeqCst) {
                if pressed {
                    WIN_KEY_SUPPRESSED.store(true, Ordering::SeqCst);
                }
                // Always forward the event to the engine, then suppress
                let event = InputEvent::Key(KeyEvent { scancode, pressed });
                if let Some(tx) = EVENT_SENDER.get() {
                    let _ = tx.send(event);
                }
                if !pressed {
                    WIN_KEY_SUPPRESSED.store(false, Ordering::SeqCst);
                }
                return LRESULT(1);
            } else if !pressed && WIN_KEY_SUPPRESSED.load(Ordering::SeqCst) {
                // Suppress turned off between Win-down and Win-up — still
                // eat the up so the shell doesn't see a bare key-up.
                WIN_KEY_SUPPRESSED.store(false, Ordering::SeqCst);
                let event = InputEvent::Key(KeyEvent { scancode, pressed });
                if let Some(tx) = EVENT_SENDER.get() {
                    let _ = tx.send(event);
                }
                return LRESULT(1);
            }
        }

        let event = InputEvent::Key(KeyEvent {
            scancode,
            pressed,
        });

        if let Some(tx) = EVENT_SENDER.get() {
            let _ = tx.send(event);
        }

        if SUPPRESS.load(Ordering::SeqCst) {
            return LRESULT(1);
        }
    }

    CallNextHookEx(None, code, wparam, lparam)
}

// --- Input Injection ---

pub struct WindowsInputInjector;

impl WindowsInputInjector {
    pub fn new() -> Self {
        Self
    }
}

impl InputInjector for WindowsInputInjector {
    fn move_mouse(&self, x: i32, y: i32) -> Result<(), String> {
        // SetCursorPos is pixel-perfect — no 0-65535 normalization rounding.
        unsafe {
            let _ = SetCursorPos(x, y);
        }
        Ok(())
    }

    fn press_mouse_button(&self, button: MouseButton, pressed: bool) -> Result<(), String> {
        let flags = match (button, pressed) {
            (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
            (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
            (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
            (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
            (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
            (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
            (MouseButton::Button4, true) => MOUSEEVENTF_XDOWN,
            (MouseButton::Button4, false) => MOUSEEVENTF_XUP,
            (MouseButton::Button5, true) => MOUSEEVENTF_XDOWN,
            (MouseButton::Button5, false) => MOUSEEVENTF_XUP,
        };

        let mouse_data = match button {
            MouseButton::Button4 => 1,
            MouseButton::Button5 => 2,
            _ => 0,
        };

        unsafe {
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0,
                        dy: 0,
                        mouseData: mouse_data,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: SHAREFLOW_EXTRA_INFO,
                    },
                },
            };
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }
        Ok(())
    }

    fn scroll(&self, dx: i32, dy: i32) -> Result<(), String> {
        unsafe {
            if dy != 0 {
                let input = INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: 0,
                            dy: 0,
                            mouseData: dy as u32,
                            dwFlags: MOUSEEVENTF_WHEEL,
                            time: 0,
                            dwExtraInfo: SHAREFLOW_EXTRA_INFO,
                        },
                    },
                };
                SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            }
            if dx != 0 {
                let input = INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 {
                        mi: MOUSEINPUT {
                            dx: 0,
                            dy: 0,
                            mouseData: dx as u32,
                            dwFlags: MOUSEEVENTF_HWHEEL,
                            time: 0,
                            dwExtraInfo: SHAREFLOW_EXTRA_INFO,
                        },
                    },
                };
                SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            }
        }
        Ok(())
    }

    fn send_key(&self, scancode: u16, pressed: bool) -> Result<(), String> {
        // Toggle keys (Caps/Num/Scroll Lock) must be injected via virtual key
        // code rather than scancode. KEYEVENTF_SCANCODE bypasses the OS
        // toggle-state logic, causing them to act as held modifiers instead
        // of latching toggles.
        let toggle_vk: Option<u16> = match scancode {
            0x3A => Some(0x14),  // Caps Lock   → VK_CAPITAL
            0x45 => Some(0x90),  // Num Lock    → VK_NUMLOCK
            0x46 => Some(0x91),  // Scroll Lock → VK_SCROLL
            _ => None,
        };

        if let Some(vk) = toggle_vk {
            let mut flags = Default::default();
            if !pressed {
                flags |= KEYEVENTF_KEYUP;
            }
            unsafe {
                let input = INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VIRTUAL_KEY(vk),
                            wScan: 0,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: SHAREFLOW_EXTRA_INFO,
                        },
                    },
                };
                SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            }
            return Ok(());
        }

        let mut flags = KEYEVENTF_SCANCODE;
        if !pressed {
            flags |= KEYEVENTF_KEYUP;
        }

        // Handle extended scancodes (bit 8 set = extended key)
        let actual_scan = if scancode > 0xFF {
            flags |= KEYEVENTF_EXTENDEDKEY;
            scancode & 0xFF
        } else {
            scancode
        };

        unsafe {
            let input = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: Default::default(),
                        wScan: actual_scan,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: SHAREFLOW_EXTRA_INFO,
                    },
                },
            };
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }
        Ok(())
    }
}
