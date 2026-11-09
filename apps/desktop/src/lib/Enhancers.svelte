<script lang="ts">
  import type { Crossfeed, DspSettings, EqInfo } from "./api";
  import { list, num, t, tn } from "./i18n.svelte";

  let {
    settings,
    eq,
    onchange,
    onpickeq,
  }: {
    settings: DspSettings;
    eq: EqInfo | null;
    onchange: (s: DspSettings) => void;
    onpickeq: () => void;
  } = $props();

  let open = $state(false);
  let root: HTMLElement;

  let active = $derived(
    [settings.upsampleKhz && t("enh.on.upsampling"), settings.eqPath && t("enh.on.eq"), settings.crossfeed && t("enh.on.crossfeed")].filter(
      Boolean,
    ) as string[],
  );

  let lossyOn = $derived(settings.headroom || settings.restoreHighs);

  const UPSAMPLE: (number | null)[] = [null, 192, 384, 768];
  const CROSSFEED: (Crossfeed | null)[] = [null, "light", "medium", "strong"];

  function set(patch: Partial<DspSettings>) {
    onchange({ ...settings, ...patch });
  }

  // EQ curve → SVG path. x: log 20 Hz–20 kHz, y: ±12 dB.
  const W = 320;
  const H = 96;
  const x = (f: number) => (Math.log10(f / 20) / 3) * W;
  const y = (db: number) => H / 2 - (Math.max(-12, Math.min(12, db)) / 12) * (H / 2 - 4);
  let curvePath = $derived(eq ? eq.curve.map(([f, db], i) => `${i ? "L" : "M"}${x(f).toFixed(1)},${y(db).toFixed(1)}`).join("") : "");

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
        <div class="seg" role="radiogroup" aria-label={t("enh.upsampling")}>
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
        {#if eq}
          <div class="eq-head">
            <span class="eq-name">{eq.name}</span>
            <span class="eq-meta num">{t("enh.eqMeta", { bands: tn("enh.bands", eq.bands.length), db: num(eq.preamp) })}</span>
          </div>
          <svg class="curve" viewBox={`0 0 ${W} ${H}`} role="img" aria-label={t("enh.eqCurve")}>
            {#each [100, 1000, 10000] as f (f)}
              <line x1={x(f)} x2={x(f)} y1="0" y2={H} class="grid" />
              <text x={x(f) + 3} y={H - 4} class="tick">{f >= 1000 ? `${f / 1000}k` : f}</text>
            {/each}
            <line x1="0" x2={W} y1={y(0)} y2={y(0)} class="zero" />
            <path d={curvePath} class="line" />
          </svg>
          <div class="row">
            <button class="link" onclick={onpickeq}>{t("enh.eqOther")}</button>
            <button class="link" onclick={() => set({ eqPath: null })}>{t("enh.eqOff")}</button>
          </div>
        {:else}
          <button class="load" onclick={onpickeq}>{t("enh.eqLoad")}</button>
          <p class="help">{t("enh.eqHelp")}</p>
        {/if}
      </fieldset>

      <fieldset>
        <legend>{t("enh.crossfeed")}</legend>
        <div class="seg" role="radiogroup" aria-label={t("enh.crossfeed")}>
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
    z-index: 20;
    background: var(--slate-700);
    border: 1px solid var(--slate-500);
    border-radius: 12px;
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
    border-radius: 6px;
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
  .eq-meta {
    font-size: var(--t-xs);
    color: var(--mist);
  }
  .curve {
    width: 100%;
    height: auto;
    margin-top: 10px;
    background: var(--slate-800);
    border-radius: var(--r-md);
  }
  .curve .grid {
    stroke: var(--slate-600);
    stroke-width: 1;
  }
  .curve .zero {
    stroke: var(--slate-500);
    stroke-dasharray: 3 3;
  }
  .curve .line {
    fill: none;
    stroke: var(--violet);
    stroke-width: 2;
  }
  .curve .tick {
    fill: var(--mist-dim);
    font-size: 9px;
    font-family: var(--font-display);
  }
  .row {
    display: flex;
    gap: 16px;
    margin-top: 8px;
  }
  .link {
    font-size: var(--t-sm);
    color: var(--mist);
  }
  .link:hover {
    color: var(--porcelain);
  }
  .load {
    clear: both;
    display: block;
    width: 100%;
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
    border-radius: 9px;
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
  @media (max-width: 700px) {
    .trigger {
      max-width: 100%;
    }
    .panel {
      position: fixed;
      left: 10px;
      right: 10px;
      top: calc(env(safe-area-inset-top) + 58px);
      width: auto;
      max-height: calc(100dvh - env(safe-area-inset-top) - env(safe-area-inset-bottom) - 80px);
      overflow-y: auto;
    }
  }
</style>
