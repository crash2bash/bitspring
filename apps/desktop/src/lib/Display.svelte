<script lang="ts">
  import { onMount } from "svelte";
  import { api, clock, deviceLabel, dsdName, dsdNoteText, filePath, khz, osMixer, transportLabel, type NowPlaying, type Status } from "./api";
  import { list, num, sentences, t } from "./i18n.svelte";

  let {
    now,
    status,
    testing = false,
    error = null,
    deviceName,
    sweepKey = 0,
    onseek,
  }: {
    now: NowPlaying | null;
    status: Status;
    testing?: boolean;
    error?: string | null;
    deviceName: string;
    /** Changes on each new track: replays the one "cable lights up" sweep. */
    sweepKey?: number;
    onseek: (seconds: number) => void;
  } = $props();

  let signal = $derived(now?.signal ?? null);
  let format = $derived(now && !testing ? (filePath(now.path).split(".").pop()?.toUpperCase() ?? null) : null);

  // ---- signal path ---------------------------------------------------------------------------
  let processing = $derived.by(() => {
    if (!signal) return [] as string[];
    const steps: string[] = [];
    if (signal.dsdRate !== null && !signal.dop) steps.push(t("step.dsdToPcm", { dsd: dsdName(signal.dsdRate), rate: khz(signal.sourceRate) }));
    if (signal.resampled) steps.push(t("step.resample", { from: khz(signal.sourceRate), to: khz(signal.deviceRate) }));
    if (signal.eq) steps.push(t(signal.eqLinear ? "step.eqLinear" : "step.eq", { name: signal.eq }));
    if (signal.convolution) steps.push(t("step.convolution", { name: signal.convolution }));
    if (signal.crossfeed) steps.push(t(`step.crossfeed.${signal.crossfeed}`));
    if (signal.headroomDb !== null)
      steps.push(t(signal.headroomMeasured ? "step.headroom" : "step.headroomPending", { db: num(signal.headroomDb) }));
    if (signal.restoreFromHz !== null) steps.push(t("step.restore", { rate: khz(signal.restoreFromHz) }));
    return steps;
  });

  let links = $derived.by(() => {
    if (!signal) return null;
    const decode = signal.sourceBits !== null && signal.sourceBits > 24 ? t("step.truncate") : null;
    const parts = [...processing];
    if (!signal.exclusive) parts.push(osMixer());
    const toDac = parts.length ? list(parts) : null;
    const out =
      signal.transport === "Bluetooth" ? t("step.bluetooth") : signal.transport === "AirPlay" ? t("step.airplay") : null;
    return { decode, toDac, out };
  });

  let clipping = $derived(status.peakDb !== null && status.peakDb > -0.1);

  let lamp = $derived.by(() => {
    if (error) return { tone: "alarm", text: t("lamp.error") };
    if (!signal) return { tone: "off", text: t("lamp.noSignal") };
    if (testing) return { tone: "ok", text: t("test.title") };
    if (clipping) return { tone: "alarm", text: t("lamp.clipping") };
    if (signal.bitPerfect) return { tone: "ok", text: t("signal.bitPerfect") };
    if (processing.length) return { tone: "dsp", text: t("signal.processing") };
    if (signal.sourceLossy && signal.exclusive) return { tone: "ok", text: t("lamp.unchanged") };
    return { tone: "dsp", text: t("lamp.altered") };
  });

  let verdict = $derived.by(() => {
    if (error) return error;
    if (!signal || !links) return t("verdict.empty");
    if (testing) return t("verdict.test");
    if (signal.bitPerfect && signal.dop) return t("verdict.dop");
    // iOS may scale the samples by the system volume (unless the DAC takes the volume itself).
    if (signal.bitPerfect && status.systemVolume !== null && status.systemVolume < 0.999)
      return t("verdict.iosVolume");
    if (signal.bitPerfect) return t("verdict.bitPerfect");
    const reasons = list([links.decode, links.toDac, links.out]);
    if (!reasons && signal.sourceLossy) return t("verdict.lossy");
    return sentences([
      t(processing.length ? "verdict.processed" : "verdict.changed", { reasons }),
      clipping && t("verdict.clip"),
      signal.dsdNote && t("verdict.noDop", { reason: dsdNoteText(signal.dsdNote) }),
    ]);
  });

  // ---- level meters ----------------------------------------------------------------------------
  const FLOOR = -60;
  const SEGMENTS = 30;
  const segDb = (i: number) => FLOOR + ((i + 1) * -FLOOR) / SEGMENTS; // top edge of segment i

  let level = $state([FLOOR, FLOOR]);
  let hold = $state([FLOOR, FLOOR]);
  let holdAt = [0, 0];

  onMount(() => {
    let alive = true;
    let last = 0;
    const tick = async (t: number) => {
      if (!alive) return;
      if (t - last >= 40) {
        last = t;
        const playing = status.active && !status.paused;
        const peaks = playing ? await api.meter() : [FLOOR, FLOOR];
        for (const c of [0, 1]) {
          // Instant attack, ~20 dB/s release — the usual peak-meter ballistics.
          level[c] = Math.max(peaks[c], level[c] - 0.8, FLOOR);
          if (level[c] >= hold[c] || t - holdAt[c] > 1500) {
            hold[c] = level[c];
            holdAt[c] = t;
          }
        }
      }
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
    return () => (alive = false);
  });

  function segClass(i: number, c: number) {
    const db = segDb(i);
    const lit = level[c] >= db - 0.01 || (Math.abs(hold[c] - db) < -FLOOR / SEGMENTS && hold[c] > FLOOR + 1);
    const zone = db > -1 ? "hot" : db > -12 ? "warm" : "cool";
    return `${zone}${lit ? " lit" : ""}`;
  }

  // ---- position ------------------------------------------------------------------------------
  let scrubbing = $state<number | null>(null);
  let duration = $derived(status.duration || now?.duration || 0);
  let position = $derived(scrubbing ?? status.position);
</script>

<section class="display" aria-label={t("display.label")}>
  <div class="top">
    <span class="lamp {lamp.tone}" role="status"><i aria-hidden="true"></i>{lamp.text}</span>
    {#if signal}
      <span class="where">{list([deviceLabel(deviceName), transportLabel(signal.transport)])}</span>
    {/if}
  </div>

  <div class="readout">
    <div class="digits" class:dim={!signal}>
      {#if signal?.dop && signal.dsdRate}
        <span class="value num">{dsdName(signal.dsdRate)}</span><span class="unit">DoP</span>
      {:else}
        <span class="value num">{signal ? khz(signal.deviceRate) : "– –"}</span><span class="unit">{t("unit.khz")}</span>
        <span class="value num bits">{signal?.dacBits ?? "– –"}</span><span class="unit">{t("unit.bit")}</span>
      {/if}
      <p class="source">
        {#if signal && !testing}
          {#if signal.dsdRate !== null}
            {t("source.file", { details: list([dsdName(signal.dsdRate), `${num(signal.dsdRate / 1e6, { maximumFractionDigits: 2 })} ${t("unit.mhz")}`, format]) })}
          {:else}
            {t("source.file", { details: list([`${khz(signal.sourceRate)} ${t("unit.khz")}`, t("source.bits", { bits: signal.sourceBits ?? "?" }), format]) })}
          {/if}
        {:else if testing}
          {t("source.test")}
        {:else}
          {t("source.waiting")}
        {/if}
      </p>
    </div>

    <!-- a level meter reads left to right in every language, like the scale under it -->
    <div class="meters" aria-hidden="true" dir="ltr">
      {#each ["L", "R"] as ch, c (ch)}
        <div class="meter">
          <span class="ch">{ch}</span>
          <span class="ladder">
            {#each Array(SEGMENTS) as _, i (i)}<b class={segClass(i, c)}></b>{/each}
          </span>
          <span class="db num">{level[c] <= FLOOR ? "−∞" : Math.round(level[c]).toString().replace("-", "−")}</span>
        </div>
      {/each}
      <div class="scale num">
        {#each [-60, -40, -20, -12, -6, 0] as d (d)}
          <span style={`left: ${((d - FLOOR) / -FLOOR) * 100}%`}>{d === 0 ? "0" : String(d).replace("-", "−")}</span>
        {/each}
      </div>
    </div>
  </div>

  {#if signal && links}
    {#key sweepKey}
      <ol class="chain">
        <li class="node">{t("chain.file")}<small>{format ?? t("chain.test")}</small></li>
        <li class="link" class:altered={links.decode} style="--i: 0"><span class="cable"></span></li>
        <li class="node">{t("chain.engine")}<small>{t(processing.length ? "chain.processing" : "chain.unchanged")}</small></li>
        <li class="link" class:altered={links.toDac} style="--i: 1"><span class="cable"></span></li>
        <li class="node">{deviceLabel(deviceName)}<small>{t(signal.integerMode ? "chain.integer" : signal.dacFloat ? "chain.float" : "chain.hal")}</small></li>
        <li class="link" class:altered={links.out} style="--i: 2"><span class="cable"></span></li>
        <li class="node">{t("chain.headphones")}<small>{t(links.out ? "chain.lossy" : "chain.lossless")}</small></li>
      </ol>
    {/key}
  {/if}

  <p class="verdict {lamp.tone}">{verdict}</p>

  <!-- media time runs left to right in every language (like the transport keys) -->
  <div class="scrub" dir="ltr">
    <span class="t num">{clock(position)}</span>
    <input
      type="range"
      min="0"
      max={duration || 1}
      step="0.1"
      value={position}
      style={`--p: ${duration ? (position / duration) * 100 : 0}%`}
      aria-label={t("display.scrub")}
      disabled={!signal || testing}
      oninput={(e) => (scrubbing = +(e.target as HTMLInputElement).value)}
      onchange={(e) => {
        onseek(+(e.target as HTMLInputElement).value);
        scrubbing = null;
      }}
    />
    <span class="t num">{clock(duration)}</span>
  </div>
</section>

<style>
  .display {
    --glass: #0e181c;
    position: relative;
    background:
      linear-gradient(180deg, rgba(232, 237, 234, 0.035), transparent 38%),
      var(--glass);
    border-radius: 10px;
    padding: 18px 22px 16px;
    /* recessed into the faceplate */
    box-shadow:
      inset 0 2px 10px rgba(0, 0, 0, 0.55),
      inset 0 0 0 1px rgba(0, 0, 0, 0.6),
      0 1px 0 rgba(232, 237, 234, 0.06);
  }

  .top {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 16px;
    font-size: var(--t-sm);
  }
  .lamp {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    font-weight: 600;
  }
  .lamp i {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--slate-600);
  }
  .lamp.ok {
    color: var(--brass);
  }
  .lamp.ok i {
    background: var(--brass);
    box-shadow: 0 0 8px var(--brass-glow);
  }
  .lamp.dsp {
    color: var(--violet);
  }
  .lamp.dsp i {
    background: var(--violet);
    box-shadow: 0 0 8px rgba(140, 122, 230, 0.4);
  }
  .lamp.alarm {
    color: var(--alarm);
  }
  .lamp.alarm i {
    background: var(--alarm);
  }
  .lamp.off {
    color: var(--mist-dim);
  }
  .where {
    color: var(--mist);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .readout {
    display: grid;
    grid-template-columns: auto minmax(220px, 1fr);
    gap: 36px;
    align-items: center;
    margin-top: 6px;
  }
  .digits {
    white-space: nowrap;
  }
  .value {
    font-weight: 600;
    font-size: 4.25rem;
    line-height: 1;
    letter-spacing: -0.02em;
    color: var(--porcelain);
  }
  .value.bits {
    margin-inline-start: 22px;
  }
  .unit {
    font-family: var(--font-display);
    font-size: var(--t-lg);
    color: var(--mist);
    margin-inline-start: 6px;
  }
  .digits.dim .value {
    color: var(--slate-500);
  }
  .source {
    margin: 4px 0 0;
    color: var(--mist);
    font-size: var(--t-sm);
  }

  .meters {
    display: grid;
    gap: 7px;
  }
  .meter {
    display: grid;
    grid-template-columns: 14px 1fr 30px;
    align-items: center;
    gap: 8px;
  }
  .ch,
  .db {
    font-size: var(--t-xs);
    color: var(--mist);
  }
  .db {
    text-align: right;
  }
  .ladder {
    display: grid;
    grid-template-columns: repeat(30, 1fr);
    gap: 2px;
    height: 12px;
  }
  .ladder b {
    border-radius: 1.5px;
    background: #1a272d;
  }
  .ladder b.cool.lit {
    background: #c5cfcb;
  }
  .ladder b.warm.lit {
    background: var(--brass);
  }
  .ladder b.hot.lit {
    background: var(--alarm);
  }
  .scale {
    position: relative;
    height: 12px;
    margin: 0 38px 0 22px;
    font-size: 10px;
    color: var(--mist-dim);
  }
  .scale span {
    position: absolute;
    transform: translateX(-50%);
  }

  .chain {
    list-style: none;
    margin: 20px 0 0;
    padding: 14px 0 0;
    border-top: 1px solid rgba(232, 237, 234, 0.06);
    display: grid;
    grid-template-columns: auto 1fr auto 1fr auto 1fr auto;
    align-items: center;
  }
  .node {
    display: flex;
    flex-direction: column;
    font-size: var(--t-sm);
    font-weight: 600;
    white-space: nowrap;
    max-width: 150px;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .node small {
    font-weight: 400;
    font-size: var(--t-xs);
    color: var(--mist);
  }
  .link {
    margin: 0 12px;
  }
  .cable {
    display: block;
    height: 3px;
    border-radius: 2px;
    background: var(--brass);
    box-shadow: 0 0 8px var(--brass-glow);
    transform-origin: left center;
  }
  :global([dir="rtl"]) .cable {
    transform-origin: right center;
    animation: light-up 420ms cubic-bezier(0.2, 0.7, 0.2, 1) both;
    animation-delay: calc(var(--i) * 160ms);
  }
  .link.altered .cable {
    background: repeating-linear-gradient(90deg, var(--violet) 0 8px, transparent 8px 13px);
    box-shadow: none;
  }
  @keyframes light-up {
    from {
      transform: scaleX(0);
      opacity: 0.2;
    }
    to {
      transform: scaleX(1);
      opacity: 1;
    }
  }

  .verdict {
    margin: 12px 0 0;
    font-size: var(--t-sm);
    color: var(--mist);
    max-width: 80ch;
  }
  .verdict.ok {
    color: var(--brass);
  }
  .verdict.dsp {
    color: var(--violet);
  }
  .verdict.alarm {
    color: var(--alarm);
  }

  .scrub {
    display: grid;
    grid-template-columns: 44px 1fr 44px;
    align-items: center;
    gap: 10px;
    margin-top: 14px;
  }
  .t {
    font-size: var(--t-sm);
    color: var(--mist);
  }
  .t:last-child {
    text-align: end;
  }
  .scrub input {
    -webkit-appearance: none;
    appearance: none;
    width: 100%;
    height: 18px;
    background: transparent;
    cursor: pointer;
    margin: 0;
  }
  .scrub input:disabled {
    cursor: default;
  }
  .scrub input::-webkit-slider-runnable-track {
    height: 4px;
    border-radius: 2px;
    background: linear-gradient(90deg, var(--porcelain) var(--p), #1f3039 var(--p));
  }
  .scrub input::-webkit-slider-thumb {
    -webkit-appearance: none;
    width: 12px;
    height: 12px;
    margin-top: -4px;
    border-radius: 50%;
    background: var(--porcelain);
    opacity: 0;
  }
  .scrub:hover input:not(:disabled)::-webkit-slider-thumb,
  .scrub input:focus-visible::-webkit-slider-thumb {
    opacity: 1;
  }

  @media (max-height: 700px) {
    .value {
      font-size: 3.25rem;
    }
    .chain {
      margin-top: 14px;
      padding-top: 10px;
    }
  }

  @media (max-width: 1040px) {
    .value {
      font-size: 3.25rem;
    }
    .readout {
      grid-template-columns: 1fr;
      gap: 14px;
    }
  }

  @media (max-width: 700px), (max-height: 500px) {
    .node {
      max-width: 84px;
    }
    .link {
      margin: 0 6px;
    }
  }
</style>
