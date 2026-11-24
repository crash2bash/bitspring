// Browser-only stand-in for the Rust backend, so the layout can be built and reviewed
// without Tauri. Never used inside the app.
// Device and preset names are generic on purpose: these screens end up in screenshots, and
// BitSpring works with any DAC and any headphones.
import { isIOS } from "./api";
import { MOCK_COVERS } from "./mockCovers";
import type { Album, Device, DeviceEq, DspSettings, EqPreset, EqState, LibTrack, Lyrics, Mark, NowPlaying, Signal, Status, StoredPreset } from "./api";

// ---- library ----------------------------------------------------------------------------------
/** The engine's name for untagged music (api.ts UNKNOWN_ARTIST; not imported: api.ts imports this file). */
const UNKNOWN_ARTIST = "Unknown artist";
/** ?store: look like an App Store / Microsoft Store build (no streaming, no self-updating). */
const storeBuild = new URLSearchParams(location.search).has("store");
let mockCrash = new URLSearchParams(location.search).has("crash");
/** ?stepdown: a few seconds into a track with a long upsampling filter, the pretend processor falls behind and the filter steps down once. */
let mockStepDown = new URLSearchParams(location.search).has("stepdown");
let mockStep: { seq: number; filter: DspSettings["upsampleFilter"]; taps: number | null; deviceKey: string } | null = null;
let mockStepSeq = 0;
// DSD output (Enhancers → DSD output) runs whenever it is chosen, on the USB DAC in exclusive
// mode, without the device's DoP switch. These give its other states:
/** ?dsdnote=exclusive|device|volume|network|rate|refused: it doesn't run, for that reason. */
const mockDsdNote = new URLSearchParams(location.search).get("dsdnote");
/** ?dsdlimit: the device takes DoP up to DSD128 only (DSD256 asked for runs as DSD128). */
const mockDsdLimit = new URLSearchParams(location.search).has("dsdlimit") ? 128 : 256;
/** ?dsdstep: a few seconds into a track, the pretend processor falls behind and the DSD output
 *  steps down once: 256 → 128 → 64 at the same modulator, then 64 with the 5th order, then off. */
let mockDsdStepDown = new URLSearchParams(location.search).has("dsdstep");
let mockDsdStep: { seq: number; dsdOut: number | null; order: number | null; deviceKey: string } | null = null;
let mockDsdStepSeq = 0;
/** The DoP word rate that carries each DSD rate. */
const DOP_WIRE: Record<number, number> = { 64: 176400, 128: 352800, 256: 705600 };
const LIB: [string, string, string | null, string | null, string, number, number | null, string | null, string[]][] = [
  ["Ada Marlow", "Paper Moons", "2017", "Folk", "ALAC", 48000, 24, "#6b5d2e", ["Kite Strings", "Harbour", "Paper Moons"]],
  ["Echo Field", "Signal Path", "2020", "Electronic", "MP3", 44100, null, "#16233a", ["Carrier", "Sideband", "Noise Floor"]],
  ["Halvorsen Quartet", "Strings in Winter", "2021", "Classical", "FLAC", 192000, 24, "#2e3b6b", ["I. Allegro", "II. Adagio", "III. Scherzo", "IV. Finale"]],
  ["Northern Lights Trio", "Quiet Hours", "2019", "Jazz", "FLAC", 96000, 24, "#2b4a5c", ["Morning Glass", "Slow Current", "Blue Hour", "Afterglow"]],
  ["Northern Lights Trio", "City Rain", "2022", "Jazz", "FLAC", 44100, 16, "#5c3a2b", ["Neon Puddles", "Last Tram", "Umbrella Song"]],
  [UNKNOWN_ARTIST, "Untitled Sketches", null, null, "WAV", 44100, 16, null, ["01 sketch one", "02 sketch two"]],
  ["Halvorsen Quartet", "Surround Sessions 5.1", "2023", "Classical", "FLAC", 48000, 24, "#3b2e6b", ["Room Tone", "Canon in Six"]],
];
// With ?android or ?ios the page plays a phone, whose library has folders of its own.
const phone = new URLSearchParams(location.search);
const libRoot = phone.has("android") ? "/storage/emulated/0/Music" : phone.has("ios") ? "/var/mobile/Containers/Data/Application/1/Documents" : "/Music";
const libFixed = phone.has("android") ? [libRoot, "/storage/emulated/0/Android/data/dev.local.hifi/files/Music"] : phone.has("ios") ? [libRoot] : [];
const libTracks: LibTrack[] = [];
const libAlbums: Album[] = LIB.map(([artist, title, year, genre, format, rate, bits, color, names], ai) => {
  const id = `a${ai}`;
  const idx = names.map((name, n) => {
    libTracks.push({
      path: `${libRoot}/${artist}/${title}/${String(n + 1).padStart(2, "0")} ${name}.${format.toLowerCase()}`,
      title: name, artist: artist === UNKNOWN_ARTIST ? null : artist, albumArtist: null, album: title, genre, year,
      trackNumber: n + 1, discNumber: null, duration: 27 + n * 7 + ai * 31, sampleRate: rate, bits, channels: 2, format,
      lossless: format !== "MP3", hasCover: !!color, size: 0, mtime: 1700000000 + ai * 1000, albumId: id,
      hiRes: format !== "MP3" && (rate > 48000 || (bits ?? 0) > 16),
    });
    return libTracks.length - 1;
  });
  return {
    id, title, artist, year, genre, tracks: idx, duration: idx.reduce((s, i) => s + libTracks[i].duration, 0),
    maxRate: rate, maxBits: bits, format, hiRes: format !== "MP3" && (rate > 48000 || (bits ?? 0) > 16),
    hasCover: !!color, trackCount: idx.length,
  };
});
const coverColor: Record<string, string> = Object.fromEntries(LIB.map((a, i) => [`a${i}`, a[7] ?? "#1f3039"]));

const coverCache = new Map<string, string>();
export function mockCover(id: string): string {
  const art = MOCK_COVERS[id];
  if (art) {
    if (!coverCache.has(id)) coverCache.set(id, art());
    return coverCache.get(id)!;
  }
  const c = coverColor[id] ?? "#1f3039";
  return `data:image/svg+xml;utf8,${encodeURIComponent(`<svg xmlns="http://www.w3.org/2000/svg" width="300" height="300"><rect width="300" height="300" fill="${c}"/><circle cx="150" cy="150" r="90" fill="none" stroke="rgba(255,255,255,.25)" stroke-width="2"/></svg>`)}`;
}

