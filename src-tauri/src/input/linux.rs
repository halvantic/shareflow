use super::{InputCapture, InputEvent, InputInjector};
use crate::core::protocol::MouseButton;

pub struct LinuxInputCapture {
    capturing: bool,
}

impl LinuxInputCapture {
    pub fn new() -> Self {
        Self { capturing: false }
    }

    pub fn new_with_channel() -> (Self, Option<std::sync::mpsc::Receiver<InputEvent>>) {
        (Self::new(), None)
    }
}

impl InputCapture for LinuxInputCapture {
    fn start_capture(
        &mut self,
        _callback: Box<dyn Fn(InputEvent) + Send>,
    ) -> Result<(), String> {
        Err("Linux input capture not yet implemented (requires evdev). \
             Run on Windows or macOS, or contribute an evdev-based implementation."
            .into())
    }

    fn stop_capture(&mut self) -> Result<(), String> {
        self.capturing = false;
        Ok(())
    }

    fn is_capturing(&self) -> bool {
        self.capturing
    }
}

pub struct LinuxInputInjector;

impl LinuxInputInjector {
    pub fn new() -> Self {
        Self
    }
}

impl InputInjector for LinuxInputInjector {
    fn move_mouse(&self, _x: i32, _y: i32) -> Result<(), String> {
        Err("Linux mouse injection not yet implemented (requires uinput)".into())
    }

    fn press_mouse_button(&self, _button: MouseButton, _pressed: bool) -> Result<(), String> {
        Err("Linux mouse button injection not yet implemented (requires uinput)".into())
    }

    fn scroll(&self, _dx: i32, _dy: i32) -> Result<(), String> {
        Err("Linux scroll injection not yet implemented (requires uinput)".into())
    }

    fn send_key(&self, _scancode: u16, _pressed: bool) -> Result<(), String> {
        Err("Linux key injection not yet implemented (requires uinput)".into())
    }
}

pub fn set_suppress(_suppress: bool) {
    log::warn!("Linux input suppression not yet implemented");
}

pub fn init_remote_mouse(_virtual_x: i32, _virtual_y: i32, _rs_x: i32, _rs_y: i32, _rs_w: i32, _rs_h: i32) {
    log::warn!("Linux remote mouse init not yet implemented");
}

pub fn get_screens_linux() -> Vec<crate::core::protocol::ScreenInfo> {
    log::warn!("Linux screen detection not yet implemented, returning default");
    vec![crate::core::protocol::ScreenInfo {
        id: "main".to_string(),
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
        primary: true,
    }]
}
