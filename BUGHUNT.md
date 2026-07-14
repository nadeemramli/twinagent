# Twinagent bug hunt — findings & fix plan

Method: 6 parallel reviewers (disjoint areas) → each finding adversarially verified by
2 independent refuters. A finding is **CONFIRMED** only if both refuters failed to refute it.
The Anthropic session limit knocked out the `usage`/`app` verifiers mid-run, so those show
as **UNVERIFIED** — two of them I re-verified by hand and promote to confirmed below.

Legend: severity in (parens). Fix tiers: **T1** break the product today, **T2** robustness/
correctness under real conditions, **T3** edge/cosmetic.

---

## Tier 1 — actively degrade the product (fix now)

### 1. (high) Frontend WebSocket ignores the configured hub port — `app/src/lib/hub.svelte.ts:147`
Self-verified. `HUB_WS = "ws://127.0.0.1:17871/..."` is hardcoded, but the hub binds
`settings.hub_port` (`main.rs:133`). Change the port in Settings → the panel connects to the
wrong port → no sessions, no usage, nothing. The `hub_port` setting is a silent footgun.
**Fix:** expose the port to the webview (a `get_hub_port` Tauri command reading the same
`Settings`, or bake it into the served page) and build the WS URL from it. Default stays 17871.

### 2. (high) Approved permission prompt stays "needs you" for the whole tool run — `crates/twin-core/src/claude_code.rs:117`
`awaiting_permission()` only clears when a transcript record newer than the hook timestamp
arrives — but approving a prompt writes nothing to the transcript; the next record is the
`tool_result` when the tool finishes. Approve a `Bash: cargo build`, the build runs 10 min →
the card shows **needs-you** (with the stale prompt text) the entire time. This is the exact
false-alarm class we removed the 2.5s heuristic for, reintroduced post-approval.
**Fix (recommended):** add a companion **PostToolUse** (or Stop) Claude Code hook that appends
a `{"session_id":...,"type":"resolved"}` line to the same `twinagent-notify.jsonl`; clear
`permission_request` when a resolved event for that session arrives after the prompt ts. This
keeps needs-you hook-driven and deterministic. Requires a one-line settings.json hook addition
on both sides (mirror the existing Notification hook). Alternative if we don't want a 2nd hook:
clear the prompt as soon as the pending tool set *changes* after the prompt (approve → the model
emits the next tool_use; deny → interrupted) — weaker, misses the single-long-tool case.

### 3. (high) Plan estimate & stats double-count duplicate message IDs across files — `crates/twin-core/src/plan_usage.rs:89` (+ `stats.rs`)
`claude_plan_estimate` / `usage_stats` flatten `usage_events()` from every per-file tracker;
dedup-by-message-id is per-tracker only. A message id that appears in two transcript files
(session resumed into a new file, compaction copy, sidechain re-emit) is counted twice — inflating
5h/weekly token totals and the stats-pane cost.
**Fix:** dedup by message id when aggregating across trackers. Cleanest: have `usage_events()`
return `(message_id, ts, TokenCounts)` and let `claude_plan_estimate` / stats keep a
`HashSet<String>` of seen ids (or a `HashMap<id, TokenCounts>` taking last-write) before summing.

### 4. (high) Collector treats partial flush success as total failure — `crates/twin-collector/src/lib.rs:194`
`flush()` runs three POSTs (snapshots, usage, stats) in one `?`-chain. If `/v1/usage` or
`/v1/stats` fails persistently (schema skew, transient 5xx) after snapshots succeeded, the whole
result is `Err` → the collector **rotates away from the working endpoint** and pins backoff at
30s, throttling snapshot delivery indefinitely — even though snapshots were fine.
**Fix:** send the three independently; only rotate/backoff on the *snapshots* failure. Track
per-payload success so a stuck usage/stats POST can't starve snapshots. Combine with #5/#8.

---

## Tier 2 — real correctness/robustness bugs

### 5. (medium) Poison-batch deadlock — `crates/twin-collector/src/lib.rs:174`
A 4xx to the snapshots POST (schema skew) keeps the identical batch buffered and retried forever,
halting *all* snapshot delivery. **Fix:** on a 4xx (non-retryable) drop the batch and log; keep
buffering only on 5xx/transport errors.

### 6. (medium) `sweep()` broadcasts stale snapshots after dropping the lock — `crates/twin-hub/src/lib.rs:160`
`mark_stale`/`prune` collect keys under the lock, then persist+broadcast after releasing it; a
concurrent `ingest` (e.g. a `Done` update) can be overwritten in the DB and UI by the sweep's
older snapshot. **Fix:** re-read each key under the lock right before persisting, or persist inside
the locked section; broadcast the value actually stored.

### 7. (medium) `upsert()` visible-change check omits fields — `crates/twin-hub/src/registry.rs:69`
Changes to `git_branch`, `needs_user_reason`, or `jump` alone return `Unchanged`, so the old
snapshot is kept and never broadcast (e.g. a branch switch or a newly-populated jump target never
reaches the UI). **Fix:** include those three fields in the comparison.

