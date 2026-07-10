<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { hub, connectHub } from "./lib/hub.svelte";
  import { settings } from "./lib/settings-store.svelte";
  import AgentCard from "./lib/AgentCard.svelte";
  import Settings from "./lib/Settings.svelte";
  import UsageFooter from "./lib/UsageFooter.svelte";

  connectHub();
  settings.load();

  // Settings pane (TWI-15), opened from the tray menu.
  let view = $state<"agents" | "settings">("agents");
  listen("open-settings", () => (view = "settings"));

  // A slow clock for relative times and reset countdowns.
  let now = $state(Date.now());
  setInterval(() => (now = Date.now()), 10_000);

  // Rust owns the expanded state (it owns the window size); the webview
  // mirrors it via the `panel` event.
  let expanded = $state(false);
  listen<boolean>("panel", (e) => {
    expanded = e.payload;
    if (!expanded) view = "agents";
  });

  function onKeydown(e: KeyboardEvent) {
    if (e.key === "Escape" && expanded) {
      invoke("set_panel", { expanded: false });
    }
  }

  // Click toggles the panel; a drag beyond a few pixels moves the window
  // instead. Manual detection because a native drag region swallows clicks.
  function onPointerDown(e: PointerEvent) {
    if (e.button !== 0) return;
    const startX = e.screenX;
    const startY = e.screenY;
    let dragging = false;

    const onMove = (ev: PointerEvent) => {
      if (
        !dragging &&
        Math.abs(ev.screenX - startX) + Math.abs(ev.screenY - startY) > 5
      ) {
        dragging = true;
        cleanup();
        getCurrentWindow().startDragging();
      }
    };
    const onUp = () => {
      cleanup();
      if (!dragging) invoke("toggle_panel");
    };
    const cleanup = () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
  }

  // The pill counts sessions that are engaged right now; finished and idle
  // sessions live in the panel, not here.
  const working = $derived(
    Object.values(hub.sessions).filter(
      (s) => s.status === "thinking" || s.status === "tool_running",
    ).length,
  );
  const needsYou = $derived(
    Object.values(hub.sessions).filter((s) => s.status === "needs_you").length,
  );
  const failed = $derived(
    Object.values(hub.sessions).filter((s) => s.status === "failed").length,
  );
  const active = $derived(working + needsYou);

  // One dot, one truth: red beats amber beats green beats gray.
  const tone = $derived(
    failed > 0
      ? "failed"
      : needsYou > 0
        ? "needs-you"
        : working > 0
          ? "working"
          : "quiet",
  );

  // Panel: cards grouped by machine, most recent activity first, attention
  // floats to the top within a group.
  const STATUS_RANK: Record<string, number> = {
    needs_you: 0,
    failed: 1,
    tool_running: 2,
    thinking: 3,
    done: 4,
    idle: 5,
    stale: 6,
  };
  const machines = $derived.by(() => {
    const groups = new Map<string, (typeof hub.sessions)[string][]>();
    for (const s of Object.values(hub.sessions)) {
      const list = groups.get(s.machine) ?? [];
      list.push(s);
      groups.set(s.machine, list);
    }
    for (const list of groups.values()) {
      list.sort(
        (a, b) =>
          (STATUS_RANK[a.status] ?? 9) - (STATUS_RANK[b.status] ?? 9) ||
          b.last_activity.localeCompare(a.last_activity),
      );
    }
    return [...groups.entries()].sort((a, b) => a[0].localeCompare(b[0]));
  });
  const usageReports = $derived(
    Object.values(hub.usage).sort((a, b) => a.machine.localeCompare(b.machine)),
  );
</script>

<svelte:window onkeydown={onKeydown} />

