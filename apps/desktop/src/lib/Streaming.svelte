<script lang="ts">
  import { onMount, tick, untrack } from "svelte";
  import {
    api, entryFromStream, errorText, longDuration, resolution, SERVICE_NAME, streamAlbumToLib, streamToLib,
    type SAlbum, type Service, type SPlaylist, type STrack, type StreamQuality, type StreamingStatus,
  } from "./api";
  import { remember } from "./labels.svelte";
  import { list, sentences, t, tn } from "./i18n.svelte";
  import AddToPlaylist from "./AddToPlaylist.svelte";
  import AlbumTile from "./AlbumTile.svelte";
  import TrackTable from "./TrackTable.svelte";

  let {
    status,
    currentPath = null,
    onstatus,
    onplay,
    onenqueue,
  }: {
    status: StreamingStatus;
    currentPath?: string | null;
    /** Sign-in state changed: fetch it again. */
    onstatus: () => void;
    onplay: (paths: string[], start: number) => void;
    onenqueue: (paths: string[]) => void;
  } = $props();

  type Tab = "search" | "favorites" | "playlists";
  function load<T>(key: string, fallback: T): T {
    try {
      const v = localStorage.getItem(`stream.${key}`);
      return v === null ? fallback : (JSON.parse(v) as T);
    } catch {
      return fallback;
    }
  }
  function save(key: string, v: unknown) {
    try {
      localStorage.setItem(`stream.${key}`, JSON.stringify(v));
    } catch {}
  }

  let service = $state<Service>(load("service", "qobuz"));
  let tab = $state<Tab>(load("tab", "search"));
  let query = $state("");
  let results = $state<{ albums: SAlbum[]; tracks: STrack[] } | null>(null);
  let favorites = $state<SAlbum[] | null>(null);
  let lists = $state<SPlaylist[] | null>(null);
  let open = $state<{ kind: "album"; album: SAlbum; tracks: STrack[] } | { kind: "playlist"; playlist: SPlaylist; tracks: STrack[] } | null>(null);
  let loading = $state(false);
  let error = $state<string | null>(null);
  let note = $state<string | null>(null);
  let signingIn = $state(false);
  let body: HTMLElement;

  $effect(() => {
    save("service", service);
    save("tab", tab);
  });

  const account = $derived(status.accounts.find((a) => a.service === service));
  const signedIn = $derived(!!account?.signedIn);

  // A different service or account: what was shown belongs to the old one.
  let shownFor = "";
  $effect(() => {
    const key = `${service}|${signedIn}`;
    if (key === shownFor) return;
    shownFor = key;
    untrack(() => {
      results = null;
      favorites = null;
      lists = null;
      open = null;
      error = null;
      if (signedIn) loadTab();
    });
  });

  async function run<T>(f: () => Promise<T>): Promise<T | undefined> {
    loading = true;
    error = null;
    try {
      return await f();
    } catch (e) {
      error = errorText(e);
    } finally {
      loading = false;
    }
  }

  async function loadTab() {
    if (tab === "favorites" && !favorites) favorites = (await run(() => api.streamingFavorites(service))) ?? null;
    if (tab === "playlists" && !lists) lists = (await run(() => api.streamingPlaylists(service))) ?? null;
  }

  async function search() {
    const q = query.trim();
    if (!q) return;
    open = null;
    results = (await run(() => api.streamingSearch(service, q))) ?? null;
  }

  /** Where the grid was scrolled when an album or playlist opened: "Back" returns there. */
  let gridScroll = 0;
  function keepGridScroll() {
    if (!open) gridScroll = body?.scrollTop ?? 0;
  }
  async function back() {
    open = null;
    await tick();
    body?.scrollTo({ top: gridScroll });
  }

  async function openAlbum(a: SAlbum) {
    const page = await run(() => api.streamingAlbum(service, a.id));
    if (page) {
      keepGridScroll();
      open = { kind: "album", album: page.album, tracks: page.tracks };
      body?.scrollTo({ top: 0 });
    }
  }

  async function openPlaylist(p: SPlaylist) {
    const page = await run(() => api.streamingPlaylist(service, p.id));
    if (page) {
      keepGridScroll();
      open = { kind: "playlist", playlist: { ...p, ...page.playlist, name: page.playlist.name || p.name }, tracks: page.tracks };
      body?.scrollTo({ top: 0 });
    }
  }

  function play(list: STrack[], i: number) {
    remember(list);
    onplay(list.map((t) => t.uri), i);
  }

  function enqueue(list: STrack[]) {
    remember(list);
    onenqueue(list.map((t) => t.uri));
  }

  async function signIn() {
    signingIn = true;
    error = null;
    try {
      await api.streamingLogin(service);
      onstatus();
    } catch (e) {
      if (!(e && typeof e === "object" && "code" in e && e.code === "signInCancelled")) error = errorText(e);
    } finally {
      signingIn = false;
    }
  }

  async function signOut() {
    await api.streamingLogout(service);
    onstatus();
  }

  async function setQuality(q: StreamQuality) {
    await api.streamingSetQuality(q);
    onstatus();
  }

  async function importPlaylist(p: SPlaylist) {
    const made = await run(() => api.streamingImportPlaylist(service, p.id));
    if (made) note = tn("stream.savedCopy", made.total, { name: made.name });
  }

  function fmt(a: { bits: number | null; rate: number | null; hiRes: boolean }) {
    if (a.bits && a.rate) return `FLAC ${resolution(a.bits, a.rate)} ${t("unit.khz")}`;
    return a.hiRes ? t("stream.hiresUpTo") : `FLAC ${resolution(16, 44100)} ${t("unit.khz")}`;
  }

  onMount(() => {
    if (signedIn) loadTab();
  });

  const TABS: Tab[] = ["search", "favorites", "playlists"];
