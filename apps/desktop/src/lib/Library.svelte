<script lang="ts">
  import { onMount, tick, untrack } from "svelte";
  import { api, artistLabel, errorText, coverUrl, entryFromTrack, inTauri, isAndroid, isIOS, longDuration, resolution, UNKNOWN_ARTIST, type Album, type Dynamics, type LibTrack, type QualityReport, type RipResult, type QualityVerdict, type LibraryGoto, type LibraryStatus, type StreamingStatus } from "./api";
  import { collator as makeCollator, i18n, list, num, sentences, t, tn } from "./i18n.svelte";
  import AlbumTile from "./AlbumTile.svelte";
  import { liquidPill } from "./liquid";
  import QualityDialog from "./QualityDialog.svelte";
  import TagEditor from "./TagEditor.svelte";
  import { SUSPECT, verdictLabel } from "./quality";
  import { remember } from "./labels.svelte";
  import AddToPlaylist from "./AddToPlaylist.svelte";
  import Playlists from "./Playlists.svelte";
  import LibraryFolders from "./LibraryFolders.svelte";
  import { folderLabel, folderName, isInside, parentOf } from "./folderPlace";
  import TrackTable, { type SortKey } from "./TrackTable.svelte";
  import Heart from "./Heart.svelte";
  import { marks, setAlbumFavorite } from "./marks.svelte";

  let {
    currentPath = null,
    goto = null,
    streaming = null,
    onplay,
    onshuffle,
    onenqueue,
  }: {
    currentPath?: string | null;
    streaming?: StreamingStatus | null;
    /** Show this artist or album (from a click on the now-playing track). */
    goto?: LibraryGoto | null;
    onplay: (paths: string[], start: number) => void;
    /** Play these in a random order. */
    onshuffle: (paths: string[]) => void;
    onenqueue: (paths: string[]) => void;
  } = $props();

  type View = "albums" | "artists" | "tracks" | "genres" | "playlists";
  type AlbumSort = "artist" | "title" | "year" | "added";

  function load<T>(key: string, fallback: T): T {
    try {
      const v = localStorage.getItem(`lib.${key}`);
      return v === null ? fallback : (JSON.parse(v) as T);
    } catch {
      return fallback;
    }
  }
  function save(key: string, v: unknown) {
    try {
      localStorage.setItem(`lib.${key}`, JSON.stringify(v));
    } catch {}
  }

  let status = $state<LibraryStatus | null>(null);
  let albums = $state<Album[]>([]);
  let tracks = $state<LibTrack[]>([]);
  let view = $state<View>(load("view", "albums"));
  let albumSort = $state<AlbumSort>(load("albumSort", "artist"));
  let trackSort = $state<SortKey>(load("trackSort", "artist"));
  let trackAsc = $state<boolean>(load("trackAsc", true));
  let hiResOnly = $state<boolean>(load("hiResOnly", false));
  /** Only what carries the listener's heart: favourite albums in the grids, favourite
   *  tracks in the track list. */
  let favOnly = $state<boolean>(load("favOnly", false));
  let query = $state("");
  let openAlbum = $state<Album | null>(null);
  let artist = $state<string | null>(null);
  let genre = $state<string | null>(null);
  let foldersOpen = $state(false);
  let foldersRoot = $state<HTMLElement>();
  let body: HTMLElement;

  $effect(() => {
    save("view", view);
    save("albumSort", albumSort);
    save("trackSort", trackSort);
    save("trackAsc", trackAsc);
    save("hiResOnly", hiResOnly);
    save("favOnly", favOnly);
  });

  const collator = $derived(makeCollator());
  /** Groups without a name (untagged artist, no genre): always last. */
  const NO_GENRE = "\u0000no-genre";
  const unnamed = (s: string) => s === UNKNOWN_ARTIST || s === NO_GENRE;
  const byArtist = (a: string, b: string) => Number(unnamed(a)) - Number(unnamed(b)) || collator.compare(a, b);
  /** A group name as shown. */
  const groupLabel = (s: string) => (s === NO_GENRE ? t("lib.noGenre") : artistLabel(s));
  const q = $derived(query.trim().toLowerCase());

  // ---- derived data ---------------------------------------------------------------------------
  let addedAt = $derived.by(() => {
    const m = new Map<string, number>();
    for (const t of tracks) m.set(t.albumId, Math.max(m.get(t.albumId) ?? 0, t.mtime));
    return m;
  });

  /** How many favourite tracks each album has (only albums that have any). */
  let favTracksIn = $derived.by(() => {
    const m = new Map<string, number>();
    for (const t of tracks) if (marks.fav(t.path)) m.set(t.albumId, (m.get(t.albumId) ?? 0) + 1);
    return m;
  });
  /** Under the heart filter an album shows for its own heart or for a heart on any of its
   *  tracks: whoever marked a track looks for it under "favourites" wherever they are. */
  const favAlbum = (a: Album) => marks.album(a.id) || favTracksIn.has(a.id);

  let visibleAlbums = $derived.by(() => {
    let list = albums.filter((a) => (!hiResOnly || a.hiRes) && (!favOnly || favAlbum(a)));
    if (q) list = list.filter((a) => `${a.title} ${artistLabel(a.artist)} ${a.genre ?? ""} ${a.year ?? ""}`.toLowerCase().includes(q));
    const by: Record<AlbumSort, (a: Album, b: Album) => number> = {
      artist: (a, b) => byArtist(a.artist, b.artist) || collator.compare(a.year ?? "", b.year ?? ""),
      title: (a, b) => collator.compare(a.title, b.title),
      year: (a, b) => collator.compare(b.year ?? "", a.year ?? ""),
      added: (a, b) => (addedAt.get(b.id) ?? 0) - (addedAt.get(a.id) ?? 0),
    };
    return [...list].sort(by[albumSort]);
  });

  /** The track list as its table sorts it: the order "Shuffle" comes back to when switched off. */
  let shownTracks = $state<LibTrack[]>([]);
  let visibleTracks = $derived.by(() => {
    let list = tracks.filter((t) => (!hiResOnly || t.hiRes) && (!favOnly || marks.fav(t.path)));
    if (q) list = list.filter((t) => `${t.title} ${t.artist ?? ""} ${t.album ?? ""}`.toLowerCase().includes(q));
    return list;
  });

  function groupBy(key: (a: Album) => string | null) {
    const m = new Map<string, Album[]>();
    for (const a of visibleAlbums) {
      const k = key(a);
      if (k) m.set(k, [...(m.get(k) ?? []), a]);
    }
    return [...m.entries()].sort((x, y) => byArtist(x[0], y[0]));
  }
  let artists = $derived(groupBy((a) => a.artist));
  let genres = $derived(groupBy((a) => a.genre ?? NO_GENRE));

  let shelf = $derived(
    view === "artists" && artist
      ? (artists.find(([n]) => n === artist)?.[1] ?? [])
      : view === "genres" && genre
        ? (genres.find(([n]) => n === genre)?.[1] ?? [])
        : visibleAlbums,
  );

  /** What the counter says: the list as it is shown, so a filter or a search is seen to work
   *  (playlists are not the library's own list: there it stays the library's size). */
  let shownCount = $derived(
    view === "tracks" ? tn("lib.nTracks", visibleTracks.length) : view === "playlists" ? tn("lib.nAlbums", status?.albums ?? 0) : tn("lib.nAlbums", shelf.length),
  );

  let currentAlbumId = $derived(tracks.find((t) => t.path === currentPath)?.albumId ?? null);
  let albumTracks = $derived(openAlbum ? openAlbum.tracks.map((i) => tracks[i]).filter(Boolean) : []);

  // ---- quality check --------------------------------------------------------------------------
  let quality = $state<Record<string, QualityReport>>({});
  let checking = $state<{ album: string; done: number; total: number } | null>(null);
  let qualityError = $state<string | null>(null);
  let shownQuality = $state<LibTrack | null>(null);
  let editingTags = $state(false);
  // After a tag edit the album may get a new id (a new title or artist): follow its tracks.
  let followPath: string | null = null;
  /** The folder all of the open album's files are in, when it is one that can be skipped:
   *  inside a library folder, not one itself (those are handled in the folders panel). */
  const albumFolder = $derived.by(() => {
    if (!albumTracks.length || !status) return null;
    let common = parentOf(albumTracks[0].path);
    for (const tr of albumTracks) {
      while (!isInside(tr.path, common) && parentOf(common) !== common) common = parentOf(common);
    }
    const inLibrary = status.folders.some((f) => f !== common && isInside(common, f));
    return inLibrary && !status.folders.includes(common) ? common : null;
  });
  /** "Don't show this folder" was pressed and waits for a yes. */
  let hiding = $state(false);
  async function hideFolder() {
    if (!albumFolder) return;
    status = await api.librarySkipFolder(albumFolder);
    hiding = false;
    await leaveAlbum();
  }

  // ---- dynamics (DR, loudness range): measured with the quality check and with levelling ------
  let dynamics = $state<Record<string, Dynamics>>({});
  let albumDynamics = $state<Record<string, Dynamics>>({});
  let dynamicsRev: number | null = null;
  async function loadDynamics() {
    try {
      const d = await api.dynamics(dynamicsRev);
      dynamicsRev = d.rev;
      if (d.tracks) [dynamics, albumDynamics] = [d.tracks, d.albums ?? {}];
    } catch {
      // (An older backend, or the mock without it: the column simply stays away.)
    }
  }
  /** The open album's figures as one line: "DR 9, loudness range 7 LU". */
  let openDynamics = $derived.by(() => {
    const d = openAlbum && albumDynamics[openAlbum.id];
    if (!d) return "";
    return list([d.dr != null && t("dyn.dr", { n: num(Math.round(d.dr)) }), d.lra != null && t("dyn.lra", { n: num(Math.round(d.lra)) })]);
  });
  let anyDynamics = $derived(visibleTracks.some((tr) => dynamics[tr.path]?.dr != null));

  let albumQuality = $derived(albumTracks.map((tr) => quality[tr.path]).filter(Boolean));
  // (An album checked before the dynamics were measured still owes those.)
  let albumChecked = $derived(albumTracks.length > 0 && albumQuality.length === albumTracks.length && albumTracks.every((tr) => dynamics[tr.path]));
  /** "Upsampled (11), Genuine (1)": the album's verdicts, most common first. */
  let qualitySummary = $derived.by(() => {
    const counts = new Map<QualityVerdict, number>();
    for (const r of albumQuality) counts.set(r.verdict.kind, (counts.get(r.verdict.kind) ?? 0) + 1);
    return [...counts].sort((a, b) => b[1] - a[1]).map(([k, n]) => (counts.size > 1 ? `${verdictLabel(k)} (${num(n)})` : verdictLabel(k)));
  });

  // Results from earlier checks show as soon as the album opens.
  $effect(() => {
    const paths = albumTracks.map((tr) => tr.path).filter((p) => !quality[p]);
    if (!paths.length) return;
    api.qualityCached(paths).then((rs) => rs.forEach((r, i) => r && (quality[paths[i]] = r)));
  });

  async function checkQuality() {
    const album = openAlbum;
    if (!album || checking) return;
    const todo = albumTracks.filter((tr) => !quality[tr.path] || !dynamics[tr.path]);
    checking = { album: album.id, done: 0, total: todo.length };
    qualityError = null;
    // A few at once: each decodes a whole file, and the disk may be slow.
    let next = 0;
    const worker = async () => {
      while (next < todo.length) {
        const tr = todo[next++];
        try {
          quality[tr.path] = await api.qualityCheck(tr.path);
        } catch (e) {
          qualityError = t("q.error", { name: tr.title, error: errorText(e) });
        }
        if (checking) checking.done++;
      }
    };
    await Promise.all([worker(), worker(), worker()]);
    await loadDynamics();
    checking = null;
  }

  // ---- rip check: a CD rip against other people's rips of the same disc ------------------------
  let rips = $state<Record<string, RipResult>>({});
  let ripping = $state<{ album: string; done: number; total: number } | null>(null);
  let ripError = $state<string | null>(null);
  /** "Verify the rip" was pressed and waits for a yes: it is the one thing here that goes online. */
  let ripAsking = $state(false);
  /** What a CD holds: 16 bits, 44.1 kHz, stereo, and not a lossy copy of it. */
  let ripFit = $derived(albumTracks.length > 0 && albumTracks.every((tr) => tr.lossless && tr.bits === 16 && tr.sampleRate === 44_100 && tr.channels === 2));
  let rip = $derived(openAlbum ? (rips[openAlbum.id] ?? null) : null);
  /** Nothing known yet — or the database didn't have the disc, and may by now. */
  let ripOffer = $derived(ripFit && (!rip || (rip.kind === "checked" && rip.status === "unknown")));
  $effect(() => {
    const album = openAlbum;
    if (!album || !ripFit || rips[album.id]) return;
    const paths = albumTracks.map((tr) => tr.path);
    api.ripCached(paths).then((r) => r && (rips[album.id] = r)).catch(() => {});
  });

  async function checkRip() {
    const album = openAlbum;
    if (!album || ripping) return;
    ripAsking = false;
    ripError = null;
    const paths = albumTracks.map((tr) => tr.path);
    ripping = { album: album.id, done: 0, total: paths.length };
    const watch = setInterval(async () => {
      const p = await api.ripProgress().catch(() => null);
      if (p && ripping) [ripping.done, ripping.total] = p;
    }, 400);
    try {
      rips[album.id] = await api.ripCheck(paths);
    } catch (e) {
      ripError = t("rip.error", { error: errorText(e) });
    }
    clearInterval(watch);
    ripping = null;
  }

  /** The verdict in a sentence. */
  let ripText = $derived.by(() => {
    if (!rip) return "";
    if (rip.kind === "unfit") return t(`rip.unfit.${rip.why}`);
    switch (rip.status) {
      case "verified":
        return sentences([tn("rip.verified", rip.matching), rip.offset !== 0 && t("rip.pressing", { n: num(Math.abs(rip.offset)) })]);
      case "partly": {
        const bad = rip.tracks.flatMap((ok, i) => (ok === false ? [num(albumTracks[i]?.trackNumber ?? i + 1)] : []));
        return tn("rip.partly", bad.length, { tracks: list(bad) });
      }
      case "different":
        return tn("rip.different", rip.known);
      case "unknown":
        return t("rip.unknown");
    }
  });
  let ripBad = $derived(rip?.kind === "checked" && (rip.status === "partly" || rip.status === "different"));
  let showRip = $derived(!!ripText && !ripping);
  let showQuality = $derived(albumQuality.length > 0 && !checking);
  /** Anything for the strip under the album's head. */
  let fileFacts = $derived(ripAsking || !!ripError || !!qualityError || showRip || showQuality || !!openDynamics || !!albumFolder);
  /** Tracks the database's rips disagree with. */
  const ripMiss = (i: number) => rip?.kind === "checked" && rip.status === "partly" && rip.tracks[i] === false;

  // ---- data loading ---------------------------------------------------------------------------
  let revision = -1;
  let ticks = 0;
  async function refresh() {
    status = await api.libraryStatus();
    // What was measured meanwhile (a background analysis stores a track every moment or so:
    // looked at every few seconds, and only fetched when something changed).
    if (ticks++ % 4 === 0) loadDynamics();
    if (status.revision !== revision) {
      revision = status.revision;
      // (Albums may have regrouped: their figures are worked out again.)
      dynamicsRev = null;
      [albums, tracks] = await Promise.all([api.libraryAlbums(), api.libraryTracks()]);
      // The queue names tracks by their files; a CUE track's file is its whole album.
      remember(tracks.filter((t) => t.path.includes("#cue:")));
      if (openAlbum) {
        const i = followPath ? tracks.findIndex((tr) => tr.path === followPath) : -1;
        openAlbum = albums.find((a) => a.id === openAlbum!.id) ?? (i >= 0 ? (albums.find((a) => a.tracks.includes(i)) ?? null) : null);
        followPath = null;
      }
    }
  }

  onMount(() => {
    refresh();
    const timer = setInterval(refresh, 1000);
    return () => clearInterval(timer);
  });

  // ---- "go to artist / album" ----------------------------------------------------------------
  let applied = 0;
  $effect(() => {
    const g = goto;
    // Wait until the library has loaded (it may have only just been mounted).
    if (!g || g.n === applied || !status || (status.folders.length > 0 && tracks.length === 0)) return;
    applied = g.n;
    untrack(() => show(g));
  });

  const same = (a: string | null | undefined, b: string | null | undefined) =>
    !!a && !!b && a.localeCompare(b, i18n.locale.tag, { sensitivity: "base" }) === 0;

  /** Where the album grid was scrolled when an album opened: "Back" returns there. */
  let gridScroll = 0;
  async function enterAlbum(a: Album) {
    gridScroll = body?.scrollTop ?? 0;
    openAlbum = a;
    hiding = false;
    await tick();
    body?.scrollTo({ top: 0 });
  }
  async function leaveAlbum() {
    openAlbum = null;
    await tick();
    body?.scrollTo({ top: gridScroll });
  }

  function show(g: LibraryGoto) {
    query = "";
    // The playing file's own library entry knows its album; a file played from outside the
    // library falls back to matching by name.
    const t = tracks.find((x) => x.path === g.path);
    const album =
      (t && albums.find((a) => a.id === t.albumId)) ??
      albums.find((a) => same(a.title, g.album) && (!g.artist || same(a.artist, g.artist))) ??
      null;
    if (album && !album.hiRes) hiResOnly = false;
    if (album && !favAlbum(album)) favOnly = false;
    if (g.kind === "album") {
      if (album) {
        openAlbum = album;
      } else if (g.album) {
        // Not in the library: search for it instead.
        openAlbum = null;
        view = "albums";
        query = g.album;
      }
    } else {
      openAlbum = null;
      const name = album?.artist ?? albums.find((a) => same(a.artist, g.artist))?.artist;
      if (name) {
        view = "artists";
        artist = name;
      } else if (g.artist) {
        view = "albums";
        query = g.artist;
      }
    }
    body?.scrollTo({ top: 0 });
  }

  /** iPhone: copy files from the Files app into the app's music folder (the library). */
  async function importFiles() {
    if (!inTauri) return;
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({ multiple: true });
    if (!picked) return;
    await api.importFiles(Array.isArray(picked) ? picked : [picked]);
    status = await api.libraryStatus();
  }

  async function addFolder() {
    if (!inTauri) {
      status = await api.libraryAddFolder("/Volumes/Music");
      return;
    }
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({ directory: true, multiple: false, title: t("dialog.musicFolder") });
    if (typeof picked === "string") status = await api.libraryAddFolder(picked);
  }

  let pickError = $state("");
  /** Android: the system's folder picker. */
  async function pickFolder() {
    pickError = "";
    try {
      status = (await api.libraryPickFolder()) ?? status;
    } catch (e) {
      pickError = errorText(e);
    }
  }

  function playAlbum(a: Album, start = 0) {
    onplay(a.tracks.map((i) => tracks[i].path), start);
  }

  function onWindowClick(e: MouseEvent) {
    if (foldersOpen && foldersRoot && !e.composedPath().includes(foldersRoot)) foldersOpen = false;
  }

  const VIEWS: View[] = ["albums", "artists", "tracks", "genres", "playlists"];
  // (With "Favourites" on and nothing marked yet, say where the heart is.)
  const nothing = $derived(q ? t("lib.nothingFor", { q: query.trim() }) : favOnly ? t(view === "tracks" ? "fav.noTracks" : "fav.noAlbums") : t("lib.nothing"));
