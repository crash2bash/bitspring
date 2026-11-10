// The equaliser's response, computed here so the curve follows a dragged band at frame rate.
// Mirrors crates/engine/src/dsp/eq.rs (RBJ cookbook biquads, Butterworth cascades, the curve's
// log-frequency interpolation, auto preamp, balance) — the engine's tests pin the numbers.

import type { EqBand, EqPreset, FilterKind } from "./api";

interface Biquad {
  b0: number;
  b1: number;
  b2: number;
  a1: number;
  a2: number;
}

const GAIN_LIMIT = 30;

export const hasGain = (k: FilterKind) => k === "peak" || k === "lowShelf" || k === "highShelf";
export const hasSlope = (k: FilterKind) => k === "lowPass" || k === "highPass";

function norm(b0: number, b1: number, b2: number, a0: number, a1: number, a2: number): Biquad {
  return { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 };
}

function rbj(kind: FilterKind, f: number, gainDb: number, q: number, fs: number): Biquad {
  const a = Math.pow(10, gainDb / 40);
  const w0 = (2 * Math.PI * f) / fs;
  const sin = Math.sin(w0);
  const cos = Math.cos(w0);
  const alpha = sin / (2 * q);
  switch (kind) {
    case "peak":
      return norm(1 + alpha * a, -2 * cos, 1 - alpha * a, 1 + alpha / a, -2 * cos, 1 - alpha / a);
    case "lowShelf": {
      const s = 2 * Math.sqrt(a) * alpha;
      return norm(a * (a + 1 - (a - 1) * cos + s), 2 * a * (a - 1 - (a + 1) * cos), a * (a + 1 - (a - 1) * cos - s), a + 1 + (a - 1) * cos + s, -2 * (a - 1 + (a + 1) * cos), a + 1 + (a - 1) * cos - s);
    }
    case "highShelf": {
      const s = 2 * Math.sqrt(a) * alpha;
      return norm(a * (a + 1 + (a - 1) * cos + s), -2 * a * (a - 1 + (a + 1) * cos), a * (a + 1 + (a - 1) * cos - s), a + 1 - (a - 1) * cos + s, 2 * (a - 1 - (a + 1) * cos), a + 1 - (a - 1) * cos - s);
    }
    case "lowPass":
      return norm((1 - cos) / 2, 1 - cos, (1 - cos) / 2, 1 + alpha, -2 * cos, 1 - alpha);
    case "highPass":
      return norm((1 + cos) / 2, -(1 + cos), (1 + cos) / 2, 1 + alpha, -2 * cos, 1 - alpha);
    case "bandPass":
      return norm(alpha, 0, -alpha, 1 + alpha, -2 * cos, 1 - alpha);
    case "notch":
      return norm(1, -2 * cos, 1, 1 + alpha, -2 * cos, 1 - alpha);
    case "allPass":
      return norm(1 - alpha, -2 * cos, 1 + alpha, 1 + alpha, -2 * cos, 1 - alpha);
  }
}

function firstOrder(high: boolean, f: number, fs: number): Biquad {
  const k = Math.tan((Math.PI * f) / fs);
  const a0 = k + 1;
  const a1 = (k - 1) / a0;
  return high ? { b0: 1 / a0, b1: -1 / a0, b2: 0, a1, a2: 0 } : { b0: k / a0, b1: k / a0, b2: 0, a1, a2: 0 };
}

function magnitudeDb(s: Biquad, w: number): number {
  const c1 = Math.cos(w), s1 = Math.sin(w), c2 = Math.cos(2 * w), s2 = Math.sin(2 * w);
  const nr = s.b0 + s.b1 * c1 + s.b2 * c2;
  const ni = -(s.b1 * s1 + s.b2 * s2);
  const dr = 1 + s.a1 * c1 + s.a2 * c2;
  const di = -(s.a1 * s1 + s.a2 * s2);
  return 10 * Math.log10(Math.max(1e-30, (nr * nr + ni * ni) / (dr * dr + di * di)));
}