const mockChecks: Record<string, unknown> = {};
const mockDynamics: Record<string, { dr: number; lra: number; lufs: number; peakDb: number }> = {};
let mockDynamicsRev = 1;
const mockRips: Record<string, unknown> = {};
// A few marks to start with, so the hearts and stars have something to show.
const mockMarks: { tracks: Record<string, Mark>; albums: Record<string, { at: number }> } = {
  tracks: Object.fromEntries(libTracks.filter((_, i) => i % 7 === 2).map((t, i) => [t.path, { fav: i % 3 !== 1, rating: [5, 4, 0, 3][i % 4], at: 1_790_000_000 + i }])),
  albums: Object.fromEntries(libAlbums.filter((_, i) => i % 4 === 1).map((a, i) => [a.id, { at: 1_790_000_000 + i }])),
};
let mockMarksRev = 1;
function mockMark(path: string, m: Mark) {
  if (m.fav || m.rating) mockMarks.tracks[path] = m;
  else delete mockMarks.tracks[path];
}
let mockRipStart = 0;
let mockRipTotal = 0;
let mockAbx: { total: number; answers: { xIsA: boolean; saidA: boolean }[]; xIsA: boolean } | null = null;

let libFolders = libFixed.length ? [...libFixed] : [libRoot];
let libSkipped: string[] = [];
/** The folder a mock track is in. */
const libDir = (t: LibTrack) => t.path.slice(0, t.path.lastIndexOf("/"));
const libUnder = (dir: string, parent: string) => dir === parent || dir.startsWith(parent + "/");

let mockVolume = 35;
let mockRemote = false;
const devices: Device[] = [
  { uid: "q7", key: "q7", name: "USB DAC", transport: "Usb", channels: 2, rates: [44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000, 705600, 768000], nominalRate: 44100, isDefault: true, bitPerfectCapable: true, maxDop: 256, network: false, model: null, protocol: null },
  { uid: "spk", key: "spk", name: "Built-in Speakers", transport: "BuiltIn", channels: 2, rates: [44100, 48000, 88200, 96000], nominalRate: 44100, isDefault: false, bitPerfectCapable: true, maxDop: null, network: false, model: null, protocol: null },
  { uid: "air", key: "air", name: "Bluetooth Headphones", transport: "Bluetooth", channels: 2, rates: [24000, 48000], nominalRate: 48000, isDefault: false, bitPerfectCapable: false, maxDop: null, network: false, model: null, protocol: null },
  { uid: "upnp:wiim", key: "upnp:wiim", name: "Living Room", transport: "Network", channels: 2, rates: [44100, 48000, 88200, 96000, 176400, 192000], nominalRate: 0, isDefault: false, bitPerfectCapable: true, maxDop: null, network: true, model: "WiiM Pro Plus", protocol: "UPnP" },
  { uid: "upnp:sonos", key: "upnp:sonos", name: "Kitchen", transport: "Network", channels: 2, rates: [44100, 48000], nominalRate: 0, isDefault: false, bitPerfectCapable: true, maxDop: null, network: true, model: "Sonos Era 100", protocol: "UPnP" },
  { uid: "cast:shield", key: "cast:shield", name: "Living Room TV", transport: "Network", channels: 2, rates: [44100, 48000, 88200, 96000], nominalRate: 0, isDefault: false, bitPerfectCapable: true, maxDop: null, network: true, model: "SHIELD Android TV", protocol: "Chromecast" },
];

/** Like the backend: each output's processing by settings key; one with none has the defaults. */
const DSP_DEFAULTS: DspSettings = { upsampleKhz: null, upsampleFilter: null, upsampleTaps: 65536, crossfeed: null, convolution: null, headroom: true, restoreHighs: false, level: null, peakHeadroom: true, dsdOut: null, dsdOrder: 7 };
const dspByKey: Record<string, DspSettings> = {};
function dspOf(dev: Device | null): DspSettings {
  return (dev && dspByKey[dev.key]) || DSP_DEFAULTS;
}
/** The pretend library analysis: when it started (null: not running) and how much was done before. */
let measuring: number | null = null;
let measured = 0;
let tagging: number | null = null;
let tagged = 0;
let tagLast: [number, number] | null = null;
/** What volume levelling does to the pretend track. */
function mockLevel(dsp: DspSettings): Pick<Signal, "levelDb" | "levelBasis" | "levelLimited"> {
  if (dsp.level) return { levelDb: dsp.level === "album" ? -6.1 : -7.3, levelBasis: dsp.level, levelLimited: false };
  const reshapes = !!(dsp.upsampleKhz || dsp.dsdOut || dsp.crossfeed || dsp.convolution || eqOn());
  if (dsp.peakHeadroom && reshapes) return { levelDb: -0.8, levelBasis: "peak", levelLimited: true };
  return { levelDb: null, levelBasis: null, levelLimited: false };
}
let lastDevice: Device | null = null;
let lastExclusive = true;

// ---- equalizer --------------------------------------------------------------------------------
const LCD2: EqPreset = {
  name: "Planar headphones", preamp: -6.4, autoPreamp: true, balance: 0, phase: "minimum", curve: [],
  bands: [
    { enabled: true, kind: "lowShelf", freq: 105, gain: 5.5, q: 0.7, slope: 12, channels: "both" },
    { enabled: true, kind: "peak", freq: 180, gain: -2.1, q: 0.9, slope: 12, channels: "both" },
    { enabled: true, kind: "peak", freq: 2800, gain: -3.2, q: 1.4, slope: 12, channels: "both" },
    { enabled: true, kind: "peak", freq: 6200, gain: 2.4, q: 3, slope: 12, channels: "both" },
    { enabled: true, kind: "peak", freq: 7400, gain: -1.8, q: 4.5, slope: 12, channels: "right" },
    { enabled: true, kind: "highShelf", freq: 10000, gain: 3, q: 0.7, slope: 12, channels: "both" },
  ],
};
// Like the backend: the EQ belongs to each output (by settings key). The sample has it on the USB
// DAC only; the speakers had the room preset and were switched off, so switching them on brings it back.
const eq: { presets: StoredPreset[]; activeId: string | null; enabled: boolean; perDevice: boolean; bypass: boolean; devices: Record<string, DeviceEq>; last: Record<string, string> } = {
  presets: [
    { ...structuredClone(LCD2), id: "p1", updated: 1 },
    {
      id: "p2", updated: 2, name: "Room, nearfield", preamp: -3, autoPreamp: false, balance: 0.4, phase: "linear", curve: [],
      bands: [
        { enabled: true, kind: "highPass", freq: 28, gain: 0, q: 0.71, slope: 24, channels: "both" },
        { enabled: true, kind: "peak", freq: 46, gain: -6.5, q: 5, slope: 12, channels: "both" },
        { enabled: true, kind: "peak", freq: 112, gain: -3, q: 3.2, slope: 12, channels: "left" },
      ],
    },
  ],
  activeId: "p1", enabled: false, perDevice: true, bypass: false,
  devices: { q7: { mode: "preset", id: "p1" }, spk: { mode: "off" } },
  last: { q7: "p1", spk: "p2" },
};
let eqSeq = 3;
/** The settings key of output `uid`, as the backend's `settings_key_of_uid`. */
const eqKey = (uid: string) => devices.find((d) => d.uid === uid)?.key ?? uid;
function eqSnapshot(): EqState {
  const playingOn = lastDevice?.key ?? null;
  const b = playingOn ? eq.devices[playingOn] : undefined;
  const effectiveId = b ? (b.mode === "preset" ? b.id : null) : !eq.perDevice && eq.enabled ? eq.activeId : null;
  return structuredClone({ ...eq, playingOn, effectiveId });
}
/** Output `uid` plays `b`; a preset is also what it comes back to, and the one chosen last. */
function eqBind(uid: string | null | undefined, b: DeviceEq) {
  if (b.mode === "preset") eq.activeId = b.id;
  if (!uid) return;
  const key = eqKey(uid);
  if (b.mode === "preset") eq.last[key] = b.id;
  eq.devices[key] = b;
}
function eqOn(): string | null {
  const s = eqSnapshot();
  return s.presets.find((p) => p.id === s.effectiveId)?.name ?? null;
}
function eqChanged() {
  return { state: eqSnapshot(), signal: duration > 0 && lastDevice ? mockSignal(lastDevice, lastExclusive, lastPath, lastDop) : null };
}

