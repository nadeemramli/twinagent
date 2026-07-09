// Small formatting helpers shared by the panel components.

/** 1234 → "1.2k", 3_400_000 → "3.4M" — card counters stay compact. */
export function compact(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}

/** "2m ago" / "3h ago" relative to `now` (ms). */
export function ago(iso: string, now: number): string {
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return "";
  const s = Math.max(0, Math.floor((now - t) / 1000));
  if (s < 10) return "now";
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

/** Countdown to a future instant: "resets in 2h 05m". Empty once passed. */
export function resetsIn(target: number, now: number): string {
  const s = Math.floor((target - now) / 1000);
  if (s <= 0) return "";
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (h > 24) return `resets in ${Math.floor(h / 24)}d ${h % 24}h`;
  if (h > 0) return `resets in ${h}h ${String(m).padStart(2, "0")}m`;
  return `resets in ${m}m`;
}

/** Last path segment, tolerant of both separators. */
export function projectName(path: string): string {
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] ?? path;
}

/** AgentNotch context thresholds: green <50, yellow 50–70, orange 70–90, red >90. */
export function contextTone(pct: number): "ok" | "elevated" | "high" | "critical" {
  if (pct > 90) return "critical";
  if (pct > 70) return "high";
  if (pct > 50) return "elevated";
  return "ok";
}
