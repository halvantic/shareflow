mod clipboard;
mod core;
mod file_transfer;
mod input;
mod network;

use std::sync::Arc;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager, WindowEvent};
use tokio::sync::mpsc;

use crate::core::config::AppConfig;
use crate::core::engine::{Engine, FocusState, UiEvent};
use crate::core::hotkey::HotkeyDetector;
use crate::core::screen::get_screens;

/// Shared application state accessible from Tauri commands.
struct AppState {
    engine: Arc<Engine>,
    hotkey: Arc<HotkeyDetector>,
}

// --- Tauri Commands ---

#[tauri::command]
fn get_config(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, String> {
    let engine = state.engine.clone();
    let config = tauri::async_runtime::block_on(async { engine.config.lock().await.clone() });
    serde_json::to_value(&config).map_err(|e| e.to_string())
}

#[tauri::command]
fn save_config(state: tauri::State<'_, AppState>, config: AppConfig) -> Result<(), String> {
    let engine = state.engine.clone();
    tauri::async_runtime::block_on(async {
        let mut current = engine.config.lock().await;
        *current = config;
        current.save();
    });
    Ok(())
}

#[tauri::command]
fn get_screens_info() -> Vec<crate::core::protocol::ScreenInfo> {
    get_screens()
}

#[tauri::command]
async fn connect_to_peer_cmd(
    state: tauri::State<'_, AppState>,
    address: String,
) -> Result<String, String> {
    let tls_config = network::tls::make_client_config()?;
    let mut conn = network::connection::connect_to_peer(&address, tls_config).await?;

    let config = state.engine.config.lock().await;
    let our_peer_id = config.peer_id.clone();
    let hello = crate::core::protocol::Message::Hello {
        peer_id: config.peer_id.clone(),
        name: config.machine_name.clone(),
        screens: get_screens(),
    };
    drop(config);

    conn.outgoing
        .send(hello)
        .await
        .map_err(|e| e.to_string())?;

    match conn.incoming.recv().await {
        Some(crate::core::protocol::Message::HelloAck {
            peer_id,
            name,
            screens,
        })
        | Some(crate::core::protocol::Message::Hello {
            peer_id,
            name,
            screens,
        }) => {
            let ack = crate::core::protocol::Message::HelloAck {
                peer_id: our_peer_id.clone(),
                name: String::new(),
                screens: get_screens(),
            };
            let _ = conn.outgoing.send(ack).await;

            let (msg_tx, mut msg_rx) = mpsc::channel(256);
            let peer = crate::core::engine::Peer {
                id: peer_id.clone(),
                name: name.clone(),
                screens,
                sender: msg_tx,
            };
            let result_name = name.clone();
            let result_id = peer_id.clone();
            state.engine.add_peer(peer).await;

            let conn_outgoing = conn.outgoing.clone();
            tokio::spawn(async move {
                while let Some(msg) = msg_rx.recv().await {
                    if conn_outgoing.send(msg).await.is_err() {
                        break;
                    }
                }
            });

            let engine = state.engine.clone();
            let remote_peer_id = peer_id.clone();
            tokio::spawn(async move {
                let injector = crate::input::create_injector();
                while let Some(msg) = conn.incoming.recv().await {
                    match msg {
                        crate::core::protocol::Message::MouseMove(mv) => {
                            let _ = injector.move_mouse(mv.x, mv.y);
                            // Check if the injected position hits a local edge for switching back.
                            let edge_event = crate::input::InputEvent::MouseMove(mv);
                            if let Some((peer_id, msg)) = engine.handle_local_input(edge_event).await {
                                if let Err(e) = engine.send_to_peer(&peer_id, msg).await {
                                    log::warn!("Failed to send edge switch: {}", e);
                                }
                            }
                        }
                        crate::core::protocol::Message::MouseButton(mb) => {
                            if let Err(e) = injector.press_mouse_button(mb.button, mb.pressed) {
                                log::error!("Mouse button injection failed: {}", e);
                            }
                        }
                        crate::core::protocol::Message::MouseScroll(ms) => {
                            if let Err(e) = injector.scroll(ms.dx, ms.dy) {
                                log::error!("Scroll injection failed: {}", e);
                            }
                        }
                        crate::core::protocol::Message::Key(ke) => {
                            if let Err(e) = injector.send_key(ke.scancode, ke.pressed) {
                                log::error!("Key injection failed: {}", e);
                            }
                        }
                        crate::core::protocol::Message::SwitchFocus {
                            target_id,
                            entry_x,
                            entry_y,
                        } => {
                            if target_id == our_peer_id {
                                let _ = injector.move_mouse(entry_x, entry_y);
                                engine.switch_to_local().await;
                            }
                        }
                        crate::core::protocol::Message::ClipboardUpdate { content } => {
                            crate::clipboard::sync::apply_remote_clipboard(content);
                        }
                        crate::core::protocol::Message::Ping => {
                            let _ = conn
                                .outgoing
                                .send(crate::core::protocol::Message::Pong)
                                .await;
                        }
                        msg @ crate::core::protocol::Message::FileStart { .. }
                        | msg @ crate::core::protocol::Message::FileChunk { .. }
                        | msg @ crate::core::protocol::Message::FileDone { .. }
                        | msg @ crate::core::protocol::Message::FileCancel { .. } => {
                            engine.handle_file_message(msg).await;
                        }
                        _ => {}
                    }
                }
                engine.remove_peer(&remote_peer_id).await;
            });

            Ok(format!("Connected to {} ({})", result_name, result_id))
        }
        _ => Err("Unexpected response from peer".into()),
    }
}

#[tauri::command]
fn get_local_ip() -> Result<String, String> {
    local_ip_address::local_ip()
        .map(|ip| ip.to_string())
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_peers(state: tauri::State<'_, AppState>) -> Result<Vec<serde_json::Value>, String> {
    let peers = state.engine.peers.lock().await;
    let list: Vec<serde_json::Value> = peers
        .values()
        .map(|p| {
            serde_json::json!({
                "id": p.id,
                "name": p.name,
                "screens": p.screens.len(),
            })
        })
        .collect();
    Ok(list)
}

#[tauri::command]
async fn get_focus_state(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, String> {
    let focus = state.engine.get_focus().await;
    serde_json::to_value(&focus).map_err(|e| e.to_string())
}

#[tauri::command]
async fn switch_focus_to(
    state: tauri::State<'_, AppState>,
    peer_id: String,
) -> Result<(), String> {
    let peers = state.engine.peers.lock().await;
    let peer = peers.get(&peer_id).ok_or("Peer not found")?;
    let target_screen = peer.screens.first().ok_or("Peer has no screens")?;
    let entry_x = target_screen.x + target_screen.width / 2;
    let entry_y = target_screen.y + target_screen.height / 2;

    let msg = crate::core::protocol::Message::SwitchFocus {
        target_id: peer_id.clone(),
        entry_x,
        entry_y,
    };
    peer.sender.send(msg).await.map_err(|e| e.to_string())?;
    drop(peers);

    state.engine.switch_to_remote(&peer_id, entry_x, entry_y).await;
    Ok(())
}

#[tauri::command]
async fn switch_focus_local(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.engine.switch_to_local().await;
    Ok(())
}

#[tauri::command]
async fn set_neighbor(
    state: tauri::State<'_, AppState>,
    peer_id: String,
    edge: String,
    screen_id: Option<String>,
) -> Result<(), String> {
    let screen_edge = match edge.as_str() {
        "left" => crate::core::config::ScreenEdge::Left,
        "right" => crate::core::config::ScreenEdge::Right,
        "top" => crate::core::config::ScreenEdge::Top,
        "bottom" => crate::core::config::ScreenEdge::Bottom,
        _ => return Err(format!("Invalid edge: {}", edge)),
    };

    let mut config = state.engine.config.lock().await;

    // Toggle: if the exact same mapping exists, remove it (deselect)
    let already_set = config.neighbors.iter().any(|n| {
        n.peer_id == peer_id && n.edge == screen_edge && n.screen_id == screen_id
    });

    if already_set {
        config.neighbors.retain(|n| {
            !(n.peer_id == peer_id && n.edge == screen_edge && n.screen_id == screen_id)
        });
    } else {
        // Remove any other mapping for this edge+screen, then add new
        config
            .neighbors
            .retain(|n| !(n.edge == screen_edge && n.screen_id == screen_id));
        config.neighbors.push(crate::core::config::Neighbor {
            peer_id,
            edge: screen_edge,
            screen_id,
        });
    }
    config.save();
    Ok(())
}

#[tauri::command]
fn set_hotkey(state: tauri::State<'_, AppState>, scancodes: Vec<u16>) -> Result<(), String> {
    state.hotkey.set_combo(scancodes.clone());

    let engine = state.engine.clone();
    tauri::async_runtime::block_on(async {
        let mut config = engine.config.lock().await;
        config.switch_hotkey = Some(scancodes);
        config.save();
    });
    Ok(())
}

#[tauri::command]
fn quit_app() {
    std::process::exit(0);
}

#[tauri::command]
async fn send_file_to_peer(
    state: tauri::State<'_, AppState>,
    peer_id: String,
    file_path: String,
) -> Result<String, String> {
    let path = std::path::PathBuf::from(&file_path);
    if !path.exists() {
        return Err("File not found".into());
    }

    let engine = state.engine.clone();
    let (progress_tx, mut progress_rx) =
        mpsc::channel::<file_transfer::sender::FileProgress>(64);

    // Forward sender progress to UI events
    let ui_events = engine.ui_events.clone();
    tokio::spawn(async move {
        while let Some(progress) = progress_rx.recv().await {
            let _ = ui_events
                .send(UiEvent::FileProgress {
                    transfer_id: progress.transfer_id,
                    file_name: progress.file_name,
                    total_bytes: progress.total_bytes,
                    transferred_bytes: progress.transferred_bytes,
                    done: progress.done,
                    direction: "send".to_string(),
                })
                .await;
        }
    });

    let transfer_id =
        file_transfer::sender::send_file(&engine, &peer_id, &path, progress_tx).await?;
    Ok(transfer_id)
}

// --- System tray setup ---

fn setup_tray(app: &tauri::App, engine: Arc<Engine>) -> Result<(), Box<dyn std::error::Error>> {
    let show = MenuItemBuilder::with_id("show", "Show ShareFlow").build(app)?;
    let status = MenuItemBuilder::with_id("status", "Status: Local")
        .enabled(false)
        .build(app)?;
    let toggle = MenuItemBuilder::with_id("toggle", "Toggle Focus (Scroll Lock)").build(app)?;
    let separator = tauri::menu::PredefinedMenuItem::separator(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;

    let menu = MenuBuilder::new(app)
        .items(&[&status, &separator, &show, &toggle, &separator, &quit])
        .build()?;

    let app_handle = app.handle().clone();
    let _tray = TrayIconBuilder::new()
        .tooltip("ShareFlow - Keyboard & Mouse Sharing")
        .menu(&menu)
        .on_menu_event(move |app, event| {
            match event.id().as_ref() {
                "show" => {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                }
                "toggle" => {
                    let engine = engine.clone();
                    tauri::async_runtime::spawn(async move {
                        let focus = engine.get_focus().await;
                        match focus {
                            FocusState::Local => {
                                let peers = engine.peers.lock().await;
                                if let Some(peer) = peers.values().next() {
                                    let peer_id = peer.id.clone();
                                    let (ex, ey) = if let Some(s) = peer.screens.first() {
                                        (s.x + s.width / 2, s.y + s.height / 2)
                                    } else {
                                        (960, 540)
                                    };
                                    let msg = crate::core::protocol::Message::SwitchFocus {
                                        target_id: peer_id.clone(),
                                        entry_x: ex,
                                        entry_y: ey,
                                    };
                                    let _ = peer.sender.send(msg).await;
                                    drop(peers);
                                    engine.switch_to_remote(&peer_id, ex, ey).await;
                                }
                            }
                            FocusState::Remote(_) => {
                                engine.switch_to_local().await;
                            }
                        }
                    });
                }
                "quit" => {
                    std::process::exit(0);
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let tauri::tray::TrayIconEvent::DoubleClick { .. } = event {
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        })
        .build(app)?;

    // Spawn task to update tray menu status text when focus changes.
    let app_handle2 = app_handle.clone();
    let _status_id = status.id().clone();
    tauri::async_runtime::spawn(async move {
        let mut last_text = String::new();
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if let Some(state) = app_handle2.try_state::<AppState>() {
                let focus = state.engine.get_focus().await;
                let peers = state.engine.peers.lock().await;
                let peer_count = peers.len();
                let text = match &focus {
                    FocusState::Local => {
                        format!("Local | {} peer(s)", peer_count)
                    }
                    FocusState::Remote(id) => {
                        let name = peers
                            .get(id)
                            .map(|p| p.name.as_str())
                            .unwrap_or("unknown");
                        format!("Remote: {} | {} peer(s)", name, peer_count)
                    }
                };
                drop(peers);
                if text != last_text {
                    last_text = text.clone();
                    // Update the status menu item text
                    if let Some(item) = app_handle2.menu().and_then(|_| None::<tauri::menu::MenuItem<tauri::Wry>>) {
                        let _ = item.set_text(&text);
                    }
                    // Update tooltip as a simpler approach
                    if let Some(tray) = app_handle2.tray_by_id("main") {
                        let _ = tray.set_tooltip(Some(&format!("ShareFlow - {}", text)));
                    }
                }
            }
        }
    });

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let config = AppConfig::load();
    log::info!(
        "ShareFlow starting — peer_id: {}, name: {}",
        config.peer_id,
        config.machine_name
    );

    // Set up hotkey detector
    let hotkey = Arc::new(HotkeyDetector::new());
    if let Some(ref combo) = config.switch_hotkey {
        hotkey.set_combo(combo.clone());
    }

    let (ui_tx, mut ui_rx) = mpsc::channel::<UiEvent>(256);
    let engine = Arc::new(Engine::new(config, ui_tx));

    // Populate local screens
    {
        let engine = engine.clone();
        let screens = get_screens();
        tauri::async_runtime::block_on(async {
            *engine.local_screens.lock().await = screens;
        });
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            engine: engine.clone(),
            hotkey: hotkey.clone(),
        })
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_config,
            get_screens_info,
            connect_to_peer_cmd,
            get_local_ip,
            get_peers,
            get_focus_state,
            switch_focus_to,
            switch_focus_local,
            set_neighbor,
            set_hotkey,
            send_file_to_peer,
            quit_app,
        ])
        // On Windows, hide the window to tray when minimized or closed
        // instead of leaving it in the taskbar.
        .on_window_event(|_window, _event| {
            #[cfg(target_os = "windows")]
            match _event {
                WindowEvent::CloseRequested { api, .. } => {
                    // Prevent actual close — hide to tray instead
                    api.prevent_close();
                    let _ = _window.hide();
                }
                _ => {}
            }
        })
        .setup(move |app| {
            let engine = engine.clone();
            let app_handle = app.handle().clone();

            // Set up system tray
            if let Err(e) = setup_tray(app, engine.clone()) {
                log::error!("Failed to set up system tray: {}", e);
            }

            // Forward UI events from engine to Tauri frontend.
            tauri::async_runtime::spawn(async move {
                while let Some(event) = ui_rx.recv().await {
                    let _ = app_handle.emit("shareflow-event", &event);
                }
            });

            // Start the network server.
            let engine_server = engine.clone();
            tauri::async_runtime::spawn(async move {
                match network::tls::make_server_config() {
                    Ok(tls_config) => {
                        if let Err(e) =
                            network::server::start_server(engine_server, tls_config).await
                        {
                            log::error!("Server error: {}", e);
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to create TLS config: {}", e);
                    }
                }
            });

            // Start input capture and forwarding loop with hotkey detection.
            let engine_input = engine.clone();
            let hotkey_input = hotkey.clone();
            {
                let (_capture, event_rx) = input::create_capture_with_channel();
                if let Some(std_rx) = event_rx {
                    let (async_tx, async_rx) = mpsc::channel(4096);
                    core::runtime::start_event_bridge(std_rx, async_tx);

                    tauri::async_runtime::spawn(async move {
                        core::runtime::start_input_loop(
                            engine_input,
                            async_rx,
                            hotkey_input,
                        )
                        .await;
                    });
                }
            }

            // Start clipboard sync.
            let engine_clip = engine.clone();
            tauri::async_runtime::spawn(async move {
                core::runtime::start_clipboard_sync(engine_clip).await;
            });

            // Start LAN auto-discovery.
            let engine_disc = engine.clone();
            tauri::async_runtime::spawn(async move {
                let config = engine_disc.config.lock().await;
                let announcement = network::discovery::Announcement {
                    peer_id: config.peer_id.clone(),
                    name: config.machine_name.clone(),
                    port: config.port,
                };
                let own_peer_id = config.peer_id.clone();
                drop(config);

                // Broadcast our presence periodically
                let ann = announcement.clone();
                tauri::async_runtime::spawn(async move {
                    network::discovery::broadcast_loop(ann).await;
                });

                // Listen for peers in a blocking thread
                let ui_events = engine_disc.ui_events.clone();
                let peers = engine_disc.peers.clone();
                tokio::task::spawn_blocking(move || {
                    let _ = network::discovery::listen_for_peers(&own_peer_id, |ann, addr| {
                        let address = format!("{}:{}", addr.ip(), ann.port);
                        // Only emit if not already connected
                        let connected = {
                            // Use try_lock to avoid blocking — skip if locked
                            if let Ok(peers) = peers.try_lock() {
                                peers.contains_key(&ann.peer_id)
                            } else {
                                false
                            }
                        };
                        if !connected {
                            let ui = ui_events.clone();
                            let id = ann.peer_id.clone();
                            let name = ann.name.clone();
                            // Fire-and-forget — non-blocking send
                            let _ = ui.try_send(UiEvent::PeerDiscovered {
                                id,
                                name,
                                address,
                            });
                        }
                    });
                });
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
