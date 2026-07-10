// User settings (TWI-15): loaded from the Rust side once at startup,
// written back through update_settings which persists and applies them.

import { invoke } from "@tauri-apps/api/core";

export interface Settings {
  hotkey: string;
  autostart: boolean;
  toasts: boolean;
  /** Panel stays open on blur until explicitly dismissed. */
  pinned: boolean;
  hub_port: number;
  /** Context-gauge tone boundaries: [elevated, high, critical] percent. */
  thresholds: [number, number, number];
}

export const DEFAULTS: Settings = {
  hotkey: "ctrl+shift+space",
  autostart: false,
  toasts: true,
  pinned: true,
  hub_port: 17871,
  thresholds: [50, 70, 90],
};

class SettingsState {
  current = $state<Settings>({ ...DEFAULTS });

  async load() {
    try {
      this.current = await invoke<Settings>("get_settings");
    } catch {
      // Outside Tauri (plain vite dev) the defaults stand.
    }
  }

  /** Persist + apply; throws with a message on e.g. an unparseable hotkey. */
  async save(next: Settings) {
    await invoke("update_settings", { settings: next });
    this.current = { ...next };
  }
}

export const settings = new SettingsState();
