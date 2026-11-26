<script lang="ts">
  import { onMount, untrack } from "svelte";
  import { trackSafeArea } from "$lib/safeArea";
  import { api, clock, isNetworkUid, inTauri, isMac, isIOS, isAndroid, isLinux, khz, dsdName, artistLabel, errorText, AUDIO_EXTENSIONS, type Device, type DspSettings, type DsdStep, type FilterStep, type UpsampleFilter, type LibraryGoto, type EqState, type IrInfo, type NowPlaying, type PlaylistEntry, type Status, type StreamingStatus, type RemoteCommand } from "$lib/api";
  import { i18n, sentences, t } from "$lib/i18n.svelte";
  import Settings from "$lib/Settings.svelte";
  import Heart from "$lib/Heart.svelte";
  import Stars from "$lib/Stars.svelte";
  import { loadMarks, markable, marks, setFavorite, setRating } from "$lib/marks.svelte";
  import { labels, labelOf, remember } from "$lib/labels.svelte";
  import AddToPlaylist from "$lib/AddToPlaylist.svelte";
  import Library from "$lib/Library.svelte";
  import Streaming from "$lib/Streaming.svelte";
  import DevicePicker from "$lib/DevicePicker.svelte";
  import Display from "$lib/Display.svelte";
  import LyricsView from "$lib/Lyrics.svelte";
  import Enhancers from "$lib/Enhancers.svelte";
  import Equalizer from "$lib/Equalizer.svelte";
  import UpdateNotice from "$lib/UpdateNotice.svelte";
  import CrashNotice from "$lib/CrashNotice.svelte";
  import EqHint from "$lib/EqHint.svelte";
  import RemoteButton from "$lib/RemoteButton.svelte";
  import { loadRemote } from "$lib/remote.svelte";
  import { abx } from "$lib/abx.svelte";
  import AbxDialog from "$lib/AbxDialog.svelte";
  import { startUpdateChecks, updateNoticeShown } from "$lib/updates.svelte";
  import { look } from "$lib/look.svelte";
  import { fade } from "svelte/transition";
  import { liquidPill } from "$lib/liquid";
  import { fitRow } from "$lib/fitRow";
  import { startGamepad } from "$lib/gamepad";
  import { nextRepeat, shuffledOrder, straightOrder, type Repeat } from "$lib/playOrder";

  // --- persisted preferences (per machine) ---
  function load<T>(key: string, fallback: T): T {
    try {
      const v = localStorage.getItem(key);
      return v === null ? fallback : (JSON.parse(v) as T);
    } catch {
      return fallback;
    }
  }
  function save(key: string, v: unknown) {
    try {
      localStorage.setItem(key, JSON.stringify(v));
    } catch {}
  }

  let devices = $state<Device[]>([]);
  let selected = $state<string>(load("device", ""));
  /** The output that was picked when the app was last left, as saved: whose the EQ of an earlier
   *  version is (see `adoptEq`), even if it isn't connected now. */
  const launchDevice = untrack(() => selected);
  // iOS has no shared/exclusive choice: our playback session never mixes with other apps.
  let exclusive = $state<boolean>(isIOS ? true : load("exclusive", true));
  /** The settings key of output `uid` (see `Device.key`): the list's, else the uid itself. */
  function keyOf(uid: string): string {
    return devices.find((d) => d.uid === uid)?.key ?? uid;
  }
  let selectedKey = $derived(keyOf(selected));
  /** Devices the user switched DoP on for (by settings key, which is the UID but on Android):
   *  DSD goes to them as DoP, not PCM. */
  let dopDevices = $state<string[]>(load("dopDevices", []));
  let dop = $derived(dopDevices.includes(selectedKey));

  let queue = $state<string[]>([]);
  let index = $state(-1);
  /** Each queue entry's place in the order the tracks were added in: what shuffle comes back to. */
  let straight: number[] = [];
  let nextPlace = 0;
  /** Shuffle and repeat stay as they were left, from launch to launch. */
  let shuffle = $state<boolean>(load("shuffle", false));
  let repeat = $state<Repeat>(load("repeat", "off"));
  let canNext = $derived(index < queue.length - 1 || (repeat === "all" && queue.length > 0));
  let now = $state<NowPlaying | null>(null);
  let status = $state<Status>({ active: false, position: 0, duration: 0, paused: false, finished: false, peakDb: null, underruns: 0, buffering: false, buffered: 0, error: null, queueIndex: null, nowRev: 0, advanceError: null, systemVolume: null, load: null, filterStep: null, dsdStep: null, crossfading: false });
  /** The backend's nowRev we are showing; when it moves, the queue went on by itself. */
  let knownRev = 0;
  let lastOutputError: string | null = null;
  // `dsp` is the one set of processing settings of earlier versions (now each output has its
  // own, under `dspByDevice`). Its `eqPath` is their single AutoEQ file: moved into the preset
  // store on start.
  const savedDsp = load<Partial<DspSettings> & { eqPath?: string | null }>("dsp", {});
  const legacyEqPath = savedDsp.eqPath ?? null;
  delete savedDsp.eqPath;
  /** What an output starts with: nothing that changes the signal (the two headroom switches act
   *  only on lossy files, or while other processing runs). The backend has the same. */
  const DSP_DEFAULTS: DspSettings = { upsampleKhz: null, upsampleFilter: null, upsampleTaps: 65536, crossfeed: null, convolution: null, headroom: true, restoreHighs: false, level: null, peakHeadroom: true, dsdOut: null, dsdOrder: 7 };
  type DspEntry = { name: string; settings: DspSettings };
  /** Each output's processing, by settings key, with the output's name as last seen (to offer
   *  the settings of an output that isn't connected now). An output without one has the defaults. */
  const savedByDevice = load<Record<string, DspEntry> | null>("dspByDevice", null);
  let dspByDevice = $state<Record<string, DspEntry>>(savedByDevice ?? {});
  /** The earlier versions' settings, until they become the picked output's (once, when the
   *  device list is in and the picked output is in it). The old item stays as it was. */
  let legacyDsp: DspSettings | null = savedByDevice === null && load<unknown>("dsp", null) !== null ? { ...DSP_DEFAULTS, ...savedDsp } : null;
  // They belong to the output that was picked when they were made, connected now or not (a DAC
  // unplugged at the first start after the update must not lose them to the speakers picked in
  // its place). Its uid is its settings key everywhere but on Android, where the saved uid says
  // nothing after a replug: there they wait for the device list. The name comes with the list.
  function sameDsp(a: DspSettings, b: DspSettings): boolean {
    return (Object.keys(DSP_DEFAULTS) as (keyof DspSettings)[]).every((k) => a[k] === b[k]);
  }
  // (Settings nobody ever changed are nobody's: every output starts with those anyway.)
  if (legacyDsp && sameDsp(legacyDsp, DSP_DEFAULTS)) legacyDsp = null;
  if (legacyDsp && untrack(() => selected) && !untrack(() => selected).startsWith("android:")) {
    dspByDevice[untrack(() => selected)] = { name: untrack(() => selected), settings: legacyDsp };
    legacyDsp = null;
    save("dspByDevice", $state.snapshot(dspByDevice));
  }
  /** The settings of the output with settings key `key`. */
  function dspOf(key: string): DspSettings {
    const e = dspByDevice[key];
    return e ? { ...DSP_DEFAULTS, ...e.settings } : DSP_DEFAULTS;
  }
  /** Keep `settings` as the output `key`'s own. */
  function storeDsp(key: string, settings: DspSettings) {
    const name = devices.find((d) => d.key === key)?.name ?? dspByDevice[key]?.name ?? key;
    dspByDevice[key] = { name, settings: { ...settings } };
    // Settings of their own now: the earlier versions' must not land on top of them.
    legacyDsp = null;
    save("dspByDevice", $state.snapshot(dspByDevice));
  }
  /** What the processing panel shows and edits: the picked output's. */
  let dsp = $derived(dspOf(selectedKey));
  /** The name to show for an output that has settings: as last seen. One that hasn't been
   *  connected since an earlier version's settings became its own has no name yet (those
   *  versions kept none, so its key stands in): a Mac's USB uid carries the product's name
   *  ("AppleUSBAudioEngine:maker:product:serial:n"), anything else is "the output used before". */
  function storedName(key: string, name: string | undefined): string {
    if (name && name !== key) return name;
    const usb = key.startsWith("AppleUSBAudioEngine:") ? key.split(":") : [];
    return (usb.length === 5 && usb[2].trim()) || t("enh.earlierOutput");
  }
  /** The picked output's name, as the list has it or as it was last seen. */
  let dspDeviceName = $derived(devices.find((d) => d.uid === selected)?.name ?? (dspByDevice[selectedKey] ? storedName(selectedKey, dspByDevice[selectedKey].name) : null));
  /** The other outputs with settings of their own, to copy from. */
  let dspOthers = $derived(
    Object.entries(dspByDevice)
      .filter(([k]) => k !== selectedKey)
      .map(([key, e]) => ({ key, name: storedName(key, e.name) }))
      .sort((a, b) => a.name.localeCompare(b.name)),
  );
  /** The settings of the picked output have been sent to the backend since start-up. */
  let dspPushed = false;
  /** The backend's last automatic filter step taken over (its seq). */
  let stepSeen: number | null = null;
  /** The filter that step left on, until the user picks one: the panel says why it changed. */
  let filterStepped = $state<{ filter: UpsampleFilter | null; taps: number | null } | null>(null);
  /** The same for the DSD output: its last automatic step taken over, and what it left on (and
   *  whether it lowered the rate, took the lighter modulator or turned the output off). */
  let dsdStepSeen: number | null = null;
  let dsdStepped = $state<{ dsdOut: number | null; order: number | null; kind: "rate" | "order" | "off" } | null>(null);
  let ir = $state<IrInfo | null>(null);
  let eqState = $state<EqState | null>(null);
  /** The start-up adoption of an earlier version's EQ has run: the EQ screen, if it was up
   *  already, is put up anew so it shows the state after it, not the one it read before. */
  let eqAdopted = $state(false);
  /** The EQ preset that plays on the picked output (null = EQ off there). An output without a
   *  binding plays without, except in an earlier version's state before `adoptEq`. */
  let eqName = $derived.by(() => {
    if (!eqState) return null;
    const b = eqState.devices[selectedKey];
    const id = b ? (b.mode === "preset" ? b.id : null) : !eqState.perDevice && eqState.enabled ? eqState.activeId : null;
    return eqState.presets.find((p) => p.id === id)?.name ?? null;
  });

  // ---- the headphone hint: Bluetooth headphones we have correction profiles for ----
  /** Outputs (by settings key) not to offer headphone profiles for again. */
  const savedHintDismissed = load<unknown>("eqHintDismissed", []);
  let eqHintDismissed = $state<string[]>(Array.isArray(savedHintDismissed) ? savedHintDismissed.filter((k) => typeof k === "string") : []);
  function dismissEqHint(key: string) {
    if (eqHintDismissed.includes(key)) return;
    eqHintDismissed = [...eqHintDismissed, key];
    save("eqHintDismissed", eqHintDismissed);
  }
  /** The picked output, when profiles may be offered for it: in the list, on Bluetooth, its EQ
   *  never set up there (no binding and nothing it last played: an output that was ever switched
   *  on or off has been seen to), and not declined. Only once the EQ state is in and adopted, so
   *  the state of an earlier version isn't taken for an output that never had one. */
  let eqHintFor = $derived.by((): Device | null => {
    if (!eqAdopted || !eqState) return null;
    const d = devices.find((d) => d.uid === selected);
    if (!d || d.transport !== "Bluetooth") return null;
    if (eqState.devices[d.key] || eqState.last[d.key] || eqHintDismissed.includes(d.key)) return null;
    return d;
  });
  /** The backend's answer for an output: the model to search the profiles for, if any. */
  let eqHintAnswer = $state<{ key: string; model: string | null } | null>(null);
  let eqHintAsked = 0;
  $effect(() => {
    const d = eqHintFor;
    // A later question makes the earlier answer stale, whenever it comes.
    const seq = ++eqHintAsked;
    if (!d) return;
    api.eqSuggest(d.name, d.transport).then(
      (model) => {
        if (seq === eqHintAsked) eqHintAnswer = { key: d.key, model };
      },
      () => {},
    );
  });
  /** What the notice offers: shown only while every condition holds, for the output it was asked for. */
  let eqHint = $derived(eqHintFor && eqHintAnswer?.key === eqHintFor.key ? eqHintAnswer.model : null);
  /** The model to open the headphone picker with, on the EQ screen (cleared once it's open). */
  let eqPick = $state<string | null>(null);
  function pickFromHint(model: string) {
    // It has done its job: whatever is picked (or not), it isn't offered here again.
    dismissEqHint(selectedKey);
    eqPick = model;
    screen = "eq";
  }
  /** The crash notice is up (the headphone hint waits for it and for the update notice). */
  let crashShowing = $state(false);

  let error = $state<string | null>(null);
  let loading = $state(false);
  let testing = $state(false);
  // Desktop: the transport buttons' tooltips name their keyboard shortcuts.
  const modKey = isMac ? "⌘" : "Ctrl+";
  function hint(label: string, keys: string): string | undefined {
    return isIOS || isAndroid ? undefined : `${label} (${keys})`;
  }
  let dragOver = $state(false);
  let sweepKey = $state(0);
  let advancing = false;
  type Screen = "player" | "library" | "queue" | "streaming" | "eq" | "settings";
  let screen = $state<Screen>(load("screen", "player"));
  /** The settings open over whatever screen was up, and go back to it. */
  let behindSettings: Screen = "player";
  function toggleSettings() {
    if (screen === "settings") screen = behindSettings;
    else {
      behindSettings = screen;
      screen = "settings";
    }
  }
  /** Pause, seek and stop fade over a few milliseconds instead of clicking. */
  let fades = $state<boolean>(load("fades", true));
  function setFades(on: boolean) {
    fades = on;
    save("fades", on);
    api.setFades(on).catch(() => {});
  }
  /** Tracks of different albums cross into each other over this many seconds; 0 = off. */
  let crossfade = $state<number>(load("crossfade", 0));
  function setCrossfade(secs: number) {
    crossfade = secs;
    save("crossfade", secs);
    api.setCrossfade(secs).catch(() => {});
  }
  /** The lyrics take the stage on the player screen (kept on from track to track). */
  let lyricsOn = $state<boolean>(load("lyrics", false));
  $effect(() => save("lyrics", lyricsOn));
  let showLyrics = $derived(lyricsOn && !!now && !testing);
  /** Qobuz/TIDAL: null until known; `available` is false in App Store builds. */
  let streaming = $state<StreamingStatus | null>(null);
  async function refreshStreaming() {
    try {
      streaming = await api.streamingStatus();
    } catch {
      streaming = null;
    }
    if (!streaming?.available && screen === "streaming") screen = "library";
  }
  /** The queue as playlist entries (tags come from the library on the Rust side). */
  function queueEntries(): PlaylistEntry[] {
    return queue.map((p) => ({ uri: p, title: labelOf(p), artist: labels[p]?.artist ?? null, album: null, duration: null }));
  }
  let libGoto = $state<LibraryGoto | null>(null);
  let gotoCount = 0;
  /** Open the playing track's artist or album in the library. */
  function goToLibrary(kind: LibraryGoto["kind"]) {
    if (!now) return;
    libGoto = { kind, path: now.path, artist: now.artist, album: now.album, n: ++gotoCount };
    screen = "library";
  }
  // (Not the settings: the app opens on a screen with music in it.)
  $effect(() => {
    if (screen !== "settings") save("screen", screen);
  });
  /** The library tab on a phone is the library alone; a desktop window shows the queue beside everything. */
  const phone = typeof window !== "undefined" ? window.matchMedia("(max-width: 700px), (max-height: 500px)") : null;
  onMount(() => trackSafeArea());
  onMount(() => {
    const fit = () => {
      if (!phone?.matches && screen === "queue") screen = "player";
    };
    fit();
    phone?.addEventListener("change", fit);
    return () => phone?.removeEventListener("change", fit);
  });

  /** The glass backdrop glows with the signal's colour: brass untouched, violet processed. */
  let signalState = $derived(!now || testing ? null : now.signal.bitPerfect ? "bp" : "dsp");
  /** What the output runs at for the loaded track, as the display has it (the device list's own
   *  rate is as old as the list, and upsampling or another track has changed it since). */
  let liveRate = $derived.by(() => {
    if (!now || testing) return null;
    const s = now.signal;
    if (s.dop && s.dsdRate) return dsdName(s.dsdRate);
    if (s.dsdOut) return `DSD${s.dsdOut}`;
    return s.deviceRate > 0 ? `${khz(s.deviceRate)} ${t("unit.khz")}` : null;
  });
  // Nothing loaded any more: the picker goes back to the list's rate, so read the list again
  // (the output stays at what the last track set).
  let hadTrack = false;
  $effect(() => {
    const has = now !== null;
    if (hadTrack && !has) untrack(() => void refreshDevices());
    hadTrack = has;
  });
  /** How much the glass backdrop darkens the cover: more over a bright one, so text keeps its contrast. */
  let coverVeil = $state(0.3);
  $effect(() => {
    const src = look.value === "glass" && !testing ? now?.cover : null;
    if (!src) return;
    let alive = true;
    coverLuminance(src).then((l) => {
      if (alive && l !== null) coverVeil = Math.min(0.62, 0.22 + l * 0.6);
    });
    return () => {
      alive = false;
    };
  });
  /** Mean relative luminance (0–1) of an image, from a 16×16 thumbnail; null if it can't be read. */
  function coverLuminance(src: string): Promise<number | null> {
    return new Promise((resolve) => {
      const img = new Image();
      img.onload = () => {
        try {
          const c = document.createElement("canvas");
          c.width = c.height = 16;
          const g = c.getContext("2d", { willReadFrequently: true })!;
          g.drawImage(img, 0, 0, 16, 16);
          const px = g.getImageData(0, 0, 16, 16).data;
          const lin = (v: number) => ((v /= 255) <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4);
          let sum = 0;
          for (let i = 0; i < px.length; i += 4) sum += 0.2126 * lin(px[i]) + 0.7152 * lin(px[i + 1]) + 0.0722 * lin(px[i + 2]);
          resolve(sum / (px.length / 4));
        } catch {
          resolve(null);
        }
      };
      img.onerror = () => resolve(null);
      img.src = src;
    });
  }

  let deviceName = $derived(devices.find((d) => d.uid === selected)?.name ?? "DAC");

  async function refreshDevices(search = isNetworkUid(selected)) {
    try {
      devices = await api.listDevices(search);
      // A network player answers a few seconds after start-up: don't give up on it.
      if (!devices.some((d) => d.uid === selected) && !isNetworkUid(selected)) {
        selected = (devices.find((d) => d.isDefault) ?? devices[0])?.uid ?? "";
      }
      // An update from a version with one set of settings, where they couldn't be given to an
      // output at once (Android): they become the picked output's.
      // (Sent at once if the start-up has sent the picked output's settings already.)
      if (legacyDsp && devices.some((d) => d.uid === selected)) {
        storeDsp(keyOf(selected), legacyDsp);
        if (dspPushed) applyDsp(dspOf(keyOf(selected)));
      }
      // Settings kept for an output take its name as the list has it now (they may have been
      // stored while it was away, or it was renamed).
      let renamed = false;
      for (const d of devices) {
        const e = dspByDevice[d.key];
        if (e && e.name !== d.name) {
          e.name = d.name;
          renamed = true;
        }
      }
      if (renamed) save("dspByDevice", $state.snapshot(dspByDevice));
    } catch (e) {
      error = t("err.devices", { error: errorText(e) });
    }
  }

  async function playIndex(i: number, startAt = 0, paused = false) {
    if (i < 0 || i >= queue.length || !selected) return;
    loading = true;
    testing = false;
    try {
      await api.setQueue($state.snapshot(queue), i, knownRev);
      now = await api.playFile(queue[i], selected, exclusive, startAt, dop, i, paused);
      knownRev = now.rev;
      index = i;
      error = null;
      sweepKey++;
    } catch (e) {
      error = t("err.play", { name: labelOf(queue[i]), error: errorText(e) });
      index = i;
    } finally {
      loading = false;
    }
  }

  async function addPaths(paths: string[]) {
    const expanded = await api.expandPaths(paths);
    if (!expanded.length) {
      error = t("err.noAudio");
      return;
    }
    // Tracks of an album ripped to one file get their names from its CUE sheet.
    remember(expanded.flatMap((f) => (f.title ? [{ path: f.path, title: f.title, artist: f.artist }] : [])));
    const files = expanded.map((f) => f.path);
    const first = queue.length;
    append(files);
    if (!status.active || status.finished) await playIndex(first);
  }

  async function pickFiles() {
    if (!inTauri) return addPaths(["/Music/Miles Davis/Kind of Blue/01 So What.flac", "/Music/Miles Davis/Kind of Blue/02 Freddie Freeloader.flac", "/Music/Miles Davis/Kind of Blue/03 Blue in Green.dsf"]);
    const { open } = await import("@tauri-apps/plugin-dialog");
    if (isIOS) {
      // The Files app knows no type for DSF/DFF, so ask for any file and filter afterwards.
      // The picker hands over temporary copies: keep them in the app's music folder.
      const picked = await open({ multiple: true });
      if (!picked) return;
      try {
        await addPaths(await api.importFiles(Array.isArray(picked) ? picked : [picked]));
      } catch (e) {
        error = t("err.addFiles", { error: errorText(e) });
      }
      return;
    }
    const picked = await open({ multiple: true, filters: [{ name: t("dialog.audio"), extensions: AUDIO_EXTENSIONS }] });
    if (picked) await addPaths(Array.isArray(picked) ? picked : [picked]);
  }

  /** Add to the end of the queue — of a shuffled one too: what was just added plays last. */
  function append(paths: string[]) {
    queue.push(...paths);
    for (let i = 0; i < paths.length; i++) straight.push(nextPlace++);
  }

  function replaceQueue(paths: string[]) {
    queue = [...paths];
    straight = paths.map((_, i) => i);
    nextPlace = paths.length;
  }

  /** Put the queue in `order` (indices into the present one); the playing track stays the playing track. */
  function reorder(order: number[]) {
    const [paths, places] = [$state.snapshot(queue), straight];
    queue = order.map((i) => paths[i]);
    straight = order.map((i) => places[i]);
    if (index >= 0) index = order.indexOf(index);
  }

  /** Replace the queue with `paths` and play from `start` (library "Play"). With shuffle on, the rest follows in a random order. */
  async function playPaths(paths: string[], start: number) {
    replaceQueue(paths);
    if (shuffle) {
      index = -1;
      reorder(shuffledOrder(queue.length, start));
      start = 0;
    }
    await playIndex(start);
  }

  /** Replace the queue with `paths` in a random order and play it (library "Shuffle"): shuffle switches on. */
  async function playShuffled(paths: string[]) {
    if (!paths.length) return;
    shuffle = true;
    replaceQueue(paths);
    index = -1;
    reorder(shuffledOrder(queue.length, -1));
    // Before the first track opens: volume levelling treats a shuffled queue track by track.
    await api.setPlayModes(true, repeat).catch(() => {});
    await playIndex(0);
  }

  /** Shuffle the queue around the playing track, or put it back in the order it was added in. Playback goes on. */
  async function setShuffle(on: boolean) {
    if (on === shuffle) return;
    // The backend may have moved on to the next track a moment ago: go by what plays now.
    await catchUp();
    shuffle = on;
    reorder(on ? shuffledOrder(queue.length, index) : straightOrder(straight));
  }

  /** Append to the queue; start playing if nothing is. */
  async function enqueue(paths: string[]) {
    const first = queue.length;
    append(paths);
    if (!status.active || status.finished) await playIndex(first);
  }

  function removeAt(i: number) {
    queue.splice(i, 1);
    straight.splice(i, 1);
    if (i < index) index--;
    else if (i === index) {
      api.stop();
      now = null;
      index = -1;
    }
  }

  function clearQueue() {
    api.stop();
    replaceQueue([]);
    index = -1;
    now = null;
  }

  async function togglePause() {
    if (!status.active) {
      if (queue.length) await playIndex(Math.max(index, 0));
      return;
    }
    await api.setPaused(!status.paused);
  }

  /** With the queue on repeat its ends meet: next from the last track is the first, and back. */
  async function next() {
    if (index < queue.length - 1) await playIndex(index + 1);
    else if (repeat === "all" && queue.length) await playIndex(0);
  }

  async function prev() {
    if (status.position > 3 || (index <= 0 && repeat !== "all")) await api.seek(0);
    else await playIndex(index > 0 ? index - 1 : queue.length - 1);
  }

  /** What a blind test interrupted: it plays on from there when the test is closed. */
  let interrupted: { path: string; index: number; position: number; paused: boolean } | null = null;
  $effect(() => {
    if (!abx.request) return;
    untrack(() => {
      const playing = status.active && !status.finished && !testing && index >= 0 && index < queue.length;
      interrupted = playing ? { path: queue[index], index, position: status.position, paused: status.paused } : null;
    });
  });

  async function endAbx() {
    const was = interrupted;
    interrupted = null;
    abx.request = null;
    await api.abxStop().catch(() => {});
    // (Closed before the test had taken the output: the music never stopped.)
    if (!was || queue[was.index] !== was.path || (await api.status()).active) return;
    await playIndex(was.index, was.position, was.paused);
  }

  async function channelTest() {
    try {
      const signal = await api.channelTest(selected);
      testing = true;
      error = null;
      sweepKey++;
      now = {
        path: "", title: t("test.title"), artist: t("test.subtitle"),
        album: null, year: null, cover: null, channels: 2, duration: 3, deviceName, signal, rev: knownRev,
      };
    } catch (e) {
      error = t("err.test", { error: errorText(e) });
    }
  }

  /** The picked output's driver settings window (ASIO). Afterwards the list is read again:
   *  the driver may run at other rates now. */
  async function controlPanel() {
    try {
      await api.openControlPanel(selected);
    } catch (e) {
      error = t("err.controlPanel", { error: errorText(e) });
    }
    await refreshDevices();
  }

  /** Apply processing settings to output `uid` (the picked one unless said) and keep them as
   *  its own; a track playing there continues in place with the new chain. */
  async function applyDsp(next: DspSettings, uid = selected) {
    // No output at all: nothing to apply them to.
    if (!uid) return;
    const key = keyOf(uid);
    const before = dspOf(key);
    if (key === selectedKey) {
      if (next.upsampleFilter !== before.upsampleFilter || next.upsampleTaps !== before.upsampleTaps) filterStepped = null;
      if (next.dsdOut !== before.dsdOut || next.dsdOrder !== before.dsdOrder) dsdStepped = null;
    }
    try {
      const res = await api.setDsp(uid, next);
      // An output that has only ever had the defaults gets no entry: nothing to copy from it.
      if (dspByDevice[key] || !sameDsp(next, DSP_DEFAULTS)) storeDsp(key, next);
      // The impulse response shown is the picked output's (it may have changed meanwhile).
      if (keyOf(selected) === key) ir = res.convolution;
      if (res.signal && now && !testing) {
        now = { ...now, signal: res.signal };
        sweepKey++;
      }
      error = null;
    } catch (e) {
      error = t("err.dsp", { error: errorText(e) });
      // An unreadable impulse response shouldn't take the rest of the settings down with it.
      if (next.convolution && next.convolution !== before.convolution) return;
      if (next.convolution) await applyDsp({ ...next, convolution: null }, uid);
    }
  }

  /** Copy another output's processing (not its EQ) to the picked one. */
  function copyDspFrom(key: string) {
    const from = dspByDevice[key];
    if (from) applyDsp({ ...DSP_DEFAULTS, ...$state.snapshot(from.settings) });
  }

  async function pickIr() {
    if (!inTauri) return applyDsp({ ...dsp, convolution: "/Users/me/REW/Room correction 48k.wav" });
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({ multiple: false, ...(isIOS ? {} : { filters: [{ name: t("dialog.ir"), extensions: ["wav", "flac", "aif", "aiff", "caf"] }] }) });
    if (typeof picked === "string") await applyDsp({ ...dsp, convolution: picked });
  }

  /** The EQ changed playback (restarted it): show the new signal path. */
  function eqSignal(signal: import("$lib/api").Signal) {
    if (now && !testing) {
      now = { ...now, signal };
      sweepKey++;
    }
  }

  /** Earlier versions had one EQ for every output: it becomes the EQ of the output that was picked
   *  when it was set up, connected now or not, as the processing settings do (see `legacyDsp`).
   *  On Android the saved uid says nothing after a replug: there it goes to the output picked now.
   *  The backend does this once; every later start it does nothing. Call with the device list in.
   *  Returns the output it went to (null: no output at all, nothing done). */
  async function adoptEq(): Promise<string | null> {
    const uid = launchDevice && !launchDevice.startsWith("android:") ? launchDevice : selected;
    if (!uid) return null;
    try {
      eqState = (await api.eqAdopt(uid)).state;
    } catch {}
    return uid;
  }

  /** The EQ state for the summaries; `owner` is the output `adoptEq` gave the earlier EQ to. */
  async function loadEq(owner: string | null) {
    try {
      // The versions before those kept one AutoEQ file: it becomes the first preset, playing on
      // the same output.
      if (legacyEqPath && inTauri) {
        const s = await api.eqState();
        if (!s.presets.length) await api.eqImportFile(legacyEqPath, owner).catch(() => null);
        // The old settings as they were, without the file: imported once.
        save("dsp", savedDsp);
      }
      eqState = await api.eqState();
    } catch {
      eqState = null;
    }
  }

  // The macOS menu bar follows the UI language.
  const MENU_KEYS = ["about", "services", "hide", "hideOthers", "showAll", "quit", "edit", "undo", "redo", "cut", "copy", "paste", "selectAll", "view", "fullscreen", "window", "minimize", "zoom", "close"];
  $effect(() => {
    void i18n.locale;
    if (!isMac || !inTauri) return;
    const labels = Object.fromEntries(MENU_KEYS.map((k) => [`menu.${k}`, t(`menu.${k}`, { app: "BitSpring" })]));
    api.setMenuLabels(labels).catch(() => {});
  });

  // Mirror the queue to the backend, which starts the next track by itself.
  $effect(() => {
    api.setQueue($state.snapshot(queue), index < 0 ? null : index, knownRev);
  });

  // The backend decides what follows a track that ends, so it knows how the keys stand.
  $effect(() => {
    save("shuffle", shuffle);
    save("repeat", repeat);
    api.setPlayModes(shuffle, repeat).catch(() => {});
  });

  /** The processor couldn't keep up and the backend moved to a cheaper upsampling filter, already
   *  playing: take it into the settings of the output it played on, as they are kept (not sent
   *  back: nothing to restart). The panel says why only if that output is the picked one. */
  function adoptFilterStep(step: FilterStep | null) {
    if (!step || step.seq === stepSeen) return;
    stepSeen = step.seq;
    const cur = dspOf(step.deviceKey);
    const next: DspSettings = { ...cur, upsampleFilter: step.filter, upsampleTaps: step.taps === 1048576 ? 1048576 : step.taps === 65536 ? 65536 : cur.upsampleTaps };
    storeDsp(step.deviceKey, next);
    if (step.deviceKey === selectedKey) filterStepped = { filter: step.filter, taps: step.taps };
    // The track restarted with it: show its new signal path.
    api
      .nowPlaying()
      .then((n) => {
        if (n && now && !testing && n.path === now.path) now = { ...now, signal: n.signal };
      })
      .catch(() => {});
  }

  /** The same for the DSD output: the backend went to a lower rate, the 5th-order modulator, or
   *  PCM, already playing. */
  function adoptDsdStep(step: DsdStep | null) {
    if (!step || step.seq === dsdStepSeen) return;
    dsdStepSeen = step.seq;
    const cur = dspOf(step.deviceKey);
    // A lower rate keeps the modulator, so a change of order means the order was the step. What
    // played before tells it (with the setting for a step taken while no page was there).
    const before = now?.signal.dsdOutOrder ?? cur.dsdOrder;
    const kind = step.dsdOut === null ? "off" : step.order !== null && step.order !== before ? "order" : "rate";
    const dsdOut = step.dsdOut === 64 || step.dsdOut === 128 || step.dsdOut === 256 ? step.dsdOut : null;
    const next: DspSettings = { ...cur, dsdOut, dsdOrder: step.order === 5 ? 5 : step.order === 7 ? 7 : cur.dsdOrder };
    storeDsp(step.deviceKey, next);
    if (step.deviceKey === selectedKey) dsdStepped = { dsdOut, order: step.order, kind };
    api
      .nowPlaying()
      .then((n) => {
        if (n && now && !testing && n.path === now.path) now = { ...now, signal: n.signal };
      })
      .catch(() => {});
  }

  /** Fetch the status; when the backend has moved on to another track by itself, show it. */
  async function catchUp() {
    status = await api.status();
    adoptFilterStep(status.filterStep);
    adoptDsdStep(status.dsdStep);
    if (status.nowRev === knownRev || loading) return;
    knownRev = status.nowRev;
    const n = await api.nowPlaying();
    if (n && !testing) {
      now = n;
      sweepKey++;
    }
    if (status.queueIndex !== null) index = status.queueIndex;
    if (status.advanceError) error = t("err.play", { name: status.advanceError.name, error: errorText(status.advanceError.error) });
  }

  // Switching device or access mode mid-track: continue the same track on the new output.
  let lastOutput = untrack(() => `${selected}|${exclusive}`);
  let lastSelected = untrack(() => selected);
  $effect(() => {
    const key = `${selected}|${exclusive}`;
    save("device", selected);
    save("exclusive", exclusive);
    if (key !== lastOutput) {
      lastOutput = key;
      const moved = selected !== lastSelected;
      lastSelected = selected;
      untrack(() => switchOutput(selected, moved));
    }
  });
  /** Bumped on every switch of output: only the latest one goes on to restart the track. */
  let switches = 0;
  /** Another output (`moved`) or access mode: on another output, its own processing goes in
   *  first (the old output plays on meanwhile, untouched); then the track continues there. */
  async function switchOutput(uid: string, moved: boolean) {
    const mine = ++switches;
    if (moved) {
      filterStepped = null;
      dsdStepped = null;
      await applyDsp(dspOf(keyOf(uid)), uid);
      if (mine !== switches) return;
    }
    // Same position, same pause state: switching outputs while paused stays paused.
    if (status.active && !testing && index >= 0) playIndex(index, status.position, status.paused);
  }

  // DoP switched for the current device: only a DSD track needs to restart, or any track while
  // DSD output is chosen (it goes as DoP too). Not on a switch of output, which restarts the
  // track itself once that output's settings are in.
  let lastDop = untrack(() => dop);
  let lastDopDevice = untrack(() => selected);
  $effect(() => {
    save("dopDevices", dopDevices);
    const sameDevice = selected === lastDopDevice;
    lastDopDevice = selected;
    if (dop !== lastDop) {
      lastDop = dop;
      if (sameDevice && untrack(() => status.active && !testing && index >= 0 && (now?.signal.dsdRate !== null || dsp.dsdOut !== null)))
        playIndex(index, untrack(() => status.position), untrack(() => status.paused));
    }
  });

  function onKey(e: KeyboardEvent) {
    // A blind test is open: the keys are its own.
    if (abx.request) return;
    const t = e.target as HTMLElement;
    // Handled already (an EQ band moved by the arrow keys), or typing in a field.
    if (e.defaultPrevented || t.tagName === "SELECT" || t.tagName === "TEXTAREA") return;
    if (t.tagName === "INPUT" && (t as HTMLInputElement).type !== "range") return;
    // ⌘ on the Mac, Ctrl elsewhere — as in Music and most players.
    const mod = isMac ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey;
    if (e.code === "Space") {
      e.preventDefault();
      // Held down, the key repeats: one press, one toggle.
      if (!e.repeat) togglePause();
    } else if (mod && !e.altKey && (e.key === "ArrowRight" || e.key === "ArrowLeft")) {
      // Next / previous track (the buttons' own rules: previous restarts past 3 s).
      e.preventDefault();
      if (e.repeat || testing) return;
      if (e.key === "ArrowRight") next();
      else if (now) prev();
    } else if (e.metaKey || e.ctrlKey) {
      // Leave the system's and the menu's shortcuts alone.
      return;
    } else if (e.key === "ArrowRight" && status.active) {
      api.seek(Math.min(status.position + 10, status.duration));
    } else if (e.key === "ArrowLeft" && status.active) {
      api.seek(Math.max(status.position - 10, 0));
    } else if (e.altKey || e.repeat) {
      return;
    } else if (letter(e, "l") && now && !testing) {
      lyricsOn = !lyricsOn;
      screen = "player";
    } else if (letter(e, "f") && canMark) {
      toggleFavorite();
    } else if (letter(e, "s")) {
      setShuffle(!shuffle);
    } else if (letter(e, "r")) {
      repeat = nextRepeat(repeat);
    }
  }
  /** The playing track can carry the listener's heart (a file; not the channel test, not a
   *  streaming track). */
  let canMark = $derived(!!now && !testing && markable(now.path));
  let isFavorite = $derived(canMark && marks.fav(now!.path));
  function toggleFavorite() {
    if (canMark) setFavorite(now!.path, !isFavorite);
  }
  /** A one-letter shortcut: the letter typed, or its key where the keyboard types another script (Cyrillic, Greek…). */
  function letter(e: KeyboardEvent, l: string): boolean {
    return /^[a-z]$/i.test(e.key) ? e.key.toLowerCase() === l : e.code === `Key${l.toUpperCase()}`;
  }

  onMount(() => {
    // Restore last session's processing (also loads and validates the impulse response): the
    // devices first, so the picked output and whose settings to send are known (and the
    // earlier versions' settings have become its own), and with a step down the backend took
    // while no page was there to see it.
    (async () => {
      await refreshDevices();
      // The EQ of an earlier version needs the device list too; nothing here waits for it.
      void adoptEq()
        .then(loadEq)
        .finally(() => (eqAdopted = true));
      const s = await api.status().catch(() => null);
      if (s) {
        adoptFilterStep(s.filterStep);
        adoptDsdStep(s.dsdStep);
      }
      await applyDsp(untrack(() => dsp));
      dspPushed = true;
    })();
    refreshStreaming();
    if (!untrack(() => fades)) api.setFades(false).catch(() => {});
    if (untrack(() => crossfade) > 0) api.setCrossfade(untrack(() => crossfade)).catch(() => {});
    loadRemote();
    loadMarks();
    const stopUpdateChecks = startUpdateChecks();
    const timer = setInterval(async () => {
      // The backend moved on to the next track (or stopped at one it couldn't play).
      await catchUp();
      // iPhone: the output route changed (DAC unplugged) — pick up the new one; the device
      // effect above then restarts the track there, paused.
      const outputError = status.error && JSON.stringify(status.error);
      if (isIOS && outputError && outputError !== lastOutputError) refreshDevices();
      lastOutputError = outputError;
      if (status.finished && testing && !advancing) {
        advancing = true;
        testing = false;
        await api.stop();
        now = null;
        if (index >= 0) await playIndex(index);
        advancing = false;
      }
    }, 250);

    let unlisten: (() => void) | undefined;
    let unlistenRemote: (() => void) | undefined;
    let unlistenMarks: (() => void) | undefined;
    let stopGamepad: (() => void) | undefined;
    // Linux: a game controller drives the window.
    if (inTauri && isLinux) {
      startGamepad({
        toggle: () => togglePause(),
        next: () => next(),
        prev: () => {
          if (now) prev();
        },
        seekBy: (d) => {
          if (status.active) api.seek(Math.min(Math.max(status.position + d, 0), status.duration));
        },
      }).then((stop) => (stopGamepad = stop));
    }
    if (inTauri) {
      // The phone remote: its commands, carried out like the window's own buttons.
      import("@tauri-apps/api/event").then(async ({ listen }) => {
        // A mark made without the window (the phone remote, the API, a scan): show it.
        unlistenMarks = await listen("marks", () => loadMarks());
        unlistenRemote = await listen<RemoteCommand>("remote", ({ payload: c }) => {
          if (c.kind === "toggle") togglePause();
          // (The API's own: play and pause that say which, and stop.)
          else if (c.kind === "resume") {
            if (!status.active) togglePause();
            else if (status.paused) api.setPaused(false);
          } else if (c.kind === "pause") {
            if (status.active && !status.paused) api.setPaused(true);
          } else if (c.kind === "stop") {
            api.stop();
            now = null;
          } else if (c.kind === "next") next();
          else if (c.kind === "prev") prev();
          else if (c.kind === "shuffle") setShuffle(c.on ?? !shuffle);
          else if (c.kind === "repeat") repeat = c.mode ?? nextRepeat(repeat);
          else if (c.kind === "seek") api.seek(c.secs);
          else if (c.kind === "jump") playIndex(c.index);
          else if (c.kind === "play") playPaths(c.paths, c.index);
          else if (c.kind === "enqueue") enqueue(c.paths);
        });
      });
      import("@tauri-apps/api/webview").then(async ({ getCurrentWebview }) => {
        unlisten = await getCurrentWebview().onDragDropEvent((ev) => {
          const p = ev.payload;
          if (p.type === "enter" || p.type === "over") dragOver = true;
          else if (p.type === "leave") dragOver = false;
          else if (p.type === "drop") {
            dragOver = false;
            addPaths(p.paths);
          }
        });
      });
    }
    return () => {
      clearInterval(timer);
      stopUpdateChecks();
      unlisten?.();
      unlistenRemote?.();
      unlistenMarks?.();
      stopGamepad?.();
    };
  });
