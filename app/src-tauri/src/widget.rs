//! Pill window placement + tray icon (TWI-11).
//!
//! The pill sits top-center of whichever monitor the user last dragged it
//! to; the choice is persisted so a restart puts it back. The tray icon is
//! the fallback surface when the pill is hidden, and hosts Quit/Settings.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, PhysicalPosition, WebviewWindow};

#[derive(Debug, Default, Serialize, Deserialize)]
struct WidgetConfig {
    /// OS name of the monitor the pill lives on (e.g. `\\.\DISPLAY1`).
    monitor: Option<String>,
}

fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("widget.json")
}

fn load_config(data_dir: &Path) -> WidgetConfig {
    std::fs::read_to_string(config_path(data_dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_config(data_dir: &Path, config: &WidgetConfig) {
    if let Ok(json) = serde_json::to_string_pretty(config) {
        let _ = std::fs::write(config_path(data_dir), json);
    }
}

/// Put the pill top-center on the remembered monitor (or wherever it is
/// now, on first run).
pub fn position_pill(window: &WebviewWindow, data_dir: &Path) {
    let saved = load_config(data_dir).monitor;
    let monitor = match (&saved, window.available_monitors()) {
        (Some(name), Ok(monitors)) => monitors
            .into_iter()
            .find(|m| m.name().map(|n| n.as_str()) == Some(name.as_str())),
        _ => None,
    }
    .or_else(|| window.current_monitor().ok().flatten());

    let Some(monitor) = monitor else { return };
    let width = window
        .outer_size()
        .map(|s| s.width as i32)
        .unwrap_or_default();
    let x = monitor.position().x + (monitor.size().width as i32 - width) / 2;
    let y = monitor.position().y;
    let _ = window.set_position(PhysicalPosition::new(x, y));
}

/// Persist the monitor whenever a drag ends somewhere new, so restarts
/// bring the pill back to the same screen.
pub fn remember_monitor_on_move(window: &WebviewWindow, data_dir: PathBuf) {
    let tracked = window.clone();
    let last: Mutex<Option<String>> = Mutex::new(load_config(&data_dir).monitor);
    window.on_window_event(move |event| {
        if !matches!(event, tauri::WindowEvent::Moved(_)) {
            return;
        }
        let Ok(Some(monitor)) = tracked.current_monitor() else {
            return;
        };
        let Some(name) = monitor.name().cloned() else {
            return;
        };
        let mut last = last.lock().unwrap();
        if last.as_deref() != Some(name.as_str()) {
            *last = Some(name.clone());
            save_config(
                &data_dir,
                &WidgetConfig {
                    monitor: Some(name),
                },
            );
        }
    });
}

/// Tray icon: the fallback surface. Left menu: show the pill, settings
/// (arrives with TWI-15), quit.
pub fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show pill", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Settings", false, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Twinagent", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &settings, &quit])?;

    TrayIconBuilder::with_id("main")
        .icon(tray_icon())
        .tooltip("Twinagent — agent monitor")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

/// The tray dot, synthesized at runtime so no binary assets live in the
/// repo: Twinagent green with a 1px antialiased edge.
fn tray_icon() -> tauri::image::Image<'static> {
    const SIZE: usize = 32;
    const RADIUS: f32 = 13.0;
    let center = (SIZE as f32 - 1.0) / 2.0;
    let mut rgba = vec![0u8; SIZE * SIZE * 4];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();
            let alpha = ((RADIUS - dist).clamp(0.0, 1.0) * 255.0) as u8;
            let i = (y * SIZE + x) * 4;
            rgba[i] = 0x4a; // Twinagent working-green
            rgba[i + 1] = 0xde;
            rgba[i + 2] = 0x80;
            rgba[i + 3] = alpha;
        }
    }
    tauri::image::Image::new_owned(rgba, SIZE as u32, SIZE as u32)
}
