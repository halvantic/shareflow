use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::clipboard;
use crate::core::engine::{Engine, FocusState};
use crate::core::protocol::Message;
use crate::input::InputEvent;

/// PS/2 scancodes for copy/paste shortcut detection (same values on Windows and macOS
/// after the mac_vk_to_scancode mapping in input/macos.rs).
const SC_C: u16 = 0x2E;
const SC_V: u16 = 0x2F;
const SC_LCTRL: u16 = 0x1D;
const SC_RCTRL: u16 = 0x11D;
/// macOS Command key maps to Windows/Super scancode 0x15B via mac_vk_to_scancode.
const SC_CMD: u16 = 0x15B;

/// Start the input capture → engine → network forwarding loop.
pub async fn start_input_loop(
    engine: Arc<Engine>,
    mut event_rx: mpsc::Receiver<InputEvent>,
) {
    log::info!("Input forwarding loop started");

    // Track modifier key state for copy/paste shortcut detection.
    let mut ctrl_held = false;
    let mut cmd_held = false;
    let mut last_focus = engine.get_focus().await;

    while let Some(event) = event_rx.recv().await {
        // Check if focus changed — if so, reset modifiers to prevent stale keys
        // after device switching (e.g., Ctrl held on Windows, key-up on Mac).
        let current_focus = engine.get_focus().await;
        if current_focus != last_focus {
            ctrl_held = false;
            cmd_held = false;
            last_focus = current_focus;
            log::debug!("Focus changed, reset modifier state");
        }

        // Track modifier keys from keyboard events.
        if let InputEvent::Key(ref ke) = event {
            match ke.scancode {
                SC_LCTRL | SC_RCTRL => ctrl_held = ke.pressed,
                SC_CMD => cmd_held = ke.pressed,
                _ => {}
            }
        }

        // Detect copy (Ctrl/Cmd+C) and paste (Ctrl/Cmd+V) for immediate clipboard sync.
        if let InputEvent::Key(ref ke) = event {
            let modifier = ctrl_held || cmd_held;
            let is_copy = ke.pressed && ke.scancode == SC_C && modifier;
            let is_paste = ke.pressed && ke.scancode == SC_V && modifier;

            if is_copy || is_paste {
                // Capture focus state at shortcut detection time
                let focus_at_detection = engine.get_focus().await;

                if is_copy {
                    if let FocusState::Local = focus_at_detection {
                        // Copying locally: push to all peers after a short delay so the OS
                        // has time to update the clipboard before we read it.
                        let engine_clone = engine.clone();
                        tokio::spawn(async move {
                            // Check upfront if any peers are connected — if not, skip
                            // the clipboard read entirely to avoid any interference.
                            if engine_clone.peers.lock().await.is_empty() {
                                return;
                            }
                            tokio::time::sleep(Duration::from_millis(150)).await;
                            if let Some(content) = clipboard::sync::get_clipboard_content() {
                                log::debug!("Copy handler: broadcasting clipboard to peers");
                                let peers = engine_clone.peers.lock().await;
                                for peer in peers.values() {
                                    let _ = peer
                                        .sender
                                        .send(Message::ClipboardUpdate {
                                            content: content.clone(),
                                        })
                                        .await;
                                }
                            } else {
                                log::debug!("Copy handler: clipboard has no syncable content (may be files)");
                            }
                        });
                    }
                    // When focus=Remote, Ctrl+C is forwarded to the remote machine.
                    // The remote's own clipboard sync loop will detect the change and
                    // push the new content back to us automatically.
                }

                if is_paste {
                    if let FocusState::Remote(ref peer_id) = focus_at_detection {
                        // Before forwarding Ctrl+V to the remote machine, push our local
                        // clipboard so the remote pastes our content instead of its own.
                        if let Some(content) = clipboard::sync::get_clipboard_content() {
                            let _ = engine
                                .send_to_peer(
                                    peer_id,
                                    Message::ClipboardUpdate { content },
                                )
                                .await;
                        }
                    }
                }
            }
        }

        // Check primary K+M setting BEFORE processing input events.
        // The check must happen here — not after handle_local_input — because
        // handle_local_input can switch focus to Remote on an edge hit. If a
        // non-primary device's focus switches to Remote, input suppression
        // activates on that machine and it becomes completely stuck (no way
        // to control anything, no way to get back to Local).
        let is_primary_km = engine.config.lock().await.is_primary_km_device;

        if !is_primary_km {
            // If we somehow ended up in Remote focus (e.g., setting changed mid-session),
            // switch back immediately so input suppression is released.
            if matches!(engine.get_focus().await, FocusState::Remote(_)) {
                engine.switch_to_local().await;
            }
            continue; // Skip all input forwarding for non-primary K+M devices
        }

        if let Some((peer_id, msg)) = engine.handle_local_input(event).await {
            if let Message::Key(ref ke) = msg {
                crate::diag(format!(
                    "TX key sc=0x{:X} pressed={} → {}",
                    ke.scancode,
                    ke.pressed,
                    &peer_id[..peer_id.len().min(8)]
                ));
            }
            if let Err(e) = engine.send_to_peer(&peer_id, msg).await {
                log::warn!("Failed to forward input: {}", e);
                engine.switch_to_local().await;
            }
        }
    }

    log::info!("Input forwarding loop ended");
}

/// Bridge from std::sync::mpsc (hook thread) to tokio::sync::mpsc (async runtime).
pub fn start_event_bridge(
    std_rx: std::sync::mpsc::Receiver<InputEvent>,
    async_tx: mpsc::Sender<InputEvent>,
) -> Result<(), String> {
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
        .map(|_| ())
        .map_err(|e| format!("Failed to spawn event bridge thread: {}", e))
}

/// Start clipboard monitoring loop.
/// The loop exits when `cancel` is signalled (send `true` to stop).
pub async fn start_clipboard_sync(engine: Arc<Engine>, mut cancel: tokio::sync::watch::Receiver<bool>) {
    log::info!("Clipboard sync started");
    let mut last_known = clipboard::sync::get_clipboard_fingerprint();

    loop {
        tokio::select! {
            // Increased from 150ms to 300ms to reduce clipboard access contention on Windows.
            // This still provides reasonable clipboard sync latency while minimizing lock contention.
            _ = tokio::time::sleep(Duration::from_millis(300)) => {}
            _ = cancel.changed() => {
                if *cancel.borrow() {
                    log::info!("Clipboard sync stopped");
                    break;
                }
            }
        }

        // Only touch the clipboard if there is at least one connected peer.
        // When no peers are connected there is nothing to sync — skipping the
        // clipboard read entirely prevents any interference with local
        // clipboard usage (e.g. file copy-paste in Windows Explorer).
        let peer_count = engine.peers.lock().await.len();
        if peer_count == 0 {
            continue;
        }

        if let Some(content) = clipboard::sync::poll_clipboard_change(&mut last_known) {
            log::debug!("Polling loop: clipboard changed, broadcasting to peers");
            // Broadcast to all connected peers regardless of focus state.
            // This ensures that whichever machine you're currently controlling always
            // has your latest clipboard content available for pasting.
            let peers = engine.peers.lock().await;
            for peer in peers.values() {
                let _ = peer
                    .sender
                    .send(Message::ClipboardUpdate {
                        content: content.clone(),
                    })
                    .await;
            }
        }
    }
}
