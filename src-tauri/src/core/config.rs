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

/// Persisted application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// This machine's display name.
    pub machine_name: String,

    /// Unique ID for this machine (generated on first run).
    pub peer_id: String,

    /// Port to listen on.
    pub port: u16,

    /// Configured screen neighbors.
    pub neighbors: Vec<Neighbor>,

    /// Hotkey scancode combo to force-switch (e.g., Ctrl+Alt+S).
    pub switch_hotkey: Option<Vec<u16>>,

    /// Known/trusted peer certificates (fingerprints).
    pub trusted_peers: Vec<TrustedPeer>,
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
            neighbors: Vec::new(),
            switch_hotkey: None,
            trusted_peers: Vec::new(),
        }
    }
}

impl AppConfig {
    /// Load config from disk, or create default if it doesn't exist.
    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
            Err(_) => {
                let config = Self::default();
                config.save();
                config
            }
        }
    }

    /// Save config to disk.
    pub fn save(&self) {
        let path = config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, json);
        }
    }
}

fn config_path() -> PathBuf {
    let mut path = dirs_config_path();
    path.push("shareflow");
    path.push("config.json");
    path
}

fn dirs_config_path() -> PathBuf {
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
        std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("HOST"))
            .unwrap_or_else(|_| "Unknown-PC".into())
    }
}
