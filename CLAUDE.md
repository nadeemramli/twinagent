# Twinagent — agent context

Private, local-first notch widget for monitoring AI agents (Claude Code, Codex, VPS) on Windows + WSL. Rust + Tauri v2 + Svelte 5. Read `README.md` for the architecture sketch.

## Where things live

- **Issues/roadmap**: Linear team **TWI**, project "Twinagent — agent monitoring notch" (8 milestones, TWI-1…TWI-22). Work issues in milestone order; set state In Progress → Done via the Linear MCP as you go.
- **PRD + research**: Obsidian vault at `/mnt/c/Users/Nadeem/Desktop/Obsidian/build-blog/build-vault/5. Idea Vault/Application/B2C/Active/Twinagent/` — PRD.md, Phase 1 Breakdown.md, Reference/Agent Notch Reference Notes.md (parsing state machine details), Reference/Usage Tracking Research.md.
- **Adapter contract**: `crates/twin-core/src/model.rs` — every source normalizes to `AgentSnapshot`.

## Build & test

```sh
source ~/.cargo/env        # rustup-installed Rust in WSL
cargo build && cargo test  # core + hub + collector; app is NOT in default-members
cd app && npm install && npm run build   # frontend (use npm — pnpm's corepack shim is broken here)
```

The Tauri app compiles only where GUI libs exist: Windows (real target, see TWI-22) or WSL after `sudo apt install libwebkit2gtk-4.1-dev ...` (optional WSLg preview).

## Hard constraints (from the PRD — don't relearn these)

- **inotify does not cross the WSL/Windows boundary.** The collector watches WSL paths from inside WSL; the Tauri app watches `C:\Users\Nadeem\{.claude,.codex}` natively. Never watch `\\wsl$` or `/mnt/c` paths for file events.
- Agent CLIs run on BOTH sides of this machine (WSL and Windows dotdirs both active) — sessions from both must appear, machine-tagged, deduped (TWI-10).
- Claude Code state machine: needs-you comes ONLY from the Notification hook (`~/.claude/twinagent-notify.jsonl`, hook installed in both sides' settings.json) — a pending tool is just tool-running however long it takes (the old >2.5s heuristic false-alarmed on every long command). `stop_reason == "end_turn"` → done; ~10s silence → idle; result containing "rejected" → interrupted; done/needs-you never decay to stale.
- Codex rollout files carry exact plan usage: `event_msg.token_count.rate_limits` → `primary.used_percent` (5h), `secondary.used_percent` (weekly), `plan_type`, `resets_at`.
- Usage readings are tagged `exact` / `estimated` / `stale` (`ReadingConfidence`) — never present an estimate as exact.
- Local-first and private: no third-party service may see prompts, code, or credentials.

## Test data

Real Claude Code transcripts live in `~/.claude/projects/` (WSL) — use copies in `crates/twin-core/tests/fixtures/` for golden tests (TWI-6), sanitized.
