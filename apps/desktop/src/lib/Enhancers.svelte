<script lang="ts">
  import type { Crossfeed, DspSettings, IrInfo } from "./api";
  import { list, num, t } from "./i18n.svelte";
  import { liquidPill } from "./liquid";

  let {
    settings,
    eqName,
    ir,
    onchange,
    onopeneq,
  }: {
    settings: DspSettings;
    /** The EQ preset playing on the picked output (null = off). */
    eqName: string | null;
    ir: IrInfo | null;
    onchange: (s: DspSettings) => void;
    onopeneq: () => void;
  } = $props();

  let open = $state(false);
  let root: HTMLElement;

  let active = $derived(
    [
      settings.upsampleKhz && t("enh.on.upsampling"),
      eqName && t("enh.on.eq"),
      settings.convolution && t("enh.on.convolution"),
      settings.crossfeed && t("enh.on.crossfeed"),
    ].filter(Boolean) as string[],
  );

  let lossyOn = $derived(settings.headroom || settings.restoreHighs);

  const UPSAMPLE: (number | null)[] = [null, 192, 384, 768];
  const CROSSFEED: (Crossfeed | null)[] = [null, "light", "medium", "strong"];

  function set(patch: Partial<DspSettings>) {
    onchange({ ...settings, ...patch });
  }

  // composedPath is captured at dispatch, so a click on a button that the click itself
  // removes from the DOM (e.g. "load profile" → replaced by the curve) still counts as inside.
  function onWindowClick(e: MouseEvent) {
    if (open && root && !e.composedPath().includes(root)) open = false;
  }
</script>

<svelte:window onclick={onWindowClick} onkeydown={(e) => e.key === "Escape" && (open = false)} />

