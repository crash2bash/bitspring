<script lang="ts">
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { api, inTauri } from "./api";
  import { t } from "./i18n.svelte";

  /** Where a report can go, if the user decides to send it. Nothing is put in the address:
   *  they paste the text themselves. */
  const ISSUES = "https://github.com/crash2bash/bitspring/issues/new";

  /** The notice is up: other notices wait (see `EqHint`), so none covers another. */
  let { showing = $bindable(false) }: { showing?: boolean } = $props();

  // Reports from earlier runs, kept on this device (see src-tauri/src/crash.rs).
  let report = $state<string | null>(null);
  $effect(() => {
    showing = !!report;
  });
  let shown = $state(false);
  let copied = $state(false);

  $effect(() => {
    api.crashReport().then(
      (r) => (report = r),
      () => {},
    );
  });

  async function copy() {
    if (!report) return;
    try {
      await navigator.clipboard.writeText(report);
      copied = true;
    } catch {
      // No clipboard access: the text is on screen to select by hand.
      shown = true;
    }
  }

  function github() {
    if (inTauri) openUrl(ISSUES).catch(() => {});
    else window.open(ISSUES, "_blank", "noopener");
  }

  function dismiss() {
    report = null;
    api.crashDismiss().catch(() => {});
  }
</script>

{#if report}
  <aside class="notice" aria-live="polite" aria-label={t("crash.title")}>
    <p class="head">
      <span class="dot" aria-hidden="true"></span>
      {t("crash.title")}
    </p>
    <p class="sub">{t("crash.sub")}</p>

    <button class="link" onclick={() => (shown = !shown)} aria-expanded={shown}>{t(shown ? "crash.hide" : "crash.show")}</button>
    {#if shown}
      <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
      <pre class="report" tabindex="0">{report}</pre>
    {/if}

    <div class="row">
      <button class="primary" onclick={copy}>{t(copied ? "eq.copied" : "crash.copy")}</button>
      <button class="ghost" onclick={github}>{t("crash.github")}</button>
      <button class="ghost" onclick={dismiss}>{t("crash.delete")}</button>
    </div>
  </aside>
{/if}

<style>
  .notice {
    position: fixed;
    inset-inline-end: 18px;
    top: 64px;
    z-index: 40;
    width: 380px;
    max-height: calc(100vh - 90px);
    overflow-y: auto;
    padding: 14px 16px;
    border-radius: var(--r-lg);
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
    background: var(--alarm);
    flex: none;
  }
  .sub {
    margin-top: 6px;
    font-size: var(--t-xs);
    line-height: 1.5;
    color: var(--mist);
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
  .report {
    margin: 8px 0 0;
    padding: 10px 12px;
    max-height: 240px;
    overflow: auto;
    border-radius: var(--r-md);
    background: var(--slate-900);
    font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    font-size: 11px;
    line-height: 1.5;
    color: var(--porcelain);
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    /* A report is in English and reads left to right in every UI language. */
    direction: ltr;
    text-align: left;
    user-select: text;
    -webkit-user-select: text;
  }
  .row {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    margin-top: 12px;
  }
  .primary,
  .ghost {
    white-space: nowrap;
    padding: 7px 14px;
    border-radius: var(--r-md);
  }
  .primary {
    background: var(--porcelain);
    color: var(--ink);
    font-weight: 600;
  }
  .ghost {
    border: 1px solid var(--slate-500);
  }
  .ghost:hover {
    border-color: var(--mist);
  }
  @media (max-width: 700px), (max-height: 500px) {
    .notice {
      left: 10px;
      right: 10px;
      width: auto;
      top: calc(var(--safe-top, env(safe-area-inset-top)) + 58px);
      max-height: calc(100dvh - var(--safe-top, env(safe-area-inset-top)) - var(--safe-bottom, env(safe-area-inset-bottom)) - 80px);
    }
  }
</style>
