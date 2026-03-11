use arboard::Clipboard;
use crate::core::protocol::ClipboardContent;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set when clipboard was updated by a remote peer, to avoid re-broadcasting it back.
static REMOTE_SET: AtomicBool = AtomicBool::new(false);

/// Get the current clipboard text content.
pub fn get_clipboard_text() -> Option<String> {
    let mut clipboard = Clipboard::new().ok()?;
    clipboard.get_text().ok()
}

/// Set the local clipboard to the content received from a remote peer.
/// Marks the content as remote-originated so the sync loop won't re-broadcast it.
pub fn apply_remote_clipboard(content: ClipboardContent) {
    match content {
        ClipboardContent::Text(text) => {
            if let Ok(mut clipboard) = Clipboard::new() {
                if clipboard.set_text(&text).is_ok() {
                    // Mark as remote-set so poll_clipboard_change won't echo it back.
                    REMOTE_SET.store(true, Ordering::SeqCst);
                }
            }
        }
    }
}

/// Monitor the clipboard for locally-originated changes (polling approach).
///
/// Updates `last_known` unconditionally. Returns the new text only when the change
/// was local (not from a remote peer). Returns `None` for remote-set changes to
/// prevent ping-pong broadcast loops.
pub fn poll_clipboard_change(last_known: &mut Option<String>) -> Option<String> {
    let current = get_clipboard_text();
    if current == *last_known {
        return None;
    }

    // Clipboard changed — update our tracking state.
    let new_text = current.clone();
    *last_known = current;

    // If this change was triggered by apply_remote_clipboard, suppress the broadcast
    // to prevent a loop: A→B→A→B…
    if REMOTE_SET.swap(false, Ordering::SeqCst) {
        return None;
    }

    new_text
}
