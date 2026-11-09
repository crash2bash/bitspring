<script lang="ts">
  import type { Crossfeed, DspSettings, EqInfo } from "./api";

  let {
    settings,
    eq,
    onchange,
    onpickeq,
  }: {
    settings: DspSettings;
    eq: EqInfo | null;
    onchange: (s: DspSettings) => void;
    onpickeq: () => void;
  } = $props();

  let open = $state(false);
  let root: HTMLElement;

  let active = $derived(
    [settings.upsampleKhz && "апсэмплинг", settings.eqPath && "эквалайзер", settings.crossfeed && "кроссфид"].filter(
      Boolean,
    ) as string[],
  );

  let lossyOn = $derived(settings.headroom || settings.restoreHighs);

  const UPSAMPLE: { value: number | null; label: string }[] = [
    { value: null, label: "Выкл" },
    { value: 192, label: "до 192" },
    { value: 384, label: "до 384" },
    { value: 768, label: "до 768" },
  ];
  const CROSSFEED: { value: Crossfeed | null; label: string }[] = [
    { value: null, label: "Выкл" },
    { value: "light", label: "Лёгкий" },
    { value: "medium", label: "Средний" },
    { value: "strong", label: "Сильный" },
  ];

  function set(patch: Partial<DspSettings>) {
    onchange({ ...settings, ...patch });
  }

  // EQ curve → SVG path. x: log 20 Hz–20 kHz, y: ±12 dB.
  const W = 320;
  const H = 96;
  const x = (f: number) => (Math.log10(f / 20) / 3) * W;
  const y = (db: number) => H / 2 - (Math.max(-12, Math.min(12, db)) / 12) * (H / 2 - 4);
  let curvePath = $derived(eq ? eq.curve.map(([f, db], i) => `${i ? "L" : "M"}${x(f).toFixed(1)},${y(db).toFixed(1)}`).join("") : "");

  // composedPath is captured at dispatch, so a click on a button that the click itself
  // removes from the DOM (e.g. "load profile" → replaced by the curve) still counts as inside.
  function onWindowClick(e: MouseEvent) {
    if (open && root && !e.composedPath().includes(root)) open = false;
  }
</script>

<svelte:window onclick={onWindowClick} onkeydown={(e) => e.key === "Escape" && (open = false)} />