</script>

<svelte:window onclick={onWindowClick} />

<section class="library" aria-label={t("lib.label")}>
  <!-- An open album has its own Back button; the tabs, search and filters belong to the lists. -->
  {#if !openAlbum}
  <div class="toolbar">
    <div class="seg" role="tablist" aria-label={t("lib.section")} use:liquidPill={view}>
      {#each VIEWS as v (v)}
        <button role="tab" aria-selected={view === v} class:sel={view === v} onclick={() => ((view = v), (openAlbum = null))}>{t(`lib.${v}`)}</button>
      {/each}
    </div>

    <input class="search" type="search" placeholder={t("lib.search")} bind:value={query} aria-label={t("lib.searchLabel")} />

    {#if view !== "tracks" && view !== "playlists"}
      <label class="sort">
        <select bind:value={albumSort} aria-label={t("lib.sortLabel")}>
          {#each ["artist", "title", "year", "added"] as k (k)}
            <option value={k}>{t(`lib.sort.${k}`)}</option>
          {/each}
        </select>
      </label>
    {/if}

    {#if view === "tracks"}
      <!-- everything listed: the whole library, or what the search and the filter leave of it -->
      <button class="shuffle-all" onclick={() => onshuffle(shownTracks.map((t) => t.path))} disabled={!visibleTracks.length}>{t("common.shuffle")}</button>
    {/if}

    {#if view !== "playlists"}
      <label class="hires" class:on={hiResOnly}>
        <input type="checkbox" bind:checked={hiResOnly} />
        <span class="box" aria-hidden="true"></span>
        {t("lib.hiresOnly")}
      </label>
      <!-- the same kind of switch, as small as its mark: the heart it filters by -->
      <label class="hires fav-only" class:on={favOnly} title={t("fav.only")}>
        <input type="checkbox" bind:checked={favOnly} aria-label={t("fav.only")} />
        <Heart on={favOnly} size={17} />
      </label>
    {/if}

    <div class="folders" bind:this={foldersRoot}>
      <button
        class="folders-btn"
        onclick={() => (foldersOpen = !foldersOpen)}
        aria-expanded={foldersOpen}
        title={status ? t("lib.foldersTip", { tracks: tn("lib.nTracks", status.tracks), folders: tn("lib.inFolders", status.folders.length) }) : ""}
      >
        {#if status?.scanning}
          {t("lib.scanning", { done: num(status.scanning[0]), total: num(status.scanning[1]) })}
        {:else if status}
          {shownCount}
        {/if}
      </button>
      {#if foldersOpen && status}
        <div class="folders-panel" role="dialog" aria-label={t("lib.foldersDialog")}>
          <LibraryFolders bind:status onadd={addFolder} onimport={importFiles} />
        </div>
      {/if}
    </div>
  </div>
  {/if}

  <div class="body" bind:this={body}>
    {#if view === "playlists" && !openAlbum}
      <Playlists {tracks} {currentPath} {query} {streaming} {onplay} {onshuffle} {onenqueue} />
    {:else if isAndroid && status && status.tracks === 0 && !status.scanning}
      <div class="empty">
        <p class="empty-title">{t("lib.empty")}</p>
        <p>{t("lib.emptyAndroid")}</p>
        <div class="empty-actions">
          <button class="primary" onclick={async () => (status = await api.libraryRescan())}>{t("common.rescan")}</button>
          <button class="ghost" onclick={pickFolder}>{t("lib.addFolder")}</button>
        </div>
        {#if pickError}
          <p class="empty-error" role="alert">{pickError}</p>
        {/if}
      </div>
    {:else if isIOS && status && status.tracks === 0 && !status.scanning}
      <div class="empty">
        <p class="empty-title">{t("lib.empty")}</p>
        <p>{t("lib.emptyIos")}</p>
        <button class="primary" onclick={importFiles}>{t("common.addFiles")}</button>
      </div>
    {:else if status && status.folders.length === 0}
      <div class="empty">
        <p class="empty-title">{t("lib.empty")}</p>
        <p>{t("lib.emptyDesktop")}</p>
        <button class="primary" onclick={addFolder}>{t("lib.chooseFolder")}</button>
      </div>
    {:else if openAlbum}
      <div class="album">
        <button class="back" onclick={leaveAlbum}>
          <svg width="8" height="12" viewBox="0 0 8 12" aria-hidden="true"><path d="M7 1L2 6l5 5" fill="none" stroke="currentColor" stroke-width="1.6" /></svg>
          {t("lib.back")}
        </button>
        <div class="album-head">
          <div class="album-art">
            {#if openAlbum.hasCover}
              <img src={coverUrl(openAlbum.id)} alt={t("common.cover", { album: openAlbum.title })} />
            {:else}
              <span class="initial" aria-hidden="true">{openAlbum.title.slice(0, 1)}</span>
            {/if}
          </div>
          <div class="album-info">
            <div class="album-title">
              <h2>{openAlbum.title}</h2>
              <button
                class="album-heart"
                class:on={marks.album(openAlbum.id)}
                aria-pressed={marks.album(openAlbum.id)}
                aria-label={t(marks.album(openAlbum.id) ? "fav.removeAlbum" : "fav.addAlbum")}
                title={t(marks.album(openAlbum.id) ? "fav.removeAlbum" : "fav.addAlbum")}
                onclick={() => setAlbumFavorite(openAlbum!.id, !marks.album(openAlbum!.id))}
              >
                {#key openAlbum.id}<Heart on={marks.album(openAlbum.id)} size={24} beats />{/key}
              </button>
            </div>
            <button class="artist-link" onclick={() => ((artist = openAlbum!.artist), (view = "artists"), (openAlbum = null))}>{artistLabel(openAlbum.artist)}</button>
            <p class="facts">
              {list([openAlbum.year, openAlbum.genre, tn("lib.nTracks", openAlbum.trackCount), longDuration(openAlbum.duration)])}
            </p>
            <p class="facts fmt" class:hi={openAlbum.hiRes}>
              {list([
                openAlbum.format,
                !!openAlbum.maxBits && `${resolution(openAlbum.maxBits, openAlbum.maxRate)}${openAlbum.maxBits === 1 ? "" : ` ${t("unit.khz")}`}`,
                openAlbum.hiRes && "hi-res",
              ])}
            </p>
          </div>
          <div class="album-actions">
            <button class="primary" onclick={() => playAlbum(openAlbum!)}>{t("common.play")}</button>
            <button class="ghost" onclick={() => onshuffle(openAlbum!.tracks.map((i) => tracks[i].path))}>{t("common.shuffle")}</button>
            <button class="ghost" onclick={() => onenqueue(openAlbum!.tracks.map((i) => tracks[i].path))}>{t("lib.enqueue")}</button>
            <AddToPlaylist entries={() => albumTracks.map(entryFromTrack)} />
            {#if !isIOS && !isAndroid}
              <button class="ghost" onclick={() => (editingTags = true)}>{t("tags.edit")}</button>
            {/if}
            {#if !albumChecked}
              <button class="ghost" onclick={checkQuality} disabled={!!checking}>
                {checking?.album === openAlbum.id ? t("q.checking", { done: num(checking.done), total: num(checking.total) }) : t("q.check")}
              </button>
            {/if}
            {#if ripOffer}
              <button class="ghost" onclick={() => (ripAsking = true)} disabled={!!ripping || ripAsking}>
                {ripping?.album === openAlbum.id ? t("rip.reading", { done: num(ripping.done), total: num(ripping.total) }) : t("rip.check")}
              </button>
            {/if}
          </div>
        </div>
        {#if fileFacts}
          <!-- What the player found out about the files, apart from what the album is. -->
          <div class="album-files">
            {#if ripAsking}
              <p class="facts rip-ask">
                <span>{t("rip.ask")}</span>
                <span class="folder-answer">
                  <button class="quiet-link" onclick={checkRip}>{t("rip.go")}</button>
                  <button class="quiet-link" onclick={() => (ripAsking = false)}>{t("pl.cancel")}</button>
                </span>
              </p>
            {/if}
            {#if ripError}<p class="facts q-err">{ripError}</p>{/if}
            {#if qualityError}<p class="facts q-err">{qualityError}</p>{/if}
            {#if showRip || showQuality || openDynamics}
              <dl class="file-facts">
                {#if showRip}
                  <div class:q-err={ripBad}>
                    <dt>{t("rip.label")}</dt>
                    <dd>{ripText}</dd>
                  </div>
                {/if}
                {#if showQuality}
                  <div>
                    <dt>{t("q.summary")}</dt>
                    <dd>{list(qualitySummary)}</dd>
                  </div>
                {/if}
                {#if openDynamics}
                  <div title={t("dyn.help")}>
                    <dt>{t("dyn.label")}</dt>
                    <dd>{openDynamics}</dd>
                  </div>
                {/if}
              </dl>
            {/if}
            {#if albumFolder}
              <p class="facts folder">
                {#if hiding}
                  <span>{t("lib.hideFolderAsk", { name: folderName(albumFolder) })}</span>
                  <span class="folder-answer">
                    <button class="quiet-link" onclick={hideFolder}>{t("lib.skip")}</button>
                    <button class="quiet-link" onclick={() => (hiding = false)}>{t("pl.cancel")}</button>
                  </span>
                {:else}
                  <span class="folder-path" title={albumFolder}>{folderLabel(albumFolder)}</span>
                  <button class="quiet-link" onclick={() => (hiding = true)}>{t("lib.hideFolder")}</button>
                {/if}
              </p>
            {/if}
          </div>
        {/if}
        <TrackTable
          tracks={albumTracks}
          {currentPath}
          sortable={false}
          columns={albumTracks.some((tr) => dynamics[tr.path]?.dr != null) ? ["number", "title", "duration", "dr", "format"] : ["number", "title", "duration", "format"]}
          {dynamics}
          onplay={(_, i) => playAlbum(openAlbum!, i)}
        >
          {#snippet marker(tr)}
            {@const r = quality[tr.path]}
            {#if ripMiss(albumTracks.indexOf(tr))}
              <span class="q-badge suspect" title={t("rip.trackHelp")}>{t("rip.track")}</span>
            {/if}
            {#if r}
              <button
                class="q-badge {SUSPECT.includes(r.verdict.kind) ? 'suspect' : r.verdict.kind === 'genuine' ? 'ok' : 'open'}"
                onclick={(e) => (e.stopPropagation(), (shownQuality = tr))}
                ondblclick={(e) => e.stopPropagation()}
                title={verdictLabel(r.verdict.kind)}
              >
                {#if r.verdict.kind === "genuine"}
                  <svg width="11" height="11" viewBox="0 0 11 11" aria-label={verdictLabel("genuine")}><path d="M1.5 5.8l2.6 2.6 5.4-6" fill="none" stroke="currentColor" stroke-width="1.6" /></svg>
                {:else}
                  {verdictLabel(r.verdict.kind)}
                {/if}
              </button>
            {/if}
          {/snippet}
          {#snippet actions(tr)}<AddToPlaylist compact entries={() => [entryFromTrack(tr)]} />{/snippet}
        </TrackTable>
        {#if editingTags}
          <TagEditor tracks={albumTracks} onclose={() => (editingTags = false)} onsaved={(p) => (followPath = p)} />
        {/if}
        {#if shownQuality && quality[shownQuality.path]}
          <QualityDialog track={shownQuality} report={quality[shownQuality.path]} dynamics={dynamics[shownQuality.path]} onclose={() => (shownQuality = null)} />
        {/if}
      </div>
    {:else if view === "tracks"}
      {#if visibleTracks.length}
        <TrackTable
          tracks={visibleTracks}
          {currentPath}
          columns={anyDynamics ? ["number", "title", "artist", "album", "year", "duration", "dr", "format"] : undefined}
          {dynamics}
          bind:sortKey={trackSort}
          bind:sortAsc={trackAsc}
          bind:shown={shownTracks}
          onplay={(list, i) => onplay(list.map((t) => t.path), i)}
        >
          {#snippet actions(tr)}<AddToPlaylist compact entries={() => [entryFromTrack(tr)]} />{/snippet}
        </TrackTable>
      {:else}
        <p class="none">{nothing}</p>
      {/if}
    {:else}
      <div class="browse" class:split={view === "artists" || view === "genres"}>
        {#if view === "artists" || view === "genres"}
          {@const groups = view === "artists" ? artists : genres}
          {@const picked = view === "artists" ? artist : genre}
          <ul class="index" aria-label={t(`lib.${view}`)}>
            <li>
              <button class:sel={!picked} onclick={() => (view === "artists" ? (artist = null) : (genre = null))}>
                {t(view === "artists" ? "lib.allArtists" : "lib.allGenres")}
                <span class="count num">{visibleAlbums.length}</span>
              </button>
            </li>
            {#each groups as [name, albumsOf] (name)}
              <li>
                <button class:sel={picked === name} onclick={() => (view === "artists" ? (artist = name) : (genre = name))}>
                  {groupLabel(name)}<span class="count num">{albumsOf.length}</span>
                </button>
              </li>
            {/each}
          </ul>
        {/if}
        {#if shelf.length}
          <div class="grid">
            {#each shelf as a (a.id)}
              <AlbumTile album={a} playing={a.id === currentAlbumId} favorite={marks.album(a.id)} favTracks={favTracksIn.get(a.id) ?? 0} onopen={enterAlbum} />
            {/each}
          </div>
        {:else}
          <p class="none">{nothing}</p>
        {/if}
      </div>
    {/if}
  </div>
</section>

<style>
  .library {
    display: flex;
    flex-direction: column;
    min-height: 0;
    min-width: 0;
  }
  .toolbar {
    display: flex;
    /* Russian or Greek labels don't fit one row in a narrow window: the rest moves down a line. */
    flex-wrap: wrap;
    align-items: center;
    gap: 12px;
    padding: 14px 24px;
    border-bottom: 1px solid var(--slate-600);
  }
  .seg {
    flex: none;
    display: flex;
    background: var(--slate-900);
    border-radius: var(--r-md);
    padding: 3px;
    gap: 2px;
  }
  .seg button {
    padding: 6px 12px;
    border-radius: var(--r-md);
    font-size: var(--t-sm);
    color: var(--mist);
  }
  .seg button:hover {
    color: var(--porcelain);
  }
  .seg button.sel {
    background: var(--slate-600);
    color: var(--porcelain);
    font-weight: 600;
  }
  .search {
    flex: 1;
    min-width: 140px;
    max-width: 280px;
    padding: 7px 12px;
    border-radius: var(--r-md);
    border: 1px solid var(--slate-600);
    background: var(--slate-900);
    color: var(--porcelain);
    font: inherit;
    font-size: var(--t-sm);
    user-select: text;
    -webkit-user-select: text;
  }
  .search:focus {
    outline: 2px solid var(--porcelain);
    outline-offset: 0;
    border-color: transparent;
  }
  .sort,
  .hires {
    display: flex;
    align-items: center;
    gap: 6px;
    white-space: nowrap;
    font-size: var(--t-sm);
    color: var(--mist);
  }
  /* The size of the search field beside it. */
  .sort select {
    font-size: var(--t-sm);
    padding: 7px 12px;
  }
  /* "Hi-res only" is a control of the toolbar's size as well — a box the height of the search
     field with its own tick mark — instead of the system's small checkbox afloat beside the
     fields. */
  .hires {
    position: relative;
    box-sizing: border-box;
    gap: 8px;
    padding: 7px 12px;
    border: 1px solid var(--slate-600);
    border-radius: var(--r-md);
    background-color: var(--slate-900);
    cursor: pointer;
  }
  .hires:hover {
    border-color: var(--slate-500);
  }
  .hires.on {
    color: var(--porcelain);
    border-color: var(--slate-500);
    background-color: var(--slate-700);
  }
  .hires input {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    margin: 0;
    opacity: 0;
    cursor: pointer;
  }
  .hires:has(input:focus-visible) {
    outline: 2px solid var(--porcelain);
    outline-offset: 2px;
  }
  .box {
    flex: none;
    display: grid;
    place-items: center;
    width: 14px;
    height: 14px;
    /* A tick box in every look (the glass look's small radius would round it into a radio button). */
    border-radius: 4px;
    border: 1.5px solid var(--mist);
  }
  .hires.on .box {
    background: var(--porcelain);
    border-color: var(--porcelain);
  }
  .hires.on .box::after {
    content: "";
    width: 7px;
    height: 3.5px;
    margin-top: -2px;
    border-left: 2px solid var(--ink);
    border-bottom: 2px solid var(--ink);
    transform: rotate(-45deg);
  }
  .fav-only {
    /* as tall as the switch with words beside it */
    align-self: stretch;
    padding-inline: 9px;
  }
  /* "Shuffle" over the track list: a toolbar control of the same size. */
  .shuffle-all {
    font-size: var(--t-sm);
    padding: 7px 12px;
    border: 1px solid var(--slate-600);
    border-radius: var(--r-md);
    background-color: var(--slate-900);
    color: var(--mist);
    white-space: nowrap;
  }
  .shuffle-all:hover:not(:disabled) {
    color: var(--porcelain);
    border-color: var(--slate-500);
  }
  .shuffle-all:disabled {
    color: var(--slate-500);
    cursor: default;
  }
  :global(html[data-look="glass"]) .shuffle-all {
    background-color: rgba(255, 255, 255, 0.07);
    border-color: var(--glass-edge);
  }
  :global(html[data-look="glass"]) .hires {
    background-color: rgba(255, 255, 255, 0.07);
    border-color: var(--glass-edge);
  }
  :global(html[data-look="glass"]) .hires.on {
    background-color: rgba(255, 255, 255, 0.16);
  }

  .folders {
    position: relative;
    margin-inline-start: auto;
    min-width: 0;
  }
  .folders-btn {
    font-size: var(--t-sm);
    color: var(--mist);
    padding: 6px 4px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 100%;
  }
  .folders-btn:hover {
    color: var(--porcelain);
  }
  .folders-panel {
    position: absolute;
    inset-inline-end: 0;
    top: calc(100% + 6px);
    width: 380px;
    z-index: 20;
    background: var(--slate-700);
    border: 1px solid var(--slate-500);
    border-radius: var(--r-lg);
    padding: 14px 16px;
    box-shadow: 0 18px 40px rgba(5, 12, 15, 0.55);
    /* Two lists of folders can outgrow a small window: the panel scrolls, the page stays. */
    max-height: min(72vh, 600px);
    overflow-y: auto;
    overscroll-behavior: contain;
  }
  .body {
    flex: 1;
    min-height: 0;
    overflow: auto;
  }
  .browse {
    padding: 22px 24px 32px;
  }
  .browse.split {
    display: grid;
    grid-template-columns: 220px 1fr;
    gap: 24px;
    align-items: start;
  }
  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(156px, 1fr));
    gap: 24px 20px;
  }
  .index {
    list-style: none;
    margin: 0;
    padding: 0;
    position: sticky;
    top: 0;
  }
  .index button {
    width: 100%;
    display: flex;
    justify-content: space-between;
    gap: 8px;
    text-align: start;
    padding: 7px 10px;
    border-radius: var(--r-md);
    font-size: var(--t-sm);
    color: var(--mist);
  }
  .index button:hover {
    color: var(--porcelain);
    background: var(--slate-700);
  }
  .index button.sel {
    color: var(--porcelain);
    font-weight: 600;
    background: var(--slate-700);
  }
  .count {
    color: var(--mist-dim);
    font-weight: 400;
  }

  .album {
    padding: 16px 24px 32px;
  }
  /* the back chevron points against the reading direction */
  :global([dir="rtl"]) .back svg {
    transform: scaleX(-1);
  }
  .back {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    color: var(--mist);
    font-size: var(--t-sm);
    margin-bottom: 16px;
  }
  .back:hover {
    color: var(--porcelain);
  }
  /* The cover keeps to the top; the buttons sit on its bottom edge while the text beside it is
     shorter than the cover, and simply come last when it is longer (a narrow window). On a
     phone they leave the column and run under cover and text alike (see the media query). */
  .album-head {
    display: grid;
    grid-template-columns: 200px minmax(0, 1fr);
    grid-template-rows: auto 1fr;
    column-gap: 28px;
    margin-bottom: 24px;
  }
  .album-info {
    min-width: 0;
    display: flex;
    flex-direction: column;
    align-items: flex-start;
  }
  .album-art {
    grid-row: 1 / 3;
    align-self: start;
    aspect-ratio: 1;
    border-radius: var(--r-md);
    overflow: hidden;
    display: grid;
    place-items: center;
    background:
      repeating-linear-gradient(90deg, rgba(232, 237, 234, 0.05) 0 2px, transparent 2px 8px),
      var(--slate-700);
    box-shadow: 0 14px 30px rgba(5, 12, 15, 0.5);
  }
  .album-art img {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }
  .initial {
    font-family: var(--font-display);
    font-weight: 600;
    font-size: 4rem;
    color: var(--slate-500);
  }
  /* The album's heart follows its title, on the title's first line. */
  .album-title {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    max-width: 100%;
  }
  .album-heart {
    flex: none;
    display: grid;
    place-items: center;
    width: 40px;
    height: 40px;
    border-radius: 50%;
    color: var(--mist);
  }
  .album-heart.on {
    color: var(--porcelain);
  }
  /* hover only where there is a pointer: on a touch screen it sticks after a tap */
  @media (hover: hover) {
    .album-heart:hover {
      color: var(--porcelain);
      background: var(--slate-700);
    }
  }
  .album-info h2 {
    min-width: 0;
    margin: 0;
    font-family: var(--font-display);
    font-weight: 600;
    font-size: var(--t-2xl);
    line-height: 1.05;
  }
  .artist-link {
    font-size: var(--t-lg);
    font-weight: 500;
    margin-top: 4px;
  }
  .artist-link:hover {
    color: var(--porcelain);
    text-decoration: underline;
    text-underline-offset: 3px;
  }
  .facts {
    margin: 6px 0 0;
    color: var(--mist);
    font-size: var(--t-sm);
  }
  .facts.fmt.hi {
    color: var(--porcelain);
  }
  .album-actions {
    grid-column: 2;
    align-self: end;
    display: flex;
    flex-wrap: wrap;
    gap: 12px;
    padding-top: 16px;
  }
  /* Under the head, across the page: what was measured in the files, and where they are. */
  .album-files {
    display: grid;
    gap: 14px;
    margin-bottom: 24px;
  }
  .album-files .facts {
    margin: 0;
  }
  .file-facts {
    margin: 0;
    display: flex;
    flex-wrap: wrap;
    gap: 14px 44px;
    font-size: var(--t-sm);
  }
  .file-facts > div {
    min-width: 0;
    max-width: 36em;
  }
  .file-facts dt {
    color: var(--mist-dim);
  }
  .file-facts dd {
    margin: 2px 0 0;
    color: var(--mist);
  }
  /* The sentences were written to follow "Rip:", so they start small. */
  .file-facts dd::first-letter {
    text-transform: uppercase;
  }
  .file-facts .q-err dd {
    color: var(--alarm);
  }
  .facts.q-err {
    color: var(--alarm);
  }
  .rip-ask {
    display: flex;
    flex-wrap: wrap;
    gap: 2px 12px;
    align-items: baseline;
    max-width: 46em;
  }
  /* Where the album's files are, and a way to leave that folder out of the library. */
  .facts.folder {
    display: flex;
    flex-wrap: wrap;
    gap: 2px 12px;
    align-items: baseline;
  }
  .folder-path {
    min-width: 0;
    overflow-wrap: anywhere;
    color: var(--mist-dim);
  }
  .folder-answer {
    display: inline-flex;
    gap: 14px;
  }
  .quiet-link {
    padding: 6px 0;
    color: var(--mist);
    text-decoration: underline;
    text-decoration-color: var(--slate-500);
    text-underline-offset: 3px;
  }
  .quiet-link:hover {
    color: var(--porcelain);
  }
  .q-badge {
    flex: none;
    padding: 1px 7px;
    border-radius: 999px;
    font-size: var(--t-xs);
    font-weight: 500;
    line-height: 1.5;
    border: 1px solid currentColor;
  }
  .q-badge.ok {
    padding: 2px 5px;
    color: var(--porcelain);
    border-color: transparent;
  }
  .q-badge.suspect {
    color: var(--alarm);
  }
  .q-badge.open {
    color: var(--mist);
  }
  .q-badge:hover {
    background: var(--slate-600);
  }

  .primary {
    padding: 8px 16px;
    border-radius: var(--r-md);
    background: var(--porcelain);
    color: var(--ink);
    font-weight: 600;
  }
  .primary:hover {
    filter: brightness(1.08);
  }
  .ghost {
    padding: 8px 16px;
    border-radius: var(--r-md);
    border: 1px solid var(--slate-500);
    font-weight: 600;
  }
  .ghost:hover {
    border-color: var(--mist);
  }

  .empty {
    max-width: 52ch;
    padding: 48px 32px;
  }
  .empty-title {
    font-family: var(--font-display);
    font-weight: 600;
    font-size: var(--t-2xl);
    margin: 0 0 8px;
  }
  .empty p:not(.empty-title) {
    color: var(--mist);
    margin: 0 0 20px;
  }
  .empty-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
  }
  .empty p.empty-error {
    color: var(--alarm);
    margin: 14px 0 0;
  }
  .none {
    color: var(--mist);
    padding: 24px;
    margin: 0;
  }

  /* phone: toolbar wraps, sections scroll sideways, album header stacks */
  @media (max-width: 700px), (max-height: 500px) {
    .toolbar {
      flex-wrap: wrap;
      gap: 10px;
      padding: 10px 12px;
    }
    .seg {
      flex: 1 1 100%;
      overflow-x: auto;
      scrollbar-width: none;
    }
    .seg button {
      flex: 1;
      padding: 7px 8px;
    }
    .search {
      flex: 1 1 100%;
      max-width: none;
      font-size: 16px; /* below 16px iOS zooms the page on focus */
    }
    .folders-panel {
      position: fixed;
      left: 10px;
      right: 10px;
      top: auto;
      width: auto;
      /* Ends above the tab bar on a phone. */
      max-height: 60vh;
    }
    .browse {
      padding: 14px 12px 24px;
    }
    .browse.split {
      grid-template-columns: minmax(0, 1fr);
      gap: 12px;
    }
    .index {
      position: static;
      display: flex;
      overflow-x: auto;
      gap: 4px;
      scrollbar-width: none;
    }
    .index button {
      white-space: nowrap;
    }
    .grid {
      grid-template-columns: repeat(2, minmax(0, 1fr));
      gap: 18px 12px;
    }
    .album {
      padding: 12px 12px 24px;
    }
    /* Cover and words side by side, the buttons under both in an even grid: two to a row,
       Play first, an odd last one across the whole width. */
    .album-head {
      grid-template-columns: 120px minmax(0, 1fr);
      grid-template-rows: auto;
      gap: 20px 16px;
    }
    .album-art {
      grid-row: 1;
    }
    .album-art .initial {
      font-size: 3rem;
    }
    .album-title {
      gap: 4px;
      width: 100%;
      justify-content: space-between;
    }
    .album-info h2 {
      font-size: var(--t-xl);
      line-height: 1.12;
      overflow-wrap: anywhere;
    }
    .album-heart {
      width: 36px;
      height: 36px;
      margin: -4px -6px 0 0;
    }
    .artist-link {
      font-size: var(--t-md);
      text-align: start;
    }
    .album-actions {
      grid-column: 1 / -1;
      display: grid;
      grid-template-columns: repeat(2, minmax(0, 1fr));
      gap: 10px;
      padding-top: 0;
    }
    .album-actions > :global(*),
    .album-actions :global(.atp > button) {
      width: 100%;
      min-height: 44px;
    }
    .album-actions > :global(:last-child:nth-child(odd)) {
      grid-column: 1 / -1;
    }
  }
</style>
