<script lang="ts">
  import type { EqBand, EqPreset } from "./api";
  import { bandDb, designer, effectivePreamp, hasGain, isSplit, logspace, newBand, channelGainDb } from "./eqmath";
  import { num, t } from "./i18n.svelte";

  let {
    preset,
    selected,
    range = 12,
    dim = false,
    onchange,
    onselect,
  }: {
    preset: EqPreset;
    selected: number | null;
    /** ± dB shown. */
    range?: number;
    /** Compare mode: the EQ isn't heard right now. */
    dim?: boolean;
    /** `final` is false while a drag is still going. */
    onchange: (p: EqPreset, final: boolean) => void;
    onselect: (i: number | null) => void;
  } = $props();

  const FS = 48000;
  const LO = 20;
  const HI = 20000;
  const PAD_T = 10;
  const PAD_B = 22;

  let width = $state(640);
  let height = $state(300);
  let svg: SVGSVGElement;

  const x = (f: number) => (Math.log10(f / LO) / Math.log10(HI / LO)) * width;
  const fx = (px: number) => LO * Math.pow(HI / LO, Math.min(1, Math.max(0, px / width)));
  const plotH = $derived(height - PAD_T - PAD_B);
  const y = (db: number) => PAD_T + plotH / 2 - (Math.max(-range * 1.2, Math.min(range * 1.2, db)) / range) * (plotH / 2);
  const dby = (py: number) => ((PAD_T + plotH / 2 - py) / (plotH / 2)) * range;

  // ---- curves -------------------------------------------------------------------------------
  let freqs = $derived(logspace(LO, HI, Math.max(64, Math.round(width / 2))));
  let shape = $derived.by(() => {
    const d = designer(preset, FS);
    return [0, 1].map((ch) => freqs.map((f) => d.shape(f, ch)));
  });
  let split = $derived(isSplit(preset));
  let preamp = $derived(effectivePreamp(preset, FS));
  let gains = $derived([channelGainDb(preset, 0, FS), channelGainDb(preset, 1, FS)]);

  const line = (dbs: number[]) => dbs.map((db, i) => `${i ? "L" : "M"}${x(freqs[i]).toFixed(1)},${y(db).toFixed(1)}`).join("");
  let pathL = $derived(line(shape[0]));
  let pathR = $derived(split ? line(shape[1]) : "");
  let area = $derived(`${pathL}L${width},${y(0)}L0,${y(0)}Z`);
  /** Where the EQ plus its preamp goes over full scale: drawn in red. */
  let total = $derived([0, 1].map((ch) => shape[ch].map((db) => db + gains[ch])));
  let overs = $derived(total.some((c) => c.some((db) => db > 0.05)));
  /** Vertical hatching wherever either channel goes over. */
  let overMask = $derived.by(() => {
    if (!overs) return "";
    let d = "";
    for (let i = 0; i < freqs.length; i++) {
      const over = total[0][i] > 0.05 || total[1][i] > 0.05;
      if (over) d += `M${x(freqs[i]).toFixed(1)},${PAD_T}V${height - PAD_B}`;
    }
    return d;
  });

  let selectedPath = $derived.by(() => {
    if (selected === null || !preset.bands[selected]) return "";
    const b = preset.bands[selected];
    const pts = freqs.map((f) => bandDb(b, f, FS));
    return `${line(pts)}L${width},${y(0)}L0,${y(0)}Z`;
  });

  // ---- grid ---------------------------------------------------------------------------------
  const GRID_F = [20, 30, 40, 50, 60, 70, 80, 90, 100, 200, 300, 400, 500, 600, 700, 800, 900, 1000, 2000, 3000, 4000, 5000, 6000, 7000, 8000, 9000, 10000, 20000];
  const LABEL_F = [20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000];
  let dbStep = $derived(range <= 6 ? 2 : range <= 12 ? 3 : 6);
  let dbLines = $derived(Array.from({ length: Math.floor(range / dbStep) * 2 + 1 }, (_, i) => (i - Math.floor(range / dbStep)) * dbStep));
  const fLabel = (f: number) => (f >= 1000 ? `${num(f / 1000)}k` : `${f}`);

  // ---- nodes --------------------------------------------------------------------------------
  function nodeDb(b: EqBand): number {
    return hasGain(b.kind) ? b.gain : bandDb(b, b.freq, FS);
  }

  let drag: { i: number; dx: number; dy: number; moved: boolean } | null = null;
  let hover = $state<{ px: number; py: number } | null>(null);

  function local(e: PointerEvent | MouseEvent | WheelEvent) {
    const r = svg.getBoundingClientRect();
    return { px: e.clientX - r.left, py: e.clientY - r.top };
  }

  function update(i: number, patch: Partial<EqBand>, final: boolean) {
    const bands = preset.bands.map((b, j) => (j === i ? { ...b, ...patch } : b));
    onchange({ ...preset, bands }, final);
  }

  const roundF = (f: number) => (f < 100 ? Math.round(f * 10) / 10 : f < 1000 ? Math.round(f) : Math.round(f / 10) * 10);
  const roundG = (g: number) => Math.round(g * 10) / 10;

  function nodeDown(e: PointerEvent, i: number) {
    e.stopPropagation();
    onselect(i);
    const b = preset.bands[i];
    const { px, py } = local(e);
    drag = { i, dx: px - x(b.freq), dy: py - y(nodeDb(b)), moved: false };
    (e.currentTarget as Element).setPointerCapture(e.pointerId);
  }

  function nodeMove(e: PointerEvent) {
    if (!drag) return;
    const { px, py } = local(e);
    const b = preset.bands[drag.i];
    const patch: Partial<EqBand> = { freq: roundF(Math.min(22000, Math.max(10, fx(px - drag.dx)))) };
    if (hasGain(b.kind)) patch.gain = roundG(Math.min(30, Math.max(-30, dby(py - drag.dy))));
    drag.moved = true;
    update(drag.i, patch, false);
  }

  function nodeUp() {
    if (drag?.moved) onchange(preset, true);
    drag = null;
  }

  function onWheel(e: WheelEvent, i: number) {
    e.preventDefault();
    const b = preset.bands[i];
    const factor = Math.pow(1.06, e.deltaY < 0 ? 1 : -1);
    onselect(i);
    update(i, { q: Math.round(Math.min(50, Math.max(0.05, b.q * factor)) * 1000) / 1000 }, true);
  }

  function onKey(e: KeyboardEvent, i: number) {
    const b = preset.bands[i];
    const big = e.shiftKey;
    const oct = Math.pow(2, big ? 1 / 6 : 1 / 24);
    let patch: Partial<EqBand> | null = null;
    switch (e.key) {
      case "ArrowRight":
        patch = { freq: roundF(Math.min(22000, b.freq * oct)) };
        break;
      case "ArrowLeft":
        patch = { freq: roundF(Math.max(10, b.freq / oct)) };
        break;
      case "ArrowUp":
        patch = hasGain(b.kind) ? { gain: roundG(Math.min(30, b.gain + (big ? 1 : 0.1))) } : { q: Math.min(50, +(b.q * 1.1).toFixed(3)) };
        break;
      case "ArrowDown":
        patch = hasGain(b.kind) ? { gain: roundG(Math.max(-30, b.gain - (big ? 1 : 0.1))) } : { q: Math.max(0.05, +(b.q / 1.1).toFixed(3)) };
        break;
      case "PageUp":
      case "]":
        patch = { q: Math.min(50, +(b.q * 1.1).toFixed(3)) };
        break;
      case "PageDown":
      case "[":
        patch = { q: Math.max(0.05, +(b.q / 1.1).toFixed(3)) };
        break;
      case "Delete":
      case "Backspace":
        e.preventDefault();
        onselect(null);
        onchange({ ...preset, bands: preset.bands.filter((_, j) => j !== i) }, true);
        return;
    }
    if (patch) {
      e.preventDefault();
      update(i, patch, true);
    }
  }

  function addAt(e: MouseEvent) {
    const { px, py } = local(e);
    const band = newBand({ freq: roundF(fx(px)), gain: roundG(Math.min(range, Math.max(-range, dby(py)))), q: 1.41 });
    const bands = [...preset.bands, band];
    onchange({ ...preset, bands }, true);
    onselect(bands.length - 1);
  }

  function kindLabel(b: EqBand) {
    return t(`eq.kind.${b.kind}`);
  }
  function nodeLabel(b: EqBand, i: number) {
    const parts = [t("eq.band", { n: i + 1 }), kindLabel(b), `${num(b.freq, { maximumFractionDigits: 1 })} ${t("unit.hz")}`];
    if (hasGain(b.kind)) parts.push(`${b.gain > 0 ? "+" : ""}${num(b.gain)} ${t("unit.db")}`);
    parts.push(`Q ${num(b.q, { maximumFractionDigits: 2 })}`);
    return parts.join(", ");
  }

  let readout = $derived.by(() => {
    if (!hover || drag) return null;
    const f = fx(hover.px);
    const d = designer(preset, FS);
    const db = d.shape(f, 0) + gains[0];
    return { px: hover.px, f, db };
  });
