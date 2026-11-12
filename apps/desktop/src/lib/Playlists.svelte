<script lang="ts">
  import { onMount } from "svelte";
  import {
    api, errorText, inTauri, isAndroid, isIOS, longDuration, serviceOf, SERVICE_NAME,
    type Imported, type LibTrack, type PlaylistDetail, type PlaylistEntry, type PlaylistSummary, type Service, type SmartRules, type StreamingStatus,
  } from "./api";
  import { remember } from "./labels.svelte";
  import { list, sentences, t, tn } from "./i18n.svelte";
  import TrackTable from "./TrackTable.svelte";
  import SmartEditor from "./SmartEditor.svelte";

  let {
    tracks,
    currentPath = null,
    query = "",
    streaming = null,
    onplay,
    onenqueue,
  }: {
    /** The library, to show each file's format. */
    tracks: LibTrack[];
    currentPath?: string | null;
    query?: string;
    streaming?: StreamingStatus | null;
    onplay: (paths: string[], start: number) => void;
    onenqueue: (paths: string[]) => void;
  } = $props();

  let lists = $state<PlaylistSummary[]>([]);
  let selected = $state<string | null>(null);
  let detail = $state<PlaylistDetail | null>(null);
  let report = $state<Imported[] | null>(null);
  let error = $state<string | null>(null);
  let note = $state<string | null>(null);
  let renaming = $state(false);
  let newName = $state("");
  let pasting = $state(false);
  let pasteText = $state("");
  let pasteName = $state("");
  let exportOpen = $state(false);
  let busy = $state(false);
  /** The smart playlist being set up (id null: a new one). */
  let smartEdit = $state<{ id: string | null; name: string; rules: SmartRules | null } | null>(null);

  const phone = isIOS || isAndroid;
  const byPath = $derived(new Map(tracks.map((t) => [t.path, t])));

  async function refresh(keep = selected) {
    lists = await api.playlists();
    selected = keep && lists.some((l) => l.id === keep) ? keep : (lists[0]?.id ?? null);
    detail = selected ? await api.playlist(selected) : null;
  }

  onMount(() => {
    refresh();
  });

  async function select(id: string) {
    selected = id;
    renaming = false;
    exportOpen = false;
    detail = await api.playlist(id);
  }

  // ---- the entries, as the track table shows them ------------------------------------------------
  type Row = LibTrack & { entry: number; available: boolean };
  const rows = $derived.by((): Row[] => {
    if (!detail) return [];
    const q = query.trim().toLowerCase();
    return detail.entries
      .map((e, i): Row => {
        const lib = e.uri ? byPath.get(e.uri) : undefined;
        const svc = serviceOf(e.uri);
        const base: LibTrack = lib ?? {
          path: e.uri ?? `missing:${i}`, title: e.title, artist: e.artist, albumArtist: null, album: e.album, genre: null, year: null,
          trackNumber: null, discNumber: null, duration: e.duration ?? 0, sampleRate: 0, bits: null, channels: 2,
          format: svc ? SERVICE_NAME[svc] : e.uri ? t("pl.file") : t("pl.missing"), lossless: !!svc, hasCover: false, size: 0, mtime: 0, albumId: "", hiRes: false,
        };
        return { ...base, title: e.title, artist: e.artist, album: e.album, trackNumber: i + 1, entry: i, available: e.available };
      })
      .filter((r) => !q || `${r.title} ${r.artist ?? ""} ${r.album ?? ""}`.toLowerCase().includes(q));
  });
  const missing = $derived(detail ? detail.entries.filter((e) => !e.available).length : 0);
  const total = $derived(detail ? detail.entries.reduce((s, e) => s + (e.duration ?? 0), 0) : 0);
  const signedIn = $derived((streaming?.accounts ?? []).filter((a) => a.signedIn).map((a) => a.service));

  /** Only what can play now; the queue skips nothing it is given. */
  function playable(from = 0): { paths: string[]; start: number } {
    if (!detail) return { paths: [], start: 0 };
    const ok = detail.entries.map((e, i) => ({ e, i })).filter(({ e }) => e.available && e.uri);
    remember(ok.map(({ e }) => e));
    return { paths: ok.map(({ e }) => e.uri!), start: Math.max(0, ok.findIndex(({ i }) => i >= from)) };
  }

  function play(from = 0) {
    const { paths, start } = playable(from);
    if (paths.length) onplay(paths, start);
  }

  // ---- editing --------------------------------------------------------------------------------------
  async function save(entries: PlaylistEntry[]) {
    if (!detail) return;
    await api.playlistUpdate(detail.id, null, entries);
    await refresh(detail.id);
  }

  function strip(e: PlaylistDetail["entries"][number]): PlaylistEntry {
    const { available: _, ...rest } = e;
    return rest;
  }

  async function move(i: number, by: number) {
    if (!detail) return;
    const list = detail.entries.map(strip);
    const j = i + by;
    if (j < 0 || j >= list.length) return;
    [list[i], list[j]] = [list[j], list[i]];
    await save(list);
  }

  async function removeEntry(i: number) {
    if (!detail) return;
    await save(detail.entries.map(strip).filter((_, k) => k !== i));
  }

  async function create() {
    const s = await api.playlistCreate(t("pl.defaultName"), []);
    await refresh(s.id);
    newName = s.name;
    renaming = true;
  }

  async function saveSmart(name: string, rules: SmartRules) {
    const editing = smartEdit;
    smartEdit = null;
    if (!editing) return;
    if (editing.id) {
      await api.playlistUpdate(editing.id, name, null, rules);
      await refresh(editing.id);
    } else {
      const s = await api.playlistCreate(name, [], rules);
      await refresh(s.id);
    }
  }

  async function rename() {
    if (!detail || !newName.trim()) return (renaming = false);
    await api.playlistUpdate(detail.id, newName.trim(), null);
    renaming = false;
    await refresh(detail.id);
  }

  let confirmDelete = $state(false);
  async function remove() {
    if (!detail) return;
    if (!confirmDelete) {
      confirmDelete = true;
      setTimeout(() => (confirmDelete = false), 4000);
      return;
    }
    confirmDelete = false;
    await api.playlistDelete(detail.id);
    await refresh(null);
  }

  // ---- import / export ------------------------------------------------------------------------------
  async function afterImport(made: Imported[]) {
    report = made;
    error = null;
    await refresh(made[0]?.id ?? selected);
  }

  async function importFile() {
    if (!inTauri) return afterImport(await api.playlistImportFile("/Users/me/Downloads/Liked Songs.csv"));
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({
      multiple: true,
      ...(isIOS ? {} : { filters: [{ name: t("lib.playlists"), extensions: ["m3u", "m3u8", "pls", "xspf", "wpl", "zpl", "xml", "csv", "tsv", "txt"] }] }),
    });
    if (!picked) return;
    busy = true;
    try {
      const made: Imported[] = [];
      for (const p of Array.isArray(picked) ? picked : [picked]) made.push(...(await api.playlistImportFile(p)));
      await afterImport(made);
    } catch (e) {
      error = errorText(e);
    } finally {
      busy = false;
    }
  }

  async function importText() {
    busy = true;
    try {
      await afterImport(await api.playlistImportText(pasteText, pasteName.trim() || t("pl.importedName")));
      pasting = false;
      pasteText = "";
      pasteName = "";
    } catch (e) {
      error = errorText(e);
    } finally {
      busy = false;
    }
  }

  const FORMATS = ["m3u8", "xspf", "pls", "csv"];

  async function exportAs(ext: string) {
    if (!detail) return;
    exportOpen = false;
    try {
      if (phone || !inTauri) {
        const where = await api.playlistExport(detail.id, null, ext);
        note = isIOS ? t("pl.savedFiles", { name: where.split("/").pop() ?? where }) : t("pl.saved", { path: where });
        return;
      }
      const { save } = await import("@tauri-apps/plugin-dialog");
      const path = await save({ defaultPath: `${detail.name}.${ext}`, filters: [{ name: ext.toUpperCase(), extensions: [ext] }] });
      if (!path) return;
      await api.playlistExport(detail.id, path);
      note = t("pl.saved", { path });
    } catch (e) {
      error = errorText(e);
    }
  }

  async function findMissing(service: Service) {
    if (!detail) return;
    busy = true;
    try {
      const n = await api.streamingFindMissing(detail.id, service);
      note = n ? tn("pl.foundOn", n, { service: SERVICE_NAME[service], missing }) : t("pl.foundNone", { service: SERVICE_NAME[service] });
      await refresh(detail.id);
    } catch (e) {
      error = errorText(e);
    } finally {
      busy = false;
    }
  }
