use std::collections::HashMap;
use std::net::UdpSocket;
use std::time::{Duration, Instant};
use serde::{Deserialize, Serialize};

const DEFAULT_DISCOVERY_PORT: u16 = 24801;
const MAGIC: &[u8; 4] = b"SFLO";

/// Broadcast announcement for LAN discovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Announcement {
    pub peer_id: String,
    pub name: String,
    pub port: u16,
    /// Discovery port used for broadcasts. Not serialized in the announcement itself.
    #[serde(skip)]
    pub discovery_port: u16,
    /// Timestamp to prevent replay attacks. Seconds since UNIX epoch.
    #[serde(default)]
    pub timestamp: u64,
    /// SHA-256 fingerprint of the sender's TLS certificate, colon-separated hex.
    /// Allows receivers to pre-verify identity before attempting a TLS connection.
    #[serde(default)]
    pub cert_fingerprint: String,
    /// Per-broadcast random UUID nonce.
    /// Receivers reject any (peer_id, nonce) pair seen in the last 60 s, preventing
    /// hot replay where an attacker re-broadcasts fresh packets every ~25 s.
    #[serde(default)]
    pub nonce: String,
}

/// Maximum age (in seconds) of a discovery announcement before it's discarded.
const MAX_ANNOUNCEMENT_AGE_SECS: u64 = 30;
/// How long a (peer_id, nonce) pair is remembered to reject duplicate broadcasts.
const NONCE_WINDOW_SECS: u64 = 60;

/// Broadcast our presence on the LAN.
pub fn broadcast_presence(announcement: &Announcement) -> Result<(), String> {
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    socket
        .set_broadcast(true)
        .map_err(|e| e.to_string())?;

    // Include current timestamp and a fresh random nonce for replay protection.
    let mut ann = announcement.clone();
    ann.timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    ann.nonce = uuid::Uuid::new_v4().to_string();

    let payload = serde_json::to_vec(&ann).map_err(|e| e.to_string())?;
    let mut packet = Vec::with_capacity(4 + payload.len());
    packet.extend_from_slice(MAGIC);
    packet.extend_from_slice(&payload);

    let port = if announcement.discovery_port > 0 {
        announcement.discovery_port
    } else {
        DEFAULT_DISCOVERY_PORT
    };
    let broadcast_addr = format!("255.255.255.255:{}", port);
    socket
        .send_to(&packet, &broadcast_addr)
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// Listen for peer announcements on the LAN.
/// Calls the callback for each discovered peer.
/// This is blocking and should be run in a dedicated thread.
pub fn listen_for_peers(
    own_peer_id: &str,
    discovery_port: u16,
    callback: impl Fn(Announcement, std::net::SocketAddr),
) -> Result<(), String> {
    let port = if discovery_port > 0 { discovery_port } else { DEFAULT_DISCOVERY_PORT };
    let socket = UdpSocket::bind(format!("0.0.0.0:{}", port))
        .map_err(|e| format!("Failed to bind discovery socket: {}", e))?;
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;

    // Track (peer_id, nonce) → first-seen Instant to reject hot-replay duplicates.
    let mut seen_nonces: HashMap<(String, String), Instant> = HashMap::new();

    let mut buf = [0u8; 4096];
    loop {
        // Evict nonces older than the replay window on each iteration.
        let now_instant = Instant::now();
        seen_nonces.retain(|_, t| {
            now_instant.duration_since(*t).as_secs() < NONCE_WINDOW_SECS
        });

        match socket.recv_from(&mut buf) {
            Ok((len, addr)) => {
                if len > 4 && &buf[..4] == MAGIC {
                    if let Ok(announcement) =
                        serde_json::from_slice::<Announcement>(&buf[4..len])
                    {
                        if announcement.peer_id != own_peer_id {
                            // Reject stale announcements to limit replay window.
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs();
                            if announcement.timestamp > 0
                                && now.abs_diff(announcement.timestamp) > MAX_ANNOUNCEMENT_AGE_SECS
                            {
                                log::debug!("Discarding stale announcement from {}", announcement.peer_id);
                                continue;
                            }
                            // Reject replayed (peer_id, nonce) pairs.
                            if !announcement.nonce.is_empty() {
                                let key = (announcement.peer_id.clone(), announcement.nonce.clone());
                                if seen_nonces.contains_key(&key) {
                                    log::debug!(
                                        "Discarding replayed announcement from {} (nonce {})",
                                        announcement.peer_id, announcement.nonce
                                    );
                                    continue;
                                }
                                seen_nonces.insert(key, Instant::now());
                            }
                            callback(announcement, addr);
                        }
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut
                || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                // Timeout — just loop and try again.
            }
            Err(e) => {
                log::error!("Discovery listen error: {}", e);
                return Err(e.to_string());
            }
        }
    }
}

/// Periodically broadcast our presence (run in a background task).
pub async fn broadcast_loop(announcement: Announcement) {
    loop {
        if let Err(e) = broadcast_presence(&announcement) {
            log::warn!("Broadcast failed: {}", e);
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
