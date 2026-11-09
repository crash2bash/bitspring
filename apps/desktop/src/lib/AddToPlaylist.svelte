<script lang="ts">
  import { api, errorText, type PlaylistEntry, type PlaylistSummary } from "./api";
  import { t } from "./i18n.svelte";

  let {
    entries,
    compact = false,
    label = undefined,
  }: {
    /** What to add — called on click, so a long album is only gathered when needed. */
    entries: () => PlaylistEntry[];
    /** A small "+" for table rows instead of a labelled button. */
    compact?: boolean;
    /** Defaults to "Add to playlist" in the UI language. */
    label?: string;
  } = $props();

  let open = $state(false);
  let lists = $state<PlaylistSummary[]>([]);
  let naming = $state(false);
  let name = $state("");
  let done = $state<string | null>(null);
  let failed = $state<string | null>(null);
  let pos = $state({ top: 0, left: 0 });
  let button: HTMLButtonElement;
  let panel = $state<HTMLElement>();

  async function toggle(e: MouseEvent) {
    e.stopPropagation();
    if (open) return close();
    const r = button.getBoundingClientRect();
    // Below the button, right edges aligned; above it when there's no room below.
    const width = 260;
    const below = window.innerHeight - r.bottom > 300;
    pos = { top: below ? r.bottom + 6 : Math.max(8, r.top - 306), left: Math.max(8, Math.min(r.right - width, window.innerWidth - width - 8)) };
    open = true;
    naming = false;
    failed = null;
    lists = await api.playlists();
  }

  function close() {
    open = false;
    naming = false;
  }

  function flash(text: string) {
    done = text;
    setTimeout(() => (done = null), 2200);
  }

  async function addTo(p: PlaylistSummary) {
    try {
      const e = entries();
      await api.playlistAdd(p.id, e);
      close();
      flash(t("pl.added", { name: p.name }));
    } catch (err) {
      failed = errorText(err);
    }
  }

  async function createWith() {
    try {
      const s = await api.playlistCreate(name.trim() || t("pl.defaultName"), entries());
      close();
      name = "";
      flash(t("pl.created", { name: s.name }));
    } catch (err) {
      failed = errorText(err);
    }
  }

  function onWindowClick(e: MouseEvent) {
    if (open && panel && !e.composedPath().includes(panel) && !e.composedPath().includes(button)) close();
  }
</script>

<svelte:window onclick={onWindowClick} onkeydown={(e) => open && e.key === "Escape" && close()} />

<span class="atp">
  <button
    bind:this={button}
    class={compact ? "plus" : "ghost"}
    onclick={toggle}
    aria-expanded={open}
    aria-label={compact ? (label ?? t("pl.add")) : undefined}
    title={compact ? (label ?? t("pl.add")) : undefined}
  >
    {#if compact}
      <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true"><path d="M6 1v10M1 6h10" stroke="currentColor" stroke-width="1.6" /></svg>
    {:else}
      {label ?? t("pl.add")}
    {/if}
  </button>
  {#if done}<span class="done" role="status">{done}</span>{/if}
</span>

{#if open}
  <div class="panel" bind:this={panel} role="dialog" aria-label={t("pl.addDialog")} style={`top:${pos.top}px;left:${pos.left}px`}>
    {#if naming}
      <form onsubmit={(e) => (e.preventDefault(), createWith())}>
        <!-- svelte-ignore a11y_autofocus -->
        <input bind:value={name} placeholder={t("pl.name")} aria-label={t("pl.newName")} autofocus />
        <div class="row">
          <button type="button" class="link" onclick={() => (naming = false)}>{t("lib.back")}</button>
          <button type="submit" class="primary">{t("pl.create")}</button>
        </div>
      </form>
    {:else}
      <button class="new" onclick={() => (naming = true)}>{t("pl.newEllipsis")}</button>
      {#if lists.length}
        <ul>
          {#each lists as p (p.id)}
            <li><button onclick={() => addTo(p)}><span>{p.name}</span><span class="n num">{p.count}</span></button></li>
          {/each}
        </ul>
      {/if}
    {/if}
    {#if failed}<p class="err">{failed}</p>{/if}
  </div>
{/if}

<style>
  .atp {
    position: relative;
    display: inline-flex;
    align-items: center;
  }
  .ghost {
    padding: 8px 16px;
    border-radius: var(--r-md);
    border: 1px solid var(--slate-500);
    font-weight: 600;
  }
  .ghost:hover {
    border-color: var(--mist);
  }
  .plus {
    width: 26px;
    height: 26px;
    display: inline-grid;
    place-items: center;
    border-radius: 6px;
    color: var(--mist);
  }
  .plus:hover {
    color: var(--porcelain);
    background: var(--slate-600);
  }
  .done {
    position: absolute;
    inset-inline-end: 0;
    top: calc(100% + 4px);
    white-space: nowrap;
    font-size: var(--t-xs);
    color: var(--brass);
    background: var(--slate-900);
    padding: 3px 8px;
    border-radius: 4px;
    z-index: 30;
  }
  .panel {
    position: fixed;
    width: 260px;
    max-height: 300px;
    overflow: auto;
    z-index: 50;
    background: var(--slate-700);
    border: 1px solid var(--slate-500);
    border-radius: 10px;
    padding: 6px;
    box-shadow: 0 18px 40px rgba(5, 12, 15, 0.55);
    font-size: var(--t-sm);
    text-align: start;
  }
  ul {
    list-style: none;
    margin: 4px 0 0;
    padding: 4px 0 0;
    border-top: 1px solid var(--slate-600);
  }
  .panel button:not(.primary) {
    width: 100%;
    display: flex;
    justify-content: space-between;
    gap: 10px;
    padding: 7px 10px;
    border-radius: 6px;
    text-align: start;
  }
  .panel li button:hover,
  .new:hover {
    background: var(--slate-600);
  }
  .panel li span:first-child {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .new {
    color: var(--brass);
    font-weight: 600;
  }
  .n {
    color: var(--mist-dim);
  }
  form {
    padding: 6px;
  }
  input {
    width: 100%;
    padding: 7px 10px;
    border-radius: 6px;
    border: 1px solid var(--slate-500);
    background: var(--slate-900);
    color: var(--porcelain);
    font: inherit;
    user-select: text;
    -webkit-user-select: text;
  }
  .row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-top: 8px;
  }
  .row .link {
    width: auto !important;
    color: var(--mist);
  }
  .primary {
    padding: 6px 14px;
    border-radius: var(--r-md);
    background: var(--brass);
    color: var(--slate-900);
    font-weight: 600;
  }
  .err {
    margin: 6px;
    color: var(--alarm);
    font-size: var(--t-xs);
  }
</style>
