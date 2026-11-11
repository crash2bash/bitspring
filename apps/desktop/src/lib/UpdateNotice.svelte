<script lang="ts">
  import { isAndroid } from "./api";
  import { num, t } from "./i18n.svelte";
  import { installUpdate, plainNotes, updates } from "./updates.svelte";

  let { playing }: { playing: boolean } = $props();

  let notesOpen = $state(false);
  let show = $derived(!!updates.update && updates.dismissed !== updates.update.version);
  let notes = $derived(plainNotes(updates.update?.notes ?? null));
</script>

{#if show && updates.update}
  <aside class="notice" aria-live="polite" aria-label={t("upd.title")}>
    <p class="head">
      <span class="dot" aria-hidden="true"></span>
      {t("upd.available", { version: updates.update.version })}
    </p>
    <p class="sub">{t("upd.current", { version: updates.current })}</p>

    {#if notes.length}
      <button class="link" onclick={() => (notesOpen = !notesOpen)} aria-expanded={notesOpen}>{t("upd.whatsNew")}</button>
      {#if notesOpen}
        <ul class="notes">
          {#each notes as line, i (i)}<li>{line}</li>{/each}
        </ul>
      {/if}
    {/if}

    {#if updates.installing}
      <div class="bar" role="progressbar" aria-label={t("upd.downloading")} aria-valuemin="0" aria-valuemax="100" aria-valuenow={updates.progress === null ? undefined : Math.round(updates.progress * 100)}>
        <i style={`width:${(updates.progress ?? 0.04) * 100}%`} class:unknown={updates.progress === null}></i>
      </div>
      <p class="sub">
        {updates.progress === null ? t("upd.downloading") : t("upd.downloadingPct", { percent: num(Math.round(updates.progress * 100)) })}
      </p>
    {:else}
      {#if playing && !isAndroid}<p class="sub warn">{t("upd.restartNote")}</p>{/if}
      {#if isAndroid}<p class="sub">{t("upd.androidNote")}</p>{/if}
      <div class="row">
        <button class="primary" onclick={installUpdate}>{t(isAndroid ? "upd.installAndroid" : "upd.install")}</button>
        <button class="ghost" onclick={() => (updates.dismissed = updates.update!.version)}>{t("upd.later")}</button>
      </div>
    {/if}
    {#if updates.error}<p class="err" role="alert">{updates.error}</p>{/if}
  </aside>
{/if}

<style>
  .notice {
    position: fixed;
    inset-inline-end: 18px;
    bottom: 86px;
    z-index: 40;
    width: 340px;
    padding: 14px 16px;
    border-radius: 12px;
    background: var(--slate-700);
    border: 1px solid var(--slate-500);
    box-shadow: 0 18px 40px rgba(5, 12, 15, 0.55);
    font-size: var(--t-sm);
  }
  p {
    margin: 0;
  }
  .head {
    display: flex;
    align-items: center;
    gap: 8px;
    font-weight: 600;
  }
  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--brass);
    box-shadow: 0 0 8px var(--brass-glow);
    flex: none;
  }
  .sub {
    margin-top: 4px;
    font-size: var(--t-xs);
    color: var(--mist);
  }
  .warn {
    color: var(--porcelain);
  }
  .link {
    margin-top: 8px;
    font-size: var(--t-xs);
    color: var(--mist);
    text-decoration: underline;
    text-decoration-color: var(--slate-500);
    text-underline-offset: 3px;
  }
  .link:hover {
    color: var(--porcelain);
  }
  .notes {
    margin: 8px 0 0;
    padding: 0;
    list-style: none;
    max-height: 160px;
    overflow-y: auto;
    font-size: var(--t-xs);
    line-height: 1.5;
    color: var(--mist);
    user-select: text;
    -webkit-user-select: text;
  }
  .notes li + li {
    margin-top: 4px;
  }
  .row {
    display: flex;
    gap: 8px;
    margin-top: 12px;
  }
  .primary,
  .ghost {
    white-space: nowrap;
  }
  .primary {
    padding: 7px 14px;
    border-radius: var(--r-md);
    background: var(--brass);
    color: var(--slate-900);
    font-weight: 600;
  }
  .ghost {
    padding: 7px 14px;
    border-radius: var(--r-md);
    border: 1px solid var(--slate-500);
  }
  .ghost:hover {
    border-color: var(--mist);
  }
  .bar {
    margin-top: 12px;
    height: 4px;
    border-radius: 2px;
    background: var(--slate-900);
    overflow: hidden;
  }
  .bar i {
    display: block;
    height: 100%;
    background: var(--brass);
    transition: width 200ms linear;
  }
  .bar i.unknown {
    width: 30% !important;
    animation: slide 1.2s ease-in-out infinite;
  }
  @keyframes slide {
    from {
      transform: translateX(-100%);
    }
    to {
      transform: translateX(340%);
    }
  }
  .err {
    margin-top: 8px;
    font-size: var(--t-xs);
    color: var(--alarm);
  }
  @media (prefers-reduced-motion: reduce) {
    .bar i,
    .bar i.unknown {
      transition: none;
      animation: none;
    }
  }
  @media (max-width: 700px) {
    .notice {
      left: 10px;
      right: 10px;
      width: auto;
      bottom: calc(env(safe-area-inset-bottom) + 132px);
    }
  }
</style>
