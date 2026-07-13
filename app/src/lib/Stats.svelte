<script lang="ts">
  import { onMount } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { compact } from "./format";

  let { onclose }: { onclose: () => void } = $props();

  interface PeriodTotals {
    sessions: number;
    messages: number;
    input_tokens: number;
    output_tokens: number;
    cache_read_tokens: number;
    cache_creation_tokens: number;
    est_cost_usd: number | null;
  }
  interface PeriodStats {
    key: string;
    claude: PeriodTotals;
    codex: PeriodTotals;
  }
  interface UsageStats {
    machine: string;
    periods: PeriodStats[];
    computed_at: string;
  }

  let reports = $state<UsageStats[]>([]);
  let period = $state("30d");
  let loading = $state(true);
  let failed = $state(false);

  onMount(async () => {
    try {
      // Via the Rust side — the hub is in-process, and a webview fetch to
      // its HTTP port would be blocked by CORS.
      reports = await invoke<UsageStats[]>("get_stats");
    } catch {
      failed = true;
    } finally {
      loading = false;
    }
  });

  const PERIODS: [string, string][] = [
    ["today", "Today"],
    ["7d", "7 Days"],
    ["30d", "30 Days"],
    ["all", "All Time"],
  ];

  const zero = (): PeriodTotals => ({
    sessions: 0,
    messages: 0,
    input_tokens: 0,
    output_tokens: 0,
    cache_read_tokens: 0,
    cache_creation_tokens: 0,
    est_cost_usd: null,
  });

  function accumulate(into: PeriodTotals, from: PeriodTotals) {
    into.sessions += from.sessions;
    into.messages += from.messages;
    into.input_tokens += from.input_tokens;
    into.output_tokens += from.output_tokens;
    into.cache_read_tokens += from.cache_read_tokens;
    into.cache_creation_tokens += from.cache_creation_tokens;
    if (from.est_cost_usd != null) {
      into.est_cost_usd = (into.est_cost_usd ?? 0) + from.est_cost_usd;
    }
  }

  // Sum the selected period across machines, per source.
  const totals = $derived.by(() => {
    const claude = zero();
    const codex = zero();
    for (const r of reports) {
      const p = r.periods.find((p) => p.key === period);
      if (!p) continue;
      accumulate(claude, p.claude);
      accumulate(codex, p.codex);
    }
    const grand =
      claude.input_tokens + claude.output_tokens + claude.cache_read_tokens +
      claude.cache_creation_tokens + codex.input_tokens + codex.output_tokens +
      codex.cache_read_tokens + codex.cache_creation_tokens;
    return { claude, codex, grand };
  });

  function money(v: number | null): string {
    if (v == null) return "—";
    return v >= 100
      ? `$${Math.round(v).toLocaleString("en-US")}`
      : `$${v.toFixed(2)}`;
  }
</script>

