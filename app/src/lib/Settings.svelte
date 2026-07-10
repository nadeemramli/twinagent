<script lang="ts">
  // NB: not "./settings.svelte" — on a case-insensitive filesystem that
  // specifier resolves to THIS component instead of the store module.
  import { settings, type Settings } from "./settings-store.svelte";

  let { onclose }: { onclose: () => void } = $props();

  // Edit a draft; the live config only changes on Save.
  let draft = $state<Settings>({ ...settings.current });
  let error = $state<string | null>(null);
  let saved = $state(false);

  async function save() {
    error = null;
    saved = false;
    try {
      await settings.save({
        ...draft,
        hub_port: Math.max(1, Math.min(65535, Math.round(draft.hub_port))),
        thresholds: draft.thresholds.map((t) =>
          Math.max(1, Math.min(100, Math.round(t))),
        ) as [number, number, number],
      });
      saved = true;
    } catch (e) {
      error = String(e);
    }
  }
</script>

<div class="settings">
  <header>
    <button class="back" onclick={onclose} title="Back to agents">‹</button>
    <h2>Settings</h2>
  </header>

  <label class="row">
    <span>Launch at login</span>
    <input type="checkbox" bind:checked={draft.autostart} />
  </label>

  <label class="row">
    <span>Toast when an agent needs you</span>
    <input type="checkbox" bind:checked={draft.toasts} />
  </label>

  <label class="row">
    <span>Keep panel open <small>(until Esc, hotkey, or pill click)</small></span>
    <input type="checkbox" bind:checked={draft.pinned} />
  </label>

  <label class="row">
    <span>Toggle hotkey</span>
    <input class="text" type="text" bind:value={draft.hotkey} spellcheck="false" />
  </label>

  <label class="row">
    <span>Hub port <small>(applies on restart)</small></span>
    <input class="num" type="number" min="1" max="65535" bind:value={draft.hub_port} />
  </label>

  <div class="row thresholds">
    <span>Gauge thresholds <small>(yellow / orange / red %)</small></span>
    <span class="triple">
      <input class="num" type="number" min="1" max="100" bind:value={draft.thresholds[0]} />
      <input class="num" type="number" min="1" max="100" bind:value={draft.thresholds[1]} />
      <input class="num" type="number" min="1" max="100" bind:value={draft.thresholds[2]} />
    </span>
  </div>

  <footer>
    {#if error}<span class="error">{error}</span>{/if}
    {#if saved}<span class="ok">saved</span>{/if}
    <button class="save" onclick={save}>Save</button>
  </footer>
</div>

<style>
  .settings {
    flex: 1;
    overflow-y: auto;
    padding: 10px 14px 14px;
    display: flex;
    flex-direction: column;
    gap: 10px;
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

  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
    font-size: 11.5px;
    color: #dee1e6;
  }

  .row small {
    color: #565b64;
    font-size: 10px;
  }

  input.text,
  input.num {
    font: inherit;
    font-size: 11.5px;
    color: #dee1e6;
    background: rgba(255, 255, 255, 0.06);
    border: 1px solid rgba(255, 255, 255, 0.08);
    border-radius: 6px;
    padding: 4px 8px;
  }

  input.text {
    width: 150px;
  }

  input.num {
    width: 56px;
  }

  .triple {
    display: inline-flex;
    gap: 5px;
  }

  footer {
    margin-top: auto;
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 10px;
  }

  .error {
    font-size: 10.5px;
    color: #ff8a75;
  }

  .ok {
    font-size: 10.5px;
    color: #4ade80;
  }

  .save {
    font: inherit;
    font-size: 11.5px;
    font-weight: 600;
    color: #101114;
    background: #4ade80;
    border: none;
    border-radius: 8px;
    padding: 5px 14px;
    cursor: pointer;
  }

  .save:hover {
    background: #6ae89a;
  }
</style>
