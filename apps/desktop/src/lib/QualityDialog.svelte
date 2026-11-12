<script lang="ts">
  import { resolution, type LibTrack, type QualityReport } from "./api";
  import { num, t } from "./i18n.svelte";
  import { SUSPECT, verdictDetail, verdictLabel } from "./quality";

  let { track, report, onclose }: { track: LibTrack; report: QualityReport; onclose: () => void } = $props();

  // ---- chart geometry -------------------------------------------------------------------------
  // Drawn at the width it gets, so its labels stay readable on a phone.
  let box = $state(560);
  let W = $derived(Math.max(280, box));
  let H = $derived(Math.round(Math.min(210, Math.max(160, W * 0.4))));
  const PAD = { l: 38, r: 10, t: 10, b: 24 };
  let pw = $derived(W - PAD.l - PAD.r);
  let ph = $derived(H - PAD.t - PAD.b);

  let nyquist = $derived(report.sampleRate / 2);
  let spec = $derived(report.spectrum);
  // 120 dB of range under the loudest point, on 10 dB lines.
  let top = $derived(Math.ceil(Math.max(-200, ...spec) / 10) * 10);
  let bottom = $derived(top - 120);
  const x = (hz: number) => PAD.l + (hz / nyquist) * pw;
  const y = (db: number) => PAD.t + ((top - Math.max(bottom, Math.min(top, db))) / (top - bottom)) * ph;

  let line = $derived(spec.map((d, i) => `${i ? "L" : "M"}${x(((i + 0.5) / spec.length) * nyquist).toFixed(1)},${y(d).toFixed(1)}`).join(""));
  let area = $derived(`${line}L${x(nyquist).toFixed(1)},${y(bottom)}L${x(0)},${y(bottom)}Z`);
  // About one label per 60 px.
  let xStep = $derived([2_000, 5_000, 10_000, 20_000, 50_000].find((s) => (nyquist / s) * 60 <= pw) ?? 50_000);
  let xTicks = $derived(Array.from({ length: Math.floor(nyquist / xStep) + 1 }, (_, i) => i * xStep));
  let yTicks = $derived(Array.from({ length: 5 }, (_, i) => top - i * 30));

  let suspect = $derived(SUSPECT.includes(report.verdict.kind));
  // The CD's limit is worth a line only where the file claims to go beyond it.
  let showCd = $derived(nyquist > 24_000);
  // Drawn where the music stops at a wall, or where an upsampled file's mirror image begins.
  let cutoff = $derived(report.cutoffHz != null && (report.wall || report.verdict.kind === "upsampled") ? report.cutoffHz : null);

  const khz = (hz: number) => num(hz / 1000, { maximumFractionDigits: 1 });
  let format = $derived(
    track.lossless && track.bits ? `${track.format} ${resolution(track.bits, track.sampleRate)}` : `${track.format} ${khz(track.sampleRate)} ${t("unit.khz")}`,
  );
</script>

<svelte:window onkeydown={(e) => e.key === "Escape" && onclose()} />

