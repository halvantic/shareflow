use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use crate::core::config::{AppConfig, ScreenEdge};
use crate::core::protocol::{ClipboardContent, Message, PeerId, ScreenInfo};
use crate::core::screen::{detect_edge, EdgeHit};
use crate::file_transfer::receiver::FileReceiver;
use crate::input::InputEvent;

/// Which machine currently has keyboard/mouse focus.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum FocusState {
    Local,
    Remote(PeerId),
}

/// A connected peer.
pub struct Peer {
    pub id: PeerId,
    pub name: String,
    pub screens: Vec<ScreenInfo>,
    pub sender: mpsc::Sender<Message>,
}

/// The core engine that manages focus switching and input routing.
pub struct Engine {
    pub config: Arc<Mutex<AppConfig>>,
    pub focus: Arc<Mutex<FocusState>>,
    pub local_screens: Arc<Mutex<Vec<ScreenInfo>>>,
    pub peers: Arc<Mutex<HashMap<PeerId, Peer>>>,
    /// Channel for sending log/status events to the UI.
    pub ui_events: mpsc::Sender<UiEvent>,
    /// Manages incoming file transfers.
    pub file_receiver: FileReceiver,
    /// Cooldown: last time a focus switch occurred, to prevent rapid oscillation.
    pub last_switch_time: Arc<Mutex<Option<std::time::Instant>>>,
}

/// Events pushed to the frontend UI.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type")]
#[allow(dead_code)]
pub enum UiEvent {
    FocusChanged { state: FocusState },
    PeerConnected { id: String, name: String },
    PeerDisconnected { id: String },
    Log { level: String, message: String },
    FileProgress {
        transfer_id: String,
        file_name: String,
        total_bytes: u64,
        transferred_bytes: u64,
        done: bool,
        direction: String,
    },
    FileReceived {
        file_name: String,
        path: String,
        size: u64,
    },
    PeerDiscovered {
        id: String,
        name: String,
        address: String,
    },
    /// A camera frame received from a peer (base64-encoded JPEG).
    CameraFrame {
        peer_id: String,
        data_b64: String,
    },
    /// An audio chunk received from a peer (base64-encoded WebM/Opus).
    AudioChunk {
        peer_id: String,
        data_b64: String,
    },
}

impl Engine {
    pub fn new(config: AppConfig, ui_events: mpsc::Sender<UiEvent>) -> Self {
        Self {
            config: Arc::new(Mutex::new(config)),
            focus: Arc::new(Mutex::new(FocusState::Local)),
            local_screens: Arc::new(Mutex::new(Vec::new())),
            peers: Arc::new(Mutex::new(HashMap::new())),
            ui_events,
            file_receiver: FileReceiver::new(),
            last_switch_time: Arc::new(Mutex::new(None)),
        }
    }

    /// Called when a local input event is captured.
    /// Returns (peer_id, message) if the event should be forwarded.
    pub async fn handle_local_input(&self, event: InputEvent) -> Option<(PeerId, Message)> {
        let focus = self.focus.lock().await;

        match &*focus {
            FocusState::Local => {
                // Check for screen edge transitions
                if let InputEvent::MouseMove(ref mv) = event {
                    let result = self.check_edge_switch(mv.x, mv.y).await;
                    if result.is_some() {
                        // Cooldown: prevent rapid oscillation between machines.
                        // Without this, in-flight messages and edge-detection races
                        // can cause focus to bounce back and forth continuously.
                        let last = self.last_switch_time.lock().await;
                        if let Some(t) = *last {
                            if t.elapsed() < std::time::Duration::from_millis(250) {
                                return None;
                            }
                        }
                        drop(last);

                        drop(focus);
                        if let Some((ref peer_id, ref msg)) = result {
                            if let Message::SwitchFocus { entry_x, entry_y, .. } = msg {
                                self.switch_to_remote(peer_id, *entry_x, *entry_y).await;
                            }
                        }
                    }
                    return result;
                }
                None
            }
            FocusState::Remote(peer_id) => {
                // Forward input to the remote peer
                let msg = match event {
                    InputEvent::MouseMove(mv) => Message::MouseMove(mv),
                    InputEvent::MouseButton(mb) => Message::MouseButton(mb),
                    InputEvent::MouseScroll(ms) => Message::MouseScroll(ms),
                    InputEvent::Key(ke) => Message::Key(ke),
                };
                Some((peer_id.clone(), msg))
            }
        }
    }

