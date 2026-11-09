<script lang="ts">
  import { onMount, untrack } from "svelte";
  import { api, coverUrl, inTauri, isIOS, longDuration, resolution, type Album, type LibTrack, type LibraryGoto, type LibraryStatus } from "./api";
  import AlbumTile from "./AlbumTile.svelte";
  import TrackTable, { type SortKey } from "./TrackTable.svelte";

  let {
    currentPath = null,
    goto = null,
    onplay,
    onenqueue,
  }: {
    currentPath?: string | null;
    /** Show this artist or album (from a click on the now-playing track). */
    goto?: LibraryGoto | null;
    onplay: (paths: string[], start: number) => void;
    onenqueue: (paths: string[]) => void;
  } = $props();

  type View = "albums" | "artists" | "tracks" | "genres";
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
  let query = $state("");
  let openAlbum = $state<Album | null>(null);
  let artist = $state<string | null>(null);
  let genre = $state<string | null>(null);
  let foldersOpen = $state(false);
  let foldersRoot: HTMLElement;
  let body: HTMLElement;

  $effect(() => {
    save("view", view);
    save("albumSort", albumSort);
    save("trackSort", trackSort);
    save("trackAsc", trackAsc);
    save("hiResOnly", hiResOnly);
  });

  const collator = new Intl.Collator("ru", { numeric: true, sensitivity: "base" });
  const UNKNOWN = "Неизвестный исполнитель";
  /** Artist order, with untagged music always last instead of first among Cyrillic names. */
  const byArtist = (a: string, b: string) => Number(a === UNKNOWN) - Number(b === UNKNOWN) || collator.compare(a, b);
  const q = $derived(query.trim().toLowerCase());

  function plural(n: number, [one, few, many]: [string, string, string]) {
    const m10 = n % 10,
      m100 = n % 100;
    const w = m10 === 1 && m100 !== 11 ? one : m10 >= 2 && m10 <= 4 && (m100 < 12 || m100 > 14) ? few : many;
    return `${n.toLocaleString("ru-RU")} ${w}`;
  }

  // ---- derived data ---------------------------------------------------------------------------
  let addedAt = $derived.by(() => {
    const m = new Map<string, number>();
    for (const t of tracks) m.set(t.albumId, Math.max(m.get(t.albumId) ?? 0, t.mtime));
    return m;
  });

  let visibleAlbums = $derived.by(() => {
    let list = albums.filter((a) => !hiResOnly || a.hiRes);
    if (q) list = list.filter((a) => `${a.title} ${a.artist} ${a.genre ?? ""} ${a.year ?? ""}`.toLowerCase().includes(q));
    const by: Record<AlbumSort, (a: Album, b: Album) => number> = {
      artist: (a, b) => byArtist(a.artist, b.artist) || collator.compare(a.year ?? "", b.year ?? ""),
      title: (a, b) => collator.compare(a.title, b.title),
      year: (a, b) => collator.compare(b.year ?? "", a.year ?? ""),
      added: (a, b) => (addedAt.get(b.id) ?? 0) - (addedAt.get(a.id) ?? 0),
    };
    return [...list].sort(by[albumSort]);
  });

  let visibleTracks = $derived.by(() => {
    let list = tracks.filter((t) => !hiResOnly || t.hiRes);
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
  let genres = $derived(groupBy((a) => a.genre ?? "Без жанра"));

  let shelf = $derived(
    view === "artists" && artist
      ? (artists.find(([n]) => n === artist)?.[1] ?? [])
      : view === "genres" && genre
        ? (genres.find(([n]) => n === genre)?.[1] ?? [])
        : visibleAlbums,
  );

  let currentAlbumId = $derived(tracks.find((t) => t.path === currentPath)?.albumId ?? null);
  let albumTracks = $derived(openAlbum ? openAlbum.tracks.map((i) => tracks[i]).filter(Boolean) : []);

  // ---- data loading ---------------------------------------------------------------------------
  let revision = -1;
  async function refresh() {
    status = await api.libraryStatus();
    if (status.revision !== revision) {
      revision = status.revision;
      [albums, tracks] = await Promise.all([api.libraryAlbums(), api.libraryTracks()]);
      if (openAlbum) openAlbum = albums.find((a) => a.id === openAlbum!.id) ?? null;
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
    !!a && !!b && a.localeCompare(b, "ru", { sensitivity: "base" }) === 0;

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
    const picked = await open({ directory: true, multiple: false, title: "Папка с музыкой" });
    if (typeof picked === "string") status = await api.libraryAddFolder(picked);
  }

  function playAlbum(a: Album, start = 0) {
    onplay(a.tracks.map((i) => tracks[i].path), start);
  }

  function onWindowClick(e: MouseEvent) {
    if (foldersOpen && foldersRoot && !e.composedPath().includes(foldersRoot)) foldersOpen = false;
  }

  const VIEWS: [View, string][] = [
    ["albums", "Альбомы"],
    ["artists", "Исполнители"],
    ["tracks", "Треки"],
    ["genres", "Жанры"],
  ];
</script>

<svelte:window onclick={onWindowClick} />

<section class="library" aria-label="Медиатека">
  <div class="toolbar">
    <div class="seg" role="tablist" aria-label="Раздел медиатеки">
      {#each VIEWS as [v, label] (v)}
        <button role="tab" aria-selected={view === v} class:sel={view === v} onclick={() => ((view = v), (openAlbum = null))}>{label}</button>
      {/each}
    </div>

    <input class="search" type="search" placeholder="Поиск" bind:value={query} aria-label="Поиск по медиатеке" />

    {#if view !== "tracks" && !openAlbum}
      <label class="sort">
        <select bind:value={albumSort} aria-label="Сортировка альбомов">
          <option value="artist">по исполнителю</option>
          <option value="title">по названию</option>
          <option value="year">по году</option>
          <option value="added">недавно добавленные</option>
        </select>
      </label>
    {/if}

    <label class="hires"><input type="checkbox" bind:checked={hiResOnly} />Только hi-res</label>

    <div class="folders" bind:this={foldersRoot}>
      <button
        class="folders-btn"
        onclick={() => (foldersOpen = !foldersOpen)}
        aria-expanded={foldersOpen}
        title={status ? `${plural(status.tracks, ["трек", "трека", "треков"])} в ${plural(status.folders.length, ["папке", "папках", "папках"])}. Нажмите, чтобы изменить папки.` : ""}
      >
        {#if status?.scanning}
          Сканирую {status.scanning[0].toLocaleString("ru-RU")} из {status.scanning[1].toLocaleString("ru-RU")}
        {:else if status}
          {plural(status.albums, ["альбом", "альбома", "альбомов"])}
        {/if}
      </button>
      {#if foldersOpen && status}
        <div class="folders-panel" role="dialog" aria-label="Папки медиатеки">
          {#if isIOS}
          <p class="fp-title">Музыка на iPhone</p>
          <p class="fp-empty">
            Медиатека — это папка приложения: Файлы → На iPhone → Hifi. Скопируйте туда альбомы через
            «Файлы» или Finder на Mac (iPhone → Файлы), либо добавьте файлы здесь.
          </p>
          <div class="fp-actions">
            <button class="primary" onclick={importFiles}>Добавить файлы</button>
            <button onclick={async () => (status = await api.libraryRescan())} disabled={!!status.scanning}>Пересканировать</button>
          </div>
          {:else}
          <p class="fp-title">Папки с музыкой</p>
          {#if status.folders.length}
            <ul>
              {#each status.folders as f (f)}
                <li>
                  <span title={f}>{f}</span>
                  <button onclick={async () => (status = await api.libraryRemoveFolder(f))} aria-label={`Убрать папку ${f}`}>Убрать</button>
                </li>
              {/each}
            </ul>
          {:else}
            <p class="fp-empty">Папок пока нет.</p>
          {/if}
          <div class="fp-actions">
            <button class="primary" onclick={addFolder}>Добавить папку</button>
            <button onclick={async () => (status = await api.libraryRescan())} disabled={!!status.scanning}>Пересканировать</button>
          </div>
          {/if}
          {#if status.failed}
            <p class="fp-failed">{plural(status.failed, ["файл не прочитан", "файла не прочитаны", "файлов не прочитаны"])} — повреждены или формат не поддерживается.</p>
          {/if}
        </div>
      {/if}
    </div>
  </div>

  <div class="body" bind:this={body}>
    {#if isIOS && status && status.tracks === 0 && !status.scanning}
      <div class="empty">
        <p class="empty-title">Медиатека пуста</p>
        <p>
          Скопируйте альбомы в Файлы → На iPhone → Hifi (или с Mac через Finder: iPhone → Файлы → Hifi) —
          медиатека найдёт их сама. Или добавьте файлы отсюда.
        </p>
        <button class="primary" onclick={importFiles}>Добавить файлы</button>
      </div>
    {:else if status && status.folders.length === 0}
      <div class="empty">
        <p class="empty-title">Медиатека пуста</p>
        <p>Укажите папку с музыкой — например, Музыка в домашней папке или внешний диск. Файлы останутся на месте: приложение только читает теги и обложки.</p>
        <button class="primary" onclick={addFolder}>Выбрать папку</button>
      </div>
    {:else if openAlbum}
      <div class="album">
        <button class="back" onclick={() => (openAlbum = null)}>
          <svg width="8" height="12" viewBox="0 0 8 12" aria-hidden="true"><path d="M7 1L2 6l5 5" fill="none" stroke="currentColor" stroke-width="1.6" /></svg>
          Назад
        </button>
        <div class="album-head">
          <div class="album-art">
            {#if openAlbum.hasCover}
              <img src={coverUrl(openAlbum.id)} alt={`Обложка альбома ${openAlbum.title}`} />
            {:else}
              <span class="initial" aria-hidden="true">{openAlbum.title.slice(0, 1)}</span>
            {/if}
          </div>
          <div class="album-info">
            <h2>{openAlbum.title}</h2>
            <button class="artist-link" onclick={() => ((artist = openAlbum!.artist), (view = "artists"), (openAlbum = null))}>{openAlbum.artist}</button>
            <p class="facts">
              {[openAlbum.year, openAlbum.genre, plural(openAlbum.trackCount, ["трек", "трека", "треков"]), longDuration(openAlbum.duration)].filter(Boolean).join(", ")}
            </p>
            <p class="facts fmt" class:hi={openAlbum.hiRes}>
              {openAlbum.format}{openAlbum.maxBits ? `, ${resolution(openAlbum.maxBits, openAlbum.maxRate)}${openAlbum.maxBits === 1 ? "" : " кГц"}` : ""}{openAlbum.hiRes ? ", hi-res" : ""}
            </p>
            <div class="album-actions">
              <button class="primary" onclick={() => playAlbum(openAlbum!)}>Играть</button>
              <button class="ghost" onclick={() => onenqueue(openAlbum!.tracks.map((i) => tracks[i].path))}>В очередь</button>
            </div>
          </div>
        </div>
        <TrackTable
          tracks={albumTracks}
          {currentPath}
          sortable={false}
          columns={["number", "title", "duration", "format"]}
          onplay={(_, i) => playAlbum(openAlbum!, i)}
        />
      </div>
    {:else if view === "tracks"}
      {#if visibleTracks.length}
        <TrackTable
          tracks={visibleTracks}
          {currentPath}
          bind:sortKey={trackSort}
          bind:sortAsc={trackAsc}
          onplay={(list, i) => onplay(list.map((t) => t.path), i)}
        />
      {:else}
        <p class="none">Ничего не найдено{q ? ` по запросу «${query.trim()}»` : ""}.</p>
      {/if}
    {:else}
      <div class="browse" class:split={view === "artists" || view === "genres"}>
        {#if view === "artists" || view === "genres"}
          {@const groups = view === "artists" ? artists : genres}
          {@const picked = view === "artists" ? artist : genre}
          <ul class="index" aria-label={view === "artists" ? "Исполнители" : "Жанры"}>
            <li>
              <button class:sel={!picked} onclick={() => (view === "artists" ? (artist = null) : (genre = null))}>
                {view === "artists" ? "Все исполнители" : "Все жанры"}
                <span class="count num">{visibleAlbums.length}</span>
              </button>
            </li>
            {#each groups as [name, list] (name)}
              <li>
                <button class:sel={picked === name} onclick={() => (view === "artists" ? (artist = name) : (genre = name))}>
                  {name}<span class="count num">{list.length}</span>
                </button>
              </li>
            {/each}
          </ul>
        {/if}
        {#if shelf.length}
          <div class="grid">
            {#each shelf as a (a.id)}
              <AlbumTile album={a} playing={a.id === currentAlbumId} onopen={(x) => (openAlbum = x)} />
            {/each}
          </div>
        {:else}
          <p class="none">Ничего не найдено{q ? ` по запросу «${query.trim()}»` : ""}.</p>
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
    border-radius: 6px;
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
    outline: 2px solid var(--brass);
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
  .sort select {
    font: inherit;
    color: var(--porcelain);
    background: var(--slate-900);
    border: 1px solid var(--slate-600);
    border-radius: 6px;
    padding: 4px 6px;
  }
  .hires input {
    accent-color: var(--brass);
  }

  .folders {
    position: relative;
    margin-left: auto;
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
    right: 0;
    top: calc(100% + 6px);
    width: 380px;
    z-index: 20;
    background: var(--slate-700);
    border: 1px solid var(--slate-500);
    border-radius: 12px;
    padding: 14px 16px;
    box-shadow: 0 18px 40px rgba(5, 12, 15, 0.55);
  }
  .fp-title {
    margin: 0 0 8px;
    font-weight: 600;
  }
  .folders-panel ul {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .folders-panel li {
    display: flex;
    justify-content: space-between;
    gap: 12px;
    padding: 6px 0;
    font-size: var(--t-sm);
    border-bottom: 1px solid var(--slate-600);
  }
  .folders-panel li span {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    direction: rtl;
    text-align: left;
  }
  .folders-panel li button {
    color: var(--mist);
    flex: none;
  }
  .folders-panel li button:hover {
    color: var(--alarm);
  }
  .fp-empty,
  .fp-failed {
    font-size: var(--t-sm);
    color: var(--mist);
  }
  .fp-actions {
    display: flex;
    gap: 14px;
    align-items: center;
    margin-top: 12px;
    font-size: var(--t-sm);
  }
  .fp-actions button:not(.primary) {
    color: var(--mist);
  }
  .fp-actions button:not(.primary):hover:not(:disabled) {
    color: var(--porcelain);
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
    text-align: left;
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
  .album-head {
    display: grid;
    grid-template-columns: 200px 1fr;
    gap: 28px;
    align-items: end;
    margin-bottom: 24px;
  }
  .album-art {
    aspect-ratio: 1;
    border-radius: 6px;
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
  .album-info h2 {
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
    color: var(--brass);
  }
  .facts {
    margin: 6px 0 0;
    color: var(--mist);
    font-size: var(--t-sm);
  }
  .facts.fmt.hi {
    color: var(--brass);
  }
  .album-actions {
    display: flex;
    gap: 12px;
    margin-top: 16px;
  }

  .primary {
    padding: 8px 16px;
    border-radius: var(--r-md);
    background: var(--brass);
    color: var(--slate-900);
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
  .none {
    color: var(--mist);
    padding: 24px;
    margin: 0;
  }

  /* phone: toolbar wraps, sections scroll sideways, album header stacks */
  @media (max-width: 700px) {
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
    .album-head {
      grid-template-columns: 112px minmax(0, 1fr);
      gap: 16px;
    }
  }
</style>
