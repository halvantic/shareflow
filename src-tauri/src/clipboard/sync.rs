use arboard::{Clipboard, ImageData};
use crate::core::protocol::ClipboardContent;
use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// macOS: cheap NSPasteboard changeCount check
// ---------------------------------------------------------------------------
// On macOS the clipboard sync loop uses 300 ms polling.  Without this check,
// every tick calls get_clipboard_content() which decodes the FULL clipboard
// image to RGBA — a 4K screenshot can be 50–100 MB of work every 300 ms,
// pinning a CPU core at 100%.  NSPasteboard.changeCount is a single integer
// that increments on every clipboard change; reading it is essentially free.
// We skip the expensive data read when the count hasn't changed.
#[cfg(target_os = "macos")]
static MACOS_LAST_CHANGE_COUNT: AtomicI64 = AtomicI64::new(i64::MIN);

/// Returns the current NSPasteboard general-pasteboard changeCount.
/// Only compiled on macOS; uses raw Objective-C runtime so no extra deps.
#[cfg(target_os = "macos")]
fn macos_pasteboard_change_count() -> i64 {
    use std::ffi::c_void;
    use std::os::raw::c_char;
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        // variadic declaration; we cast to the specific signature we need below
        fn objc_msgSend(receiver: *mut c_void, op: *mut c_void, ...) -> *mut c_void;
    }
    unsafe {
        let class = objc_getClass(b"NSPasteboard\0".as_ptr() as *const c_char);
        if class.is_null() {
            return -1;
        }
        let sel_gp = sel_registerName(b"generalPasteboard\0".as_ptr() as *const c_char);
        let pb = objc_msgSend(class, sel_gp);
        if pb.is_null() {
            return -1;
        }
        let sel_cc = sel_registerName(b"changeCount\0".as_ptr() as *const c_char);
        // NSInteger is isize on 64-bit.  Cast objc_msgSend to the exact
        // signature so the return value comes back in the integer register.
        let get_count: unsafe extern "C" fn(*mut c_void, *mut c_void) -> isize =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn(*mut c_void, *mut c_void, ...) -> *mut c_void);
        get_count(pb, sel_cc) as i64
    }
}

/// Set when clipboard was updated by a remote peer, to avoid re-broadcasting it back.
static REMOTE_SET: AtomicBool = AtomicBool::new(false);

/// Timestamp of the last time we broadcast a locally-originated clipboard change to peers.
/// Used to suppress incoming peer clipboard updates for a short window after a local push,
/// preventing the peer from echoing our clipboard back and overwriting it (e.g., a Snipping
/// Tool screenshot that gets sent to the peer then bounced back as a stripped arboard copy).
static LAST_LOCAL_PUSH: std::sync::LazyLock<Mutex<Option<Instant>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));

/// How long to ignore incoming clipboard updates after pushing a local clipboard change.
const LOCAL_PUSH_PROTECT_MS: u64 = 2000;

/// Record that we just pushed a local clipboard change to one or more peers.
/// Call this immediately after broadcasting a locally-originated clipboard update.
pub fn notify_local_push() {
    if let Ok(mut guard) = LAST_LOCAL_PUSH.lock() {
        *guard = Some(Instant::now());
    }
}

/// Returns true if we pushed a local clipboard change recently enough that we should
/// ignore incoming clipboard updates from peers (protection against echo-back).
fn recently_pushed_locally() -> bool {
    LAST_LOCAL_PUSH
        .lock()
        .ok()
        .and_then(|g| *g)
        .map(|t| t.elapsed() < Duration::from_millis(LOCAL_PUSH_PROTECT_MS))
        .unwrap_or(false)
}

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

/// On Windows, do a cheap format check before opening the OLE clipboard.
/// `IsClipboardFormatAvailable` reads a format table without opening/locking
/// the clipboard, so it cannot interfere with Explorer or other apps.
/// Returns false when the clipboard contains file-drop (CF_HDROP) or other
/// formats we can't sync — we skip the expensive OLE open entirely.
///
/// Important: CF_HDROP is checked FIRST and causes an early false return even
/// when text/image formats are also present. Windows Explorer adds both CF_HDROP
/// and CF_UNICODETEXT (the file path) to the clipboard when copying files. If we
/// only check for syncable text formats, we would open the OLE clipboard during
/// a file copy and block concurrent paste operations in Explorer.
#[cfg(windows)]
fn clipboard_has_syncable_format() -> bool {
    extern "system" {
        fn IsClipboardFormatAvailable(format: u32) -> i32;
    }
    const CF_HDROP: u32 = 15;    // file list — must skip entirely
    const CF_TEXT: u32 = 1;
    const CF_UNICODETEXT: u32 = 13;
    const CF_DIB: u32 = 8;       // device-independent bitmap
    const CF_DIBV5: u32 = 17;    // v5 DIB (used by some apps for images)
    unsafe {
        // If files are in the clipboard, skip entirely — even if text/image
        // formats are also present (Explorer puts the file path in CF_UNICODETEXT
        // alongside CF_HDROP). Opening the OLE clipboard during a file copy/paste
        // operation blocks the user's paste and causes "paste greyed out" issues.
        if IsClipboardFormatAvailable(CF_HDROP) != 0 {
            return false;
        }
        IsClipboardFormatAvailable(CF_TEXT) != 0
            || IsClipboardFormatAvailable(CF_UNICODETEXT) != 0
            || IsClipboardFormatAvailable(CF_DIB) != 0
            || IsClipboardFormatAvailable(CF_DIBV5) != 0
    }
}