<div class="bg" role="presentation" onclick={(e) => e.target === e.currentTarget && onclose()}>
  <div class="dialog" role="dialog" aria-modal="true" aria-labelledby="q-title">
    <header>
      <div>
        <p class="eyebrow">{t("q.dialog")}</p>
        <h3 id="q-title">{track.title}</h3>
        <p class="sub">{format}</p>
      </div>
      <button class="close" onclick={onclose} aria-label={t("q.close")}>
        <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true"><path d="M2 2l8 8M10 2l-8 8" stroke="currentColor" stroke-width="1.6" /></svg>
      </button>
    </header>

    <div class="verdict" class:suspect class:open={report.verdict.kind === "inconclusive"}>
      <strong>{verdictLabel(report.verdict.kind)}</strong>
      <p>{verdictDetail(report)}</p>
    </div>

    {#if spec.length}
      <div class="chart-box" bind:clientWidth={box}>
      <svg class="chart" viewBox={`0 0 ${W} ${H}`} role="img" aria-label={t("q.chart")}>
        <defs>
          <linearGradient id="q-fill" x1="0" y1="0" x2="0" y2="1">
            <stop offset="0" stop-color="var(--brass)" stop-opacity="0.45" />
            <stop offset="1" stop-color="var(--brass)" stop-opacity="0.02" />
          </linearGradient>
        </defs>
        {#each yTicks as d (d)}
          <line class="grid" x1={PAD.l} x2={W - PAD.r} y1={y(d)} y2={y(d)} />
          <text class="tick" x={PAD.l - 6} y={y(d) + 3} text-anchor="end">{d}</text>
        {/each}
        {#each xTicks as hz (hz)}
          <text class="tick" x={x(hz)} y={H - 6} text-anchor={hz === 0 ? "start" : "middle"}>{hz / 1000}</text>
        {/each}
        <text class="tick" x={W - PAD.r} y={H - 6} text-anchor="end">{t("unit.khz")}</text>
        <path d={area} fill="url(#q-fill)" />
        <path d={line} class="trace" />
        {#if showCd}
          <line class="cd" x1={x(22_050)} x2={x(22_050)} y1={PAD.t} y2={PAD.t + ph} />
          <text class="cd-label" x={x(22_050) + 4} y={PAD.t + 10}>{t("q.cd")}</text>
        {/if}
        {#if cutoff != null}
          <line class="cut" class:suspect x1={x(cutoff)} x2={x(cutoff)} y1={PAD.t} y2={PAD.t + ph} />
          <text class="cut-label" class:suspect x={x(cutoff) + 4} y={PAD.t + ph - 6}>{khz(cutoff)} {t("unit.khz")}</text>
        {/if}
      </svg>
      </div>
    {/if}

    <dl class="facts">
      {#if report.verdict.kind !== "dsd"}
        <div>
          <dt>{t("q.range")}</dt>
          <dd>{report.cutoffHz == null ? t("q.full") : t("q.upTo", { khz: khz(report.cutoffHz) })}</dd>
        </div>
      {/if}
      {#if report.effectiveBits != null && report.bits != null}
        <div>
          <dt>{t("q.bitsUsed")}</dt>
          <dd class:bad={report.effectiveBits < report.bits}>{t("q.bitsOf", { n: report.effectiveBits, of: report.bits })}</dd>
        </div>
      {/if}
      {#if report.verdict.kind !== "dsd"}
        <div>
          <dt>{t("q.peak")}</dt>
          <dd>{num(report.peakDb, { maximumFractionDigits: 1 })} dBFS</dd>
        </div>
        <div>
          <dt>{t("q.clipping")}</dt>
          <dd class:bad={report.clipped > 0}>{report.clipped > 0 ? num(report.clipped) : t("q.none")}</dd>
        </div>
      {/if}
    </dl>
    <p class="note">{t("q.note")}</p>
  </div>
</div>

<style>
  .bg {
    position: fixed;
    inset: 0;
    z-index: 60;
    display: grid;
    place-items: center;
    padding: var(--safe-top, env(safe-area-inset-top)) 16px var(--safe-bottom, env(safe-area-inset-bottom));
    background: rgba(5, 12, 15, 0.6);
  }
  .dialog {
    width: min(620px, 100%);
    max-height: calc(100dvh - 32px);
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 14px;
    padding: 18px 20px 16px;
    border-radius: 12px;
    background: var(--slate-800);
    border: 1px solid var(--slate-500);
    box-shadow: 0 24px 60px rgba(0, 0, 0, 0.45);
  }
  header {
    display: flex;
    justify-content: space-between;
    align-items: start;
    gap: 12px;
  }
  .eyebrow {
    margin: 0;
    font-size: var(--t-xs);
    color: var(--mist);
  }
  h3 {
    margin: 2px 0 0;
    font-family: var(--font-display);
    font-size: var(--t-lg);
    font-weight: 600;
  }
  .sub {
    margin: 2px 0 0;
    font-size: var(--t-sm);
    color: var(--mist);
  }
  .close {
    padding: 8px;
    border-radius: var(--r-sm);
    color: var(--mist);
  }
  .close:hover {
    color: var(--porcelain);
    background: var(--slate-700);
  }

  .verdict {
    padding: 10px 12px;
    border-radius: var(--r-md);
    border-inline-start: 3px solid var(--brass);
    background: var(--slate-700);
  }
  .verdict.suspect {
    border-color: var(--alarm);
  }
  .verdict.open {
    border-color: var(--mist);
  }
  .verdict strong {
    font-size: var(--t-md);
  }
  .verdict p {
    margin: 4px 0 0;
    font-size: var(--t-sm);
    color: var(--mist);
    line-height: 1.45;
  }

  .chart-box {
    width: 100%;
  }
  .chart {
    width: 100%;
    height: auto;
    display: block;
  }
  .grid {
    stroke: var(--slate-600);
    stroke-width: 1;
  }
  .tick {
    fill: var(--mist-dim);
    font-size: 10px;
    font-variant-numeric: tabular-nums;
  }
  .trace {
    fill: none;
    stroke: var(--brass);
    stroke-width: 1.3;
    stroke-linejoin: round;
  }
  .cd {
    stroke: var(--mist);
    stroke-dasharray: 3 4;
  }
  .cd-label {
    fill: var(--mist);
    font-size: 10px;
  }
  .cut {
    stroke: var(--mist);
    stroke-width: 1.5;
  }
  .cut.suspect {
    stroke: var(--alarm);
  }
  .cut-label {
    fill: var(--mist);
    font-size: 11px;
    font-weight: 600;
  }
  .cut-label.suspect {
    fill: var(--alarm);
  }

  .facts {
    margin: 0;
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(120px, 1fr));
    gap: 10px 16px;
  }
  .facts dt {
    font-size: var(--t-xs);
    color: var(--mist);
  }
  .facts dd {
    margin: 2px 0 0;
    font-size: var(--t-sm);
    font-variant-numeric: tabular-nums;
  }
  .facts dd.bad {
    color: var(--alarm);
  }
  .note {
    margin: 0;
    font-size: var(--t-xs);
    color: var(--mist-dim);
  }
</style>
