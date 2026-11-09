// Typed wrapper over the Rust commands in src-tauri/src/lib.rs.
// Outside Tauri (plain browser, for layout work) it falls back to mock data.
import { invoke } from "@tauri-apps/api/core";
import { mock, mockCover } from "./mock";

export interface Device {
  uid: string;
  name: string;
  transport: string;
  channels: number;
  rates: number[];
  nominalRate: number;
  isDefault: boolean;
  bitPerfectCapable: boolean;
  /** Highest DSD the device could take as DoP (64, 128, 256…), if any. */
  maxDop: number | null;
}

export interface Signal {
  sourceRate: number;
  sourceBits: number | null;
  deviceRate: number;
  dacBits: number | null;
  dacFloat: boolean;
  integerMode: boolean;
  exclusive: boolean;
  transport: string;
  resampled: boolean;
  eq: string | null;
  crossfeed: Crossfeed | null;
  sourceLossy: boolean;
  headroomDb: number | null;
  headroomMeasured: boolean;
  restoreFromHz: number | null;
  /** DSD source: its 1-bit rate in Hz. */
  dsdRate: number | null;
  /** DSD goes to the DAC as DoP (otherwise it was converted to PCM). */
  dop: boolean;
  /** Why DoP was asked for but this file plays as PCM. */
  dsdNote: string | null;
  bitPerfect: boolean;
}

export type Crossfeed = "light" | "medium" | "strong";

export interface EqInfo {
  name: string;
  preamp: number;
  bands: { kind: "peak" | "lowShelf" | "highShelf"; freq: number; gain: number; q: number }[];
  curve: [number, number][];
}

export interface DspSettings {
  upsampleKhz: number | null;
  crossfeed: Crossfeed | null;
  eqPath: string | null;
  /** Lossy files only: lower the level by exactly the decoded overshoot. */
  headroom: boolean;
  /** Lossy files only: synthesise the octave above the encoder's low-pass. */
  restoreHighs: boolean;
}

export interface DspResult {
  signal: Signal | null;
  eq: EqInfo | null;
}

export interface NowPlaying {
  path: string;
  title: string;
  artist: string | null;
  album: string | null;
  year: string | null;
  cover: string | null;
  channels: number;
  duration: number;
  deviceName: string;
  signal: Signal;
  /** The status nowRev this track started as. */
  rev: number;
}

export interface Status {
  active: boolean;
  position: number;
  duration: number;
  paused: boolean;
  finished: boolean;
  peakDb: number | null;
  underruns: number;
  buffering: boolean;
  buffered: number;
  error: string | null;
  /** Queue position of the playing track; the backend moves it on at the end of a track. */
  queueIndex: number | null;
  /** Changes whenever a different track starts — then fetch nowPlaying(). */
  nowRev: number;
  /** The queue stopped at a track it couldn't play. */
  advanceError: string | null;
  /** iOS: system volume 0…1 (below 1 iOS may scale the samples). Null elsewhere. */
  systemVolume: number | null;
}

export interface LibraryStatus {
  folders: string[];
  scanning: [number, number] | null;
  albums: number;
  tracks: number;
  failed: number;
  revision: number;
}

export interface Album {
  id: string;
  title: string;
  artist: string;
  year: string | null;
  genre: string | null;
  tracks: number[];
  duration: number;
  maxRate: number;
  maxBits: number | null;
  format: string;
  hiRes: boolean;
  hasCover: boolean;
  trackCount: number;
}

export interface LibTrack {
  path: string;
  title: string;
  artist: string | null;
  albumArtist: string | null;
  album: string | null;
  genre: string | null;
  year: string | null;
  trackNumber: number | null;
  discNumber: number | null;
  duration: number;
  sampleRate: number;
  bits: number | null;
  channels: number;
  format: string;
  lossless: boolean;
  hasCover: boolean;
  size: number;
  mtime: number;
  albumId: string;
  hiRes: boolean;
}

/** A request to show something in the library ("go to this artist / album"). */
export interface LibraryGoto {
  kind: "artist" | "album";
  /** The playing file — the surest way to find its album in the library. */
  path: string;
  artist: string | null;
  album: string | null;
  /** Bumped on every request, so asking for the same thing twice still navigates. */
  n: number;
}

export const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

export const isWindows = typeof navigator !== "undefined" && navigator.userAgent.includes("Windows");
/** iPhone / iPad (an iPad asks for desktop sites and says "Macintosh", but has a touch screen). */
export const isIOS =
  typeof navigator !== "undefined" &&
  (/iPhone|iPad|iPod/.test(navigator.userAgent) ||
    (navigator.userAgent.includes("Mac") && navigator.maxTouchPoints > 1) ||
    // layout work in a desktop browser: append ?ios to the URL
    (!("__TAURI_INTERNALS__" in window) && new URLSearchParams(location.search).has("ios")));
