use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::clipboard;
use crate::core::engine::{Engine, FocusState};
use crate::core::hotkey::HotkeyDetector;
use crate::core::protocol::{ClipboardContent, Message};
use crate::input::InputEvent;

/// Start the input capture → engine → network forwarding loop.
/// Also handles hotkey detection for toggling focus.
pub async fn start_input_loop(
    engine: Arc<Engine>,
    mut event_rx: mpsc::Receiver<InputEvent>,
    hotkey: Arc<HotkeyDetector>,
) {
    log::info!("Input forwarding loop started");

    while let Some(event) = event_rx.recv().await {
        // Check hotkey before normal processing.
        if hotkey.process(&event) {
            handle_hotkey_toggle(&engine).await;
            continue; // Don't forward the hotkey itself
        }

        if let Some((peer_id, msg)) = engine.handle_local_input(event).await {
            if matches!(&msg, crate::core::protocol::Message::Key(_)) {
                log::debug!("Forwarding key event to peer {}", peer_id);
            }
            if let Err(e) = engine.send_to_peer(&peer_id, msg).await {
                log::warn!("Failed to forward input: {}", e);
                engine.switch_to_local().await;
            }
        }
    }

    log::info!("Input forwarding loop ended");
}

/// Toggle focus: if local → switch to first peer, if remote → switch to local.
async fn handle_hotkey_toggle(engine: &Engine) {
    let focus = engine.get_focus().await;
    match focus {
        FocusState::Local => {
            // Switch to the first available peer
            let peers = engine.peers.lock().await;
            if let Some(peer) = peers.values().next() {
                let peer_id = peer.id.clone();
                let target_screen = peer.screens.first();
                let (entry_x, entry_y) = if let Some(s) = target_screen {
                    (s.x + s.width / 2, s.y + s.height / 2)
                } else {
                    (960, 540)
                };

                let msg = Message::SwitchFocus {
                    target_id: peer_id.clone(),
                    entry_x,
                    entry_y,
                };
                let _ = peer.sender.send(msg).await;
                drop(peers);
                engine.switch_to_remote(&peer_id, entry_x, entry_y).await;
                log::info!("Hotkey: switched to remote peer {}", peer_id);
            } else {
                log::info!("Hotkey: no peers connected, staying local");
            }
        }
        FocusState::Remote(_) => {
            engine.switch_to_local().await;
            log::info!("Hotkey: switched back to local");
        }
    }
}

/// Bridge from std::sync::mpsc (hook thread) to tokio::sync::mpsc (async runtime).
pub fn start_event_bridge(
    std_rx: std::sync::mpsc::Receiver<InputEvent>,
    async_tx: mpsc::Sender<InputEvent>,
) {
    std::thread::Builder::new()
        .name("event-bridge".into())
        .spawn(move || {
            loop {
                match std_rx.recv() {
                    Ok(event) => {
                        if async_tx.blocking_send(event).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            log::info!("Event bridge thread ended");
        })
        .expect("Failed to spawn event bridge thread");
}

/// Start clipboard monitoring loop.
pub async fn start_clipboard_sync(engine: Arc<Engine>) {
    log::info!("Clipboard sync started");
    let mut last_known: Option<String> = clipboard::sync::get_clipboard_text();

    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;

        if let Some(new_text) = clipboard::sync::poll_clipboard_change(&last_known) {
            last_known = Some(new_text.clone());

            let focus = engine.get_focus().await;
            if matches!(focus, FocusState::Local) {
                let peers = engine.peers.lock().await;
                for peer in peers.values() {
                    let _ = peer
                        .sender
                        .send(Message::ClipboardUpdate {
                            content: ClipboardContent::Text(new_text.clone()),
                        })
                        .await;
                }
            }
        }
    }
}
