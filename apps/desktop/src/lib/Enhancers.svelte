<script lang="ts">
  import { api, deviceLabel, dsdOutNoteText, isAndroid, isIOS, type Crossfeed, type DsdOrder, type DsdOut, type DspSettings, type IrInfo, type LevelMode, type LoudnessStatus, type Signal, type UpsampleFilter, type UpsampleTaps } from "./api";
  import { list, num, sentences, t } from "./i18n.svelte";
  import { liquidPill } from "./liquid";
  import { startAbx } from "./abx.svelte";

  let {
    settings,
    eqName,
    ir,
    track = null,
    signal = null,
    load = null,
    stepped = null,
    dsdStepped = null,
    device = null,
    others = [],
    onchange,
    oncopy,
    onopeneq,
  }: {
    /** The picked output's settings: every output has its own. */
    settings: DspSettings;
    /** The EQ preset playing on the picked output (null = off). */
    eqName: string | null;
    ir: IrInfo | null;
    /** What plays now, for the blind test of the processing. */
    track?: { path: string; title: string } | null;
    /** The playing track's signal path, for the processor load of a long filter. */
    signal?: Signal | null;
    /** The processing's share of one core (status.load). */
    load?: number | null;
    /** The filter an automatic step down left on, while it is still the one chosen. */
    stepped?: { filter: UpsampleFilter | null; taps: number | null } | null;
    /** What an automatic step down of the DSD output left on, and which step it was. */
    dsdStepped?: { dsdOut: number | null; order: number | null; kind: "rate" | "order" | "off" } | null;
    /** The picked output's name, whose settings these are (null: not known yet). */
    device?: string | null;
    /** The other outputs with settings of their own, to copy from (settings key, name as last seen). */
    others?: { key: string; name: string }[];
    onchange: (s: DspSettings) => void;
    /** Copy the settings of the output with this settings key to the picked one. */
    oncopy: (key: string) => void;
    onopeneq: () => void;
  } = $props();

  let open = $state(false);
  let root: HTMLElement;

  let active = $derived(
    [
      settings.upsampleKhz && t("enh.on.upsampling"),
      settings.dsdOut && t("enh.on.dsd"),
      eqName && t("enh.on.eq"),
      settings.convolution && t("enh.on.convolution"),
      settings.crossfeed && t("enh.on.crossfeed"),
      settings.level && t("enh.on.level"),
    ].filter(Boolean) as string[],
  );

  let lossyOn = $derived(settings.headroom || settings.restoreHighs);

  // A blind test needs something to compare (processing that reshapes the sound) and a file
  // to compare it on (the playing track; not a streaming one).
  let canTest = $derived(!!track && !/^(qobuz|tidal):/.test(track.path) && !!(eqName || settings.convolution || settings.crossfeed));
  function blindTest() {
    if (!track) return;
    open = false;
    startAbx({ kind: "processing", a: track.path, title: track.title });
  }
  // The long filter against the standard one: only while one plays this track (the test
  // takes its rate and filter from what plays), and not while DSD output runs (the test plays
  // PCM: it wouldn't be what is heard).
  let canTestFilter = $derived(!!track && !/^(qobuz|tidal):/.test(track.path) && !!signal?.upsampleFilter && !signal?.dsdOut);
  function filterTest() {
    if (!track) return;
    open = false;
    startAbx({ kind: "filters", a: track.path, title: track.title });
  }

  const UPSAMPLE: (number | null)[] = [null, 192, 384, 768];
  const CROSSFEED: (Crossfeed | null)[] = [null, "light", "medium", "strong"];
  const LEVEL: (LevelMode | null)[] = [null, "track", "album"];

  const FILTERS: (UpsampleFilter | null)[] = [null, "linear", "minimum", "apodizing"];
  const TAPS: UpsampleTaps[] = [65536, 1048576];
  /** Hz of transition band at each length, for a 44.1 kHz recording. */
  const TRANSITION_HZ: Record<UpsampleTaps, number> = { 65536: 13, 1048576: 0.8 };

  // Phones play the shorter length only (the backend holds them to it): nothing to choose.
  const phone = isIOS || isAndroid;
  let hasLength = $derived(!phone && (settings.upsampleFilter === "linear" || settings.upsampleFilter === "minimum"));
  // The processing's cost, where it is worth showing: a long filter or DSD output. Shown once,
  // in the DSD block while that runs (it is then most of the cost), else under the filter.
  let loadPct = $derived((signal?.upsampleFilter || signal?.dsdOut) && load ? load * 100 : null);
  let loadText = $derived(loadPct === null ? null : t("enh.load", { pct: num(loadPct, { maximumFractionDigits: loadPct < 10 ? 1 : 0 }) }));

  // DSD output: phones take DSD64 only (the higher rates cost a core's worth of battery or more).
  const DSD_OUT: (DsdOut | null)[] = phone ? [null, 64] : [null, 64, 128, 256];
  /** The rate chosen, as it plays: a higher one in a phone's settings plays as DSD64. */
  let dsdChosen = $derived<DsdOut | null>(phone && settings.dsdOut ? 64 : settings.dsdOut);
  const ORDERS: DsdOrder[] = [7, 5];
  /** Each modulator's signal-to-noise at each rate, dB: measured by the engine's tests (a 1 kHz
   *  tone at full scale against the noise from 20 Hz to 20 kHz), rounded down. */
  const DSD_SNR: Record<DsdOut, Record<DsdOrder, number>> = {
    64: { 5: 109, 7: 119 },
    128: { 5: 127, 7: 132 },
    256: { 5: 146, 7: 160 },
  };
  /** The one notice under DSD output: why it changed by itself, why it doesn't run, or that it
   *  runs at a lower rate than chosen. */
  let dsdNotice = $derived.by(() => {
    const s = dsdStepped;
    if (s && s.dsdOut === settings.dsdOut && (s.order === null || s.order === settings.dsdOrder)) {
      if (s.kind === "off") return t("enh.dsdStepOff");
      if (s.kind === "order") return t("enh.dsdStepOrder");
      return t("enh.dsdStepRate", { dsd: `DSD${s.dsdOut}` });
    }
    if (signal && settings.dsdOut !== null && signal.dsdOut === null && signal.dsdOutNote)
      return t("enh.dsdOff", { reason: dsdOutNoteText(signal.dsdOutNote) });
    if (signal?.dsdOut && dsdChosen !== null && signal.dsdOut < dsdChosen) return t("enh.dsdLimited", { dsd: `DSD${signal.dsdOut}` });
    return null;
  });

  /** The step-down notice. A length written with an abbreviation's dot ("64 тыс.") ends the
   *  sentence with that dot, not a second one. */
  function shortened(taps: number): string {
    const length = t(`enh.len.${taps}`);
    const text = t("enh.stepShorter", { length });
    return length.endsWith(".") ? text.replace(`${length}.`, length) : text;
  }

  /** Arrow keys walk the filter list; Enter or a click picks (each pick restarts playback, so
   *  walking past a choice doesn't pick it; Space stays play/pause). Tab enters at the chosen one. */
  function filterKeys(e: KeyboardEvent) {
    const group = e.currentTarget as HTMLElement;
    const rows = [...group.querySelectorAll<HTMLButtonElement>("button")];
    const at = rows.indexOf(document.activeElement as HTMLButtonElement);
    // Sideways follows the reading direction.
    const back = getComputedStyle(group).direction === "rtl" ? "ArrowRight" : "ArrowLeft";
    let to: number;
    if (e.key === "ArrowDown" || e.key === "ArrowRight" || e.key === "ArrowLeft" || e.key === "ArrowUp") {
      const step = e.key === "ArrowUp" || e.key === back ? -1 : 1;
      to = ((at < 0 ? 0 : at) + step + rows.length) % rows.length;
    } else if (e.key === "Home") to = 0;
    else if (e.key === "End") to = rows.length - 1;
    else return;
    e.preventDefault();
    rows[to].focus();
  }

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
      <!-- Whose settings these are: each output keeps its own. -->
      <div class="whose">
        <div class="whose-row">
          {#if device}<span class="whose-name">{t("enh.forDevice", { device: deviceLabel(device) })}</span>{/if}
          {#if others.length}
            <select
              class="copy"
              aria-label={t("enh.copyFrom")}
              value=""
              onchange={(e) => {
                const key = e.currentTarget.value;
                // Back to the prompt: the list is an action, not a setting.
                e.currentTarget.value = "";
                if (key) oncopy(key);
              }}
            >
              <option value="" disabled>{t("enh.copyFrom")}</option>
              {#each others as o (o.key)}
                <option value={o.key}>{deviceLabel(o.name)}</option>
              {/each}
            </select>
          {/if}
        </div>
        <p class="help">{t("enh.perDevice")}</p>
      </div>

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
        {#if settings.dsdOut !== null}
          <p class="help">{t("enh.dsdUpsampling")}</p>
        {/if}
        <!-- DSD output upsamples too (to its own rate), through the filter chosen here. -->
        {#if settings.upsampleKhz !== null || settings.dsdOut !== null}
          <div class="sub">
            <span class="sub-title" id="enh-filter">{t("enh.filter")}</span>
            <!-- A list rather than a row: four names don't fit side by side in most languages. -->
            <div class="radios" role="radiogroup" aria-labelledby="enh-filter" tabindex="-1" onkeydown={filterKeys}>
              {#each FILTERS as f (f)}
                {@const sel = settings.upsampleFilter === f}
                {@const key = f ?? "standard"}
                <button role="radio" aria-checked={sel} class:sel tabindex={sel ? 0 : -1} aria-describedby={`enh-f-${key}`} onclick={() => sel || set({ upsampleFilter: f })}>
                  <span class="r-name">{t(`enh.filter.${key}`)}</span>
                  <span class="r-desc" id={`enh-f-${key}`}>{t(`enh.filter.${key}Help`)}</span>
                </button>
              {/each}
            </div>
            {#if hasLength}
              <span class="sub-title" id="enh-length">{t("enh.length")}</span>
              <div class="seg" role="radiogroup" aria-labelledby="enh-length" use:liquidPill={settings.upsampleTaps}>
                {#each TAPS as v (v)}
                  <button role="radio" aria-checked={settings.upsampleTaps === v} class:sel={settings.upsampleTaps === v} onclick={() => set({ upsampleTaps: v })}>
                    {t(`enh.len.${v}`)}
                  </button>
                {/each}
              </div>
              <p class="help">
                {sentences([
                  t("enh.lengthHelp", { a: num(TRANSITION_HZ[65536]), b: num(TRANSITION_HZ[1048576]), rate: num(44.1) }),
                  settings.upsampleFilter === "linear" && settings.upsampleTaps === 1048576 && t("enh.lengthAhead"),
                ])}
              </p>
            {/if}
            {#if loadText && !signal?.dsdOut}
              <p class="help figure">{loadText}</p>
            {/if}
            {#if stepped && stepped.filter === settings.upsampleFilter}
              <p class="help notice" role="status">
                {stepped.filter && stepped.taps ? shortened(stepped.taps) : t("enh.stepStandard")}
              </p>
            {/if}
            {#if canTestFilter}
              <button class="load" onclick={filterTest}>{t("enh.abxFilter")}</button>
              <p class="help">{t("enh.abxFilterHelp")}</p>
            {/if}
          </div>
        {/if}
      </fieldset>

      <!-- Next to upsampling: DSD output sets the rate, and the filter above serves both. -->
      <fieldset>
        <legend>{t("enh.dsd")}</legend>
        <div class="seg" role="radiogroup" aria-label={t("enh.dsd")} use:liquidPill={dsdChosen}>
          {#each DSD_OUT as v (v)}
            <button role="radio" aria-checked={dsdChosen === v} class:sel={dsdChosen === v} onclick={() => set({ dsdOut: v })}>
              {v ? `DSD${v}` : t("enh.offShort")}
            </button>
          {/each}
        </div>
        <p class="help">{sentences([t("enh.dsdHelp"), t("enh.dsdNeeds")])}</p>
        {#if dsdChosen !== null}
          {@const rate = dsdChosen}
          <div class="sub">
            <span class="sub-title" id="enh-modulator">{t("enh.modulator")}</span>
            <div class="radios" role="radiogroup" aria-labelledby="enh-modulator" tabindex="-1" onkeydown={filterKeys}>
              {#each ORDERS as o (o)}
                {@const sel = settings.dsdOrder === o}
                <button role="radio" aria-checked={sel} class:sel tabindex={sel ? 0 : -1} aria-describedby={`enh-m-${o}`} onclick={() => sel || set({ dsdOrder: o })}>
                  <span class="r-name">{t(`enh.modulator.${o}`)}</span>
                  <span class="r-desc" id={`enh-m-${o}`}>{t(`enh.modulator.${o}Help`, { db: num(DSD_SNR[rate][o]) })}</span>
                </button>
              {/each}
            </div>
          </div>
        {/if}
        {#if loadText && signal?.dsdOut}
          <p class="help figure">{loadText}</p>
        {/if}
        <!-- Outside the block above: "turned off" stays readable while the row shows "Off". -->
        {#if dsdNotice}
          <p class="help notice" role="status">{dsdNotice}</p>
        {/if}
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

      <!-- "Do I hear it?", answered by listening: the playing track with and without. -->
      {#if canTest}
        <fieldset>
          <button class="load" onclick={blindTest}>{t("enh.abx")}</button>
          <p class="help">{t("enh.abxHelp")}</p>
        </fieldset>
      {/if}

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
  /* Whose settings these are: a block of its own above the first one, with its line. */
  .whose {
    border-bottom: 1px solid var(--slate-600);
    padding: 14px 0 16px;
  }
  .whose-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 6px 12px;
    flex-wrap: wrap;
  }
  .whose-name {
    min-width: 0;
    font-weight: 600;
    overflow-wrap: anywhere;
  }
  .whose .help {
    margin-top: 6px;
  }
  .copy {
    flex: none;
    max-width: 100%;
    margin-inline-start: auto;
    box-sizing: border-box;
    height: 27px;
    padding: 0 10px;
    font-size: var(--t-xs);
    color: var(--mist);
  }
  .copy:hover,
  .copy:focus-visible {
    color: var(--porcelain);
  }
  :global(html[data-look="glass"]) .enh .copy {
    border-radius: 999px;
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

  /* The upsampling filter: a sub-block of its own under the rate. */
  .sub {
    clear: both;
  }
  .sub-title {
    display: block;
    margin: 16px 0 8px;
    font-size: var(--t-sm);
    font-weight: 600;
  }
  .radios {
    display: flex;
    flex-direction: column;
    gap: 3px;
    background: var(--slate-800);
    border-radius: var(--r-md);
    padding: 3px;
  }
  .radios:focus {
    outline: none;
  }
  .radios button {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 2px;
    padding: 7px 10px 8px;
    border-radius: var(--r-md);
    text-align: start;
    color: var(--mist);
  }
  .r-name {
    font-size: var(--t-sm);
  }
  .r-desc {
    font-size: var(--t-xs);
    line-height: 1.45;
    color: var(--mist);
  }
  .radios button:hover {
    color: var(--porcelain);
  }
  .radios button.sel {
    background: var(--slate-600);
    color: var(--porcelain);
  }
  .radios button.sel .r-name {
    font-weight: 600;
  }
  .help.figure {
    font-variant-numeric: tabular-nums;
  }
  .help.notice {
    color: var(--porcelain);
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