<div class="enh" bind:this={root}>
  <button class="trigger" class:on={active.length > 0} onclick={() => (open = !open)} aria-expanded={open} aria-haspopup="dialog">
    <span>{t("enh.button")}</span>
    <span class="state">{active.length ? list(active) : t(lossyOn ? "enh.lossyOnly" : "enh.off")}</span>
  </button>

  {#if open}
    <div class="panel" role="dialog" aria-label={t("enh.dialog")}>
      <fieldset>
        <legend>{t("enh.upsampling")}</legend>
        <div class="seg" role="radiogroup" aria-label={t("enh.upsampling")} use:liquidPill={settings.upsampleKhz}>
          {#each UPSAMPLE as v (v)}
            <button role="radio" aria-checked={settings.upsampleKhz === v} class:sel={settings.upsampleKhz === v} onclick={() => set({ upsampleKhz: v })}>
              {v ? t("enh.upTo", { rate: num(v) }) : t("enh.offShort")}
            </button>
          {/each}
        </div>
        <p class="help">{t("enh.upsamplingHelp", { a: num(44.1), b: num(352.8) })}</p>
      </fieldset>

      <fieldset>
        <legend>{t("enh.eq")}</legend>
        <div class="eq-head">
          <span class="eq-name" class:off={!eqName}>{eqName ?? t("enh.offShort")}</span>
          {#if ir}<span class="eq-meta">{t("enh.convolutionWith", { name: ir.name })}</span>{/if}
        </div>
        <button class="load" onclick={() => ((open = false), onopeneq())}>{t("enh.eqOpen")}</button>
        <p class="help">{t("enh.eqHelp")}</p>
      </fieldset>

      <fieldset>
        <legend>{t("enh.crossfeed")}</legend>
        <div class="seg" role="radiogroup" aria-label={t("enh.crossfeed")} use:liquidPill={settings.crossfeed}>
          {#each CROSSFEED as v (v)}
            <button role="radio" aria-checked={settings.crossfeed === v} class:sel={settings.crossfeed === v} onclick={() => set({ crossfeed: v })}>
              {v ? t(`enh.cf.${v}`) : t("enh.offShort")}
            </button>
          {/each}
        </div>
        <p class="help">{t("enh.crossfeedHelp")}</p>
      </fieldset>

      <fieldset>
        <legend>{t("enh.lossy")}</legend>
        <label class="toggle">
          <input type="checkbox" role="switch" checked={settings.headroom} onchange={(e) => set({ headroom: e.currentTarget.checked })} />
          <span class="knob" aria-hidden="true"></span>
          <span class="t-label">{t("enh.headroom")}</span>
        </label>
        <p class="help">{t("enh.headroomHelp")}</p>
        <label class="toggle">
          <input type="checkbox" role="switch" checked={settings.restoreHighs} onchange={(e) => set({ restoreHighs: e.currentTarget.checked })} />
          <span class="knob" aria-hidden="true"></span>
          <span class="t-label">{t("enh.restore")}</span>
        </label>
        <p class="help">{t("enh.restoreHelp")}</p>
        <p class="help">{t("enh.losslessNote")}</p>
      </fieldset>

      <p class="footer" class:pure={active.length === 0}>
        {#if active.length === 0}
          {t(lossyOn ? "enh.footerLossy" : "enh.footerPure")}
        {:else}
          {t("enh.footerOn")}
        {/if}
      </p>
    </div>
  {/if}
</div>

<style>
  .enh {
    position: relative;
  }
  .trigger {
    display: flex;
    align-items: baseline;
    gap: 8px;
    padding: 7px 12px;
    border-radius: var(--r-md);
    border: 1px solid var(--slate-600);
    font-size: var(--t-sm);
    font-weight: 600;
    max-width: 320px;
  }
  .trigger:hover {
    border-color: var(--slate-500);
  }
  .state {
    font-weight: 400;
    color: var(--mist);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .trigger.on {
    border-color: var(--violet);
  }
  .trigger.on .state {
    color: var(--violet);
  }

  .panel {
    position: absolute;
    inset-inline-end: 0;
    top: calc(100% + 8px);
    width: 380px;
    max-height: calc(100vh - 80px);
    overflow-y: auto;
    /* where a scroll bar takes room, take the same on the other side, so the content stays centred */
    scrollbar-gutter: stable both-edges;
    z-index: 20;
    background: var(--slate-700);
    border: 1px solid var(--slate-500);
    border-radius: var(--r-lg);
    padding: 6px 18px 14px;
    box-shadow: 0 18px 40px rgba(5, 12, 15, 0.55);
  }
  fieldset {
    border: 0;
    border-bottom: 1px solid var(--slate-600);
    margin: 0;
    padding: 14px 0 16px;
  }
  legend {
    padding: 0;
    margin-bottom: 10px;
    font-weight: 600;
    float: inline-start;
    width: 100%;
  }
  .seg {
    clear: both;
    display: grid;
    grid-auto-flow: column;
    grid-auto-columns: 1fr;
    background: var(--slate-800);
    border-radius: var(--r-md);
    padding: 3px;
    gap: 3px;
  }
  .seg button {
    padding: 6px 4px;
    border-radius: var(--r-md);
    font-size: var(--t-sm);
    color: var(--mist);
    white-space: nowrap;
  }
  .seg button:hover {
    color: var(--porcelain);
  }
  .seg button.sel {
    background: var(--slate-600);
    color: var(--porcelain);
    font-weight: 600;
  }
  .help {
    clear: both;
    margin: 10px 0 0;
    font-size: var(--t-xs);
    line-height: 1.5;
    color: var(--mist);
  }

  .eq-head {
    clear: both;
    display: flex;
    flex-direction: column;
  }
  .eq-name {
    font-weight: 600;
  }
  .eq-name.off {
    color: var(--mist);
    font-weight: 400;
  }
  .eq-meta {
    font-size: var(--t-xs);
    color: var(--mist);
  }
  .load {
    clear: both;
    display: block;
    width: 100%;
    margin-top: 10px;
    padding: 9px;
    border-radius: var(--r-md);
    border: 1px dashed var(--slate-500);
    color: var(--porcelain);
    font-size: var(--t-sm);
  }
  .load:hover {
    border-color: var(--mist);
  }

  .toggle {
    clear: both;
    display: flex;
    align-items: center;
    gap: 10px;
    margin-top: 12px;
    cursor: pointer;
    font-size: var(--t-sm);
    font-weight: 500;
  }
  .toggle:first-of-type {
    margin-top: 0;
  }
  .toggle input {
    position: absolute;
    opacity: 0;
    width: 1px;
    height: 1px;
  }
  .knob {
    flex: none;
    position: relative;
    width: 30px;
    height: 18px;
    border-radius: 999px;
    background: var(--slate-800);
    border: 1px solid var(--slate-500);
    transition: background 120ms ease, border-color 120ms ease;
  }
  .knob::after {
    content: "";
    position: absolute;
    top: 2px;
    inset-inline-start: 2px;
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: var(--mist);
    transition: transform 120ms ease, background 120ms ease;
  }
  .toggle input:checked + .knob {
    background: rgba(140, 122, 230, 0.25);
    border-color: var(--violet);
  }
  .toggle input:checked + .knob::after {
    transform: translateX(12px);
  }
  :global([dir="rtl"]) .toggle input:checked + .knob::after {
    transform: translateX(-12px);
    background: var(--violet);
  }
  .toggle input:focus-visible + .knob {
    outline: 2px solid var(--porcelain);
    outline-offset: 2px;
  }
  @media (prefers-reduced-motion: reduce) {
    .knob,
    .knob::after {
      transition: none;
    }
  }

  .footer {
    margin: 12px 0 0;
    font-size: var(--t-sm);
    color: var(--violet);
  }
  .footer.pure {
    color: var(--brass);
  }

  /* phone: the panel spans the screen under the header */
  @media (max-width: 700px), (max-height: 500px) {
    .trigger {
      max-width: 100%;
    }
    .panel {
      position: fixed;
      left: 10px;
      right: 10px;
      top: calc(var(--safe-top, env(safe-area-inset-top)) + 58px);
      width: auto;
      max-height: calc(100dvh - var(--safe-top, env(safe-area-inset-top)) - var(--safe-bottom, env(safe-area-inset-bottom)) - 80px);
      overflow-y: auto;
    }
  }
</style>
