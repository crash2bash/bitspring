<script lang="ts">
  import { api, type Crossfeed, type DspSettings, type IrInfo, type LevelMode, type LoudnessStatus } from "./api";
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
      settings.level && t("enh.on.level"),
    ].filter(Boolean) as string[],
  );

  let lossyOn = $derived(settings.headroom || settings.restoreHighs);

  const UPSAMPLE: (number | null)[] = [null, 192, 384, 768];
  const CROSSFEED: (Crossfeed | null)[] = [null, "light", "medium", "strong"];
  const LEVEL: (LevelMode | null)[] = [null, "track", "album"];

  // How much of the library is measured: asked while the panel is open and levelling is on,
  // more often while the analysis runs.
  let loudness = $state<LoudnessStatus | null>(null);
  $effect(() => {
    if (!open || !settings.level) return;
    let timer: ReturnType<typeof setTimeout>;
    let gone = false;
    const ask = async () => {
      try {
        loudness = await api.loudnessStatus();
      } catch {
        loudness = null;
      }
      if (!gone) timer = setTimeout(ask, loudness?.running || loudness?.tagging ? 700 : 4000);
    };
    ask();
    return () => {
      gone = true;
      clearTimeout(timer);
    };
  });

  async function analyze(start: boolean) {
    await api.loudnessAnalyze(start);
    loudness = await api.loudnessStatus();
  }

  // Writing into the user's files is asked about first.
  let confirmTags = $state(false);
  async function writeTags(start: boolean) {
    confirmTags = false;
    await api.loudnessWriteTags(start);
    loudness = await api.loudnessStatus();
  }

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

      <!-- Right after the three it protects against: it acts only while one of them is on. -->
      <fieldset>
        <label class="toggle lead">
          <input type="checkbox" role="switch" checked={settings.peakHeadroom} onchange={(e) => set({ peakHeadroom: e.currentTarget.checked })} />
          <span class="knob" aria-hidden="true"></span>
          <span class="t-label">{t("enh.peak")}</span>
        </label>
        <p class="help">{t("enh.peakHelp")}</p>
      </fieldset>

      <fieldset>
        <legend>{t("enh.level")}</legend>
        <div class="seg" role="radiogroup" aria-label={t("enh.level")} use:liquidPill={settings.level}>
          {#each LEVEL as v (v)}
            <button role="radio" aria-checked={settings.level === v} class:sel={settings.level === v} onclick={() => set({ level: v })}>
              {v ? t(`enh.level.${v}`) : t("enh.offShort")}
            </button>
          {/each}
        </div>
        <p class="help">{t("enh.levelHelp")}</p>
        {#if settings.level && loudness && loudness.tracks > 0}
          <div class="measure">
            {#if loudness.running}
              <span>{t("enh.levelRunning", { done: num(loudness.running[0]), total: num(loudness.running[1]) })}</span>
              <button onclick={() => analyze(false)}>{t("enh.levelStop")}</button>
            {:else}
              <span>{t("enh.levelStatus", { done: num(loudness.analysed), total: num(loudness.tracks) })}</span>
              {#if loudness.analysed < loudness.tracks}
                <button onclick={() => analyze(true)}>{t("enh.levelAnalyze")}</button>
              {/if}
            {/if}
          </div>
          {#if loudness.tagging}
            <div class="measure">
              <span>{t("enh.tagsRunning", { done: num(loudness.tagging[0]), total: num(loudness.tagging[1]) })}</span>
              <button onclick={() => writeTags(false)}>{t("enh.levelStop")}</button>
            </div>
          {:else}
            {#if loudness.tagged}
              <p class="measure">
                {t("enh.tagsDone", { written: num(loudness.tagged[0]) })}
                {#if loudness.tagged[1] > 0}{t("enh.tagsFailed", { failed: num(loudness.tagged[1]) })}{/if}
              </p>
            {/if}
            {#if loudness.untagged > 0 && !loudness.running}
              {#if confirmTags}
                <div class="measure ask">
                  <p>{t("enh.tagsConfirm", { count: num(loudness.untagged) })}</p>
                  <button onclick={() => writeTags(true)}>{t("enh.tagsYes")}</button>
                  <button onclick={() => (confirmTags = false)}>{t("pl.cancel")}</button>
                </div>
              {:else}
                <div class="measure">
                  <span>{t("enh.tagsOffer", { count: num(loudness.untagged) })}</span>
                  <button onclick={() => (confirmTags = true)}>{t("enh.tagsWrite")}</button>
                </div>
              {/if}
            {/if}
          {/if}
        {/if}
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
          <!-- Names what breaks bit-perfect: the MP3 switches and the headroom don't, and
               needn't be turned off to get it back. -->
          {t("enh.footerOn", { what: list(active) })}
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
  /* A block that is one switch: the switch's name stands where a block's title does. */
  .toggle.lead {
    font-weight: 600;
    font-size: inherit;
  }

  .measure {
    clear: both;
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    flex-wrap: wrap;
    gap: 4px 12px;
    margin-top: 10px;
    font-size: var(--t-xs);
    color: var(--mist);
    font-variant-numeric: tabular-nums;
  }
  .measure button {
    font-size: var(--t-xs);
    color: var(--porcelain);
    text-decoration: underline;
    text-underline-offset: 3px;
  }
  .measure button:hover {
    color: var(--violet);
  }
  /* The question before writing into files: its text on a line of its own, the two
     answers under it, side by side. */
  .measure.ask {
    justify-content: flex-start;
    gap: 6px 16px;
    color: var(--porcelain);
  }
  .measure.ask p {
    flex-basis: 100%;
    margin: 0;
    line-height: 1.45;
  }
  p.measure {
    display: block;
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
