<script lang="ts">
  import { api, errorText, type LibTrack, type TagEdit } from "./api";
  import { t } from "./i18n.svelte";

  let { tracks, onclose, onsaved }: { tracks: LibTrack[]; onclose: () => void; onsaved: (path: string) => void } = $props();

  type AlbumField = "album" | "albumArtist" | "artist" | "year" | "genre";
  const ALBUM_FIELDS: AlbumField[] = ["album", "albumArtist", "artist", "year", "genre"];

  // The tracks we can write (not DSD, not the parts of a CUE image).
  const editable = (tr: LibTrack) => !tr.path.includes("#cue:") && !tr.path.includes("#sacd:") && !/\.(dsf|dff)$/i.test(tr.path);
  // svelte-ignore state_referenced_locally
  const writable = tracks.filter(editable);
  // svelte-ignore state_referenced_locally
  const readOnly = tracks.length - writable.length;

  function common(f: AlbumField): { value: string; mixed: boolean } {
    const vals = new Set(writable.map((tr) => (tr[f] ?? "") as string));
    return vals.size === 1 ? { value: [...vals][0], mixed: false } : { value: "", mixed: true };
  }
  const start = Object.fromEntries(ALBUM_FIELDS.map((f) => [f, common(f)])) as Record<AlbumField, { value: string; mixed: boolean }>;
  let album = $state(Object.fromEntries(ALBUM_FIELDS.map((f) => [f, start[f].value])) as Record<AlbumField, string>);
  let touched = $state<Record<string, boolean>>({});

  let rows = $state(
    writable.map((tr) => ({ path: tr.path, number: tr.trackNumber?.toString() ?? "", title: tr.title, artist: tr.artist ?? "" })),
  );
  const original = new Map(writable.map((tr) => [tr.path, { number: tr.trackNumber?.toString() ?? "", title: tr.title, artist: tr.artist ?? "" }]));

  function setAlbum(f: AlbumField, v: string) {
    album[f] = v;
    touched[f] = true;
    // One artist for the whole album: written into every track's row.
    if (f === "artist") for (const r of rows) r.artist = v;
  }

  let saving = $state<{ done: number; total: number } | null>(null);
  let failures = $state<string[]>([]);

  function editFor(r: (typeof rows)[number]): TagEdit {
    const e: TagEdit = {};
    for (const f of ["album", "albumArtist", "year", "genre"] as const) if (touched[f]) e[f] = album[f].trim();
    const o = original.get(r.path)!;
    if (r.title.trim() !== o.title) e.title = r.title.trim();
    if (r.artist.trim() !== o.artist) e.artist = r.artist.trim();
    if (r.number.trim() !== o.number) e.trackNumber = r.number.trim();
    return e;
  }

  async function save() {
    const edits = rows.map((r) => [r.path, editFor(r)] as [string, TagEdit]).filter(([, e]) => Object.keys(e).length);
    if (!edits.length) return onclose();
    failures = [];
    saving = { done: 0, total: edits.length };
    // A few at a time, so the count moves and one bad file doesn't hold up the rest.
    for (let i = 0; i < edits.length; i += 4) {
      const part = edits.slice(i, i + 4);
      try {
        const failed = await api.editTags(part);
        for (const f of failed) failures.push(`${f.path.split(/[\\/]/).pop()}: ${f.error}`);
      } catch (e) {
        failures.push(errorText(e));
      }
      saving = { done: Math.min(i + 4, edits.length), total: edits.length };
    }
    saving = null;
    onsaved(edits[0][0]);
    if (!failures.length) onclose();
  }
</script>

<svelte:window onkeydown={(e) => e.key === "Escape" && !saving && onclose()} />