</script>

<svelte:window onkeydown={onKey} />

{#if look.value === "glass"}
  <!-- The glass look's backdrop: the playing album's cover, blurred down to its colours. -->
  <div class="ambient" class:bp={signalState === "bp"} class:dsp={signalState === "dsp"} style={`--veil: ${coverVeil}`} aria-hidden="true">
    {#if now?.cover && !testing}
      {#key now.cover}<img src={now.cover} alt="" transition:fade={{ duration: 900 }} />{/key}
    {/if}
  </div>
{/if}

<div class="app" class:drag-over={dragOver} class:sig-bp={signalState === "bp"} class:sig-dsp={signalState === "dsp"}>
  <!-- Translations differ by up to ~150px here: when the row doesn't fit, the wordmark goes
       first, then the enhancers' state; the device name ellipsizes last. -->
  <header data-tauri-drag-region class:mac={isMac} class:chrome-right={isMac && i18n.system.dir === "rtl"} use:fitRow={["no-wordmark", "no-enh-state"]}>
    <span class="wordmark" data-tauri-drag-region>BitSpring</span>
    <nav class="tabs" aria-label={t("nav.screens")} use:liquidPill={screen}>
      <!-- data-label: the same words in the chosen tab's bold, to keep each tab's width (see .tabs button::after) -->
      <button class:sel={screen === "player"} aria-current={screen === "player" ? "page" : undefined} data-label={t("nav.player")} onclick={() => (screen = "player")}>{t("nav.player")}</button>
      <button class:sel={screen === "library"} aria-current={screen === "library" ? "page" : undefined} data-label={t("nav.library")} onclick={() => (screen = "library")}>{t("nav.library")}</button>
      {#if streaming?.available}
        <button class:sel={screen === "streaming"} aria-current={screen === "streaming" ? "page" : undefined} data-label={t("nav.streaming")} onclick={() => (screen = "streaming")}>{t("nav.streaming")}</button>
      {/if}
      <button class:sel={screen === "eq"} aria-current={screen === "eq" ? "page" : undefined} data-label={t("nav.eq")} onclick={() => (screen = "eq")}>{t("nav.eq")}</button>
    </nav>
    <div class="controls">
      <Enhancers settings={dsp} {eqName} {ir} track={now && !testing ? { path: now.path, title: now.title } : null} signal={now && !testing ? now.signal : null} load={status.load} stepped={filterStepped} {dsdStepped} device={dspDeviceName} maxNativeDsd={devices.find((d) => d.uid === selected)?.maxNativeDsd ?? null} others={dspOthers} onchange={(s) => applyDsp(s)} oncopy={copyDspFrom} onopeneq={() => (screen = "eq")} />
      <DevicePicker {devices} bind:selected bind:exclusive bind:dopDevices playing={liveRate} onrefresh={() => refreshDevices(true)} onchanneltest={channelTest} oncontrolpanel={controlPanel} />
      <RemoteButton />
      <button class="trigger gear" class:open={screen === "settings"} onclick={toggleSettings} aria-pressed={screen === "settings"} aria-label={t("settings.label")} title={t("settings.label")}>
        <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
          <path
            d="M6.9 1.5h2.2l.4 1.8 1.2.5 1.6-1 1.5 1.5-1 1.6.5 1.2 1.8.4v2.2l-1.8.4-.5 1.2 1 1.6-1.5 1.5-1.6-1-1.2.5-.4 1.8H6.9l-.4-1.8-1.2-.5-1.6 1-1.5-1.5 1-1.6-.5-1.2-1.8-.4V6.9l1.8-.4.5-1.2-1-1.6 1.5-1.5 1.6 1 1.2-.5z"
            fill="none"
            stroke="currentColor"
            stroke-width="1.3"
            stroke-linejoin="round"
          />
          <circle cx="8" cy="8" r="2.2" fill="none" stroke="currentColor" stroke-width="1.3" />
        </svg>
      </button>
    </div>
  </header>

  <aside class="queue" class:show={screen === "queue"} aria-label={t("nav.queue")}>
    <div class="q-head">
      <h2>{t("nav.queue")}</h2>
      {#if queue.length}
        <span class="q-tools">
          <AddToPlaylist compact label={t("pl.saveQueue")} entries={queueEntries} />
          <button class="q-clear" onclick={clearQueue}>{t("queue.clear")}</button>
        </span>
      {/if}
    </div>

    {#if queue.length}
      <ol class="q-list">
        {#each queue as path, i (path + i)}
          <li class:current={i === index}>
            <button class="q-item" onclick={() => playIndex(i)} title={labels[path]?.artist ? `${labels[path].artist} — ${labelOf(path)}` : path}>
              <span class="q-n num">{i + 1}</span>
              <span class="q-name">{labelOf(path)}</span>
            </button>
            <button class="q-remove" onclick={() => removeAt(i)} aria-label={t("queue.remove", { name: labelOf(path) })}>
              <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true"><path d="M1 1l8 8M9 1l-8 8" stroke="currentColor" stroke-width="1.5" /></svg>
            </button>
          </li>
        {/each}
      </ol>
    {:else}
      <p class="q-empty">{t(isAndroid ? "queue.emptyAndroid" : isIOS ? "queue.emptyIos" : "queue.empty")}</p>
    {/if}

    <!-- Android's file picker hands out content:// links, not files: music comes via the Music folder. -->
    {#if !isAndroid}<button class="q-add" onclick={pickFiles}>{t("common.addFiles")}</button>{/if}
  </aside>

  {#if screen === "player" || screen === "queue"}
  <main class="panel" class:phone-hidden={screen === "queue"} class:with-lyrics={showLyrics}>
    <div class="np">
      {#if now && !testing}
        <button class="cover" onclick={() => goToLibrary("album")} title={t("np.openAlbum")}>
          {#if now.cover}
            <img src={now.cover} alt={t("common.cover", { album: now.album ?? now.title })} />
          {:else}
            <span class="cover-initial" aria-hidden="true">{(now.album ?? now.title).slice(0, 1)}</span>
          {/if}
        </button>
      {:else}
        <div class="cover" class:test={testing}>
          {#if now}
            <span class="cover-initial" aria-hidden="true">{(now.album ?? now.title).slice(0, 1)}</span>
          {/if}
        </div>
      {/if}
      <div class="np-info">
        {#if now}
          <h1>{now.title}</h1>
          {#if now.artist}
            <p class="artist">
              {#if testing}{now.artist}{:else}<button class="link" onclick={() => goToLibrary("artist")} title={t("np.openArtist")}>{artistLabel(now.artist)}</button>{/if}
            </p>
          {/if}
          {#if now.album}
            <p class="album">
              {#if testing}{now.album}{:else}<button class="link" onclick={() => goToLibrary("album")} title={t("np.openAlbum")}><bdi>{now.album}</bdi></button>{/if}{now.year ? `${t("list.sep")}${now.year}` : ""}
            </p>
          {/if}
          <!-- the stars of what plays, where they are switched on: the one place a phone has for them -->
          {#if marks.ratings && canMark}
            <p class="np-stars"><Stars large value={marks.rating(now.path)} label={t("rate.of", { title: now.title })} onchange={(n) => setRating(now!.path, n)} /></p>
          {/if}
        {:else}
          <h1 class="idle">{t("np.idle")}</h1>
          <p class="album">{sentences([t(isAndroid ? "np.hintAndroid" : isIOS ? "np.hintIos" : "np.hint"), t("np.promise")])}</p>
          {#if !isAndroid}<button class="primary" onclick={pickFiles}>{t("common.addFiles")}</button>{/if}
        {/if}
      </div>
    </div>

    {#if showLyrics && now}
      <LyricsView {now} {status} onseek={(s) => api.seek(s)} />
      <!-- phones: the readout makes way for the words; where the track is stays in view -->
      <div class="lyr-time num" style={`--p: ${status.duration ? (status.position / status.duration) * 100 : 0}%`} aria-hidden="true">
        <span>{clock(status.position)}</span><i></i><span>{clock(status.duration)}</span>
      </div>
    {/if}

    <Display {now} {status} {testing} error={error ?? (status.error && errorText(status.error))} {deviceName} {sweepKey} onseek={(s) => api.seek(s)} />

    <!-- transport keys keep their tape direction in right-to-left languages -->
    <div class="keys" dir="ltr">
      <button class="key mode fav-key" class:on={isFavorite} onclick={toggleFavorite} aria-pressed={isFavorite} aria-label={t(isFavorite ? "fav.remove" : "fav.add")} title={hint(t(isFavorite ? "fav.remove" : "fav.add"), "F") ?? t(isFavorite ? "fav.remove" : "fav.add")} disabled={!canMark}>
        {#key now?.path}<Heart on={isFavorite} size={18} beats />{/key}
      </button>
      <button class="key mode shuffle-key" class:on={shuffle} onclick={() => setShuffle(!shuffle)} aria-pressed={shuffle} aria-label={t("common.shuffle")} title={hint(t("common.shuffle"), "S") ?? t("common.shuffle")} disabled={testing}>
        <!-- two lines that trade places -->
        <svg width="18" height="18" viewBox="0 0 22 22" aria-hidden="true"><path d="M3 6.5h3.2c1.6 0 2.7.7 3.6 2.1l2.4 3.8c.9 1.4 2 2.1 3.6 2.1H19M3 14.5h3.2c1.2 0 2.1-.4 2.9-1.4M12.9 7.9c.8-1 1.7-1.4 2.9-1.4H19M16.5 4L19 6.5 16.5 9M16.5 12l2.5 2.5-2.5 2.5" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" /></svg>
      </button>
      <button class="key" onclick={prev} aria-label={t("common.prev")} title={hint(t("common.prev"), `${modKey}←`)} disabled={!now || testing}>
        <svg width="18" height="18" viewBox="0 0 22 22" aria-hidden="true"><path d="M6 4v14M18 4L8 11l10 7z" fill="currentColor" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" /></svg>
      </button>
      <button class="play" onclick={togglePause} aria-label={t(status.active && !status.paused ? "common.pause" : "common.play")} title={hint(t(status.active && !status.paused ? "common.pause" : "common.play"), t("keys.space"))} disabled={loading || (!now && !queue.length)}>
        {#if status.active && !status.paused}
          <svg width="24" height="24" viewBox="0 0 26 26" aria-hidden="true"><rect x="6" y="5" width="5" height="16" rx="1" fill="currentColor" /><rect x="15" y="5" width="5" height="16" rx="1" fill="currentColor" /></svg>
        {:else}
          <svg width="24" height="24" viewBox="0 0 26 26" aria-hidden="true"><path d="M8 4.5v17l14-8.5z" fill="currentColor" /></svg>
        {/if}
      </button>
      <button class="key" onclick={next} aria-label={t("common.next")} title={hint(t("common.next"), `${modKey}→`)} disabled={testing || !canNext}>
        <svg width="18" height="18" viewBox="0 0 22 22" aria-hidden="true"><path d="M16 4v14M4 4l10 7-10 7z" fill="currentColor" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" /></svg>
      </button>
      <button class="key mode repeat-key" class:on={repeat !== "off"} onclick={() => (repeat = nextRepeat(repeat))} aria-label={t(`repeat.${repeat}`)} title={hint(t(`repeat.${repeat}`), "R") ?? t(`repeat.${repeat}`)} disabled={testing}>
        <!-- a loop; with a 1 inside when it is the one track that repeats -->
        <svg width="18" height="18" viewBox="0 0 22 22" aria-hidden="true"><path d="M4 10V8.5a3 3 0 0 1 3-3h10M14.5 3L17 5.5 14.5 8M18 12v1.5a3 3 0 0 1-3 3H5M7.5 14L5 16.5 7.5 19" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" />{#if repeat === "one"}<path d="M9.9 9.4l1.7-1.2v5.7" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" />{/if}</svg>
      </button>
      <button
        class="key mode lyr-key"
        class:on={showLyrics}
        onclick={() => (lyricsOn = !lyricsOn)}
        aria-pressed={showLyrics}
        aria-label={t(showLyrics ? "lyrics.hide" : "lyrics.show")}
        title={`${t(showLyrics ? "lyrics.hide" : "lyrics.show")} (L)`}
        disabled={!now || testing}
      >
        <!-- a speech bubble with a quotation mark: the words of the song -->
        <svg width="18" height="18" viewBox="0 0 22 22" aria-hidden="true"><path d="M4 4.5h14a1.5 1.5 0 0 1 1.5 1.5v9a1.5 1.5 0 0 1-1.5 1.5h-7.5L6 20v-3.5H4A1.5 1.5 0 0 1 2.5 15V6A1.5 1.5 0 0 1 4 4.5z" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" /><path d="M8 9.2h2.2v2.1c0 1-.5 1.7-1.6 2M12.4 9.2h2.2v2.1c0 1-.5 1.7-1.6 2" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" /></svg>
      </button>
    </div>
  </main>
  {:else if screen === "eq"}
    <main class="lib">
      {#key eqAdopted}
        <Equalizer {devices} device={selected} deviceName={dspDeviceName} convolution={ir} pick={eqPick} onpickopened={() => (eqPick = null)} onsignal={eqSignal} onstate={(s) => (eqState = s)} onpickir={pickIr} onclearir={() => applyDsp({ ...dsp, convolution: null })} />
      {/key}
    </main>
  {:else if screen === "settings"}
    <main class="lib">
      <Settings {fades} onfades={setFades} {crossfade} oncrossfade={setCrossfade} onclose={toggleSettings} />
    </main>
  {:else if screen === "streaming" && streaming?.available}
    <main class="lib">
      <Streaming status={streaming} currentPath={testing ? null : (now?.path ?? null)} onstatus={refreshStreaming} onplay={playPaths} onshuffle={playShuffled} onenqueue={enqueue} />
    </main>
  {:else}
    <main class="lib">
      <Library currentPath={testing ? null : (now?.path ?? null)} goto={libGoto} {streaming} onplay={playPaths} onshuffle={playShuffled} onenqueue={enqueue} />
    </main>
  {/if}

  {#if now && screen !== "player"}
    <div class="mini" style={`--p: ${status.duration ? (status.position / status.duration) * 100 : 0}%`}>
      <div class="mini-np">
        <button class="mini-art" onclick={() => goToLibrary("album")} title={t("mini.openAlbum")} aria-label={now.album ? t("mini.openAlbumNamed", { album: now.album }) : t("mini.openAlbum")}>
          {#if now.cover}<img src={now.cover} alt="" />{/if}
        </button>
        <span class="mini-text">
          <button class="mini-title" onclick={() => (screen = "player")} title={t("mini.openPlayer")}>{now.title}</button>
          {#if now.artist}<button class="mini-artist" onclick={() => goToLibrary("artist")} title={t("mini.openArtist")}>{artistLabel(now.artist)}</button>{/if}
        </span>
      </div>
      <span class="mini-sig" class:ok={now.signal.bitPerfect}>
        <i aria-hidden="true"></i>{t(now.signal.bitPerfect ? "signal.bitPerfect" : "signal.processing")}{t("list.sep")}{#if now.signal.dop && now.signal.dsdRate}{dsdName(now.signal.dsdRate)} {now.signal.nativeDsd ? t("unit.native") : "DoP"}{:else if now.signal.dsdOut}DSD{now.signal.dsdOut} {now.signal.nativeDsd ? t("unit.native") : "DoP"}{:else}{khz(now.signal.deviceRate)} {t("unit.khz")}{/if}
      </span>
      <div class="mini-keys" dir="ltr">
        <button onclick={prev} aria-label={t("common.prev")} title={hint(t("common.prev"), `${modKey}←`)} disabled={testing}>
          <svg width="14" height="14" viewBox="0 0 22 22" aria-hidden="true"><path d="M6 4v14M18 4L8 11l10 7z" fill="currentColor" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" /></svg>
        </button>
        <button class="mini-play" onclick={togglePause} aria-label={t(status.active && !status.paused ? "common.pause" : "common.play")} title={hint(t(status.active && !status.paused ? "common.pause" : "common.play"), t("keys.space"))}>
          {#if status.active && !status.paused}
            <svg width="16" height="16" viewBox="0 0 26 26" aria-hidden="true"><rect x="6" y="5" width="5" height="16" rx="1" fill="currentColor" /><rect x="15" y="5" width="5" height="16" rx="1" fill="currentColor" /></svg>
          {:else}
            <svg width="16" height="16" viewBox="0 0 26 26" aria-hidden="true"><path d="M8 4.5v17l14-8.5z" fill="currentColor" /></svg>
          {/if}
        </button>
        <button onclick={next} aria-label={t("common.next")} title={hint(t("common.next"), `${modKey}→`)} disabled={testing || !canNext}>
          <svg width="14" height="14" viewBox="0 0 22 22" aria-hidden="true"><path d="M16 4v14M4 4l10 7-10 7z" fill="currentColor" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" /></svg>
        </button>
      </div>
    </div>
  {/if}

  <nav class="tabbar" aria-label={t("nav.screens")} use:liquidPill={{ key: screen, part: "svg" }}>
    <button class:sel={screen === "player"} aria-current={screen === "player" ? "page" : undefined} onclick={() => (screen = "player")}>
      <svg width="22" height="22" viewBox="0 0 22 22" aria-hidden="true"><circle cx="11" cy="11" r="8.5" fill="none" stroke="currentColor" stroke-width="1.6" /><circle cx="11" cy="11" r="2.2" fill="currentColor" /></svg>
      <span class="tl">{t("tab.player")}</span>
    </button>
    <button class:sel={screen === "library"} aria-current={screen === "library" ? "page" : undefined} onclick={() => (screen = "library")}>
      <svg width="22" height="22" viewBox="0 0 22 22" aria-hidden="true"><rect x="3" y="3" width="7" height="7" rx="1.5" fill="none" stroke="currentColor" stroke-width="1.6" /><rect x="12" y="3" width="7" height="7" rx="1.5" fill="none" stroke="currentColor" stroke-width="1.6" /><rect x="3" y="12" width="7" height="7" rx="1.5" fill="none" stroke="currentColor" stroke-width="1.6" /><rect x="12" y="12" width="7" height="7" rx="1.5" fill="none" stroke="currentColor" stroke-width="1.6" /></svg>
      <span class="tl">{t("nav.library")}</span>
    </button>
    {#if streaming?.available}
      <button class:sel={screen === "streaming"} aria-current={screen === "streaming" ? "page" : undefined} onclick={() => (screen = "streaming")}>
        <svg width="22" height="22" viewBox="0 0 22 22" aria-hidden="true"><path d="M3 13a8 8 0 0 1 16 0M6.5 13a4.5 4.5 0 0 1 9 0" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" /><circle cx="11" cy="14.5" r="1.8" fill="currentColor" /></svg>
        <span class="tl">{t("nav.streaming")}</span>
      </button>
    {/if}
    <button class:sel={screen === "eq"} aria-current={screen === "eq" ? "page" : undefined} onclick={() => (screen = "eq")}>
      <svg width="22" height="22" viewBox="0 0 22 22" aria-hidden="true"><path d="M5 3v16M11 3v16M17 3v16" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" /><rect x="2.5" y="12" width="5" height="3" rx="1.5" fill="var(--ink)" stroke="currentColor" stroke-width="1.6" /><rect x="8.5" y="6" width="5" height="3" rx="1.5" fill="var(--ink)" stroke="currentColor" stroke-width="1.6" /><rect x="14.5" y="10" width="5" height="3" rx="1.5" fill="var(--ink)" stroke="currentColor" stroke-width="1.6" /></svg>
      <span class="tl">{t("nav.eq")}</span>
    </button>
    <button class:sel={screen === "queue"} aria-current={screen === "queue" ? "page" : undefined} onclick={() => (screen = "queue")}>
      <svg width="22" height="22" viewBox="0 0 22 22" aria-hidden="true"><path d="M3 5h16M3 11h16M3 17h10" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" /></svg>
      <span class="tl">{t("nav.queue")}</span>{#if queue.length}<span class="badge num">{queue.length}</span>{/if}
    </button>
  </nav>

  <UpdateNotice playing={status.active && !status.paused} />
  {#if abx.request}
    <AbxDialog request={abx.request} device={selected} {exclusive} onclose={endAbx} />
  {/if}
  <CrashNotice bind:showing={crashShowing} />
  <!-- One offer at a time, after the notices that matter more: so none covers another. -->
  {#if eqHint && !crashShowing && !updateNoticeShown()}
    {@const model = eqHint}
    <EqHint {model} onpick={() => pickFromHint(model)} ondismiss={() => dismissEqHint(selectedKey)} />
  {/if}

  {#if dragOver}
    <div class="drop-hint" aria-hidden="true"><p>{t("queue.drop")}</p></div>
  {/if}
</div>

<style>
  .app {
    display: grid;
    grid-template-columns: 260px minmax(0, 1fr);
    grid-template-rows: 52px minmax(0, 1fr) auto;
    height: 100vh;
  }
  .queue {
    grid-row: 2 / 4;
  }
  .mini {
    grid-column: 2;
  }
  .tabbar {
    display: none;
  }

  header {
    grid-column: 1 / -1;
    display: flex;
    align-items: center;
    padding: 0 16px;
    border-bottom: 1px solid var(--slate-600);
    background: var(--slate-900);
  }
  /* room for the macOS traffic lights of the overlay title bar (Windows has its own bar) */
  header.mac {
    padding-left: 88px;
  }
  /* macOS in a right-to-left language puts the traffic lights on the right */
  header.mac.chrome-right {
    padding-left: 16px;
    padding-right: 88px;
  }
  .wordmark {
    flex: none;
    font-family: var(--font-display);
    font-weight: 600;
    font-size: var(--t-lg);
    letter-spacing: 0.04em;
  }
  .tabs {
    flex: none;
    display: flex;
    gap: 4px;
    margin-inline-start: 28px;
    margin-inline-end: auto;
  }
  .tabs button {
    display: inline-flex;
    flex-direction: column;
    align-items: center;
    white-space: nowrap;
    padding: 6px 12px;
    border-radius: var(--r-md);
    font-size: var(--t-sm);
    color: var(--mist);
  }
  /* Every tab is as wide as its label set bold, chosen or not: the row keeps its width
     whichever tab is chosen, and when none is (the settings screen). */
  .tabs button::after {
    content: attr(data-label);
    height: 0;
    overflow: hidden;
    visibility: hidden;
    font-weight: 600;
    pointer-events: none;
  }
  .tabs button:hover {
    color: var(--porcelain);
  }
  .tabs button.sel {
    color: var(--porcelain);
    font-weight: 600;
    background: var(--slate-700);
  }

  /* --- library screen --- */
  .lib {
    min-height: 0;
    display: flex;
    flex-direction: column;
    background: var(--slate-800);
  }
  .lib > :global(.library) {
    flex: 1;
    min-height: 0;
  }
  .mini {
    position: relative;
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto auto;
    align-items: center;
    gap: 20px;
    padding: 10px 20px;
    background: var(--slate-900);
    border-top: 1px solid var(--slate-600);
  }
  /* progress as a hairline along the top edge */
  .mini::before {
    content: "";
    position: absolute;
    top: -1px;
    left: 0;
    height: 2px;
    width: var(--p);
    background: var(--porcelain);
  }
  .mini-np {
    display: flex;
    align-items: center;
    gap: 12px;
    min-width: 0;
    text-align: start;
  }
  .mini-art {
    width: 40px;
    height: 40px;
    border-radius: var(--r-sm);
    overflow: hidden;
    flex: none;
    background:
      repeating-linear-gradient(90deg, rgba(232, 237, 234, 0.05) 0 2px, transparent 2px 6px),
      var(--slate-700);
  }
  .mini-art img {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }
  .mini-text {
    display: flex;
    flex-direction: column;
    min-width: 0;
    font-size: var(--t-sm);
  }
  .mini-text {
    align-items: flex-start;
  }
  .mini-text button {
    max-width: 100%;
    text-align: start;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .mini-title {
    font-weight: 600;
  }
  .mini-artist {
    color: var(--mist);
  }
  .mini-text button:hover {
    color: var(--porcelain);
    text-decoration: underline;
    text-underline-offset: 3px;
  }
  .mini-art:hover {
    outline: 1px solid var(--mist);
    outline-offset: 1px;
  }
  .mini-sig {
    display: flex;
    align-items: center;
    gap: 7px;
    font-size: var(--t-sm);
    color: var(--violet);
    white-space: nowrap;
  }
  .mini-sig i {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: currentColor;
  }
  .mini-sig.ok {
    color: var(--brass);
  }
  .mini-keys {
    display: flex;
    align-items: center;
    gap: 14px;
  }
  .mini-keys button {
    display: grid;
    place-items: center;
    color: var(--porcelain);
  }
  .mini-keys button:disabled {
    color: var(--slate-500);
  }
  .mini-play {
    width: 36px;
    height: 36px;
    border-radius: 50%;
    border: 1.5px solid var(--live);
    color: var(--live) !important;
  }

  .controls {
    display: flex;
    gap: 10px;
    align-items: center;
    min-width: 0;
  }
  /* Settings: a screen of its own, opened and closed by the gear. */
  .gear {
    flex: none;
    display: grid;
    place-items: center;
    width: 32px;
    height: 32px;
    border-radius: var(--r-md);
    border: 1px solid var(--slate-600);
    color: var(--mist);
  }
  .gear:hover,
  .gear.open {
    color: var(--porcelain);
    border-color: var(--slate-500);
  }
  .gear.open {
    background: var(--slate-600);
  }
  /* What the row still lacks after fitRow comes out of the device name (it ellipsizes). */
  header :global(.picker),
  header :global(.picker .trigger) {
    min-width: 0;
  }
  /* (classes set by fitRow, hence :global) */
  header:global(.no-wordmark) .wordmark {
    display: none;
  }
  header:global(.no-wordmark) .tabs {
    margin-inline-start: 0;
  }
  header:global(.no-enh-state) :global(.enh .state) {
    display: none;
  }
  /* fitRow measures the row at its natural width: nothing shrinks while it does. */
  header:global(.fit-measure) :global(*) {
    flex-shrink: 0 !important;
  }

  /* --- queue --- */
  .queue {
    display: flex;
    flex-direction: column;
    min-height: 0;
    border-inline-end: 1px solid var(--slate-600);
    background: var(--slate-900);
  }
  .q-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    padding: 18px 20px 8px;
  }
  h2 {
    margin: 0;
    font-size: var(--t-md);
    font-weight: 600;
  }
  .q-tools {
    display: inline-flex;
    align-items: center;
    gap: 8px;
  }
  .q-clear {
    color: var(--mist);
    font-size: var(--t-sm);
  }
  .q-clear:hover {
    color: var(--porcelain);
  }
  .q-list {
    list-style: none;
    margin: 0;
    padding: 0 8px;
    overflow-y: auto;
    flex: 1;
  }
  .q-list li {
    display: flex;
    align-items: center;
    border-radius: var(--r-md);
  }
  .q-list li:hover {
    background: var(--slate-800);
  }
  .q-list li.current {
    background: var(--slate-700);
  }
  .q-item {
    flex: 1;
    min-width: 0;
    display: flex;
    gap: 10px;
    align-items: baseline;
    text-align: start;
    padding-block: 8px;
    padding-inline: 10px 4px;
  }
  .q-n {
    color: var(--mist-dim);
    font-size: var(--t-sm);
    width: 1.4em;
    text-align: end;
    flex: none;
  }
  .current .q-n {
    color: var(--live);
  }
  .q-name {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    font-size: var(--t-sm);
  }
  .current .q-name {
    font-weight: 600;
  }
  .q-remove {
    padding: 8px 10px;
    color: var(--mist-dim);
    opacity: 0;
  }
  .q-list li:hover .q-remove,
  .q-remove:focus-visible {
    opacity: 1;
  }
  .q-remove:hover {
    color: var(--porcelain);
  }
  .q-empty {
    flex: 1;
    margin: 0;
    padding: 4px 20px;
    color: var(--mist);
    font-size: var(--t-sm);
    max-width: 26ch;
  }
  .q-add {
    margin: 12px;
    padding: 9px;
    border-radius: var(--r-md);
    border: 1px dashed var(--slate-500);
    color: var(--mist);
    font-size: var(--t-sm);
  }
  .q-add:hover {
    color: var(--porcelain);
    border-color: var(--mist);
  }

  /* --- faceplate --- */
  .panel {
    min-height: 0;
    overflow: auto;
    display: flex;
    flex-direction: column;
    gap: 22px;
    padding: 28px 40px 32px;
    /* brushed anodised finish, barely there */
    background:
      repeating-linear-gradient(90deg, rgba(232, 237, 234, 0.012) 0 1px, transparent 1px 3px),
      linear-gradient(180deg, #1a2930, var(--slate-800));
  }
  .panel > :global(*) {
    width: 100%;
    max-width: 900px;
    margin-inline: auto;
  }
  /* Centre vertically, but never push content above the scroll origin when it doesn't fit
     (justify-content: center would clip the cover off the top). */
  .panel > :global(:first-child) {
    margin-top: auto;
  }
  .panel > :global(:last-child) {
    margin-bottom: auto;
  }

  .np {
    display: grid;
    grid-template-columns: 148px minmax(0, 1fr);
    gap: 24px;
    align-items: end;
  }
  .cover {
    aspect-ratio: 1;
    border-radius: var(--r-md);
    overflow: hidden;
    background:
      repeating-linear-gradient(90deg, rgba(232, 237, 234, 0.05) 0 2px, transparent 2px 8px),
      var(--slate-700);
    display: grid;
    place-items: center;
    box-shadow: 0 14px 30px rgba(5, 12, 15, 0.5);
  }
  button.cover {
    padding: 0;
    width: 100%;
    cursor: pointer;
  }
  button.cover:hover {
    outline: 1px solid var(--mist);
    outline-offset: 2px;
  }
  .cover img {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }
  .cover-initial {
    font-family: var(--font-display);
    font-weight: 600;
    font-size: 4rem;
    color: var(--slate-500);
  }
  .cover.test .cover-initial {
    color: var(--porcelain);
  }
  .np-info {
    min-width: 0;
    padding-bottom: 4px;
  }
  h1 {
    margin: 0 0 4px;
    font-family: var(--font-display);
    font-weight: 600;
    font-size: var(--t-2xl);
    line-height: 1.05;
    letter-spacing: -0.01em;
    overflow-wrap: anywhere;
  }
  h1.idle {
    color: var(--mist);
  }
  .artist {
    margin: 0;
    font-size: var(--t-lg);
    font-weight: 500;
  }
  .link {
    text-align: start;
    font: inherit;
    color: inherit;
  }
  .link:hover {
    color: var(--porcelain);
    text-decoration: underline;
    text-underline-offset: 3px;
  }
  .album {
    margin: 2px 0 0;
    color: var(--mist);
    max-width: 60ch;
  }
  /* (the first star stands under the first letter of the lines above it) */
  .np-stars {
    margin: 6px 0 0;
    margin-inline-start: -6px;
    line-height: 0;
  }
  .primary {
    margin-top: 14px;
    padding: 9px 16px;
    border-radius: var(--r-md);
    background: var(--porcelain);
    color: var(--ink);
    font-weight: 600;
  }
  .primary:hover {
    filter: brightness(1.08);
  }

  /* hardware keys */
  /* prev / play / next in the middle, shuffle and repeat on either side of them; the heart
     and the lyrics key stand beside those, one on each side, and don't move the middle */
  .keys {
    display: grid;
    grid-template-columns: minmax(0, 1fr) repeat(5, auto) minmax(0, 1fr);
    align-items: center;
    gap: 18px;
  }
  .keys > .fav-key {
    justify-self: end;
  }
  .keys > .lyr-key {
    justify-self: start;
  }
  .key {
    width: 52px;
    height: 38px;
    border-radius: var(--r-md);
    display: grid;
    place-items: center;
    color: var(--porcelain);
    background: linear-gradient(180deg, #25383f, var(--slate-700));
    box-shadow:
      inset 0 1px 0 rgba(232, 237, 234, 0.08),
      0 2px 0 #0c1418,
      0 3px 8px rgba(0, 0, 0, 0.35);
  }
  .key:active:not(:disabled) {
    transform: translateY(1px);
    box-shadow:
      inset 0 1px 0 rgba(232, 237, 234, 0.05),
      0 1px 0 #0c1418;
  }
  .key:disabled {
    color: var(--slate-500);
    cursor: default;
  }
  .play {
    width: 64px;
    height: 64px;
    border-radius: 50%;
    display: grid;
    place-items: center;
    color: var(--live);
    border: 2px solid var(--live);
    background: radial-gradient(circle at 50% 35%, #22343b, var(--slate-800));
    box-shadow: 0 3px 10px rgba(0, 0, 0, 0.4);
  }
  .play:hover:not(:disabled) {
    background: radial-gradient(circle at 50% 35%, #2a3f47, var(--slate-800));
  }
  .play:disabled {
    color: var(--slate-500);
    border-color: var(--slate-500);
    cursor: default;
  }

  .drop-hint {
    position: fixed;
    inset: 8px;
    border: 2px dashed var(--porcelain);
    border-radius: var(--r-lg);
    background: rgba(18, 29, 35, 0.82);
    display: grid;
    place-items: center;
    pointer-events: none;
    z-index: 50;
  }
  .drop-hint p {
    font-family: var(--font-display);
    font-size: var(--t-xl);
    font-weight: 600;
    color: var(--porcelain);
  }

  /* touch screens have no hover to reveal controls */
  @media (hover: none) {
    .q-remove {
      opacity: 1;
    }
  }

  /* --- phone: one screen at a time, tab bar at the bottom --- */
  @media (max-width: 700px), (max-height: 500px) {
    .app {
      grid-template-columns: minmax(0, 1fr);
      grid-template-rows: auto minmax(0, 1fr) auto auto;
      height: 100dvh;
    }
    header {
      gap: 10px;
      padding: var(--safe-top, env(safe-area-inset-top)) 12px 0;
      min-height: calc(54px + var(--safe-top, env(safe-area-inset-top)));
    }
    header .tabs {
      display: none;
    }
    .controls {
      margin-inline-start: auto;
      min-width: 0;
      gap: 8px;
    }
    /* a bare icon: the header is tight */
    .gear {
      width: 26px;
      border-color: transparent;
    }
    header :global(.enh .state) {
      display: none;
    }
    header :global(.picker),
    header :global(.picker .trigger) {
      min-width: 0;
    }
  }
  /* The narrowest phones (iPhone Duo folded, mini): the device's name matters more than its rate,
     which the now-playing card shows in large anyway. */
  @media (max-width: 420px) {
    header :global(.picker .rate) {
      display: none;
    }
  }
  @media (max-width: 700px), (max-height: 500px) {
    main,
    .queue {
      grid-row: 2;
      grid-column: 1;
    }
    .queue {
      display: none;
      border-inline-end: 0;
    }
    .queue.show {
      display: flex;
    }
    .phone-hidden {
      display: none !important;
    }
    .q-item {
      padding-block: 12px;
    }
    .mini {
      grid-column: 1;
      grid-row: 3;
      grid-template-columns: minmax(0, 1fr) auto;
      gap: 12px;
      padding: 8px 12px;
    }
    .mini-sig {
      display: none;
    }
    .tabbar {
      grid-row: 4;
      display: flex;
      background: var(--slate-900);
      border-top: 1px solid var(--slate-600);
      padding-bottom: var(--safe-bottom, env(safe-area-inset-bottom));
    }
    .tabbar button {
      position: relative;
      /* widths follow the labels ("Now Playing" needs more than "Queue"); a label that still
         doesn't fit (long translations on the narrowest phones) ends in an ellipsis */
      flex: 1 1 auto;
      min-width: 0;
      padding-inline: 3px;
      display: flex;
      flex-direction: column;
      align-items: center;
      gap: 3px;
      padding: 8px 0 6px;
      font-size: var(--t-xs);
      color: var(--mist);
    }
    .tabbar button.sel {
      color: var(--porcelain);
    }
    .tl {
      max-width: 100%;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .badge {
      position: absolute;
      top: 3px;
      inset-inline-start: calc(50% + 8px);
      min-width: 18px;
      padding: 0 5px;
      border-radius: 999px;
      background: var(--slate-600);
      color: var(--porcelain);
      font-size: 11px;
      line-height: 18px;
    }
    .panel {
      gap: 18px;
      padding: 20px 20px 24px;
    }
    .np {
      grid-template-columns: minmax(0, 1fr);
      justify-items: center;
      text-align: center;
      gap: 18px;
    }
    .np .cover {
      width: min(100%, 280px, 34dvh);
    }
    /* transport right under the title, within thumb reach; the signal readout below */
    .keys {
      order: 1;
    }
    .panel > :global(:not(.np):not(.keys)) {
      order: 2;
    }
    .np-info {
      width: 100%;
    }
    .link {
      text-align: center;
    }
    .np-stars {
      margin-inline-start: 0;
    }
    h1 {
      font-size: var(--t-xl);
    }
    .album {
      margin-inline: auto;
    }
  }

  /* --- the wide layout on a big phone screen (the unfolded iPhone Duo): keep clear of the
     status bar, camera and home indicator too. On a computer every inset is 0, so nothing moves. --- */
  @media (min-width: 701px) and (min-height: 501px) {
    .app {
      grid-template-rows: calc(52px + var(--safe-top, env(safe-area-inset-top))) minmax(0, 1fr) auto;
      box-sizing: border-box;
      padding: 0 var(--safe-right, env(safe-area-inset-right)) var(--safe-bottom, env(safe-area-inset-bottom))
        var(--safe-left, env(safe-area-inset-left));
    }
    header {
      padding-top: var(--safe-top, env(safe-area-inset-top));
    }
  }
  /* --- any phone screen: keep clear of what the system puts at the sides (the Dynamic Island
     when held sideways; the iPhone Duo's status bar and camera, which sit on the right) --- */
  @media (max-width: 700px), (max-height: 500px) {
    header,
    .mini,
    .tabbar {
      padding-inline: max(12px, var(--safe-left, env(safe-area-inset-left))) max(12px, var(--safe-right, env(safe-area-inset-right)));
    }
    .panel,
    .queue {
      padding-inline: max(20px, var(--safe-left, env(safe-area-inset-left))) max(20px, var(--safe-right, env(safe-area-inset-right)));
    }
  }
  /* --- phone held sideways: the cover beside the title, so the transport fits without
     scrolling --- */
  @media (max-height: 500px) and (min-width: 600px) {
    .np {
      grid-template-columns: auto minmax(0, 1fr);
      justify-items: start;
      align-items: center;
      text-align: start;
      gap: 22px;
    }
    .np .cover {
      width: min(150px, 34dvh);
    }
    .album {
      margin-inline: 0;
    }
    .link {
      text-align: start;
    }
  }

  @media (min-width: 701px) and (max-width: 1000px) and (min-height: 501px) {
    .app {
      grid-template-columns: 220px minmax(0, 1fr);
    }
    .panel {
      padding: 22px 24px;
    }
  }
  @media (min-width: 701px) and (min-height: 501px) and (max-height: 700px) {
    .panel {
      gap: 14px;
      padding-block: 18px;
    }
    .np {
      grid-template-columns: 104px minmax(0, 1fr);
      gap: 18px;
    }
    h1 {
      font-size: var(--t-xl);
    }
  }
  /* --- the keys that stay down: shuffle, repeat, lyrics --- */
  .keys .key.mode {
    width: 38px;
    height: 38px;
    border-radius: 50%;
    color: var(--mist);
  }
  .keys .key.mode:hover:not(:disabled) {
    color: var(--porcelain);
  }
  /* (lit under the pointer too: a key just clicked shows what the click did) */
  .keys .key.mode.on,
  .keys .key.mode.on:hover:not(:disabled) {
    color: var(--ink);
    background: var(--porcelain);
  }
  .keys .key.mode:disabled {
    color: var(--slate-500);
  }
  .keys .key.lyr-key {
    margin-inline-start: 6px;
  }
  .keys .key.fav-key {
    margin-inline-end: 6px;
  }
  /* narrow phones: the four of them in a row of their own under the transport */
  @media (max-width: 480px) {
    .keys {
      display: flex;
      flex-wrap: wrap;
      justify-content: center;
      /* (twice over: the break between the rows is a row of no height) */
      row-gap: 5px;
    }
    .keys::before {
      content: "";
      flex-basis: 100%;
      order: 1;
    }
    .keys .key.mode {
      order: 2;
      width: 44px;
      height: 44px;
    }
    .keys .key.lyr-key {
      margin-inline-start: 0;
    }
    .keys .key.fav-key {
      margin-inline-end: 0;
    }
  }
  /* --- lyrics --- */
  /* Most windows: the title in brief, the words, where the track is, the keys. The readout
     makes way (the header still names the output and its rate). */
  .panel.with-lyrics {
    gap: 16px;
  }
  @media (max-width: 1199px), (max-height: 599px) {
    .panel.with-lyrics > :global(.display) {
      display: none;
    }
  }
  .panel.with-lyrics > :global(.lyrics) {
    flex: 1 1 0;
    min-height: 160px;
  }
  .panel.with-lyrics .np {
    grid-template-columns: 64px minmax(0, 1fr);
    gap: 16px;
    align-items: center;
  }
  .panel.with-lyrics .np-info {
    padding-bottom: 0;
  }
  .panel.with-lyrics h1 {
    font-size: var(--t-xl);
  }
  .panel.with-lyrics .artist {
    font-size: var(--t-md);
  }
  .panel.with-lyrics .cover-initial {
    font-size: 2rem;
  }
  .lyr-time {
    display: flex;
    flex: none;
    align-items: center;
    gap: 10px;
    font-size: var(--t-xs);
    color: var(--mist);
  }
  .lyr-time i {
    flex: 1;
    height: 3px;
    border-radius: 2px;
    background: linear-gradient(90deg, var(--porcelain) var(--p), rgba(232, 237, 234, 0.16) var(--p));
  }
  :global([dir="rtl"]) .lyr-time i {
    background: linear-gradient(270deg, var(--porcelain) var(--p), rgba(232, 237, 234, 0.16) var(--p));
  }
  /* A wide window: the player as it is on the left, the words beside it. */
  @media (min-width: 1200px) and (min-height: 600px) {
    .panel.with-lyrics {
      display: grid;
      grid-template-columns: minmax(0, 560px) minmax(0, 1fr);
      grid-template-rows: minmax(0, 1fr) auto auto auto minmax(0, 1fr);
      column-gap: clamp(32px, 4vw, 72px);
      row-gap: 22px;
      overflow: hidden;
    }
    .panel.with-lyrics > :global(*) {
      margin: 0;
      max-width: none;
    }
    .panel.with-lyrics > .lyr-time {
      display: none;
    }
    .panel.with-lyrics .np {
      grid-column: 1;
      grid-row: 2;
      grid-template-columns: 120px minmax(0, 1fr);
      gap: 24px;
      align-items: end;
    }
    .panel.with-lyrics h1 {
      font-size: var(--t-2xl);
    }
    .panel.with-lyrics .artist {
      font-size: var(--t-lg);
    }
    .panel.with-lyrics .cover-initial {
      font-size: 4rem;
    }
    .panel.with-lyrics > :global(.display) {
      grid-column: 1;
      grid-row: 3;
    }
    .panel.with-lyrics > .keys {
      grid-column: 1;
      grid-row: 4;
    }
    .panel.with-lyrics > :global(.lyrics) {
      grid-column: 2;
      grid-row: 1 / -1;
      max-width: 760px;
      min-height: 0;
    }
  }
  /* phones: the cover and title in a row, then the words */
  @media (max-width: 700px), (max-height: 500px) {
    .panel.with-lyrics {
      overflow: hidden;
      gap: 14px;
    }
    .panel.with-lyrics .np {
      grid-template-columns: 56px minmax(0, 1fr);
      justify-items: start;
      text-align: start;
      gap: 14px;
    }
    .panel.with-lyrics .np .cover {
      width: 56px;
      border-radius: var(--r-sm);
    }
    .panel.with-lyrics .link {
      text-align: start;
    }
    .panel.with-lyrics h1 {
      font-size: var(--t-lg);
      margin-bottom: 0;
    }
    .panel.with-lyrics .artist {
      font-size: var(--t-sm);
    }
    .panel.with-lyrics .album {
      display: none;
    }
    .panel.with-lyrics > :global(.lyrics) {
      order: 1;
      min-height: 0;
    }
    .panel.with-lyrics > .lyr-time {
      order: 2;
    }
    .panel.with-lyrics > .keys {
      order: 3;
    }
  }
  /* a phone held sideways: title and keys on the left, the words on the right */
  @media (max-height: 500px) and (min-width: 600px) {
    .panel.with-lyrics {
      display: grid;
      grid-template-columns: minmax(0, 1fr) minmax(0, 1.3fr);
      grid-template-rows: minmax(0, 1fr) auto auto auto minmax(0, 1fr);
      column-gap: 28px;
    }
    .panel.with-lyrics > :global(*) {
      margin: 0;
    }
    .panel.with-lyrics .np {
      grid-column: 1;
      grid-row: 2;
    }
    .panel.with-lyrics > .lyr-time {
      grid-column: 1;
      grid-row: 3;
    }
    .panel.with-lyrics > .keys {
      grid-column: 1;
      grid-row: 4;
    }
    .panel.with-lyrics > :global(.lyrics) {
      grid-column: 2;
      grid-row: 1 / -1;
    }
  }
</style>
