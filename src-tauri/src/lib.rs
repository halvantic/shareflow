mod clipboard;
mod core;
mod file_transfer;
mod input;
mod network;

use std::sync::Arc;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, WindowEvent, Wry};
use tokio::sync::mpsc;

use crate::core::config::AppConfig;
use crate::core::engine::{Engine, FocusState, UiEvent};
use crate::core::screen::get_screens;

// --- Diagnostic ring-buffer log ---
use std::collections::VecDeque;
use std::sync::Mutex;

static DIAG_LOG: std::sync::LazyLock<Mutex<VecDeque<String>>> =
    std::sync::LazyLock::new(|| Mutex::new(VecDeque::new()));

/// Push a diagnostic message (kept in a ring buffer, max 200 entries).
pub fn diag(msg: String) {
    log::info!("{}", msg);
    if let Ok(mut buf) = DIAG_LOG.lock() {
        if buf.len() >= 200 {
            buf.pop_front();
        }
        buf.push_back(msg);
    }
}

/// Shared application state accessible from Tauri commands.
struct AppState {
    engine: Arc<Engine>,
}

// On macOS, check whether the process has the Accessibility (Event Tap) permission.
#[cfg(target_os = "macos")]
fn macos_accessibility_trusted() -> bool {
    extern "C" {
        fn AXIsProcessTrusted() -> u8;
    }
    unsafe { AXIsProcessTrusted() != 0 }
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
    let trusted_fps: Vec<String> = {
        let config = state.engine.config.lock().await;
        config.trusted_peers.iter().map(|p| p.cert_fingerprint.clone()).collect()
    };
    let tls_config = network::tls::make_client_config(trusted_fps)?;
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
                            if engine.get_focus().await != FocusState::Local {
                                continue;
                            }
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
                            if engine.get_focus().await != FocusState::Local {
                                continue;
                            }
                            if let Err(e) = injector.press_mouse_button(mb.button, mb.pressed) {
                                log::error!("Mouse button injection failed: {}", e);
                            }
                        }
                        crate::core::protocol::Message::MouseScroll(ms) => {
                            if engine.get_focus().await != FocusState::Local {
                                continue;
                            }
                            if let Err(e) = injector.scroll(ms.dx, ms.dy) {
                                log::error!("Scroll injection failed: {}", e);
                            }
                        }
                        crate::core::protocol::Message::Key(ke) => {
                            if engine.get_focus().await != FocusState::Local {
                                continue;
                            }
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
                                crate::input::reprime_keyboard_for_focus();
                            }
                        }
                        crate::core::protocol::Message::ClipboardUpdate { content } => {
                            if engine.config.lock().await.clipboard_sync_enabled {
                                crate::clipboard::sync::apply_remote_clipboard(content);
                            }
                        }
                        crate::core::protocol::Message::CameraFrame { data } => {
                            use base64::engine::Engine as _;
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                            let _ = engine
                                .ui_events
                                .send(UiEvent::CameraFrame {
                                    peer_id: remote_peer_id.clone(),
                                    data_b64: b64,
                                })
                                .await;
                        }
                        crate::core::protocol::Message::AudioChunk { data } => {
                            use base64::engine::Engine as _;
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                            let _ = engine
                                .ui_events
                                .send(UiEvent::AudioChunk {
                                    peer_id: remote_peer_id.clone(),
                                    data_b64: b64,
                                })
                                .await;
                        }
                        crate::core::protocol::Message::ScreenUpdate { screens } => {
                            engine.update_peer_screens(&remote_peer_id, screens).await;
                        }
                        crate::core::protocol::Message::PrimaryKmDeviceSync { .. } => {}
                        crate::core::protocol::Message::ConfigSync { clipboard_sync_enabled } => {
                            let mut cfg = engine.config.lock().await;
                            if cfg.agent_mode {
                                cfg.clipboard_sync_enabled = clipboard_sync_enabled;
                            }
                        }
                        crate::core::protocol::Message::AutoNeighbor { peer_id, edge, remove } => {
                            let screen_edge = match edge.as_str() {
                                "left" => crate::core::config::ScreenEdge::Left,
                                "right" => crate::core::config::ScreenEdge::Right,
                                "top" => crate::core::config::ScreenEdge::Top,
                                "bottom" => crate::core::config::ScreenEdge::Bottom,
                                _ => { log::warn!("AutoNeighbor: invalid edge '{}'", edge); continue; }
                            };
                            let mut cfg = engine.config.lock().await;
                            if remove {
                                cfg.neighbors.retain(|n| !(n.peer_id == peer_id && n.edge == screen_edge && n.screen_id.is_none()));
                            } else {
                                cfg.neighbors.retain(|n| !(n.edge == screen_edge && n.screen_id.is_none()));
                                cfg.neighbors.push(crate::core::config::Neighbor { peer_id, edge: screen_edge, screen_id: None });
                            }
                            cfg.save();
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
    // Send an initial MouseMove so the Mac has cursor context before key events.
    // Without this, CGEventPost keyboard injection can silently fail because
    // macOS hasn't fully synced cursor/event state from the SwitchFocus warp.
    let mouse_msg = crate::core::protocol::Message::MouseMove(
        crate::core::protocol::MouseMoveEvent { x: entry_x, y: entry_y },
    );
    peer.sender.send(mouse_msg).await.map_err(|e| e.to_string())?;
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

    let reciprocal_edge = match screen_edge {
        crate::core::config::ScreenEdge::Left => "right",
        crate::core::config::ScreenEdge::Right => "left",
        crate::core::config::ScreenEdge::Top => "bottom",
        crate::core::config::ScreenEdge::Bottom => "top",
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
            peer_id: peer_id.clone(),
            edge: screen_edge,
            screen_id,
        });
    }
    config.save();
    let our_peer_id = config.peer_id.clone();
    drop(config);

    // Notify the peer so it sets the reciprocal edge pointing back at us.
    let auto = crate::core::protocol::Message::AutoNeighbor {
        peer_id: our_peer_id,
        edge: reciprocal_edge.to_string(),
        remove: already_set,
    };
    let _ = state.engine.send_to_peer(&peer_id, auto).await;

    Ok(())
}