/** The biquads that realise `b` at `fs`. */
export function sections(b: EqBand, fs: number): Biquad[] {
  if (!b.enabled) return [];
  const limit = fs * 0.49;
  const q = Math.min(50, Math.max(0.05, b.q));
  const gain = Math.min(GAIN_LIMIT, Math.max(-GAIN_LIMIT, b.gain));
  const f = b.freq;
  if ((b.kind === "peak" || b.kind === "notch" || b.kind === "allPass" || b.kind === "lowPass") && f >= limit) return [];
  if (b.kind === "bandPass" && f >= limit) return [rbj("bandPass", limit, 0, q, fs)];
  if (b.kind === "highShelf" && f >= fs / 2) return [];
  if (hasGain(b.kind) && gain === 0) return [];
  if (hasSlope(b.kind)) {
    const high = b.kind === "highPass";
    const fc = Math.min(f, limit);
    const order = Math.floor(Math.min(48, Math.max(6, b.slope)) / 6);
    if (order === 2) return [rbj(b.kind, fc, 0, q, fs)];
    const out: Biquad[] = [];
    if (order % 2 === 1) out.push(firstOrder(high, fc, fs));
    for (let k = 1; k <= Math.floor(order / 2); k++) {
      const theta = order % 2 === 0 ? ((2 * k - 1) * Math.PI) / (2 * order) : (k * Math.PI) / order;
      out.push(rbj(b.kind, fc, 0, 1 / (2 * Math.cos(theta)), fs));
    }
    return out;
  }
  return [rbj(b.kind, Math.min(f, limit), gain, q, fs)];
}

const covers = (b: EqBand, ch: number) => b.channels === "both" || (b.channels === "left" ? ch === 0 : ch === 1);

export function curveDb(p: EqPreset, f: number): number {
  const c = p.curve;
  if (!c.length) return 0;
  if (f <= c[0][0]) return c[0][1];
  if (f >= c[c.length - 1][0]) return c[c.length - 1][1];
  let i = 1;
  while (c[i][0] <= f) i++;
  const [f0, g0] = c[i - 1];
  const [f1, g1] = c[i];
  return g0 + ((g1 - g0) * Math.log(f / f0)) / Math.log(f1 / f0);
}

/** Response of one band alone (dB). */
export function bandDb(b: EqBand, f: number, fs: number): number {
  const w = (2 * Math.PI * f) / fs;
  return sections({ ...b, enabled: true }, fs).reduce((sum, s) => sum + magnitudeDb(s, w), 0);
}

/** Precomputed sections per channel, for drawing many points fast. */
export function designer(p: EqPreset, fs: number) {
  const per = [0, 1].map((ch) => p.bands.filter((b) => b.enabled && covers(b, ch)).flatMap((b) => sections(b, fs)));
  const shape = (f: number, ch: number) => {
    const w = (2 * Math.PI * f) / fs;
    let db = curveDb(p, f);
    for (const s of per[ch]) db += magnitudeDb(s, w);
    return db;
  };
  return { shape };
}

export function maxBoostDb(p: EqPreset, fs = 48000): number {
  const top = Math.min(fs / 2, 22000);
  const freqs: number[] = [];
  for (let i = 0; i <= 480; i++) freqs.push(10 * Math.pow(top / 10, i / 480));
  for (const b of p.bands) if (b.enabled && b.freq < top) freqs.push(b.freq);
  for (const [f] of p.curve) if (f < top) freqs.push(f);
  const d = designer(p, fs);
  const channels = p.bands.some((b) => b.channels !== "both") ? 2 : 1;
  let max = 0;
  for (let ch = 0; ch < channels; ch++) for (const f of freqs) max = Math.max(max, d.shape(f, ch));
  return max;
}

