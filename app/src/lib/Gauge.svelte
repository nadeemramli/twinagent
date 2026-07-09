<script lang="ts">
  import { contextTone } from "./format";

  // A thin status meter. State is never color-alone: the percent is always
  // printed beside the track (dataviz status rule).
  let { pct, label = "context" }: { pct: number; label?: string } = $props();
  const clamped = $derived(Math.min(100, Math.max(0, pct)));
  const tone = $derived(contextTone(clamped));
</script>

<div class="gauge" role="meter" aria-valuenow={Math.round(clamped)} aria-valuemin={0} aria-valuemax={100} aria-label={label}>
  <span class="track">
    <span class="fill {tone}" style:width="{clamped}%"></span>
  </span>
  <span class="value {tone}">{Math.round(clamped)}%</span>
</div>

<style>
  .gauge {
    display: flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
  }

  .track {
    flex: 1;
    height: 4px;
    border-radius: 2px;
    background: rgba(255, 255, 255, 0.08);
    overflow: hidden;
  }

  .fill {
    display: block;
    height: 100%;
    border-radius: 2px;
    transition: width 300ms ease;
  }

  .fill.ok {
    background: #4ade80;
  }

  .fill.elevated {
    background: #facc15;
  }

  .fill.high {
    background: #fb923c;
  }

  .fill.critical {
    background: #f0442c;
  }

  .value {
    font-size: 10.5px;
    font-variant-numeric: tabular-nums;
    color: #8a8f98;
    flex: none;
    width: 32px;
    text-align: right;
  }

  .value.critical {
    color: #ff8a75;
  }

  @media (prefers-reduced-motion: reduce) {
    .fill {
      transition: none;
    }
  }
</style>
