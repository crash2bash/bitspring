<script lang="ts">
  import { onMount, tick } from "svelte";
  import { api, errorText, type EqChanged } from "./api";
  import { num, t, tn } from "./i18n.svelte";

  let {
    onpicked,
    onclose,
  }: {
    /** A profile became a preset. */
    onpicked: (res: EqChanged & { ids: string[] }) => void;
    onclose: () => void;
  } = $props();

  /** One measurement of a model: from the downloaded catalog (`index`), or built in. */
  interface Measure {
    source: string;
    rig?: string | null;
    index?: number;
  }
  interface Model {
    name: string;
    /** Lower case, accents dropped, punctuation as spaces: "sennheiser hd 600". */
    key: string;
    /** The same without spaces, so "hd600" finds "HD 600". */
    compact: string;
    popular: boolean;
    measures: Measure[];
  }

  /** Whose measurements to suggest first: the most careful rigs, then the biggest databases. */
  const PREFER = ["oratory1990", "crinacle", "Rtings", "Innerfidelity", "Headphone.com Legacy", "HypetheSonics", "Super Review", "Kuulokenurkka"];
  const rank = (m: Measure) => {
    const i = PREFER.indexOf(m.source);
    return i < 0 ? PREFER.length : i;
  };
  const LIMIT = 60;

  const fold = (s: string) => s.toLowerCase().normalize("NFKD").replace(/[̀-ͯ]/g, "");
  const words = (s: string) => fold(s).split(/[^\p{L}\p{N}]+/u).filter(Boolean);

  function model(name: string, popular: boolean, measures: Measure[]): Model {
    const key = words(name).join(" ");
    return { name, key, compact: key.replaceAll(" ", ""), popular, measures: measures.sort((a, b) => rank(a) - rank(b)) };
  }

  let models = $state<Model[]>([]);
  let catalog = $state<"loading" | "ready" | "failed">("loading");
  let catalogError = $state<string | null>(null);
  let query = $state("");
  let active = $state(0);
  /** The model whose measurements are being chosen from. */
  let chosen = $state<Model | null>(null);
  let busy = $state(false);
  let error = $state<string | null>(null);
  let input = $state<HTMLInputElement | null>(null);
  let list = $state<HTMLElement | null>(null);

  let popularNames = new Set<string>();

  async function loadCatalog(refresh = false) {
    catalog = "loading";
    catalogError = null;
    try {
      const all = await api.eqCatalog(refresh);
      const byName = new Map<string, Measure[]>();
      all.forEach((e, index) => {
        const m = byName.get(e.name) ?? [];
        m.push({ source: e.source, rig: e.rig, index });
        byName.set(e.name, m);
      });
      // Built-in models stay even if the catalog has dropped one.
      for (const m of models) if (m.popular && !byName.has(m.name)) byName.set(m.name, m.measures);
      models = [...byName].map(([name, measures]) => model(name, popularNames.has(name), measures));
      catalog = "ready";
    } catch (e) {
      catalog = "failed";
      catalogError = errorText(e);
    }
  }

  onMount(async () => {
    try {
      const built = await api.eqHeadphones();
      popularNames = new Set(built.map((b) => b.name));
      if (catalog !== "ready") models = built.map((b) => model(b.name, true, [{ source: b.source }]));
    } catch (e) {
      error = errorText(e);
    }
    loadCatalog();
    input?.focus();
  });

  /** Models matching the query, best first: every word typed must be in the name. */
  let matches = $derived.by(() => {
    const q = words(query);
    if (!q.length) return models.filter((m) => m.popular).sort((a, b) => a.name.localeCompare(b.name));
    const joined = q.join("");
    const scored: [number, Model][] = [];
    for (const m of models) {
      if (!q.every((w) => m.compact.includes(w))) continue;
      // Starts with what was typed; a word starts with it; anywhere.
      const score = m.compact.startsWith(joined) ? 0 : (" " + m.key).includes(" " + q[0]) ? 1 : 2;
      scored.push([score * 2 + (m.popular ? 0 : 1), m]);
    }
    return scored.sort((a, b) => a[0] - b[0] || a[1].name.length - b[1].name.length || a[1].name.localeCompare(b[1].name)).map((s) => s[1]);
  });
  let shown = $derived(matches.slice(0, LIMIT));
  let modelCount = $derived(models.length);

  $effect(() => {
    void query;
    active = 0;
  });

  /** The name in pieces, the parts matching a typed word marked. */
  function pieces(name: string): { text: string; hit: boolean }[] {
    const q = words(query);
    if (!q.length) return [{ text: name, hit: false }];
    // Letters and digits only, each remembering where it is in the name, so "hd600" marks "HD 600".
    let compact = "";
    const at: number[] = [];
    for (let i = 0; i < name.length; i++) {
      const c = fold(name[i]);
      if (/^[\p{L}\p{N}]+$/u.test(c)) {
        compact += c;
        for (let k = 0; k < c.length; k++) at.push(i);
      }
    }
    const hit = new Array<boolean>(name.length).fill(false);
    for (const w of q) {
      let i = compact.indexOf(w);
      while (i >= 0) {
        for (let k = i; k < i + w.length; k++) hit[at[k]] = true;
        i = compact.indexOf(w, i + w.length);
      }
    }
    // The spaces inside a marked run belong to it.
    for (let i = 1; i < name.length - 1; i++) if (!hit[i] && hit[i - 1] && hit[i + 1]) hit[i] = true;
    const out: { text: string; hit: boolean }[] = [];
    for (let i = 0; i < name.length; i++) {
      const last = out[out.length - 1];
      if (last && last.hit === hit[i]) last.text += name[i];
      else out.push({ text: name[i], hit: hit[i] });
    }
    return out;
  }

  function measureMeta(m: Model): string {
    if (m.measures.length > 1) return tn("eq.hpMeasures", m.measures.length);
    return m.measures[0]?.source ?? "";
  }

  async function open(m: Model) {
    if (m.measures.length === 1) return pick(m, m.measures[0]);
    chosen = m;
    active = 0;
    await tick();
    list?.querySelector<HTMLElement>("[role=option]")?.scrollIntoView({ block: "nearest" });
  }

  async function pick(m: Model, measure: Measure) {
    if (busy) return;
    busy = true;
    error = null;
    try {
      const res = measure.index !== undefined ? await api.eqAddCatalog(measure.index, m.name, measure.source) : await api.eqAddHeadphone(m.name);
      onpicked(res);
      onclose();
    } catch (e) {
      error = errorText(e);
    } finally {
      busy = false;
    }
  }

  async function back() {
    chosen = null;
    await tick();
    input?.focus();
  }

  let count = $derived(chosen ? chosen.measures.length : shown.length);

  async function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      if (chosen) back();
      else onclose();
      return;
    }
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!count) return;
      active = (active + (e.key === "ArrowDown" ? 1 : count - 1)) % count;
      await tick();
      list?.querySelector<HTMLElement>(`#hp-opt-${active}`)?.scrollIntoView({ block: "nearest" });
    } else if (e.key === "Enter") {
      e.preventDefault();
      if (chosen) {
        const m = chosen.measures[active];
        if (m) pick(chosen, m);
      } else if (shown[active]) open(shown[active]);
    } else if (e.key === "Backspace" && chosen) {
      e.preventDefault();
      back();
    }
  }