#[tauri::command]
async fn send_audio_chunk(
    state: tauri::State<'_, AppState>,
    data_b64: String,
) -> Result<(), String> {
    use base64::engine::Engine as _;
    let data = base64::engine::general_purpose::STANDARD
        .decode(&data_b64)
        .map_err(|e| e.to_string())?;
    let msg = crate::core::protocol::Message::AudioChunk { data };
    let peers = state.engine.peers.lock().await;
    for peer in peers.values() {
        let _ = peer.sender.send(msg.clone()).await;
    }
    Ok(())
}

#[tauri::command]
async fn send_camera_frame(
    state: tauri::State<'_, AppState>,
    data_b64: String,
) -> Result<(), String> {
    use base64::engine::Engine as _;
    let data = base64::engine::general_purpose::STANDARD
        .decode(&data_b64)
        .map_err(|e| e.to_string())?;
    let msg = crate::core::protocol::Message::CameraFrame { data };
    let peers = state.engine.peers.lock().await;
    for peer in peers.values() {
        let _ = peer.sender.send(msg.clone()).await;
    }
    Ok(())
}

#[tauri::command]
fn get_diagnostics() -> Vec<String> {
    DIAG_LOG.lock().map(|buf| buf.iter().cloned().collect()).unwrap_or_default()
}

#[tauri::command]
fn quit_app() {
    std::process::exit(0);
}

/// Returns true if Accessibility permission is granted (macOS), always true on other platforms.
#[tauri::command]
fn check_accessibility_permission() -> bool {
    #[cfg(target_os = "macos")]
    { macos_accessibility_trusted() }
    #[cfg(not(target_os = "macos"))]
    { true }
}

