// Twinagent — notch widget. The Rust core embeds twin-hub (local-first) and
// watches the Windows-side agent dirs directly: the app IS the Windows
// collector (TWI-8). Pill/panel window behaviors (always-on-top, frameless,
// hotkey) land with TWI-11/TWI-12.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod jump;
mod notify;
mod widget;

use std::path::PathBuf;

use tauri::Manager;
use twin_core::SourceRoot;
use twin_hub::{EmbeddedCollector, HubService, Store};

/// Agent data dirs on THIS machine's native filesystem. Never `\\wsl$` or
/// `/mnt/c` — file events don't cross the WSL/Windows boundary; the WSL side
/// runs its own collector daemon that POSTs to the hub we host.
fn native_roots() -> Vec<SourceRoot> {
    // USERPROFILE on Windows, HOME elsewhere (WSLg preview builds).
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default();
    // A Gemini root joins this list once twin-core grows a Gemini parser.
    vec![
        SourceRoot::claude(home.join(".claude").join("projects")),
        SourceRoot::codex(home.join(".codex").join("sessions")),
    ]
}

/// Second hub listener on the `vEthernet (WSL)` adapter, so the WSL
/// collector can reach us under NAT networking. The adapter exists only
/// while the WSL VM runs and gets a fresh subnet each time it starts, so
/// poll for it and rebind whenever its address changes.
#[cfg(windows)]
fn spawn_wsl_facing_server(hub: twin_hub::HubService, port: u16) {
    fn wsl_adapter_ip() -> Option<std::net::IpAddr> {
        if_addrs::get_if_addrs()
            .ok()?
            .into_iter()
            .find(|iface| iface.name.contains("WSL") && iface.ip().is_ipv4())
            .map(|iface| iface.ip())
    }

    tauri::async_runtime::spawn(async move {
        let poll = std::time::Duration::from_secs(30);
        loop {
            let Some(ip) = wsl_adapter_ip() else {
                tokio::time::sleep(poll).await;
                continue;
            };
            let addr = std::net::SocketAddr::from((ip, port));
            let server = twin_hub::server::serve(hub.clone(), addr);
            tokio::pin!(server);
            loop {
                tokio::select! {
                    result = &mut server => {
                        if let Err(err) = result {
                            eprintln!("twinagent: WSL-facing hub server on {addr} exited: {err}");
                        }
                        tokio::time::sleep(poll).await;
                        break;
                    }
                    _ = tokio::time::sleep(poll) => {
                        // WSL restarted onto a new subnet? Drop the stale
                        // listener and rebind on the new address.
                        if wsl_adapter_ip() != Some(ip) {
                            break;
                        }
                    }
                }
            }
        }
    });
}

#[tauri::command]
fn toggle_panel(window: tauri::WebviewWindow) {
    widget::toggle_panel(&window);
}

/// Stats for the panel's stats pane, straight from the in-process hub —
/// a webview fetch to the hub's HTTP port would be cross-origin (the hub
/// serves no CORS headers; only the WebSocket escapes that).
#[tauri::command]
fn get_stats(hub: tauri::State<twin_hub::HubService>) -> Vec<twin_core::UsageStats> {
    hub.usage_stats()
}

#[tauri::command]
fn set_panel(window: tauri::WebviewWindow, expanded: bool) {
    widget::set_expanded(&window, expanded);
}

/// The configured hub port, so the webview builds its WebSocket URL from the
/// same `Settings` the hub binds. A changed `hub_port` must not strand the
/// panel on the default 17871 with no sessions (BUGHUNT #1).
#[tauri::command]
fn get_hub_port(app: tauri::AppHandle) -> Result<u16, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(widget::load_settings(&data_dir).hub_port)
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            toggle_panel,
            set_panel,
            get_stats,
            get_hub_port,
            jump::jump,
            jump::focus_agent,
            widget::get_settings,
            widget::update_settings,
            widget::set_pill_size
        ])
        .manage(widget::PanelState::default())
        .setup(|app| {
            // Hub state lives in the app data dir; history survives restarts.
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let store = Store::open(data_dir.join("twin-hub.db"))?;
            let hub = HubService::new(Some(store));

            let settings = widget::load_settings(&data_dir);
            app.manage(notify::ToastsEnabled(settings.toasts.into()));
            app.manage(widget::PinState(settings.pinned.into()));
            widget::apply_autostart(app.handle(), settings.autostart);

            // Serve WebSocket + ingest for the widget UI and the WSL
            // collector. Loopback covers the UI and mirrored networking;
            // under NAT the collector reaches this host at the vEthernet
            // (WSL) adapter address, so we bind that too — and only that,
            // never 0.0.0.0: nothing here may be visible to the LAN.
            let addr: std::net::SocketAddr = std::env::var("TWIN_HUB_ADDR")
                .unwrap_or_else(|_| format!("127.0.0.1:{}", settings.hub_port))
                .parse()?;
            let server_hub = hub.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(err) = twin_hub::server::serve(server_hub, addr).await {
                    eprintln!("twinagent: hub server exited: {err}");
                }
            });
            #[cfg(windows)]
            spawn_wsl_facing_server(hub.clone(), addr.port());

            // Watch this machine's agent dirs in-process.
            let machine = if cfg!(windows) { "windows" } else { "linux" };
            let _collector = EmbeddedCollector::spawn(hub.clone(), machine, native_roots());

            // Toast on needs-you transitions, whichever machine they're on.
            notify::spawn(app.handle().clone(), hub.clone());

            // Housekeeping: stale-mark and prune once a minute.
            let sweeper = hub.clone();
            tauri::async_runtime::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
                loop {
                    tick.tick().await;
                    sweeper.sweep(chrono::Utc::now());
                }
            });

            app.manage(hub);

            // Pill placement: top-center of the remembered monitor, and
            // keep remembering as the user drags it around (TWI-11).
            let window = app
                .get_webview_window("main")
                .expect("main window exists");
            app.manage(widget::PillWidth::default());
            widget::position_pill(&window, &data_dir);
            widget::remember_monitor_on_move(&window, data_dir.clone());
            widget::collapse_on_blur(&window);
            widget::setup_click_through(&window);
            widget::setup_hotkey(app, &data_dir)?;

            widget::setup_tray(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Twinagent");
}