<div class="bg" role="presentation" onclick={(e) => e.target === e.currentTarget && !saving && onclose()}>
  <div class="dialog" role="dialog" aria-modal="true" aria-labelledby="tags-title">
    <header>
      <h3 id="tags-title">{t("tags.edit")}</h3>
      <p class="sub">{t("tags.note")}</p>
    </header>

    <div class="fields">
      {#each ALBUM_FIELDS as f (f)}
        <label class:wide={f === "album"}>
          <span>{t(`tags.${f}`)}</span>
          <input
            value={album[f]}
            placeholder={start[f].mixed && !touched[f] ? t("tags.mixed") : ""}
            oninput={(e) => setAlbum(f, e.currentTarget.value)}
            inputmode={f === "year" ? "numeric" : undefined}
          />
        </label>
      {/each}
    </div>

    <div class="table" role="table" aria-label={t("tags.tracks")}>
      <div class="head" role="row">
        <span role="columnheader">{t("tags.number")}</span>
        <span role="columnheader">{t("tags.trackTitle")}</span>
        <span role="columnheader">{t("tags.artist")}</span>
      </div>
      {#each rows as r (r.path)}
        <div class="row" role="row">
          <input class="num" bind:value={r.number} inputmode="numeric" aria-label={t("tags.number")} />
          <input bind:value={r.title} aria-label={t("tags.trackTitle")} />
          <input bind:value={r.artist} aria-label={t("tags.artist")} />
        </div>
      {/each}
    </div>

    {#if readOnly}<p class="note">{t("tags.readOnly")}</p>{/if}
    {#each failures as f (f)}<p class="note err">{f}</p>{/each}

    <footer>
      {#if saving}<span class="progress">{t("tags.saving", { done: saving.done, total: saving.total })}</span>{/if}
      <button class="ghost" onclick={onclose} disabled={!!saving}>{t("pl.cancel")}</button>
      <button class="primary" onclick={save} disabled={!!saving || !writable.length}>{t("tags.save")}</button>
    </footer>
  </div>
</div>

<style>
  .bg {
    position: fixed;
    inset: 0;
    z-index: 60;
    display: grid;
    place-items: center;
    padding: var(--safe-top, env(safe-area-inset-top)) 16px var(--safe-bottom, env(safe-area-inset-bottom));
    background: rgba(5, 12, 15, 0.6);
  }
  .dialog {
    width: min(720px, 100%);
    max-height: calc(100dvh - 32px);
    display: flex;
    flex-direction: column;
    gap: 14px;
    padding: 18px 20px 16px;
    border-radius: var(--r-lg);
    background: var(--slate-800);
    border: 1px solid var(--slate-500);
    box-shadow: 0 24px 60px rgba(0, 0, 0, 0.45);
  }
  h3 {
    margin: 0;
    font-family: var(--font-display);
    font-size: var(--t-lg);
    font-weight: 600;
  }
  .sub {
    margin: 4px 0 0;
    font-size: var(--t-sm);
    color: var(--mist);
  }
  .fields {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
    gap: 10px 12px;
  }
  label {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-size: var(--t-xs);
    color: var(--mist);
  }
  label.wide {
    grid-column: 1 / -1;
  }
  input {
    font: inherit;
    font-size: var(--t-sm);
    color: var(--porcelain);
    background: var(--slate-700);
    border: 1px solid var(--slate-600);
    border-radius: var(--r-sm);
    padding: 7px 9px;
    min-width: 0;
  }
  input:focus {
    border-color: var(--mist);
    outline: none;
  }
  input::placeholder {
    color: var(--mist-dim);
    font-style: italic;
  }
  .table {
    overflow-y: auto;
    min-height: 0;
    border-top: 1px solid var(--slate-600);
    padding-top: 8px;
  }
  .head,
  .row {
    display: grid;
    grid-template-columns: 52px 1fr 1fr;
    gap: 8px;
    padding-block: 3px;
  }
  .head span {
    font-size: var(--t-xs);
    color: var(--mist);
  }
  .num {
    text-align: center;
    font-variant-numeric: tabular-nums;
  }
  .note {
    margin: 0;
    font-size: var(--t-xs);
    color: var(--mist);
  }
  .note.err {
    color: var(--alarm);
  }
  footer {
    display: flex;
    justify-content: flex-end;
    align-items: center;
    gap: 10px;
  }
  .progress {
    margin-inline-end: auto;
    font-size: var(--t-sm);
    color: var(--mist);
  }
  footer button {
    padding: 8px 16px;
    border-radius: var(--r-md);
    font-weight: 600;
  }
  .ghost {
    border: 1px solid var(--slate-500);
  }
  .primary {
    background: var(--porcelain);
    color: var(--ink);
  }
  button:disabled {
    opacity: 0.5;
  }
  @media (max-width: 560px) {
    .head,
    .row {
      grid-template-columns: 44px 1fr;
    }
    .head span:last-child,
    .row input:last-child {
      display: none;
    }
  }
</style>
