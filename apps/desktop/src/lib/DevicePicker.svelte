<script lang="ts">
  import { api, deviceLabel, isAndroid, isIOS, khz, transportLabel, type Device } from "./api";
  import { list, t } from "./i18n.svelte";

  let {
    devices,
    selected = $bindable(),
    exclusive = $bindable(),
    dopDevices = $bindable(),
    onrefresh,
    onchanneltest,
  }: {
    devices: Device[];
    selected: string;
    exclusive: boolean;
    dopDevices: string[];
    onrefresh: () => void;
    onchanneltest: () => void;
  } = $props();

  let open = $state(false);
  let root: HTMLElement;
  let current = $derived(devices.find((d) => d.uid === selected));

  let local = $derived(devices.filter((d) => !d.network));
  let network = $derived(devices.filter((d) => d.network));

  function toggle() {
    open = !open;
    if (open) onrefresh();
  }

  // Network players answer a few seconds after a search: keep the list current while open.
  $effect(() => {
    if (!open || isIOS) return;
    const timer = setInterval(onrefresh, 3000);
    return () => clearInterval(timer);
  });

  // A network player's own volume, when it has one.
  let volume = $state<number | null>(null);
  $effect(() => {
    if (!open || !current?.network) {
      volume = null;
      return;
    }
    api.outputVolume(current.uid).then((v) => (volume = v)).catch(() => (volume = null));
  });
  let volumeTimer: ReturnType<typeof setTimeout> | undefined;
  function setVolume(v: number) {
    volume = v;
    const uid = current?.uid;
    clearTimeout(volumeTimer);
    if (uid) volumeTimer = setTimeout(() => api.setOutputVolume(uid, v), 120);
  }

  function choose(uid: string) {
    selected = uid;
    open = false;
  }

  let dop = $derived(current ? dopDevices.includes(current.uid) : false);
  function setDop(on: boolean) {
    if (!current) return;
    const uid = current.uid;
    dopDevices = on ? [...dopDevices.filter((u) => u !== uid), uid] : dopDevices.filter((u) => u !== uid);
  }

  function maxRate(d: Device) {
    return d.rates.length ? t("dev.upTo", { rate: khz(Math.max(...d.rates)) }) : "";
  }

  function onWindowClick(e: MouseEvent) {
    if (open && root && !e.composedPath().includes(root)) open = false;
  }
  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") open = false;
  }
</script>

<svelte:window onclick={onWindowClick} onkeydown={onKey} />

