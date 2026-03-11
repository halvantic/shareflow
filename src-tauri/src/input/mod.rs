#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "linux")]
pub mod linux;

use crate::core::protocol::{KeyEvent, MouseButton, MouseButtonEvent, MouseMoveEvent, MouseScrollEvent};

/// Trait for capturing input events from the local machine.
#[allow(dead_code)]
pub trait InputCapture: Send + 'static {
    fn start_capture(&mut self, callback: Box<dyn Fn(InputEvent) + Send>) -> Result<(), String>;
    fn stop_capture(&mut self) -> Result<(), String>;
    fn is_capturing(&self) -> bool;
}

/// Trait for injecting input events into the local OS.
pub trait InputInjector: Send + 'static {
    fn move_mouse(&self, x: i32, y: i32) -> Result<(), String>;
    fn press_mouse_button(&self, button: MouseButton, pressed: bool) -> Result<(), String>;
    fn scroll(&self, dx: i32, dy: i32) -> Result<(), String>;
    fn send_key(&self, scancode: u16, pressed: bool) -> Result<(), String>;
}

/// Events produced by the input capture layer.
#[derive(Debug, Clone)]
pub enum InputEvent {
    MouseMove(MouseMoveEvent),
    MouseButton(MouseButtonEvent),
    MouseScroll(MouseScrollEvent),
    Key(KeyEvent),
}

/// Create platform-specific input capture.
#[allow(dead_code)]
pub fn create_capture() -> Box<dyn InputCapture> {
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsInputCapture::new())
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacOSInputCapture::new())
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::LinuxInputCapture::new())
    }
}

/// Create platform-specific input injector.
pub fn create_injector() -> Box<dyn InputInjector> {
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsInputInjector::new())
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacOSInputInjector::new())
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::LinuxInputInjector::new())
    }
}

/// Set whether input should be suppressed (not passed to local OS).
pub fn set_input_suppression(suppress: bool) {
    #[cfg(target_os = "windows")]
    {
        windows::set_suppress(suppress);
    }
    #[cfg(target_os = "macos")]
    {
        macos::set_suppress(suppress);
    }
    #[cfg(target_os = "linux")]
    {
        linux::set_suppress(suppress);
    }
}

/// Initialize remote mouse control with the entry point on the remote screen.
/// `rs_*` parameters define the remote screen bounds for clamping.
pub fn init_remote_mouse(virtual_x: i32, virtual_y: i32, rs_x: i32, rs_y: i32, rs_w: i32, rs_h: i32) {
    #[cfg(target_os = "windows")]
    {
        windows::init_remote_mouse(virtual_x, virtual_y, rs_x, rs_y, rs_w, rs_h);
    }
    #[cfg(target_os = "macos")]
    {
        macos::init_remote_mouse(virtual_x, virtual_y, rs_x, rs_y, rs_w, rs_h);
    }
    #[cfg(target_os = "linux")]
    {
        linux::init_remote_mouse(virtual_x, virtual_y, rs_x, rs_y, rs_w, rs_h);
    }
}

/// Create capture and return the event receiver channel.
#[cfg(target_os = "windows")]
pub fn create_capture_with_channel() -> (
    windows::WindowsInputCapture,
    Option<std::sync::mpsc::Receiver<InputEvent>>,
) {
    let mut capture = windows::WindowsInputCapture::new();
    // Start capture to set up hooks and channel
    let _ = capture.start_capture(Box::new(|_| {}));
    let rx = capture.take_event_receiver();
    (capture, rx)
}

/// Create capture and return the event receiver channel (macOS).
#[cfg(target_os = "macos")]
pub fn create_capture_with_channel() -> (
    macos::MacOSInputCapture,
    Option<std::sync::mpsc::Receiver<InputEvent>>,
) {
    macos::MacOSInputCapture::new_with_channel()
}

/// Create capture and return the event receiver channel (Linux).
#[cfg(target_os = "linux")]
pub fn create_capture_with_channel() -> (
    linux::LinuxInputCapture,
    Option<std::sync::mpsc::Receiver<InputEvent>>,
) {
    linux::LinuxInputCapture::new_with_channel()
}
