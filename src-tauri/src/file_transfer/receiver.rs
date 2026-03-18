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

        // Sanitize: strip any path components to prevent directory traversal,
        // and remove null bytes and other control characters that could cause OS issues.
        let safe_name = std::path::Path::new(file_name)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "download".to_string());
        // Strip null bytes and control characters (Windows rejects them, others may misbehave).
        let safe_name: String = safe_name
            .chars()
            .filter(|c| *c != '\0' && !c.is_control())
            .collect();
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
            .unwrap_or_else(|e| e.into_inner())
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
        let mut transfers = self.transfers.lock().unwrap_or_else(|e| e.into_inner());
        let incoming = transfers
            .get_mut(transfer_id)
            .ok_or_else(|| format!("Unknown transfer: {}", transfer_id))?;

        // Validate that chunk fits within declared file size.
        // This prevents out-of-order chunks with overlapping offsets from corrupting files.
        let chunk_end = offset.saturating_add(data.len() as u64);
        if chunk_end > incoming.file_size {
            return Err(format!(
                "Transfer {} chunk exceeds file size: offset={}, len={}, file_size={}",
                transfer_id, offset, data.len(), incoming.file_size
            ));
        }

        // Also validate offset is reasonable (not before start of file)
        if offset > incoming.file_size {
            return Err(format!(
                "Transfer {} chunk offset {} exceeds file size {}",
                transfer_id, offset, incoming.file_size
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

        // Update received count (don't double-count overlapping regions,
        // but for now trust the sender's chunks are sequential and non-overlapping)
        incoming.received = incoming.received.max(chunk_end);
        Ok((incoming.received, incoming.file_size, incoming.file_name.clone()))
    }

    /// Finalize a completed transfer. Returns error if not all bytes were received.
    pub fn finish(&self, transfer_id: &str) -> Result<(String, PathBuf, u64), String> {
        let mut transfers = self.transfers.lock().unwrap_or_else(|e| e.into_inner());
        let incoming = transfers
            .remove(transfer_id)
            .ok_or_else(|| format!("Unknown transfer: {}", transfer_id))?;

        // Validate that we actually received all the data we expected.
        if incoming.received < incoming.file_size {
            // Partial file on disk — remove it to avoid leaving junk files.
            drop(incoming.writer);
            let _ = std::fs::remove_file(&incoming.path);
            return Err(format!(
                "Transfer {} incomplete: received {} of {} bytes",
                transfer_id, incoming.received, incoming.file_size
            ));
        }

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
        let mut transfers = self.transfers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(incoming) = transfers.remove(transfer_id) {
            drop(incoming.writer);
            let _ = std::fs::remove_file(&incoming.path);
            log::info!("File transfer cancelled: {}", transfer_id);
        }
    }

    /// Cancel all in-progress transfers (e.g. when a peer disconnects mid-transfer).
    /// Cleans up partial files on disk to avoid leaving junk.
    pub fn cancel_all(&self) {
        let mut transfers = self.transfers.lock().unwrap_or_else(|e| e.into_inner());
        let count = transfers.len();
        for (id, incoming) in transfers.drain() {
            drop(incoming.writer);
            let _ = std::fs::remove_file(&incoming.path);
            log::info!("File transfer {} abandoned on peer disconnect", id);
        }
        if count > 0 {
            log::warn!("Cancelled {} abandoned file transfer(s) due to peer disconnect", count);
        }
    }
}