<div class="picker" bind:this={root}>
  <button class="trigger" onclick={toggle} aria-expanded={open} aria-haspopup="dialog">
    <span class="dot" class:warn={current && !current.bitPerfectCapable}></span>
    <span class="name">{current ? deviceLabel(current.name) : t("dev.choose")}</span>
    {#if current && current.nominalRate > 0}<span class="rate num">{khz(current.nominalRate)} {t("unit.khz")}</span>{/if}
    <svg width="10" height="6" viewBox="0 0 10 6" aria-hidden="true"><path d="M1 1l4 4 4-4" fill="none" stroke="currentColor" stroke-width="1.5" /></svg>
  </button>

  {#if open}
    <div class="panel" role="dialog" aria-label={t("dev.dialog")}>
      {#snippet item(d: Device)}
        <li>
          <button
            class="device"
            class:on={d.uid === selected}
            role="option"
            aria-selected={d.uid === selected}
            onclick={() => choose(d.uid)}
          >
            <span class="d-name">{d.isDefault ? t("dev.default", { name: deviceLabel(d.name) }) : deviceLabel(d.name)}</span>
            <span class="d-meta">{list([d.network ? d.model : transportLabel(d.transport), maxRate(d)])}{#if !d.bitPerfectCapable}<em>{t("list.sep")}{t("dev.noBitPerfect")}</em>{/if}</span>
          </button>
        </li>
      {/snippet}
      <ul class="list" role="listbox" aria-label={t("dev.list")}>
        {#each local as d (d.uid)}{@render item(d)}{/each}
      </ul>
      {#if !isIOS}
        <p class="group">{t("dev.network")}</p>
        {#if network.length}
          <ul class="list" role="listbox" aria-label={t("dev.network")}>
            {#each network as d (d.uid)}{@render item(d)}{/each}
          </ul>
        {:else}
          <p class="searching">{t("dev.networkSearching")}</p>
        {/if}
      {/if}

      {#if current?.network}
        {#if volume !== null}
          <label class="option volume">
            <span>
              <strong>{t("dev.volume")}</strong>
              <input type="range" min="0" max="100" step="1" value={volume} oninput={(e) => setVolume(Number(e.currentTarget.value))} aria-label={t("dev.volume")} />
            </span>
            <span class="num">{volume}</span>
          </label>
        {/if}
        <p class="note">{t("dev.networkHelp")}</p>
      {:else if isIOS}
        <p class="note">{t("dev.iosNote")}</p>
      {:else}
      <label class="option">
        <input type="checkbox" bind:checked={exclusive} />
        {#if isAndroid}
          <span>
            <strong>{t("dev.androidExclusive")}</strong>
            <small>{t("dev.androidExclusiveHelp")}</small>
          </span>
        {:else}
        <span>
          <strong>{t("dev.exclusive")}</strong>
          <small>{t("dev.exclusiveHelp")}</small>
        </span>
        {/if}
      </label>
      {/if}

      {#if current?.maxDop}
        <label class="option">
          <input type="checkbox" checked={dop} onchange={(e) => setDop(e.currentTarget.checked)} />
          <span>
            <strong>{t("dev.dop")}</strong>
            <small>{t("dev.dopHelp", { device: deviceLabel(current.name), max: current.maxDop })}</small>
          </span>
        </label>
      {/if}

      <button class="test" onclick={() => { open = false; onchanneltest(); }} disabled={!current}>
        {t("dev.test")}
        <small>{t("dev.testHelp")}</small>
      </button>
    </div>
  {/if}
</div>

<style>
  .picker {
    position: relative;
  }
  .trigger {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 7px 12px;
    border-radius: var(--r-md);
    background: var(--slate-700);
    border: 1px solid var(--slate-600);
    font-size: var(--t-sm);
    max-width: 340px;
  }
  .trigger:hover {
    border-color: var(--slate-500);
  }
  .name {
    font-weight: 600;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .rate {
    color: var(--mist);
    white-space: nowrap;
  }
  .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--brass);
    box-shadow: 0 0 6px var(--brass-glow);
    flex: none;
  }
  .dot.warn {
    background: var(--violet);
    box-shadow: none;
  }

  .panel {
    position: absolute;
    inset-inline-end: 0;
    top: calc(100% + 8px);
    width: 360px;
    z-index: 20;
    background: var(--slate-700);
    border: 1px solid var(--slate-500);
    border-radius: 12px;
    padding: 8px;
    box-shadow: 0 18px 40px rgba(5, 12, 15, 0.55);
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .device {
    width: 100%;
    text-align: start;
    padding-block: 10px;
    padding-inline: 14px 12px;
    border-radius: var(--r-md);
    display: flex;
    flex-direction: column;
    gap: 1px;
    border-inline-start: 3px solid transparent;
  }
  .device:hover {
    background: var(--slate-600);
  }
  .device.on {
    border-inline-start-color: var(--brass);
    background: var(--slate-600);
  }
  .d-name {
    font-weight: 600;
  }
  .d-meta {
    color: var(--mist);
    font-size: var(--t-sm);
  }
  .d-meta em {
    font-style: normal;
    color: var(--violet);
  }

  .group {
    margin: 8px 0 0;
    padding: 10px 14px 4px;
    border-top: 1px solid var(--slate-600);
    color: var(--mist);
    font-size: var(--t-xs);
    font-weight: 600;
  }
  .searching {
    margin: 0;
    padding: 4px 14px 10px;
    color: var(--mist-dim);
    font-size: var(--t-xs);
    line-height: 1.4;
  }
  .volume {
    align-items: center;
    cursor: default;
  }
  .volume > span:first-child {
    flex: 1;
  }
  .volume input[type="range"] {
    width: 100%;
    accent-color: var(--brass);
    margin: 6px 0 0;
  }
  .volume .num {
    min-width: 2.5ch;
    text-align: end;
    color: var(--mist);
    font-variant-numeric: tabular-nums;
  }
  .note {
    margin: 4px 0 0;
    padding: 10px 14px;
    color: var(--mist);
    font-size: var(--t-sm);
  }
  .option {
    display: flex;
    gap: 10px;
    padding: 12px 14px;
    margin-top: 6px;
    border-top: 1px solid var(--slate-600);
    cursor: pointer;
  }
  .option input {
    accent-color: var(--brass);
    margin-top: 3px;
  }
  .option span,
  .test {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  small {
    color: var(--mist);
    font-size: var(--t-xs);
    line-height: 1.4;
    font-weight: 400;
  }
  .test {
    width: 100%;
    text-align: start;
    padding: 12px 14px;
    border-top: 1px solid var(--slate-600);
    font-weight: 600;
    border-radius: 0 0 var(--r-md) var(--r-md);
  }
  .test:hover:not(:disabled) {
    background: var(--slate-600);
  }
  .test:disabled {
    opacity: 0.5;
    cursor: default;
  }

  /* phone: the panel spans the screen under the header */
  @media (max-width: 700px), (max-height: 500px) {
    .trigger {
      max-width: 100%;
    }
    .panel {
      position: fixed;
      left: 10px;
      right: 10px;
      top: calc(var(--safe-top, env(safe-area-inset-top)) + 58px);
      width: auto;
      max-height: calc(100dvh - var(--safe-top, env(safe-area-inset-top)) - var(--safe-bottom, env(safe-area-inset-bottom)) - 80px);
      overflow-y: auto;
    }
  }
</style>
