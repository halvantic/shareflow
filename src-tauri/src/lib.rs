mod amt;
mod clipboard;
mod core;
mod credentials;
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
        let km = current.is_primary_km_device && !current.agent_mode;
        drop(current);
        engine.primary_km.store(km, std::sync::atomic::Ordering::SeqCst);
        // Force back to Local immediately if no longer the primary K+M device
        // so input suppression is never left active on a non-controlling machine.
        if !km {
            engine.switch_to_local().await;
        }
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
    let (tls_config, fp_capture) = network::tls::make_client_config()?;
    let mut conn = network::connection::connect_to_peer(&address, tls_config).await?;

    let config = state.engine.config.lock().await;
    let our_peer_id = config.peer_id.clone();
    let hello = crate::core::protocol::Message::Hello {
        protocol_version: crate::core::protocol::PROTOCOL_VERSION,
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
            protocol_version,
            peer_id,
            name,
            screens,
        })
        | Some(crate::core::protocol::Message::Hello {
            protocol_version,
            peer_id,
            name,
            screens,
        }) => {
            if protocol_version < crate::core::protocol::MIN_SUPPORTED_PROTOCOL_VERSION {
                return Err(format!(
                    "Peer protocol version {} is below minimum supported version {}",
                    protocol_version,
                    crate::core::protocol::MIN_SUPPORTED_PROTOCOL_VERSION
                ));
            }

            // Validate / pin the TLS certificate fingerprint now that we know the peer_id.
            let live_fp = fp_capture.lock().map(|g| g.clone()).unwrap_or(None)
                .unwrap_or_default();
            if live_fp.is_empty() {
                return Err("TLS handshake did not produce a certificate fingerprint".into());
            }
            {
                let mut cfg = state.engine.config.lock().await;
                match cfg.trusted_peers.iter().find(|p| p.peer_id == peer_id).map(|p| p.cert_fingerprint.clone()) {
                    Some(expected) if expected != live_fp => {
                        let _ = state.engine.ui_events.send(
                            crate::core::engine::UiEvent::CertificateMismatch {
                                id: peer_id.clone(),
                                fingerprint: live_fp.clone(),
                                expected: expected.clone(),
                            }
                        ).await;
                        return Err(format!(
                            "Certificate fingerprint mismatch for peer {}! \
                             Got {} but expected {}. Refusing connection.",
                            peer_id, live_fp, expected
                        ));
                    }
                    None => {
                        // New peer — TOFU: pin fingerprint and persist.
                        cfg.trusted_peers.push(crate::core::config::TrustedPeer {
                            peer_id: peer_id.clone(),
                            name: name.clone(),
                            cert_fingerprint: live_fp.clone(),
                        });
                        cfg.save();
                        drop(cfg);
                        let _ = state.engine.ui_events.send(
                            crate::core::engine::UiEvent::CertificatePinned {
                                id: peer_id.clone(),
                                name: name.clone(),
                                fingerprint: live_fp.clone(),
                            }
                        ).await;
                        log::info!("Pinned new peer {} cert fingerprint: {}", peer_id, live_fp);
                    }
                    Some(_) => {
                        // Known peer, fingerprint matches — all good.
                        log::debug!("Cert fingerprint verified for peer {}", peer_id);
                    }
                }
            }

            let ack = crate::core::protocol::Message::HelloAck {
                protocol_version: crate::core::protocol::PROTOCOL_VERSION,
                peer_id: our_peer_id.clone(),
                name: String::new(),
                screens: get_screens(),
            };
            let _ = conn.outgoing.send(ack).await;

            // Auth handshake (4.2): v2+ servers always send an auth signal after HelloAck.
            // AuthResult{success:true} = no pairing code required; AuthChallenge = respond with HMAC.
            if protocol_version >= 2 {
                let pairing_code = state.engine.config.lock().await.pairing_code.clone();
                match tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    conn.incoming.recv(),
                )
                .await
                {
                    Ok(Some(crate::core::protocol::Message::AuthResult { success: true })) => {
                        // Server has no pairing code — proceed.
                    }
                    Ok(Some(crate::core::protocol::Message::AuthResult { success: false })) => {
                        return Err("Server rejected the connection (auth failed)".into());
                    }
                    Ok(Some(crate::core::protocol::Message::AuthChallenge { nonce })) => {
                        let hash = network::auth::compute_auth_hmac(&pairing_code, &nonce);
                        if conn
                            .outgoing
                            .send(crate::core::protocol::Message::AuthResponse { hash })
                            .await
                            .is_err()
                        {
                            return Err("Connection closed during auth".into());
                        }
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(10),
                            conn.incoming.recv(),
                        )
                        .await
                        {
                            Ok(Some(crate::core::protocol::Message::AuthResult {
                                success: true,
                            })) => {}
                            Ok(Some(crate::core::protocol::Message::AuthResult {
                                success: false,
                            })) => {
                                return Err(
                                    "Authentication failed — check pairing code".into()
                                );
                            }
                            _ => return Err("Auth result timeout or connection closed".into()),
                        }
                    }
                    _ => return Err("Auth handshake failed or server disconnected".into()),
                }
            }

            // Hand off to the shared session loop (4.1).
            let result_name = name.clone();
            let result_id = peer_id.clone();
            let (reg_tx, reg_rx) = tokio::sync::oneshot::channel();
            tokio::spawn(network::session::run_peer_session(
                conn,
                state.engine.clone(),
                our_peer_id,
                peer_id,
                name,
                screens,
                Some(reg_tx),
            ));
            // Wait until the peer is registered before returning so the UI reflects the connection.
            let _ = reg_rx.await;

            Ok(format!("Connected to {} ({})", result_name, result_id))
        }
        _ => Err("Unexpected response from peer".into()),
    }
}

