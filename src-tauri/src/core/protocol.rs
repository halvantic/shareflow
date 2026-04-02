use serde::{Deserialize, Serialize};

/// Unique identifier for a peer on the network.
pub type PeerId = String;

/// Current wire-protocol version. Increment this whenever a
/// backward-incompatible change is made to the Message enum (new required
/// fields in existing variants, removed variants, changed field types).
///
/// Version history:
///   1 — initial versioned protocol (Hello/HelloAck gain protocol_version field)
pub const PROTOCOL_VERSION: u16 = 1;

/// Oldest protocol version this build will accept from a remote peer.
/// Connections with a lower version are rejected with a clear error message
/// instead of silently failing or producing corrupt state.
pub const MIN_SUPPORTED_PROTOCOL_VERSION: u16 = 1;

/// All messages sent between peers over the network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    /// Handshake sent on connection.
    Hello {
        /// Wire-protocol version spoken by the sender.
        /// Checked against MIN_SUPPORTED_PROTOCOL_VERSION on receipt.
        protocol_version: u16,
        peer_id: PeerId,
        name: String,
        screens: Vec<ScreenInfo>,
    },

    /// Acknowledge a hello.
    HelloAck {
        /// Wire-protocol version spoken by the sender.
        protocol_version: u16,
        peer_id: PeerId,
        name: String,
        screens: Vec<ScreenInfo>,
    },

    /// Authentication challenge/response using pairing code.
    AuthChallenge { nonce: Vec<u8> },
    AuthResponse { hash: Vec<u8> },
    AuthResult { success: bool },

    /// Mouse moved to absolute position.
    MouseMove(MouseMoveEvent),

    /// Mouse button pressed or released.
    MouseButton(MouseButtonEvent),

    /// Mouse scroll wheel.
    MouseScroll(MouseScrollEvent),

    /// Keyboard event using hardware scancodes.
    Key(KeyEvent),

    /// Request to switch input focus to a target peer.
    SwitchFocus {
        target_id: PeerId,
        entry_x: i32,
        entry_y: i32,
    },

    /// Clipboard content changed on the active machine.
    ClipboardUpdate { content: ClipboardContent },

    /// Clipboard image with LZ4-compressed RGBA payload.
    /// Sent instead of ClipboardUpdate for images to reduce wire size.
    ClipboardUpdateCompressed {
        width: usize,
        height: usize,
        /// LZ4-compressed RGBA bytes.
        compressed_rgba: Vec<u8>,
        /// Original uncompressed length (needed for decompression).
        original_len: usize,
    },

    /// File transfer: start a new transfer.
    FileStart {
        transfer_id: String,
        file_name: String,
        file_size: u64,
    },

    /// File transfer: a chunk of data.
    FileChunk {
        transfer_id: String,
        offset: u64,
        data: Vec<u8>,
    },

    /// File transfer: transfer complete.
    FileDone {
        transfer_id: String,
    },

    /// File transfer: cancel/error.
    FileCancel {
        transfer_id: String,
        reason: String,
    },

    /// Notify peers that our screen configuration has changed (e.g. after wake).
    ScreenUpdate {
        screens: Vec<ScreenInfo>,
    },

    /// Sync primary keyboard & mouse device setting across peers.
    /// None means "allow all devices". Some(peer_id) means only that device can inject input.
    PrimaryKmDeviceSync {
        primary_km_peer_id: Option<PeerId>,
    },

    /// Host → peer: automatically set a reciprocal neighbor edge.
    /// Sent whenever the host calls set_neighbor so both sides stay in sync.
    AutoNeighbor {
        /// The peer_id the recipient should point at (the sender's peer_id).
        peer_id: String,
        /// Edge on the recipient's side ("Left", "Right", "Top", "Bottom").
        edge: String,
        /// True = remove the mapping, false = add/replace it.
        remove: bool,
    },

    /// Ping/pong for keepalive.
    Ping,
    Pong,

    /// Host pushes its active settings to agents on connect and whenever
    /// settings change.  Agents apply these values in memory without
    /// persisting them — the host is authoritative at runtime.
    ConfigSync {
        clipboard_sync_enabled: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MouseMoveEvent {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MouseButtonEvent {
    pub button: MouseButton,
    pub pressed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MouseScrollEvent {
    pub dx: i32,
    pub dy: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyEvent {
    pub scancode: u16,
    pub pressed: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Button4,
    Button5,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenInfo {
    pub id: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub primary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClipboardContent {
    Text(String),
    Image {
        width: usize,
        height: usize,
        rgba: Vec<u8>,
    },
}

/// Serialize a message to bytes (length-prefixed bincode).
pub fn encode_message(msg: &Message) -> Result<Vec<u8>, String> {
    let payload = bincode::serialize(msg).map_err(|e| e.to_string())?;
    let len = (payload.len() as u32).to_be_bytes();
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&len);
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Maximum allowed size for a single framed message (64 MB).
/// Rejects absurdly large length prefixes before we attempt to accumulate
/// that many bytes in the pending buffer, protecting against DoS via a
/// crafted 0xFFFF_FFFF length header.
const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

/// Deserialize a message from a length-prefixed buffer.
/// Returns (message, bytes_consumed).
pub fn decode_message(buf: &[u8]) -> Result<Option<(Message, usize)>, String> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_MESSAGE_SIZE {
        return Err(format!(
            "Message length {} exceeds maximum allowed size of {} bytes",
            len, MAX_MESSAGE_SIZE
        ));
    }
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let msg = bincode::deserialize(&buf[4..4 + len]).map_err(|e| e.to_string())?;
    Ok(Some((msg, 4 + len)))
}

/// Wrap a ClipboardContent into the appropriate Message, compressing images
/// with LZ4 to dramatically reduce wire size (4K RGBA ~33 MB → ~1-3 MB).
pub fn clipboard_to_message(content: ClipboardContent) -> Message {
    match content {
        ClipboardContent::Text(_) => Message::ClipboardUpdate { content },
        ClipboardContent::Image { width, height, rgba } => {
            let original_len = rgba.len();
            let compressed_rgba = lz4_flex::compress_prepend_size(&rgba);
            log::debug!(
                "Clipboard image {}x{}: {} → {} bytes ({:.0}% reduction)",
                width, height, original_len, compressed_rgba.len(),
                (1.0 - compressed_rgba.len() as f64 / original_len as f64) * 100.0
            );
            Message::ClipboardUpdateCompressed {
                width,
                height,
                compressed_rgba,
                original_len,
            }
        }
    }
}

/// Maximum decompressed clipboard image size (512 MB of RGBA data).
/// A 16 K × 16 K image at 4 bytes/pixel is 1 073 741 824 bytes (~1 GB);
/// 512 MB covers 4K displays (3840 × 2160 × 4 = ~33 MB) with ample headroom.
const MAX_CLIPBOARD_IMAGE_BYTES: usize = 512 * 1024 * 1024;

/// Decompress a ClipboardUpdateCompressed back into ClipboardContent.
pub fn decompress_clipboard(
    width: usize,
    height: usize,
    compressed_rgba: Vec<u8>,
    _original_len: usize,
) -> Result<ClipboardContent, String> {
    // Validate dimensions before decompressing to prevent OOM from a malicious peer
    // sending a crafted width/height that causes a multi-gigabyte allocation.
    let expected_bytes = width
        .checked_mul(height)
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| format!("Clipboard image dimensions overflow: {}x{}", width, height))?;
    if expected_bytes > MAX_CLIPBOARD_IMAGE_BYTES {
        return Err(format!(
            "Clipboard image too large: {}x{} = {} bytes (max {})",
            width, height, expected_bytes, MAX_CLIPBOARD_IMAGE_BYTES
        ));
    }
    let rgba = lz4_flex::decompress_size_prepended(&compressed_rgba)
        .map_err(|e| format!("LZ4 decompression failed: {}", e))?;
    Ok(ClipboardContent::Image { width, height, rgba })
}
