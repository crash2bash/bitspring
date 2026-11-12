<script lang="ts">
  import { onMount } from "svelte";
  import {
    api,
    errorText,
    inTauri,
    isAndroid,
    isIOS,
    type BandChannels,
    type Device,
    type DeviceEq,
    type EqBand,
    type EqChanged,
    type EqFormat,
    type EqPreset,
    type EqState,
    type FilterKind,
    type IrInfo,
    type Signal,
    type StoredPreset,
  } from "./api";
  import EqCurve from "./EqCurve.svelte";
  import { TEMPLATES, effectivePreamp, hasGain, hasSlope, maxBoostDb, newBand } from "./eqmath";
  import { list, num, t, tn } from "./i18n.svelte";

  let {
    devices,
    device,
    convolution,
    onsignal,
    onstate,
    onpickir,
    onclearir,
  }: {
    devices: Device[];
    /** The output picked in the device menu: what "this device" means here. */
    device: string;
    convolution: IrInfo | null;
    onsignal: (s: Signal) => void;
    /** Every new EQ state, for summaries elsewhere. */
    onstate?: (s: EqState) => void;
    onpickir: () => void;
    onclearir: () => void;
  } = $props();

  const phone = isIOS || isAndroid;

  let eqs = $state<EqState | null>(null);
  let draft = $state<EqPreset | null>(null);
  let selectedBand = $state<number | null>(null);
  let range = $state(12);
  let error = $state<string | null>(null);
  let note = $state<string | null>(null);
  let menu = $state<"new" | "import" | "export" | null>(null);
  let pasting = $state(false);
  let pasteText = $state("");
  let pasteName = $state("");
  let confirmDelete = $state(false);

  let deviceName = $derived(devices.find((d) => d.uid === device)?.name ?? "");
  let binding = $derived<DeviceEq | undefined>(eqs?.devices[device]);
  /** The preset this output plays — and the one the editor shows. */
  let editId = $derived.by(() => {
    if (!eqs) return null;
    if (binding?.mode === "preset") return binding.id;
    return eqs.activeId ?? eqs.presets[0]?.id ?? null;
  });
  let editing = $derived(eqs?.presets.find((p) => p.id === editId) ?? null);
  let heard = $derived(!!eqs && !!editing && (binding ? binding.mode === "preset" : eqs.enabled));

  // ---- eqs from the backend ---------------------------------------------------------------
  /** A plain deep copy (state proxies can't be structured-cloned). */
  const clone = <T,>(v: T): T => $state.snapshot(v) as T;

  function strip(p: StoredPreset): EqPreset {
    const { id: _id, updated: _u, ...rest } = p;
    return clone(rest);
  }

  function applyChanged(res: EqChanged, sync = true) {
    eqs = res.state;
    onstate?.(res.state);
    if (res.signal) onsignal(res.signal);
    if (sync) syncDraft();
  }

  let draftOf: string | null = null;
  function syncDraft() {
    const p = eqs?.presets.find((x) => x.id === editId);
    if (!p) {
      draft = null;
      draftOf = null;
      return;
    }
    if (draftOf !== p.id) {
      past = [];
      future = [];
      selectedBand = null;
    }
    draft = strip(p);
    draftOf = p.id;
    base = clone(draft);
  }

  async function run(fn: () => Promise<EqChanged>) {
    try {
      applyChanged(await fn());
      error = null;
    } catch (e) {
      error = errorText(e);
    }
  }

  onMount(async () => {
    try {
      eqs = await api.eqState();
      onstate?.(eqs);
      syncDraft();
    } catch (e) {
      error = errorText(e);
    }
  });

  // A different output was picked: show its preset.
  $effect(() => {
    void device;
    if (eqs && draftOf !== editId) syncDraft();
  });

  // ---- edits: applied as they happen, written when a gesture ends ---------------------------
  let past = $state<EqPreset[]>([]);
  let future = $state<EqPreset[]>([]);
  let base: EqPreset | null = null;
  let inflight = false;
  let queued: { p: EqPreset; persist: boolean } | null = null;

  async function push(p: EqPreset, persist: boolean) {
    if (inflight) {
      queued = { p, persist: persist || (queued?.persist ?? false) };
      return;
    }
    inflight = true;
    try {
      applyChanged(await api.eqSave(editId, p, persist), false);
      error = null;
    } catch (e) {
      error = errorText(e);
    }
    inflight = false;
    if (queued) {
      const q = queued;
      queued = null;
      void push(q.p, q.persist);
    }
  }

  function edit(p: EqPreset, final: boolean) {
    draft = p;
    if (final && base) {
      past = [...past.slice(-99), base];
      future = [];
      base = clone(p);
    }
    void push(clone(p), final);
  }

  function patch(over: Partial<EqPreset>) {
    if (draft) edit({ ...draft, ...over }, true);
  }

  function patchBand(i: number, over: Partial<EqBand>) {
    if (!draft) return;
    edit({ ...draft, bands: draft.bands.map((b, j) => (j === i ? { ...b, ...over } : b)) }, true);
  }

  function undo() {
    if (!past.length || !draft) return;
    future = [clone(draft), ...future];
    const prev = past[past.length - 1];
    past = past.slice(0, -1);
    base = clone(prev);
    draft = prev;
    void push(clone(prev), true);
  }

  function redo() {
    if (!future.length || !draft) return;
    past = [...past, clone(draft)];
    const next = future[0];
    future = future.slice(1);
    base = clone(next);
    draft = next;
    void push(clone(next), true);
  }

  function onKey(e: KeyboardEvent) {
    const mod = e.metaKey || e.ctrlKey;
    const target = e.target as HTMLElement;
    if (!mod || e.key.toLowerCase() !== "z" || target.closest("input, textarea")) return;
    e.preventDefault();
    if (e.shiftKey) redo();
    else undo();
  }

  // ---- presets ------------------------------------------------------------------------------
  async function choose(id: string) {
    if (binding?.mode === "preset") await run(() => api.eqBindDevice(device, { mode: "preset", id }));
    else {
      if (binding?.mode === "off") await run(() => api.eqBindDevice(device, null));
      await run(() => api.eqActivate(id));
    }
  }

  /** Make `preset` and show it on this output. */
  async function create(preset: EqPreset) {
    menu = null;
    const before = new Set(eqs?.presets.map((p) => p.id));
    try {
      const res = await api.eqSave(null, preset, true);
      const made = res.state.presets.find((p) => !before.has(p.id));
      applyChanged(res);
      if (made && binding?.mode === "preset") await run(() => api.eqBindDevice(device, { mode: "preset", id: made.id }));
      error = null;
    } catch (e) {
      error = errorText(e);
    }
  }

  function uniqueName(base: string) {
    const names = new Set(eqs?.presets.map((p) => p.name));
    if (!names.has(base)) return base;
    for (let i = 2; ; i++) if (!names.has(`${base} ${i}`)) return `${base} ${i}`;
  }

  async function duplicate() {
    if (draft) await create({ ...clone(draft), name: uniqueName(t("eq.copyName", { name: draft.name })) });
  }

  async function remove() {
    if (!editId) return;
    if (!confirmDelete) {
      confirmDelete = true;
      setTimeout(() => (confirmDelete = false), 3000);
      return;
    }
    confirmDelete = false;
    const id = editId;
    if (binding?.mode === "preset") await run(() => api.eqBindDevice(device, null));
    await run(() => api.eqDelete(id));
  }

  async function toggleOn() {
    if (!eqs) return;
    if (binding?.mode === "off") {
      await run(() => api.eqBindDevice(device, null));
      if (!eqs.enabled) await run(() => api.eqSetEnabled(true));
    } else if (binding?.mode === "preset") await run(() => api.eqBindDevice(device, { mode: "off" }));
    else await run(() => api.eqSetEnabled(!eqs!.enabled));
  }

  async function setBinding(v: string) {
    const b: DeviceEq | null = v === "off" ? { mode: "off" } : v === "own" && editId ? { mode: "preset", id: editId } : null;
    await run(() => api.eqBindDevice(device, b));
  }

  // ---- import / export ----------------------------------------------------------------------
  async function afterImport(res: EqChanged & { ids: string[] }) {
    applyChanged(res);
    if (res.ids[0] && binding?.mode === "preset") await run(() => api.eqBindDevice(device, { mode: "preset", id: res.ids[0] }));
    note = tn("eq.imported", res.ids.length);
  }

  async function importFile() {
    menu = null;
    try {
      if (!inTauri) return await afterImport(await api.eqImportFile("/Users/me/AutoEq/My Headphones ParametricEQ.txt"));
      const { open } = await import("@tauri-apps/plugin-dialog");
      const picked = await open({
        multiple: true,
        ...(isIOS ? {} : { filters: [{ name: t("eq.title"), extensions: ["txt", "cfg", "json", "csv", "tsv", "feq"] }] }),
      });
      if (!picked) return;
      for (const p of Array.isArray(picked) ? picked : [picked]) await afterImport(await api.eqImportFile(p));
      error = null;
    } catch (e) {
      error = errorText(e);
    }
  }

  async function importText() {
    try {
      await afterImport(await api.eqImportText(pasteText, pasteName.trim() || t("eq.defaultName")));
      pasting = false;
      pasteText = "";
      pasteName = "";
      error = null;
    } catch (e) {
      error = errorText(e);
    }
  }

  const FORMATS: { f: EqFormat; ext: string }[] = [
    { f: "apo", ext: "txt" },
    { f: "graphicEq", ext: "txt" },
    { f: "easyEffects", ext: "json" },
    { f: "csv", ext: "csv" },
    { f: "hifi", ext: "json" },
  ];

  async function exportAs(f: EqFormat, ext: string, all = false) {
    menu = null;
    const ids = all ? (eqs?.presets.map((p) => p.id) ?? []) : editId ? [editId] : [];
    if (!ids.length) return;
    try {
      if (phone || !inTauri) {
        const where = await api.eqExport(ids, f, null);
        note = t("eq.saved", { path: isIOS ? (where.split("/").pop() ?? where) : where });
        return;
      }
      const name = all ? "BitSpring EQ" : (draft?.name ?? "EQ");
      const suffix = f === "graphicEq" ? " GraphicEQ" : "";
      const { save } = await import("@tauri-apps/plugin-dialog");
      const path = await save({ defaultPath: `${name}${suffix}.${ext}`, filters: [{ name: ext.toUpperCase(), extensions: [ext] }] });
      if (!path) return;
      await api.eqExport(ids, f, path);
      note = t("eq.saved", { path });
    } catch (e) {
      error = errorText(e);
    }
  }

  async function copyText() {
    menu = null;
    if (!editId) return;
    try {
      await navigator.clipboard.writeText(await api.eqExportText([editId], "apo"));
      note = t("eq.copied");
    } catch (e) {
      error = errorText(e);
    }
  }

  // ---- display helpers ----------------------------------------------------------------------
  const KINDS: FilterKind[] = ["peak", "lowShelf", "highShelf", "lowPass", "highPass", "bandPass", "notch", "allPass"];
  const CHANNELS: BandChannels[] = ["both", "left", "right"];
  const SLOPES = [6, 12, 18, 24, 36, 48];

  let preamp = $derived(draft ? effectivePreamp(draft) : 0);
  let clips = $derived(!!draft && !draft.autoPreamp && maxBoostDb(draft) + draft.preamp > 0.05);
  const signed = (v: number, dp = 1) => `${v > 0 ? "+" : v < 0 ? "−" : ""}${num(Math.abs(v), { maximumFractionDigits: dp })}`;

  function parse(s: string): number | null {
    const v = parseFloat(s.replace(/\s/g, "").replace("−", "-").replace(",", "."));
    return Number.isFinite(v) ? v : null;
  }

  /** Commit a typed number; a "k" suffix means thousands (2.5k). */
  function numInput(e: Event, apply: (v: number) => void, k = false) {
    const el = e.currentTarget as HTMLInputElement;
    let raw = el.value.trim();
    let mult = 1;
    if (k && /k$/i.test(raw)) {
      mult = 1000;
      raw = raw.slice(0, -1);
    }
    const v = parse(raw);
    if (v === null) return;
    apply(v * mult);
  }

  function presetMeta(p: StoredPreset) {
    const parts = [tn("eq.bandsCount", p.bands.length)];
    if (p.curve.length) parts.push(t("eq.withCurve"));
    if (p.phase === "linear") parts.push(t("eq.phase.linearShort"));
    return list(parts);
  }

  function addBand() {
    if (!draft) return;
    // In the widest gap between existing bands.
    const fs = [20, ...draft.bands.map((b) => b.freq).sort((a, b) => a - b), 20000];
    let best = 1000;
    let gap = 0;
    for (let i = 1; i < fs.length; i++) {
      const g = Math.log(fs[i] / fs[i - 1]);
      if (g > gap) {
        gap = g;
        best = Math.sqrt(fs[i] * fs[i - 1]);
      }
    }
    const freq = best < 100 ? Math.round(best) : Math.round(best / 10) * 10;
    edit({ ...draft, bands: [...draft.bands, newBand({ freq, q: 1.41 })] }, true);
    selectedBand = draft.bands.length - 1;
  }

  function removeBand(i: number) {
    if (!draft) return;
    edit({ ...draft, bands: draft.bands.filter((_, j) => j !== i) }, true);
    selectedBand = null;
  }

  let rowEls: HTMLElement[] = $state([]);
  $effect(() => {
    if (selectedBand !== null) rowEls[selectedBand]?.scrollIntoView({ block: "nearest" });
  });

  function onWindowClick(e: MouseEvent) {
    if (menu && !(e.target as HTMLElement).closest(".menu-wrap")) menu = null;
  }