export function effectivePreamp(p: EqPreset, fs = 48000): number {
  return p.autoPreamp ? -maxBoostDb(p, fs) : p.preamp;
}

export function channelGainDb(p: EqPreset, ch: number, fs = 48000): number {
  const pre = effectivePreamp(p, fs);
  return ch === 0 ? pre - Math.max(0, p.balance) : pre - Math.max(0, -p.balance);
}

/** Whole response per channel at the given frequencies. */
export function response(p: EqPreset, freqs: number[], fs = 48000): [number[], number[]] {
  const d = designer(p, fs);
  const g = [channelGainDb(p, 0, fs), channelGainDb(p, 1, fs)];
  return [0, 1].map((ch) => freqs.map((f) => g[ch] + d.shape(f, ch))) as [number[], number[]];
}

/** True when left and right differ (per-channel bands or balance). */
export function isSplit(p: EqPreset): boolean {
  return p.balance !== 0 || p.bands.some((b) => b.enabled && b.channels !== "both");
}

export const newBand = (over: Partial<EqBand> = {}): EqBand => ({
  enabled: true,
  kind: "peak",
  freq: 1000,
  gain: 0,
  q: 1,
  slope: 12,
  channels: "both",
  ...over,
});

export const emptyPreset = (name: string): EqPreset => ({
  name,
  preamp: 0,
  autoPreamp: true,
  balance: 0,
  bands: [],
  curve: [],
  phase: "minimum",
});

/** Starting points for a new preset. */
export const TEMPLATES: { key: string; make: (name: string) => EqPreset }[] = [
  { key: "empty", make: (name) => emptyPreset(name) },
  {
    key: "graphic10",
    make: (name) => ({
      ...emptyPreset(name),
      bands: [31.5, 63, 125, 250, 500, 1000, 2000, 4000, 8000, 16000].map((f) => newBand({ freq: f, q: 1.41 })),
    }),
  },
  {
    key: "graphic31",
    make: (name) => ({
      ...emptyPreset(name),
      bands: [20, 25, 31.5, 40, 50, 63, 80, 100, 125, 160, 200, 250, 315, 400, 500, 630, 800, 1000, 1250, 1600, 2000, 2500, 3150, 4000, 5000, 6300, 8000, 10000, 12500, 16000, 20000].map((f) =>
        newBand({ freq: f, q: 4.32 }),
      ),
    }),
  },
  {
    key: "tone",
    make: (name) => ({
      ...emptyPreset(name),
      bands: [newBand({ kind: "lowShelf", freq: 105, q: 0.71 }), newBand({ freq: 1000, q: 0.7 }), newBand({ kind: "highShelf", freq: 10000, q: 0.71 })],
    }),
  },
  {
    key: "headphone",
    make: (name) => ({
      ...emptyPreset(name),
      bands: [
        newBand({ kind: "highPass", freq: 15, q: 0.71, slope: 12 }),
        newBand({ kind: "lowShelf", freq: 105, q: 0.71 }),
        newBand({ freq: 200, q: 1 }),
        newBand({ freq: 1000, q: 1 }),
        newBand({ freq: 3000, q: 2 }),
        newBand({ freq: 6000, q: 3 }),
        newBand({ kind: "highShelf", freq: 10000, q: 0.71 }),
      ],
    }),
  },
];

/** Log-spaced frequencies from `lo` to `hi`. */
export function logspace(lo: number, hi: number, n: number): number[] {
  return Array.from({ length: n }, (_, i) => lo * Math.pow(hi / lo, i / (n - 1)));
}

/** "1.2k", "350", "31.5". */
export function freqLabel(f: number): string {
  if (f >= 10000) return `${+(f / 1000).toFixed(1)}k`;
  if (f >= 1000) return `${+(f / 1000).toFixed(2)}k`;
  if (f >= 100) return `${Math.round(f)}`;
  return `${+f.toFixed(1)}`;
}
