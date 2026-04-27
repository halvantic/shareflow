use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

use crate::core::engine::Engine;
use crate::core::protocol::{Message, MouseMoveEvent, ScreenInfo};
use crate::network::connection::PeerConnection;

/// Sentinel x-coordinate used to skip move injection after mouse-move coalescing.
const SKIP_MOVE_SENTINEL: i32 = i32::MIN;

/// Token-bucket rate limiter for incoming input events.
/// Allows up to 200 events/sec sustained with a burst of 50.
/// Excess events are dropped and a warning is logged (at most once per second).
struct InputRateLimiter {
    tokens: f64,
    last_refill: std::time::Instant,
    violations: u64,
    last_violation_log: std::time::Instant,
}

const INPUT_RATE_PER_SEC: f64 = 200.0;
const INPUT_RATE_BURST: f64 = 50.0;

impl InputRateLimiter {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Self { tokens: INPUT_RATE_BURST, last_refill: now, violations: 0, last_violation_log: now }
    }

    fn allow(&mut self, peer_id: &str) -> bool {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        self.last_refill = now;
        self.tokens = (self.tokens + elapsed * INPUT_RATE_PER_SEC).min(INPUT_RATE_BURST);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            self.violations += 1;
            if now.duration_since(self.last_violation_log).as_secs() >= 1 {
                log::warn!(
                    "Peer {} exceeded input rate limit: {} events dropped in last second",
                    peer_id, self.violations
                );
                self.violations = 0;
                self.last_violation_log = now;
            }
            false
        }
    }
}

