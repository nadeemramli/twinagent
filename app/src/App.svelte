<script lang="ts">
  // Placeholder pill — real data arrives over the hub WebSocket (TWI-13).
  const active = 0;
  const needsYou = 0;
</script>

<main class="pill" data-tauri-drag-region>
  <span class="dot" class:alert={needsYou > 0}></span>
  <span class="label">{active} agents</span>
  {#if needsYou > 0}
    <span class="needs-you">{needsYou} need you</span>
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
    gap: 8px;
    height: 36px;
    margin: 10px auto 0;
    width: fit-content;
    padding: 0 16px;
    border-radius: 18px;
    background: rgba(18, 18, 20, 0.92);
    color: #e8e8ea;
    font:
      500 13px/1 system-ui,
      "Segoe UI",
      sans-serif;
    box-shadow: 0 4px 18px rgba(0, 0, 0, 0.35);
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: #3ecf6e;
  }

  .dot.alert {
    background: #ff9f43;
  }

  .needs-you {
    color: #ff9f43;
  }
</style>