### 8. (medium) Immortal session on unparseable `last_activity` — `crates/twin-hub/src/registry.rs:124`
`mark_stale` `continue`s and `prune` `unwrap_or(false)` on a bad timestamp → the session is never
staled and never pruned, leaking a card and a DB row forever. **Fix:** treat an unparseable
`last_activity` as maximally old (stale + prunable), or drop it on ingest.

### 9. (medium) Lagged-WS resync omits usage reports — `crates/twin-hub/src/server.rs:132`
On WS lag the server resends sessions but not usage; a dropped `Usage` event leaves the widget
showing stale plan usage until the next change. **Fix:** on resync, also re-send current
`usage_reports()` (and stats) as it does the full session set.

### 10. (medium) Rule-2 dedup applies only to `Event::Full`, not `Upsert` — `crates/twin-hub/src/lib.rs:137`
Merged cross-side cards immediately split back into duplicates on the next upsert diff. **Fix:**
apply the same logical-key dedup to the upsert broadcast path (frontend `apply` should also merge —
see #16).

### 11. (medium) TailReader loses a file's final lines on a transient read error — `crates/twin-core/src/pipeline.rs:180`
A one-off `reader.poll()` `Err` advances past the tail; the last lines are never re-read. **Fix:**
don't advance the offset on error — retry the same offset next poll.

### 12. (medium) Truncation/rotation re-feeds the whole file, double-counting Codex usage — `crates/twin-core/src/pipeline.rs:178` / `tail.rs:61`
On truncation `TailReader` resets `offset=0` and re-emits the whole file into the *same* tracker,
so Codex cumulative-delta logic double-counts; a rotation to a same-or-longer file resumes
mid-line. **Fix:** on shrink, reset the tracker (new `FileSession`) alongside the reader; detect
rotation by inode/created-time, not just size, and always resume on a line boundary.

### 13. (medium) Notification-hook events dropped if no tracker matches yet — `crates/twin-core/src/pipeline.rs:248`
A permission event whose `session_id` has no tracker (transcript not yet discovered) is silently
dropped. **Fix:** buffer unmatched permission events briefly and re-attempt on subsequent polls.

### 14. (medium) Deleted-then-recreated root never re-registered with the watcher — `crates/twin-core/src/watch.rs:132`
`watched_roots` remembers the path so `rescan` never re-watches it after the dir is recreated →
inotify events stop; only the fallback poll survives. **Fix:** drop the root from `watched_roots`
when it disappears so the next rescan re-registers it.

### 15. (medium) Blocking HTTP on the collector's only thread — `crates/twin-collector/src/main.rs:45` (+ `claude_plan_api.rs:172`, `embedded.rs:51`)
Inline blocking POSTs (3×5s hub + 10s Anthropic poll) stall file tailing and needs-you delivery
up to ~15s/iteration. **Fix:** move the plan poll and hub POSTs off the poll thread (a channel to
a sender thread), or drop timeouts to ~2s. Lower priority than the logic bugs; note in plan.

---

## Tier 3 — edge cases, cosmetic, or design calls (self-verify before fixing)

- (medium, UNVERIFIED) `stats.rs:102` — nonexistent/ambiguous local midnight (DST) makes `today` == `all`. Fix: fall back to `now - 24h` when `single()` is None.
- (medium, UNVERIFIED) `Settings.svelte:9` — draft shallow-copy shares the `thresholds` array with `settings.current`; threshold edits apply without Save. Fix: deep-copy the array in the draft.
- (medium, UNVERIFIED) `widget.rs:181` — unsynchronized load-modify-save of `widget.json` races window-move vs update_settings; non-atomic write. Fix: a mutex around config I/O + write-temp-then-rename.
- (medium, UNVERIFIED) `hub.svelte.ts:126` — frontend `upsert` bypasses dedup/freshness (mirror of #10). Fix with #10.
- (low, SPLIT) `codex.rs:151` — the reset-detection concern was refuted by the real resumed-session fixture (totals continue cumulatively, don't reset to 0). **No fix; the current code is correct.**
- (low) `watch.rs:157` size-only change detection; `tail.rs` same-length replacement — same root as #12.
- (low) `dedup.rs:101` rule-2 over-merges two distinct single-per-side sessions in the same aliased dir. Design call.
- (low, UNVERIFIED) `stats.rs:24/144` substring pricing misprices legacy Opus/Haiku & non-Claude sources; `notify.rs:44` toast map grows unbounded; `hub.svelte.ts:139` dead-collector usage shown as current; `jump.rs:131` unencoded vscode URIs; `install-collector.sh:19` no restart on reinstall. Batch these opportunistically.

**Refuted (no fix — verifiers showed the code is correct):** pending-tool-blocks-stale (both parsers),
orphaned-tool_use, awaiting_permission tie-at-equal-ts, walk() symlink loop, bare last_activity bump.