</script>

<div class="playlists">
  <aside class="side" aria-label={t("lib.playlists")}>
    <div class="side-actions">
      <button class="small" onclick={create}>{t("pl.new")}</button>
      <button class="small" onclick={() => (smartEdit = { id: null, name: t("sp.defaultName"), rules: null })}>{t("sp.new")}</button>
      {#if !isAndroid}<button class="small" onclick={importFile} disabled={busy}>{t("pl.import")}</button>{/if}
      <button class="small" onclick={() => (pasting = !pasting)} aria-expanded={pasting}>{t("pl.paste")}</button>
    </div>
    {#if lists.length}
      <ul class="index">
        {#each lists as p (p.id)}
          <li>
            <button class:sel={p.id === selected} onclick={() => select(p.id)}>
              <span class="pname">{#if p.smart}<span class="smart" title={t("sp.title")} aria-label={t("sp.title")}>✦</span>{/if}{p.name}</span><span class="count num">{p.count}</span>
            </button>
          </li>
        {/each}
      </ul>
    {/if}
  </aside>

  <section class="main">
    {#if pasting}
      <form class="paste" onsubmit={(e) => (e.preventDefault(), importText())}>
        <!-- Links to Qobuz or TIDAL only where this build has streaming (never in App Store builds). -->
        <p class="hint">{t(streaming?.available ? "pl.pasteHint" : "pl.pasteHintLocal")}</p>
        <textarea
          bind:value={pasteText}
          rows="8"
          placeholder={streaming?.available ? "Miles Davis — So What\nhttps://open.qobuz.com/track/19512574" : "Miles Davis — So What\nJohn Coltrane — Naima"}
          aria-label={t("pl.trackList")}
        ></textarea>
        <div class="paste-row">
          <input bind:value={pasteName} placeholder={t("pl.name")} aria-label={t("pl.name")} />
          <button class="primary" type="submit" disabled={busy || !pasteText.trim()}>{t("pl.importButton")}</button>
          <button type="button" class="link" onclick={() => (pasting = false)}>{t("pl.cancel")}</button>
        </div>
      </form>
    {/if}

    {#if report}
      <div class="report" role="status">
        {#each report as r (r.id)}
          <div class="rep">
            {tn("pl.found", r.total, { name: r.name, found: r.found })}
            {#if r.missing.length}
              <details>
                <summary>{t("pl.notFound", { n: r.total - r.found })}</summary>
                <ul>{#each r.missing as m}<li>{m}</li>{/each}</ul>
                <p class="fine">{sentences([t("pl.notFoundHint"), signedIn.length > 0 && t("pl.notFoundHintStream")])}</p>
              </details>
            {/if}
          </div>
        {/each}
        <button class="link" onclick={() => (report = null)}>{t("pl.hide")}</button>
      </div>
    {/if}
    {#if error}<p class="err" role="alert">{error} <button class="link" onclick={() => (error = null)}>{t("pl.hide")}</button></p>{/if}
    {#if note}<p class="note" role="status">{note} <button class="link" onclick={() => (note = null)}>{t("pl.hide")}</button></p>{/if}

    {#if detail}
      <header class="head">
        {#if renaming}
          <form onsubmit={(e) => (e.preventDefault(), rename())} class="rename">
            <!-- svelte-ignore a11y_autofocus -->
            <input bind:value={newName} aria-label={t("pl.name")} autofocus onblur={rename} />
          </form>
        {:else}
          <h2><button class="title-btn" onclick={() => ((newName = detail!.name), (renaming = true))} title={t("pl.rename")}>{detail.name}</button></h2>
        {/if}
        <p class="facts">
          {list([tn("lib.nTracks", detail.entries.length), total > 0 && longDuration(total), missing > 0 && t("pl.unavailable", { n: missing })])}
        </p>
        <div class="actions">
          <button class="primary" onclick={() => play()} disabled={!detail.entries.some((e) => e.available)}>{t("common.play")}</button>
          <button class="ghost" onclick={() => { const { paths } = playable(); onenqueue(paths); }} disabled={!detail.entries.some((e) => e.available)}>{t("lib.enqueue")}</button>
          <span class="export">
            <button class="ghost" onclick={() => (exportOpen = !exportOpen)} aria-expanded={exportOpen} disabled={!detail.entries.length}>{t("pl.export")}</button>
            {#if exportOpen}
              <div class="menu" role="menu">
                {#each FORMATS as ext (ext)}
                  <button role="menuitem" onclick={() => exportAs(ext)}><b>{ext.toUpperCase()}</b><span>{t(`pl.fmt.${ext}`)}</span></button>
                {/each}
              </div>
            {/if}
          </span>
          {#if missing && signedIn.length}
            {#each signedIn as s (s)}
              <button class="ghost" onclick={() => findMissing(s)} disabled={busy}>{t("pl.findMissing", { service: SERVICE_NAME[s] })}</button>
            {/each}
          {/if}
          {#if detail.smart}
            <button class="ghost" onclick={() => (smartEdit = { id: detail!.id, name: detail!.name, rules: detail!.smart })}>{t("sp.edit")}</button>
          {/if}
          <button class="link danger" onclick={remove}>{confirmDelete ? t("pl.deleteConfirm") : t("pl.delete")}</button>
        </div>
      </header>

      {#if detail.entries.length}
        <TrackTable
          tracks={rows}
          {currentPath}
          sortable={false}
          columns={["number", "title", "artist", "album", "duration", "format"]}
          dimmed={(t) => !(t as Row).available}
          onplay={(_, i) => play((rows[i] as Row).entry)}
        >
          {#snippet actions(tr)}
            {@const r = tr as Row}
            {#if !detail?.smart}
            <span class="row-actions">
              <button onclick={() => move(r.entry, -1)} disabled={r.entry === 0 || !!query} aria-label={t("pl.up")} title={t("pl.up")}>↑</button>
              <button onclick={() => move(r.entry, 1)} disabled={r.entry === detail!.entries.length - 1 || !!query} aria-label={t("pl.down")} title={t("pl.down")}>↓</button>
              <button onclick={() => removeEntry(r.entry)} aria-label={t("pl.removeEntry", { title: r.title })} title={t("pl.removeEntry", { title: r.title })}>×</button>
            </span>
            {/if}
          {/snippet}
        </TrackTable>
      {:else}
        <p class="none">{detail.smart ? t("sp.empty") : streaming?.available ? t("pl.emptyStream") : t("pl.empty")}</p>
      {/if}
    {:else if !pasting}
      <div class="empty">
        <p class="empty-title">{t("pl.noneTitle")}</p>
        <p>{t("pl.noneText")}</p>
      </div>
    {/if}
  </section>
</div>

{#if smartEdit}
  <SmartEditor name={smartEdit.name} rules={smartEdit.rules} onsave={saveSmart} onclose={() => (smartEdit = null)} />
{/if}

<style>
  .playlists {
    display: grid;
    grid-template-columns: 220px minmax(0, 1fr);
    gap: 24px;
    padding: 22px 24px 32px;
    align-items: start;
  }
  .side {
    position: sticky;
    top: 0;
  }
  .side-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin-bottom: 12px;
  }
  .small {
    padding: 5px 10px;
    border-radius: var(--r-md);
    border: 1px solid var(--slate-500);
    font-size: var(--t-xs);
    font-weight: 600;
  }
  .small:hover:not(:disabled) {
    border-color: var(--mist);
  }
  .index {
    list-style: none;
    margin: 0;
    padding: 0;
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
  .pname {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .smart {
    color: var(--porcelain);
    margin-inline-end: 6px;
  }
  .count {
    color: var(--mist-dim);
    font-weight: 400;
  }
  .main {
    min-width: 0;
  }
  .head {
    margin-bottom: 18px;
  }
  h2 {
    margin: 0;
    font-family: var(--font-display);
    font-weight: 600;
    font-size: var(--t-2xl);
    line-height: 1.05;
  }
  .title-btn {
    text-align: start;
  }
  .title-btn:hover {
    color: var(--porcelain);
    text-decoration: underline;
    text-underline-offset: 3px;
  }
  .rename input {
    font-family: var(--font-display);
    font-weight: 600;
    font-size: var(--t-xl);
    width: min(100%, 520px);
  }
  .facts {
    margin: 6px 0 0;
    color: var(--mist);
    font-size: var(--t-sm);
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 12px;
    margin-top: 16px;
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
    opacity: 0.5;
  }
  .ghost {
    padding: 8px 16px;
    border-radius: var(--r-md);
    border: 1px solid var(--slate-500);
    font-weight: 600;
  }
  .ghost:hover:not(:disabled) {
    border-color: var(--mist);
  }
  .ghost:disabled {
    opacity: 0.5;
  }
  .link {
    color: var(--mist);
    font-size: var(--t-sm);
  }
  .link:hover {
    color: var(--porcelain);
  }
  .danger:hover {
    color: var(--alarm);
  }
  .export {
    position: relative;
  }
  .menu {
    position: absolute;
    top: calc(100% + 6px);
    inset-inline-start: 0;
    z-index: 20;
    width: 340px;
    background: var(--slate-700);
    border: 1px solid var(--slate-500);
    border-radius: var(--r-lg);
    padding: 6px;
    box-shadow: 0 18px 40px rgba(5, 12, 15, 0.55);
  }
  .menu button {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 1px;
    width: 100%;
    text-align: start;
    padding: 8px 10px;
    border-radius: var(--r-md);
    font-size: var(--t-sm);
  }
  .menu button:hover {
    background: var(--slate-600);
  }
  .menu span {
    color: var(--mist);
    font-size: var(--t-xs);
  }
  input,
  textarea {
    padding: 7px 10px;
    border-radius: var(--r-md);
    border: 1px solid var(--slate-500);
    background: var(--slate-900);
    color: var(--porcelain);
    font: inherit;
    font-size: var(--t-sm);
    user-select: text;
    -webkit-user-select: text;
  }
  .paste {
    margin-bottom: 20px;
    padding: 14px 16px;
    border-radius: var(--r-lg);
    background: var(--slate-700);
  }
  .hint {
    margin: 0 0 10px;
    color: var(--mist);
    font-size: var(--t-sm);
    max-width: 70ch;
  }
  textarea {
    width: 100%;
    resize: vertical;
    font-family: var(--font);
  }
  .paste-row {
    display: flex;
    gap: 12px;
    align-items: center;
    margin-top: 10px;
  }
  .paste-row input {
    flex: 1;
    max-width: 320px;
  }
  .report,
  .err,
  .note {
    margin: 0 0 16px;
    padding: 10px 14px;
    border-radius: var(--r-md);
    font-size: var(--t-sm);
    background: var(--slate-700);
  }
  .rep {
    margin: 0 0 6px;
  }
  .rep p {
    margin: 6px 0 0;
  }
  .report ul {
    margin: 6px 0;
    padding-inline-start: 18px;
    color: var(--mist);
    max-height: 180px;
    overflow: auto;
  }
  .fine {
    color: var(--mist);
  }
  summary {
    cursor: pointer;
    color: var(--mist);
  }
  .err {
    color: var(--alarm);
  }
  .note {
    color: var(--porcelain);
  }
  .row-actions {
    display: inline-flex;
    gap: 2px;
  }
  .row-actions button {
    width: 22px;
    height: 22px;
    border-radius: var(--r-sm);
    color: var(--mist);
    line-height: 1;
  }
  .row-actions button:hover:not(:disabled) {
    color: var(--porcelain);
    background: var(--slate-600);
  }
  .row-actions button:disabled {
    opacity: 0.3;
  }
  .none {
    color: var(--mist);
    margin: 0;
  }
  .empty {
    max-width: 56ch;
  }
  .empty-title {
    font-family: var(--font-display);
    font-weight: 600;
    font-size: var(--t-2xl);
    margin: 0 0 8px;
  }
  .empty p:not(.empty-title) {
    color: var(--mist);
  }

  @media (max-width: 700px), (max-height: 500px) {
    .playlists {
      grid-template-columns: minmax(0, 1fr);
      padding: 14px 12px 24px;
      gap: 12px;
    }
    .side {
      position: static;
    }
    .index {
      display: flex;
      overflow-x: auto;
      gap: 4px;
      scrollbar-width: none;
    }
    .index button {
      white-space: nowrap;
    }
    .menu {
      width: min(340px, calc(100vw - 24px));
    }
    .paste-row {
      flex-wrap: wrap;
    }
  }
</style>