/// Run a complete peer session to completion.
///
/// Registers the peer with the engine, sets up forwarding and keepalive tasks,
/// processes the incoming message loop, then removes the peer on disconnect.
///
/// `peer_registered_tx` — if provided, signalled immediately after
/// `engine.add_peer()` completes, allowing the caller to return a success
/// result without waiting for the session to end.
pub async fn run_peer_session(
    conn: PeerConnection,
    engine: Arc<Engine>,
    our_peer_id: String,
    remote_peer_id: String,
    remote_name: String,
    remote_screens: Vec<ScreenInfo>,
    peer_registered_tx: Option<tokio::sync::oneshot::Sender<()>>,
) {
    // 1. Register the peer so the engine and UI know it is connected.
    let (msg_tx, mut msg_rx) = mpsc::channel(256);
    let (msg_lo_tx, mut msg_lo_rx) = mpsc::channel(64);
    let peer = crate::core::engine::Peer {
        id: remote_peer_id.clone(),
        name: remote_name,
        screens: remote_screens,
        sender: msg_tx,
        sender_lo: msg_lo_tx,
    };
    engine.add_peer(peer).await;

    // Signal the caller that add_peer has completed.
    if let Some(tx) = peer_registered_tx {
        let _ = tx.send(());
    }

    // 2. Push current host settings to the peer (host-only, in-memory).
    {
        let cfg = engine.config.lock().await;
        if !cfg.agent_mode {
            let sync = Message::ConfigSync {
                clipboard_sync_enabled: cfg.clipboard_sync_enabled,
            };
            drop(cfg);
            let _ = engine.send_to_peer_lo(&remote_peer_id, sync).await;
        }
    }

    // 3. Spawn forwarding tasks: engine channels → network connection.
    let outgoing_hi = conn.outgoing.clone();
    tokio::spawn(async move {
        while let Some(msg) = msg_rx.recv().await {
            if outgoing_hi.send(msg).await.is_err() {
                break;
            }
        }
    });
    let outgoing_lo = conn.outgoing_lo.clone();
    tokio::spawn(async move {
        while let Some(msg) = msg_lo_rx.recv().await {
            if outgoing_lo.send(msg).await.is_err() {
                break;
            }
        }
    });

    // 4. Keepalive: ping every 5 s, disconnect after 6 consecutive missed pongs.
    // (Increased threshold to tolerate tokio task scheduling delays when the app is backgrounded.)
    let ping_tx = conn.outgoing.clone();
    let pong_received = Arc::new(AtomicBool::new(true));
    let pong_flag = Arc::clone(&pong_received);
    tokio::spawn(async move {
        let mut missed = 0u32;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            if !pong_flag.load(Ordering::SeqCst) {
                missed += 1;
                if missed >= 6 {
                    log::warn!(
                        "Peer failed to respond to 6 consecutive pings, closing connection"
                    );
                    break;
                }
            } else {
                missed = 0;
            }
            pong_flag.store(false, Ordering::SeqCst);
            if ping_tx.send(Message::Ping).await.is_err() {
                break;
            }
        }
    });

    // 5. Incoming message loop.
    let injector = crate::input::create_injector();
    let pong_tx = conn.outgoing.clone();
    let mut incoming = conn.incoming;
    let mut rate_limiter = InputRateLimiter::new();

    while let Some(msg) = incoming.recv().await {
        match msg {
            Message::MouseMove(mut mv) => {
                if !rate_limiter.allow(&remote_peer_id) {
                    continue;
                }
                if engine.is_remote.load(Ordering::Acquire) {
                    continue;
                }
                // Coalesce: drain any buffered mouse moves and jump to the latest.
                while let Ok(next) = incoming.try_recv() {
                    match next {
                        Message::MouseMove(newer) => mv = newer,
                        other => {
                            // Inject the coalesced move first.
                            let _ = injector.move_mouse(mv.x, mv.y);
                            let ev = crate::input::InputEvent::MouseMove(mv);
                            if let Some((pid, fwd)) = engine.handle_local_input(ev).await {
                                if let Err(e) = engine.send_to_peer(&pid, fwd).await {
                                    log::warn!("Failed to send edge switch: {}", e);
                                }
                            }
                            // Handle simple input messages synchronously (no await needed).
                            // Complex protocol messages that appear here are rare and are
                            // handled on the next outer loop iteration via `_ => {}`.
                            match other {
                                Message::MouseButton(mb) => {
                                    let _ = injector.press_mouse_button(mb.button, mb.pressed);
                                }
                                Message::MouseScroll(ms) => {
                                    let _ = injector.scroll(ms.dx, ms.dy);
                                }
                                Message::Key(ke) => {
                                    crate::diag(format!(
                                        "RX key sc=0x{:X} pressed={}",
                                        ke.scancode, ke.pressed
                                    ));
                                    let _ = injector.send_key(ke.scancode, ke.pressed);
                                }
                                _ => {} // handled in next outer iteration
                            }
                            mv = MouseMoveEvent {
                                x: SKIP_MOVE_SENTINEL,
                                y: SKIP_MOVE_SENTINEL,
                            };
                            break;
                        }
                    }
                }
                if mv.x != SKIP_MOVE_SENTINEL {
                    let _ = injector.move_mouse(mv.x, mv.y);
                    let ev = crate::input::InputEvent::MouseMove(mv);
                    if let Some((pid, fwd)) = engine.handle_local_input(ev).await {
                        if let Err(e) = engine.send_to_peer(&pid, fwd).await {
                            log::warn!("Failed to send edge switch: {}", e);
                        }
                    }
                }
            }
            Message::MouseButton(mb) => {
                if !rate_limiter.allow(&remote_peer_id) {
                    continue;
                }
                if !engine.is_remote.load(Ordering::Acquire) {
                    if let Err(e) = injector.press_mouse_button(mb.button, mb.pressed) {
                        log::error!("Mouse button injection failed: {}", e);
                    }
                }
            }
            Message::MouseScroll(ms) => {
                if !rate_limiter.allow(&remote_peer_id) {
                    continue;
                }
                if !engine.is_remote.load(Ordering::Acquire) {
                    if let Err(e) = injector.scroll(ms.dx, ms.dy) {
                        log::error!("Scroll injection failed: {}", e);
                    }
                }
            }
            Message::Key(ke) => {
                if !rate_limiter.allow(&remote_peer_id) {
                    continue;
                }
                if !engine.is_remote.load(Ordering::Acquire) {
                    crate::diag(format!(
                        "RX key sc=0x{:X} pressed={}",
                        ke.scancode, ke.pressed
                    ));
                    if let Err(e) = injector.send_key(ke.scancode, ke.pressed) {
                        log::error!("Key injection failed: {}", e);
                    }
                }
            }
            Message::SwitchFocus {
                target_id,
                entry_x,
                entry_y,
            } => {
                if target_id == our_peer_id {
                    let _ = injector.move_mouse(entry_x, entry_y);
                    engine.switch_to_local().await;
                    crate::input::reprime_keyboard_for_focus();
                    log::info!("Received focus at ({}, {})", entry_x, entry_y);
                }
            }
            Message::ClipboardUpdate { content } => {
                let enabled = engine.config.lock().await.clipboard_sync_enabled;
                if enabled {
                    crate::clipboard::sync::apply_remote_clipboard(content);
                }
            }
            Message::ClipboardUpdateCompressed {
                width,
                height,
                compressed_rgba,
                original_len,
            } => {
                let enabled = engine.config.lock().await.clipboard_sync_enabled;
                if enabled {
                    match crate::core::protocol::decompress_clipboard(
                        width,
                        height,
                        compressed_rgba,
                        original_len,
                    ) {
                        Ok(content) => crate::clipboard::sync::apply_remote_clipboard(content),
                        Err(e) => log::error!("Failed to decompress clipboard: {}", e),
                    }
                }
            }
            Message::ConfigSync {
                clipboard_sync_enabled,
            } => {
                let mut cfg = engine.config.lock().await;
                if cfg.agent_mode {
                    cfg.clipboard_sync_enabled = clipboard_sync_enabled;
                    log::debug!(
                        "ConfigSync from host: clipboard_sync_enabled={}",
                        clipboard_sync_enabled
                    );
                }
            }
            Message::ScreenUpdate { screens } => {
                engine.update_peer_screens(&remote_peer_id, screens).await;
            }
            Message::AutoNeighbor {
                peer_id,
                edge,
                remove,
            } => {
                let screen_edge = match edge.as_str() {
                    "left" => crate::core::config::ScreenEdge::Left,
                    "right" => crate::core::config::ScreenEdge::Right,
                    "top" => crate::core::config::ScreenEdge::Top,
                    "bottom" => crate::core::config::ScreenEdge::Bottom,
                    _ => {
                        log::warn!("AutoNeighbor: invalid edge '{}'", edge);
                        continue;
                    }
                };
                let mut cfg = engine.config.lock().await;
                if remove {
                    cfg.neighbors.retain(|n| {
                        !(n.peer_id == peer_id
                            && n.edge == screen_edge
                            && n.screen_id.is_none())
                    });
                } else {
                    cfg.neighbors
                        .retain(|n| !(n.edge == screen_edge && n.screen_id.is_none()));
                    cfg.neighbors.push(crate::core::config::Neighbor {
                        peer_id,
                        edge: screen_edge,
                        screen_id: None,
                    });
                }
                cfg.save();
            }
            Message::Ping => {
                let _ = pong_tx.send(Message::Pong).await;
            }
            Message::Pong => {
                pong_received.store(true, Ordering::SeqCst);
            }
            msg @ Message::FileStart { .. }
            | msg @ Message::FileChunk { .. }
            | msg @ Message::FileDone { .. }
            | msg @ Message::FileCancel { .. }
            | msg @ Message::FileIntegrity { .. } => {
                engine.handle_file_message(msg).await;
            }
            _ => {}
        }
    }

    engine.remove_peer(&remote_peer_id).await;
}
