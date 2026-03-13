use std::collections::HashMap;
use std::io::{Seek, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use super::receive_dir;

/// State of a file being received.
struct IncomingFile {
    file_name: String,
    file_size: u64,
    received: u64,
    path: PathBuf,
    writer: std::fs::File,
}

/// Manages incoming file transfers.
pub struct FileReceiver {
    transfers: Mutex<HashMap<String, IncomingFile>>,
}

impl FileReceiver {
    pub fn new() -> Self {
        Self {
            transfers: Mutex::new(HashMap::new()),
        }
    }

    /// Start receiving a new file.
    pub fn start(&self, transfer_id: &str, file_name: &str, file_size: u64) -> Result<PathBuf, String> {
        let dir = receive_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot create dir: {}", e))?;

        // Sanitize: strip any path components to prevent directory traversal.
        let safe_name = std::path::Path::new(file_name)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "download".to_string());
        let safe_name = if safe_name.is_empty() || safe_name == "." || safe_name == ".." {
            "download".to_string()
        } else {
            safe_name
        };

        // Avoid overwriting: add suffix if file exists
        let mut path = dir.join(&safe_name);
        if path.exists() {
            let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
            let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
            let mut i = 1;
            loop {
                path = dir.join(format!("{} ({}){}", stem, i, ext));
                if !path.exists() {
                    break;
                }
                i += 1;
            }
        }

        let writer = std::fs::File::create(&path)
            .map_err(|e| format!("Cannot create file: {}", e))?;

        let incoming = IncomingFile {
            file_name: file_name.to_string(),
            file_size,
            received: 0,
            path: path.clone(),
            writer,
        };

        self.transfers
            .lock()
            .unwrap()
            .insert(transfer_id.to_string(), incoming);

        log::info!(
            "Receiving file: {} ({} bytes) -> {:?}",
            file_name,
            file_size,
            path
        );

        Ok(path)
    }

    /// Write a chunk of data. Returns (received_bytes, total_bytes, file_name).
    pub fn write_chunk(&self, transfer_id: &str, offset: u64, data: &[u8]) -> Result<(u64, u64, String), String> {
        let mut transfers = self.transfers.lock().unwrap();
        let incoming = transfers
            .get_mut(transfer_id)
            .ok_or_else(|| format!("Unknown transfer: {}", transfer_id))?;

        // Enforce file_size limit: reject writes that would exceed declared size.
        if incoming.received + data.len() as u64 > incoming.file_size {
            return Err(format!(
                "Transfer {} exceeded declared file size ({} bytes)",
                transfer_id, incoming.file_size
            ));
        }

        // Seek to the correct offset for out-of-order chunks.
        incoming
            .writer
            .seek(std::io::SeekFrom::Start(offset))
            .map_err(|e| format!("Seek error: {}", e))?;

        incoming
            .writer
            .write_all(data)
            .map_err(|e| format!("Write error: {}", e))?;

        incoming.received += data.len() as u64;
        Ok((incoming.received, incoming.file_size, incoming.file_name.clone()))
    }

    /// Finalize a completed transfer.
    pub fn finish(&self, transfer_id: &str) -> Result<(String, PathBuf, u64), String> {
        let mut transfers = self.transfers.lock().unwrap();
        let incoming = transfers
            .remove(transfer_id)
            .ok_or_else(|| format!("Unknown transfer: {}", transfer_id))?;

        // Flush is handled by drop, but let's be explicit
        drop(incoming.writer);

        log::info!(
            "File received: {} ({} bytes) at {:?}",
            incoming.file_name,
            incoming.received,
            incoming.path
        );

        Ok((incoming.file_name, incoming.path, incoming.received))
    }

    /// Cancel a transfer and clean up.
    pub fn cancel(&self, transfer_id: &str) {
        let mut transfers = self.transfers.lock().unwrap();
        if let Some(incoming) = transfers.remove(transfer_id) {
            drop(incoming.writer);
            let _ = std::fs::remove_file(&incoming.path);
            log::info!("File transfer cancelled: {}", transfer_id);
        }
    }
}
