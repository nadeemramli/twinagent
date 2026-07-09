<script lang="ts">
  import { hub, connectHub } from "./lib/hub.svelte";

  connectHub();

  // The pill counts sessions that are engaged right now; finished and idle
  // sessions live in the panel (TWI-13), not here.
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
</script>

<main class="pill" data-tauri-drag-region class:offline={!hub.connected}>
  <span class="dot {tone}" class:breathing={working > 0}></span>
  <span class="count">{active}</span>
  <span class="label">{active === 1 ? "agent" : "agents"}</span>
  {#if needsYou > 0}
    <span class="chip needs-you">{needsYou} need{needsYou === 1 ? "s" : ""} you</span>
  {/if}
  {#if failed > 0}
    <span class="chip failed">{failed} failed</span>
  {/if}
  {#if !hub.connected}
    <span class="chip offline-chip">connecting…</span>
  {/if}
</main>

<style>
  :global(html, body) {
    margin: 0;
    background: transparent;
    overflow: hidden;
    user-select: none;
  }

  .pill {
    display: flex;
    align-items: center;
    gap: 7px;
    height: 34px;
    margin: 10px auto 0;
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

  @media (prefers-reduced-motion: reduce) {
    .dot.breathing {
      animation: none;
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
</style>
