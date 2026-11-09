<script lang="ts">
  import { coverUrl, resolution, type Album } from "./api";

  let { album, playing = false, onopen }: { album: Album; playing?: boolean; onopen: (a: Album) => void } = $props();
  let broken = $state(false);
</script>

<button class="tile" onclick={() => onopen(album)} title={`${album.artist} — ${album.title}`}>
  <span class="art">
    {#if album.hasCover && !broken}
      <img src={coverUrl(album.id)} alt="" loading="lazy" decoding="async" onerror={() => (broken = true)} />
    {:else}
      <span class="initial" aria-hidden="true">{album.title.slice(0, 1)}</span>
    {/if}
    {#if playing}<span class="now" aria-label="Играет">Играет</span>{/if}
  </span>
  <span class="title">{album.title}</span>
  <span class="artist">{album.artist}</span>
  <span class="meta">
    {#if album.year}<span class="num">{album.year}</span>{/if}
    <span class="fmt" class:hi={album.hiRes} class:lossy={album.format === "MP3" || album.format === "AAC" || album.format === "Vorbis"}>
      {album.hiRes || album.maxBits ? resolution(album.maxBits, album.maxRate) : album.format}
    </span>
  </span>
</button>

<style>
  .tile {
    display: flex;
    flex-direction: column;
    text-align: left;
    min-width: 0;
    gap: 2px;
    border-radius: 8px;
    padding: 0;
  }
  .art {
    position: relative;
    /* explicit: older WebViews (Android) don't stretch an aspect-ratio box in a column */
    width: 100%;
    aspect-ratio: 1;
    border-radius: 6px;
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
    left: 8px;
    bottom: 8px;
    padding: 2px 8px;
    border-radius: 4px;
    font-size: var(--t-xs);
    font-weight: 600;
    background: rgba(14, 24, 28, 0.85);
    color: var(--brass);
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
