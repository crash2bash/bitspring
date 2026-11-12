<script lang="ts">
  import { i18n, LOCALES, t } from "./i18n.svelte";
  import { checkUpdates, updates } from "./updates.svelte";
  import { api, isAndroid, isIOS, type RemoteStatus } from "./api";
  import { look, LOOKS } from "./look.svelte";
  import { liquidPill } from "./liquid";

  let open = $state(false);
  let root: HTMLElement;

  // The phone remote: computers only (a phone is the remote).
  const hasRemote = !isIOS && !isAndroid;
  let remote = $state<RemoteStatus | null>(null);
  $effect(() => {
    if (open && hasRemote) api.remoteStatus().then((r) => (remote = r)).catch(() => {});
  });
  async function setRemote(on: boolean) {
    remote = await api.remoteEnable(on);
  }
  async function newAddress() {
    remote = await api.remoteNewAddress();
  }

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
      <div class="field look">
        <span id="look-label">{t("settings.look")}</span>
        <div class="seg" role="radiogroup" aria-labelledby="look-label" use:liquidPill={look.value}>
          {#each LOOKS as l (l)}
            <button role="radio" aria-checked={look.value === l} class:sel={look.value === l} onclick={() => (look.value = l)}>
              {t(l === "glass" ? "settings.lookGlass" : "settings.lookClassic")}
            </button>
          {/each}
        </div>
      </div>
      {#if hasRemote && remote}
        <div class="field upd remote">
          <label class="toggle">
            <input type="checkbox" checked={remote.enabled} onchange={(e) => setRemote(e.currentTarget.checked)} />
            <span>{t("remote.setting")}</span>
          </label>
          <p class="note">{t("remote.help")}</p>
          {#if remote.enabled && remote.url}
            <!-- Our own SVG, made by the app from the address. -->
            <div class="qr" aria-hidden="true">{@html remote.qr}</div>
            <p class="url">{remote.url}</p>
            <button class="check" onclick={newAddress}>{t("remote.newAddress")}</button>
            <p class="note">{t("remote.newHelp")}</p>
          {/if}
          {#if remote.error}<p class="note err">{t("remote.failed", { error: remote.error })}</p>{/if}
        </div>
      {/if}
      {#if updates.enabled}
        <div class="field upd">
          <span>{t("upd.title")}</span>
          <p class="ver">{t("upd.version", { version: updates.current })}</p>
          {#if updates.update}
            <p class="note new">{t("upd.available", { version: updates.update.version })}</p>
            {#if updates.dismissed === updates.update.version}
              <button class="check" onclick={() => (updates.dismissed = null)}>{t("upd.show")}</button>
            {/if}
          {:else}
            <button class="check" onclick={() => checkUpdates(true)} disabled={updates.checking}>
              {t(updates.checking ? "upd.checking" : "upd.check")}
            </button>
            {#if updates.upToDate}<p class="note">{t("upd.upToDate")}</p>{/if}
          {/if}
          {#if updates.error && !updates.update}<p class="note err">{updates.error}</p>{/if}
        </div>
      {/if}
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
    border-radius: var(--r-lg);
    padding: 14px 16px 16px;
    box-shadow: 0 18px 40px rgba(5, 12, 15, 0.55);
    /* with the phone remote's code shown it can be taller than the window: scroll inside it */
    max-height: calc(100vh - 72px);
    overflow-y: auto;
    overscroll-behavior: contain;
    scrollbar-gutter: stable both-edges;
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: 8px;
    font-weight: 600;
  }
  .look {
    margin-top: 14px;
  }
  .seg {
    display: flex;
    padding: 3px;
    gap: 2px;
    border-radius: var(--r-md);
    border: 1px solid var(--slate-600);
    font-weight: 500;
    font-size: var(--t-sm);
  }
  .seg button {
    flex: 1;
    padding: 5px 8px;
    border-radius: var(--r-md);
    color: var(--mist);
  }
  .seg button:hover {
    color: var(--porcelain);
  }
  .seg button.sel {
    color: var(--porcelain);
    background: var(--slate-600);
  }
  :global(html[data-look="glass"]) .seg,
  :global(html[data-look="glass"]) .seg button {
    border-radius: 999px;
  }
  .upd {
    margin-top: 16px;
    padding-top: 14px;
    border-top: 1px solid var(--slate-600);
  }
  .ver,
  .note {
    margin: 0;
    font-weight: 400;
    font-size: var(--t-sm);
    color: var(--mist);
  }
  .note.new {
    color: var(--porcelain);
  }
  .note.err {
    color: var(--alarm);
  }
  .check {
    align-self: flex-start;
    padding: 6px 12px;
    border-radius: var(--r-md);
    border: 1px solid var(--slate-500);
    font-weight: 500;
    font-size: var(--t-sm);
  }
  .check:hover:not(:disabled) {
    border-color: var(--mist);
  }
  .check:disabled {
    color: var(--mist);
  }
  .toggle {
    display: flex;
    align-items: center;
    gap: 8px;
    cursor: pointer;
  }
  .toggle input {
    accent-color: var(--porcelain);
  }
  .qr {
    align-self: center;
    width: 180px;
    padding: 8px;
    border-radius: var(--r-md);
    background: #fff;
    line-height: 0;
  }
  .qr :global(svg) {
    width: 100%;
    height: auto;
  }
  .url {
    margin: 0;
    font-weight: 400;
    font-size: var(--t-xs);
    color: var(--porcelain);
    user-select: all;
    word-break: break-all;
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
  @media (max-width: 700px), (max-height: 500px) {
    .trigger {
      width: 26px;
      border-color: transparent;
    }
    .panel {
      position: fixed;
      left: 10px;
      right: 10px;
      top: calc(var(--safe-top, env(safe-area-inset-top)) + 58px);
      width: auto;
      max-height: calc(100dvh - var(--safe-top, env(safe-area-inset-top)) - var(--safe-bottom, env(safe-area-inset-bottom)) - 68px);
    }
  }
</style>
