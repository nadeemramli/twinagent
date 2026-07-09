// Twinagent — notch widget. The Rust core embeds twin-hub (local-first) and
// watches the Windows-side agent dirs directly: the app IS the Windows
// collector (TWI-8). Pill/panel window behaviors (always-on-top, frameless,
// hotkey) land with TWI-11/TWI-12.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

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

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            // Hub state lives in the app data dir; history survives restarts.
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let store = Store::open(data_dir.join("twin-hub.db"))?;
            let hub = HubService::new(Some(store));

            // Serve WebSocket + ingest for the widget UI and the WSL
            // collector. 127.0.0.1 only; WSL reaches it via localhost under
            // mirrored networking (or the host address otherwise — the
            // collector unit's TWIN_HUB_URL is the knob).
            let addr: std::net::SocketAddr = std::env::var("TWIN_HUB_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:17871".into())
                .parse()?;
            let server_hub = hub.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(err) = twin_hub::server::serve(server_hub, addr).await {
                    eprintln!("twinagent: hub server exited: {err}");
                }
            });

            // Watch this machine's agent dirs in-process.
            let machine = if cfg!(windows) { "windows" } else { "linux" };
            let _collector = EmbeddedCollector::spawn(hub.clone(), machine, native_roots());

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
            widget::position_pill(&window, &data_dir);
            widget::remember_monitor_on_move(&window, data_dir.clone());

            widget::setup_tray(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Twinagent");
}