    /// Check if the cursor is at a screen edge and should switch to a neighbor.
    async fn check_edge_switch(&self, x: i32, y: i32) -> Option<(PeerId, Message)> {
        let screens = self.local_screens.lock().await;
        let config = self.config.lock().await;

        if let Some((screen_id, edge_hit, ratio)) = detect_edge(x, y, &screens) {
            let config_edge = match edge_hit {
                EdgeHit::Left => ScreenEdge::Left,
                EdgeHit::Right => ScreenEdge::Right,
                EdgeHit::Top => ScreenEdge::Top,
                EdgeHit::Bottom => ScreenEdge::Bottom,
            };

            for neighbor in &config.neighbors {
                // Match edge direction, and screen_id if specified
                let screen_match = neighbor.screen_id.as_ref()
                    .map(|sid| sid == &screen_id)
                    .unwrap_or(true);
                if neighbor.edge == config_edge && screen_match {
                    let peers = self.peers.lock().await;
                    if let Some(peer) = peers.get(&neighbor.peer_id) {
                        let target_screen = peer.screens.first()?;
                        let (entry_x, entry_y) = match edge_hit {
                            EdgeHit::Right => (
                                target_screen.x,
                                target_screen.y + (ratio * target_screen.height as f64) as i32,
                            ),
                            EdgeHit::Left => (
                                target_screen.x + target_screen.width - 1,
                                target_screen.y + (ratio * target_screen.height as f64) as i32,
                            ),
                            EdgeHit::Bottom => (
                                target_screen.x + (ratio * target_screen.width as f64) as i32,
                                target_screen.y,
                            ),
                            EdgeHit::Top => (
                                target_screen.x + (ratio * target_screen.width as f64) as i32,
                                target_screen.y + target_screen.height - 1,
                            ),
                        };

                        return Some((
                            neighbor.peer_id.clone(),
                            Message::SwitchFocus {
                                target_id: neighbor.peer_id.clone(),
                                entry_x,
                                entry_y,
                            },
                        ));
                    }
                }
            }
        }
        None
    }

    /// Switch focus to a remote peer — starts suppressing local input.
    /// `entry_x`/`entry_y` is the cursor entry point on the remote screen.
    pub async fn switch_to_remote(&self, peer_id: &str, entry_x: i32, entry_y: i32) {
        // Record switch time for cooldown.
        *self.last_switch_time.lock().await = Some(std::time::Instant::now());

        // Get remote screen bounds FIRST, before enabling suppress.
        let peers = self.peers.lock().await;
        let (rs_x, rs_y, rs_w, rs_h) = if let Some(peer) = peers.get(peer_id) {
            if let Some(s) = peer.screens.first() {
                (s.x, s.y, s.width, s.height)
            } else {
                (0, 0, 1920, 1080)
            }
        } else {
            (0, 0, 1920, 1080)
        };
        drop(peers);

        // Initialize virtual cursor tracking BEFORE enabling suppress.
        // This is critical: if suppress is enabled first, the mouse hook
        // immediately starts calculating deltas from the warp center.
        // With stale center/position values (0,0 on first use, or leftover
        // from a previous session), the delta from the real cursor position
        // to the stale center is huge, producing garbage coordinates that
        // make the remote cursor snap wildly across the screen.
        crate::input::init_remote_mouse(entry_x, entry_y, rs_x, rs_y, rs_w, rs_h);

        let mut focus = self.focus.lock().await;
        *focus = FocusState::Remote(peer_id.to_string());
        drop(focus);

        crate::input::set_input_suppression(true);
        crate::diag(format!("Focus → remote {}", &peer_id[..peer_id.len().min(8)]));

        // Push our local clipboard to the remote peer immediately so that Ctrl+V
        // on the remote machine uses our clipboard content rather than its own.
        if let Some(text) = crate::clipboard::sync::get_clipboard_text() {
            let peers = self.peers.lock().await;
            if let Some(peer) = peers.get(peer_id) {
                let _ = peer
                    .sender
                    .send(Message::ClipboardUpdate {
                        content: ClipboardContent::Text(text),
                    })
                    .await;
            }
        }

        let _ = self
            .ui_events
            .send(UiEvent::FocusChanged {
                state: FocusState::Remote(peer_id.to_string()),
            })
            .await;
    }

