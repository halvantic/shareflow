use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::core::engine::Engine;
use crate::core::protocol::{Message, PROTOCOL_VERSION, MIN_SUPPORTED_PROTOCOL_VERSION};
use crate::core::screen::get_screens;
use crate::network::connection::PeerConnection;

/// Start the TCP/TLS server that accepts incoming peer connections.
///
/// `ready_tx` — if provided, fired after the listener is bound so callers that
/// need to connect to this server can wait for it rather than using a fixed delay.
pub async fn start_server(
    engine: Arc<Engine>,
    tls_config: Arc<rustls::ServerConfig>,
    ready_tx: Option<tokio::sync::oneshot::Sender<()>>,
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

    // Signal the server is ready to accept connections.
    if let Some(tx) = ready_tx {
        let _ = tx.send(());
    }

    let acceptor = TlsAcceptor::from(tls_config);

    loop {
        match listener.accept().await {
            Ok((tcp_stream, addr)) => {
                log::info!("Incoming connection from {}", addr);

                // Disable Nagle's algorithm: focus-switch and input events are
                // small, latency-sensitive packets, and Nagling can hold them
                // back tens of ms waiting to coalesce with more data.
                let _ = tcp_stream.set_nodelay(true);

                let acceptor = acceptor.clone();
                let engine = engine.clone();
                let peer_id = peer_id.clone();
                let machine_name = machine_name.clone();

                tokio::spawn(async move {
                    match acceptor.accept(tcp_stream).await {
                        Ok(tls_stream) => {
                            let conn = PeerConnection::from_server_stream(tls_stream);
                            handle_peer_session(conn, engine, peer_id, machine_name).await;
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

/// Handle a connected peer session: handshake, optional auth, then message loop.
async fn handle_peer_session(
    mut conn: PeerConnection,
    engine: Arc<Engine>,
    our_peer_id: String,
    our_name: String,
) {
    let screens = get_screens();

    // Send Hello (handshake).
    let hello = Message::Hello {
        protocol_version: PROTOCOL_VERSION,
        peer_id: our_peer_id.clone(),
        name: our_name.clone(),
        screens: screens.clone(),
    };
    if conn.outgoing.send(hello).await.is_err() {
        return;
    }

    // Wait for Hello or HelloAck from the remote peer.
    let (remote_protocol_version, remote_peer_id, remote_name, remote_screens) =
        match conn.incoming.recv().await {
            Some(Message::Hello {
                protocol_version,
                peer_id,
                name,
                screens,
            })
            | Some(Message::HelloAck {
                protocol_version,
                peer_id,
                name,
                screens,
            }) => {
                if protocol_version < MIN_SUPPORTED_PROTOCOL_VERSION {
                    log::error!(
                        "Rejecting peer {}: protocol version {} below minimum {}",
                        peer_id,
                        protocol_version,
                        MIN_SUPPORTED_PROTOCOL_VERSION
                    );
                    return;
                }
                (protocol_version, peer_id, name, screens)
            }
            _ => {
                log::error!("Expected Hello/HelloAck from peer");
                return;
            }
        };

    // Auth challenge/response (protocol v2+).
    // Only performed when the remote peer also speaks v2 — old peers skip this.
    if remote_protocol_version >= 2 {
        let pairing_code = engine.config.lock().await.pairing_code.clone();
        if pairing_code.is_empty() {
            // No pairing code — signal the client to proceed without auth.
            let _ = conn.outgoing.send(Message::AuthResult { success: true }).await;
        } else {
            // Send a random 32-byte challenge nonce.
            let nonce: Vec<u8> = (0..32).map(|_| rand::random::<u8>()).collect();
            if conn
                .outgoing
                .send(Message::AuthChallenge { nonce: nonce.clone() })
                .await
                .is_err()
            {
                return;
            }
            // Wait for the client's HMAC response (10 s timeout).
            let auth_ok = match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                conn.incoming.recv(),
            )
            .await
            {
                Ok(Some(Message::AuthResponse { hash })) => {
                    crate::network::auth::verify_auth_hmac(&pairing_code, &nonce, &hash)
                }
                _ => false,
            };
            let _ = conn
                .outgoing
                .send(Message::AuthResult { success: auth_ok })
                .await;
            if !auth_ok {
                log::warn!(
                    "Peer {} failed authentication — wrong pairing code or timeout",
                    remote_peer_id
                );
                return;
            }
            log::info!("Peer {} authenticated successfully", remote_peer_id);
        }
    }

    // Send HelloAck (after auth so the client knows auth passed).
    let ack = Message::HelloAck {
        protocol_version: PROTOCOL_VERSION,
        peer_id: our_peer_id.clone(),
        name: our_name.clone(),
        screens: get_screens(),
    };
    if conn.outgoing.send(ack).await.is_err() {
        return;
    }

    log::info!(
        "Peer connected: {} ({}), {} screens",
        remote_name,
        remote_peer_id,
        remote_screens.len()
    );

    // Hand off to the shared session loop (peer registration, message loop, cleanup).
    crate::network::session::run_peer_session(
        conn,
        engine,
        our_peer_id,
        remote_peer_id,
        remote_name,
        remote_screens,
        None,
    )
    .await;
}
