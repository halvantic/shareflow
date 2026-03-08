pub mod receiver;
pub mod sender;

use std::path::PathBuf;

/// Chunk size for file transfers (64 KB).
pub const CHUNK_SIZE: usize = 64 * 1024;

/// Get the default downloads directory for received files.
pub fn receive_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        std::env::var("USERPROFILE")
            .map(|p| PathBuf::from(p).join("Downloads").join("ShareFlow"))
            .unwrap_or_else(|_| PathBuf::from("ShareFlow_Downloads"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var("HOME")
            .map(|p| PathBuf::from(p).join("Downloads").join("ShareFlow"))
            .unwrap_or_else(|_| PathBuf::from("ShareFlow_Downloads"))
    }
}