#[tauri::command]
async fn get_local_ip(state: tauri::State<'_, AppState>) -> Result<String, String> {
    let preferred = state.engine.config.lock().await.preferred_ip.clone();
    if !preferred.is_empty() {
        // Validate the preferred IP still exists on a local interface.
        if let Ok(ifas) = local_ip_address::list_afinet_netifas() {
            if ifas.iter().any(|(_, ip)| ip.to_string() == preferred) {
                return Ok(preferred);
            }
            log::warn!("Preferred IP {} no longer present on any interface, falling back", preferred);
        }
    }
    local_ip_address::local_ip()
        .map(|ip| ip.to_string())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_network_interfaces() -> Result<Vec<serde_json::Value>, String> {
    let ifas = local_ip_address::list_afinet_netifas()
        .map_err(|e| e.to_string())?;
    let mut result: Vec<serde_json::Value> = ifas
        .into_iter()
        .filter(|(_, ip)| ip.is_ipv4()) // only IPv4 for simplicity
        .map(|(name, ip)| {
            serde_json::json!({
                "name": name,
                "ip": ip.to_string(),
            })
        })
        .collect();
    // Sort by interface name for stable ordering
    result.sort_by(|a, b| {
        a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or(""))
    });
    Ok(result)
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
async fn release_control(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.engine.switch_to_local().await;
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
fn get_diagnostics() -> Vec<String> {
    DIAG_LOG.lock().map(|buf| buf.iter().cloned().collect()).unwrap_or_default()
}

#[tauri::command]
fn quit_app() {
    // Release input suppression before exiting so the Mac is never left
    // with a live event tap in suppress=true state after the process dies.
    crate::input::set_input_suppression(false);
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
    is_primary_km_device: bool,
    clipboard_sync_enabled: bool,
    preferred_ip: Option<String>,
) -> Result<(), String> {
    let mut config = state.engine.config.lock().await;
    config.port = port;
    config.discovery_port = discovery_port;
    config.auto_connect = auto_connect;
    // Agents are always non-primary — ignore any value passed in.
    config.is_primary_km_device = if config.agent_mode { false } else { is_primary_km_device };
    config.clipboard_sync_enabled = clipboard_sync_enabled;
    if let Some(ip) = preferred_ip {
        config.preferred_ip = ip;
    }
    if !machine_name.is_empty() {
        config.machine_name = machine_name;
    }
    config.save();
    let km = config.is_primary_km_device && !config.agent_mode;

    // If we are the host, push updated settings to all connected agents.
    if !config.agent_mode {
        let sync = crate::core::protocol::Message::ConfigSync {
            clipboard_sync_enabled: config.clipboard_sync_enabled,
        };
        drop(config);
        let peers = state.engine.peers.lock().await;
        for peer in peers.values() {
            let _ = peer.sender_lo.send(sync.clone()).await;
        }
    }

    state.engine.primary_km.store(km, std::sync::atomic::Ordering::SeqCst);
    if !km {
        state.engine.switch_to_local().await;
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
    let km = config.is_primary_km_device && !config.agent_mode;
    drop(config);
    state.engine.primary_km.store(km, std::sync::atomic::Ordering::SeqCst);
    if !km {
        state.engine.switch_to_local().await;
    }
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
async fn add_amt_computer(
    state: tauri::State<'_, AppState>,
    name: String,
    host: String,
    port: u16,
    username: String,
    password: String,
) -> Result<(), String> {
    if name.is_empty() || host.is_empty() || username.is_empty() || password.is_empty() {
        return Err("All fields are required".to_string());
    }

    let mut config = state.engine.config.lock().await;
    let computer = crate::core::config::AmtComputer {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        host,
        port,
        username,
        password,
    };
    config.amt_computers.push(computer);
    config.save();
    Ok(())
}

#[tauri::command]
async fn remove_amt_computer(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let mut config = state.engine.config.lock().await;
    config.amt_computers.retain(|c| c.id != id);
    config.save();
    Ok(())
}

#[tauri::command]
async fn power_on_amt_computer(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<String, String> {
    let config = state.engine.config.lock().await;
    let computer = config
        .amt_computers
        .iter()
        .find(|c| c.id == id)
        .ok_or("Computer not found")?
        .clone();
    drop(config);

    log::info!("AMT power-on initiated for '{}' ({})", computer.name, computer.host);
    let controller = amt::AmtController::new(computer.host.clone(), computer.port, computer.username, computer.password);
    let result = controller.power_on().await;
    match &result {
        Ok(msg) => log::info!("AMT power-on success for '{}': {}", computer.name, msg),
        Err(e)  => log::warn!("AMT power-on failed for '{}': {}", computer.name, e),
    }
    result
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

    // Get AMT computers from config
    let config = state.engine.config.lock().await;
    let amt_computers: Vec<(String, String)> = config.amt_computers
        .iter()
        .map(|c| (c.id.clone(), c.name.clone()))
        .collect();
    drop(config);

    if let Some(tray) = app.tray_by_id("main") {
        if let Ok(menu) = build_tray_menu(app, &peer_names, &focus, &amt_computers) {
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
    amt_computers: &[(String, String)], // (id, name)
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

    // Add AMT power control items
    if !amt_computers.is_empty() {
        let separator_amt = tauri::menu::PredefinedMenuItem::separator(app)?;
        builder = builder.item(&separator_amt);
        for (comp_id, comp_name) in amt_computers {
            let label = format!("Power On: {}", comp_name);
            let item = MenuItemBuilder::with_id(
                &format!("amt_power_on_{}", comp_id),
                &label,
            ).build(app)?;
            builder = builder.item(&item);
        }
    }

    let separator3 = tauri::menu::PredefinedMenuItem::separator(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit ShareFlow").build(app)?;
    builder = builder.items(&[&separator3, &quit]);

    Ok(builder.build()?)
}

fn setup_tray(app: &tauri::App, _engine: Arc<Engine>) -> Result<(), Box<dyn std::error::Error>> {
    let initial_menu = build_tray_menu(app.handle(), &[], &FocusState::Local, &[])?;

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
                    crate::input::set_input_suppression(false);
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
                _ if id.starts_with("amt_power_on_") => {
                    let amt_id = id.strip_prefix("amt_power_on_").unwrap_or(&id).to_string();
                    if let Some(state) = app.try_state::<AppState>() {
                        let engine = state.engine.clone();
                        tauri::async_runtime::spawn(async move {
                            let config = engine.config.lock().await;
                            let computer = config
                                .amt_computers
                                .iter()
                                .find(|c| c.id == amt_id)
                                .cloned();
                            drop(config);

                            if let Some(computer) = computer {
                                let controller = amt::AmtController::new(
                                    computer.host.clone(),
                                    computer.port,
                                    computer.username.clone(),
                                    computer.password.clone(),
                                );
                                match controller.power_on().await {
                                    Ok(msg) => {
                                        log::info!("AMT: Power-on from tray successful: {}", msg);
                                    }
                                    Err(e) => {
                                        log::error!("AMT: Power-on from tray failed: {}", e);
                                    }
                                }
                            } else {
                                log::warn!("AMT: Computer with id {} not found", amt_id);
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
    let (tls_config, fp_capture) = network::tls::make_client_config()?;
    let mut conn = network::connection::connect_to_peer(address, tls_config).await?;

    let config = engine.config.lock().await;
    let our_peer_id = config.peer_id.clone();
    let hello = crate::core::protocol::Message::Hello {
        protocol_version: crate::core::protocol::PROTOCOL_VERSION,
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
            protocol_version,
            peer_id,
            name,
            screens,
        })
        | Some(crate::core::protocol::Message::Hello {
            protocol_version,
            peer_id,
            name,
            screens,
        }) => {
            if protocol_version < crate::core::protocol::MIN_SUPPORTED_PROTOCOL_VERSION {
                return Err(format!(
                    "Peer protocol version {} is below minimum supported version {}",
                    protocol_version,
                    crate::core::protocol::MIN_SUPPORTED_PROTOCOL_VERSION
                ));
            }

            // Validate / pin the TLS certificate fingerprint now that we know the peer_id.
            let live_fp = fp_capture.lock().map(|g| g.clone()).unwrap_or(None)
                .unwrap_or_default();
            if live_fp.is_empty() {
                return Err("TLS handshake did not produce a certificate fingerprint".into());
            }
            {
                let mut cfg = engine.config.lock().await;
                match cfg.trusted_peers.iter().find(|p| p.peer_id == peer_id).map(|p| p.cert_fingerprint.clone()) {
                    Some(expected) if expected != live_fp => {
                        let _ = engine.ui_events.send(
                            crate::core::engine::UiEvent::CertificateMismatch {
                                id: peer_id.clone(),
                                fingerprint: live_fp.clone(),
                                expected: expected.clone(),
                            }
                        ).await;
                        return Err(format!(
                            "Certificate fingerprint mismatch for peer {}! \
                             Got {} but expected {}. Refusing connection.",
                            peer_id, live_fp, expected
                        ));
                    }
                    None => {
                        // New peer — TOFU: pin fingerprint and persist.
                        cfg.trusted_peers.push(crate::core::config::TrustedPeer {
                            peer_id: peer_id.clone(),
                            name: name.clone(),
                            cert_fingerprint: live_fp.clone(),
                        });
                        cfg.save();
                        drop(cfg);
                        let _ = engine.ui_events.send(
                            crate::core::engine::UiEvent::CertificatePinned {
                                id: peer_id.clone(),
                                name: name.clone(),
                                fingerprint: live_fp.clone(),
                            }
                        ).await;
                        log::info!("Pinned new peer {} cert fingerprint: {}", peer_id, live_fp);
                    }
                    Some(_) => {
                        // Known peer, fingerprint matches — all good.
                        log::debug!("Cert fingerprint verified for peer {}", peer_id);
                    }
                }
            }

            let ack = crate::core::protocol::Message::HelloAck {
                protocol_version: crate::core::protocol::PROTOCOL_VERSION,
                peer_id: our_peer_id.clone(),
                name: String::new(),
                screens: get_screens(),
            };
            let _ = conn.outgoing.send(ack).await;

            // Auth handshake (4.2): v2+ servers always send an auth signal after HelloAck.
            if protocol_version >= 2 {
                let pairing_code = engine.config.lock().await.pairing_code.clone();
                match tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    conn.incoming.recv(),
                )
                .await
                {
                    Ok(Some(crate::core::protocol::Message::AuthResult { success: true })) => {}
                    Ok(Some(crate::core::protocol::Message::AuthResult { success: false })) => {
                        return Err("Server rejected the connection (auth failed)".into());
                    }
                    Ok(Some(crate::core::protocol::Message::AuthChallenge { nonce })) => {
                        let hash = network::auth::compute_auth_hmac(&pairing_code, &nonce);
                        if conn
                            .outgoing
                            .send(crate::core::protocol::Message::AuthResponse { hash })
                            .await
                            .is_err()
                        {
                            return Err("Connection closed during auth".into());
                        }
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(10),
                            conn.incoming.recv(),
                        )
                        .await
                        {
                            Ok(Some(crate::core::protocol::Message::AuthResult {
                                success: true,
                            })) => {}
                            Ok(Some(crate::core::protocol::Message::AuthResult {
                                success: false,
                            })) => {
                                return Err(
                                    "Authentication failed — check pairing code".into()
                                );
                            }
                            _ => return Err("Auth result timeout or connection closed".into()),
                        }
                    }
                    _ => return Err("Auth handshake failed or server disconnected".into()),
                }
            }

            // Hand off to the shared session loop (4.1).
            let result_name = name.clone();
            let result_id = peer_id.clone();
            let (reg_tx, reg_rx) = tokio::sync::oneshot::channel();
            tokio::spawn(network::session::run_peer_session(
                conn,
                engine,
                our_peer_id,
                peer_id,
                name,
                screens,
                Some(reg_tx),
            ));
            let _ = reg_rx.await;

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
        // Restrict crash log to owner-only so peer IDs / file paths are not
        // readable by other local users.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &crash_path,
                std::fs::Permissions::from_mode(0o600),
            );
        }
    }));

    let config = AppConfig::load();
    let config_corrupted = config.was_corrupted;
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
            list_network_interfaces,
            get_peers,
            get_focus_state,
            release_control,
            switch_focus_local,
            set_neighbor,
            send_file_to_peer,
            get_diagnostics,
            quit_app,
            update_settings,
            add_trusted_host,
            remove_trusted_host,
            add_amt_computer,
            remove_amt_computer,
            power_on_amt_computer,
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

            // If the config was corrupted on load, alert the UI once the event
            // listener is ready (slight delay so the frontend has subscribed).
            if config_corrupted {
                let engine_alert = engine.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
                    let _ = engine_alert.ui_events.send(UiEvent::ConfigCorrupted).await;
                });
            }

            // On macOS, remove the Dock icon so the app lives only in the menu bar.
            #[cfg(target_os = "macos")]
            let _ = app.handle().set_activation_policy(tauri::ActivationPolicy::Accessory);

            // Check Accessibility permission on macOS and notify the UI if not yet granted.
            // Also re-notifies if the event tap reports that permission was revoked at runtime.
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
                    // Periodically re-check in case permission is revoked after launch.
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        if crate::input::take_accessibility_permission_lost() || !macos_accessibility_trusted() {
                            log::warn!("Accessibility permission lost or revoked — notifying UI");
                            let _ = app_handle_perm.emit("permissions-required", serde_json::json!({
                                "accessibility": false
                            }));
                        }
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

            // Periodic tray menu update (refreshes every 2s to catch config changes like AMT computers)
            let tray_update_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    update_tray(&tray_update_handle).await;
                }
            });

            // On macOS: Periodic screen refresh to detect wake-from-sleep
            // When the Mac wakes, screen resolution may change; we detect this by
            // checking if local screens have changed and broadcast to peers.
            #[cfg(target_os = "macos")]
            {
                let engine_screen_refresh = engine.clone();
                tauri::async_runtime::spawn(async move {
                    let mut last_screens = crate::core::screen::get_screens();
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        let current_screens = crate::core::screen::get_screens();
                        // Check if screens have changed (resolution, count, or position)
                        let screens_changed = last_screens.len() != current_screens.len()
                            || last_screens.iter().zip(&current_screens).any(|(a, b)| {
                                a.width != b.width || a.height != b.height || a.x != b.x || a.y != b.y
                            });
                        if screens_changed {
                            log::info!(
                                "macOS: Screen configuration changed, refreshing and broadcasting to peers"
                            );
                            engine_screen_refresh.refresh_and_broadcast_screens().await;
                            last_screens = current_screens;
                        }
                    }
                });
            }

            // Start the network server. Signal server_ready_rx once the port is bound
            // so the agent-mode auto-connect can wait on it instead of a fixed delay.
            let (server_ready_tx, server_ready_rx) = tokio::sync::oneshot::channel::<()>();
            let engine_server = engine.clone();
            tauri::async_runtime::spawn(async move {
                match network::tls::make_server_config() {
                    Ok(tls_config) => {
                        if let Err(e) =
                            network::server::start_server(engine_server, tls_config, Some(server_ready_tx)).await
                        {
                            log::error!("Server error: {}", e);
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to create TLS config: {}", e);
                        // Drop server_ready_tx without sending so the agent task
                        // sees the channel closed and falls through to auto-connect anyway.
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
            tauri::async_runtime::spawn(async move {
                core::runtime::start_clipboard_sync(engine_clip, clip_change_rx).await;
            });

            // Agent mode: auto-connect to the configured host on startup.
            // Wait for the server to finish binding (via server_ready_rx) so that
            // the host can reach back to us if needed, rather than using a fixed delay.
            {
                let engine_agent = engine.clone();
                tauri::async_runtime::spawn(async move {
                    let (is_agent, host_addr) = {
                        let cfg = engine_agent.config.lock().await;
                        (cfg.agent_mode, cfg.host_address.clone())
                    };
                    if is_agent && !host_addr.is_empty() {
                        // Wait for our own server to be bound (or give up after 5 s).
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            server_ready_rx,
                        ).await;
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
                // Bridge the blocking std mpsc receiver onto a tokio channel so the
                // async task does not occupy a tokio worker thread while waiting.
                let (async_disp_tx, mut async_disp_rx) = tokio::sync::mpsc::channel::<()>(4);
                std::thread::Builder::new()
                    .name("display-change-bridge".into())
                    .spawn(move || {
                        loop {
                            match display_rx.recv() {
                                Ok(()) => { if async_disp_tx.blocking_send(()).is_err() { break; } }
                                Err(_) => break,
                            }
                        }
                    })
                    .ok();
                tauri::async_runtime::spawn(async move {
                    // macOS fires multiple callbacks per reconfiguration event,
                    // so we debounce with a short delay.
                    while let Some(()) = async_disp_rx.recv().await {
                        // Debounce: wait for the display config to stabilize.
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        // Drain any additional notifications that arrived during debounce.
                        while async_disp_rx.try_recv().is_ok() {}
                        diag("Display configuration changed — refreshing screens".into());
                        engine_display.refresh_and_broadcast_screens().await;

                        // Secondary refresh: macOS may initially report a
                        // transitional resolution after sleep/wake (e.g. 1920×1080
                        // on an ultrawide). Fire a second refresh after the display
                        // has fully settled to catch the native resolution.
                        let engine_retry = engine_display.clone();
                        tauri::async_runtime::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                            diag("Secondary screen refresh after wake".into());
                            engine_retry.refresh_and_broadcast_screens().await;
                        });
                    }
                });
            }

            // Start LAN auto-discovery.
            let engine_disc = engine.clone();
            let app_handle_disc = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let config = engine_disc.config.lock().await;
                // Include our cert fingerprint so receivers can pre-verify identity.
                let own_cert_fp = network::tls::get_or_create_identity()
                    .ok()
                    .and_then(|(certs, _)| certs.into_iter().next())
                    .map(|cert| network::tls::cert_fingerprint(cert.as_ref()))
                    .unwrap_or_default();
                let announcement = network::discovery::Announcement {
                    peer_id: config.peer_id.clone(),
                    name: config.machine_name.clone(),
                    port: config.port,
                    discovery_port: config.discovery_port,
                    timestamp: 0, // filled in by broadcast_presence
                    cert_fingerprint: own_cert_fp,
                    nonce: String::new(), // filled in by broadcast_presence
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
                            let ann_cert_fp = ann.cert_fingerprint.clone();
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

                                // If announcement includes a cert fingerprint and we have a
                                // stored fingerprint for this peer, verify they match before
                                // attempting TLS — catches spoofed announcements early.
                                if !ann_cert_fp.is_empty() {
                                    if let Some(stored) = config.trusted_peers.iter().find(|p| p.peer_id == peer_id) {
                                        if stored.cert_fingerprint != ann_cert_fp {
                                            log::warn!(
                                                "Discovery: cert fingerprint mismatch for peer {} — \
                                                 announcement fp={}, stored fp={}. Skipping auto-connect.",
                                                peer_id, ann_cert_fp, stored.cert_fingerprint
                                            );
                                            drop(config);
                                            connecting.lock().unwrap_or_else(|e| e.into_inner()).remove(&peer_id);
                                            return;
                                        }
                                    }
                                }
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
