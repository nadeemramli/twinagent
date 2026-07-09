# Twinagent

Private, local-first command center for monitoring AI agent work (Claude Code, Codex, VPS agents) from a Superwhisper-style notch widget on Windows + WSL.

Inspired by [AgentNotch](https://github.com/appgram/agentnotch) (open source, macOS-only). Twinagent is the Windows/WSL equivalent, extended with multi-machine visibility and plan-usage tracking. Detection needs no APIs and no scraping: agent CLIs write session transcripts to disk.

## Architecture

```
agent activity ──► collector (normalize) ──► hub (state, rules, alerts) ──► widget
```

| Piece | Crate | Runs |
|---|---|---|
| Shared types, JSONL parsers, session state machine | `crates/twin-core` | everywhere |
| Session registry, SQLite history, WebSocket push | `crates/twin-hub` | embedded in the app (default) or standalone on a VPS (Phase 3) |
| File-watching daemon | `crates/twin-collector` | inside WSL (systemd user service); later on VPS |
| Notch widget (Tauri v2 + Svelte) | `app/` | Windows |

Key constraints:

- **inotify does not cross the WSL/Windows boundary** — the collector must run inside WSL; the app watches the Windows-side dirs (`C:\Users\<user>\{.claude,.codex}`) natively in its Rust core.
- **Local-first**: no third-party service sees prompts, code, or credentials.

## Building

```sh
cargo build            # core + hub + collector (no GUI deps needed)
cargo test
```

The widget builds on Windows (rustup + VS Build Tools + WebView2):

```sh
cd app && npm install && npm run tauri dev
```

Optional Linux preview under WSLg first needs:

```sh
sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file \
  libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev
```

## Tracking

- Roadmap + issues: Linear project “Twinagent — agent monitoring notch” (team TWI).
- PRD and research notes: Obsidian `build-blog/build-vault/5. Idea Vault/Application/B2C/Active/Twinagent/`.
