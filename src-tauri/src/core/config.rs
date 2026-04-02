use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Where a neighbor screen is relative to this machine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ScreenEdge {
    Left,
    Right,
    Top,
    Bottom,
}

/// A configured neighbor: which peer is on which edge of which screen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Neighbor {
    pub peer_id: String,
    pub edge: ScreenEdge,
    /// Which local monitor this applies to. None = any monitor (legacy/global).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_id: Option<String>,
}

/// A host that is trusted for auto-connect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedHost {
    pub peer_id: String,
    pub name: String,
}

/// An Intel AMT (vPro) computer for remote power control.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AmtComputer {
    /// Unique ID for this AMT computer (UUID).
    pub id: String,
    /// Display name for this computer.
    pub name: String,
    /// IP address or hostname of the AMT device.
    pub host: String,
    /// IPMI port (default 623).
    pub port: u16,
    /// IPMI username.
    pub username: String,
    /// IPMI password.
    pub password: String,
}

/// Persisted application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// This machine's display name.
    pub machine_name: String,

    /// Unique ID for this machine (generated on first run).
    pub peer_id: String,

    /// Port to listen on.
    pub port: u16,

    /// Port used for LAN discovery broadcasts.
    #[serde(default = "default_discovery_port")]
    pub discovery_port: u16,

    /// Automatically connect to trusted hosts when discovered.
    #[serde(default)]
    pub auto_connect: bool,

    /// Hosts trusted for auto-connect.
    #[serde(default)]
    pub trusted_hosts: Vec<TrustedHost>,

    /// Configured screen neighbors.
    pub neighbors: Vec<Neighbor>,

    /// Known/trusted peer certificates (fingerprints).
    pub trusted_peers: Vec<TrustedPeer>,

    /// Whether this machine is the primary keyboard & mouse device.
    /// When true, this machine can control other connected machines.
    /// When false, this machine only receives control from a primary device.
    /// Defaults to true so existing installs keep working.
    #[serde(default = "default_true")]
    pub is_primary_km_device: bool,

    /// Enable clipboard synchronisation with connected peers.
    /// When false, no clipboard data is sent to or received from peers.
    /// Defaults to true so existing installs keep working.
    #[serde(default = "default_true")]
    pub clipboard_sync_enabled: bool,

    /// When true the app runs in agent mode: minimal UI, auto-connects to
    /// `host_address`, and defers settings to the host via ConfigSync.
    #[serde(default)]
    pub agent_mode: bool,

    /// Address of the host to auto-connect to in agent mode (e.g. "192.168.1.5:24800").
    #[serde(default)]
    pub host_address: String,

    /// Preferred network interface IP address (e.g. "10.0.0.1").
    /// When set, discovery announcements and `get_local_ip` report this IP
    /// instead of the OS-chosen default. Empty string = auto-detect.
    #[serde(default)]
    pub preferred_ip: String,

    /// True only on the very first launch (config file did not exist).
    /// The setup wizard sets this to false once the user completes it.
    /// Existing installs deserialise this as false (field absent → default),
    /// so they skip the wizard and keep their current host-mode behaviour.
    #[serde(default)]
    pub is_first_run: bool,

    /// Configured AMT (Intel vPro) computers for remote power control.
    #[serde(default)]
    pub amt_computers: Vec<AmtComputer>,

    /// Optional pairing code for challenge-response authentication with peers.
    /// When non-empty, connecting peers must prove knowledge of this code via
    /// HMAC-SHA256 before the session starts. Empty = no auth required (default,
    /// backward-compatible with peers that predate this feature).
    #[serde(default)]
    pub pairing_code: String,

    /// Set to true if the config file could not be parsed and defaults were used.
    /// Never written to disk — indicates to the engine that it should alert the user.
    #[serde(skip)]
    pub was_corrupted: bool,
}

fn default_true() -> bool {
    true
}

fn default_discovery_port() -> u16 {
    24801
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedPeer {
    pub peer_id: String,
    pub name: String,
    pub cert_fingerprint: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            machine_name: hostname(),
            peer_id: uuid::Uuid::new_v4().to_string(),
            port: 24800,
            discovery_port: 24801,
            auto_connect: false,
            trusted_hosts: Vec::new(),
            neighbors: Vec::new(),
            trusted_peers: Vec::new(),
            is_primary_km_device: true,
            clipboard_sync_enabled: true,
            agent_mode: false,
            host_address: String::new(),
            preferred_ip: String::new(),
            is_first_run: false, // used as serde fallback for existing configs
            amt_computers: Vec::new(),
            pairing_code: String::new(),
            was_corrupted: false,
        }
    }
}