<div class="enh" bind:this={root}>
  <button class="trigger" class:on={active.length > 0} onclick={() => (open = !open)} aria-expanded={open} aria-haspopup="dialog">
    <span>Обработка</span>
    <span class="state">{active.length ? active.join(", ") : lossyOn ? "только MP3 и AAC" : "выключена"}</span>
  </button>

  {#if open}
    <div class="panel" role="dialog" aria-label="Обработка звука">
      <fieldset>
        <legend>Апсэмплинг</legend>
        <div class="seg" role="radiogroup" aria-label="Апсэмплинг">
          {#each UPSAMPLE as o (o.label)}
            <button role="radio" aria-checked={settings.upsampleKhz === o.value} class:sel={settings.upsampleKhz === o.value} onclick={() => set({ upsampleKhz: o.value })}>
              {o.label}{o.value ? " кГц" : ""}
            </button>
          {/each}
        </div>
        <p class="help">
          Пересчитывает трек в самую высокую частоту того же семейства, которую принимает DAC
          (44,1 → 352,8, 48 → 384). Фильтр с линейной фазой, подавление около 100 дБ.
        </p>
      </fieldset>

      <fieldset>
        <legend>Эквалайзер</legend>
        {#if eq}
          <div class="eq-head">
            <span class="eq-name">{eq.name}</span>
            <span class="eq-meta num">{eq.bands.length} полос, предусиление {eq.preamp.toLocaleString("ru-RU")} дБ</span>
          </div>
          <svg class="curve" viewBox={`0 0 ${W} ${H}`} role="img" aria-label="Кривая эквалайзера">
            {#each [100, 1000, 10000] as f (f)}
              <line x1={x(f)} x2={x(f)} y1="0" y2={H} class="grid" />
              <text x={x(f) + 3} y={H - 4} class="tick">{f >= 1000 ? `${f / 1000}k` : f}</text>
            {/each}
            <line x1="0" x2={W} y1={y(0)} y2={y(0)} class="zero" />
            <path d={curvePath} class="line" />
          </svg>
          <div class="row">
            <button class="link" onclick={onpickeq}>Выбрать другой профиль</button>
            <button class="link" onclick={() => set({ eqPath: null })}>Выключить</button>
          </div>
        {:else}
          <button class="load" onclick={onpickeq}>Загрузить профиль AutoEQ</button>
          <p class="help">
            Файл ParametricEQ.txt для вашей модели и ревизии наушников — их публикует проект AutoEQ.
            У разных ревизий LCD-2 разная АЧХ, поэтому профиль нужен именно под вашу.
          </p>
        {/if}
      </fieldset>

      <fieldset>
        <legend>Кроссфид</legend>
        <div class="seg" role="radiogroup" aria-label="Кроссфид">
          {#each CROSSFEED as o (o.label)}
            <button role="radio" aria-checked={settings.crossfeed === o.value} class:sel={settings.crossfeed === o.value} onclick={() => set({ crossfeed: o.value })}>
              {o.label}
            </button>
          {/each}
        </div>
        <p class="help">
          Подмешивает в каждое ухо приглушённую копию соседнего канала, как при прослушивании колонок.
          Убирает ощущение «звука внутри головы» на записях с жёсткой стереопанорамой.
        </p>
      </fieldset>

      <fieldset>
        <legend>MP3 и AAC</legend>
        <label class="toggle">
          <input type="checkbox" role="switch" checked={settings.headroom} onchange={(e) => set({ headroom: e.currentTarget.checked })} />
          <span class="knob" aria-hidden="true"></span>
          <span class="t-label">Защита от перегрузки</span>
        </label>
        <p class="help">
          После декодирования пики MP3 часто выходят за 0 dBFS, и громкие места хрипят. Плеер
          измеряет перегрузку по всему треку и снижает уровень ровно на неё — обычно меньше 1 дБ.
        </p>
        <label class="toggle">
          <input type="checkbox" role="switch" checked={settings.restoreHighs} onchange={(e) => set({ restoreHighs: e.currentTarget.checked })} />
          <span class="knob" aria-hidden="true"></span>
          <span class="t-label">Восстановление верхов</span>
        </label>
        <p class="help">
          Находит частоту, на которой кодек срезал верха (16–19 кГц у MP3 128–256 kbps), и достраивает
          октаву выше по гармоникам соседней полосы. Это правдоподобная дорисовка, а не возврат потерянного.
          Файлы со срезом выше 20 кГц не трогает.
        </p>
        <p class="help">Lossless-файлов эти настройки не касаются — они идут как раньше.</p>
      </fieldset>

      <p class="footer" class:pure={active.length === 0}>
        {#if active.length === 0}
          {lossyOn ? "Lossless-файлы идут в DAC bit-perfect, MP3 и AAC — с исправлениями." : "Всё выключено: сигнал идёт в DAC bit-perfect."}
        {:else}
          Обработка отключает bit-perfect. Выключите всё, чтобы вернуть нетронутый сигнал.
        {/if}
      </p>
    </div>
  {/if}
</div>

<style>
  .enh {
    position: relative;
  }
  .trigger {
    display: flex;
    align-items: baseline;
    gap: 8px;
    padding: 7px 12px;
    border-radius: var(--r-md);
    border: 1px solid var(--slate-600);
    font-size: var(--t-sm);
    font-weight: 600;
    max-width: 320px;
  }
  .trigger:hover {
    border-color: var(--slate-500);
  }
  .state {
    font-weight: 400;
    color: var(--mist);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .trigger.on {
    border-color: var(--violet);
  }
  .trigger.on .state {
    color: var(--violet);
  }

  .panel {
    position: absolute;
    right: 0;
    top: calc(100% + 8px);
    width: 380px;
    max-height: calc(100vh - 80px);
    overflow-y: auto;
    z-index: 20;
    background: var(--slate-700);
    border: 1px solid var(--slate-500);
    border-radius: 12px;
    padding: 6px 18px 14px;
    box-shadow: 0 18px 40px rgba(5, 12, 15, 0.55);
  }
  fieldset {
    border: 0;
    border-bottom: 1px solid var(--slate-600);
    margin: 0;
    padding: 14px 0 16px;
  }
  legend {
    padding: 0;
    margin-bottom: 10px;
    font-weight: 600;
    float: left;
    width: 100%;
  }
  .seg {
    clear: both;
    display: grid;
    grid-auto-flow: column;
    grid-auto-columns: 1fr;
    background: var(--slate-800);
    border-radius: var(--r-md);
    padding: 3px;
    gap: 3px;
  }
  .seg button {
    padding: 6px 4px;
    border-radius: 6px;
    font-size: var(--t-sm);
    color: var(--mist);
    white-space: nowrap;
  }
  .seg button:hover {
    color: var(--porcelain);
  }
  .seg button.sel {
    background: var(--slate-600);
    color: var(--porcelain);
    font-weight: 600;
  }
  .help {
    clear: both;
    margin: 10px 0 0;
    font-size: var(--t-xs);
    line-height: 1.5;
    color: var(--mist);
  }

  .eq-head {
    clear: both;
    display: flex;
    flex-direction: column;
  }
  .eq-name {
    font-weight: 600;
  }
  .eq-meta {
    font-size: var(--t-xs);
    color: var(--mist);
  }
  .curve {
    width: 100%;
    height: auto;
    margin-top: 10px;
    background: var(--slate-800);
    border-radius: var(--r-md);
  }
  .curve .grid {
    stroke: var(--slate-600);
    stroke-width: 1;
  }
  .curve .zero {
    stroke: var(--slate-500);
    stroke-dasharray: 3 3;
  }
  .curve .line {
    fill: none;
    stroke: var(--violet);
    stroke-width: 2;
  }
  .curve .tick {
    fill: var(--mist-dim);
    font-size: 9px;
    font-family: var(--font-display);
  }
  .row {
    display: flex;
    gap: 16px;
    margin-top: 8px;
  }
  .link {
    font-size: var(--t-sm);
    color: var(--mist);
  }
  .link:hover {
    color: var(--porcelain);
  }
  .load {
    clear: both;
    display: block;
    width: 100%;
    padding: 9px;
    border-radius: var(--r-md);
    border: 1px dashed var(--slate-500);
    color: var(--porcelain);
    font-size: var(--t-sm);
  }
  .load:hover {
    border-color: var(--mist);
  }

  .toggle {
    clear: both;
    display: flex;
    align-items: center;
    gap: 10px;
    margin-top: 12px;
    cursor: pointer;
    font-size: var(--t-sm);
    font-weight: 500;
  }
  .toggle:first-of-type {
    margin-top: 0;
  }
  .toggle input {
    position: absolute;
    opacity: 0;
    width: 1px;
    height: 1px;
  }
  .knob {
    flex: none;
    position: relative;
    width: 30px;
    height: 18px;
    border-radius: 9px;
    background: var(--slate-800);
    border: 1px solid var(--slate-500);
    transition: background 120ms ease, border-color 120ms ease;
  }
  .knob::after {
    content: "";
    position: absolute;
    top: 2px;
    left: 2px;
    width: 12px;
    height: 12px;
    border-radius: 50%;
    background: var(--mist);
    transition: transform 120ms ease, background 120ms ease;
  }
  .toggle input:checked + .knob {
    background: rgba(140, 122, 230, 0.25);
    border-color: var(--violet);
  }
  .toggle input:checked + .knob::after {
    transform: translateX(12px);
    background: var(--violet);
  }
  .toggle input:focus-visible + .knob {
    outline: 2px solid var(--porcelain);
    outline-offset: 2px;
  }
  @media (prefers-reduced-motion: reduce) {
    .knob,
    .knob::after {
      transition: none;
    }
  }

  .footer {
    margin: 12px 0 0;
    font-size: var(--t-sm);
    color: var(--violet);
  }
  .footer.pure {
    color: var(--brass);
  }

  /* phone: the panel spans the screen under the header */
  @media (max-width: 700px) {
    .trigger {
      max-width: 100%;
    }
    .panel {
      position: fixed;
      left: 10px;
      right: 10px;
      top: calc(env(safe-area-inset-top) + 58px);
      width: auto;
      max-height: calc(100dvh - env(safe-area-inset-top) - env(safe-area-inset-bottom) - 80px);
      overflow-y: auto;
    }
  }
</style>
