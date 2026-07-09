// Twinagent — notch widget. The Rust core embeds twin-hub (local-first) and
// watches the Windows-side agent dirs directly (TWI-8). Pill/panel window
// behaviors (always-on-top, frameless, hotkey) land with TWI-11/TWI-12.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let _hub = twin_hub::Hub::new();

    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running Twinagent");
}