impl AppConfig {
    /// Load config from disk, or create default if it doesn't exist.
    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                let mut config: Self = match serde_json::from_str(&contents) {
                    Ok(c) => c,
                    Err(e) => {
                        log::error!(
                            "Config file is corrupt and cannot be parsed ({}). \
                             Resetting to defaults — trusted peers and neighbors have been cleared.",
                            e
                        );
                        let mut default = Self::default();
                        default.was_corrupted = true;
                        return default;
                    }
                };
                let mut changed = false;

                // Fix placeholder hostnames from previous versions
                if config.machine_name == "Unknown-PC" || config.machine_name.is_empty() {
                    config.machine_name = hostname();
                    changed = true;
                }

                // Decrypt AMT credentials (migration: plaintext values pass through unchanged).
                for computer in &mut config.amt_computers {
                    computer.username = crate::credentials::decrypt_credential(&computer.username);
                    computer.password = crate::credentials::decrypt_credential(&computer.password);
                }

                // Normalize any invalid state combinations.
                changed |= config.validate();

                if changed {
                    config.save();
                }
                config
            }
            Err(_) => {
                // Config file does not exist — genuine first launch.
                let mut config = Self::default();
                config.is_first_run = true;
                config.save();
                config
            }
        }
    }

    /// Enforce invariants that must always hold.
    /// Returns true if any field was modified (caller should save).
    pub fn validate(&mut self) -> bool {
        let mut changed = false;
        // Agent mode machines must never be the primary K+M device — they receive
        // control rather than send it. Silently correct if both are set true.
        if self.agent_mode && self.is_primary_km_device {
            log::warn!("Config invalid: agent_mode=true with is_primary_km_device=true — forcing is_primary_km_device=false");
            self.is_primary_km_device = false;
            changed = true;
        }
        // Agent mode without a host address is a misconfiguration — log a warning
        // but don't block startup; the user can fix it in settings.
        if self.agent_mode && self.host_address.is_empty() {
            log::warn!("Config warning: agent_mode=true but host_address is empty — auto-connect will be skipped");
        }
        changed
    }

    /// Save config to disk atomically (write to temp file then rename).
    /// This prevents config corruption if the process crashes mid-write.
    pub fn save(&self) {
        let path = config_path();
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                log::error!("Failed to create config directory: {}", e);
                return;
            }
        }
        // Encrypt AMT credentials before writing to disk.
        let mut to_save = self.clone();
        for computer in &mut to_save.amt_computers {
            computer.username = crate::credentials::encrypt_credential(&computer.username);
            computer.password = crate::credentials::encrypt_credential(&computer.password);
        }
        let json = match serde_json::to_string_pretty(&to_save) {
            Ok(j) => j,
            Err(e) => {
                log::error!("Failed to serialize config: {}", e);
                return;
            }
        };
        // Write to a temp file alongside the target, then rename atomically.
        let tmp_path = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp_path, &json) {
            log::error!("Failed to write temp config file: {}", e);
            return;
        }
        if let Err(e) = std::fs::rename(&tmp_path, &path) {
            log::error!("Failed to rename config file: {}", e);
            let _ = std::fs::remove_file(&tmp_path);
        }
    }
}

fn config_path() -> PathBuf {
    let mut path = dirs_config_path();
    path.push("shareflow");
    path.push("config.json");
    path
}

pub(crate) fn dirs_config_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
    }
    #[cfg(target_os = "macos")]
    {
        let mut p = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
        p.push("Library/Application Support");
        p
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let mut p = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
                p.push(".config");
                p
            })
    }
}

fn hostname() -> String {
    #[cfg(target_os = "windows")]
    {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Unknown-PC".into())
    }
    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(h) = std::env::var("HOSTNAME") {
            if !h.is_empty() {
                return h;
            }
        }
        if let Ok(h) = std::env::var("HOST") {
            if !h.is_empty() {
                return h;
            }
        }
        // Fallback: run `hostname` command (reliable on macOS GUI apps where env vars aren't set)
        if let Ok(output) = std::process::Command::new("hostname").output() {
            if let Ok(name) = String::from_utf8(output.stdout) {
                let trimmed = name.trim().to_string();
                if !trimmed.is_empty() {
                    return trimmed;
                }
            }
        }
        "Unknown-PC".into()
    }
}