/// Opens System Settings → Privacy & Security → Accessibility on macOS.
#[tauri::command]
fn open_accessibility_settings() {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            .spawn();
    }
}

#[tauri::command]
async fn update_settings(
    state: tauri::State<'_, AppState>,
    port: u16,
    discovery_port: u16,
    auto_connect: bool,
    machine_name: String,
    camera_sharing_enabled: bool,
    audio_sharing_enabled: bool,
    is_primary_km_device: bool,
    clipboard_sync_enabled: bool,
) -> Result<(), String> {
    let mut config = state.engine.config.lock().await;
    config.port = port;
    config.discovery_port = discovery_port;
    config.auto_connect = auto_connect;
    config.camera_sharing_enabled = camera_sharing_enabled;
    config.audio_sharing_enabled = audio_sharing_enabled;
    // Agents are always non-primary — ignore any value passed in.
    config.is_primary_km_device = if config.agent_mode { false } else { is_primary_km_device };
    config.clipboard_sync_enabled = clipboard_sync_enabled;
    if !machine_name.is_empty() {
        config.machine_name = machine_name;
    }
    config.save();

    // If we are the host, push updated settings to all connected agents.
    if !config.agent_mode {
        let sync = crate::core::protocol::Message::ConfigSync {
            clipboard_sync_enabled: config.clipboard_sync_enabled,
        };
        drop(config);
        let peers = state.engine.peers.lock().await;
        for peer in peers.values() {
            let _ = peer.sender.send(sync.clone()).await;
        }
    }

    Ok(())
}

/// Returns true if this is the first launch and the setup wizard should be shown.
#[tauri::command]
async fn get_setup_state(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.engine.config.lock().await.is_first_run)
}

/// Called by the setup wizard to save the chosen mode and mark first-run complete.
#[tauri::command]
async fn complete_setup(
    state: tauri::State<'_, AppState>,
    agent_mode: bool,
    host_address: String,
) -> Result<(), String> {
    let mut config = state.engine.config.lock().await;
    config.agent_mode = agent_mode;
    config.host_address = host_address;
    config.is_first_run = false;
    if agent_mode {
        // Agents are controlled, not controllers — they must never act as a
        // primary K+M device regardless of what was previously configured.
        config.is_primary_km_device = false;
    }
    config.save();
    Ok(())
}

#[tauri::command]
async fn add_trusted_host(
    state: tauri::State<'_, AppState>,
    peer_id: String,
    name: String,
) -> Result<(), String> {
    let mut config = state.engine.config.lock().await;
    if !config.trusted_hosts.iter().any(|h| h.peer_id == peer_id) {
        config.trusted_hosts.push(crate::core::config::TrustedHost {
            peer_id,
            name,
        });
        config.save();
    }
    Ok(())
}

#[tauri::command]
async fn remove_trusted_host(
    state: tauri::State<'_, AppState>,
    peer_id: String,
) -> Result<(), String> {
    let mut config = state.engine.config.lock().await;
    config.trusted_hosts.retain(|h| h.peer_id != peer_id);
    config.save();
    Ok(())
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

/// Update the tray icon menu and tooltip from current engine state.
async fn update_tray(app: &AppHandle<Wry>) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let focus = state.engine.get_focus().await;
    let peers = state.engine.peers.lock().await;
    let peer_names: Vec<(String, String)> = peers
        .values()
        .map(|p| (p.id.clone(), p.name.clone()))
        .collect();
    let tooltip = match &focus {
        FocusState::Local => format!("ShareFlow - Local | {} peer(s)", peer_names.len()),
        FocusState::Remote(id) => {
            let name = peers
                .get(id)
                .map(|p| p.name.as_str())
                .unwrap_or("unknown");
            format!("ShareFlow - Controlling {}", name)
        }
    };
    drop(peers);
    if let Some(tray) = app.tray_by_id("main") {
        if let Ok(menu) = build_tray_menu(app, &peer_names, &focus) {
            let _ = tray.set_menu(Some(menu));
        }
        let _ = tray.set_tooltip(Some(&tooltip));
    }
}

