<script lang="ts">
  import { t } from "./i18n.svelte";

  // An offer, never more: the picked Bluetooth output looks like headphones we have correction
  // profiles for, and its EQ was never set up. Nothing is switched on from here; "Choose" only
  // opens the headphone picker with the model typed in (see `eqHint` in +page.svelte).
  let {
    model,
    onpick,
    ondismiss,
  }: {
    /** The model as the profiles write it ("AirPods Pro", "WH-1000XM4"). */
    model: string;
    onpick: () => void;
    /** Not to be offered again for this output. */
    ondismiss: () => void;
  } = $props();
</script>

<aside class="notice" aria-live="polite" aria-label={t("eq.hpTitle")}>
  <p class="head">
    <span class="dot" aria-hidden="true"></span>
    <span>{t("eqHint.text", { model })}</span>
  </p>
  <div class="row">
    <button class="primary" onclick={onpick}>{t("eqHint.pick")}</button>
    <button class="ghost" onclick={ondismiss}>{t("eqHint.dismiss")}</button>
  </div>
</aside>

<style>
  /* The update notice's place and look (UpdateNotice.svelte): the page shows only one of the two
     at a time, so they never cover each other. */
  .notice {
    position: fixed;
    inset-inline-end: 18px;
    bottom: 86px;
    z-index: 40;
    width: 340px;
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
    align-items: baseline;
    gap: 8px;
    font-weight: 600;
    line-height: 1.4;
  }
  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--porcelain);
    box-shadow: 0 0 8px rgba(232, 237, 234, 0.35);
    flex: none;
    /* On the first line's middle, as beside the update notice's one-line head. */
    align-self: flex-start;
    margin-top: calc(0.7em - 3.5px);
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
      bottom: calc(var(--safe-bottom, env(safe-area-inset-bottom)) + 132px);
    }
  }
</style>
