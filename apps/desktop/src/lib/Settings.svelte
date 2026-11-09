<script lang="ts">
  import { i18n, LOCALES, t } from "./i18n.svelte";

  let open = $state(false);
  let root: HTMLElement;

  function onWindowClick(e: MouseEvent) {
    if (open && root && !e.composedPath().includes(root)) open = false;
  }
</script>

<svelte:window onclick={onWindowClick} onkeydown={(e) => e.key === "Escape" && (open = false)} />

<div class="settings" bind:this={root}>
  <button class="trigger" onclick={() => (open = !open)} aria-expanded={open} aria-haspopup="dialog" aria-label={t("settings.label")} title={t("settings.label")}>
    <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
      <path
        d="M6.9 1.5h2.2l.4 1.8 1.2.5 1.6-1 1.5 1.5-1 1.6.5 1.2 1.8.4v2.2l-1.8.4-.5 1.2 1 1.6-1.5 1.5-1.6-1-1.2.5-.4 1.8H6.9l-.4-1.8-1.2-.5-1.6 1-1.5-1.5 1-1.6-.5-1.2-1.8-.4V6.9l1.8-.4.5-1.2-1-1.6 1.5-1.5 1.6 1 1.2-.5z"
        fill="none"
        stroke="currentColor"
        stroke-width="1.3"
        stroke-linejoin="round"
      />
      <circle cx="8" cy="8" r="2.2" fill="none" stroke="currentColor" stroke-width="1.3" />
    </svg>
  </button>

  {#if open}
    <div class="panel" role="dialog" aria-label={t("settings.label")}>
      <label class="field">
        <span>{t("settings.language")}</span>
        <select value={i18n.pref} onchange={(e) => (i18n.pref = e.currentTarget.value)}>
          <option value="auto">{t("settings.system", { name: i18n.system.name })}</option>
          {#each LOCALES as l (l.code)}
            <option value={l.code} lang={l.tag} dir={l.dir}>{l.name}</option>
          {/each}
        </select>
      </label>
    </div>
  {/if}
</div>

<style>
  .settings {
    position: relative;
  }
  .trigger {
    display: grid;
    place-items: center;
    width: 32px;
    height: 32px;
    border-radius: var(--r-md);
    border: 1px solid var(--slate-600);
    color: var(--mist);
  }
  .trigger:hover,
  .trigger[aria-expanded="true"] {
    color: var(--porcelain);
    border-color: var(--slate-500);
  }
  .panel {
    position: absolute;
    inset-inline-end: 0;
    top: calc(100% + 8px);
    width: 260px;
    z-index: 20;
    background: var(--slate-700);
    border: 1px solid var(--slate-500);
    border-radius: 12px;
    padding: 14px 16px 16px;
    box-shadow: 0 18px 40px rgba(5, 12, 15, 0.55);
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: 8px;
    font-weight: 600;
  }
  select {
    font: inherit;
    font-weight: 400;
    font-size: var(--t-sm);
    color: var(--porcelain);
    background: var(--slate-800);
    border: 1px solid var(--slate-500);
    border-radius: var(--r-md);
    padding: 7px 10px;
  }

  /* phone: a bare icon (the header is tight), and the panel spans the screen under it */
  @media (max-width: 700px) {
    .trigger {
      width: 26px;
      border-color: transparent;
    }
    .panel {
      position: fixed;
      left: 10px;
      right: 10px;
      top: calc(env(safe-area-inset-top) + 58px);
      width: auto;
    }
  }
</style>