/// Build a tray menu dynamically based on current peers and focus state.
fn build_tray_menu(
    app: &AppHandle<Wry>,
    peer_names: &[(String, String)], // (peer_id, name)
    focus: &FocusState,
) -> Result<tauri::menu::Menu<Wry>, Box<dyn std::error::Error>> {
    let status_text = match focus {
        FocusState::Local => format!("Status: Local | {} peer(s)", peer_names.len()),
        FocusState::Remote(id) => {
            let name = peer_names.iter()
                .find(|(pid, _)| pid == id)
                .map(|(_, n)| n.as_str())
                .unwrap_or("unknown");
            format!("Status: Controlling {}", name)
        }
    };

    let status = MenuItemBuilder::with_id("status", &status_text)
        .enabled(false)
        .build(app)?;
    let separator1 = tauri::menu::PredefinedMenuItem::separator(app)?;
    let show = MenuItemBuilder::with_id("show", "Show ShareFlow").build(app)?;
    let separator2 = tauri::menu::PredefinedMenuItem::separator(app)?;

    let mut builder = MenuBuilder::new(app);
    builder = builder.items(&[&status, &separator1, &show, &separator2]);

    // Add peer switch items
    if !peer_names.is_empty() {
        for (peer_id, name) in peer_names {
            let is_active = matches!(focus, FocusState::Remote(id) if id == peer_id);
            let label = if is_active {
                format!("Return to Local (from {})", name)
            } else {
                format!("Switch to {}", name)
            };
            let item = MenuItemBuilder::with_id(
                &format!("peer_{}", peer_id),
                &label,
            ).build(app)?;
            builder = builder.item(&item);
        }
    } else {
        let no_peers = MenuItemBuilder::with_id("no_peers", "No peers connected")
            .enabled(false)
            .build(app)?;
        builder = builder.item(&no_peers);
    }

    let separator3 = tauri::menu::PredefinedMenuItem::separator(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit ShareFlow").build(app)?;
    builder = builder.items(&[&separator3, &quit]);

    Ok(builder.build()?)
}

fn setup_tray(app: &tauri::App, _engine: Arc<Engine>) -> Result<(), Box<dyn std::error::Error>> {
    let initial_menu = build_tray_menu(app.handle(), &[], &FocusState::Local)?;

    let app_handle = app.handle().clone();
    let _tray = TrayIconBuilder::with_id("main")
        .icon(app.default_window_icon().cloned().unwrap_or_else(|| {
            log::warn!("Default window icon not found, using empty icon");
            tauri::image::Image::new(&[], 0, 0)
        }))
        .tooltip("ShareFlow - Keyboard & Mouse Sharing")
        .menu(&initial_menu)
        .on_menu_event(move |app, event| {
            let id = event.id().0.to_string();
            match id.as_str() {
                "show" => {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.unminimize();
                        let _ = window.set_focus();
                    }
                }
                "quit" => {
                    std::process::exit(0);
                }
                _ if id.starts_with("peer_") => {
                    let peer_id = id.strip_prefix("peer_").unwrap_or(&id).to_string();
                    if let Some(state) = app.try_state::<AppState>() {
                        let engine = state.engine.clone();
                        tauri::async_runtime::spawn(async move {
                            let focus = engine.get_focus().await;
                            if matches!(&focus, FocusState::Remote(id) if id == &peer_id) {
                                // Already controlling this peer — switch back to local
                                engine.switch_to_local().await;
                            } else {
                                // Switch to this peer
                                let peers = engine.peers.lock().await;
                                if let Some(peer) = peers.get(&peer_id) {
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
                                    // Send initial MouseMove to prime Mac's event stream
                                    let mouse_msg = crate::core::protocol::Message::MouseMove(
                                        crate::core::protocol::MouseMoveEvent { x: ex, y: ey },
                                    );
                                    let _ = peer.sender.send(mouse_msg).await;
                                    drop(peers);
                                    engine.switch_to_remote(&peer_id, ex, ey).await;
                                }
                            }
                        });
                    }
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let tauri::tray::TrayIconEvent::DoubleClick { .. } = event {
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.unminimize();
                    let _ = window.set_focus();
                }
            }
        })
        .build(app)?;


    Ok(())
}

/// Auto-connect to a peer (reuses connection logic from connect_to_peer_cmd).
async fn auto_connect_to_peer(engine: Arc<Engine>, address: &str) -> Result<String, String> {
    let trusted_fps: Vec<String> = {
        let config = engine.config.lock().await;
        config.trusted_peers.iter().map(|p| p.cert_fingerprint.clone()).collect()
    };
    let tls_config = network::tls::make_client_config(trusted_fps)?;
    let mut conn = network::connection::connect_to_peer(address, tls_config).await?;

    let config = engine.config.lock().await;
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
            engine.add_peer(peer).await;

            let conn_outgoing = conn.outgoing.clone();
            tokio::spawn(async move {
                while let Some(msg) = msg_rx.recv().await {
                    if conn_outgoing.send(msg).await.is_err() {
                        break;
                    }
                }
            });

            let engine2 = engine.clone();
            let remote_peer_id = peer_id.clone();
            tokio::spawn(async move {
                let injector = crate::input::create_injector();
                while let Some(msg) = conn.incoming.recv().await {
                    match msg {
                        crate::core::protocol::Message::MouseMove(mv) => {
                            if engine2.get_focus().await != FocusState::Local {
                                continue;
                            }
                            let _ = injector.move_mouse(mv.x, mv.y);
                            let edge_event = crate::input::InputEvent::MouseMove(mv);
                            if let Some((pid, msg)) = engine2.handle_local_input(edge_event).await {
                                if let Err(e) = engine2.send_to_peer(&pid, msg).await {
                                    log::warn!("Failed to send edge switch: {}", e);
                                }
                            }
                        }
                        crate::core::protocol::Message::MouseButton(mb) => {
                            if engine2.get_focus().await != FocusState::Local {
                                continue;
                            }
                            let _ = injector.press_mouse_button(mb.button, mb.pressed);
                        }
                        crate::core::protocol::Message::MouseScroll(ms) => {
                            if engine2.get_focus().await != FocusState::Local {
                                continue;
                            }
                            let _ = injector.scroll(ms.dx, ms.dy);
                        }
                        crate::core::protocol::Message::Key(ke) => {
                            if engine2.get_focus().await != FocusState::Local {
                                continue;
                            }
                            let _ = injector.send_key(ke.scancode, ke.pressed);
                        }
                        crate::core::protocol::Message::SwitchFocus {
                            target_id,
                            entry_x,
                            entry_y,
                        } => {
                            if target_id == our_peer_id {
                                let _ = injector.move_mouse(entry_x, entry_y);
                                engine2.switch_to_local().await;
                            }
                        }
                        crate::core::protocol::Message::ClipboardUpdate { content } => {
                            if engine2.config.lock().await.clipboard_sync_enabled {
                                crate::clipboard::sync::apply_remote_clipboard(content);
                            }
                        }
                        crate::core::protocol::Message::CameraFrame { data } => {
                            use base64::engine::Engine as _;
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                            let _ = engine2
                                .ui_events
                                .send(UiEvent::CameraFrame {
                                    peer_id: remote_peer_id.clone(),
                                    data_b64: b64,
                                })
                                .await;
                        }
                        crate::core::protocol::Message::AudioChunk { data } => {
                            use base64::engine::Engine as _;
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                            let _ = engine2
                                .ui_events
                                .send(UiEvent::AudioChunk {
                                    peer_id: remote_peer_id.clone(),
                                    data_b64: b64,
                                })
                                .await;
                        }
                        crate::core::protocol::Message::ScreenUpdate { screens } => {
                            engine2.update_peer_screens(&remote_peer_id, screens).await;
                        }
                        crate::core::protocol::Message::PrimaryKmDeviceSync { .. } => {}
                        crate::core::protocol::Message::ConfigSync { clipboard_sync_enabled } => {
                            let mut cfg = engine2.config.lock().await;
                            if cfg.agent_mode {
                                cfg.clipboard_sync_enabled = clipboard_sync_enabled;
                            }
                        }
                        crate::core::protocol::Message::AutoNeighbor { peer_id, edge, remove } => {
                            let screen_edge = match edge.as_str() {
                                "left" => crate::core::config::ScreenEdge::Left,
                                "right" => crate::core::config::ScreenEdge::Right,
                                "top" => crate::core::config::ScreenEdge::Top,
                                "bottom" => crate::core::config::ScreenEdge::Bottom,
                                _ => { log::warn!("AutoNeighbor: invalid edge '{}'", edge); continue; }
                            };
                            let mut cfg = engine2.config.lock().await;
                            if remove {
                                cfg.neighbors.retain(|n| !(n.peer_id == peer_id && n.edge == screen_edge && n.screen_id.is_none()));
                            } else {
                                cfg.neighbors.retain(|n| !(n.edge == screen_edge && n.screen_id.is_none()));
                                cfg.neighbors.push(crate::core::config::Neighbor { peer_id, edge: screen_edge, screen_id: None });
                            }
                            cfg.save();
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
                            engine2.handle_file_message(msg).await;
                        }
                        _ => {}
                    }
                }
                engine2.remove_peer(&remote_peer_id).await;
            });

            Ok(format!("Connected to {} ({})", result_name, result_id))
        }
        _ => Err("Unexpected response from peer".into()),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Install a global panic hook that writes to a crash log file before exiting.
    // Since windows_subsystem = "windows" suppresses panic dialogs, this ensures
    // panics are always recorded for debugging.
    std::panic::set_hook(Box::new(|info| {
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "Unknown panic payload".to_string()
        };

        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|_| "unknown".to_string());

        let message = format!(
            "[epoch:{}] PANIC at {}: {}\n",
            timestamp,
            location,
            payload
        );

        log::error!("{}", message.trim());

        // Write to a crash log file next to the executable or in APPDATA
        let crash_path = {
            #[cfg(target_os = "windows")]
            {
                std::env::var("APPDATA")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|_| std::path::PathBuf::from("."))
                    .join("shareflow")
                    .join("crash.log")
            }
            #[cfg(target_os = "macos")]
            {
                let mut p = std::path::PathBuf::from(
                    std::env::var("HOME").unwrap_or_else(|_| ".".into()),
                );
                p.push("Library");
                p.push("Application Support");
                p.push("shareflow");
                p.push("crash.log");
                p
            }
            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
            {
                let mut p = std::path::PathBuf::from(
                    std::env::var("HOME").unwrap_or_else(|_| ".".into()),
                );
                p.push(".local");
                p.push("share");
                p.push("shareflow");
                p.push("crash.log");
                p
            }
        };

        let _ = std::fs::create_dir_all(crash_path.parent().unwrap_or(std::path::Path::new(".")));
        // Append to crash log so we can see history
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&crash_path)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(message.as_bytes())
            });
    }));

    let config = AppConfig::load();
    log::info!(
        "ShareFlow starting — peer_id: {}, name: {}",
        config.peer_id,
        config.machine_name
    );

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
            send_file_to_peer,
            send_camera_frame,
            send_audio_chunk,
            get_diagnostics,
            quit_app,
            update_settings,
            add_trusted_host,
            remove_trusted_host,
            check_accessibility_permission,
            open_accessibility_settings,
            get_setup_state,
            complete_setup,
        ])
        // Hide to tray when the window is closed on Windows and macOS,
        // instead of quitting. Use Quit from the tray menu to fully exit.
        .on_window_event(|_window, _event| {
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            if let WindowEvent::CloseRequested { api, .. } = _event {
                api.prevent_close();
                let _ = _window.hide();
            }
        })
        .setup(move |app| {
            let engine = engine.clone();
            let app_handle = app.handle().clone();

            // On macOS, remove the Dock icon so the app lives only in the menu bar.
            #[cfg(target_os = "macos")]
            let _ = app.handle().set_activation_policy(tauri::ActivationPolicy::Accessory);

            // Check Accessibility permission on macOS and notify the UI if not yet granted.
            // The user must enable ShareFlow in System Settings → Privacy & Security → Accessibility
            // for keyboard/mouse capture (event tap) to work.
            #[cfg(target_os = "macos")]
            {
                let app_handle_perm = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                    if !macos_accessibility_trusted() {
                        let _ = app_handle_perm.emit("permissions-required", serde_json::json!({
                            "accessibility": false
                        }));
                    }
                });
            }

            // Set up system tray
            if let Err(e) = setup_tray(app, engine.clone()) {
                log::error!("Failed to set up system tray: {}", e);
            }

            // Forward UI events from engine to Tauri frontend and update tray on state changes.
            tauri::async_runtime::spawn(async move {
                while let Some(event) = ui_rx.recv().await {
                    match &event {
                        UiEvent::FocusChanged { .. }
                        | UiEvent::PeerConnected { .. }
                        | UiEvent::PeerDisconnected { .. } => {
                            update_tray(&app_handle).await;
                        }
                        _ => {}
                    }
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

            // Start input capture and forwarding loop.
            // Keep _capture alive for the lifetime of the app — dropping it
            // detaches the hook thread which is fine but we avoid any edge cases.
            let engine_input = engine.clone();
            let (mut _capture, event_rx) = input::create_capture_with_channel();

            // On Windows: take the clipboard change receiver from the hook thread.
            // WM_CLIPBOARDUPDATE signals replace the 300ms polling loop, eliminating
            // any timing races between clipboard reads and concurrent paste operations.
            #[cfg(target_os = "windows")]
            let clip_change_rx = _capture
                .take_clipboard_change_receiver()
                .map(core::runtime::start_clipboard_change_bridge);
            #[cfg(not(target_os = "windows"))]
            let clip_change_rx: Option<tokio::sync::mpsc::Receiver<()>> = None;

            if let Some(std_rx) = event_rx {
                let (async_tx, async_rx) = mpsc::channel(4096);
                match core::runtime::start_event_bridge(std_rx, async_tx) {
                    Ok(()) => {
                        tauri::async_runtime::spawn(async move {
                            core::runtime::start_input_loop(
                                engine_input,
                                async_rx,
                            )
                            .await;
                        });
                        diag("Input capture pipeline fully initialized".into());
                    }
                    Err(e) => {
                        log::error!("Failed to start event bridge: {}", e);
                        diag(format!("WARNING: Input pipeline initialization failed: {}", e));
                    }
                }
            } else {
                log::error!("Failed to create input capture — no event receiver");
            }

            // Start clipboard sync (event-driven on Windows, polling on other platforms).
            let engine_clip = engine.clone();
            let (_clip_stop_tx, clip_stop_rx) = tokio::sync::watch::channel(false);
            tauri::async_runtime::spawn(async move {
                core::runtime::start_clipboard_sync(engine_clip, clip_stop_rx, clip_change_rx).await;
            });

            // Agent mode: auto-connect to the configured host on startup.
            // A short delay lets the server finish binding and the UI load so
            // that PeerConnected events reach the frontend listener.
            {
                let engine_agent = engine.clone();
                tauri::async_runtime::spawn(async move {
                    let (is_agent, host_addr) = {
                        let cfg = engine_agent.config.lock().await;
                        (cfg.agent_mode, cfg.host_address.clone())
                    };
                    if is_agent && !host_addr.is_empty() {
                        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                        log::info!("Agent mode: auto-connecting to host at {}", host_addr);
                        match auto_connect_to_peer(engine_agent, &host_addr).await {
                            Ok(msg) => log::info!("Agent auto-connect: {}", msg),
                            Err(e) => log::warn!("Agent auto-connect failed: {}", e),
                        }
                    }
                });
            }

            // Monitor display configuration changes (resolution, wake from sleep).
            // When the display config changes, refresh local_screens and broadcast
            // the updated info to all connected peers so mouse bounds stay correct.
            #[cfg(target_os = "macos")]
            {
                let engine_display = engine.clone();
                let display_rx = input::start_display_change_monitor();
                tauri::async_runtime::spawn(async move {
                    // Bridge from sync receiver to async with debouncing.
                    // macOS fires multiple callbacks per reconfiguration event,
                    // so we debounce with a short delay.
                    loop {
                        match display_rx.recv() {
                            Ok(()) => {
                                // Debounce: wait a moment for the display config to stabilize
                                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                                // Drain any additional notifications that arrived during debounce
                                while display_rx.try_recv().is_ok() {}
                                diag("Display configuration changed — refreshing screens".into());
                                engine_display.refresh_and_broadcast_screens().await;
                            }
                            Err(_) => break,
                        }
                    }
                });
            }

            // Start LAN auto-discovery.
            let engine_disc = engine.clone();
            let app_handle_disc = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let config = engine_disc.config.lock().await;
                let announcement = network::discovery::Announcement {
                    peer_id: config.peer_id.clone(),
                    name: config.machine_name.clone(),
                    port: config.port,
                    discovery_port: config.discovery_port,
                    timestamp: 0, // filled in by broadcast_presence
                };
                let own_peer_id = config.peer_id.clone();
                let discovery_port = config.discovery_port;
                drop(config);

                // Broadcast our presence periodically
                let ann = announcement.clone();
                tauri::async_runtime::spawn(async move {
                    network::discovery::broadcast_loop(ann).await;
                });

                // Listen for peers in a blocking thread
                let ui_events = engine_disc.ui_events.clone();
                let peers = engine_disc.peers.clone();
                let engine_auto = engine_disc.clone();
                let connecting_peers: Arc<std::sync::Mutex<std::collections::HashSet<String>>> =
                    Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
                tokio::task::spawn_blocking(move || {
                    let _ = network::discovery::listen_for_peers(&own_peer_id, discovery_port, |ann, addr| {
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
                                address: address.clone(),
                            });

                            // Auto-connect if enabled and peer is trusted
                            let engine_ac = engine_auto.clone();
                            let app_handle_ac = app_handle_disc.clone();
                            let peer_id = ann.peer_id.clone();
                            let addr_clone = address.clone();
                            let connecting = connecting_peers.clone();

                            // Guard against duplicate auto-connect attempts
                            {
                                let mut set = connecting.lock().unwrap_or_else(|e| e.into_inner());
                                if set.contains(&peer_id) {
                                    return; // Already connecting to this peer
                                }
                                set.insert(peer_id.clone());
                            }

                            tauri::async_runtime::spawn(async move {
                                let config = engine_ac.config.lock().await;
                                let auto_connect = config.auto_connect;
                                let is_trusted = config.trusted_hosts.iter().any(|h| h.peer_id == peer_id);
                                drop(config);

                                if auto_connect && is_trusted {
                                    log::info!("Auto-connecting to trusted peer {} at {}", peer_id, addr_clone);
                                    if let Some(state) = app_handle_ac.try_state::<AppState>() {
                                        // Use the same logic as connect_to_peer_cmd
                                        let result = auto_connect_to_peer(state.engine.clone(), &addr_clone).await;
                                        match result {
                                            Ok(msg) => log::info!("Auto-connect success: {}", msg),
                                            Err(e) => log::warn!("Auto-connect failed: {}", e),
                                        }
                                    }
                                }
                                // Remove from connecting set so a future rediscovery can retry
                                connecting.lock().unwrap_or_else(|e| e.into_inner()).remove(&peer_id);
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
