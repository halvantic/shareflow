use arboard::{Clipboard, ImageData};
use crate::core::protocol::ClipboardContent;
use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Set when clipboard was updated by a remote peer, to avoid re-broadcasting it back.
static REMOTE_SET: AtomicBool = AtomicBool::new(false);

/// Global mutex to serialize all clipboard access.
/// On Windows, arboard uses OLE clipboard APIs that are not thread-safe —
/// concurrent access from multiple threads causes access violations (silent crash).
static CLIPBOARD_LOCK: std::sync::LazyLock<Mutex<()>> =
    std::sync::LazyLock::new(|| Mutex::new(()));

/// Lightweight fingerprint for change detection without storing full image data.
#[derive(Clone, PartialEq)]
pub enum ClipboardFingerprint {
    Text(String),
    Image { width: usize, height: usize, hash: u64 },
}

/// Compute a fast hash of image data by sampling the head and tail.
fn sample_hash(width: usize, height: usize, rgba: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    width.hash(&mut h);
    height.hash(&mut h);
    rgba.len().hash(&mut h);
    let n = rgba.len().min(4096);
    rgba[..n].hash(&mut h);
    if rgba.len() > n {
        rgba[rgba.len() - n..].hash(&mut h);
    }
    h.finish()
}

/// Get the current clipboard content (text or image).
pub fn get_clipboard_content() -> Option<ClipboardContent> {
    let _guard = CLIPBOARD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut clipboard = Clipboard::new().ok()?;
    // Try text first.
    if let Ok(text) = clipboard.get_text() {
        if !text.is_empty() {
            return Some(ClipboardContent::Text(text));
        }
    }
    // Fall back to image.
    if let Ok(img) = clipboard.get_image() {
        return Some(ClipboardContent::Image {
            width: img.width,
            height: img.height,
            rgba: img.bytes.into_owned(),
        });
    }
    None
}

/// Build a fingerprint from existing content (avoids a second clipboard read).
fn fingerprint_of(content: &Option<ClipboardContent>) -> Option<ClipboardFingerprint> {
    content.as_ref().map(|c| match c {
        ClipboardContent::Text(t) => ClipboardFingerprint::Text(t.clone()),
        ClipboardContent::Image { width, height, rgba } => ClipboardFingerprint::Image {
            width: *width,
            height: *height,
            hash: sample_hash(*width, *height, rgba),
        },
    })
}

/// Get a fingerprint of the current clipboard for initialising change tracking.
pub fn get_clipboard_fingerprint() -> Option<ClipboardFingerprint> {
    fingerprint_of(&get_clipboard_content())
}

/// Set the local clipboard to the content received from a remote peer.
/// Marks the content as remote-originated so the sync loop won't re-broadcast it.
pub fn apply_remote_clipboard(content: ClipboardContent) {
    let _guard = CLIPBOARD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(mut clipboard) = Clipboard::new() {
        let ok = match &content {
            ClipboardContent::Text(text) => clipboard.set_text(text).is_ok(),
            ClipboardContent::Image { width, height, rgba } => clipboard
                .set_image(ImageData {
                    width: *width,
                    height: *height,
                    bytes: Cow::Borrowed(rgba),
                })
                .is_ok(),
        };
        if ok {
            REMOTE_SET.store(true, Ordering::SeqCst);
        }
    }
}

/// Monitor the clipboard for locally-originated changes (polling approach).
///
/// Updates `last_known` unconditionally. Returns the new content only when the change
/// was local (not from a remote peer). Returns `None` for remote-set changes to
/// prevent ping-pong broadcast loops.
pub fn poll_clipboard_change(
    last_known: &mut Option<ClipboardFingerprint>,
) -> Option<ClipboardContent> {
    let content = get_clipboard_content();
    let fp = fingerprint_of(&content);

    if fp == *last_known {
        return None;
    }

    // Clipboard changed — update our tracking state.
    *last_known = fp;

    // If this change was triggered by apply_remote_clipboard, suppress the broadcast
    // to prevent a loop: A→B→A→B…
    if REMOTE_SET.swap(false, Ordering::SeqCst) {
        return None;
    }

    content
}
