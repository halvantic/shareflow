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

    while let Some(event) = event_rx.recv().await {
        // Track modifier keys.
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
                let focus = engine.get_focus().await;

                if is_copy {
                    if let FocusState::Local = focus {
                        // Copying locally: push to all peers after a brief delay so the OS
                        // has time to update the clipboard before we read it.
                        let engine_clone = engine.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            if let Some(content) = clipboard::sync::get_clipboard_content() {
                                let peers = engine_clone.peers.lock().await;
                                for peer in peers.values() {
                                    let _ = peer
                                        .sender
                                        .send(Message::ClipboardUpdate {
                                            content: content.clone(),
                                        })
                                        .await;
                                }
                            }
                        });
                    }
                    // When focus=Remote, Ctrl+C is forwarded to the remote machine.
                    // The remote's own clipboard sync loop will detect the change and
                    // push the new content back to us automatically.
                }

                if is_paste {
                    if let FocusState::Remote(ref peer_id) = focus {
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

        if let Some((peer_id, msg)) = engine.handle_local_input(event).await {
            if let Message::Key(ref ke) = msg {
                crate::diag(format!(
                    "TX key sc=0x{:X} pressed={} → {}",
                    ke.scancode,
                    ke.pressed,
                    &peer_id[..8]
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
    let mut last_known = clipboard::sync::get_clipboard_fingerprint();

    loop {
        tokio::time::sleep(Duration::from_millis(150)).await;

        if let Some(content) = clipboard::sync::poll_clipboard_change(&mut last_known) {
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