<div class="stats">
  <header>
    <button class="back" onclick={onclose} title="Back to agents">‹</button>
    <h2>Usage stats</h2>
    <span class="machines">{reports.map((r) => r.machine).join(" + ")}</span>
  </header>

  <div class="chips" role="tablist" aria-label="Time period">
    {#each PERIODS as [key, label] (key)}
      <button
        class="chip"
        class:active={period === key}
        role="tab"
        aria-selected={period === key}
        onclick={() => (period = key)}>{label}</button
      >
    {/each}
  </div>

  {#if loading}
    <p class="note">loading…</p>
  {:else if failed}
    <p class="note">Couldn't read stats from the hub.</p>
  {:else if reports.length === 0}
    <p class="note">No stats yet — collectors report within a minute of startup.</p>
  {:else}
    <div class="tiles">
      <div class="tile accent">
        <span class="label">Est. cost <small>(API-equivalent)</small></span>
        <span class="value">{money(totals.claude.est_cost_usd)}</span>
      </div>
      <div class="tile">
        <span class="label">Total tokens</span>
        <span class="value">{compact(totals.grand)}</span>
      </div>
      <div class="tile">
        <span class="label">Messages</span>
        <span class="value"
          >{(totals.claude.messages + totals.codex.messages).toLocaleString(
            "en-US",
          )}</span
        >
      </div>
      <div class="tile">
        <span class="label">Sessions</span>
        <span class="value">{totals.claude.sessions + totals.codex.sessions}</span>
      </div>
    </div>

    {#each [["claude", totals.claude], ["codex", totals.codex]] as [name, t] (name)}
      {#if (t as PeriodTotals).messages > 0}
        {@const src = t as PeriodTotals}
        <h3>{name}</h3>
        <div class="tiles small">
          <div class="tile">
            <span class="label">Input</span>
            <span class="value">{compact(src.input_tokens)}</span>
          </div>
          <div class="tile">
            <span class="label">Output</span>
            <span class="value">{compact(src.output_tokens)}</span>
          </div>
          <div class="tile">
            <span class="label">Cache read</span>
            <span class="value">{compact(src.cache_read_tokens)}</span>
          </div>
          <div class="tile">
            <span class="label">{name === "claude" ? "Cache write" : "Sessions"}</span>
            <span class="value"
              >{name === "claude"
                ? compact(src.cache_creation_tokens)
                : src.sessions}</span
            >
          </div>
        </div>
      {/if}
    {/each}

    <p class="note">
      Cost is what these tokens would bill at Claude API list prices — an
      estimate of value used, not what you pay on your plan. Codex tokens are
      counted but not priced.
    </p>
  {/if}
</div>

<style>
  .stats {
    flex: 1;
    overflow-y: auto;
    padding: 10px 14px 14px;
    display: flex;
    flex-direction: column;
    gap: 10px;
    scrollbar-width: thin;
    scrollbar-color: rgba(255, 255, 255, 0.15) transparent;
  }

  header {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  h2 {
    margin: 0;
    font-size: 12.5px;
    font-weight: 600;
    color: #dee1e6;
  }

  .machines {
    margin-left: auto;
    font-size: 10px;
    color: #565b64;
  }

  .back {
    font: inherit;
    font-size: 16px;
    line-height: 1;
    color: #8a8f98;
    background: rgba(255, 255, 255, 0.06);
    border: none;
    border-radius: 6px;
    padding: 2px 8px;
    cursor: pointer;
  }

  .back:hover {
    color: #dee1e6;
  }

  .chips {
    display: flex;
    gap: 5px;
  }

  .chip {
    font: inherit;
    font-size: 11px;
    color: #8a8f98;
    background: rgba(255, 255, 255, 0.05);
    border: 1px solid rgba(255, 255, 255, 0.07);
    border-radius: 12px;
    padding: 3px 10px;
    cursor: pointer;
  }

  .chip.active {
    color: #101114;
    background: #dee1e6;
    border-color: #dee1e6;
    font-weight: 600;
  }

  .tiles {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 6px;
  }

  .tile {
    display: flex;
    flex-direction: column;
    gap: 3px;
    padding: 9px 11px;
    border-radius: 10px;
    background: rgba(255, 255, 255, 0.035);
    border: 1px solid rgba(255, 255, 255, 0.05);
  }

  .tile.accent {
    background: rgba(74, 222, 128, 0.08);
    border-color: rgba(74, 222, 128, 0.18);
  }

  .tile.accent .value {
    color: #4ade80;
  }

  .label {
    font-size: 9.5px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: #565b64;
  }

  .label small {
    text-transform: none;
    letter-spacing: 0;
    font-weight: 500;
  }

  .value {
    font-size: 16px;
    font-weight: 600;
    font-variant-numeric: tabular-nums;
    color: #dee1e6;
  }

  .tiles.small .value {
    font-size: 13px;
  }

  h3 {
    margin: 2px 0 0;
    font-size: 10.5px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: #565b64;
  }

  .note {
    margin: 0;
    font-size: 10px;
    line-height: 1.45;
    color: #565b64;
  }
</style>
