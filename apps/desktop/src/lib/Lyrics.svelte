<script module lang="ts">
  import type { Lyrics as LyricsData } from "./api";
  /** Lyrics already looked up (path + whether online was allowed): toggling the view or
      coming back to a track shows them at once. */
  const found = new Map<string, LyricsData | null>();
</script>

<script lang="ts">
  import { onMount } from "svelte";
  import { api, type NowPlaying, type Status } from "./api";
  import { t } from "./i18n.svelte";
  import { lyricsPrefs } from "./lyricsPrefs.svelte";

  let {
    now,
    status,
    onseek,
  }: {
    now: NowPlaying;
    status: Status;
    onseek: (seconds: number) => void;
  } = $props();

  /** A line shows this much before its time: the eye gets there as the voice does. */
  const LEAD = 0.15;
  /** After the listener scrolls, the view leaves them be for this long. */
  const HANDS_OFF = 4000;

  let lookup = $state<{ key: string; state: "loading" | "done" | "error"; lyrics: LyricsData | null } | null>(null);
  let attempt = $state(0);

  /** The track as the lookup needs it; a string, so a fresh `now` for the same track (the
      queue's revision moved) doesn't start the lookup over. */
  let track = $derived(JSON.stringify([now.path, now.title, now.artist, now.album, now.duration]));

  $effect(() => {
    const [path, title, artist, album, duration] = JSON.parse(track) as [string, string, string | null, string | null, number];
    const online = lyricsPrefs.online;
    void attempt;
    const key = `${path}\n${online}`;
    if (found.has(key)) {
      lookup = { key, state: "done", lyrics: found.get(key) ?? null };
      return;
    }
    lookup = { key, state: "loading", lyrics: null };
    let alive = true;
    api
      .lyrics({ path, title, artist, album, duration }, online)
      .then((l) => {
        found.set(key, l);
        if (alive) lookup = { key, state: "done", lyrics: l };
      })
      .catch(() => {
        if (alive) lookup = { key, state: "error", lyrics: null };
      });
    return () => {
      alive = false;
    };
  });

  let lyrics = $derived(lookup?.lyrics ?? null);
  type Row = { time: number | null; text: string };
  let rows = $derived.by((): Row[] => {
    if (!lyrics) return [];
    const r: Row[] = lyrics.lines.map((l) => ({ time: l.time, text: l.text }));
    // A long intro gets the break marker too, so the first words aren't lit before they're sung.
    if (lyrics.synced && (r[0]?.time ?? 0) > 4) r.unshift({ time: 0, text: "" });
    return r;
  });

  // ---- following the music --------------------------------------------------------------------
  // Status arrives four times a second; between those the position runs on from the last one,
  // so the line changes when it's sung rather than up to a quarter second late.
  let anchor = { pos: 0, at: 0 };
  $effect(() => {
    anchor = { pos: status.position, at: performance.now() };
  });
  let active = $state(-1);

  function lineAt(pos: number): number {
    let i = -1;
    for (let k = 0; k < rows.length; k++) {
      const time = rows[k].time;
      if (time === null || time > pos + LEAD) break;
      i = k;
    }
    return i;
  }

  onMount(() => {
    let alive = true;
    const tick = () => {
      if (!alive) return;
      if (lyrics?.synced) {
        const running = status.active && !status.paused && !status.buffering;
        // Never more than a moment past the last report: a stall must not run the words ahead.
        const pos = running ? anchor.pos + Math.min((performance.now() - anchor.at) / 1000, 0.6) : anchor.pos;
        const i = lineAt(pos);
        if (i !== active) active = i;
      } else if (active !== -1) {
        active = -1;
      }
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
    return () => {
      alive = false;
      clearTimeout(handsOffTimer);
    };
  });

  // ---- scrolling ------------------------------------------------------------------------------
  let box = $state<HTMLElement | null>(null);
  let lineEls: HTMLElement[] = $state([]);
  let handsOffUntil = 0;
  let handsOffTimer: ReturnType<typeof setTimeout> | undefined;
  const reduceMotion = typeof window !== "undefined" && window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  /** Bring the sung line to a third of the way down, where the eye rests. */
  function follow(smooth = true) {
    if (!box || !lyrics?.synced) return;
    const el = lineEls[Math.max(active, 0)];
    if (!el) return;
    const top = el.offsetTop + el.offsetHeight / 2 - box.clientHeight * 0.36;
    box.scrollTo({ top: Math.max(0, top), behavior: smooth && !reduceMotion ? "smooth" : "auto" });
  }

  $effect(() => {
    void active;
    if (performance.now() >= handsOffUntil) follow();
  });

  // A new track (or new lyrics): from the top, at once.
  $effect(() => {
    void lookup?.key;
    void lyrics;
    handsOffUntil = 0;
    if (box) box.scrollTop = 0;
    requestAnimationFrame(() => follow(false));
  });

  /** The listener scrolls: leave them be for a while, then go back to the music. */
  function handsOff() {
    if (!lyrics?.synced) return;
    handsOffUntil = performance.now() + HANDS_OFF;
    clearTimeout(handsOffTimer);
    handsOffTimer = setTimeout(() => follow(), HANDS_OFF);
  }

  function seekTo(time: number | null) {
    if (time === null) return;
    handsOffUntil = 0;
    clearTimeout(handsOffTimer);
    anchor = { pos: time, at: performance.now() };
    onseek(time);
  }

  // The window resized: lines wrap anew, so the sung one moved.
  $effect(() => {
    if (!box || typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(() => {
      if (performance.now() >= handsOffUntil) follow(false);
    });
    ro.observe(box);
    // The listener scrolling (wheel, finger, keys): not the view following the music.
    const el = box;
    const events = ["wheel", "touchmove", "keydown"] as const;
    for (const e of events) el.addEventListener(e, handsOff, { passive: true });
    return () => {
      ro.disconnect();
      for (const e of events) el.removeEventListener(e, handsOff);
    };
  });

  let sourceLabel = $derived(
    !lyrics ? null : lyrics.source === "online" ? t("lyrics.fromLrclib") : lyrics.source === "sidecar" ? t("lyrics.fromLrc") : t("lyrics.fromFile"),
  );
</script>

<section class="lyrics" aria-label={t("lyrics.title")}>
  {#if lookup?.state === "loading"}
    <p class="state loading">{t("lyrics.loading")}</p>
  {:else if lookup?.state === "error"}
    <div class="state">
      <p class="head">{t("lyrics.offline")}</p>
      <button class="action" onclick={() => attempt++}>{t("lyrics.retry")}</button>
    </div>
  {:else if lookup?.state === "done" && !lyrics}
    <div class="state">
      <p class="head">{t("lyrics.none")}</p>
      {#if lyricsPrefs.online}
        <p class="help">{t("lyrics.noneOnline")}</p>
      {:else}
        <p class="help">{t("lyrics.noneHelp")}</p>
        <button class="action" onclick={() => (lyricsPrefs.online = true)}>{t("lyrics.searchOnline")}</button>
        <p class="help small">{t("lyrics.onlineHelp")}</p>
      {/if}
    </div>
  {:else if lyrics}
    <!-- a scrolling region takes focus, so the keyboard can scroll it too -->
    <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
    <div
      class="scroll"
      class:synced={lyrics.synced}
      bind:this={box}
      tabindex="0"
      role="region"
      aria-label={t("lyrics.title")}
    >
      <div class="pad top" aria-hidden="true"></div>
      {#if lyrics.synced}
        {#each rows as row, i (i)}
          {#if row.text}
            <button
              class="line"
              class:on={i === active}
              class:past={i < active}
              bind:this={lineEls[i]}
              onclick={() => seekTo(row.time)}
              title={t("lyrics.seek")}
              aria-current={i === active ? "true" : undefined}
              dir="auto">{row.text}</button
            >
          {:else}
            <div class="break" class:on={i === active} bind:this={lineEls[i]} aria-hidden="true"><i></i><i></i><i></i></div>
          {/if}
        {/each}
      {:else}
        {#each rows as row, i (i)}
          {#if row.text}<p class="plain" dir="auto">{row.text}</p>{:else}<p class="gap" aria-hidden="true"></p>{/if}
        {/each}
      {/if}
      <div class="pad bottom" aria-hidden="true"></div>
    </div>
    <p class="source">{lyrics.synced ? sourceLabel : `${sourceLabel}${t("list.sep")}${t("lyrics.untimed")}`}</p>
  {/if}
</section>

<style>
  .lyrics {
    position: relative;
    display: flex;
    flex-direction: column;
    min-height: 0;
  }
  .scroll {
    position: relative;
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    overscroll-behavior: contain;
    scrollbar-width: none;
    /* the lines come out of and go into the dark at the edges */
    -webkit-mask-image: linear-gradient(180deg, transparent, #000 14%, #000 78%, transparent);
    mask-image: linear-gradient(180deg, transparent, #000 14%, #000 78%, transparent);
    container-type: size;
    outline: none;
  }
  .scroll::-webkit-scrollbar {
    display: none;
  }
  .scroll:focus-visible {
    box-shadow: inset 0 0 0 1px var(--slate-500);
    border-radius: var(--r-md);
  }
  /* room to bring the first line to a third of the way down, and the last one up to it */
  .pad.top {
    height: 30cqh;
  }
  .scroll:not(.synced) .pad.top {
    height: 10cqh;
  }
  .pad.bottom {
    height: 60cqh;
  }

  .line {
    display: block;
    width: 100%;
    padding: 7px 0;
    text-align: start;
    font-family: var(--font-display);
    font-weight: 700;
    /* by the room the words have, not the window: a narrow column gets smaller lines */
    font-size: clamp(1.3rem, 0.55rem + 3.6cqi, 2.1rem);
    line-height: 1.18;
    letter-spacing: -0.01em;
    color: var(--porcelain);
    opacity: 0.3;
    transform-origin: 0 50%;
    transform: scale(0.97);
    overflow-wrap: anywhere;
    transition:
      opacity 0.45s ease,
      transform 0.45s cubic-bezier(0.2, 0.7, 0.2, 1);
  }
  :global([dir="rtl"]) .line {
    transform-origin: 100% 50%;
  }
  .line:hover {
    opacity: 0.6;
  }
  .line.on {
    opacity: 1;
    transform: none;
  }
  .line:focus-visible {
    outline: 1px solid var(--slate-500);
    outline-offset: 2px;
    border-radius: var(--r-sm);
  }

  /* an instrumental break: three dots that breathe while it lasts, and fold away after */
  .break {
    display: flex;
    gap: 7px;
    height: 0;
    overflow: hidden;
    opacity: 0;
    transition:
      height 0.4s ease,
      opacity 0.4s ease;
  }
  .break.on {
    height: 44px;
    padding-top: 14px;
    opacity: 1;
  }
  .break i {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    background: var(--porcelain);
    opacity: 0.35;
    animation: breathe 1.8s ease-in-out infinite;
  }
  .break i:nth-child(2) {
    animation-delay: 0.3s;
  }
  .break i:nth-child(3) {
    animation-delay: 0.6s;
  }
  @keyframes breathe {
    50% {
      opacity: 1;
      transform: scale(1.15);
    }
  }

  .plain {
    margin: 0;
    padding: 3px 0;
    font-size: var(--t-lg);
    font-weight: 500;
    line-height: 1.4;
    color: var(--porcelain);
    overflow-wrap: anywhere;
  }
  .gap {
    margin: 0;
    height: 1em;
  }

  .source {
    margin: 6px 0 0;
    font-size: var(--t-xs);
    color: var(--mist-dim);
  }

  .state {
    margin: auto 0;
    padding: 24px 0;
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 6px;
    max-width: 46ch;
  }
  .head {
    margin: 0;
    font-family: var(--font-display);
    font-size: var(--t-lg);
    font-weight: 600;
  }
  .help {
    margin: 0;
    color: var(--mist);
    font-size: var(--t-sm);
  }
  .help.small {
    color: var(--mist-dim);
    font-size: var(--t-xs);
  }
  .action {
    margin: 8px 0 4px;
    padding: 7px 14px;
    border-radius: 999px;
    border: 1px solid var(--slate-500);
    font-size: var(--t-sm);
    color: var(--porcelain);
  }
  .action:hover {
    border-color: var(--mist);
  }
  p.loading {
    margin: auto 0;
    color: var(--mist);
    /* only when the wait is noticeable: a file's own lyrics come back before this shows */
    animation: appear 0.3s ease 0.35s both;
  }
  @keyframes appear {
    from {
      opacity: 0;
    }
  }

  @media (prefers-reduced-motion: reduce) {
    .line,
    .break {
      transition: none;
    }
    .break i {
      animation: none;
    }
  }
</style>