</script>

<div class="curve" class:dim bind:clientWidth={width} bind:clientHeight={height} dir="ltr">
  <svg
    bind:this={svg}
    width={width}
    height={height}
    role="application"
    aria-label={t("eq.editorLabel")}
    aria-describedby="eq-curve-help"
    ondblclick={addAt}
    onpointermove={(e) => (hover = local(e))}
    onpointerleave={() => (hover = null)}
    onpointerdown={() => onselect(null)}
  >
    <defs>
      <linearGradient id="eq-fill" x1="0" x2="0" y1="0" y2="1">
        <stop offset="0" stop-color="var(--violet)" stop-opacity="0.28" />
        <stop offset="0.5" stop-color="var(--violet)" stop-opacity="0.04" />
        <stop offset="1" stop-color="var(--violet)" stop-opacity="0.28" />
      </linearGradient>
    </defs>

    <!-- grid -->
    {#each GRID_F as f (f)}
      <line class="grid" class:decade={f === 100 || f === 1000 || f === 10000} x1={x(f)} x2={x(f)} y1={PAD_T} y2={height - PAD_B} />
    {/each}
    {#each dbLines as db (db)}
      <line class="grid" class:zero={db === 0} x1="0" x2={width} y1={y(db)} y2={y(db)} />
      {#if db !== 0 && db % (dbStep * 2) === 0}
        <text class="tick" x="4" y={y(db) - 3}>{db > 0 ? "+" : "−"}{Math.abs(db)}</text>
      {/if}
    {/each}
    {#each LABEL_F as f (f)}
      <text class="tick f" x={x(f)} y={height - 6} text-anchor={f === 20 ? "start" : f === 20000 ? "end" : "middle"}>{fLabel(f)}</text>
    {/each}

    <!-- where the EQ + preamp would go past full scale -->
    {#if overMask}<path class="over" d={overMask} />{/if}

    <!-- preamp -->
    {#if Math.abs(preamp) > 0.05}
      <line class="preamp" x1="0" x2={width} y1={y(preamp)} y2={y(preamp)} />
      <text class="tick pre" x={width - 6} y={y(preamp) + 12} text-anchor="end">{t("eq.preampTick", { db: num(preamp) })}</text>
    {/if}

    {#if selectedPath}<path class="sel-area" d={selectedPath} />{/if}
    <path class="area" d={area} />
    {#if pathR}<path class="line r" d={pathR} />{/if}
    <path class="line" d={pathL} />

    {#if readout}
      <line class="cross" x1={readout.px} x2={readout.px} y1={PAD_T} y2={height - PAD_B} />
      <text class="readout" x={readout.px + (readout.px > width - 150 ? -8 : 8)} y={PAD_T + 14} text-anchor={readout.px > width - 150 ? "end" : "start"}>
        <tspan>{readout.f >= 1000 ? `${num(readout.f / 1000, { maximumFractionDigits: 2 })} ${t("unit.khz")}` : `${num(readout.f, { maximumFractionDigits: 0 })} ${t("unit.hz")}`}</tspan>
        <tspan dx="10" class="rdb">{readout.db > 0.05 ? "+" : readout.db < -0.05 ? "−" : ""}{num(Math.abs(readout.db))} {t("unit.db")}</tspan>
      </text>
    {/if}

    <!-- band nodes -->
    {#each preset.bands as b, i (i)}
      {@const cx = x(Math.min(HI, Math.max(LO, b.freq)))}
      {@const cy = y(nodeDb(b))}
      <g
        class="node"
        class:sel={selected === i}
        class:off={!b.enabled}
        class:lr={b.channels !== "both"}
        role="button"
        tabindex="0"
        aria-label={nodeLabel(b, i)}
        aria-pressed={selected === i}
        transform={`translate(${cx},${cy})`}
        onpointerdown={(e) => nodeDown(e, i)}
        onpointermove={nodeMove}
        onpointerup={nodeUp}
        onpointercancel={nodeUp}
        onwheel={(e) => onWheel(e, i)}
        onkeydown={(e) => onKey(e, i)}
        onfocus={() => onselect(i)}
        ondblclick={(e) => e.stopPropagation()}
      >
        <circle class="hit" r="18" />
        <circle class="dot" r="9.5" />
        <text class="n" y="3.5" text-anchor="middle">{b.channels === "left" ? "L" : b.channels === "right" ? "R" : i + 1}</text>
      </g>
    {/each}
  </svg>
  <p id="eq-curve-help" class="sr">{t("eq.editorHelp")}</p>
</div>

<style>
  .curve {
    position: relative;
    width: 100%;
    height: 100%;
    min-height: 200px;
    border-radius: var(--r-lg);
    background:
      radial-gradient(120% 80% at 50% 50%, rgba(140, 122, 230, 0.05), transparent 70%),
      var(--slate-900);
    border: 1px solid var(--slate-600);
    overflow: hidden;
    touch-action: none;
  }
  svg {
    display: block;
    cursor: crosshair;
  }
  .grid {
    stroke: var(--slate-700);
    stroke-width: 1;
  }
  .grid.decade {
    stroke: var(--slate-600);
  }
  .grid.zero {
    stroke: var(--slate-500);
  }
  .tick {
    fill: var(--mist-dim);
    font-family: var(--font-display);
    font-size: 10.5px;
    font-variant-numeric: tabular-nums;
  }
  .tick.pre {
    fill: var(--mist);
  }
  .preamp {
    stroke: var(--mist);
    stroke-width: 1;
    stroke-dasharray: 2 4;
  }
  .over {
    stroke: var(--alarm);
    stroke-width: 2;
    opacity: 0.18;
  }
  .area {
    fill: url(#eq-fill);
  }
  .sel-area {
    fill: rgba(232, 237, 234, 0.07);
    stroke: rgba(232, 237, 234, 0.35);
    stroke-width: 1;
  }
  .line {
    fill: none;
    stroke: var(--violet);
    stroke-width: 2.25;
    stroke-linejoin: round;
  }
  .line.r {
    stroke-dasharray: 5 4;
    stroke-width: 1.75;
    opacity: 0.85;
  }
  .cross {
    stroke: var(--slate-500);
    stroke-dasharray: 1 3;
  }
  .readout {
    fill: var(--porcelain);
    font-family: var(--font-display);
    font-size: 12px;
    font-variant-numeric: tabular-nums;
    paint-order: stroke;
    stroke: var(--ink);
    stroke-width: 4px;
  }
  .readout .rdb {
    fill: var(--violet);
  }
  .node {
    cursor: grab;
    outline: none;
  }
  .node:active {
    cursor: grabbing;
  }
  .hit {
    fill: transparent;
  }
  .dot {
    fill: var(--ink-soft);
    stroke: var(--porcelain);
    stroke-width: 1.5;
    transition: r 80ms ease;
  }
  .node .n {
    fill: var(--porcelain);
    font-family: var(--font-display);
    font-size: 10px;
    font-weight: 600;
    pointer-events: none;
  }
  .node:hover .dot {
    r: 11;
  }
  .node.sel .dot {
    fill: var(--violet);
    stroke: var(--porcelain);
  }
  .node.sel .n {
    fill: var(--ink);
  }
  .node.off .dot {
    stroke: var(--mist-dim);
    stroke-dasharray: 2 2;
  }
  .node.off .n {
    fill: var(--mist-dim);
  }
  .node:focus-visible .dot {
    stroke: var(--porcelain);
    stroke-width: 2.5;
  }
  .dim .line,
  .dim .area,
  .dim .node {
    opacity: 0.35;
  }
  .sr {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip: rect(0 0 0 0);
    margin: -1px;
  }
  @media (prefers-reduced-motion: reduce) {
    .dot {
      transition: none;
    }
  }
</style>
