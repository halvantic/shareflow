use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::OnceLock;

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, GetSystemMetrics, PostThreadMessageW, SetWindowsHookExW,
    UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, MSLLHOOKSTRUCT, MSG, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WH_KEYBOARD_LL, WH_MOUSE_LL,
    WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_XBUTTONDOWN,
    WM_XBUTTONUP,
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
    SUPPRESS.store(suppress, Ordering::SeqCst);
}

pub fn is_suppressing() -> bool {
    SUPPRESS.load(Ordering::SeqCst)
}

unsafe extern "system" fn mouse_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code >= 0 && HOOK_ACTIVE.load(Ordering::SeqCst) {
        let data = &*(lparam.0 as *const MSLLHOOKSTRUCT);

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
            // Send to async runtime via channel.
            if let Some(tx) = EVENT_SENDER.get() {
                let _ = tx.send(event);
            }

            // Suppress if active — don't pass to local OS.
            if SUPPRESS.load(Ordering::SeqCst) {
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
        let pressed = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);

        let event = InputEvent::Key(KeyEvent {
            scancode: data.scanCode as u16,
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
        unsafe {
            let virt_x = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let virt_y = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let screen_w = GetSystemMetrics(SM_CXVIRTUALSCREEN) as f64;
            let screen_h = GetSystemMetrics(SM_CYVIRTUALSCREEN) as f64;

            // Absolute coordinates are normalized 0..65535 across the virtual desktop.
            let abs_x = (((x - virt_x) as f64 / screen_w) * 65535.0) as i32;
            let abs_y = (((y - virt_y) as f64 / screen_h) * 65535.0) as i32;

            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: abs_x,
                        dy: abs_y,
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_MOVE
                            | MOUSEEVENTF_ABSOLUTE
                            | MOUSEEVENTF_VIRTUALDESK,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
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
                        dwExtraInfo: 0,
                    },
                },
            };
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }
        Ok(())
    }

    fn scroll(&self, _dx: i32, dy: i32) -> Result<(), String> {
        unsafe {
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 0,
                        dy: 0,
                        mouseData: dy as u32,
                        dwFlags: MOUSEEVENTF_WHEEL,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }
        Ok(())
    }

    fn send_key(&self, scancode: u16, pressed: bool) -> Result<(), String> {
        let mut flags = KEYEVENTF_SCANCODE;
        if !pressed {
            flags |= KEYEVENTF_KEYUP;
        }

        unsafe {
            let input = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: Default::default(),
                        wScan: scancode,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }
        Ok(())
    }
}
