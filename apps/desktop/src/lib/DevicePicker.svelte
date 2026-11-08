<script lang="ts">
  import { khz, TRANSPORT_LABEL, type Device } from "./api";

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

  function toggle() {
    open = !open;
    if (open) onrefresh();
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
    return d.rates.length ? `до ${khz(Math.max(...d.rates))} кГц` : "";
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
    <span class="name">{current?.name ?? "Выберите выход"}</span>
    {#if current}<span class="rate num">{khz(current.nominalRate)} кГц</span>{/if}
    <svg width="10" height="6" viewBox="0 0 10 6" aria-hidden="true"><path d="M1 1l4 4 4-4" fill="none" stroke="currentColor" stroke-width="1.5" /></svg>
  </button>

  {#if open}
    <div class="panel" role="dialog" aria-label="Выход звука">
      <ul class="list" role="listbox" aria-label="Устройства">
        {#each devices as d (d.uid)}
          <li>
            <button
              class="device"
              class:on={d.uid === selected}
              role="option"
              aria-selected={d.uid === selected}
              onclick={() => choose(d.uid)}
            >
              <span class="d-name">{d.name}{d.isDefault ? " (системный)" : ""}</span>
              <span class="d-meta">{TRANSPORT_LABEL[d.transport] ?? d.transport}, {maxRate(d)}{#if !d.bitPerfectCapable}<em>, без bit-perfect</em>{/if}</span>
            </button>
          </li>
        {/each}
      </ul>

      <label class="option">
        <input type="checkbox" bind:checked={exclusive} />
        <span>
          <strong>Эксклюзивный доступ</strong>
          <small>Другие приложения не подмешивают звук, частота DAC подстраивается под каждый трек.</small>
        </span>
      </label>

      {#if current?.maxDop}
        <label class="option">
          <input type="checkbox" checked={dop} onchange={(e) => setDop(e.currentTarget.checked)} />
          <span>
            <strong>DSD через DoP</strong>
            <small>
              DSD-файлы уходят в {current.name} как есть, до DSD{current.maxDop}. Включайте, только если DAC
              поддерживает DoP: иначе вместо музыки будет тихий шум. Выключено — DSD переводится в PCM.
            </small>
          </span>
        </label>
      {/if}

      <button class="test" onclick={() => { open = false; onchanneltest(); }} disabled={!current}>
        Проверить левый и правый канал
        <small>Сначала низкий тон слева, затем высокий справа.</small>
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
    right: 0;
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
    text-align: left;
    padding: 10px 12px 10px 14px;
    border-radius: var(--r-md);
    display: flex;
    flex-direction: column;
    gap: 1px;
    border-left: 3px solid transparent;
  }
  .device:hover {
    background: var(--slate-600);
  }
  .device.on {
    border-left-color: var(--brass);
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
    text-align: left;
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
</style>