<main class="shell" class:expanded>
  <div
    class="pill"
    class:offline={!hub.connected}
    onpointerdown={onPointerDown}
    role="button"
    tabindex="0"
    aria-expanded={expanded}
    aria-label="Toggle agent panel"
  >
    <span class="dot {tone}" class:breathing={working > 0}></span>
    <span class="count">{active}</span>
    <span class="label">{active === 1 ? "agent" : "agents"}</span>
    {#if needsYou > 0}
      <span class="chip needs-you"
        >{needsYou} need{needsYou === 1 ? "s" : ""} you</span
      >
    {/if}
    {#if failed > 0}
      <span class="chip failed">{failed} failed</span>
    {/if}
    {#if !hub.connected}
      <span class="chip offline-chip">connecting…</span>
    {/if}
  </div>

  {#if expanded}
    <section class="panel">
      {#if view === "settings"}
        <Settings onclose={() => (view = "agents")} />
      {:else}
        <div class="sessions">
          {#if machines.length === 0}
            <p class="empty">
              No agents yet.
              <span>Sessions appear here when Claude Code or Codex runs.</span>
            </p>
          {:else}
            {#each machines as [machine, sessions] (machine)}
              <h3 class="machine-header">{machine}</h3>
              {#each sessions as session (`${session.source}/${session.agent_id}`)}
                <AgentCard {session} {now} />
              {/each}
            {/each}
          {/if}
        </div>
        <UsageFooter reports={usageReports} {now} />
      {/if}
    </section>
  {/if}
</main>

<style>
  :global(html, body) {
    margin: 0;
    background: transparent;
    overflow: hidden;
    user-select: none;
  }

  .shell {
    display: flex;
    flex-direction: column;
    align-items: center;
    height: 100vh;
  }

  .pill {
    display: flex;
    align-items: center;
    gap: 7px;
    height: 34px;
    margin-top: 10px;
    width: fit-content;
    padding: 0 14px;
    border-radius: 17px;
    background: rgba(16, 17, 20, 0.92);
    border: 1px solid rgba(255, 255, 255, 0.07);
    color: #dee1e6;
    font:
      500 12.5px/1 "Segoe UI Variable Text",
      "Segoe UI",
      system-ui,
      sans-serif;
    box-shadow:
      0 1px 2px rgba(0, 0, 0, 0.3),
      0 6px 22px rgba(0, 0, 0, 0.38);
    cursor: pointer;
  }

  .pill:focus-visible {
    outline: 2px solid #4ade80;
    outline-offset: 2px;
  }

  .pill.offline {
    color: #8a8f98;
  }

  .count {
    font-variant-numeric: tabular-nums;
    font-weight: 600;
  }

  .label {
    color: #8a8f98;
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: #565b64;
    flex: none;
  }

  .dot.working {
    background: #4ade80;
  }

  .dot.needs-you {
    background: #f5a623;
  }

  .dot.failed {
    background: #f0442c;
  }

  /* The widget's one flourish: it breathes while agents work. */
  .dot.breathing {
    animation: breathe 2.4s ease-in-out infinite;
  }

  @keyframes breathe {
    0%,
    100% {
      box-shadow: 0 0 0 0 rgba(74, 222, 128, 0.45);
    }
    50% {
      box-shadow: 0 0 6px 2px rgba(74, 222, 128, 0.25);
    }
  }

  .chip {
    padding: 3px 8px;
    border-radius: 10px;
    font-size: 11.5px;
    font-weight: 600;
  }

  .chip.needs-you {
    color: #ffc46b;
    background: rgba(245, 166, 35, 0.14);
  }

  .chip.failed {
    color: #ff8a75;
    background: rgba(240, 68, 44, 0.14);
  }

  .chip.offline-chip {
    color: #8a8f98;
    background: rgba(255, 255, 255, 0.06);
    font-weight: 500;
  }

  /* The window is resized by Rust before this mounts; the content slides
     into the new space rather than popping. */
  .panel {
    width: calc(100% - 24px);
    flex: 1;
    margin: 8px 12px 12px;
    border-radius: 14px;
    background: rgba(16, 17, 20, 0.94);
    border: 1px solid rgba(255, 255, 255, 0.07);
    box-shadow:
      0 1px 2px rgba(0, 0, 0, 0.3),
      0 10px 32px rgba(0, 0, 0, 0.42);
    color: #dee1e6;
    overflow: hidden;
    animation: reveal 160ms ease-out;
  }

  @keyframes reveal {
    from {
      opacity: 0;
      transform: translateY(-6px);
    }
    to {
      opacity: 1;
      transform: translateY(0);
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .dot.breathing,
    .panel {
      animation: none;
    }
  }

  .panel {
    display: flex;
    flex-direction: column;
    font:
      400 12.5px/1.4 "Segoe UI Variable Text",
      "Segoe UI",
      system-ui,
      sans-serif;
  }

  .sessions {
    flex: 1;
    overflow-y: auto;
    padding: 10px 12px;
    display: flex;
    flex-direction: column;
    gap: 6px;
    scrollbar-width: thin;
    scrollbar-color: rgba(255, 255, 255, 0.15) transparent;
  }

  .machine-header {
    margin: 4px 0 2px;
    font-size: 10.5px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: #565b64;
  }

  .machine-header:first-child {
    margin-top: 0;
  }

  .empty {
    margin: 24px 8px;
    text-align: center;
    color: #8a8f98;
  }

  .empty span {
    display: block;
    margin-top: 4px;
    font-size: 11px;
    color: #565b64;
  }
</style>
