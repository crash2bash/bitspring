<script lang="ts" module>
  export type SortKey = "number" | "title" | "artist" | "album" | "year" | "duration" | "format";
</script>

<script lang="ts">
  import { clock, khz, resolution, type LibTrack } from "./api";

  let {
    tracks,
    currentPath = null,
    columns = ["number", "title", "artist", "album", "year", "duration", "format"],
    sortable = true,
    sortKey = $bindable<SortKey>("artist"),
    sortAsc = $bindable(true),
    onplay,
  }: {
    tracks: LibTrack[];
    currentPath?: string | null;
    columns?: SortKey[];
    sortable?: boolean;
    sortKey?: SortKey;
    sortAsc?: boolean;
    /** Play `list` starting at `index` (the list as currently shown). */
    onplay: (list: LibTrack[], index: number) => void;
  } = $props();

  const LABEL: Record<SortKey, string> = {
    number: "№",
    title: "Название",
    artist: "Исполнитель",
    album: "Альбом",
    year: "Год",
    duration: "Время",
    format: "Формат",
  };

  const collator = new Intl.Collator("ru", { numeric: true, sensitivity: "base" });
  const artistOf = (t: LibTrack) => t.albumArtist ?? t.artist ?? "";
  /** Empty values (untagged files) sort last, whichever direction. */
  const filledFirst = (a: string, b: string) => Number(!a) - Number(!b);
  // Within an artist or album, keep album order (disc, track) as the tie-breaker.
  const albumOrder = (a: LibTrack, b: LibTrack) =>
    collator.compare(a.album ?? "", b.album ?? "") ||
    (a.discNumber ?? 1) - (b.discNumber ?? 1) ||
    (a.trackNumber ?? 999) - (b.trackNumber ?? 999) ||
    collator.compare(a.path, b.path);

  let sorted = $derived.by(() => {
    if (!sortable) return tracks;
    const dir = sortAsc ? 1 : -1;
    const by: Record<SortKey, (a: LibTrack, b: LibTrack) => number> = {
      number: (a, b) => (a.trackNumber ?? 999) - (b.trackNumber ?? 999),
      title: (a, b) => collator.compare(a.title, b.title),
      artist: (a, b) => collator.compare(artistOf(a), artistOf(b)) || collator.compare(a.year ?? "", b.year ?? ""),

      album: (a, b) => collator.compare(a.album ?? "", b.album ?? ""),
      year: (a, b) => collator.compare(a.year ?? "", b.year ?? ""),
      duration: (a, b) => a.duration - b.duration,
      format: (a, b) => (a.bits ?? 0) - (b.bits ?? 0) || a.sampleRate - b.sampleRate,
    };
    const value: Partial<Record<SortKey, (t: LibTrack) => string>> = {
      artist: artistOf,
      album: (t) => t.album ?? "",
      year: (t) => t.year ?? "",
    };
    const v = value[sortKey];
    return [...tracks].sort(
      (a, b) => (v ? filledFirst(v(a), v(b)) : 0) || dir * by[sortKey](a, b) || albumOrder(a, b),
    );
  });

  function sortBy(k: SortKey) {
    if (!sortable) return;
    if (sortKey === k) sortAsc = !sortAsc;
    else {
      sortKey = k;
      sortAsc = k !== "duration" && k !== "format";
    }
  }
</script>

