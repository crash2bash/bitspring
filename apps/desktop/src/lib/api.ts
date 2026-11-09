// Typed wrapper over the Rust commands in src-tauri/src/lib.rs.
// Outside Tauri (plain browser, for layout work) it falls back to mock data.
import { invoke } from "@tauri-apps/api/core";
import { mock, mockCover } from "./mock";
import { num, t } from "./i18n.svelte";

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
  dsdNote: DsdNote | null;
  bitPerfect: boolean;
}

export type Crossfeed = "light" | "medium" | "strong";

/** Why a DSD file plays as PCM although DoP was asked for (worded by dsdNoteText). */
export type DsdNote =
  | { kind: "exclusive" }
  | { kind: "volume" }
  | { kind: "dsp" }
  | { kind: "rate"; device: string; khz: number; dsd: string };

/** The queue stopped at a track it couldn't play. */
export interface AdvanceError {
  name: string;
  error: EngineError;
}

/**
 * An error from the engine, as a code with parameters (hifi_engine::UserError), worded by
 * errorText(). Commands reject with one; Status.error and AdvanceError.error carry one.
 */
export type EngineError =
  | {
      code:
        | "deviceBusy" | "audioBusy" | "deviceUnplugged" | "deviceStalled" | "deviceNotFound" | "noDevice"
        | "formatRefused" | "exclusiveDisabled" | "dopVolume" | "profileEmpty" | "noAppFolder"
        | "playlistNotFound" | "playlistNoTracks" | "playlistFormat" | "streamingOff" | "signInCancelled";
    }
  | { code: "audioSetup" | "outputFailed" | "profileUnreadable" | "playlistUnreadable" | "writeFailed"; detail: string }
  | { code: "signInNeeded" | "signInExpired" | "streamPreview" | "streamEncrypted" | "streamRestricted" | "streamChanged"; service: string }
  | { code: "serviceUnreachable"; service: string; detail: string }
  | { code: "noRates"; device: string }
  | { code: "rateRefused"; khz: number }
  | { code: "rateStuck"; khz: number; current: number }
  | { code: "resample"; from: number; to: number }
  | { code: "dopRefused"; device: string; dsd: string; khz: number }
  | { code: "openFailed"; device: string; detail: string }
  | { code: "otherFormat"; device: string; khz: number }
  | { code: "copyFailed"; name: string; detail: string }
  /** No code: the failing part's own words (a corrupt file, an OS error). */
  | { code: "other"; message: string };

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
  /** Playback hit a problem (decoding stopped, the device went away, iOS volume during DoP). */
  error: EngineError | null;
  /** Queue position of the playing track; the backend moves it on at the end of a track. */
  queueIndex: number | null;
  /** Changes whenever a different track starts — then fetch nowPlaying(). */
  nowRev: number;
  /** The queue stopped at a track it couldn't play. */
  advanceError: AdvanceError | null;
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

// --- playlists ---------------------------------------------------------------------------------

export interface PlaylistEntry {
  /** File path or streaming id (qobuz:track:…); null = not found when imported. */
  uri: string | null;
  title: string;
  artist: string | null;
  album: string | null;
  duration: number | null;
  isrc?: string | null;
}

export interface PlaylistSummary {
  id: string;
  name: string;
  count: number;
  duration: number;
  updated: number;
}

export interface PlaylistDetail {
  id: string;
  name: string;
  entries: (PlaylistEntry & { available: boolean })[];
}

export interface Imported {
  id: string;
  name: string;
  total: number;
  found: number;
  missing: string[];
}

// --- streaming -----------------------------------------------------------------------------------

export type Service = "qobuz" | "tidal";
export type StreamQuality = "cd" | "hires96" | "max";

export interface Account {
  service: Service;
  signedIn: boolean;
  user: string | null;
  note: string | null;
}

export interface StreamingStatus {
  /** This build has the services at all (never in App Store builds). */
  available: boolean;
  accounts: Account[];
  quality: StreamQuality;
}

export interface STrack {
  uri: string;
  service: Service | null;
  id: string;
  title: string;
  artist: string | null;
  album: string | null;
  albumId: string | null;
  albumArtist: string | null;
  year: string | null;
  duration: number;
  trackNumber: number | null;
  discNumber: number | null;
  cover: string | null;
  isrc: string | null;
  bits: number | null;
  rate: number | null;
  hiRes: boolean;
  streamable: boolean;
}

export interface SAlbum {
  id: string;
  service: Service | null;
  title: string;
  artist: string;
  year: string | null;
  genre: string | null;
  cover: string | null;
  trackCount: number;
  duration: number;
  bits: number | null;
  rate: number | null;
  hiRes: boolean;
}