const convName = (dsp: DspSettings) => (dsp.convolution ? dsp.convolution.split("/").pop()!.replace(/\.[^.]+$/, "") : null);

let lastPath = "";
let lastDop = false;

function mockSignal(dev: Device, exclusive: boolean, path = "", dopWanted = false): Signal {
  const dsp = dspOf(dev);
  const air = dev.transport === "Bluetooth";
  if (/\.(dsf|dff)$/i.test(path)) {
    const dsdRate = 2822400;
    const dspOn = !!eqOn() || !!dsp.convolution || !!dsp.crossfeed;
    const dop = dopWanted && exclusive && !!dev.maxDop && !dspOn;
    const maxRate = Math.max(...dev.rates);
    const deviceRate = dop ? 176400 : Math.min(176400, maxRate);
    const dsdNote: Signal["dsdNote"] = !dopWanted || dop ? null : !exclusive ? { kind: "exclusive" } : dspOn ? { kind: "dsp" } : { kind: "rate", device: dev.name, khz: 176.4, dsd: "DSD64" };
    return {
      sourceRate: 176400, sourceBits: dop ? 1 : null, deviceRate, dacBits: 32, dacFloat: air, integerMode: !air,
      exclusive, transport: dev.transport, resampled: deviceRate !== 176400,
      eq: dop ? null : eqOn(), eqLinear: false, convolution: dop ? null : convName(dsp), crossfeed: dop ? null : dsp.crossfeed,
      sourceLossy: false, headroomDb: null, headroomMeasured: true, restoreFromHz: null,
      ...(dop ? { levelDb: null, levelBasis: null, levelLimited: false } : mockLevel(dsp)),
      dsdRate, dop, dsdNote, layout: "Stereo", downmix: null, upsampleFilter: null, upsampleTaps: null,
      dsdOut: null, dsdOutOrder: null, dsdOutNote: null, bitPerfect: dop,
    };
  }
  const src = 44100;
  // A surround file folds down to stereo on every mock device.
  const surround = /surround|5\.1/i.test(path);
  const dsd = mockDsdOut(dev, exclusive);
  const up = dsd.dsdOut ? DOP_WIRE[dsd.dsdOut] : dsp.upsampleKhz ? Math.min(dsp.upsampleKhz * 1000, air ? 48000 : 352800) : null;
  const rate = up ?? (air ? 48000 : src);
  // Like the engine: the long filter runs where upsampling at least doubles the rate.
  const long = up !== null && rate >= 2 * src ? dsp.upsampleFilter : null;
  const phoneTaps = phone.has("android") || phone.has("ios") ? 65536 : dsp.upsampleTaps;
  return {
    upsampleFilter: long, upsampleTaps: long && long !== "apodizing" ? phoneTaps : null,
    sourceRate: src, sourceBits: 16, deviceRate: rate, dacBits: 32, dacFloat: air, integerMode: !air,
    exclusive, transport: dev.transport, resampled: rate !== src,
    eq: eqOn(), eqLinear: eqSnapshot().presets.find((p) => p.name === eqOn())?.phase === "linear", convolution: convName(dsp), crossfeed: dsp.crossfeed,
    sourceLossy: false, headroomDb: null, headroomMeasured: true, restoreFromHz: null,
    ...mockLevel(dsp),
    dsdRate: null, dop: false, dsdNote: null, ...dsd,
    layout: surround ? "5.1" : "Stereo", downmix: surround ? { to: "Stereo", gainDb: -7.7 } : null,
    bitPerfect: !surround && !air && exclusive && rate === src && !eqOn() && !dsp.convolution && !dsp.crossfeed && !dsp.level && !dsd.dsdOut,
  };
}

/** DSD output for a PCM track on `dev`: what runs of the one chosen, or why nothing does. */
function mockDsdOut(dev: Device, exclusive: boolean): Pick<Signal, "dsdOut" | "dsdOutOrder" | "dsdOutNote"> {
  const dsp = dspOf(dev);
  const want = dsp.dsdOut;
  if (!want) return { dsdOut: null, dsdOutOrder: null, dsdOutNote: null };
  const off = (note: NonNullable<Signal["dsdOutNote"]>) => ({ dsdOut: null, dsdOutOrder: null, dsdOutNote: note });
  const name = dev.name;
  if (mockDsdNote === "rate" || (!mockDsdNote && !dev.network && exclusive && !dev.maxDop)) return off({ kind: "rate", device: name, dsd: "DSD64", khz: 176.4 });
  if (mockDsdNote === "refused") return off({ kind: "refused", device: name, dsd: `DSD${want}`, khz: DOP_WIRE[want] / 1000 });
  if (mockDsdNote === "exclusive" || mockDsdNote === "device" || mockDsdNote === "volume" || mockDsdNote === "network") return off({ kind: mockDsdNote });
  if (dev.network) return off({ kind: "network" });
  if (!exclusive) return off({ kind: "exclusive" });
  return { dsdOut: Math.min(want, dev.maxDop ?? 64, mockDsdLimit), dsdOutOrder: dsp.dsdOrder, dsdOutNote: null };
}

let mockQueue: string[] = [];
let mockIndex: number | null = null;
let mockRepeat: "off" | "all" | "one" = "off";
let mockRev = 0;
let mockNow: NowPlaying | null = null;
let started = 0;
let paused = false;
let pausedAt = 0;
let duration = 0;