</script>

<svelte:window onkeydown={onKey} onclick={onWindowClick} />

<div class="equalizer">
  <aside class="presets" aria-label={t("eq.presets")}>
    <div class="p-head">
      <h2>{t("eq.presets")}</h2>
      <div class="p-tools">
        <div class="menu-wrap">
          <button class="tool" onclick={() => (menu = menu === "import" ? null : "import")} aria-expanded={menu === "import"}>{t("eq.import")}</button>
          {#if menu === "import"}
            <div class="menu" role="menu">
              {#if !isAndroid}<button role="menuitem" onclick={importFile}>{t("eq.importFile")}</button>{/if}
              <button role="menuitem" onclick={() => ((menu = null), (pasting = true))}>{t("eq.importPaste")}</button>
              <p class="menu-note">{t("eq.importHelp")}</p>
            </div>
          {/if}
        </div>
        <div class="menu-wrap">
          <button class="tool add" onclick={() => (menu = menu === "new" ? null : "new")} aria-expanded={menu === "new"} aria-label={t("eq.new")} title={t("eq.new")}>
            <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true"><path d="M6 1v10M1 6h10" stroke="currentColor" stroke-width="1.6" /></svg>
          </button>
          {#if menu === "new"}
            <div class="menu end" role="menu">
              <p class="menu-title">{t("eq.newFrom")}</p>
              {#each TEMPLATES as tpl (tpl.key)}
                <button role="menuitem" onclick={() => create(tpl.make(uniqueName(t(`eq.tpl.${tpl.key}`))))}>{t(`eq.tpl.${tpl.key}`)}</button>
              {/each}
            </div>
          {/if}
        </div>
      </div>
    </div>

    {#if eqs?.presets.length}
      <ul class="p-list">
        {#each eqs.presets as p (p.id)}
          <li>
            <button class="p-item" class:sel={p.id === editId} onclick={() => choose(p.id)} aria-current={p.id === editId ? "true" : undefined}>
              <span class="p-name">{p.name}</span>
              <span class="p-meta">{presetMeta(p)}</span>
              {#if p.id === editId && heard}<span class="p-live" title={t("eq.playing")} aria-label={t("eq.playing")}></span>{/if}
            </button>
          </li>
        {/each}
      </ul>
      <button class="backup" onclick={() => exportAs("hifi", "json", true)}>{t("eq.backup")}</button>
    {/if}
  </aside>

  <section class="editor" aria-label={t("eq.title")}>
    {#if eqs && !eqs.presets.length}
      <div class="empty">
        <h2>{t("eq.emptyTitle")}</h2>
        <p>{t("eq.emptyText")}</p>
        <div class="empty-actions">
          {#each TEMPLATES as tpl (tpl.key)}
            <button class="chip" onclick={() => create(tpl.make(t(`eq.tpl.${tpl.key}`)))}>{t(`eq.tpl.${tpl.key}`)}</button>
          {/each}
        </div>
        <div class="empty-actions">
          {#if !isAndroid}<button class="primary" onclick={importFile}>{t("eq.importFile")}</button>{/if}
          <button class="ghost" onclick={() => (pasting = true)}>{t("eq.importPaste")}</button>
        </div>
        <p class="fine">{t("eq.importHelp")}</p>
      </div>
    {:else if draft}
      <div class="head">
        <label class="switch">
          <input type="checkbox" role="switch" checked={heard} onchange={toggleOn} />
          <span class="knob" aria-hidden="true"></span>
          <span class="sr">{t("eq.switch")}</span>
        </label>
        <input
          class="name"
          value={draft.name}
          aria-label={t("eq.name")}
          onchange={(e) => patch({ name: e.currentTarget.value.trim() || draft!.name })}
          onkeydown={(e) => e.key === "Enter" && e.currentTarget.blur()}
        />
        <div class="head-tools">
          <button class="icon" onclick={undo} disabled={!past.length} aria-label={t("eq.undo")} title={t("eq.undo")}>
            <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d="M5 3L2 6l3 3M2.5 6H10a4 4 0 0 1 0 8H7" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" /></svg>
          </button>
          <button class="icon" onclick={redo} disabled={!future.length} aria-label={t("eq.redo")} title={t("eq.redo")}>
            <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d="M11 3l3 3-3 3M13.5 6H6a4 4 0 0 0 0 8h3" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" /></svg>
          </button>
          <div class="menu-wrap">
            <button class="tool" onclick={() => (menu = menu === "export" ? null : "export")} aria-expanded={menu === "export"}>{t("eq.export")}</button>
            {#if menu === "export"}
              <div class="menu end" role="menu">
                {#each FORMATS as { f, ext } (f)}
                  <button role="menuitem" onclick={() => exportAs(f, ext)}>{t(`eq.fmt.${f}`)}</button>
                {/each}
                <button role="menuitem" onclick={copyText}>{t("eq.copy")}</button>
              </div>
            {/if}
          </div>
          <button class="icon" onclick={duplicate} aria-label={t("eq.duplicate")} title={t("eq.duplicate")}>
            <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><rect x="5" y="5" width="9" height="9" rx="1.5" fill="none" stroke="currentColor" stroke-width="1.4" /><path d="M11 2.5H3.5a1 1 0 0 0-1 1V11" fill="none" stroke="currentColor" stroke-width="1.4" /></svg>
          </button>
          <button class="icon danger" class:armed={confirmDelete} onclick={remove} aria-label={confirmDelete ? t("eq.deleteConfirm", { name: draft.name }) : t("eq.delete")} title={confirmDelete ? t("eq.deleteConfirm", { name: draft.name }) : t("eq.delete")}>
            {#if confirmDelete}
              <span class="confirm">{t("eq.deleteShort")}</span>
            {:else}
              <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><path d="M3 4.5h10M6.5 4.5V3h3v1.5M4.5 4.5l.7 9h5.6l.7-9" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" /></svg>
            {/if}
          </button>
        </div>
      </div>

      <div class="modes">
        <button
          class="compare"
          class:on={eqs?.bypass}
          aria-pressed={eqs?.bypass}
          disabled={!heard}
          title={t("eq.compareHelp")}
          onclick={() => run(() => api.eqSetBypass(!eqs!.bypass))}
        >
          {t("eq.compare")}
        </button>
        <div class="seg" role="radiogroup" aria-label={t("eq.phase")}>
          {#each ["minimum", "linear"] as const as ph (ph)}
            <button role="radio" aria-checked={draft.phase === ph} class:sel={draft.phase === ph} title={t(`eq.phaseHelp.${ph}`)} onclick={() => patch({ phase: ph })}>
              {t(`eq.phase.${ph}`)}
            </button>
          {/each}
        </div>
        <p class="status" class:bypassing={eqs?.bypass && heard} class:off={!heard} role="status">
          {#if !heard}
            {binding?.mode === "off" ? t("eq.offForDevice", { device: deviceName }) : t("eq.offAll")}
          {:else if eqs?.bypass}
            {t("eq.comparing")}
          {:else}
            {t(draft.phase === "linear" ? "eq.phaseHelp.linear" : "eq.phaseHelp.minimum")}
          {/if}
        </p>
      </div>

      <div class="graph">
        <EqCurve preset={draft} selected={selectedBand} {range} dim={!heard || !!eqs?.bypass} onchange={edit} onselect={(i) => (selectedBand = i)} />
      </div>

      <div class="strip">
        <div class="field">
          <span class="label" id="eq-pre">{t("eq.preamp")}</span>
          <input
            class="numf"
            aria-labelledby="eq-pre"
            value={signed(preamp)}
            disabled={draft.autoPreamp}
            onchange={(e) => numInput(e, (v) => patch({ preamp: Math.max(-60, Math.min(30, v)) }))}
          />
          <span class="unit">{t("unit.db")}</span>
          <label class="check" title={t("eq.autoHelp")}>
            <input type="checkbox" checked={draft.autoPreamp} onchange={(e) => patch({ autoPreamp: e.currentTarget.checked, preamp: e.currentTarget.checked ? draft!.preamp : +preamp.toFixed(1) })} />
            {t("eq.auto")}
          </label>
        </div>
        <div class="field">
          <span class="label" id="eq-bal">{t("eq.balance")}</span>
          <span class="lr" aria-hidden="true">L</span>
          <input
            type="range"
            class="balance"
            min="-6"
            max="6"
            step="0.1"
            dir="ltr"
            aria-labelledby="eq-bal"
            aria-valuetext={draft.balance === 0 ? t("eq.centre") : t(draft.balance > 0 ? "eq.towardsRight" : "eq.towardsLeft", { db: num(Math.abs(draft.balance)) })}
            value={-draft.balance}
            oninput={(e) => edit({ ...draft!, balance: Math.abs(+e.currentTarget.value) < 0.15 ? 0 : -e.currentTarget.value }, false)}
            onchange={(e) => edit({ ...draft!, balance: Math.abs(+e.currentTarget.value) < 0.15 ? 0 : -e.currentTarget.value }, true)}
          />
          <span class="lr" aria-hidden="true">R</span>
          <span class="bal-v num">{draft.balance === 0 ? "0" : `${draft.balance > 0 ? "R" : "L"} ${num(Math.abs(draft.balance))}`}</span>
        </div>
        <div class="field">
          <span class="label" id="eq-range">{t("eq.range")}</span>
          <div class="seg small" role="radiogroup" aria-labelledby="eq-range">
            {#each [6, 12, 24] as r (r)}
              <button role="radio" aria-checked={range === r} class:sel={range === r} onclick={() => (range = r)}>±{r}</button>
            {/each}
          </div>
        </div>
        {#if deviceName}
          <div class="field device">
            <span class="label" id="eq-dev">{t("eq.device", { device: deviceName })}</span>
            <select aria-labelledby="eq-dev" value={binding?.mode === "off" ? "off" : binding?.mode === "preset" ? "own" : "follow"} onchange={(e) => setBinding(e.currentTarget.value)}>
              <option value="follow">{t("eq.dev.follow")}</option>
              <option value="own">{t("eq.dev.own")}</option>
              <option value="off">{t("eq.dev.off")}</option>
            </select>
          </div>
        {/if}
      </div>
      {#if clips}<p class="warn">{t("eq.clipWarn")}</p>{/if}

      <div class="bands">
        {#if draft.curve.length}
          <div class="layer">
            <span>{tn("eq.curveLayer", draft.curve.length)}</span>
            <button class="link" onclick={() => patch({ curve: [] })}>{t("eq.curveRemove")}</button>
          </div>
        {/if}
        {#if draft.bands.length}
          <table>
            <thead>
              <tr>
                <th class="c-on"><span class="sr">{t("eq.col.on")}</span></th>
                <th class="c-n">#</th>
                <th>{t("eq.col.type")}</th>
                <th>{t("eq.col.channels")}</th>
                <th class="r">{t("eq.col.freq")}</th>
                <th class="r">{t("eq.col.gain")}</th>
                <th class="r">{t("eq.col.q")}</th>
                <th class="c-x"><span class="sr">{t("eq.removeBand", { n: "" })}</span></th>
              </tr>
            </thead>
            <tbody>
              {#each draft.bands as b, i (i)}
                <tr bind:this={rowEls[i]} class:sel={selectedBand === i} class:off={!b.enabled} onpointerdown={() => (selectedBand = i)}>
                  <td class="c-on">
                    <input type="checkbox" checked={b.enabled} aria-label={t("eq.bandOn", { n: i + 1 })} onchange={(e) => patchBand(i, { enabled: e.currentTarget.checked })} />
                  </td>
                  <td class="c-n num">{i + 1}</td>
                  <td>
                    <select value={b.kind} aria-label={t("eq.col.type")} onchange={(e) => patchBand(i, { kind: e.currentTarget.value as FilterKind })}>
                      {#each KINDS as k (k)}<option value={k}>{t(`eq.kind.${k}`)}</option>{/each}
                    </select>
                  </td>
                  <td>
                    <select value={b.channels} aria-label={t("eq.col.channels")} onchange={(e) => patchBand(i, { channels: e.currentTarget.value as BandChannels })}>
                      {#each CHANNELS as c (c)}<option value={c}>{t(`eq.ch.${c}`)}</option>{/each}
                    </select>
                  </td>
                  <td class="r">
                    <input class="numf" value={num(b.freq, { maximumFractionDigits: 1 })} aria-label={t("eq.col.freq")} onchange={(e) => numInput(e, (v) => patchBand(i, { freq: Math.max(10, Math.min(40000, v)) }), true)} /><span class="unit">{t("unit.hz")}</span>
                  </td>
                  <td class="r">
                    {#if hasGain(b.kind)}
                      <input class="numf" value={signed(b.gain)} aria-label={t("eq.col.gain")} onchange={(e) => numInput(e, (v) => patchBand(i, { gain: Math.max(-30, Math.min(30, v)) }))} /><span class="unit">{t("unit.db")}</span>
                    {:else}
                      <span class="na">—</span>
                    {/if}
                  </td>
                  <td class="r">
                    {#if hasSlope(b.kind)}
                      <select value={b.slope} aria-label={t("eq.col.slope")} onchange={(e) => patchBand(i, { slope: +e.currentTarget.value })}>
                        {#each SLOPES as s (s)}<option value={s}>{t("eq.slopeValue", { db: s })}</option>{/each}
                      </select>
                      {#if b.slope === 12}
                        <span class="qlab" aria-hidden="true">Q</span><input class="numf q" value={num(b.q, { maximumFractionDigits: 3 })} aria-label={t("eq.col.q")} onchange={(e) => numInput(e, (v) => patchBand(i, { q: Math.max(0.05, Math.min(50, v)) }))} />
                      {/if}
                    {:else}
                      <span class="qlab" aria-hidden="true">Q</span><input class="numf q" value={num(b.q, { maximumFractionDigits: 3 })} aria-label={t("eq.col.q")} onchange={(e) => numInput(e, (v) => patchBand(i, { q: Math.max(0.05, Math.min(50, v)) }))} />
                    {/if}
                  </td>
                  <td class="c-x">
                    <button class="x" onclick={() => removeBand(i)} aria-label={t("eq.removeBand", { n: i + 1 })}>
                      <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true"><path d="M1 1l8 8M9 1l-8 8" stroke="currentColor" stroke-width="1.5" /></svg>
                    </button>
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        {:else}
          <p class="fine">{t("eq.noBands")}</p>
        {/if}
        <button class="add-band" onclick={addBand}>{t("eq.addBand")}</button>

        {#if !isAndroid}
        <div class="ir">
          <h3>{t("eq.ir")}</h3>
          {#if convolution}
            <p class="ir-name">
              <span>{convolution.name}</span>
              <span class="ir-meta num">{list([t(convolution.channels === 1 ? "eq.mono" : "eq.stereo"), t("eq.seconds", { s: num(convolution.seconds, { maximumFractionDigits: 2 }) }), `${num(convolution.rate / 1000)} ${t("unit.khz")}`])}</span>
              <button class="link" onclick={onclearir}>{t("eq.irRemove")}</button>
            </p>
          {:else}
            <button class="ghost" onclick={onpickir}>{t("eq.irLoad")}</button>
          {/if}
          <p class="fine">{t("eq.irHelp")}</p>
        </div>
        {/if}
      </div>
    {/if}

    {#if error}<p class="err" role="alert">{error}</p>{/if}
    {#if note}<p class="note" role="status">{note} <button class="link" onclick={() => (note = null)}>{t("eq.dismiss")}</button></p>{/if}
  </section>
</div>

{#if pasting}
  <div class="modal-bg" role="presentation" onclick={(e) => e.target === e.currentTarget && (pasting = false)}>
    <div class="modal" role="dialog" aria-modal="true" aria-labelledby="eq-paste-title">
    <form class="modal-form" onsubmit={(e) => (e.preventDefault(), importText())}>
      <h2 id="eq-paste-title">{t("eq.pasteTitle")}</h2>
      <p class="fine">{t("eq.pasteHint")}</p>
      <input bind:value={pasteName} placeholder={t("eq.defaultName")} aria-label={t("eq.name")} />
      <!-- svelte-ignore a11y_autofocus -->
      <textarea bind:value={pasteText} rows="9" spellcheck="false" autofocus placeholder={"Preamp: -6.4 dB\nFilter 1: ON LSC Fc 105 Hz Gain 6.0 dB Q 0.70\nFilter 2: ON PK Fc 2700 Hz Gain -3.1 dB Q 1.41"}></textarea>
      <div class="modal-row">
        <button type="button" class="ghost" onclick={() => (pasting = false)}>{t("eq.cancel")}</button>
        <button type="submit" class="primary" disabled={!pasteText.trim()}>{t("eq.pasteImport")}</button>
      </div>
    </form>
    </div>
  </div>
{/if}

<style>
  .equalizer {
    flex: 1;
    min-height: 0;
    display: grid;
    grid-template-columns: 236px minmax(0, 1fr);
  }

  /* ---- preset list ---- */
  .presets {
    display: flex;
    flex-direction: column;
    min-height: 0;
    border-inline-end: 1px solid var(--slate-600);
    background: var(--slate-800);
  }
  .p-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 16px 12px 8px 16px;
  }
  h2 {
    margin: 0;
    font-size: var(--t-md);
    font-weight: 600;
  }
  .p-tools {
    display: flex;
    gap: 4px;
  }
  .tool {
    padding: 5px 10px;
    border-radius: 6px;
    font-size: var(--t-sm);
    color: var(--mist);
    border: 1px solid var(--slate-600);
  }
  .tool:hover {
    color: var(--porcelain);
    border-color: var(--slate-500);
  }
  .tool.add {
    width: 30px;
    display: grid;
    place-items: center;
    padding: 5px 0;
  }
  .p-list {
    list-style: none;
    margin: 0;
    padding: 4px 8px;
    overflow-y: auto;
    flex: 1;
  }
  .p-item {
    position: relative;
    width: 100%;
    display: flex;
    flex-direction: column;
    gap: 1px;
    padding: 8px 10px;
    border-radius: 8px;
    text-align: start;
  }
  .p-item:hover {
    background: var(--slate-700);
  }
  .p-item.sel {
    background: var(--slate-700);
    box-shadow: inset 3px 0 0 var(--violet);
  }
  :global([dir="rtl"]) .p-item.sel {
    box-shadow: inset -3px 0 0 var(--violet);
  }
  .p-name {
    font-weight: 600;
    font-size: var(--t-sm);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    padding-inline-end: 14px;
  }
  .p-meta {
    font-size: var(--t-xs);
    color: var(--mist);
  }
  .p-live {
    position: absolute;
    top: 12px;
    inset-inline-end: 10px;
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--violet);
    box-shadow: 0 0 8px var(--violet);
  }
  .backup {
    margin: 8px 16px 14px;
    font-size: var(--t-xs);
    color: var(--mist);
    text-align: start;
  }
  .backup:hover {
    color: var(--porcelain);
  }

  /* ---- menus ---- */
  .menu-wrap {
    position: relative;
  }
  .menu {
    position: absolute;
    top: calc(100% + 6px);
    inset-inline-start: 0;
    z-index: 30;
    min-width: 230px;
    padding: 6px;
    border-radius: 10px;
    background: var(--slate-700);
    border: 1px solid var(--slate-500);
    box-shadow: 0 18px 40px rgba(5, 12, 15, 0.55);
  }
  .menu.end {
    inset-inline-start: auto;
    inset-inline-end: 0;
  }
  .menu button {
    display: block;
    width: 100%;
    padding: 7px 10px;
    border-radius: 6px;
    font-size: var(--t-sm);
    text-align: start;
    white-space: nowrap;
  }
  .menu button:hover {
    background: var(--slate-600);
  }
  .menu-title {
    margin: 4px 10px 6px;
    font-size: var(--t-xs);
    color: var(--mist);
  }
  .menu-note {
    margin: 6px 10px 4px;
    font-size: var(--t-xs);
    color: var(--mist);
    line-height: 1.45;
    max-width: 240px;
  }

  /* ---- editor ---- */
  .editor {
    min-height: 0;
    min-width: 0;
    overflow-y: auto;
    padding: 14px 22px 24px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .head {
    display: flex;
    align-items: center;
    gap: 12px;
    flex-wrap: wrap;
  }
  .name {
    flex: 1;
    min-width: 140px;
    font: inherit;
    font-family: var(--font-display);
    font-size: var(--t-xl);
    font-weight: 600;
    color: var(--porcelain);
    background: none;
    border: 1px solid transparent;
    border-radius: 6px;
    padding: 2px 6px;
    margin-inline-start: -6px;
    user-select: text;
    -webkit-user-select: text;
  }
  .name:hover {
    border-color: var(--slate-600);
  }
  .name:focus {
    outline: none;
    border-color: var(--slate-500);
    background: var(--slate-900);
  }
  .head-tools {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-wrap: wrap;
  }
  .compare {
    padding: 6px 14px;
    border-radius: var(--r-md);
    border: 1px solid var(--slate-500);
    font-size: var(--t-sm);
    font-weight: 600;
  }
  .compare:hover:not(:disabled) {
    border-color: var(--brass);
  }
  .compare.on {
    background: var(--brass);
    border-color: var(--brass);
    color: var(--slate-900);
  }
  .compare:disabled,
  .icon:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .icon {
    width: 32px;
    height: 32px;
    display: grid;
    place-items: center;
    border-radius: 6px;
    color: var(--mist);
  }
  .icon:hover:not(:disabled) {
    color: var(--porcelain);
    background: var(--slate-700);
  }
  .icon.danger.armed {
    width: auto;
    padding: 0 10px;
    color: var(--slate-900);
    background: var(--alarm);
  }
  .confirm {
    font-size: var(--t-xs);
    font-weight: 600;
  }
  .seg {
    display: inline-grid;
    grid-auto-flow: column;
    background: var(--slate-900);
    border-radius: var(--r-md);
    padding: 3px;
    gap: 3px;
  }
  .seg button {
    padding: 4px 10px;
    border-radius: 6px;
    font-size: var(--t-sm);
    color: var(--mist);
    white-space: nowrap;
  }
  .seg.small button {
    padding: 2px 8px;
    font-family: var(--font-display);
    font-variant-numeric: tabular-nums;
  }
  .seg button:hover {
    color: var(--porcelain);
  }
  .seg button.sel {
    background: var(--slate-600);
    color: var(--porcelain);
    font-weight: 600;
  }
  .modes {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 8px 12px;
  }
  .status {
    flex: 1;
    min-width: 220px;
    margin: 0;
    font-size: var(--t-xs);
    line-height: 1.4;
    color: var(--mist);
  }
  .status.bypassing {
    color: var(--brass);
  }
  .status.off {
    color: var(--mist-dim);
  }

  .graph {
    height: clamp(220px, 42vh, 460px);
    flex: none;
  }

  /* switch */
  .switch {
    position: relative;
    display: inline-flex;
    cursor: pointer;
  }
  .switch input {
    position: absolute;
    opacity: 0;
    width: 1px;
    height: 1px;
  }
  .knob {
    position: relative;
    width: 38px;
    height: 22px;
    border-radius: 11px;
    background: var(--slate-900);
    border: 1px solid var(--slate-500);
    transition: background 120ms ease, border-color 120ms ease;
  }
  .knob::after {
    content: "";
    position: absolute;
    top: 3px;
    inset-inline-start: 3px;
    width: 14px;
    height: 14px;
    border-radius: 50%;
    background: var(--mist);
    transition: transform 120ms ease, background 120ms ease;
  }
  .switch input:checked + .knob {
    background: rgba(140, 122, 230, 0.25);
    border-color: var(--violet);
  }
  .switch input:checked + .knob::after {
    transform: translateX(16px);
    background: var(--violet);
  }
  :global([dir="rtl"]) .switch input:checked + .knob::after {
    transform: translateX(-16px);
  }
  .switch input:focus-visible + .knob {
    outline: 2px solid var(--brass);
    outline-offset: 2px;
  }

  /* strip */
  .strip {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 10px 26px;
    padding: 4px 0;
  }
  .field {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: var(--t-sm);
  }
  .label {
    color: var(--mist);
  }
  .unit {
    color: var(--mist);
    font-size: var(--t-xs);
    margin-inline-start: 3px;
  }
  .numf {
    width: 64px;
    padding: 4px 6px;
    border-radius: 5px;
    border: 1px solid var(--slate-600);
    background: var(--slate-900);
    color: var(--porcelain);
    font: inherit;
    font-family: var(--font-display);
    font-variant-numeric: tabular-nums;
    text-align: end;
    user-select: text;
    -webkit-user-select: text;
  }
  .numf:focus {
    outline: none;
    border-color: var(--violet);
  }
  .numf:disabled {
    color: var(--mist);
    border-color: transparent;
    background: none;
  }
  .numf.q {
    width: 54px;
  }
  .qlab {
    display: none;
    margin-inline-end: 6px;
    font-family: var(--font-display);
    font-size: var(--t-xs);
    color: var(--mist);
  }
  .check {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    cursor: pointer;
    color: var(--mist);
  }
  .check input {
    accent-color: var(--violet);
  }
  .balance {
    width: 120px;
    accent-color: var(--violet);
  }
  .lr {
    font-family: var(--font-display);
    font-size: var(--t-xs);
    color: var(--mist-dim);
  }
  .bal-v {
    min-width: 40px;
    color: var(--mist);
  }
  select {
    font: inherit;
    font-size: var(--t-sm);
    color: var(--porcelain);
    background: var(--slate-900);
    border: 1px solid var(--slate-600);
    border-radius: 5px;
    padding: 3px 6px;
  }
  .warn {
    margin: 0;
    font-size: var(--t-xs);
    color: var(--alarm);
  }

  /* bands */
  .bands {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .layer {
    display: flex;
    align-items: center;
    gap: 14px;
    font-size: var(--t-sm);
    color: var(--mist);
  }
  table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--t-sm);
  }
  th {
    font-weight: 500;
    color: var(--mist-dim);
    font-size: var(--t-xs);
    text-align: start;
    padding: 4px 6px;
    border-bottom: 1px solid var(--slate-600);
  }
  th.r,
  td.r {
    text-align: end;
    white-space: nowrap;
  }
  td {
    padding: 4px 6px;
    border-bottom: 1px solid var(--slate-700);
  }
  tr.sel td {
    background: rgba(140, 122, 230, 0.1);
  }
  tr.off td:not(.c-on) {
    opacity: 0.5;
  }
  .c-on,
  .c-x {
    width: 28px;
  }
  .c-on input {
    accent-color: var(--violet);
  }
  .c-n {
    width: 26px;
    color: var(--mist);
  }
  .na {
    color: var(--mist-dim);
    padding-inline-end: 22px;
  }
  .x {
    width: 24px;
    height: 24px;
    display: grid;
    place-items: center;
    border-radius: 5px;
    color: var(--mist-dim);
  }
  .x:hover {
    color: var(--alarm);
    background: var(--slate-700);
  }
  .add-band {
    align-self: flex-start;
    padding: 7px 14px;
    border-radius: var(--r-md);
    border: 1px dashed var(--slate-500);
    font-size: var(--t-sm);
  }
  .add-band:hover {
    border-color: var(--mist);
  }

  .ir {
    margin-top: 14px;
    padding-top: 14px;
    border-top: 1px solid var(--slate-600);
  }
  h3 {
    margin: 0 0 8px;
    font-size: var(--t-sm);
    font-weight: 600;
  }
  .ir-name {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 6px 14px;
    margin: 0;
    font-size: var(--t-sm);
  }
  .ir-meta {
    color: var(--mist);
  }

  .fine {
    margin: 6px 0 0;
    font-size: var(--t-xs);
    line-height: 1.5;
    color: var(--mist);
    max-width: 68ch;
  }
  .link {
    font-size: var(--t-sm);
    color: var(--mist);
    text-decoration: underline;
    text-decoration-color: var(--slate-500);
    text-underline-offset: 3px;
  }
  .link:hover {
    color: var(--porcelain);
  }
  .ghost {
    padding: 8px 16px;
    border-radius: var(--r-md);
    border: 1px solid var(--slate-500);
    font-weight: 600;
    font-size: var(--t-sm);
  }
  .ghost:hover {
    border-color: var(--mist);
  }
  .primary {
    padding: 8px 18px;
    border-radius: var(--r-md);
    background: var(--brass);
    color: var(--slate-900);
    font-weight: 600;
    font-size: var(--t-sm);
  }
  .primary:disabled {
    opacity: 0.5;
  }
  .chip {
    padding: 7px 14px;
    border-radius: 999px;
    border: 1px solid var(--slate-500);
    font-size: var(--t-sm);
  }
  .chip:hover {
    border-color: var(--violet);
  }
  .err {
    margin: 0;
    color: var(--alarm);
    font-size: var(--t-sm);
  }
  .note {
    margin: 0;
    color: var(--brass);
    font-size: var(--t-sm);
    overflow-wrap: anywhere;
  }

  .empty {
    max-width: 560px;
    margin: 8vh auto 0;
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  .empty h2 {
    font-family: var(--font-display);
    font-size: var(--t-2xl);
    font-weight: 600;
    line-height: 1.1;
  }
  .empty > p {
    margin: 0;
    color: var(--mist);
  }
  .empty-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }

  .modal-bg {
    position: fixed;
    inset: 0;
    z-index: 60;
    display: grid;
    place-items: center;
    background: rgba(5, 12, 15, 0.6);
  }
  .modal {
    width: min(560px, calc(100vw - 32px));
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding: 18px 20px;
    border-radius: 12px;
    background: var(--slate-700);
    border: 1px solid var(--slate-500);
  }
  .modal-form {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .modal input,
  .modal textarea {
    font: inherit;
    font-size: var(--t-sm);
    color: var(--porcelain);
    background: var(--slate-900);
    border: 1px solid var(--slate-600);
    border-radius: 6px;
    padding: 8px 10px;
    user-select: text;
    -webkit-user-select: text;
  }
  .modal textarea {
    font-family: ui-monospace, "SF Mono", Menlo, monospace;
    font-size: var(--t-xs);
    resize: vertical;
  }
  .modal-row {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }
  .sr {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip: rect(0 0 0 0);
    margin: -1px;
  }

  @media (prefers-reduced-motion: reduce) {
    .knob,
    .knob::after {
      transition: none;
    }
  }

  /* phone: presets above the editor */
  @media (max-width: 700px), (max-height: 500px) {
    .equalizer {
      grid-template-columns: 1fr;
      grid-template-rows: auto minmax(0, 1fr);
      overflow-y: auto;
    }
    .presets {
      border-inline-end: 0;
      border-bottom: 1px solid var(--slate-600);
    }
    .p-list {
      display: flex;
      gap: 6px;
      overflow-x: auto;
      padding: 0 12px 10px;
    }
    .p-item {
      width: auto;
      min-width: 140px;
      background: var(--slate-700);
    }
    .backup {
      display: none;
    }
    .editor {
      overflow: visible;
      padding: 12px 14px 24px;
    }
    .graph {
      height: 240px;
    }
    .name {
      font-size: var(--t-lg);
    }
    table thead {
      display: none;
    }
    table,
    tbody,
    tr {
      display: block;
    }
    tr {
      display: grid;
      grid-template-columns: 28px 22px 1fr 1fr 28px;
      grid-template-areas: "on n type ch x" ". . freq gain ." ". . q q .";
      gap: 4px 6px;
      padding: 8px 0;
      border-bottom: 1px solid var(--slate-700);
    }
    td {
      border: 0;
      padding: 0;
    }
    td:nth-child(1) { grid-area: on; }
    td:nth-child(2) { grid-area: n; }
    td:nth-child(3) { grid-area: type; }
    td:nth-child(4) { grid-area: ch; }
    td:nth-child(5) { grid-area: freq; text-align: start; }
    td:nth-child(6) { grid-area: gain; text-align: start; }
    td:nth-child(7) { grid-area: q; text-align: start; }
    .qlab {
      display: inline;
    }
    td:nth-child(8) { grid-area: x; }
  }
</style>