export interface SPlaylist {
  id: string;
  service: Service | null;
  name: string;
  trackCount: number;
  cover: string | null;
  owner: string | null;
}

export const SERVICE_NAME: Record<Service, string> = { qobuz: "Qobuz", tidal: "TIDAL" };

/** "qobuz" for qobuz:track:…, null for a file. */
export function serviceOf(path: string | null | undefined): Service | null {
  const m = path?.match(/^(qobuz|tidal):track:\d+$/);
  return m ? (m[1] as Service) : null;
}

/** A streaming track in the shape the track table shows. */
export function streamToLib(t: STrack): LibTrack {
  return {
    path: t.uri, title: t.title, artist: t.artist, albumArtist: t.albumArtist, album: t.album, genre: null, year: t.year,
    trackNumber: t.trackNumber, discNumber: t.discNumber, duration: t.duration, sampleRate: t.rate ?? 0, bits: t.bits,
    channels: 2, format: t.rate ? "FLAC" : t.hiRes ? "FLAC hi-res" : "FLAC", lossless: true, hasCover: !!t.cover, size: 0, mtime: 0, albumId: t.albumId ?? "", hiRes: t.hiRes,
  };
}

/** A streaming album in the shape the album tiles show. */
export function streamAlbumToLib(a: SAlbum): Album {
  return {
    id: a.id, title: a.title, artist: a.artist, year: a.year, genre: a.genre, tracks: [], duration: a.duration,
    maxRate: a.rate ?? 0, maxBits: a.bits, format: a.rate ? "FLAC" : a.hiRes ? "Hi-Res" : "FLAC", hiRes: a.hiRes,
    hasCover: !!a.cover, trackCount: a.trackCount,
  };
}

export function entryFromTrack(t: LibTrack): PlaylistEntry {
  return { uri: t.path, title: t.title, artist: t.artist ?? t.albumArtist, album: t.album, duration: t.duration || null };
}

export function entryFromStream(t: STrack): PlaylistEntry {
  return { uri: t.uri, title: t.title, artist: t.artist, album: t.album, duration: t.duration || null, isrc: t.isrc };
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
export const isAndroid =
  typeof navigator !== "undefined" &&
  (/Android/.test(navigator.userAgent) || (!("__TAURI_INTERNALS__" in window) && new URLSearchParams(location.search).has("android")));
export const isMac = typeof navigator !== "undefined" && navigator.userAgent.includes("Mac") && !isIOS && !isAndroid;
/** The system mixer that shared mode goes through. */
export function osMixer(): string {
  return t(isWindows ? "mixer.windows" : isIOS ? "mixer.ios" : isAndroid ? "mixer.android" : "mixer.mac");
}

function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  return inTauri ? invoke<T>(cmd, args) : mock<T>(cmd, args);
}

export const api = {
  listDevices: () => call<Device[]>("list_devices"),
  systemLocales: () => call<string[]>("system_locales"),
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
  playlists: () => call<PlaylistSummary[]>("playlists_list"),
  playlist: (id: string) => call<PlaylistDetail>("playlist_get", { id }),
  playlistCreate: (name: string, entries: PlaylistEntry[]) => call<PlaylistSummary>("playlist_create", { name, entries }),
  playlistUpdate: (id: string, name: string | null, entries: PlaylistEntry[] | null) => call<PlaylistSummary>("playlist_update", { id, name, entries }),
  playlistAdd: (id: string, entries: PlaylistEntry[]) => call<PlaylistSummary>("playlist_add", { id, entries }),
  playlistDelete: (id: string) => call<void>("playlist_delete", { id }),
  playlistImportFile: (path: string) => call<Imported[]>("playlist_import_file", { path }),
  playlistImportText: (text: string, name: string) => call<Imported[]>("playlist_import_text", { text, name }),
  playlistExport: (id: string, path: string | null, format: string | null = null) => call<string>("playlist_export", { id, path, format }),
  streamingStatus: () => call<StreamingStatus>("streaming_status"),
  streamingSetQuality: (quality: StreamQuality) => call<void>("streaming_set_quality", { quality }),
  streamingLogin: (service: Service) => call<Account[]>("streaming_login", { service }),
  streamingLogout: (service: Service) => call<void>("streaming_logout", { service }),
  streamingSearch: (service: Service, query: string) => call<{ albums: SAlbum[]; tracks: STrack[] }>("streaming_search", { service, query }),
  streamingAlbum: (service: Service, id: string) => call<{ album: SAlbum; tracks: STrack[] }>("streaming_album", { service, id }),
  streamingFavorites: (service: Service) => call<SAlbum[]>("streaming_favorites", { service }),
  streamingPlaylists: (service: Service) => call<SPlaylist[]>("streaming_playlists", { service }),
  streamingPlaylist: (service: Service, id: string) => call<{ playlist: SPlaylist; tracks: STrack[] }>("streaming_playlist", { service, id }),
  streamingImportPlaylist: (service: Service, id: string) => call<Imported>("streaming_import_playlist", { service, id }),
  streamingFindMissing: (playlistId: string, service: Service) => call<number>("streaming_find_missing", { playlistId, service }),
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
  return num(hz / 1000);
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
  // Tauri serves custom schemes as scheme://localhost on Apple platforms and Linux,
  // http://scheme.localhost on Windows and Android.
  return isWindows || isAndroid ? `http://cover.localhost/${albumId}` : `cover://localhost/${albumId}`;
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
  return m >= 60 ? t("dur.hm", { h: num(Math.floor(m / 60)), m: num(m % 60) }) : t("dur.m", { m: num(m) });
}

