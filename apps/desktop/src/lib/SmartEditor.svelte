<script lang="ts">
  import type { SmartField, SmartOp, SmartRule, SmartRules, SmartSort } from "./api";
  import { t } from "./i18n.svelte";

  let {
    name: startName,
    rules: start,
    onsave,
    onclose,
  }: { name: string; rules: SmartRules | null; onsave: (name: string, rules: SmartRules) => void; onclose: () => void } = $props();

  type Kind = "text" | "number" | "days" | "flag" | "quality";
  const FIELDS: [SmartField, Kind][] = [
    ["genre", "text"],
    ["artist", "text"],
    ["albumArtist", "text"],
    ["album", "text"],
    ["title", "text"],
    ["year", "number"],
    ["added", "days"],
    ["hiRes", "flag"],
    ["lossless", "flag"],
    ["format", "text"],
    ["sampleRate", "number"],
    ["bits", "number"],
    ["duration", "number"],
    ["quality", "quality"],
  ];
  const kindOf = (f: SmartField) => FIELDS.find(([x]) => x === f)![1];
  const OPS: Record<Kind, SmartOp[]> = {
    text: ["contains", "notContains", "is", "isNot", "startsWith"],
    number: ["is", "greater", "less", "isNot"],
    days: ["withinDays"],
    flag: ["isTrue", "isFalse"],
    quality: ["is", "isNot"],
  };
  const QUALITY = ["genuine", "suspect", "unclear", "unchecked"];
  const SORTS: SmartSort[] = ["album", "artist", "title", "year", "added", "duration", "random"];

  // svelte-ignore state_referenced_locally
  let name = $state(startName);
  // svelte-ignore state_referenced_locally
  let matchAll = $state(start?.matchAll ?? true);
  // svelte-ignore state_referenced_locally
  let rules = $state<SmartRule[]>(start?.rules.map((r) => ({ ...r })) ?? [{ field: "genre", op: "contains", value: "" }]);
  // svelte-ignore state_referenced_locally
  let sort = $state<SmartSort>(start?.sort ?? "album");
  // svelte-ignore state_referenced_locally
  let limited = $state(start?.limit != null);
  // svelte-ignore state_referenced_locally
  let limit = $state(start?.limit ?? 100);

  function setField(r: SmartRule, f: SmartField) {
    r.field = f;
    const k = kindOf(f);
    if (!OPS[k].includes(r.op)) r.op = OPS[k][0];
    if (k === "quality") r.value = "suspect";
    else if (k === "days") r.value = r.value && /^\d+$/.test(r.value) ? r.value : "30";
    else if (k === "flag") r.value = "";
  }

  function add() {
    rules.push({ field: "genre", op: "contains", value: "" });
  }

  function save() {
    onsave(name.trim() || t("sp.defaultName"), {
      matchAll,
      rules: $state.snapshot(rules).filter((r) => kindOf(r.field) === "flag" || r.value.trim()),
      sort,
      limit: limited ? Math.max(1, Math.round(limit)) : null,
    });
  }
</script>

<svelte:window onkeydown={(e) => e.key === "Escape" && onclose()} />

<div class="bg" role="presentation" onclick={(e) => e.target === e.currentTarget && onclose()}>
  <div class="dialog" role="dialog" aria-modal="true" aria-labelledby="sp-title">
    <h3 id="sp-title">{t("sp.title")}</h3>
    <p class="sub">{t("sp.note")}</p>

    <label class="name">
      <span>{t("pl.name")}</span>
      <input bind:value={name} />
    </label>

    <div class="match">
      <span>{t("sp.match")}</span>
      <select bind:value={matchAll} aria-label={t("sp.match")}>
        <option value={true}>{t("sp.all")}</option>
        <option value={false}>{t("sp.any")}</option>
      </select>
    </div>

    <ul class="rules">
      {#each rules as r, i (i)}
        {@const k = kindOf(r.field)}
        <li>
          <select value={r.field} onchange={(e) => setField(r, e.currentTarget.value as SmartField)} aria-label={t("sp.field")}>
            {#each FIELDS as [f] (f)}<option value={f}>{t(`sp.f.${f}`)}</option>{/each}
          </select>
          <select bind:value={r.op} aria-label={t("sp.condition")}>
            {#each OPS[k] as o (o)}<option value={o}>{t(`sp.op.${o}`)}</option>{/each}
          </select>
          {#if k === "quality"}
            <select bind:value={r.value} aria-label={t("sp.value")}>
              {#each QUALITY as q (q)}<option value={q}>{t(`sp.q.${q}`)}</option>{/each}
            </select>
          {:else if k !== "flag"}
            <input bind:value={r.value} inputmode={k === "text" ? undefined : "decimal"} aria-label={t("sp.value")} />
          {:else}
            <span></span>
          {/if}
          <button class="x" onclick={() => rules.splice(i, 1)} aria-label={t("sp.remove")} title={t("sp.remove")}>×</button>
        </li>
      {/each}
    </ul>
    <button class="add" onclick={add}>+ {t("sp.add")}</button>

    <div class="more">
      <label>
        <span>{t("sp.sort")}</span>
        <select bind:value={sort}>
          {#each SORTS as s (s)}<option value={s}>{t(`sp.sort.${s}`)}</option>{/each}
        </select>
      </label>
      <label class="limit">
        <input type="checkbox" bind:checked={limited} />
        <span>{t("sp.limit")}</span>
        <input type="number" min="1" bind:value={limit} disabled={!limited} aria-label={t("sp.limit")} />
      </label>
    </div>

    <footer>
      <button class="ghost" onclick={onclose}>{t("pl.cancel")}</button>
      <button class="primary" onclick={save}>{t("tags.save")}</button>
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
    width: min(640px, 100%);
    max-height: calc(100dvh - 32px);
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 12px;
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
    margin: -6px 0 0;
    font-size: var(--t-sm);
    color: var(--mist);
  }
  input,
  select {
    font: inherit;
    font-size: var(--t-sm);
    color: var(--porcelain);
    background: var(--slate-700);
    border: 1px solid var(--slate-600);
    border-radius: var(--r-sm);
    padding: 6px 8px;
    min-width: 0;
  }
  input:focus,
  select:focus {
    border-color: var(--mist);
    outline: none;
  }
  .name {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-size: var(--t-xs);
    color: var(--mist);
  }
  .match {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: var(--t-sm);
  }
  .rules {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .rules li {
    display: grid;
    grid-template-columns: 1.1fr 1fr 1.2fr 28px;
    gap: 6px;
    align-items: center;
  }
  .x {
    color: var(--mist);
    font-size: 18px;
    border-radius: var(--r-sm);
  }
  .x:hover {
    color: var(--alarm);
  }
  .add {
    align-self: flex-start;
    color: var(--porcelain);
    font-size: var(--t-sm);
    font-weight: 600;
  }
  .more {
    display: flex;
    flex-wrap: wrap;
    gap: 12px 24px;
    padding-top: 10px;
    border-top: 1px solid var(--slate-600);
  }
  .more label {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: var(--t-sm);
  }
  .limit input[type="number"] {
    width: 80px;
  }
  .limit input[type="checkbox"] {
    accent-color: var(--porcelain);
  }
  footer {
    display: flex;
    justify-content: flex-end;
    gap: 10px;
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
  @media (max-width: 560px) {
    .rules li {
      grid-template-columns: 1fr 1fr 28px;
    }
    .rules li > :nth-child(3) {
      grid-column: 1 / 3;
    }
  }
</style>