// --- playlists and streaming (layout work only) ---
type MockEntry = { uri: string | null; title: string; artist: string | null; album: string | null; duration: number | null };
const mockLists: { id: string; name: string; updated: number; entries: MockEntry[]; smart?: unknown }[] = [];
let mockSignedIn: Record<string, boolean> = { qobuz: false, tidal: false };
let mockQuality = "max";
function mockSAlbum(i: number, svc: string) {
  const t = ["Kind of Blue", "A Love Supreme", "Blue Train", "Mingus Ah Um", "Time Out", "Moanin'"][i % 6];
  const a = ["Miles Davis", "John Coltrane", "John Coltrane", "Charles Mingus", "Dave Brubeck", "Art Blakey"][i % 6];
  return { id: `${svc}-${i}`, service: svc, title: t, artist: a, year: String(1957 + i), genre: "Jazz", cover: mockCover(`m${i}`), trackCount: 6, duration: 2700, bits: i % 2 ? 24 : 16, rate: svc === "tidal" ? null : i % 2 ? 192000 : 44100, hiRes: i % 2 === 1 };
}
function mockSTrack(i: number, svc: string, album = mockSAlbum(0, svc)) {
  const titles = ["So What", "Freddie Freeloader", "Blue in Green", "All Blues", "Flamenco Sketches", "Acknowledgement"];
  return { uri: `${svc}:track:${1000 + i}`, service: svc, id: String(1000 + i), title: titles[i % 6], artist: album.artist, album: album.title, albumId: album.id, albumArtist: album.artist, year: album.year, duration: 300 + i * 17, trackNumber: i + 1, discNumber: 1, cover: album.cover, isrc: null, bits: album.bits, rate: album.rate, hiRes: album.hiRes, streamable: true };
}
function mockSummary(p: (typeof mockLists)[number]) {
  return { id: p.id, name: p.name, count: p.entries.length, duration: p.entries.reduce((s, e) => s + (e.duration ?? 0), 0), updated: p.updated, smart: !!p.smart };
}