<table class="tracks">
  <thead>
    <tr>
      {#each columns as c (c)}
        <th class={c} aria-sort={sortable && sortKey === c ? (sortAsc ? "ascending" : "descending") : undefined}>
          {#if sortable}
            <button onclick={() => sortBy(c)}>
              {LABEL[c]}{#if sortKey === c}<span class="arrow" aria-hidden="true">{sortAsc ? "▲" : "▼"}</span>{/if}
            </button>
          {:else}
            <span class="th">{LABEL[c]}</span>
          {/if}
        </th>
      {/each}
    </tr>
  </thead>
  <tbody>
    {#each sorted as t, i (t.path)}
      <tr class:current={t.path === currentPath} ondblclick={() => onplay(sorted, i)}>
        {#each columns as c (c)}
          <td class={c}>
            {#if c === "number"}
              {#if t.path === currentPath}
                <svg width="10" height="10" viewBox="0 0 10 10" aria-label="Играет"><path d="M2 1v8l7-4z" fill="currentColor" /></svg>
              {:else}
                <button class="play-row" onclick={() => onplay(sorted, i)} aria-label={`Играть «${t.title}»`}>
                  <span class="n num">{t.trackNumber ?? "–"}</span>
                  <svg class="p" width="10" height="10" viewBox="0 0 10 10" aria-hidden="true"><path d="M2 1v8l7-4z" fill="currentColor" /></svg>
                </button>
              {/if}
            {:else if c === "title"}
              {t.title}
            {:else if c === "artist"}
              {t.artist ?? "—"}
            {:else if c === "album"}
              {t.album ?? "—"}
            {:else if c === "year"}
              <span class="num">{t.year ?? ""}</span>
            {:else if c === "duration"}
              <span class="num">{clock(t.duration)}</span>
            {:else if c === "format"}
              <span class="fmt" class:hi={t.hiRes} class:lossy={!t.lossless}>
                {t.format}{t.lossless ? (t.bits ? ` ${resolution(t.bits, t.sampleRate)}` : ` ${khz(t.sampleRate)} кГц`) : ""}
              </span>
            {/if}
          </td>
        {/each}
      </tr>
    {/each}
  </tbody>
</table>

<style>
  .tracks {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--t-sm);
    table-layout: fixed;
  }
  th {
    position: sticky;
    top: 0;
    z-index: 1;
    background: var(--slate-800);
    text-align: left;
    font-weight: 500;
    color: var(--mist);
    padding: 0;
    border-bottom: 1px solid var(--slate-600);
  }
  th button,
  .th {
    display: block;
    width: 100%;
    text-align: inherit;
    padding: 8px 10px;
    color: inherit;
  }
  th button:hover {
    color: var(--porcelain);
  }
  th[aria-sort] {
    color: var(--porcelain);
  }
  .arrow {
    font-size: 8px;
    margin-left: 5px;
    vertical-align: 1px;
  }
  td {
    padding: 7px 10px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    border-bottom: 1px solid rgba(43, 65, 76, 0.5);
  }
  tbody tr {
    /* long libraries: let the browser skip layout for rows far off-screen */
    content-visibility: auto;
    contain-intrinsic-size: auto 33px;
  }
  tbody tr:hover {
    background: var(--slate-700);
  }
  tr.current td {
    color: var(--brass);
  }
  .number {
    width: 44px;
    text-align: right;
  }
  th.number button,
  th.number .th {
    text-align: right;
  }
  .year {
    width: 60px;
  }
  .duration {
    width: 64px;
    text-align: right;
  }
  th.duration button,
  th.duration .th {
    text-align: right;
  }
  .format {
    width: 120px;
  }
  td.title {
    font-weight: 500;
  }
  td.artist,
  td.album,
  td.year {
    color: var(--mist);
  }
  .play-row {
    display: inline-grid;
    place-items: center end;
    width: 100%;
  }
  .play-row > * {
    grid-area: 1 / 1;
  }
  .play-row .p {
    opacity: 0;
    color: var(--porcelain);
  }
  .n {
    color: var(--mist-dim);
  }
  tr:hover .play-row .n {
    opacity: 0;
  }
  tr:hover .play-row .p,
  .play-row:focus-visible .p {
    opacity: 1;
  }
  .fmt {
    color: var(--mist);
  }
  .fmt.hi {
    color: var(--brass);
  }
  .fmt.lossy {
    color: var(--mist-dim);
  }
</style>
