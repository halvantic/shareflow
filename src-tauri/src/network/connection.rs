use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::client::TlsStream as ClientTlsStream;
use tokio_rustls::server::TlsStream as ServerTlsStream;
use tokio_rustls::TlsConnector;

use crate::core::protocol::{decode_message, encode_message, Message};

/// Maximum bytes to write in a single flush before checking the hi-priority
/// channel. Large messages (clipboard images, file chunks) are broken into
/// slices of this size so that urgent mouse/key events can be interleaved.
const CHUNK_SIZE: usize = 64 * 1024; // 64 KB

/// A bidirectional connection to a peer, wrapping a TLS stream.
pub struct PeerConnection {
    /// High-priority outgoing channel (mouse, key, focus, ping/pong).
    pub outgoing: mpsc::Sender<Message>,
    /// Low-priority outgoing channel (clipboard, files, screen updates, config).
    pub outgoing_lo: mpsc::Sender<Message>,
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
        let (hi_tx, mut hi_rx) = mpsc::channel::<Message>(256);
        let (lo_tx, mut lo_rx) = mpsc::channel::<Message>(64);
        let (in_tx, in_rx) = mpsc::channel::<Message>(256);

        // Writer task: drains hi-priority first, then lo-priority.
        // Large messages are written in chunks so hi-priority events can
        // slip through between chunk flushes.
        tokio::spawn(async move {
            loop {
                // Always drain all queued hi-priority messages first.
                while let Ok(msg) = hi_rx.try_recv() {
                    if write_message(&mut writer, &msg).await.is_err() {
                        return;
                    }
                }

                // Biased select: prefer hi, fall back to lo.
                tokio::select! {
                    biased;
                    msg = hi_rx.recv() => {
                        match msg {
                            Some(m) => {
                                if write_message(&mut writer, &m).await.is_err() {
                                    return;
                                }
                            }
                            None => return, // channel closed
                        }
                    }
                    msg = lo_rx.recv() => {
                        match msg {
                            Some(m) => {
                                if write_message_chunked(&mut writer, &m, &mut hi_rx).await.is_err() {
                                    return;
                                }
                            }
                            None => return,
                        }
                    }
                }
            }
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
            outgoing: hi_tx,
            outgoing_lo: lo_tx,
            incoming: in_rx,
        }
    }
}

/// Write a single message to the stream (used for hi-priority).
async fn write_message<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &Message,
) -> Result<(), ()> {
    match encode_message(msg) {
        Ok(data) => {
            if writer.write_all(&data).await.is_err() {
                return Err(());
            }
            Ok(())
        }
        Err(e) => {
            log::error!("Failed to encode message: {}", e);
            Err(())
        }
    }
}

/// Write a message in chunks, checking the hi-priority channel between each
/// chunk so urgent events can be interleaved.
async fn write_message_chunked<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &Message,
    hi_rx: &mut mpsc::Receiver<Message>,
) -> Result<(), ()> {
    let data = match encode_message(msg) {
        Ok(d) => d,
        Err(e) => {
            log::error!("Failed to encode message: {}", e);
            return Err(());
        }
    };

    // Small messages (< 2 chunks) — just write directly, no point chunking.
    if data.len() <= CHUNK_SIZE * 2 {
        if writer.write_all(&data).await.is_err() {
            return Err(());
        }
        return Ok(());
    }

    // Large message — write in chunks, flushing hi-priority between each.
    let mut offset = 0;
    while offset < data.len() {
        let end = (offset + CHUNK_SIZE).min(data.len());
        if writer.write_all(&data[offset..end]).await.is_err() {
            return Err(());
        }
        offset = end;

        // Drain any hi-priority messages that queued up during the write.
        while let Ok(hi_msg) = hi_rx.try_recv() {
            if write_message(writer, &hi_msg).await.is_err() {
                return Err(());
            }
        }
    }
    Ok(())
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
