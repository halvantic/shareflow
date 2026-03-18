use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;

use crate::core::engine::{Engine, FocusState};
use crate::core::protocol::Message;
use crate::core::screen::get_screens;
use crate::network::connection::PeerConnection;

/// Sentinel value used to signal a skipped mouse-move injection after coalescing.
/// Using a named constant makes the intent clear and avoids the raw sentinel pitfall.
const SKIP_MOVE_SENTINEL: i32 = i32::MIN;

/// Start the TCP/TLS server that accepts incoming peer connections.
pub async fn start_server(
    engine: Arc<Engine>,
    tls_config: Arc<rustls::ServerConfig>,
) -> Result<(), String> {
    let config = engine.config.lock().await;
    let port = config.port;
    let peer_id = config.peer_id.clone();
    let machine_name = config.machine_name.clone();
    drop(config);

    let bind_addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&bind_addr)
        .await
        .map_err(|e| format!("Failed to bind {}: {}", bind_addr, e))?;

    log::info!("Server listening on {}", bind_addr);

    let acceptor = TlsAcceptor::from(tls_config);

    loop {
        match listener.accept().await {
            Ok((tcp_stream, addr)) => {
                log::info!("Incoming connection from {}", addr);

                let acceptor = acceptor.clone();
                let engine = engine.clone();
                let peer_id = peer_id.clone();
                let machine_name = machine_name.clone();

                tokio::spawn(async move {
                    match acceptor.accept(tcp_stream).await {
                        Ok(tls_stream) => {
                            let mut conn = PeerConnection::from_server_stream(tls_stream);
                            handle_peer_session(
                                &mut conn,
                                engine,
                                peer_id,
                                machine_name,
                            )
                            .await;
                        }
                        Err(e) => {
                            log::error!("TLS accept failed from {}: {}", addr, e);
                        }
                    }
                });
            }
            Err(e) => {
                log::error!("Accept failed: {}", e);
            }
        }
    }
}

