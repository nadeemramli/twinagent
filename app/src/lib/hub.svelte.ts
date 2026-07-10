// Live connection to the embedded hub: one Full snapshot on connect, then
// diffs. Cards are keyed by logical_key so cross-side duplicates collapse
// (TWI-10). Reconnects forever with capped backoff — the pill must survive
// hub restarts without user attention.

export type AgentStatus =
  | "thinking"
  | "tool_running"
  | "needs_you"
  | "done"
  | "failed"
  | "idle"
  | "stale";

export interface UsageMetrics {
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_creation_tokens: number;
  context_pct: number | null;
}

export interface AgentSnapshot {
  agent_id: string;
  source: string;
  machine: string;
  project: string;
  status: AgentStatus;
  git_branch: string | null;
  current_task: string | null;
  needs_user: boolean;
  needs_user_reason: string | null;
  usage: UsageMetrics;
  last_activity: string;
  jump: { type: string; target: string } | null;
}

export interface TokenCounts {
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_creation_tokens: number;
}

export interface WindowUsage {
  tokens: TokenCounts;
  messages: number;
}

/// JSONL-derived Claude estimate — always confidence "estimated".
export interface PlanEstimate {
  five_hour: WindowUsage;
  five_hour_resets_at: string | null;
  weekly: WindowUsage;
  confidence: "estimated" | "exact" | "stale";
}

export interface RateLimitWindow {
  used_percent: number;
  window_minutes: number | null;
  resets_at: number | null;
}

/// Exact Codex plan windows straight from rollout rate_limits.
export interface RateLimits {
  primary: RateLimitWindow | null;
  secondary: RateLimitWindow | null;
  plan_type: string | null;
  observed_at: string | null;
}

/// One exact plan window from the OAuth usage endpoint.
export interface PlanWindow {
  used_percent: number;
  resets_at: string | null;
}

/// Exact account-level Claude plan usage — server-side percentages and
/// real reset times, same data as Claude Code's /usage command.
export interface ClaudePlanWindows {
  five_hour: PlanWindow;
  seven_day: PlanWindow;
  seven_day_opus: PlanWindow | null;
  seven_day_sonnet: PlanWindow | null;
  observed_at: string;
}

export interface UsageReport {
  machine: string;
  claude: PlanEstimate | null;
  claude_exact: ClaudePlanWindows | null;
  codex: RateLimits | null;
  reported_at: string;
}

type HubEvent =
  | { type: "full"; sessions: AgentSnapshot[] }
  | {
      type: "upsert";
      key: string;
      logical_key: string;
      session: AgentSnapshot;
      entered_needs_you: boolean;
    }
  | { type: "removed"; key: string }
  | { type: "usage"; report: UsageReport };

function logicalKey(s: AgentSnapshot): string {
  return `${s.source}/${s.agent_id}`;
}

class HubState {
  sessions = $state<Record<string, AgentSnapshot>>({});
  usage = $state<Record<string, UsageReport>>({});
  connected = $state(false);

  apply(event: HubEvent) {
    switch (event.type) {
      case "full": {
        const next: Record<string, AgentSnapshot> = {};
        for (const s of event.sessions) next[logicalKey(s)] = s;
        this.sessions = next;
        break;
      }
      case "upsert":
        this.sessions[event.logical_key] = event.session;
        break;
      case "removed": {
        // Raw registry key is machine/source/agent_id; drop the card only if
        // the removed observation is the one we're showing.
        const [machine, source, ...rest] = event.key.split("/");
        const logical = `${source}/${rest.join("/")}`;
        if (this.sessions[logical]?.machine === machine) {
          delete this.sessions[logical];
        }
        break;
      }
      case "usage":
        this.usage[event.report.machine] = event.report;
        break;
    }
  }
}

export const hub = new HubState();

const HUB_WS = "ws://127.0.0.1:17871/v1/ws";
const RECONNECT_MIN_MS = 500;
const RECONNECT_MAX_MS = 10_000;

export function connectHub(url: string = HUB_WS) {
  let backoff = RECONNECT_MIN_MS;

  const open = () => {
    const ws = new WebSocket(url);
    ws.onopen = () => {
      hub.connected = true;
      backoff = RECONNECT_MIN_MS;
    };
    ws.onmessage = (msg) => {
      try {
        hub.apply(JSON.parse(msg.data) as HubEvent);
      } catch {
        // A malformed frame must never take the widget down.
      }
    };
    ws.onclose = () => {
      hub.connected = false;
      setTimeout(open, backoff);
      backoff = Math.min(backoff * 2, RECONNECT_MAX_MS);
    };
    ws.onerror = () => ws.close();
  };
  open();
}
