use serde::{Deserialize, Serialize};

/// Unique identifier for a peer on the network.
pub type PeerId = String;

/// All messages sent between peers over the network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    /// Handshake sent on connection.
    Hello {
        peer_id: PeerId,
        name: String,
        screens: Vec<ScreenInfo>,
    },

    /// Acknowledge a hello.
    HelloAck {
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

    /// Camera frame from a peer (JPEG-encoded bytes).
    CameraFrame { data: Vec<u8> },

    /// Audio chunk from a peer (WebM/Opus encoded bytes).
    AudioChunk { data: Vec<u8> },

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

/// Deserialize a message from a length-prefixed buffer.
/// Returns (message, bytes_consumed).
pub fn decode_message(buf: &[u8]) -> Result<Option<(Message, usize)>, String> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let msg = bincode::deserialize(&buf[4..4 + len]).map_err(|e| e.to_string())?;
    Ok(Some((msg, 4 + len)))
}
