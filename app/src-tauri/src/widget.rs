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

/// Whether the panel stays open when focus leaves it (managed app state,
/// mirrors the persisted `pinned` setting).
pub struct PinState(pub AtomicBool);

/// Rendered pill width in logical px, reported by the webview's
/// ResizeObserver — the hit target for click-through tracking. The window
/// is a fixed 420-wide rectangle; everything outside the pill is
/// transparent air that must not swallow clicks.
pub struct PillWidth(pub std::sync::atomic::AtomicU32);

impl Default for PillWidth {
    fn default() -> Self {
        // Roomy fallback until the webview reports ("0 agents" ≈ 120px).
        Self(std::sync::atomic::AtomicU32::new(200))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct WidgetConfig {
    /// OS name of the monitor the pill lives on (e.g. `\\.\DISPLAY1`).
    monitor: Option<String>,
    /// Last dragged position (physical px) — "put it aside" survives a
    /// restart. When unset (or off-screen), fall back to top-center.
    position: Option<(i32, i32)>,
    /// Global toggle shortcut, e.g. `ctrl+shift+space`.
    hotkey: Option<String>,
    /// Panel stays open on blur until explicitly dismissed.
    pinned: Option<bool>,
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
    pub pinned: bool,
    pub hub_port: u16,
    pub thresholds: [u8; 3],
}

impl From<&WidgetConfig> for Settings {
    fn from(config: &WidgetConfig) -> Self {
        Self {
            hotkey: config.hotkey.clone().unwrap_or_else(|| DEFAULT_HOTKEY.into()),
            autostart: config.autostart.unwrap_or(false),
            toasts: config.toasts.unwrap_or(true),
            pinned: config.pinned.unwrap_or(true),
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

/// Restore the pill where the user last dragged it; without a saved (and
/// still on-screen) position, top-center of the remembered monitor.
pub fn position_pill(window: &WebviewWindow, data_dir: &Path) {
    let config = load_config(data_dir);
    if let Some((x, y)) = config.position {
        let on_screen = window.available_monitors().is_ok_and(|monitors| {
            monitors.iter().any(|m| {
                let p = m.position();
                let s = m.size();
                // A little slack so "mostly on this monitor" counts.
                x >= p.x - 50
                    && x < p.x + s.width as i32
                    && y >= p.y - 50
                    && y < p.y + s.height as i32
            })
        });
        if on_screen {
            let _ = window.set_position(PhysicalPosition::new(x, y));
            return;
        }
    }
    let saved = config.monitor;
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

/// Persist position + monitor as the user drags the pill around, so a
/// restart puts it back exactly where they left it. Mid-drag writes are
/// throttled; the final resting spot is flushed on blur.
pub fn remember_monitor_on_move(window: &WebviewWindow, data_dir: PathBuf) {
    let tracked = window.clone();
    let last_save: Mutex<std::time::Instant> =
        Mutex::new(std::time::Instant::now() - std::time::Duration::from_secs(1));
    window.on_window_event(move |event| match event {
        tauri::WindowEvent::Moved(pos) => {
            let mut last = last_save.lock().unwrap();
            if last.elapsed() < std::time::Duration::from_millis(400) {
                return;
            }
            *last = std::time::Instant::now();
            persist_placement(&tracked, &data_dir, (pos.x, pos.y));
        }
        tauri::WindowEvent::Focused(false) => {
            if let Ok(pos) = tracked.outer_position() {
                persist_placement(&tracked, &data_dir, (pos.x, pos.y));
            }
        }
        _ => {}
    });
}

/// Load-modify-save so unrelated settings survive a placement change.
fn persist_placement(window: &WebviewWindow, data_dir: &Path, position: (i32, i32)) {
    let mut config = load_config(data_dir);
    config.position = Some(position);
    if let Ok(Some(monitor)) = window.current_monitor() {
        if let Some(name) = monitor.name() {
            config.monitor = Some(name.clone());
        }
    }
    save_config(data_dir, &config);
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
    config.pinned = Some(settings.pinned);
    config.hub_port = Some(settings.hub_port);
    config.thresholds = Some(settings.thresholds);
    save_config(&data_dir, &config);

    app.state::<PinState>()
        .0
        .store(settings.pinned, Ordering::Relaxed);

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

/// The pill's CSS geometry inside the window (logical px): margin-top 10,
/// height 34, horizontally centered — must match App.svelte's `.pill`.
const PILL_TOP: f64 = 10.0;
const PILL_VISUAL_HEIGHT: f64 = 34.0;
/// Hover slack so the edge of the pill isn't fiddly to hit.
const PILL_SLACK: f64 = 6.0;

#[tauri::command]
pub fn set_pill_size(app: tauri::AppHandle, width: f64) {
    app.state::<PillWidth>()
        .0
        .store(width.max(1.0) as u32, Ordering::Relaxed);
}

/// Make the window click-through except when the cursor is actually over
/// the pill (or the panel is open). The window is a fixed-size transparent
/// rectangle; without this, its invisible margins steal clicks from
/// whatever the pill floats over. Cursor-poll + `set_ignore_cursor_events`
/// is the standard pattern — per-region hit testing doesn't exist for
/// webview windows.
pub fn setup_click_through(window: &WebviewWindow) {
    let tracked = window.clone();
    tauri::async_runtime::spawn(async move {
        let mut interactive = true;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            let want = wants_interaction(&tracked);
            if want != interactive {
                interactive = want;
                let _ = tracked.set_ignore_cursor_events(!want);
            }
        }
    });
}

fn wants_interaction(window: &WebviewWindow) -> bool {
    // Expanded panel: the whole window is real UI.
    if window.state::<PanelState>().0.load(Ordering::Relaxed) {
        return true;
    }
    let (Ok(pos), Ok(size), Ok(cursor)) = (
        window.outer_position(),
        window.outer_size(),
        window.cursor_position(),
    ) else {
        // Can't tell — stay interactive rather than locking the user out.
        return true;
    };
    let scale = window.scale_factor().unwrap_or(1.0);
    let pill_width =
        window.state::<PillWidth>().0.load(Ordering::Relaxed) as f64 * scale;
    let slack = PILL_SLACK * scale;

    let center_x = pos.x as f64 + size.width as f64 / 2.0;
    let left = center_x - pill_width / 2.0 - slack;
    let right = center_x + pill_width / 2.0 + slack;
    let top = pos.y as f64 + PILL_TOP * scale - slack;
    let bottom = pos.y as f64 + (PILL_TOP + PILL_VISUAL_HEIGHT) * scale + slack;

    cursor.x >= left && cursor.x <= right && cursor.y >= top && cursor.y <= bottom
}

/// Collapse when focus leaves the panel — unless the user pinned it open
/// (the default): then only Esc, the pill, or the hotkey dismiss it.
pub fn collapse_on_blur(window: &WebviewWindow) {
    let tracked = window.clone();
    window.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Focused(false))
            && tracked.state::<PanelState>().0.load(Ordering::Relaxed)
            && !tracked.state::<PinState>().0.load(Ordering::Relaxed)
        {
            set_expanded(&tracked, false);
        }
    });
}

/// Tray icon: the fallback surface. Left-click brings the pill back;
/// the menu adds hide/settings/quit.
pub fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show pill", true, None::<&str>)?;
    let hide = MenuItem::with_id(app, "hide", "Hide pill", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Settings", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Twinagent", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &hide, &settings, &quit])?;

    TrayIconBuilder::with_id("main")
        .icon(tray_icon())
        .tooltip("Twinagent — agent monitor")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            // Left-click = bring the pill back (after "Hide pill").
            if let tauri::tray::TrayIconEvent::Click {
                button: tauri::tray::MouseButton::Left,
                button_state: tauri::tray::MouseButtonState::Up,
                ..
            } = event
            {
                if let Some(window) = tray.app_handle().get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        })
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            "hide" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
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