#[cfg(not(windows))]
fn clipboard_has_syncable_format() -> bool {
    true // macOS/Linux: always attempt; arboard handles format filtering there
}

/// Get the current clipboard content (text or image).
/// Returns None if the clipboard is empty, contains unsyncable formats (e.g.
/// file drops), or cannot be opened. Never clears or modifies the clipboard.
pub fn get_clipboard_content() -> Option<ClipboardContent> {
    // Fast path on Windows: if the clipboard doesn't have text or image
    // formats, skip opening the OLE clipboard entirely. This prevents
    // ShareFlow from interfering with file copy-paste (CF_HDROP) which
    // the arboard OLE calls can disrupt even during a read.
    if !clipboard_has_syncable_format() {
        return None;
    }

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
/// The REMOTE_SET flag is set BEFORE modifying the clipboard to prevent a race
/// where poll_clipboard_change reads the new content before seeing the flag.
pub fn apply_remote_clipboard(content: ClipboardContent) {
    // If we recently pushed a local clipboard change, ignore peer updates for a short
    // window. This prevents the peer from echoing our content back and overwriting it
    // (e.g. a Snipping Tool screenshot replaced by the peer's older clipboard content,
    // or by a stripped arboard-only version that loses CF_BITMAP and proprietary formats).
    if recently_pushed_locally() {
        log::debug!("Ignoring remote clipboard update: within local-push protection window");
        return;
    }

    // Set flag BEFORE modifying clipboard to prevent race with poll_clipboard_change
    REMOTE_SET.store(true, Ordering::SeqCst);

    let _guard = CLIPBOARD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut success = false;
    if let Ok(mut clipboard) = Clipboard::new() {
        let ok = match &content {
            ClipboardContent::Text(text) => {
                match clipboard.set_text(text) {
                    Ok(_) => {
                        log::debug!("Applied remote clipboard: text ({} chars)", text.len());
                        true
                    }
                    Err(e) => {
                        log::warn!("Failed to set remote clipboard text: {}", e);
                        false
                    }
                }
            }
            ClipboardContent::Image { width, height, rgba } => {
                match clipboard.set_image(ImageData {
                    width: *width,
                    height: *height,
                    bytes: Cow::Borrowed(rgba),
                }) {
                    Ok(_) => {
                        log::debug!("Applied remote clipboard: image {}x{}", width, height);
                        true
                    }
                    Err(e) => {
                        log::warn!("Failed to set remote clipboard image: {}", e);
                        false
                    }
                }
            }
        };
        success = ok;
    } else {
        log::warn!("Failed to open clipboard to apply remote update");
    }

    // Clear flag if we couldn't apply the update, to prevent suppressing next local change
    if !success {
        REMOTE_SET.store(false, Ordering::SeqCst);
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
    // macOS fast-path: skip the expensive clipboard data read when the
    // NSPasteboard changeCount hasn't moved.  This prevents decoding a full
    // screenshot image every 300 ms, which was pinning a CPU core at ~100%.
    #[cfg(target_os = "macos")]
    {
        let count = macos_pasteboard_change_count();
        let prev = MACOS_LAST_CHANGE_COUNT.load(Ordering::SeqCst);
        if count == prev && prev != i64::MIN {
            return None; // Nothing changed — skip the expensive data read
        }
        // Store the new count so subsequent calls skip until the next change.
        // We always store here; the full read below may still return None if
        // the content is unsyncable (e.g. file-drop), but that's fine.
        MACOS_LAST_CHANGE_COUNT.store(count, Ordering::SeqCst);
    }

    let content = get_clipboard_content();
    let fp = fingerprint_of(&content);

    if fp == *last_known {
        return None;
    }

    // Clipboard changed — update our tracking state.
    *last_known = fp.clone();

    // If this change was triggered by apply_remote_clipboard, suppress the broadcast
    // to prevent a loop: A→B→A→B…
    // We must check the flag atomically with reading the content to prevent races:
    // If another thread is calling apply_remote_clipboard simultaneously, we might
    // have read the new clipboard content but then suppress it anyway (correct).
    // However, if the flag was already cleared by a previous poll, we won't suppress.
    // This is the intended behavior — each remote update sets the flag once.
    if REMOTE_SET.swap(false, Ordering::SeqCst) {
        return None;
    }

    content
}