export const isMac = typeof navigator !== "undefined" && navigator.userAgent.includes("Mac") && !isIOS;
/** The system mixer that shared mode goes through. */
export const OS_MIXER = isWindows ? "микшер Windows" : isIOS ? "микшер iOS" : "общий микшер macOS";

function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  return inTauri ? invoke<T>(cmd, args) : mock<T>(cmd, args);
}

export const api = {
  listDevices: () => call<Device[]>("list_devices"),
  expandPaths: (paths: string[]) => call<string[]>("expand_paths", { paths }),
  playFile: (path: string, deviceUid: string, exclusive: boolean, startAt = 0, dop = false, index: number | null = null, paused = false) =>
    call<NowPlaying>("play_file", { path, deviceUid, exclusive, startAt, dop, index, paused }),
  setQueue: (paths: string[], index: number | null, rev: number) => call<void>("set_queue", { paths, index, rev }),
  nowPlaying: () => call<NowPlaying | null>("now_playing"),
  importFiles: (paths: string[]) => call<string[]>("import_files", { paths }),
  setDsp: (s: DspSettings) => call<DspResult>("set_dsp", { ...s }),
  channelTest: (deviceUid: string) => call<Signal>("channel_test", { deviceUid }),
  status: () => call<Status>("status"),
  libraryStatus: () => call<LibraryStatus>("library_status"),
  libraryAlbums: () => call<Album[]>("library_albums"),
  libraryTracks: () => call<LibTrack[]>("library_tracks"),
  libraryAddFolder: (path: string) => call<LibraryStatus>("library_add_folder", { path }),
  libraryRemoveFolder: (path: string) => call<LibraryStatus>("library_remove_folder", { path }),
  libraryRescan: () => call<LibraryStatus>("library_rescan"),
  meter: () => call<[number, number]>("meter"),
  setPaused: (paused: boolean) => call<void>("set_paused", { paused }),
  seek: (seconds: number) => call<void>("seek", { seconds }),
  stop: () => call<void>("stop"),
};

export const AUDIO_EXTENSIONS = ["flac", "wav", "aif", "aiff", "m4a", "alac", "mp3", "ogg", "caf", "mka", "dsf", "dff"];

export function isAudioPath(p: string): boolean {
  const ext = p.split(".").pop()?.toLowerCase() ?? "";
  return AUDIO_EXTENSIONS.includes(ext);
}

export function khz(hz: number): string {
  return (hz / 1000).toLocaleString("ru-RU", { maximumFractionDigits: 1 });
}

export function clock(secs: number): string {
  const s = Math.max(0, Math.floor(secs));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

export function fileName(p: string): string {
  return p.split(/[\\/]/).pop()?.replace(/\.[^.]+$/, "") ?? p;
}

/** File name without a leading track number ("01 So What" → "So What"). */
export function trackName(p: string): string {
  const name = fileName(p);
  return name.replace(/^\d{1,3}(?:[\s._-]+|$)/, "") || name;
}

/** URL of an album's artwork (served by the backend's cover:// protocol). */
export function coverUrl(albumId: string): string {
  if (!inTauri) return mockCover(albumId);
  // Tauri serves custom schemes as scheme://localhost on macOS/Linux, http://scheme.localhost on Windows.
  return navigator.userAgent.includes("Windows") ? `http://cover.localhost/${albumId}` : `cover://localhost/${albumId}`;
}

/** "DSD64", "DSD256" from a 1-bit rate (Hz). */
export function dsdName(rate: number): string {
  const base = rate % 44100 === 0 ? 44100 : 48000;
  return `DSD${Math.round(rate / base)}`;
}

/** "24/96", "16/44,1" — the shorthand audiophiles read formats in. DSD (1 bit): "DSD64". */
export function resolution(bits: number | null, rate: number): string {
  if (bits === 1) return dsdName(rate);
  return `${bits ?? "?"}/${khz(rate)}`;
}

export function longDuration(secs: number): string {
  const m = Math.round(secs / 60);
  return m >= 60 ? `${Math.floor(m / 60)} ч ${m % 60} мин` : `${m} мин`;
}

export const CROSSFEED_LABEL: Record<Crossfeed, string> = {
  light: "лёгкий",
  medium: "средний",
  strong: "сильный",
};

export const TRANSPORT_LABEL: Record<string, string> = {
  Usb: "USB",
  BuiltIn: "Встроенный",
  Bluetooth: "Bluetooth",
  Hdmi: "HDMI",
  DisplayPort: "DisplayPort",
  Thunderbolt: "Thunderbolt",
  AirPlay: "AirPlay",
  Aggregate: "Агрегатное",
  Virtual: "Виртуальное",
};