</script>

<div class="modal-bg" role="presentation" onclick={(e) => e.target === e.currentTarget && onclose()}>
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <div class="picker" role="dialog" aria-modal="true" aria-labelledby="hp-title" tabindex="-1" onkeydown={onKey}>
    <h2 id="hp-title">{t("eq.hpTitle")}</h2>

    {#if chosen}
      <div class="chosen">
        <button class="back" onclick={back} aria-label={t("eq.hpBack")} title={t("eq.hpBack")}>
          <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true"><path d="M9 2 4 7l5 5" fill="none" stroke="currentColor" stroke-width="1.6" /></svg>
        </button>
        <div>
          <p class="chosen-name">{chosen.name}</p>
          <p class="fine">{t("eq.hpWhich")}</p>
        </div>
      </div>
      <!-- svelte-ignore a11y_autofocus -->
      <ul class="list" role="listbox" aria-label={chosen.name} bind:this={list} tabindex="0" autofocus aria-activedescendant={`hp-opt-${active}`}>
        {#each chosen.measures as ms, i (ms.source + (ms.rig ?? "") + i)}
          <li id={`hp-opt-${i}`} role="option" aria-selected={i === active} class:active={i === active}>
            <button tabindex="-1" disabled={busy} onclick={() => pick(chosen!, ms)} onmouseenter={() => (active = i)}>
              <span class="name">{ms.source}{#if i === 0}<span class="best">{t("eq.hpBest")}</span>{/if}</span>
              {#if ms.rig}<span class="meta">{ms.rig}</span>{/if}
            </button>
          </li>
        {/each}
      </ul>
    {:else}
      <input
        bind:this={input}
        bind:value={query}
        class="search"
        type="search"
        role="combobox"
        aria-autocomplete="list"
        aria-expanded={shown.length > 0}
        aria-controls="hp-list"
        aria-activedescendant={shown.length ? `hp-opt-${active}` : undefined}
        aria-label={t("eq.hpSearch")}
        placeholder={catalog === "ready" ? t("eq.hpSearchAll", { n: num(modelCount) }) : t("eq.hpSearch")}
        autocomplete="off"
        autocorrect="off"
        spellcheck="false"
      />
      <p class="status fine" aria-live="polite">
        {#if !query.trim()}
          {t("eq.hpPopular")}
        {:else if matches.length > LIMIT}
          {t("eq.hpFirst", { n: num(LIMIT), total: num(matches.length) })}
        {:else if matches.length}
          {tn("eq.hpFound", matches.length)}
        {/if}
        {#if catalog === "loading"}<span class="loading">{t("eq.hpLoading")}</span>{/if}
      </p>
      <ul class="list" id="hp-list" role="listbox" aria-label={t("eq.hpTitle")} bind:this={list}>
        {#each shown as m, i (m.name)}
          <li id={`hp-opt-${i}`} role="option" aria-selected={i === active} class:active={i === active}>
            <button tabindex="-1" onclick={() => open(m)} onmouseenter={() => (active = i)}>
              <span class="name">{#each pieces(m.name) as p, k (k)}{#if p.hit}<mark>{p.text}</mark>{:else}{p.text}{/if}{/each}</span>
              <span class="meta">{measureMeta(m)}</span>
            </button>
          </li>
        {/each}
      </ul>
      {#if query.trim() && !matches.length}
        <p class="none">{t("eq.hpNone", { q: query.trim() })}{#if catalog !== "ready"} {t("eq.hpNoneOffline")}{/if}</p>
      {/if}
      {#if catalog === "failed"}
        <p class="offline">{t("eq.hpOffline")} <span class="detail">{catalogError}</span> <button class="link" onclick={() => loadCatalog(true)}>{t("eq.hpRetry")}</button></p>
      {/if}
    {/if}

    {#if error}<p class="err" role="alert">{error}</p>{/if}
    <div class="foot">
      <p class="fine">{t("eq.hpCredit")}</p>
      <button class="ghost" onclick={onclose}>{t("eq.cancel")}</button>
    </div>
  </div>
</div>

<style>
  .modal-bg {
    position: fixed;
    inset: 0;
    z-index: 60;
    display: grid;
    place-items: center;
    background: rgba(5, 12, 15, 0.6);
  }
  .picker {
    width: min(560px, calc(100vw - 32px));
    height: min(620px, calc(100vh - 48px));
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding: 18px 20px 14px;
    border-radius: var(--r-lg);
    background: var(--slate-700);
    border: 1px solid var(--slate-500);
    outline: none;
  }
  h2 {
    margin: 0;
    font-family: var(--font-display);
    font-size: var(--t-lg);
    font-weight: 600;
  }
  .fine {
    margin: 0;
    font-size: var(--t-xs);
    color: var(--mist);
    line-height: 1.4;
  }
  .search {
    font: inherit;
    font-size: var(--t-md);
    color: var(--porcelain);
    background: var(--slate-900);
    border: 1px solid var(--slate-600);
    border-radius: var(--r-md);
    padding: 10px 12px;
    user-select: text;
    -webkit-user-select: text;
  }
  .search:focus {
    outline: none;
    border-color: var(--mist);
    box-shadow: 0 0 0 3px rgba(232, 237, 234, 0.12);
  }
  .status {
    display: flex;
    justify-content: space-between;
    gap: 12px;
    min-height: 1.4em;
  }
  .loading {
    color: var(--mist-dim);
  }
  .list {
    flex: 1;
    min-height: 0;
    margin: 0;
    padding: 0;
    list-style: none;
    overflow-y: auto;
    border-top: 1px solid var(--slate-600);
    outline: none;
  }
  .list li button {
    width: 100%;
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 12px;
    padding: 8px 10px;
    border: 0;
    border-bottom: 1px solid var(--slate-600);
    background: none;
    color: var(--porcelain);
    font: inherit;
    font-size: var(--t-sm);
    text-align: left;
    cursor: pointer;
  }
  .list li.active button {
    background: var(--slate-600);
    box-shadow: inset 2px 0 0 var(--porcelain);
  }
  .name {
    min-width: 0;
  }
  mark {
    background: none;
    color: var(--porcelain);
    font-weight: 600;
  }
  .meta {
    flex-shrink: 0;
    color: var(--mist);
    font-size: var(--t-xs);
  }
  .best {
    margin-left: 8px;
    padding: 1px 6px;
    border-radius: var(--r-sm);
    border: 1px solid var(--mist);
    color: var(--porcelain);
    font-size: var(--t-xs);
  }
  .chosen {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .chosen-name {
    margin: 0;
    font-size: var(--t-md);
    font-weight: 600;
  }
  .back {
    display: grid;
    place-items: center;
    width: 30px;
    height: 30px;
    border-radius: var(--r-md);
    border: 1px solid var(--slate-500);
    background: var(--slate-800);
    color: var(--porcelain);
    cursor: pointer;
  }
  .none,
  .offline,
  .err {
    margin: 0;
    font-size: var(--t-sm);
    color: var(--mist);
  }
  .detail {
    color: var(--mist-dim);
  }
  .err {
    color: var(--alarm);
  }
  .link {
    border: 0;
    padding: 0;
    background: none;
    color: var(--porcelain);
    text-decoration: underline;
    text-underline-offset: 3px;
    font: inherit;
    text-decoration: underline;
    cursor: pointer;
  }
  .foot {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: 16px;
  }
  .ghost {
    flex-shrink: 0;
    font: inherit;
    font-size: var(--t-sm);
    padding: 6px 14px;
    border-radius: var(--r-md);
    border: 1px solid var(--slate-500);
    background: none;
    color: var(--porcelain);
    cursor: pointer;
  }
  button:focus-visible {
    outline: 2px solid var(--porcelain);
    outline-offset: 2px;
  }
</style>
