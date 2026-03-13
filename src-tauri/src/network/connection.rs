use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::client::TlsStream as ClientTlsStream;
use tokio_rustls::server::TlsStream as ServerTlsStream;
use tokio_rustls::TlsConnector;

use crate::core::protocol::{decode_message, encode_message, Message};

/// A bidirectional connection to a peer, wrapping a TLS stream.
pub struct PeerConnection {
    /// Channel to send outgoing messages (written to the stream by a background task).
    pub outgoing: mpsc::Sender<Message>,
    /// Channel to receive incoming messages (read from the stream by a background task).
    pub incoming: mpsc::Receiver<Message>,
}

impl PeerConnection {
    /// Wrap a server-side TLS stream into a PeerConnection.
    pub fn from_server_stream(stream: ServerTlsStream<TcpStream>) -> Self {
        let (read_half, write_half) = tokio::io::split(stream);
        Self::from_split(read_half, write_half)
    }

    /// Wrap a client-side TLS stream into a PeerConnection.
    pub fn from_client_stream(stream: ClientTlsStream<TcpStream>) -> Self {
        let (read_half, write_half) = tokio::io::split(stream);
        Self::from_split(read_half, write_half)
    }

    fn from_split<R, W>(mut reader: R, mut writer: W) -> Self
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (out_tx, mut out_rx) = mpsc::channel::<Message>(256);
        let (in_tx, in_rx) = mpsc::channel::<Message>(256);

        // Writer task: sends outgoing messages.
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                match encode_message(&msg) {
                    Ok(data) => {
                        if writer.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to encode message: {}", e);
                        break;
                    }
                }
            }
            log::info!("Writer task ended");
        });

        // Reader task: receives incoming messages.
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            let mut pending = Vec::new();
            const MAX_PENDING: usize = 16 * 1024 * 1024; // 16 MB limit

            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => {
                        log::info!("Connection closed by peer");
                        break;
                    }
                    Ok(n) => {
                        pending.extend_from_slice(&buf[..n]);

                        if pending.len() > MAX_PENDING {
                            log::error!("Pending buffer exceeded {} bytes, disconnecting", MAX_PENDING);
                            break;
                        }

                        // Decode as many complete messages as we can.
                        loop {
                            match decode_message(&pending) {
                                Ok(Some((msg, consumed))) => {
                                    pending.drain(..consumed);
                                    if in_tx.send(msg).await.is_err() {
                                        return;
                                    }
                                }
                                Ok(None) => break, // Need more data.
                                Err(e) => {
                                    log::error!("Failed to decode message: {}", e);
                                    return;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log::error!("Read error: {}", e);
                        break;
                    }
                }
            }
            log::info!("Reader task ended");
        });

        Self {
            outgoing: out_tx,
            incoming: in_rx,
        }
    }
}

/// Connect to a remote peer as a client (with 10-second timeout).
pub async fn connect_to_peer(
    addr: &str,
    tls_config: Arc<rustls::ClientConfig>,
) -> Result<PeerConnection, String> {
    let stream = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        TcpStream::connect(addr),
    )
    .await
    .map_err(|_| format!("Connection timed out after 10 seconds"))?
    .map_err(|e| format!("TCP connect failed: {}", e))?;

    let server_name = rustls::pki_types::ServerName::try_from("shareflow.local")
        .map_err(|e| format!("Invalid server name: {}", e))?;

    let connector = TlsConnector::from(tls_config);
    let tls_stream = connector
        .connect(server_name, stream)
        .await
        .map_err(|e| format!("TLS handshake failed: {}", e))?;

    Ok(PeerConnection::from_client_stream(tls_stream))
}
