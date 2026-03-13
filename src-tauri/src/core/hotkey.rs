#![allow(dead_code)]
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::input::InputEvent;

/// Scancodes for common modifier/toggle keys.
#[allow(dead_code)]
pub const SC_SCROLL_LOCK: u16 = 0x46;
#[allow(dead_code)]
pub const SC_LCTRL: u16 = 0x1D;
#[allow(dead_code)]
pub const SC_RCTRL: u16 = 0x11D; // Extended
#[allow(dead_code)]
pub const SC_LALT: u16 = 0x38;
#[allow(dead_code)]
pub const SC_RALT: u16 = 0x138;
#[allow(dead_code)]
pub const SC_LSHIFT: u16 = 0x2A;
#[allow(dead_code)]
pub const SC_RSHIFT: u16 = 0x36;
#[allow(dead_code)]
pub const SC_SPACE: u16 = 0x39;

/// Tracks currently pressed keys and detects hotkey combos.
pub struct HotkeyDetector {
    /// Set of scancodes currently held down.
    pressed: Mutex<HashSet<u16>>,
    /// The configured hotkey combo (set of scancodes that must all be pressed).
    /// Default: just Scroll Lock.
    combo: Mutex<Vec<u16>>,
    /// Debounce: was the hotkey already fired for this press cycle?
    fired: AtomicBool,
}

impl HotkeyDetector {
    pub fn new() -> Self {
        Self {
            pressed: Mutex::new(HashSet::new()),
            combo: Mutex::new(vec![SC_SCROLL_LOCK]),
            fired: AtomicBool::new(false),
        }
    }

    /// Set a custom hotkey combo (list of scancodes).
    pub fn set_combo(&self, scancodes: Vec<u16>) {
        log::info!("Hotkey combo set to: {:?} (scancodes: [{}])",
            scancodes,
            scancodes.iter().map(|s| format!("0x{:X}", s)).collect::<Vec<_>>().join(", "));
        *self.combo.lock().unwrap_or_else(|e| e.into_inner()) = scancodes;
    }

    /// Process an input event. Returns true if the hotkey was just triggered.
    pub fn process(&self, event: &InputEvent) -> bool {
        if let InputEvent::Key(ke) = event {
            let mut pressed = self.pressed.lock().unwrap_or_else(|e| e.into_inner());
            if ke.pressed {
                pressed.insert(ke.scancode);
            } else {
                pressed.remove(&ke.scancode);
                // Reset fired state when any key in the combo is released.
                self.fired.store(false, Ordering::SeqCst);
                return false;
            }

            // Check if all keys in the combo are currently held.
            let combo = self.combo.lock().unwrap_or_else(|e| e.into_inner());
            if !combo.is_empty() && combo.iter().all(|sc| pressed.contains(sc)) {
                if !self.fired.swap(true, Ordering::SeqCst) {
                    let combo_str = combo.iter().map(|s| format!("0x{:X}", s)).collect::<Vec<_>>().join("+");
                    log::info!("Hotkey triggered! ({})", combo_str);
                    return true; // Fire once per press cycle
                }
            }
        }
        false
    }
}
