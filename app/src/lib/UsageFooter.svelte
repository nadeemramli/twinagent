<script lang="ts">
  import type { UsageReport } from "./hub.svelte";
  import { compact, resetsIn } from "./format";

  // Plan usage per machine. Codex numbers are exact percentages and get
  // bars; the Claude estimate has no known cap, so it shows token counts —
  // never dressed up as a percentage it isn't (TWI-16/17 confidence rule).
  let { reports, now }: { reports: UsageReport[]; now: number } = $props();

  function barTone(pct: number): string {
    if (pct > 90) return "critical";
    if (pct > 70) return "high";
    if (pct > 50) return "elevated";
    return "ok";
  }
</script>

{#if reports.length > 0}
  <section class="usage">
    <h3>Plan usage</h3>
    {#each reports as report (report.machine)}
      {#if report.codex?.primary || report.claude}
        <div class="machine">
          <span class="machine-tag">{report.machine}</span>
          {#if report.codex?.primary}
            {@const p = report.codex.primary}
            {@const s = report.codex.secondary}
            <div class="row">
              <span class="who">
                codex{#if report.codex.plan_type}&nbsp;· {report.codex.plan_type}{/if}
                <span class="confidence exact">exact</span>
              </span>
              <div class="windows">
                <div class="window">
                  <span class="win-label">5h</span>
                  <span class="bar"><span class="bar-fill {barTone(p.used_percent)}" style:width="{Math.min(100, p.used_percent)}%"></span></span>
                  <span class="pct">{Math.round(p.used_percent)}%</span>
                  {#if p.resets_at}<span class="resets">{resetsIn(p.resets_at * 1000, now)}</span>{/if}
                </div>
                {#if s}
                  <div class="window">
                    <span class="win-label">week</span>
                    <span class="bar"><span class="bar-fill {barTone(s.used_percent)}" style:width="{Math.min(100, s.used_percent)}%"></span></span>
                    <span class="pct">{Math.round(s.used_percent)}%</span>
                    {#if s.resets_at}<span class="resets">{resetsIn(s.resets_at * 1000, now)}</span>{/if}
                  </div>
                {/if}
              </div>
            </div>
          {/if}
          {#if report.claude}
            {@const c = report.claude}
            <div class="row">
              <span class="who">
                claude
                <span class="confidence estimated">estimated</span>
              </span>
              <div class="windows">
                <div class="window">
                  <span class="win-label">5h</span>
                  <span class="counts">
                    {compact(c.five_hour.tokens.output_tokens)} out · {c.five_hour.messages} msgs
                  </span>
                  {#if c.five_hour_resets_at}
                    <span class="resets">{resetsIn(Date.parse(c.five_hour_resets_at), now)}</span>
                  {/if}
                </div>
                <div class="window">
                  <span class="win-label">week</span>
                  <span class="counts">
                    {compact(c.weekly.tokens.output_tokens)} out · {c.weekly.messages} msgs
                  </span>
                </div>
              </div>
            </div>
          {/if}
        </div>
      {/if}
    {/each}
  </section>
{/if}

<style>
  .usage {
    padding: 10px 12px 12px;
    border-top: 1px solid rgba(255, 255, 255, 0.06);
  }

  h3 {
    margin: 0 0 8px;
    font-size: 10.5px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: #565b64;
  }

  .machine {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-bottom: 8px;
  }

  .machine:last-child {
    margin-bottom: 0;
  }

  .machine-tag {
    font-size: 10.5px;
    color: #8a8f98;
    font-weight: 600;
  }

  .row {
    display: flex;
    gap: 10px;
    align-items: flex-start;
  }

  .who {
    flex: none;
    width: 108px;
    font-size: 11.5px;
    color: #dee1e6;
    display: inline-flex;
    align-items: center;
    gap: 5px;
  }

  .confidence {
    font-size: 9px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    padding: 1.5px 5px;
    border-radius: 6px;
  }

  /* The reading's provenance is always visible: exact vs estimated. */
  .confidence.exact {
    color: #4ade80;
    background: rgba(74, 222, 128, 0.12);
  }

  .confidence.estimated {
    color: #8a8f98;
    background: rgba(255, 255, 255, 0.07);
  }

  .windows {
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 4px;
    min-width: 0;
  }

  .window {
    display: flex;
    align-items: center;
    gap: 7px;
  }

  .win-label {
    flex: none;
    width: 30px;
    font-size: 10.5px;
    color: #565b64;
  }

  .bar {
    flex: 1;
    height: 4px;
    border-radius: 2px;
    background: rgba(255, 255, 255, 0.08);
    overflow: hidden;
  }

  .bar-fill {
    display: block;
    height: 100%;
    border-radius: 2px;
  }

  .bar-fill.ok {
    background: #4ade80;
  }

  .bar-fill.elevated {
    background: #facc15;
  }

  .bar-fill.high {
    background: #fb923c;
  }

  .bar-fill.critical {
    background: #f0442c;
  }

  .pct {
    flex: none;
    width: 34px;
    text-align: right;
    font-size: 10.5px;
    font-variant-numeric: tabular-nums;
    color: #dee1e6;
  }

  .counts {
    flex: 1;
    font-size: 10.5px;
    color: #8a8f98;
    font-variant-numeric: tabular-nums;
  }

  .resets {
    flex: none;
    font-size: 10px;
    color: #565b64;
  }
</style>
