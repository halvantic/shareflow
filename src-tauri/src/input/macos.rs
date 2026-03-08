use super::{InputCapture, InputEvent, InputInjector};
use crate::core::protocol::MouseButton;

/// macOS input capture using CGEventTap.
/// Requires Accessibility permissions.
pub struct MacOSInputCapture {
    capturing: bool,
}

impl MacOSInputCapture {
    pub fn new() -> Self {
        Self { capturing: false }
    }
}

impl InputCapture for MacOSInputCapture {
    fn start_capture(&mut self, _callback: Box<dyn Fn(InputEvent) + Send>) -> Result<(), String> {
        // TODO: Implement CGEventTap-based capture.
        // Requires:
        //   1. Check/request Accessibility permission
        //   2. Create CGEventTap with kCGHeadInsertEventTap
        //   3. Register for mouse & keyboard events
        //   4. Run in a CFRunLoop
        self.capturing = true;
        log::warn!("macOS input capture is not yet implemented");
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

/// macOS input injection using CGEventPost.
pub struct MacOSInputInjector;

impl MacOSInputInjector {
    pub fn new() -> Self {
        Self
    }
}

impl InputInjector for MacOSInputInjector {
    fn move_mouse(&self, _x: i32, _y: i32) -> Result<(), String> {
        // TODO: CGEventCreateMouseEvent + CGEventPost
        log::warn!("macOS mouse injection not yet implemented");
        Ok(())
    }

    fn press_mouse_button(&self, _button: MouseButton, _pressed: bool) -> Result<(), String> {
        log::warn!("macOS mouse button injection not yet implemented");
        Ok(())
    }

    fn scroll(&self, _dx: i32, _dy: i32) -> Result<(), String> {
        log::warn!("macOS scroll injection not yet implemented");
        Ok(())
    }

    fn send_key(&self, _scancode: u16, _pressed: bool) -> Result<(), String> {
        log::warn!("macOS key injection not yet implemented");
        Ok(())
    }
}
