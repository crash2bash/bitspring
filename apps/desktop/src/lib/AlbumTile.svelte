<script lang="ts">
  import { artistLabel, coverUrl, resolution, type Album } from "./api";
  import { num, t } from "./i18n.svelte";
  import Heart from "./Heart.svelte";

  let {
    album,
    playing = false,
    favorite = false,
    favTracks = 0,
    src = undefined,
    onopen,
  }: {
    album: Album;
    playing?: boolean;
    /** The listener's heart is on it. */
    favorite?: boolean;
    /** How many of its tracks carry the listener's heart. */
    favTracks?: number;
    /** Cover URL (streaming albums); library albums use the cover:// protocol. */
    src?: string | null;
    onopen: (a: Album) => void;
  } = $props();
  let broken = $state(false);
</script>

<button class="tile" onclick={() => onopen(album)} title={`${artistLabel(album.artist)} — ${album.title}`}>
  <span class="art">
    {#if album.hasCover && !broken}
      <img src={src ?? coverUrl(album.id)} alt="" loading="lazy" decoding="async" onerror={() => (broken = true)} />
    {:else}
      <span class="initial" aria-hidden="true">{album.title.slice(0, 1)}</span>
    {/if}
    {#if playing}<span class="now">{t("common.playing")}</span>{/if}
  </span>
  <span class="title">{album.title}</span>
  <span class="artist">{artistLabel(album.artist)}</span>
  <span class="meta">
    {#if album.year}<span class="num">{album.year}</span>{/if}
    <span class="fmt" class:hi={album.hiRes} class:lossy={album.format === "MP3" || album.format === "AAC" || album.format === "Vorbis" || album.format === "Opus"}>
      {(album.hiRes || album.maxBits) && album.maxRate ? resolution(album.maxBits, album.maxRate) : album.format}
    </span>
    <!-- A filled heart: the album itself is a favourite. A number: that many of its tracks
         are (beside an outlined heart where the album is not). -->
    {#if favorite || favTracks}
      {@const tip = [favorite && t("fav.album"), favTracks > 0 && t("fav.tracksIn", { n: num(favTracks) })].filter(Boolean).join(". ")}
      <span class="fav" class:tracks-only={!favorite} title={tip} aria-label={tip}>
        {#if favTracks}<span class="num">{num(favTracks)}</span>{/if}<Heart on={favorite} size={13} />
      </span>
    {/if}
  </span>
</button>

<style>
  .tile {
    display: flex;
    flex-direction: column;
    /* explicit: Android's WebView gives a <button> align-items: flex-start, which sizes a
       long title to its own width (past the tile) so its ellipsis never shows */
    align-items: stretch;
    text-align: start;
    min-width: 0;
    gap: 2px;
    border-radius: var(--r-md);
    padding: 0;
  }
  .art {
    position: relative;
    /* explicit: older WebViews (Android) don't stretch an aspect-ratio box in a column */
    width: 100%;
    aspect-ratio: 1;
    border-radius: var(--r-md);
    overflow: hidden;
    margin-bottom: 8px;
    display: grid;
    place-items: center;
    background:
      repeating-linear-gradient(90deg, rgba(232, 237, 234, 0.05) 0 2px, transparent 2px 8px),
      var(--slate-700);
    box-shadow: 0 8px 20px rgba(5, 12, 15, 0.45);
    transition: transform 160ms ease;
  }
  .tile:hover .art {
    transform: translateY(-2px);
  }
  img {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }
  .initial {
    font-family: var(--font-display);
    font-weight: 600;
    font-size: 3rem;
    color: var(--slate-500);
  }
  .now {
    position: absolute;
    inset-inline-start: 8px;
    bottom: 8px;
    padding: 2px 8px;
    border-radius: var(--r-sm);
    font-size: var(--t-xs);
    font-weight: 600;
    background: rgba(14, 24, 28, 0.85);
    color: var(--live);
  }
  .title {
    font-weight: 600;
    font-size: var(--t-sm);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .artist {
    font-size: var(--t-sm);
    color: var(--mist);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .meta {
    display: flex;
    gap: 8px;
    align-items: baseline;
    font-size: var(--t-xs);
    color: var(--mist-dim);
  }
  /* the listener's heart, at the end of the line */
  .fav {
    margin-inline-start: auto;
    align-self: center;
    display: flex;
    align-items: center;
    gap: 3px;
    color: var(--porcelain);
  }
  .fav.tracks-only {
    color: var(--mist);
  }
  .fmt {
    color: var(--mist);
  }
  .fmt.hi {
    color: var(--porcelain);
  }
  .fmt.lossy {
    color: var(--mist-dim);
  }
</style>