    /// Switch focus back to local — stops suppressing input.
    pub async fn switch_to_local(&self) {
        // Record switch time for cooldown.
        *self.last_switch_time.lock().await = Some(std::time::Instant::now());

        let mut focus = self.focus.lock().await;
        *focus = FocusState::Local;
        drop(focus);
        crate::input::set_input_suppression(false);
        crate::diag("Focus → local".into());
        let _ = self
            .ui_events
            .send(UiEvent::FocusChanged {
                state: FocusState::Local,
            })
            .await;
    }

    /// Register a connected peer.
    pub async fn add_peer(&self, peer: Peer) {
        let id = peer.id.clone();
        let name = peer.name.clone();
        self.peers.lock().await.insert(id.clone(), peer);
        log::info!("Peer added: {} ({})", name, id);
        let _ = self
            .ui_events
            .send(UiEvent::PeerConnected {
                id: id.clone(),
                name,
            })
            .await;
    }

    /// Remove a disconnected peer.
    pub async fn remove_peer(&self, peer_id: &str) {
        self.peers.lock().await.remove(peer_id);
        // If we were focused on this peer, switch back to local
        let focus = self.focus.lock().await;
        if matches!(&*focus, FocusState::Remote(id) if id == peer_id) {
            drop(focus);
            self.switch_to_local().await;
        }
        log::info!("Peer removed: {}", peer_id);
        let _ = self
            .ui_events
            .send(UiEvent::PeerDisconnected {
                id: peer_id.to_string(),
            })
            .await;
    }

    /// Send a message to a specific peer.
    pub async fn send_to_peer(&self, peer_id: &str, msg: Message) -> Result<(), String> {
        let peers = self.peers.lock().await;
        if let Some(peer) = peers.get(peer_id) {
            peer.sender
                .send(msg)
                .await
                .map_err(|e| format!("Failed to send to peer {}: {}", peer_id, e))
        } else {
            Err(format!("Peer not found: {}", peer_id))
        }
    }

    /// Get current focus state.
    pub async fn get_focus(&self) -> FocusState {
        self.focus.lock().await.clone()
    }

    /// Handle an incoming file transfer message.
    pub async fn handle_file_message(&self, msg: Message) {
        match msg {
            Message::FileStart {
                transfer_id,
                file_name,
                file_size,
            } => {
                match self.file_receiver.start(&transfer_id, &file_name, file_size) {
                    Ok(_path) => {
                        let _ = self
                            .ui_events
                            .send(UiEvent::FileProgress {
                                transfer_id,
                                file_name,
                                total_bytes: file_size,
                                transferred_bytes: 0,
                                done: false,
                                direction: "receive".to_string(),
                            })
                            .await;
                    }
                    Err(e) => log::error!("FileStart error: {}", e),
                }
            }
            Message::FileChunk {
                transfer_id,
                offset,
                data,
            } => {
                let data_len = data.len() as u64;
                match self.file_receiver.write_chunk(&transfer_id, offset, &data) {
                    Ok((received, total, file_name)) => {
                        // Emit progress every ~256KB to avoid flooding
                        if received % (256 * 1024) < data_len || received >= total {
                            let _ = self
                                .ui_events
                                .send(UiEvent::FileProgress {
                                    transfer_id,
                                    file_name,
                                    total_bytes: total,
                                    transferred_bytes: received,
                                    done: false,
                                    direction: "receive".to_string(),
                                })
                                .await;
                        }
                    }
                    Err(e) => log::error!("FileChunk error: {}", e),
                }
            }
            Message::FileDone { transfer_id } => {
                match self.file_receiver.finish(&transfer_id) {
                    Ok((file_name, path, size)) => {
                        let _ = self
                            .ui_events
                            .send(UiEvent::FileReceived {
                                file_name,
                                path: path.to_string_lossy().to_string(),
                                size,
                            })
                            .await;
                    }
                    Err(e) => log::error!("FileDone error: {}", e),
                }
            }
            Message::FileCancel {
                transfer_id,
                reason,
            } => {
                log::warn!("File transfer cancelled: {} - {}", transfer_id, reason);
                self.file_receiver.cancel(&transfer_id);
            }
            _ => {}
        }
    }
}