/// Handle a connected peer session: handshake then message loop.
async fn handle_peer_session(
    conn: &mut PeerConnection,
    engine: Arc<Engine>,
    our_peer_id: String,
    our_name: String,
) {
    let screens = get_screens();

    // Send Hello
    let hello = Message::Hello {
        peer_id: our_peer_id.clone(),
        name: our_name.clone(),
        screens: screens.clone(),
    };
    if conn.outgoing.send(hello).await.is_err() {
        return;
    }

    // Wait for HelloAck or Hello from remote
    let (remote_peer_id, remote_name, remote_screens) = match conn.incoming.recv().await {
        Some(Message::Hello {
            peer_id,
            name,
            screens,
        })
        | Some(Message::HelloAck {
            peer_id,
            name,
            screens,
        }) => {
            // Send our HelloAck if they sent Hello
            let ack = Message::HelloAck {
                peer_id: our_peer_id.clone(),
                name: our_name.clone(),
                screens: get_screens(),
            };
            let _ = conn.outgoing.send(ack).await;
            (peer_id, name, screens)
        }
        _ => {
            log::error!("Expected Hello/HelloAck from peer");
            return;
        }
    };

    log::info!(
        "Peer connected: {} ({}), {} screens",
        remote_name,
        remote_peer_id,
        remote_screens.len()
    );

    // Register the peer in the engine.
    let (msg_tx, mut msg_rx) = mpsc::channel(256);
    let peer = crate::core::engine::Peer {
        id: remote_peer_id.clone(),
        name: remote_name,
        screens: remote_screens,
        sender: msg_tx,
    };
    engine.add_peer(peer).await;

    // If we are the host (not in agent mode), push our current settings to
    // the newly connected peer so it immediately honours our configuration.
    {
        let cfg = engine.config.lock().await;
        if !cfg.agent_mode {
            let sync = Message::ConfigSync {
                clipboard_sync_enabled: cfg.clipboard_sync_enabled,
            };
            drop(cfg);
            let _ = engine.send_to_peer(&remote_peer_id, sync).await;
        }
    }

    // Forward outgoing messages from engine to connection.
    let outgoing = conn.outgoing.clone();
    tokio::spawn(async move {
        while let Some(msg) = msg_rx.recv().await {
            if outgoing.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Keepalive: send Ping every 5 seconds, detect dead peers via Pong timeout.
    let ping_outgoing = conn.outgoing.clone();
    let pong_received = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let pong_flag = pong_received.clone();
    tokio::spawn(async move {
        let mut missed = 0u32;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            if !pong_flag.load(std::sync::atomic::Ordering::SeqCst) {
                missed += 1;
                if missed >= 3 {
                    log::warn!("Peer failed to respond to 3 consecutive pings, closing connection");
                    break;
                }
            } else {
                missed = 0;
            }
            pong_flag.store(false, std::sync::atomic::Ordering::SeqCst);
            if ping_outgoing.send(Message::Ping).await.is_err() {
                break;
            }
        }
    });

    // Process incoming messages from the peer.
    let injector = crate::input::create_injector();
    while let Some(msg) = conn.incoming.recv().await {
        match msg {
            Message::MouseMove(mut mv) => {
                // Only inject received input when we have local focus (being
                // controlled by the remote peer). If focus is Remote, these
                // are stale in-flight events that arrived after an edge switch.
                // Without this guard, they get forwarded back via
                // handle_local_input's Remote branch, creating a feedback loop
                // of bouncing coordinates between the two machines.
                if engine.get_focus().await != FocusState::Local {
                    continue;
                }
                // Coalesce: drain any queued mouse moves and jump to the latest
                // position. This avoids processing stale positions when events
                // arrive in bursts over the network.
                while let Ok(next) = conn.incoming.try_recv() {
                    match next {
                        Message::MouseMove(newer) => mv = newer,
                        other => {
                            // Non-mouse message — process the coalesced move first,
                            // then handle this message on the next loop iteration.
                            let _ = injector.move_mouse(mv.x, mv.y);
                            let edge_event = crate::input::InputEvent::MouseMove(mv);
                            if let Some((peer_id, msg)) = engine.handle_local_input(edge_event).await {
                                if let Err(e) = engine.send_to_peer(&peer_id, msg).await {
                                    log::warn!("Failed to send edge switch: {}", e);
                                }
                            }
                            // Re-process the non-mouse message
                            match other {
                                Message::MouseButton(mb) => {
                                    let _ = injector.press_mouse_button(mb.button, mb.pressed);
                                }
                                Message::MouseScroll(ms) => {
                                    let _ = injector.scroll(ms.dx, ms.dy);
                                }
                                Message::Key(ke) => {
                                    crate::diag(format!("RX key sc=0x{:X} pressed={}", ke.scancode, ke.pressed));
                                    let _ = injector.send_key(ke.scancode, ke.pressed);
                                }
                                _ => {} // Other messages handled below in main match
                            }
                            // Use a sentinel to skip the move injection below
                            mv = crate::core::protocol::MouseMoveEvent { x: SKIP_MOVE_SENTINEL, y: SKIP_MOVE_SENTINEL };
                            break;
                        }
                    }
                }
                if mv.x != SKIP_MOVE_SENTINEL {
                    let _ = injector.move_mouse(mv.x, mv.y);
                    let edge_event = crate::input::InputEvent::MouseMove(mv);
                    if let Some((peer_id, msg)) = engine.handle_local_input(edge_event).await {
                        if let Err(e) = engine.send_to_peer(&peer_id, msg).await {
                            log::warn!("Failed to send edge switch: {}", e);
                        }
                    }
                }
            }
            Message::MouseButton(mb) => {
                if engine.get_focus().await != FocusState::Local {
                    continue;
                }
                if let Err(e) = injector.press_mouse_button(mb.button, mb.pressed) {
                    log::error!("Mouse button injection failed: {}", e);
                }
            }
            Message::MouseScroll(ms) => {
                if engine.get_focus().await != FocusState::Local {
                    continue;
                }
                if let Err(e) = injector.scroll(ms.dx, ms.dy) {
                    log::error!("Scroll injection failed: {}", e);
                }
            }
            Message::Key(ke) => {
                if engine.get_focus().await != FocusState::Local {
                    continue;
                }
                crate::diag(format!("RX key sc=0x{:X} pressed={}", ke.scancode, ke.pressed));
                if let Err(e) = injector.send_key(ke.scancode, ke.pressed) {
                    log::error!("Key injection failed: {}", e);
                }
            }
            Message::SwitchFocus {
                target_id,
                entry_x,
                entry_y,
            } => {
                if target_id == our_peer_id {
                    // We're getting focus — place cursor at entry point.
                    let _ = injector.move_mouse(entry_x, entry_y);
                    engine.switch_to_local().await;
                    // Re-warm the HID keyboard pipeline so that the first key
                    // event from the remote peer is delivered immediately.
                    crate::input::reprime_keyboard_for_focus();
                    log::info!("Received focus at ({}, {})", entry_x, entry_y);
                }
            }
            Message::ClipboardUpdate { content } => {
                crate::clipboard::sync::apply_remote_clipboard(content);
            }
            Message::ConfigSync { clipboard_sync_enabled } => {
                // Only agents apply host-pushed settings; hosts ignore this.
                let mut cfg = engine.config.lock().await;
                if cfg.agent_mode {
                    cfg.clipboard_sync_enabled = clipboard_sync_enabled;
                    // Not saved — host settings are applied in-memory only.
                    log::debug!("ConfigSync from host: clipboard_sync_enabled={}", clipboard_sync_enabled);
                }
            }
            Message::CameraFrame { data } => {
                use base64::engine::Engine as _;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                let _ = engine
                    .ui_events
                    .send(crate::core::engine::UiEvent::CameraFrame {
                        peer_id: remote_peer_id.clone(),
                        data_b64: b64,
                    })
                    .await;
            }
            Message::AudioChunk { data } => {
                use base64::engine::Engine as _;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                let _ = engine
                    .ui_events
                    .send(crate::core::engine::UiEvent::AudioChunk {
                        peer_id: remote_peer_id.clone(),
                        data_b64: b64,
                    })
                    .await;
            }
            Message::ScreenUpdate { screens } => {
                engine.update_peer_screens(&remote_peer_id, screens).await;
            }
            Message::AutoNeighbor { peer_id, edge, remove } => {
                let screen_edge = match edge.as_str() {
                    "left" => crate::core::config::ScreenEdge::Left,
                    "right" => crate::core::config::ScreenEdge::Right,
                    "top" => crate::core::config::ScreenEdge::Top,
                    "bottom" => crate::core::config::ScreenEdge::Bottom,
                    _ => { log::warn!("AutoNeighbor: invalid edge '{}'", edge); continue; }
                };
                let mut cfg = engine.config.lock().await;
                if remove {
                    cfg.neighbors.retain(|n| !(n.peer_id == peer_id && n.edge == screen_edge && n.screen_id.is_none()));
                } else {
                    cfg.neighbors.retain(|n| !(n.edge == screen_edge && n.screen_id.is_none()));
                    cfg.neighbors.push(crate::core::config::Neighbor { peer_id, edge: screen_edge, screen_id: None });
                }
                cfg.save();
            }
            Message::Ping => {
                let _ = conn.outgoing.send(Message::Pong).await;
            }
            Message::Pong => {
                pong_received.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            msg @ Message::FileStart { .. }
            | msg @ Message::FileChunk { .. }
            | msg @ Message::FileDone { .. }
            | msg @ Message::FileCancel { .. } => {
                engine.handle_file_message(msg).await;
            }
            _ => {}
        }
    }

    // Peer disconnected.
    engine.remove_peer(&remote_peer_id).await;
}