export async function mock<T>(cmd: string, args: Record<string, unknown> = {}): Promise<T> {
  const now = () => (paused ? pausedAt : (performance.now() - started) / 1000);
  switch (cmd) {
    case "list_devices":
      return devices as T;
    case "edit_tags":
      return [] as T;
    case "remote_status":
    case "remote_enable":
    case "remote_new_address": {
      if (cmd === "remote_enable") mockRemote = Boolean(args.enabled);
      const url = "http://192.168.0.169:48173/?t=3f9c2a7e5b1d4c8a9e0f6b2d7c4a1e8f";
      return { enabled: mockRemote, url: mockRemote ? url : null, qr: mockRemote ? '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 21 21"><rect width="21" height="21" fill="#fff"/><path d="M1 1h7v7H1zM13 1h7v7h-7zM1 13h7v7H1z" fill="#000"/></svg>' : null, error: null } as T;
    }
    case "output_volume":
      return (mockVolume as unknown) as T;
    case "set_output_volume":
      mockVolume = Number(args.volume);
      return undefined as T;
    case "system_locales":
      return [...navigator.languages] as T;
    case "library_status":
    case "library_rescan":
      return { folders: libFolders, skipped: libSkipped, fixed: libFixed, scanning: null, albums: libAlbums.length, tracks: libTracks.length, failed: 0, revision: 1 } as T;
    case "library_add_folder":
      libFolders = [...libFolders, args.path as string];
      return mock("library_status");
    case "library_pick_folder":
      libFolders = [...libFolders, "/storage/1A2B-3C4D/Audio/Vinyl rips 24-96 (cleaned)"];
      return mock("library_status");
    case "library_remove_folder":
      libFolders = libFolders.filter((f) => f !== args.path);
      return mock("library_status");
    case "library_skip_folder":
      libSkipped = [...libSkipped.filter((f) => f !== args.path), args.path as string];
      return mock("library_status");
    case "library_unskip_folder":
      libSkipped = libSkipped.filter((f) => f !== args.path);
      return mock("library_status");
    case "library_subfolders": {
      // The mock library keeps its tracks; only this list leaves the skipped folders out.
      const dirs = libTracks.map(libDir).filter((d) => !libSkipped.some((s) => libUnder(d, s)));
      const entry = (path: string) => {
        const inside = dirs.filter((d) => libUnder(d, path));
        return { path, name: path.slice(path.lastIndexOf("/") + 1), tracks: inside.length, hasSubfolders: inside.some((d) => d !== path) };
      };
      const parent = args.path as string | null;
      if (parent === null) return libFolders.map(entry) as T;
      const names = new Set(dirs.filter((d) => d.startsWith(parent + "/")).map((d) => d.slice(parent.length + 1).split("/")[0]));
      return [...names].sort().map((n) => entry(`${parent}/${n}`)) as T;
    }
    case "library_albums":
      return libAlbums as T;
    case "library_tracks":
      return libTracks as T;
    case "crash_report":
      // http://localhost:1420/?crash shows the notice.
      return (mockCrash
        ? "BitSpring 0.19.0 — error report\nTime: 2026-11-18 12:40 UTC\nSystem: macOS 15.1, aarch64\nThread: hifi-render\nError: index out of bounds: the len is 3 but the index is 7\nWhere: engine/src/output.rs:912"
        : null) as T;
    case "crash_dismiss":
      mockCrash = false;
      return undefined as T;
    case "loudness_status": {
      // The pretend analysis takes about 12 s for the whole library.
      const total = libTracks.length;
      if (measuring !== null) {
        const now = Math.min(total, measured + Math.floor(((performance.now() - measuring) / 12000) * total));
        if (now >= total) {
          measured = total;
          measuring = null;
        } else return { analysed: now, tracks: total, running: [now - measured, total - measured], untagged: 0, tagging: null, tagged: null } as T;
      }
      // Writing the tags takes about 6 s; three files pretend to be read-only.
      if (tagging !== null) {
        const todo = measured - tagged;
        const now = Math.min(todo, Math.floor(((performance.now() - tagging) / 6000) * todo));
        if (now < todo) return { analysed: measured, tracks: total, running: null, untagged: todo, tagging: [now, todo], tagged: null } as T;
        tagLast = [Math.max(0, todo - 3), Math.min(3, todo)];
        tagged = measured;
        tagging = null;
      }
      return { analysed: measured, tracks: total, running: null, untagged: measured - tagged, tagging: null, tagged: tagLast } as T;
    }
    case "loudness_write_tags":
      if (args.start) tagging = performance.now();
      else tagging = null;
      return undefined as T;
    case "loudness_analyze":
      if (args.start) measuring = performance.now();
      else if (measuring !== null) {
        measured = Math.min(libTracks.length, measured + Math.floor(((performance.now() - measuring) / 12000) * libTracks.length));
        measuring = null;
      }
      return undefined as T;
    case "quality_cached":
      return (args.paths as string[]).map((p) => mockChecks[p] ?? null) as T;
    case "quality_check": {
      await new Promise((r) => setTimeout(r, 400));
      const path = args.path as string;
      const t = libTracks.find((x) => x.path === path);
      const rate = t?.sampleRate ?? 44100;
      // The second track of a hi-res album "turns out" upsampled, so every state can be seen.
      const n = t?.trackNumber ?? 1;
      const kind = !t?.lossless ? "lossy" : rate > 48000 && n === 2 ? "upsampled" : t?.format === "WAV" ? "inconclusive" : "genuine";
      const top = kind === "upsampled" ? 21600 : kind === "lossy" ? 16000 : kind === "inconclusive" ? 20100 : rate > 48000 ? rate * 0.36 : rate / 2;
      const spectrum = Array.from({ length: 400 }, (_, i) => {
        const hz = ((i + 0.5) / 400) * (rate / 2);
        const music = -38 - 22 * Math.log10(1 + hz / 900) + Math.sin(i * 1.7) * 2;
        return hz < top ? music : kind === "genuine" || kind === "lossy" ? Math.min(music, -150) : -150 - (i % 7);
      });
      const r = { verdict: { kind }, sampleRate: rate, bits: t?.bits ?? null, effectiveBits: t?.bits === 24 ? 24 : null, cutoffHz: kind === "genuine" && rate <= 48000 ? null : Math.round(top), wall: kind !== "genuine", spectrum, peakDb: -0.3 - n * 0.4, clipped: n === 3 ? 4 : 0, analysedSecs: t?.duration ?? 60 };
      mockChecks[path] = r;
      // Measured in the same pass: figures that differ from track to track, steady per track.
      const seed = [...path].reduce((h, c) => (h * 31 + c.charCodeAt(0)) >>> 0, 7);
      const dr = 5 + (seed % 100) / 10;
      mockDynamics[path] = { dr, lra: 2 + ((seed >> 8) % 120) / 10, lufs: -6 - dr * 0.8, peakDb: -0.1 - (seed % 9) / 10 };
      mockDynamicsRev++;
      return r as T;
    }
    // The rip check: every state can be seen — an album of three tracks has a damaged
    // second one, one of two isn't in the database, the rest are verified.
    case "rip_cached":
      return (mockRips[(args.paths as string[])[0]] ?? null) as T;
    case "rip_progress":
      return (mockRipStart ? [Math.min(mockRipTotal, Math.floor((performance.now() - mockRipStart) / 300)), mockRipTotal] : null) as T;
    case "rip_check": {
      const paths = args.paths as string[];
      mockRipStart = performance.now();
      mockRipTotal = paths.length;
      await new Promise((r) => setTimeout(r, 300 * paths.length + 300));
      mockRipStart = 0;
      const base = { kind: "checked", matching: 0, known: 0, offset: 0, tracks: [], checkedAt: Math.floor(Date.now() / 1000) };
      const r =
        paths.length === 3
          ? { ...base, status: "partly", known: 41, tracks: [true, false, true] }
          : paths.length === 2
            ? { ...base, status: "unknown" }
            : { ...base, status: "verified", matching: 116, known: 121, offset: paths.length % 2 ? 664 : 0 };
      mockRips[paths[0]] = r;
      return r as T;
    }
    // The blind test: getting ready takes a moment, X is a coin toss per trial.
    case "abx_start": {
      await new Promise((r) => setTimeout(r, 900));
      mockAbx = { total: (args.trials as number) ?? 16, answers: [], xIsA: Math.random() < 0.5 };
      started = performance.now(); paused = false; pausedAt = 0; duration = 204;
      const files = args.kind === "files";
      if (args.kind === "filters") {
        // Like the app: the rate and the long filter of what plays now.
        const sig = mockNow && mockNow.path === args.a ? mockNow.signal : null;
        if (!sig?.upsampleFilter) throw { code: "abxNoFilter" };
        duration = 95;
        const side = (filter: { kind: string; taps: number } | null) => ({ rate: sig.sourceRate, bits: sig.sourceBits, resampled: true, gainDb: -0.4, filter });
        return {
          prepared: {
            rate: sig.deviceRate,
            a: side(null),
            b: side({ kind: sig.upsampleFilter, taps: sig.upsampleTaps ?? 1024 }),
            offsetMs: 0, likeness: 0.9999, seconds: 95, shortened: true, identical: false,
          },
          trials: mockAbx.total,
        } as T;
      }
      return {
        prepared: {
          rate: files ? 96000 : 44100,
          a: { rate: files ? 96000 : 44100, bits: files ? 24 : 16, resampled: false, gainDb: 0, filter: null },
          b: { rate: 44100, bits: 16, resampled: files, gainDb: files ? -1.3 : 0, filter: null },
          offsetMs: files ? 23 : 0.4, likeness: 0.97, seconds: 204, shortened: files, identical: false,
        },
        trials: mockAbx.total,
      } as T;
    }
    case "abx_listen":
    case "abx_stop":
      return undefined as T;
    case "abx_again":
      if (mockAbx) mockAbx = { ...mockAbx, answers: [] };
      return undefined as T;
    case "abx_set_trials":
      if (mockAbx) mockAbx.total = args.trials as number;
      return args.trials as T;
    case "abx_answer": {
      if (!mockAbx) return { done: 0, total: 0, result: null } as T;
      mockAbx.answers.push({ xIsA: mockAbx.xIsA, saidA: args.isA as boolean });
      mockAbx.xIsA = Math.random() < 0.5;
      const { answers, total } = mockAbx;
      const correct = answers.filter((a) => a.xIsA === a.saidA).length;
      // P(at least `correct` of `total` by guessing).
      let p = 0;
      for (let k = correct, c = 1; k <= total; k++) {
        c = 1;
        for (let i = 0; i < k; i++) c = (c * (total - i)) / (i + 1);
        p += c / 2 ** total;
      }
      return { done: answers.length, total, result: answers.length >= total ? { correct, total, p, trials: answers } : null } as T;
    }
    case "dynamics": {
      if (args.rev === mockDynamicsRev) return { rev: mockDynamicsRev, tracks: null, albums: null } as T;
      const albums: Record<string, unknown> = {};
      for (const a of libAlbums) {
        const all = a.tracks.map((i) => mockDynamics[libTracks[i].path]);
        if (!all.length || all.some((d) => !d)) continue;
        const mean = (f: (d: { dr: number; lra: number; lufs: number }) => number) => all.reduce((s, d) => s + f(d), 0) / all.length;
        albums[a.id] = { dr: Math.round(mean((d) => Math.round(d.dr))), lra: mean((d) => d.lra) + 1.5, lufs: mean((d) => d.lufs), peakDb: -0.1 };
      }
      return { rev: mockDynamicsRev, tracks: { ...mockDynamics }, albums } as T;
    }
    case "marks":
      return { rev: mockMarksRev, tracks: { ...mockMarks.tracks }, albums: { ...mockMarks.albums } } as T;
    case "mark_favorite": {
      const path = args.path as string;
      const m = mockMarks.tracks[path] ?? { fav: false, rating: 0, at: 0 };
      mockMark(path, { ...m, fav: args.on as boolean, at: args.on ? Date.now() / 1000 : 0 });
      return ++mockMarksRev as T;
    }
    case "mark_rating": {
      const path = args.path as string;
      const m = mockMarks.tracks[path] ?? { fav: false, rating: 0, at: 0 };
      mockMark(path, { ...m, rating: Math.min(5, args.rating as number) });
      return ++mockMarksRev as T;
    }
    case "mark_album": {
      if (args.on) mockMarks.albums[args.id as string] = { at: Date.now() / 1000 };
      else delete mockMarks.albums[args.id as string];
      return ++mockMarksRev as T;
    }
    case "expand_paths":
      return (args.paths as string[]).map((path) => ({ path, title: null, artist: null })) as T;
    case "play_file": {
      const dev = devices.find((d) => d.uid === args.deviceUid) ?? devices[0];
      lastDevice = dev;
      lastExclusive = args.exclusive as boolean;
      lastPath = args.path as string;
      lastDop = !!args.dop;
      const signal = mockSignal(dev, lastExclusive, lastPath, lastDop);
      started = performance.now() - ((args.startAt as number) ?? 0) * 1000; paused = !!args.paused; pausedAt = (args.startAt as number) ?? 0; duration = 251;
      const np: NowPlaying = {
        path: args.path as string, ...(() => {
          const t = libTracks.find((x) => x.path === args.path);
          return t
            ? { title: t.title, artist: t.artist, album: t.album, year: t.year, cover: t.hasCover ? mockCover(t.albumId) : null }
            : { title: "So What", artist: "Miles Davis", album: "Kind of Blue", year: "1959", cover: null };
        })(),
        channels: 2, duration, deviceName: dev.name, signal, rev: ++mockRev,
      };
      mockNow = np;
      if (args.index !== null && args.index !== undefined) mockIndex = args.index as number;
      return np as T;
    }
    case "set_queue":
      mockQueue = args.paths as string[];
      if (args.rev === mockRev) mockIndex = args.index as number | null;
      return undefined as T;
    case "set_play_modes":
      mockRepeat = args.repeat as typeof mockRepeat;
      return undefined as T;
    case "now_playing":
      return mockNow as T;
    case "lyrics": {
      // Made-up words, timed every few seconds: one track has timed lyrics, one plain, the
      // DSD one only "online".
      await new Promise((r) => setTimeout(r, 400));
      const path = String(args.path);
      const verses = [
        "Morning light along the river", "Every window holds a song", "I keep walking where it takes me", "Never knowing for how long", "",
        "Turn the dial and let it carry", "Over rooftops, over stone", "All the voices in the wire", "Say you're never quite alone", "",
        "Hold the note until it settles", "Let the silence have its say", "When the last chord finds the morning", "We'll be miles and miles away",
      ];
      if (path.endsWith(".dsf") && !args.online) return null as T;
      if (path.includes("Freddie")) return { lines: verses.map((text) => ({ time: null, text })), synced: false, source: "embedded" } satisfies Lyrics as T;
      return { lines: verses.map((text, i) => ({ time: 8 + i * 4.5, text })), synced: true, source: path.endsWith(".dsf") ? "online" : "sidecar" } satisfies Lyrics as T;
    }
    case "import_files":
      return args.paths as T;
    case "status": {
      // Like the backend: at the end of a track the queue moves on by itself.
      if (duration > 0 && !paused && now() >= duration && mockIndex !== null && lastDevice) {
        const next = mockRepeat === "one" ? mockIndex : mockRepeat === "all" && mockQueue.length ? (mockIndex + 1) % mockQueue.length : mockIndex + 1;
        if (next < mockQueue.length) await mock("play_file", { path: mockQueue[next], deviceUid: lastDevice.uid, exclusive: lastExclusive, dop: lastDop, index: next });
      }
      const dsp = dspOf(lastDevice);
      const processing = !!(dsp.upsampleKhz || dsp.dsdOut || eqOn() || dsp.convolution || dsp.crossfeed);
      const long = duration > 0 && lastDevice ? mockSignal(lastDevice, lastExclusive, lastPath, lastDop) : null;
      const filter = long?.upsampleFilter ?? null;
      // A long filter's pretend cost: a share of a core that wobbles a little.
      const cost = filter === "apodizing" ? 0.004 : long?.upsampleTaps === 1048576 ? (filter === "minimum" ? 0.019 : 0.016) : 0.007;
      if (mockStepDown && filter && now() > 4 && mockNow) {
        mockStepDown = false;
        const next = filter !== "apodizing" && long?.upsampleTaps === 1048576 ? { filter, taps: 65536 } : { filter: null, taps: null };
        dspByKey[lastDevice!.key] = { ...dsp, upsampleFilter: next.filter, upsampleTaps: next.taps === 65536 ? 65536 : dsp.upsampleTaps };
        mockStep = { seq: ++mockStepSeq, ...next, deviceKey: lastDevice!.key };
        mockNow = { ...mockNow, signal: mockSignal(lastDevice!, lastExclusive, lastPath, lastDop) };
      }
      const encoding = long?.dsdOut ?? null;
      if (mockDsdStepDown && encoding && now() > 4 && mockNow) {
        mockDsdStepDown = false;
        // Like the backend: a lower rate at the same modulator, then the lighter modulator, then off.
        const order = long!.dsdOutOrder ?? 7;
        const next: { dsdOut: number | null; order: number | null } =
          encoding > 64 ? { dsdOut: encoding / 2, order } : order === 7 ? { dsdOut: 64, order: 5 } : { dsdOut: null, order: null };
        const stepped = dspOf(lastDevice);
        dspByKey[lastDevice!.key] = { ...stepped, dsdOut: next.dsdOut as DspSettings["dsdOut"], dsdOrder: next.order === 5 ? 5 : stepped.dsdOrder };
        mockDsdStep = { seq: ++mockDsdStepSeq, ...next, deviceKey: lastDevice!.key };
        mockNow = { ...mockNow, signal: mockSignal(lastDevice!, lastExclusive, lastPath, lastDop) };
      }
      // The encoder's pretend cost: more at the higher rates and with the 7th order.
      const dsdCost = encoding ? (encoding / 64) * 0.05 * (long?.dsdOutOrder === 5 ? 0.7 : 1) : 0;
      const s: Status = {
        active: duration > 0, position: Math.min(now(), duration), duration, paused,
        finished: now() >= duration && duration > 0, peakDb: processing ? -3.4 : null, underruns: 0,
        buffering: false, buffered: duration, error: null,
        queueIndex: mockIndex, nowRev: mockRev, advanceError: null, systemVolume: isIOS ? 0.8 : null,
        load: processing && duration > 0 ? ((filter ? cost : 0.002) + dsdCost) * (1 + 0.08 * Math.sin(performance.now() / 900)) : null,
        filterStep: mockStep,
        dsdStep: mockDsdStep,
      };
      return s as T;
    }
    case "meter": {
      if (duration === 0 || paused) return [-90, -90] as T;
      const t = performance.now() / 1000;
      const base = -14 + 6 * Math.sin(t * 1.3) + 4 * Math.sin(t * 7.1);
      return [base + 2 * Math.sin(t * 13), base - 2 + 2 * Math.cos(t * 11)] as T;
    }
    case "set_paused":
      if (args.paused && !paused) pausedAt = now();
      if (!args.paused && paused) started = performance.now() - pausedAt * 1000;
      paused = args.paused as boolean;
      return undefined as T;
    case "seek":
      started = performance.now() - (args.seconds as number) * 1000;
      pausedAt = args.seconds as number;
      return undefined as T;
    case "stop":
      duration = 0;
      return undefined as T;
    case "set_fades":
      return undefined as T;
    case "set_dsp": {
      // Like the backend: kept for that output; what plays restarts only if it plays there.
      const key = devices.find((d) => d.uid === args.deviceUid)?.key ?? (args.deviceUid as string);
      const here = duration > 0 && lastDevice?.key === key;
      if (here) {
        mockStep = null;
        mockDsdStep = null;
      }
      const dsp: DspSettings = (dspByKey[key] = {
        upsampleKhz: args.upsampleKhz as number | null,
        upsampleFilter: (["linear", "minimum", "apodizing"].includes(args.upsampleFilter as string) ? args.upsampleFilter : null) as DspSettings["upsampleFilter"],
        upsampleTaps: args.upsampleTaps === 1048576 ? 1048576 : 65536,
        crossfeed: args.crossfeed as DspSettings["crossfeed"],
        convolution: args.convolution as string | null, headroom: !!args.headroom, restoreHighs: !!args.restoreHighs,
        level: (args.level as DspSettings["level"]) ?? null, peakHeadroom: !!args.peakHeadroom,
        // Like the backend: 64, 128 or 256 (phones keep 64), anything else is off; order 5 or 7.
        dsdOut: args.dsdOut === 64 || args.dsdOut === 128 || args.dsdOut === 256 ? (phone.has("android") || phone.has("ios") ? 64 : args.dsdOut) : null,
        dsdOrder: args.dsdOrder === 5 ? 5 : 7,
      });
      return {
        signal: here && lastDevice ? mockSignal(lastDevice, lastExclusive, lastPath, lastDop) : null,
        convolution: dsp.convolution ? { name: convName(dsp), channels: 2, rate: 48000, seconds: 0.68 } : null,
      } as T;
    }
    case "set_menu_labels":
      return undefined as T;
    case "update_check":
      return {
        enabled: !storeBuild, current: "0.2.0",
        update: new URLSearchParams(location.search).has("update")
          ? { version: "0.3.0", date: null, notes: "**What's new**\n- **Equalizer** presets per output.\n- Faster library scan." }
          : null,
      } as T;
    case "update_install":
      await new Promise((r) => setTimeout(r, 2500));
      return undefined as T;
    case "update_progress":
      return [Math.min(30e6, ((performance.now() / 80) % 400) * 75e3), 30e6] as T;
    case "eq_state":
      return eqSnapshot() as T;
    case "eq_save": {
      const p = eq.presets.find((x) => x.id === args.id);
      if (p) Object.assign(p, structuredClone(args.preset as EqPreset), { updated: Date.now() });
      else {
        const id = `p${eqSeq++}`;
        eq.presets.push({ ...structuredClone(args.preset as EqPreset), id, updated: Date.now() });
        eqBind(args.deviceUid as string | null, { mode: "preset", id });
      }
      return eqChanged() as T;
    }
    case "eq_delete":
      eq.presets = eq.presets.filter((p) => p.id !== args.id);
      if (eq.activeId === args.id) eq.activeId = eq.presets[0]?.id ?? null;
      for (const [k, v] of Object.entries(eq.devices)) if (v.mode === "preset" && v.id === args.id) delete eq.devices[k];
      for (const [k, v] of Object.entries(eq.last)) if (v === args.id) delete eq.last[k];
      return eqChanged() as T;
    case "eq_adopt":
      // The sample is no earlier version's file: only the flag, as the backend does for a fresh install.
      eq.perDevice = true;
      return eqChanged() as T;
    case "eq_set_bypass":
      eq.bypass = !!args.bypass;
      return eqChanged() as T;
    case "eq_bind_device":
      eqBind(args.deviceUid as string, args.binding as DeviceEq);
      return eqChanged() as T;
    case "eq_headphones":
      return MOCK_HEADPHONES.map(([name, source]) => ({ name, source })) as T;
    case "eq_catalog":
      await new Promise((r) => setTimeout(r, 600));
      return mockCatalog() as T;
    case "eq_add_headphone":
    case "eq_add_catalog": {
      const id = `p${eqSeq++}`;
      const name = cmd === "eq_add_catalog" ? `${args.name} (${args.source})` : (args.name as string);
      eq.presets.push({ ...structuredClone(LCD2), name, id, updated: Date.now() });
      eqBind(args.deviceUid as string | null, { mode: "preset", id });
      return { ...eqChanged(), ids: [id] } as T;
    }
    case "eq_import_file":
    case "eq_import_text": {
      const id = `p${eqSeq++}`;
      const name = cmd === "eq_import_text" ? (args.name as string) : "My headphones (imported)";
      eq.presets.push({ ...structuredClone(LCD2), name, id, updated: Date.now() });
      eqBind(args.deviceUid as string | null, { mode: "preset", id });
      return { ...eqChanged(), ids: [id] } as T;
    }
    case "eq_export":
      return ((args.path as string | null) ?? `/Users/me/Downloads/${eq.presets.find((p) => p.id === (args.ids as string[])[0])?.name ?? "BitSpring EQ"}.txt`) as T;
    case "eq_export_text":
      return "Preamp: -6.4 dB\nFilter 1: ON LSC Fc 105 Hz Gain 5.5 dB Q 0.7\n" as T;
    case "channel_test":
      return { sourceRate: 44100, sourceBits: 24, deviceRate: 44100, dacBits: 32, dacFloat: false, integerMode: true, exclusive: true, transport: "Usb", resampled: false, eq: null, eqLinear: false, convolution: null, crossfeed: null, sourceLossy: false, headroomDb: null, headroomMeasured: true, restoreFromHz: null, levelDb: null, levelBasis: null, levelLimited: false, dsdRate: null, dop: false, dsdNote: null, dsdOut: null, dsdOutOrder: null, dsdOutNote: null, layout: "Stereo", downmix: null, bitPerfect: true } as T;
    case "playlists_list":
      return [...mockLists].sort((a, b) => b.updated - a.updated).map(mockSummary) as T;
    case "playlist_get": {
      const p = mockLists.find((l) => l.id === args.id)!;
      return { id: p.id, name: p.name, entries: p.entries.map((e) => ({ ...e, available: !!e.uri })), smart: p.smart ?? null } as T;
    }
    case "playlist_create": {
      const smart = args.smart ?? undefined;
      const entries = smart ? [0, 1, 2].map((i) => ({ uri: `/m/${i}.flac`, title: ["Kite Strings", "Harbour", "Paper Moons"][i], artist: "Ada Marlow", album: "Paper Moons", duration: 200 + i })) : (args.entries as MockEntry[]);
      const p = { id: String(Date.now()), name: args.name as string, updated: Date.now(), entries, smart };
      mockLists.push(p);
      return mockSummary(p) as T;
    }
    case "playlist_update": {
      const p = mockLists.find((l) => l.id === args.id)!;
      if (args.name) p.name = args.name as string;
      if (args.entries && !p.smart) p.entries = args.entries as MockEntry[];
      if (args.smart && p.smart) p.smart = args.smart;
      p.updated = Date.now();
      return mockSummary(p) as T;
    }
    case "playlist_add": {
      const p = mockLists.find((l) => l.id === args.id)!;
      p.entries.push(...(args.entries as MockEntry[]));
      return mockSummary(p) as T;
    }
    case "playlist_delete":
      mockLists.splice(mockLists.findIndex((l) => l.id === args.id), 1);
      return undefined as T;
    case "playlist_import_file":
    case "playlist_import_text": {
      const entries: MockEntry[] = [
        ...libTracks.slice(0, 5).map((t) => ({ uri: t.path, title: t.title, artist: t.artist, album: t.album, duration: t.duration })),
        { uri: null, title: "Come Together", artist: "The Beatles", album: "Abbey Road", duration: 259 },
        { uri: "qobuz:track:19512574", title: "So What", artist: "Miles Davis", album: "Kind of Blue", duration: 562 },
      ];
      const p = { id: String(Date.now()), name: (args.name as string) || "Liked Songs", updated: Date.now(), entries };
      mockLists.push(p);
      return [{ id: p.id, name: p.name, total: entries.length, found: entries.length - 1, missing: ["The Beatles – Come Together"] }] as T;
    }
    case "playlist_export":
      return "/Users/me/Downloads/playlist.m3u8" as T;
    case "streaming_status":
      return { available: !isIOS && !storeBuild, quality: mockQuality, accounts: ["qobuz", "tidal"].map((s) => ({ service: s, signedIn: mockSignedIn[s], user: mockSignedIn[s] ? "me@example.com" : null, note: null })) } as T;
    case "streaming_set_quality":
      mockQuality = args.quality as string;
      return undefined as T;
    case "streaming_login":
      mockSignedIn = { ...mockSignedIn, [args.service as string]: true };
      return [] as T;
    case "streaming_logout":
      mockSignedIn = { ...mockSignedIn, [args.service as string]: false };
      return undefined as T;
    case "streaming_search":
      return { albums: [0, 1, 2, 3, 4].map((i) => mockSAlbum(i, args.service as string)), tracks: [0, 1, 2, 3].map((i) => mockSTrack(i, args.service as string)) } as T;
    case "streaming_album": {
      const i = Number(String(args.id).split("-")[1] ?? 0);
      const a = mockSAlbum(i, args.service as string);
      return { album: a, tracks: [0, 1, 2, 3, 4].map((k) => mockSTrack(k, args.service as string, a)) } as T;
    }
    case "streaming_favorites":
      return [1, 3, 5].map((i) => mockSAlbum(i, args.service as string)) as T;
    case "streaming_playlists":
      return [{ id: "p1", service: args.service, name: "Late night jazz", trackCount: 42, cover: mockCover("m3"), owner: "me" }] as T;
    case "streaming_playlist":
      return { playlist: { id: args.id, service: args.service, name: "Late night jazz", trackCount: 4, cover: mockCover("m3"), owner: "me" }, tracks: [0, 1, 2, 3].map((i) => mockSTrack(i, args.service as string)) } as T;
    case "streaming_import_playlist": {
      const p = { id: String(Date.now()), name: "Late night jazz", updated: Date.now(), entries: [0, 1, 2, 3].map((i) => { const t = mockSTrack(i, args.service as string); return { uri: t.uri, title: t.title, artist: t.artist, album: t.album, duration: t.duration }; }) };
      mockLists.push(p);
      return { id: p.id, name: p.name, total: 4, found: 4, missing: [] } as T;
    }
    case "streaming_find_missing":
      return 1 as T;
  }
  throw new Error(`mock: unknown command ${cmd}`);
}

