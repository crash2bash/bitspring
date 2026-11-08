// Browser-only stand-in for the Rust backend, so the layout can be built and reviewed
// without Tauri. Never used inside the app.
import type { Album, Device, DspSettings, EqInfo, LibTrack, NowPlaying, Signal, Status } from "./api";

// ---- library ----------------------------------------------------------------------------------
const LIB: [string, string, string | null, string | null, string, number, number | null, string | null, string[]][] = [
  ["Ada Marlow", "Paper Moons", "2017", "Folk", "ALAC", 48000, 24, "#6b5d2e", ["Kite Strings", "Harbour", "Paper Moons"]],
  ["Echo Field", "Signal Path", "2020", "Electronic", "MP3", 44100, null, null, ["Carrier", "Sideband", "Noise Floor"]],
  ["Halvorsen Quartet", "Strings in Winter", "2021", "Classical", "FLAC", 192000, 24, "#2e3b6b", ["I. Allegro", "II. Adagio", "III. Scherzo", "IV. Finale"]],
  ["Northern Lights Trio", "Quiet Hours", "2019", "Jazz", "FLAC", 96000, 24, "#2b4a5c", ["Morning Glass", "Slow Current", "Blue Hour", "Afterglow"]],
  ["Northern Lights Trio", "City Rain", "2022", "Jazz", "FLAC", 44100, 16, "#5c3a2b", ["Neon Puddles", "Last Tram", "Umbrella Song"]],
  ["Неизвестный исполнитель", "Untitled Sketches", null, null, "WAV", 44100, 16, null, ["01 sketch one", "02 sketch two"]],
];
const libTracks: LibTrack[] = [];
const libAlbums: Album[] = LIB.map(([artist, title, year, genre, format, rate, bits, color, names], ai) => {
  const id = `a${ai}`;
  const idx = names.map((name, n) => {
    libTracks.push({
      path: `/Music/${artist}/${title}/${String(n + 1).padStart(2, "0")} ${name}.${format.toLowerCase()}`,
      title: name, artist: artist === "Неизвестный исполнитель" ? null : artist, albumArtist: null, album: title, genre, year,
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

export function mockCover(id: string): string {
  const c = coverColor[id] ?? "#1f3039";
  return `data:image/svg+xml;utf8,${encodeURIComponent(`<svg xmlns="http://www.w3.org/2000/svg" width="300" height="300"><rect width="300" height="300" fill="${c}"/><circle cx="150" cy="150" r="90" fill="none" stroke="rgba(255,255,255,.25)" stroke-width="2"/></svg>`)}`;
}

let libFolders = ["/Users/me/Music"];

const devices: Device[] = [
  { uid: "q7", name: "FiiO Q7", transport: "Usb", channels: 2, rates: [44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000, 705600, 768000], nominalRate: 44100, isDefault: true, bitPerfectCapable: true, maxDop: 256 },
  { uid: "spk", name: "Mac Studio Speakers", transport: "BuiltIn", channels: 2, rates: [44100, 48000, 88200, 96000], nominalRate: 44100, isDefault: false, bitPerfectCapable: true, maxDop: null },
  { uid: "air", name: "AirPods Pro", transport: "Bluetooth", channels: 2, rates: [24000, 48000], nominalRate: 48000, isDefault: false, bitPerfectCapable: false, maxDop: null },
];

let dsp: DspSettings = { upsampleKhz: null, crossfeed: null, eqPath: null, headroom: true, restoreHighs: false };
let lastDevice: Device | null = null;
let lastExclusive = true;

function mockEq(path: string): EqInfo {
  const bands: EqInfo["bands"] = [
    { kind: "lowShelf", freq: 105, gain: 5.5, q: 0.7 },
    { kind: "peak", freq: 2800, gain: -3.2, q: 1.4 },
    { kind: "peak", freq: 6200, gain: 2.4, q: 3 },
    { kind: "highShelf", freq: 10000, gain: 3, q: 0.7 },
  ];
  const curve: [number, number][] = Array.from({ length: 97 }, (_, i) => {
    const f = 20 * Math.pow(1000, i / 96);
    const lo = 5.5 / (1 + Math.pow(f / 105, 2));
    const pk = -3.2 * Math.exp(-Math.pow(Math.log2(f / 2800) * 1.4, 2));
    const pk2 = 2.4 * Math.exp(-Math.pow(Math.log2(f / 6200) * 3, 2));
    const hi = 3 / (1 + Math.pow(10000 / f, 2));
    return [f, -6 + lo + pk + pk2 + hi];
  });
  return { name: path.split("/").pop()!.replace(/ ParametricEQ\.txt$|\.txt$/, ""), preamp: -6, bands, curve };
}

let lastPath = "";
let lastDop = false;

function mockSignal(dev: Device, exclusive: boolean, path = "", dopWanted = false): Signal {
  const air = dev.transport === "Bluetooth";
  if (/\.(dsf|dff)$/i.test(path)) {
    const dsdRate = 2822400;
    const dspOn = !!dsp.eqPath || !!dsp.crossfeed;
    const dop = dopWanted && exclusive && !!dev.maxDop && !dspOn;
    const maxRate = Math.max(...dev.rates);
    const deviceRate = dop ? 176400 : Math.min(176400, maxRate);
    const dsdNote = !dopWanted || dop ? null : !exclusive ? "DoP работает только в эксклюзивном режиме" : dspOn ? "эквалайзер и кроссфид работают с PCM" : `${dev.name} не работает на 176,4 кГц, нужных для DoP DSD64`;
    return {
      sourceRate: 176400, sourceBits: dop ? 1 : null, deviceRate, dacBits: 32, dacFloat: air, integerMode: !air,
      exclusive, transport: dev.transport, resampled: deviceRate !== 176400,
      eq: !dop && dsp.eqPath ? mockEq(dsp.eqPath).name : null, crossfeed: dop ? null : dsp.crossfeed,
      sourceLossy: false, headroomDb: null, headroomMeasured: true, restoreFromHz: null,
      dsdRate, dop, dsdNote, bitPerfect: dop,
    };
  }
  const src = 44100;
  const up = dsp.upsampleKhz ? Math.min(dsp.upsampleKhz * 1000, air ? 48000 : 352800) : null;
  const rate = up ?? (air ? 48000 : src);
  return {
    sourceRate: src, sourceBits: 16, deviceRate: rate, dacBits: 32, dacFloat: air, integerMode: !air,
    exclusive, transport: dev.transport, resampled: rate !== src,
    eq: dsp.eqPath ? mockEq(dsp.eqPath).name : null, crossfeed: dsp.crossfeed,
    sourceLossy: false, headroomDb: null, headroomMeasured: true, restoreFromHz: null,
    dsdRate: null, dop: false, dsdNote: null,
    bitPerfect: !air && exclusive && rate === src && !dsp.eqPath && !dsp.crossfeed,
  };
}

let started = 0;
let paused = false;
let pausedAt = 0;
let duration = 0;

export async function mock<T>(cmd: string, args: Record<string, unknown> = {}): Promise<T> {
  const now = () => (paused ? pausedAt : (performance.now() - started) / 1000);
  switch (cmd) {
    case "list_devices":
      return devices as T;
    case "library_status":
    case "library_rescan":
      return { folders: libFolders, scanning: null, albums: libAlbums.length, tracks: libTracks.length, failed: 0, revision: 1 } as T;
    case "library_add_folder":
      libFolders = [...libFolders, args.path as string];
      return mock("library_status");
    case "library_remove_folder":
      libFolders = libFolders.filter((f) => f !== args.path);
      return mock("library_status");
    case "library_albums":
      return libAlbums as T;
    case "library_tracks":
      return libTracks as T;
    case "expand_paths":
      return args.paths as T;
    case "play_file": {
      const dev = devices.find((d) => d.uid === args.deviceUid) ?? devices[0];
      lastDevice = dev;
      lastExclusive = args.exclusive as boolean;
      lastPath = args.path as string;
      lastDop = !!args.dop;
      const signal = mockSignal(dev, lastExclusive, lastPath, lastDop);
      started = performance.now() - ((args.startAt as number) ?? 0) * 1000; paused = false; duration = 251;
      const np: NowPlaying = {
        path: args.path as string, ...(() => {
          const t = libTracks.find((x) => x.path === args.path);
          return t
            ? { title: t.title, artist: t.artist, album: t.album, year: t.year, cover: t.hasCover ? mockCover(t.albumId) : null }
            : { title: "So What", artist: "Miles Davis", album: "Kind of Blue", year: "1959", cover: null };
        })(),
        channels: 2, duration, deviceName: dev.name, signal,
      };
      return np as T;
    }
    case "status": {
      const processing = !!(dsp.upsampleKhz || dsp.eqPath || dsp.crossfeed);
      const s: Status = {
        active: duration > 0, position: Math.min(now(), duration), duration, paused,
        finished: now() >= duration && duration > 0, peakDb: processing ? -3.4 : null, underruns: 0,
        buffering: false, buffered: duration, error: null,
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
    case "set_dsp": {
      dsp = {
        upsampleKhz: args.upsampleKhz as number | null, crossfeed: args.crossfeed as DspSettings["crossfeed"],
        eqPath: args.eqPath as string | null, headroom: !!args.headroom, restoreHighs: !!args.restoreHighs,
      };
      return { signal: duration > 0 && lastDevice ? mockSignal(lastDevice, lastExclusive, lastPath, lastDop) : null, eq: dsp.eqPath ? mockEq(dsp.eqPath) : null } as T;
    }
    case "channel_test":
      return { sourceRate: 44100, sourceBits: 24, deviceRate: 44100, dacBits: 32, dacFloat: false, integerMode: true, exclusive: true, transport: "Usb", resampled: false, eq: null, crossfeed: null, sourceLossy: false, headroomDb: null, headroomMeasured: true, restoreFromHz: null, dsdRate: null, dop: false, dsdNote: null, bitPerfect: true } as T;
  }
  throw new Error(`mock: unknown command ${cmd}`);
}