/** The library's name for music without an artist tag (set by the engine). */
export const UNKNOWN_ARTIST = "Unknown artist";

/** An artist name as shown: the untagged placeholder in the UI language. */
export function artistLabel(name: string): string {
  return name === UNKNOWN_ARTIST ? t("lib.unknownArtist") : name;
}

/** Built-in outputs the phone backends name themselves (the rest carry the OS's own names). */
const DEVICE_KEYS: Record<string, string> = {
  Speaker: "dev.speaker",
  Headphones: "dev.headphones",
  Dock: "dev.dock",
  "Audio output": "dev.output",
};

/** A device name as shown. */
export function deviceLabel(name: string): string {
  const key = DEVICE_KEYS[name.trim()];
  return key ? t(key) : name;
}

/** Transports whose names are the same in every language; the others are translated. */
const TRANSPORT_NAMES: Record<string, string> = {
  Usb: "USB",
  Bluetooth: "Bluetooth",
  Hdmi: "HDMI",
  DisplayPort: "DisplayPort",
  Thunderbolt: "Thunderbolt",
  AirPlay: "AirPlay",
};
const TRANSPORT_KEYS = new Set(["BuiltIn", "Aggregate", "Virtual"]);

/** "USB", "Built-in" … for a device's transport. */
export function transportLabel(transport: string): string {
  return TRANSPORT_NAMES[transport] ?? (TRANSPORT_KEYS.has(transport) ? t(`transport.${transport}`) : transport);
}

/** Why DoP isn't used, in the UI language. */
export function dsdNoteText(n: DsdNote): string {
  if (n.kind === "rate") return t("dsd.rate", { device: deviceLabel(n.device), rate: num(n.khz), dsd: n.dsd });
  return t(`dsd.${n.kind}`);
}

/**
 * Any error a command rejects with, in the UI language: an engine code is worded from the
 * error.* keys; plain text (from Tauri itself or a plugin) is shown as it is.
 */
export function errorText(e: unknown): string {
  if (e && typeof e === "object" && "code" in e) {
    const err = e as EngineError;
    switch (err.code) {
      case "other":
        return err.message;
      case "rateRefused":
        return t("error.rateRefused", { rate: num(err.khz) });
      case "rateStuck":
        return t("error.rateStuck", { rate: num(err.khz), current: num(err.current) });
      case "resample":
        return t("error.resample", { from: num(err.from, { useGrouping: false }), to: num(err.to, { useGrouping: false }) });
      case "dopRefused":
        return t("error.dopRefused", { device: deviceLabel(err.device), dsd: err.dsd, rate: num(err.khz) });
      case "otherFormat":
        return t("error.otherFormat", { device: deviceLabel(err.device), rate: num(err.khz) });
      case "noRates":
        return t("error.noRates", { device: deviceLabel(err.device) });
      case "openFailed":
        return t("error.openFailed", { device: deviceLabel(err.device), detail: err.detail });
      case "copyFailed":
        return t("error.copyFailed", { name: err.name, detail: err.detail });
      case "audioSetup":
      case "outputFailed":
      case "profileUnreadable":
      case "playlistUnreadable":
      case "writeFailed":
        return t(`error.${err.code}`, { detail: err.detail });
      case "signInNeeded":
      case "signInExpired":
      case "streamPreview":
      case "streamEncrypted":
      case "streamRestricted":
      case "streamChanged":
        return t(`error.${err.code}`, { service: err.service });
      case "serviceUnreachable":
        return t("error.serviceUnreachable", { service: err.service, detail: err.detail });
      default:
        return t(`error.${err.code}`);
    }
  }
  if (e instanceof Error) return e.message;
  return String(e);
}
