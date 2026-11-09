// Browser-only stand-in for the Rust backend, so the layout can be built and reviewed
// without Tauri. Never used inside the app.
import { isIOS } from "./api";
import type { Album, Device, DspSettings, EqInfo, LibTrack, NowPlaying, Signal, Status } from "./api";

// ---- library ----------------------------------------------------------------------------------
/** The engine's name for untagged music (api.ts UNKNOWN_ARTIST; not imported: api.ts imports this file). */
const UNKNOWN_ARTIST = "Unknown artist";
const LIB: [string, string, string | null, string | null, string, number, number | null, string | null, string[]][] = [
  ["Ada Marlow", "Paper Moons", "2017", "Folk", "ALAC", 48000, 24, "#6b5d2e", ["Kite Strings", "Harbour", "Paper Moons"]],
  ["Echo Field", "Signal Path", "2020", "Electronic", "MP3", 44100, null, null, ["Carrier", "Sideband", "Noise Floor"]],
  ["Halvorsen Quartet", "Strings in Winter", "2021", "Classical", "FLAC", 192000, 24, "#2e3b6b", ["I. Allegro", "II. Adagio", "III. Scherzo", "IV. Finale"]],
  ["Northern Lights Trio", "Quiet Hours", "2019", "Jazz", "FLAC", 96000, 24, "#2b4a5c", ["Morning Glass", "Slow Current", "Blue Hour", "Afterglow"]],
  ["Northern Lights Trio", "City Rain", "2022", "Jazz", "FLAC", 44100, 16, "#5c3a2b", ["Neon Puddles", "Last Tram", "Umbrella Song"]],
  [UNKNOWN_ARTIST, "Untitled Sketches", null, null, "WAV", 44100, 16, null, ["01 sketch one", "02 sketch two"]],
];
const libTracks: LibTrack[] = [];
const libAlbums: Album[] = LIB.map(([artist, title, year, genre, format, rate, bits, color, names], ai) => {
  const id = `a${ai}`;
  const idx = names.map((name, n) => {
    libTracks.push({
      path: `/Music/${artist}/${title}/${String(n + 1).padStart(2, "0")} ${name}.${format.toLowerCase()}`,
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
    const dsdNote: Signal["dsdNote"] = !dopWanted || dop ? null : !exclusive ? { kind: "exclusive" } : dspOn ? { kind: "dsp" } : { kind: "rate", device: dev.name, khz: 176.4, dsd: "DSD64" };
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

let mockQueue: string[] = [];
let mockIndex: number | null = null;
let mockRev = 0;
let mockNow: NowPlaying | null = null;
let started = 0;
let paused = false;
let pausedAt = 0;
let duration = 0;

// --- playlists and streaming (layout work only) ---
type MockEntry = { uri: string | null; title: string; artist: string | null; album: string | null; duration: number | null };
const mockLists: { id: string; name: string; updated: number; entries: MockEntry[] }[] = [];
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
  return { id: p.id, name: p.name, count: p.entries.length, duration: p.entries.reduce((s, e) => s + (e.duration ?? 0), 0), updated: p.updated };
}

export async function mock<T>(cmd: string, args: Record<string, unknown> = {}): Promise<T> {
  const now = () => (paused ? pausedAt : (performance.now() - started) / 1000);
  switch (cmd) {
    case "list_devices":
      return devices as T;
    case "system_locales":
      return [...navigator.languages] as T;
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
    case "now_playing":
      return mockNow as T;
    case "import_files":
      return args.paths as T;
    case "status": {
      // Like the backend: at the end of a track the queue moves on by itself.
      if (duration > 0 && !paused && now() >= duration && mockIndex !== null && mockIndex + 1 < mockQueue.length && lastDevice) {
        await mock("play_file", { path: mockQueue[mockIndex + 1], deviceUid: lastDevice.uid, exclusive: lastExclusive, dop: lastDop, index: mockIndex + 1 });
      }
      const processing = !!(dsp.upsampleKhz || dsp.eqPath || dsp.crossfeed);
      const s: Status = {
        active: duration > 0, position: Math.min(now(), duration), duration, paused,
        finished: now() >= duration && duration > 0, peakDb: processing ? -3.4 : null, underruns: 0,
        buffering: false, buffered: duration, error: null,
        queueIndex: mockIndex, nowRev: mockRev, advanceError: null, systemVolume: isIOS ? 0.8 : null,
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
    case "playlists_list":
      return [...mockLists].sort((a, b) => b.updated - a.updated).map(mockSummary) as T;
    case "playlist_get": {
      const p = mockLists.find((l) => l.id === args.id)!;
      return { id: p.id, name: p.name, entries: p.entries.map((e) => ({ ...e, available: !!e.uri })) } as T;
    }
    case "playlist_create": {
      const p = { id: String(Date.now()), name: args.name as string, updated: Date.now(), entries: args.entries as MockEntry[] };
      mockLists.push(p);
      return mockSummary(p) as T;
    }
    case "playlist_update": {
      const p = mockLists.find((l) => l.id === args.id)!;
      if (args.name) p.name = args.name as string;
      if (args.entries) p.entries = args.entries as MockEntry[];
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
      return { available: !isIOS, quality: mockQuality, accounts: ["qobuz", "tidal"].map((s) => ({ service: s, signedIn: mockSignedIn[s], user: mockSignedIn[s] ? "me@example.com" : null, note: null })) } as T;
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