</script>

<section class="streaming" aria-label={t("nav.streaming")}>
  <div class="toolbar">
    <div class="seg" role="tablist" aria-label={t("stream.service")}>
      {#each ["qobuz", "tidal"] as const as s (s)}
        <button role="tab" aria-selected={service === s} class:sel={service === s} onclick={() => (service = s)}>
          {SERVICE_NAME[s]}{#if status.accounts.find((a) => a.service === s)?.signedIn}<i class="dot" aria-label={t("stream.signedIn")}></i>{/if}
        </button>
      {/each}
    </div>
    {#if signedIn}
      <div class="seg" role="tablist" aria-label={t("stream.section")}>
        {#each TABS as tb (tb)}
          <button role="tab" aria-selected={tab === tb && !open} class:sel={tab === tb && !open} onclick={() => ((tab = tb), (open = null), loadTab())}>{t(`stream.${tb}`)}</button>
        {/each}
      </div>
      {#if tab === "search"}
        <form class="search-form" onsubmit={(e) => (e.preventDefault(), (tab = "search"), search())}>
          <input class="search" type="search" placeholder={t("stream.searchIn", { service: SERVICE_NAME[service] })} bind:value={query} aria-label={t("stream.searchIn", { service: SERVICE_NAME[service] })} />
        </form>
      {/if}
      <div class="account">
        <label class="quality">
          <select value={status.quality} onchange={(e) => setQuality((e.currentTarget as HTMLSelectElement).value as StreamQuality)} aria-label={t("stream.quality")}>
            <option value="max">{t("stream.q.max")}</option>
            <option value="hires96">{t("stream.q.hires96")}</option>
            <option value="cd">{t("stream.q.cd")}</option>
          </select>
        </label>
        <span class="user" title={account?.user ?? ""}>{account?.user ?? SERVICE_NAME[service]}</span>
        <button class="link" onclick={signOut}>{t("stream.signOut")}</button>
      </div>
    {/if}
  </div>

  <div class="body" bind:this={body}>
    {#if error}<p class="err" role="alert">{error}</p>{/if}
    {#if note}<p class="note" role="status">{note} <button class="link" onclick={() => (note = null)}>{t("pl.hide")}</button></p>{/if}
    {#if account?.note}<p class="err">{t(`stream.note.${account.note}`)}</p>{/if}

    {#if !signedIn}
      <div class="signin">
        <p class="si-title">{SERVICE_NAME[service]}</p>
        <p>{sentences([t("stream.signinText", { service: SERVICE_NAME[service] }), service === "qobuz" ? t("stream.needPlanQobuz") : t("stream.needPlan")])}</p>
        <p class="fine">{t(service === "qobuz" ? "stream.unofficialWeb" : "stream.unofficialAndroid", { service: SERVICE_NAME[service] })}</p>
        <button class="primary" onclick={signIn} disabled={signingIn}>{signingIn ? t("stream.waiting") : t("stream.signIn", { service: SERVICE_NAME[service] })}</button>
      </div>
    {:else if open?.kind === "album"}
      {@const a = open.album}
      <div class="page">
        <button class="back" onclick={back}>
          <svg width="8" height="12" viewBox="0 0 8 12" aria-hidden="true"><path d="M7 1L2 6l5 5" fill="none" stroke="currentColor" stroke-width="1.6" /></svg>
          {t("lib.back")}
        </button>
        <div class="head">
          <div class="art">{#if a.cover}<img src={a.cover} alt={t("common.cover", { album: a.title })} />{/if}</div>
          <div class="info">
            <h2>{a.title}</h2>
            <p class="by">{a.artist}</p>
            <p class="facts">{list([a.year, a.genre, tn("lib.nTracks", open.tracks.length), a.duration > 0 && longDuration(a.duration)])}</p>
            <p class="facts" class:hi={a.hiRes}>{fmt(a)}</p>
            <div class="actions">
              <button class="primary" onclick={() => play(open!.tracks, 0)} disabled={!open.tracks.length}>{t("common.play")}</button>
              <button class="ghost" onclick={() => enqueue(open!.tracks)}>{t("lib.enqueue")}</button>
              <AddToPlaylist entries={() => open!.tracks.map(entryFromStream)} />
            </div>
          </div>
        </div>
        <TrackTable tracks={open.tracks.map(streamToLib)} {currentPath} sortable={false} columns={["number", "title", "duration", "format"]} onplay={(_, i) => play(open!.tracks, i)}>
          {#snippet actions(_, i)}<AddToPlaylist compact entries={() => [entryFromStream(open!.tracks[i])]} />{/snippet}
        </TrackTable>
      </div>
    {:else if open?.kind === "playlist"}
      {@const p = open.playlist}
      <div class="page">
        <button class="back" onclick={back}>
          <svg width="8" height="12" viewBox="0 0 8 12" aria-hidden="true"><path d="M7 1L2 6l5 5" fill="none" stroke="currentColor" stroke-width="1.6" /></svg>
          {t("lib.back")}
        </button>
        <div class="head">
          <div class="art">{#if p.cover}<img src={p.cover} alt="" />{/if}</div>
          <div class="info">
            <h2>{p.name}</h2>
            {#if p.owner}<p class="by">{p.owner}</p>{/if}
            <p class="facts">{tn("lib.nTracks", open.tracks.length)}</p>
            <div class="actions">
              <button class="primary" onclick={() => play(open!.tracks, 0)} disabled={!open.tracks.length}>{t("common.play")}</button>
              <button class="ghost" onclick={() => enqueue(open!.tracks)}>{t("lib.enqueue")}</button>
              <button class="ghost" onclick={() => importPlaylist(p)} title={t("stream.saveToMineTitle")}>{t("stream.saveToMine")}</button>
            </div>
          </div>
        </div>
        <TrackTable tracks={open.tracks.map(streamToLib)} {currentPath} sortable={false} columns={["number", "title", "artist", "album", "duration"]} onplay={(_, i) => play(open!.tracks, i)}>
          {#snippet actions(_, i)}<AddToPlaylist compact entries={() => [entryFromStream(open!.tracks[i])]} />{/snippet}
        </TrackTable>
      </div>
    {:else if tab === "search"}
      {#if results}
        {#if results.albums.length}
          <h3>{t("lib.albums")}</h3>
          <div class="grid">
            {#each results.albums as a (a.id)}
              <AlbumTile album={streamAlbumToLib(a)} src={a.cover} onopen={() => openAlbum(a)} />
            {/each}
          </div>
        {/if}
        {#if results.tracks.length}
          <h3>{t("lib.tracks")}</h3>
          <TrackTable tracks={results.tracks.map(streamToLib)} {currentPath} sortable={false} columns={["title", "artist", "album", "duration", "format"]} onplay={(_, i) => play(results!.tracks, i)}>
            {#snippet actions(_, i)}<AddToPlaylist compact entries={() => [entryFromStream(results!.tracks[i])]} />{/snippet}
          </TrackTable>
        {/if}
        {#if !results.albums.length && !results.tracks.length}<p class="none">{t("lib.nothing")}</p>{/if}
      {:else if !loading}
        <p class="none">{t("stream.searchHint", { service: SERVICE_NAME[service] })}</p>
      {/if}
    {:else if tab === "favorites"}
      {#if favorites?.length}
        <div class="grid">
          {#each favorites as a (a.id)}
            <AlbumTile album={streamAlbumToLib(a)} src={a.cover} onopen={() => openAlbum(a)} />
          {/each}
        </div>
      {:else if favorites && !loading}
        <p class="none">{t("stream.noFavorites", { service: SERVICE_NAME[service] })}</p>
      {/if}
    {:else if tab === "playlists"}
      {#if lists?.length}
        <div class="grid">
          {#each lists as p (p.id)}
            <button class="pl" onclick={() => openPlaylist(p)}>
              <span class="pl-art">{#if p.cover}<img src={p.cover} alt="" loading="lazy" />{:else}<span aria-hidden="true">{p.name.slice(0, 1)}</span>{/if}</span>
              <span class="pl-name">{p.name}</span>
              <span class="pl-n">{tn("lib.nTracks", p.trackCount)}</span>
            </button>
          {/each}
        </div>
      {:else if lists && !loading}
        <p class="none">{t("stream.noPlaylists", { service: SERVICE_NAME[service] })}</p>
      {/if}
    {/if}
    {#if loading}<p class="none" aria-live="polite">{t("stream.loading")}</p>{/if}
  </div>
</section>

<style>
  .streaming {
    display: flex;
    flex-direction: column;
    min-height: 0;
    min-width: 0;
    flex: 1;
  }
  .toolbar {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 14px 24px;
    border-bottom: 1px solid var(--slate-600);
    flex-wrap: wrap;
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
    display: inline-flex;
    align-items: center;
    gap: 6px;
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
  .dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--porcelain);
  }
  .search-form {
    flex: 1;
    min-width: 140px;
    max-width: 320px;
  }
  .search {
    width: 100%;
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
  .account {
    margin-inline-start: auto;
    display: flex;
    align-items: center;
    gap: 12px;
    font-size: var(--t-sm);
    color: var(--mist);
    min-width: 0;
  }
  .user {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 180px;
  }
  .quality select {
    font: inherit;
    color: var(--porcelain);
    background: var(--slate-900);
    border: 1px solid var(--slate-600);
    border-radius: var(--r-md);
    padding: 4px 6px;
  }
  .link {
    color: var(--mist);
    font-size: var(--t-sm);
  }
  .link:hover {
    color: var(--porcelain);
  }
  .body {
    flex: 1;
    min-height: 0;
    overflow: auto;
    padding: 22px 24px 32px;
  }
  h3 {
    margin: 0 0 14px;
    font-size: var(--t-md);
    font-weight: 600;
  }
  h3:not(:first-child) {
    margin-top: 28px;
  }
  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(156px, 1fr));
    gap: 24px 20px;
  }
  .signin {
    max-width: 58ch;
    padding: 24px 8px;
  }
  .si-title {
    font-family: var(--font-display);
    font-weight: 600;
    font-size: var(--t-2xl);
    margin: 0 0 10px;
  }
  .signin p:not(.si-title) {
    color: var(--mist);
    margin: 0 0 14px;
  }
  .signin .fine {
    font-size: var(--t-sm);
    color: var(--mist-dim);
  }
  .primary {
    padding: 8px 16px;
    border-radius: var(--r-md);
    background: var(--porcelain);
    color: var(--ink);
    font-weight: 600;
  }
  .primary:hover:not(:disabled) {
    filter: brightness(1.08);
  }
  .primary:disabled {
    opacity: 0.6;
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
  .head {
    display: grid;
    grid-template-columns: 200px 1fr;
    gap: 28px;
    align-items: end;
    margin-bottom: 24px;
  }
  .art {
    aspect-ratio: 1;
    border-radius: var(--r-md);
    overflow: hidden;
    background:
      repeating-linear-gradient(90deg, rgba(232, 237, 234, 0.05) 0 2px, transparent 2px 8px),
      var(--slate-700);
    box-shadow: 0 14px 30px rgba(5, 12, 15, 0.5);
  }
  .art img {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }
  .info h2 {
    margin: 0;
    font-family: var(--font-display);
    font-weight: 600;
    font-size: var(--t-2xl);
    line-height: 1.05;
  }
  .by {
    margin: 4px 0 0;
    font-size: var(--t-lg);
    font-weight: 500;
  }
  .facts {
    margin: 6px 0 0;
    color: var(--mist);
    font-size: var(--t-sm);
  }
  .facts.hi {
    color: var(--porcelain);
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 12px;
    margin-top: 16px;
  }
  .pl {
    display: flex;
    flex-direction: column;
    gap: 2px;
    text-align: start;
    min-width: 0;
  }
  .pl-art {
    width: 100%;
    aspect-ratio: 1;
    border-radius: var(--r-md);
    overflow: hidden;
    margin-bottom: 8px;
    display: grid;
    place-items: center;
    font-family: var(--font-display);
    font-size: 3rem;
    color: var(--slate-500);
    background:
      repeating-linear-gradient(90deg, rgba(232, 237, 234, 0.05) 0 2px, transparent 2px 8px),
      var(--slate-700);
  }
  .pl-art img {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }
  .pl-name {
    font-weight: 600;
    font-size: var(--t-sm);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .pl-n {
    font-size: var(--t-xs);
    color: var(--mist-dim);
  }
  .none {
    color: var(--mist);
    margin: 0;
  }
  .err,
  .note {
    margin: 0 0 16px;
    padding: 10px 14px;
    border-radius: var(--r-md);
    font-size: var(--t-sm);
    background: var(--slate-700);
  }
  .err {
    color: var(--alarm);
  }
  .note {
    color: var(--porcelain);
  }

  @media (max-width: 700px), (max-height: 500px) {
    .toolbar {
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
      justify-content: center;
      padding: 7px 8px;
    }
    .search-form {
      flex: 1 1 100%;
      max-width: none;
    }
    .search {
      font-size: 16px;
    }
    .account {
      margin-inline-start: 0;
      flex: 1 1 100%;
    }
    .body {
      padding: 14px 12px 24px;
    }
    .grid {
      grid-template-columns: repeat(2, minmax(0, 1fr));
      gap: 18px 12px;
    }
    .head {
      grid-template-columns: 112px minmax(0, 1fr);
      gap: 16px;
    }
  }
</style>
