<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import type { AgentSnapshot } from "./hub.svelte";
  import { ago, compact, projectName } from "./format";
  import Gauge from "./Gauge.svelte";

  let { session, now }: { session: AgentSnapshot; now: number } = $props();

  // Click-to-jump (TWI-18): both actions derive from the session's cwd;
  // the Rust side translates WSL paths to UNC / vscode-remote forms.
  function jump(app: "code" | "terminal") {
    if (session.jump?.type !== "terminal") return;
    invoke("jump", {
      app,
      machine: session.machine,
      dir: session.jump.target,
    }).catch((err) => console.warn("jump failed:", err));
  }

  // Clicking the card tries to focus whichever window hosts this agent —
  // matched by project name in the window title (IDE windows and terminal
  // tabs carry it). No match: quietly do nothing; the hover buttons can
  // still open a fresh editor/terminal.
  function focusAgent() {
    invoke<boolean>("focus_agent", {
      query: projectName(session.project),
    }).catch((err) => console.warn("focus failed:", err));
  }

  const STATUS_LABEL: Record<string, string> = {
    thinking: "thinking",
    tool_running: "running",
    needs_you: "needs you",
    done: "done · waiting",
    failed: "failed",
    idle: "idle",
    stale: "stale",
  };
</script>

<article
  class="card"
  class:attention={session.needs_user}
  role="button"
  tabindex="0"
  onclick={focusAgent}
  onkeydown={(e) => {
    if (e.key === "Enter" || e.key === " ") focusAgent();
  }}
  title="Click to focus this agent's window"
>
  <header>
    <span class="status {session.status}">
      <span class="status-dot"></span>{STATUS_LABEL[session.status] ?? session.status}
    </span>
    <span class="project" title={session.project}>{projectName(session.project)}</span>
    {#if session.git_branch}
      <span class="branch" title="git branch">{session.git_branch}</span>
    {/if}
    <span class="source">{session.source === "claude-code" ? "claude" : session.source}</span>
    {#if session.jump?.type === "terminal"}
      <span class="jumps">
        <button
          class="jump"
          title="Open in VS Code"
          onclick={(e) => {
            e.stopPropagation();
            jump("code");
          }}>‹›</button
        >
        <button
          class="jump"
          title="Open terminal here"
          onclick={(e) => {
            e.stopPropagation();
            jump("terminal");
          }}>❯_</button
        >
      </span>
    {/if}
  </header>

  {#if session.current_task}
    <p class="task" title={session.current_task}>{session.current_task}</p>
  {:else if session.needs_user_reason}
    <p class="task attention-text">{session.needs_user_reason}</p>
  {/if}

  <footer>
    {#if session.usage.context_pct != null}
      <Gauge pct={session.usage.context_pct} />
    {:else}
      <span class="no-gauge">no context reading</span>
    {/if}
    <span class="tokens" title="input · output · cache read">
      {compact(session.usage.input_tokens)} in · {compact(session.usage.output_tokens)} out
      · {compact(session.usage.cache_read_tokens)} cached
    </span>
    <span class="when">{ago(session.last_activity, now)}</span>
  </footer>
</article>

<style>
  .card {
    padding: 9px 12px;
    border-radius: 10px;
    background: rgba(255, 255, 255, 0.035);
    border: 1px solid rgba(255, 255, 255, 0.05);
    display: flex;
    flex-direction: column;
    gap: 6px;
    cursor: pointer;
  }

  .card:hover {
    background: rgba(255, 255, 255, 0.055);
  }

  .card.attention {
    border-color: rgba(245, 166, 35, 0.4);
  }

  header {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }

  .status {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    font-size: 10.5px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    flex: none;
  }

  .status-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: currentColor;
  }

  .status.thinking,
  .status.tool_running {
    color: #4ade80;
  }

  .status.needs_you {
    color: #f5a623;
  }

  .status.failed {
    color: #f0442c;
  }

  /* Done pops (it's the "your turn" signal), unlike idle/stale gray. */
  .status.done {
    color: #7cb0fa;
  }

  .status.idle,
  .status.stale {
    color: #565b64;
  }

  .project {
    font-weight: 600;
    font-size: 12.5px;
    color: #dee1e6;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .branch {
    font-size: 10.5px;
    color: #8a8f98;
    background: rgba(255, 255, 255, 0.06);
    padding: 2px 7px;
    border-radius: 8px;
    flex: none;
    max-width: 110px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .source {
    margin-left: auto;
    font-size: 10.5px;
    color: #565b64;
    flex: none;
  }

  .jumps {
    display: inline-flex;
    gap: 4px;
    flex: none;
    opacity: 0;
    transition: opacity 0.12s ease;
  }

  .card:hover .jumps {
    opacity: 1;
  }

  .jump {
    font: inherit;
    font-size: 10px;
    line-height: 1;
    color: #8a8f98;
    background: rgba(255, 255, 255, 0.06);
    border: none;
    border-radius: 6px;
    padding: 3px 6px;
    cursor: pointer;
  }

  .jump:hover {
    color: #dee1e6;
    background: rgba(255, 255, 255, 0.12);
  }

  .task {
    margin: 0;
    font-size: 11.5px;
    color: #8a8f98;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .attention-text {
    color: #ffc46b;
  }

  footer {
    display: flex;
    align-items: center;
    gap: 10px;
  }

  footer :global(.gauge) {
    flex: 1;
    min-width: 70px;
  }

  .no-gauge {
    flex: 1;
    font-size: 10.5px;
    color: #565b64;
  }

  .tokens {
    font-size: 10.5px;
    color: #8a8f98;
    font-variant-numeric: tabular-nums;
    flex: none;
  }

  .when {
    font-size: 10.5px;
    color: #565b64;
    flex: none;
    min-width: 48px;
    text-align: right;
  }
</style>
