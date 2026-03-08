use arboard::Clipboard;
use crate::core::protocol::ClipboardContent;

/// Get the current clipboard text content.
pub fn get_clipboard_text() -> Option<String> {
    let mut clipboard = Clipboard::new().ok()?;
    clipboard.get_text().ok()
}

/// Set the local clipboard to the content received from a remote peer.
pub fn apply_remote_clipboard(content: ClipboardContent) {
    match content {
        ClipboardContent::Text(text) => {
            if let Ok(mut clipboard) = Clipboard::new() {
                if let Err(e) = clipboard.set_text(&text) {
                    log::error!("Failed to set clipboard: {}", e);
                }
            }
        }
    }
}

/// Monitor the clipboard for changes (polling approach).
/// Returns the new text if it changed since `last_known`.
pub fn poll_clipboard_change(last_known: &Option<String>) -> Option<String> {
    let current = get_clipboard_text();
    if current != *last_known {
        current
    } else {
        None
    }
}
