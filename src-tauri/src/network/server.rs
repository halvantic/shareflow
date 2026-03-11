use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;

use crate::core::engine::{Engine, FocusState};
use crate::core::protocol::Message;
use crate::core::screen::get_screens;
use crate::network::connection::PeerConnection;

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
        name: our_name,
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
                name: "".into(),
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

    // Forward outgoing messages from engine to connection.
    let outgoing = conn.outgoing.clone();
    tokio::spawn(async move {
        while let Some(msg) = msg_rx.recv().await {
            if outgoing.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Keepalive: send Ping every 5 seconds.
    let ping_outgoing = conn.outgoing.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
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
                            mv = crate::core::protocol::MouseMoveEvent { x: i32::MIN, y: i32::MIN };
                            break;
                        }
                    }
                }
                if mv.x != i32::MIN {
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
                    log::info!("Received focus at ({}, {})", entry_x, entry_y);
                }
            }
            Message::ClipboardUpdate { content } => {
                crate::clipboard::sync::apply_remote_clipboard(content);
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
            Message::Ping => {
                let _ = conn.outgoing.send(Message::Pong).await;
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
