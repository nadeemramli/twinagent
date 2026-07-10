//! Pill window placement + tray icon (TWI-11).
//!
//! The pill sits top-center of whichever monitor the user last dragged it
//! to; the choice is persisted so a restart puts it back. The tray icon is
//! the fallback surface when the pill is hidden, and hosts Quit/Settings.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, LogicalSize, Manager, PhysicalPosition, WebviewWindow};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

/// Pill and panel logical sizes; width is shared so expanding only grows
/// downward and no horizontal repositioning is needed.
const WIDTH: f64 = 420.0;
const PILL_HEIGHT: f64 = 64.0;
const PANEL_HEIGHT: f64 = 560.0;
/// Default toggle hotkey; rebindable via widget.json (TWI-15 adds UI).
const DEFAULT_HOTKEY: &str = "ctrl+shift+space";

/// Whether the window is currently the full panel (managed app state).
#[derive(Default)]
pub struct PanelState(AtomicBool);

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct WidgetConfig {
    /// OS name of the monitor the pill lives on (e.g. `\\.\DISPLAY1`).
    monitor: Option<String>,
    /// Global toggle shortcut, e.g. `ctrl+shift+space`.
    hotkey: Option<String>,
    /// Launch at login (TWI-15).
    autostart: Option<bool>,
    /// Toast on needs-you transitions (TWI-15; per-alert-type splits when
    /// more alert types exist).
    toasts: Option<bool>,
    /// Hub bind port — applies on next launch (`TWIN_HUB_ADDR` still wins).
    hub_port: Option<u16>,
    /// Context-gauge tone boundaries: [elevated, high, critical] percent.
    thresholds: Option<[u8; 3]>,
}

/// The user-facing settings view of the config — what the panel's settings
/// pane edits (TWI-15). Window placement stays internal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub hotkey: String,
    pub autostart: bool,
    pub toasts: bool,
    pub hub_port: u16,
    pub thresholds: [u8; 3],
}

impl From<&WidgetConfig> for Settings {
    fn from(config: &WidgetConfig) -> Self {
        Self {
            hotkey: config.hotkey.clone().unwrap_or_else(|| DEFAULT_HOTKEY.into()),
            autostart: config.autostart.unwrap_or(false),
            toasts: config.toasts.unwrap_or(true),
            hub_port: config.hub_port.unwrap_or(17871),
            thresholds: config.thresholds.unwrap_or([50, 70, 90]),
        }
    }
}

pub fn load_settings(data_dir: &Path) -> Settings {
    Settings::from(&load_config(data_dir))
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
            // Load-modify-save so other settings (hotkey, and whatever
            // TWI-15 adds) survive a monitor change.
            let mut config = load_config(&data_dir);
            config.monitor = Some(name);
            save_config(&data_dir, &config);
        }
    });
}

/// Switch between pill and panel (TWI-12). The window only grows downward
/// (shared width, top edge fixed), and the webview hears about it via a
/// `panel` event so it can stage the content transition.
pub fn set_expanded(window: &WebviewWindow, expanded: bool) {
    let state = window.state::<PanelState>();
    state.0.store(expanded, Ordering::Relaxed);
    let height = if expanded { PANEL_HEIGHT } else { PILL_HEIGHT };
    let _ = window.set_size(LogicalSize::new(WIDTH, height));
    if expanded {
        let _ = window.set_focus();
    }
    let _ = window.emit("panel", expanded);
}

pub fn toggle_panel(window: &WebviewWindow) {
    let expanded = window.state::<PanelState>().0.load(Ordering::Relaxed);
    set_expanded(window, !expanded);
}

/// Register the global toggle hotkey from widget.json (default
/// `ctrl+shift+space`). An unparseable binding falls back to the default
/// rather than leaving the widget hotkey-less.
pub fn setup_hotkey(app: &tauri::App, data_dir: &Path) -> tauri::Result<()> {
    let binding = load_config(data_dir)
        .hotkey
        .unwrap_or_else(|| DEFAULT_HOTKEY.into());
    let shortcut: Shortcut = binding
        .parse()
        .or_else(|_| DEFAULT_HOTKEY.parse())
        .expect("default hotkey parses");

    app.handle().plugin(
        tauri_plugin_global_shortcut::Builder::new()
            .with_handler(move |app, _shortcut, event| {
                if event.state() == ShortcutState::Pressed {
                    if let Some(window) = app.get_webview_window("main") {
                        toggle_panel(&window);
                    }
                }
            })
            .build(),
    )?;
    if let Err(err) = app.global_shortcut().register(shortcut) {
        // Another app may own the combo; the pill still works by click.
        eprintln!("twinagent: could not register hotkey {binding}: {err}");
    }
    Ok(())
}

#[tauri::command]
pub fn get_settings(app: tauri::AppHandle) -> Result<Settings, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(load_settings(&data_dir))
}

/// Persist and apply settings. Hotkey, toasts, and autostart take effect
/// immediately; the hub port applies on next launch.
#[tauri::command]
pub fn update_settings(app: tauri::AppHandle, settings: Settings) -> Result<(), String> {
    let shortcut: Shortcut = settings
        .hotkey
        .parse()
        .map_err(|_| format!("unparseable hotkey: {}", settings.hotkey))?;

    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let mut config = load_config(&data_dir);
    config.hotkey = Some(settings.hotkey.clone());
    config.autostart = Some(settings.autostart);
    config.toasts = Some(settings.toasts);
    config.hub_port = Some(settings.hub_port);
    config.thresholds = Some(settings.thresholds);
    save_config(&data_dir, &config);

    let shortcuts = app.global_shortcut();
    let _ = shortcuts.unregister_all();
    if let Err(err) = shortcuts.register(shortcut) {
        eprintln!("twinagent: could not register hotkey {}: {err}", settings.hotkey);
    }

    apply_autostart(&app, settings.autostart);

    app.state::<crate::notify::ToastsEnabled>()
        .0
        .store(settings.toasts, Ordering::Relaxed);
    Ok(())
}

/// Converge the OS launch-at-login entry with the setting; disabling an
/// entry that never existed is fine to ignore.
pub fn apply_autostart(app: &tauri::AppHandle, enabled: bool) {
    use tauri_plugin_autostart::ManagerExt;
    let autolaunch = app.autolaunch();
    let _ = if enabled {
        autolaunch.enable()
    } else {
        autolaunch.disable()
    };
}

/// Collapse when focus leaves the panel — the widget must never sit
/// expanded over someone's work.
pub fn collapse_on_blur(window: &WebviewWindow) {
    let tracked = window.clone();
    window.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Focused(false))
            && tracked.state::<PanelState>().0.load(Ordering::Relaxed)
        {
            set_expanded(&tracked, false);
        }
    });
}

/// Tray icon: the fallback surface. Left menu: show the pill, settings
/// (arrives with TWI-15), quit.
pub fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show pill", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Settings", true, None::<&str>)?;
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
            "settings" => {
                // Open the panel on its settings pane (TWI-15).
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    set_expanded(&window, true);
                    let _ = window.emit("open-settings", ());
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