const MOCK_HEADPHONES: [string, string][] = [
  ["Audeze LCD-2", "oratory1990"],
  ["Audeze LCD-X", "oratory1990"],
  ["HIFIMAN Sundara", "oratory1990"],
  ["Sennheiser HD 600", "oratory1990"],
  ["Sennheiser HD 650", "oratory1990"],
  ["Sennheiser HD 800 S", "oratory1990"],
  ["Sony WH-1000XM5", "oratory1990"],
];

/** A few thousand made-up catalog rows around real names, enough to try the search. */
function mockCatalog(): { name: string; source: string; rig: string | null }[] {
  const out: { name: string; source: string; rig: string | null }[] = [];
  for (const [name] of MOCK_HEADPHONES) {
    out.push({ name, source: "oratory1990", rig: null });
    out.push({ name, source: "crinacle", rig: "GRAS 43AG-7" });
    out.push({ name, source: "Rtings", rig: null });
  }
  const brands = ["Moondrop", "Truthear", "Sennheiser", "Beyerdynamic", "AKG", "Audio-Technica", "FiiO", "Shure", "Sony", "Focal"];
  for (let i = 0; i < 3000; i++) out.push({ name: `${brands[i % brands.length]} Model ${100 + i}`, source: i % 3 ? "crinacle" : "Super Review", rig: i % 3 ? "711" : null });
  return out.sort((a, b) => a.name.localeCompare(b.name));
}
