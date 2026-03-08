use std::path::Path;
use tokio::sync::mpsc;

use crate::core::engine::Engine;
use crate::core::protocol::Message;
use super::CHUNK_SIZE;

/// Send a file to a specific peer in chunks.
/// Returns the transfer_id.
pub async fn send_file(
    engine: &Engine,
    peer_id: &str,
    file_path: &Path,
    progress_tx: mpsc::Sender<FileProgress>,
) -> Result<String, String> {
    let file_name = file_path
        .file_name()
        .ok_or("Invalid file name")?
        .to_string_lossy()
        .to_string();

    let metadata = std::fs::metadata(file_path)
        .map_err(|e| format!("Cannot read file: {}", e))?;
    let file_size = metadata.len();

    let transfer_id = uuid::Uuid::new_v4().to_string();

    // Send FileStart
    engine
        .send_to_peer(
            peer_id,
            Message::FileStart {
                transfer_id: transfer_id.clone(),
                file_name: file_name.clone(),
                file_size,
            },
        )
        .await?;

    let _ = progress_tx
        .send(FileProgress {
            transfer_id: transfer_id.clone(),
            file_name: file_name.clone(),
            total_bytes: file_size,
            transferred_bytes: 0,
            done: false,
            error: None,
        })
        .await;

    // Read and send chunks
    let data = std::fs::read(file_path)
        .map_err(|e| format!("Failed to read file: {}", e))?;

    let mut offset: u64 = 0;
    for chunk in data.chunks(CHUNK_SIZE) {
        engine
            .send_to_peer(
                peer_id,
                Message::FileChunk {
                    transfer_id: transfer_id.clone(),
                    offset,
                    data: chunk.to_vec(),
                },
            )
            .await?;

        offset += chunk.len() as u64;

        let _ = progress_tx
            .send(FileProgress {
                transfer_id: transfer_id.clone(),
                file_name: file_name.clone(),
                total_bytes: file_size,
                transferred_bytes: offset,
                done: false,
                error: None,
            })
            .await;
    }

    // Send FileDone
    engine
        .send_to_peer(
            peer_id,
            Message::FileDone {
                transfer_id: transfer_id.clone(),
            },
        )
        .await?;

    let _ = progress_tx
        .send(FileProgress {
            transfer_id: transfer_id.clone(),
            file_name,
            total_bytes: file_size,
            transferred_bytes: file_size,
            done: true,
            error: None,
        })
        .await;

    log::info!(
        "File transfer complete: {} ({} bytes) -> {}",
        transfer_id,
        file_size,
        peer_id
    );

    Ok(transfer_id)
}

/// Progress update for a file transfer.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileProgress {
    pub transfer_id: String,
    pub file_name: String,
    pub total_bytes: u64,
    pub transferred_bytes: u64,
    pub done: bool,
    pub error: Option<String>,
}
