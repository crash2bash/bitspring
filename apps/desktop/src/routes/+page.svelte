<script lang="ts">
  import { onMount, untrack } from "svelte";
  import { api, inTauri, isMac, isIOS, isAndroid, khz, dsdName, artistLabel, errorText, AUDIO_EXTENSIONS, type Device, type DspSettings, type LibraryGoto, type EqState, type IrInfo, type NowPlaying, type PlaylistEntry, type Status, type StreamingStatus } from "$lib/api";
  import { i18n, sentences, t } from "$lib/i18n.svelte";
  import Settings from "$lib/Settings.svelte";
  import { labels, labelOf } from "$lib/labels.svelte";
  import AddToPlaylist from "$lib/AddToPlaylist.svelte";
  import Library from "$lib/Library.svelte";
  import Streaming from "$lib/Streaming.svelte";
  import DevicePicker from "$lib/DevicePicker.svelte";
  import Display from "$lib/Display.svelte";
  import Enhancers from "$lib/Enhancers.svelte";
  import Equalizer from "$lib/Equalizer.svelte";
  import UpdateNotice from "$lib/UpdateNotice.svelte";
  import { startUpdateChecks } from "$lib/updates.svelte";

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
  // iOS has no shared/exclusive choice: our playback session never mixes with other apps.
  let exclusive = $state<boolean>(isIOS ? true : load("exclusive", true));
  /** Devices the user switched DoP on for (by UID): DSD goes to them as DoP, not PCM. */
  let dopDevices = $state<string[]>(load("dopDevices", []));
  let dop = $derived(dopDevices.includes(selected));

  let queue = $state<string[]>([]);
  let index = $state(-1);
  let now = $state<NowPlaying | null>(null);
  let status = $state<Status>({ active: false, position: 0, duration: 0, paused: false, finished: false, peakDb: null, underruns: 0, buffering: false, buffered: 0, error: null, queueIndex: null, nowRev: 0, advanceError: null, systemVolume: null });
  /** The backend's nowRev we are showing; when it moves, the queue went on by itself. */
  let knownRev = 0;
  let lastOutputError: string | null = null;
  // `eqPath` is the single AutoEQ file of earlier versions: moved into the preset store on start.
  const savedDsp = load<Partial<DspSettings> & { eqPath?: string | null }>("dsp", {});
  const legacyEqPath = savedDsp.eqPath ?? null;
  delete savedDsp.eqPath;
  let dsp = $state<DspSettings>({ upsampleKhz: null, crossfeed: null, convolution: null, headroom: true, restoreHighs: false, ...savedDsp });
  let ir = $state<IrInfo | null>(null);
  let eqState = $state<EqState | null>(null);
  /** The EQ preset that plays on the picked output (null = EQ off there). */
  let eqName = $derived.by(() => {
    if (!eqState) return null;
    const b = eqState.devices[selected];
    const id = b ? (b.mode === "preset" ? b.id : null) : eqState.enabled ? eqState.activeId : null;
    return eqState.presets.find((p) => p.id === id)?.name ?? null;
  });
  let error = $state<string | null>(null);
  let loading = $state(false);
  let testing = $state(false);
  let dragOver = $state(false);
  let sweepKey = $state(0);
  let advancing = false;
  let screen = $state<"player" | "library" | "queue" | "streaming" | "eq">(load("screen", "player"));
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
  $effect(() => save("screen", screen));
  /** The library tab on a phone is the library alone; a desktop window shows the queue beside everything. */
  const phone = typeof window !== "undefined" ? window.matchMedia("(max-width: 700px)") : null;
  onMount(() => {
    const fit = () => {
      if (!phone?.matches && screen === "queue") screen = "player";
    };
    fit();
    phone?.addEventListener("change", fit);
    return () => phone?.removeEventListener("change", fit);
  });

  let deviceName = $derived(devices.find((d) => d.uid === selected)?.name ?? "DAC");

  async function refreshDevices() {
    try {
      devices = await api.listDevices();
      if (!devices.some((d) => d.uid === selected)) {
        selected = (devices.find((d) => d.isDefault) ?? devices[0])?.uid ?? "";
      }
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
    const files = await api.expandPaths(paths);
    if (!files.length) {
      error = t("err.noAudio");
      return;
    }
    const first = queue.length;
    queue.push(...files);
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

  /** Replace the queue with `paths` and play from `start` (library "Play"). */
  async function playPaths(paths: string[], start: number) {
    queue = [...paths];
    await playIndex(start);
  }

  /** Append to the queue; start playing if nothing is. */
  async function enqueue(paths: string[]) {
    const first = queue.length;
    queue.push(...paths);
    if (!status.active || status.finished) await playIndex(first);
  }

  function removeAt(i: number) {
    queue.splice(i, 1);
    if (i < index) index--;
    else if (i === index) {
      api.stop();
      now = null;
      index = -1;
    }
  }

  function clearQueue() {
    api.stop();
    queue = [];
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

  async function next() {
    if (index < queue.length - 1) await playIndex(index + 1);
  }

  async function prev() {
    if (status.position > 3 || index <= 0) await api.seek(0);
    else await playIndex(index - 1);
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

  /** Apply processing settings; a playing track continues in place with the new chain. */
  async function applyDsp(next: DspSettings) {
    try {
      const res = await api.setDsp(next);
      dsp = next;
      ir = res.convolution;
      save("dsp", next);
      if (res.signal && now && !testing) {
        now = { ...now, signal: res.signal };
        sweepKey++;
      }
      error = null;
    } catch (e) {
      error = t("err.dsp", { error: errorText(e) });
      // An unreadable impulse response shouldn't take the rest of the settings down with it.
      if (next.convolution && next.convolution !== dsp.convolution) return;
      if (next.convolution) await applyDsp({ ...next, convolution: null });
    }
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

  async function loadEq() {
    try {
      // Earlier versions kept one AutoEQ file: it becomes the first preset.
      if (legacyEqPath && inTauri) {
        const s = await api.eqState();
        if (!s.presets.length) await api.eqImportFile(legacyEqPath).catch(() => null);
        save("dsp", $state.snapshot(dsp));
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
    const labels = Object.fromEntries(MENU_KEYS.map((k) => [`menu.${k}`, t(`menu.${k}`, { app: "Hifi" })]));
    api.setMenuLabels(labels).catch(() => {});
  });

  // Mirror the queue to the backend, which starts the next track by itself.
  $effect(() => {
    api.setQueue($state.snapshot(queue), index < 0 ? null : index, knownRev);
  });

  // Switching device or access mode mid-track: continue the same track on the new output.
  let lastOutput = untrack(() => `${selected}|${exclusive}`);
  $effect(() => {
    const key = `${selected}|${exclusive}`;
    save("device", selected);
    save("exclusive", exclusive);
    if (key !== lastOutput) {
      lastOutput = key;
      // Same position, same pause state: switching outputs while paused stays paused.
      if (status.active && !testing && index >= 0) playIndex(index, status.position, status.paused);
    }
  });

  // DoP switched for the current device: only a DSD track needs to restart.
  let lastDop = untrack(() => dop);
  $effect(() => {
    save("dopDevices", dopDevices);
    if (dop !== lastDop) {
      lastDop = dop;
      if (untrack(() => status.active && !testing && index >= 0 && now?.signal.dsdRate !== null))
        playIndex(index, untrack(() => status.position), untrack(() => status.paused));
    }
  });

  function onKey(e: KeyboardEvent) {
    const t = e.target as HTMLElement;
    // Handled already (an EQ band moved by the arrow keys), or typing in a field.
    if (e.defaultPrevented || t.tagName === "SELECT" || t.tagName === "TEXTAREA") return;
    if (t.tagName === "INPUT" && (t as HTMLInputElement).type !== "range") return;
    if (e.code === "Space") {
      e.preventDefault();
      togglePause();
    } else if (e.key === "ArrowRight" && status.active) {
      api.seek(Math.min(status.position + 10, status.duration));
    } else if (e.key === "ArrowLeft" && status.active) {
      api.seek(Math.max(status.position - 10, 0));
    }
  }

  onMount(() => {
    refreshDevices();
    refreshStreaming();
    // Restore last session's processing (also loads and validates the impulse response).
    applyDsp(untrack(() => dsp));
    loadEq();
    const stopUpdateChecks = startUpdateChecks();
    const timer = setInterval(async () => {
      status = await api.status();
      // The backend moved on to the next track (or stopped at one it couldn't play).
      if (status.nowRev !== knownRev && !loading) {
        knownRev = status.nowRev;
        const n = await api.nowPlaying();
        if (n && !testing) {
          now = n;
          sweepKey++;
        }
        if (status.queueIndex !== null) index = status.queueIndex;
        if (status.advanceError) error = t("err.play", { name: status.advanceError.name, error: errorText(status.advanceError.error) });
      }
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
    if (inTauri) {
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
    };
  });
</script>

<svelte:window onkeydown={onKey} />

<div class="app" class:drag-over={dragOver}>
  <header data-tauri-drag-region class:mac={isMac} class:chrome-right={isMac && i18n.system.dir === "rtl"}>
    <span class="wordmark" data-tauri-drag-region>Hifi</span>
    <nav class="tabs" aria-label={t("nav.screens")}>
      <button class:sel={screen === "player"} aria-current={screen === "player" ? "page" : undefined} onclick={() => (screen = "player")}>{t("nav.player")}</button>
      <button class:sel={screen === "library"} aria-current={screen === "library" ? "page" : undefined} onclick={() => (screen = "library")}>{t("nav.library")}</button>
      {#if streaming?.available}
        <button class:sel={screen === "streaming"} aria-current={screen === "streaming" ? "page" : undefined} onclick={() => (screen = "streaming")}>{t("nav.streaming")}</button>
      {/if}
      <button class:sel={screen === "eq"} aria-current={screen === "eq" ? "page" : undefined} onclick={() => (screen = "eq")}>{t("nav.eq")}</button>
    </nav>
    <div class="controls">
      <Enhancers settings={dsp} {eqName} {ir} onchange={applyDsp} onopeneq={() => (screen = "eq")} />
      <DevicePicker {devices} bind:selected bind:exclusive bind:dopDevices onrefresh={refreshDevices} onchanneltest={channelTest} />
      <Settings />
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
  <main class="panel" class:phone-hidden={screen === "queue"}>
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
        {:else}
          <h1 class="idle">{t("np.idle")}</h1>
          <p class="album">{sentences([t(isAndroid ? "np.hintAndroid" : isIOS ? "np.hintIos" : "np.hint"), t("np.promise")])}</p>
          {#if !isAndroid}<button class="primary" onclick={pickFiles}>{t("common.addFiles")}</button>{/if}
        {/if}
      </div>
    </div>

    <Display {now} {status} {testing} error={error ?? (status.error && errorText(status.error))} {deviceName} {sweepKey} onseek={(s) => api.seek(s)} />

    <!-- transport keys keep their tape direction in right-to-left languages -->
    <div class="keys" dir="ltr">
      <button class="key" onclick={prev} aria-label={t("common.prev")} disabled={!now || testing}>
        <svg width="18" height="18" viewBox="0 0 22 22" aria-hidden="true"><path d="M6 4v14M18 4L8 11l10 7z" fill="currentColor" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" /></svg>
      </button>
      <button class="play" onclick={togglePause} aria-label={t(status.active && !status.paused ? "common.pause" : "common.play")} disabled={loading || (!now && !queue.length)}>
        {#if status.active && !status.paused}
          <svg width="24" height="24" viewBox="0 0 26 26" aria-hidden="true"><rect x="6" y="5" width="5" height="16" rx="1" fill="currentColor" /><rect x="15" y="5" width="5" height="16" rx="1" fill="currentColor" /></svg>
        {:else}
          <svg width="24" height="24" viewBox="0 0 26 26" aria-hidden="true"><path d="M8 4.5v17l14-8.5z" fill="currentColor" /></svg>
        {/if}
      </button>
      <button class="key" onclick={next} aria-label={t("common.next")} disabled={testing || index >= queue.length - 1}>
        <svg width="18" height="18" viewBox="0 0 22 22" aria-hidden="true"><path d="M16 4v14M4 4l10 7-10 7z" fill="currentColor" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" /></svg>
      </button>
    </div>
  </main>
  {:else if screen === "eq"}
    <main class="lib">
      <Equalizer {devices} device={selected} convolution={ir} onsignal={eqSignal} onstate={(s) => (eqState = s)} onpickir={pickIr} onclearir={() => applyDsp({ ...dsp, convolution: null })} />
    </main>
  {:else if screen === "streaming" && streaming?.available}
    <main class="lib">
      <Streaming status={streaming} currentPath={testing ? null : (now?.path ?? null)} onstatus={refreshStreaming} onplay={playPaths} onenqueue={enqueue} />
    </main>
  {:else}
    <main class="lib">
      <Library currentPath={testing ? null : (now?.path ?? null)} goto={libGoto} {streaming} onplay={playPaths} onenqueue={enqueue} />
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
        <i aria-hidden="true"></i>{t(now.signal.bitPerfect ? "signal.bitPerfect" : "signal.processing")}{t("list.sep")}{#if now.signal.dop && now.signal.dsdRate}{dsdName(now.signal.dsdRate)} DoP{:else}{khz(now.signal.deviceRate)} {t("unit.khz")}{/if}
      </span>
      <div class="mini-keys" dir="ltr">
        <button onclick={prev} aria-label={t("common.prev")} disabled={testing}>
          <svg width="14" height="14" viewBox="0 0 22 22" aria-hidden="true"><path d="M6 4v14M18 4L8 11l10 7z" fill="currentColor" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" /></svg>
        </button>
        <button class="mini-play" onclick={togglePause} aria-label={t(status.active && !status.paused ? "common.pause" : "common.play")}>
          {#if status.active && !status.paused}
            <svg width="16" height="16" viewBox="0 0 26 26" aria-hidden="true"><rect x="6" y="5" width="5" height="16" rx="1" fill="currentColor" /><rect x="15" y="5" width="5" height="16" rx="1" fill="currentColor" /></svg>
          {:else}
            <svg width="16" height="16" viewBox="0 0 26 26" aria-hidden="true"><path d="M8 4.5v17l14-8.5z" fill="currentColor" /></svg>
          {/if}
        </button>
        <button onclick={next} aria-label={t("common.next")} disabled={testing || index >= queue.length - 1}>
          <svg width="14" height="14" viewBox="0 0 22 22" aria-hidden="true"><path d="M16 4v14M4 4l10 7-10 7z" fill="currentColor" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" /></svg>
        </button>
      </div>
    </div>
  {/if}

  <nav class="tabbar" aria-label={t("nav.screens")}>
    <button class:sel={screen === "player"} aria-current={screen === "player" ? "page" : undefined} onclick={() => (screen = "player")}>
      <svg width="22" height="22" viewBox="0 0 22 22" aria-hidden="true"><circle cx="11" cy="11" r="8.5" fill="none" stroke="currentColor" stroke-width="1.6" /><circle cx="11" cy="11" r="2.2" fill="currentColor" /></svg>
      {t("nav.player")}
    </button>
    <button class:sel={screen === "library"} aria-current={screen === "library" ? "page" : undefined} onclick={() => (screen = "library")}>
      <svg width="22" height="22" viewBox="0 0 22 22" aria-hidden="true"><rect x="3" y="3" width="7" height="7" rx="1.5" fill="none" stroke="currentColor" stroke-width="1.6" /><rect x="12" y="3" width="7" height="7" rx="1.5" fill="none" stroke="currentColor" stroke-width="1.6" /><rect x="3" y="12" width="7" height="7" rx="1.5" fill="none" stroke="currentColor" stroke-width="1.6" /><rect x="12" y="12" width="7" height="7" rx="1.5" fill="none" stroke="currentColor" stroke-width="1.6" /></svg>
      {t("nav.library")}
    </button>
    {#if streaming?.available}
      <button class:sel={screen === "streaming"} aria-current={screen === "streaming" ? "page" : undefined} onclick={() => (screen = "streaming")}>
        <svg width="22" height="22" viewBox="0 0 22 22" aria-hidden="true"><path d="M3 13a8 8 0 0 1 16 0M6.5 13a4.5 4.5 0 0 1 9 0" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" /><circle cx="11" cy="14.5" r="1.8" fill="currentColor" /></svg>
        {t("nav.streaming")}
      </button>
    {/if}
    <button class:sel={screen === "eq"} aria-current={screen === "eq" ? "page" : undefined} onclick={() => (screen = "eq")}>
      <svg width="22" height="22" viewBox="0 0 22 22" aria-hidden="true"><path d="M5 3v16M11 3v16M17 3v16" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" /><rect x="2.5" y="12" width="5" height="3" rx="1.5" fill="var(--slate-900)" stroke="currentColor" stroke-width="1.6" /><rect x="8.5" y="6" width="5" height="3" rx="1.5" fill="var(--slate-900)" stroke="currentColor" stroke-width="1.6" /><rect x="14.5" y="10" width="5" height="3" rx="1.5" fill="var(--slate-900)" stroke="currentColor" stroke-width="1.6" /></svg>
      {t("nav.eq")}
    </button>
    <button class:sel={screen === "queue"} aria-current={screen === "queue" ? "page" : undefined} onclick={() => (screen = "queue")}>
      <svg width="22" height="22" viewBox="0 0 22 22" aria-hidden="true"><path d="M3 5h16M3 11h16M3 17h10" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" /></svg>
      {t("nav.queue")}{#if queue.length}<span class="badge num">{queue.length}</span>{/if}
    </button>
  </nav>

  <UpdateNotice playing={status.active && !status.paused} />

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
    font-family: var(--font-display);
    font-weight: 600;
    font-size: var(--t-lg);
    letter-spacing: 0.04em;
  }
  .tabs {
    display: flex;
    gap: 4px;
    margin-inline-start: 28px;
    margin-inline-end: auto;
  }
  .tabs button {
    padding: 6px 12px;
    border-radius: 6px;
    font-size: var(--t-sm);
    color: var(--mist);
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
    border-radius: 4px;
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
    color: var(--brass);
  }
  .mini-art:hover {
    outline: 1px solid var(--brass);
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
    border: 1.5px solid var(--brass);
    color: var(--brass) !important;
  }

  .controls {
    display: flex;
    gap: 10px;
    align-items: center;
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
    color: var(--brass);
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
    border-radius: 6px;
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
    outline: 1px solid var(--brass);
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
    color: var(--brass);
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
    color: var(--brass);
  }
  .album {
    margin: 2px 0 0;
    color: var(--mist);
    max-width: 60ch;
  }
  .primary {
    margin-top: 14px;
    padding: 9px 16px;
    border-radius: var(--r-md);
    background: var(--brass);
    color: var(--slate-900);
    font-weight: 600;
  }
  .primary:hover {
    filter: brightness(1.08);
  }

  /* hardware keys */
  .keys {
    display: flex;
    justify-content: center;
    align-items: center;
    gap: 18px;
  }
  .key {
    width: 52px;
    height: 38px;
    border-radius: 8px;
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
    color: var(--brass);
    border: 2px solid var(--brass);
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
    border: 2px dashed var(--brass);
    border-radius: 14px;
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
    color: var(--brass);
  }

  /* touch screens have no hover to reveal controls */
  @media (hover: none) {
    .q-remove {
      opacity: 1;
    }
  }

  /* --- phone: one screen at a time, tab bar at the bottom --- */
  @media (max-width: 700px) {
    .app {
      grid-template-columns: minmax(0, 1fr);
      grid-template-rows: auto minmax(0, 1fr) auto auto;
      height: 100dvh;
    }
    header {
      gap: 10px;
      padding: env(safe-area-inset-top) 12px 0;
      min-height: calc(54px + env(safe-area-inset-top));
    }
    header .tabs {
      display: none;
    }
    .controls {
      margin-inline-start: auto;
      min-width: 0;
      gap: 8px;
    }
    header :global(.enh .state) {
      display: none;
    }
    header :global(.picker),
    header :global(.picker .trigger) {
      min-width: 0;
    }
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
      padding-bottom: env(safe-area-inset-bottom);
    }
    .tabbar button {
      position: relative;
      flex: 1;
      display: flex;
      flex-direction: column;
      align-items: center;
      gap: 3px;
      padding: 8px 0 6px;
      font-size: var(--t-xs);
      color: var(--mist);
    }
    .tabbar button.sel {
      color: var(--brass);
    }
    .badge {
      position: absolute;
      top: 3px;
      inset-inline-start: calc(50% + 8px);
      min-width: 18px;
      padding: 0 5px;
      border-radius: 9px;
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
    h1 {
      font-size: var(--t-xl);
    }
    .album {
      margin-inline: auto;
    }
  }

  @media (min-width: 701px) and (max-width: 1000px) {
    .app {
      grid-template-columns: 220px minmax(0, 1fr);
    }
    .panel {
      padding: 22px 24px;
    }
  }
  @media (min-width: 701px) and (max-height: 700px) {
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
</style>
