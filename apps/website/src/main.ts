import "@fontsource/barlow/400.css";
import "@fontsource/barlow/500.css";
import "@fontsource/barlow-semi-condensed/300.css";
import "@fontsource/barlow-semi-condensed/500.css";
import "lenis/dist/lenis.css";
import "./style.css";

import gsap from "gsap";
import { ScrollTrigger } from "gsap/ScrollTrigger";
import Lenis from "lenis";

import { LINKS } from "./config";
import { localeUrl, LOCALES, type FontGroup } from "./i18n/locales";
import { createField, HERO, type FieldState } from "./scene";

gsap.registerPlugin(ScrollTrigger);

// ---- language ------------------------------------------------------------------------------
// The page arrives already translated (see src/i18n/render.ts); only strings that JS writes at
// runtime come from this inline dictionary.

const root = document.documentElement;
const runtime = JSON.parse(document.getElementById("i18n")?.textContent ?? "{}") as {
  numbers?: string;
  strings?: Record<string, string>;
};
const t = (key: string) => runtime.strings?.[key] ?? key;
const numberLocale = runtime.numbers ?? root.lang;
const isRtl = root.dir === "rtl";

// Barlow covers Latin only. Other scripts fall back to a companion face (see --script-font in
// style.css); each page loads just its own, and CJK faces arrive in small unicode-range slices.
const SCRIPT_FONTS: Record<FontGroup, () => Promise<unknown>> = {
  latin: async () => {},
  cyrillic: () =>
    Promise.all([
      import("@fontsource/ibm-plex-sans/cyrillic-300.css"),
      import("@fontsource/ibm-plex-sans/cyrillic-400.css"),
      import("@fontsource/ibm-plex-sans/cyrillic-500.css"),
    ]),
  greek: () =>
    Promise.all([
      import("@fontsource/ibm-plex-sans/greek-300.css"),
      import("@fontsource/ibm-plex-sans/greek-400.css"),
      import("@fontsource/ibm-plex-sans/greek-500.css"),
    ]),
  arabic: () =>
    Promise.all([
      import("@fontsource/ibm-plex-sans-arabic/arabic-300.css"),
      import("@fontsource/ibm-plex-sans-arabic/arabic-400.css"),
      import("@fontsource/ibm-plex-sans-arabic/arabic-500.css"),
    ]),
  sc: () =>
    Promise.all([
      import("@fontsource/noto-sans-sc/300.css"),
      import("@fontsource/noto-sans-sc/400.css"),
      import("@fontsource/noto-sans-sc/500.css"),
    ]),
  tc: () =>
    Promise.all([
      import("@fontsource/noto-sans-tc/300.css"),
      import("@fontsource/noto-sans-tc/400.css"),
      import("@fontsource/noto-sans-tc/500.css"),
    ]),
  jp: () =>
    Promise.all([
      import("@fontsource/noto-sans-jp/300.css"),
      import("@fontsource/noto-sans-jp/400.css"),
      import("@fontsource/noto-sans-jp/500.css"),
    ]),
  kr: () =>
    Promise.all([
      import("@fontsource/noto-sans-kr/300.css"),
      import("@fontsource/noto-sans-kr/400.css"),
      import("@fontsource/noto-sans-kr/500.css"),
    ]),
};
const fontGroup = (root.dataset.font ?? "latin") as FontGroup;
const fontsReady = (SCRIPT_FONTS[fontGroup] ?? SCRIPT_FONTS.latin)();

const langSelect = document.querySelector<HTMLSelectElement>("[data-lang-select]");
const langCurrent = document.querySelector<HTMLElement>("[data-lang-current]");
if (langSelect && langCurrent) {
  const current = LOCALES.find((l) => l.tag === root.lang) ?? LOCALES[0];
  langCurrent.textContent = (current.path || "en").split("-")[0].toUpperCase();
  langSelect.addEventListener("change", () => {
    const next = LOCALES.find((l) => l.path === langSelect.value);
    if (!next) return;
    try {
      localStorage.setItem("lang", next.path); // the root page stops auto-redirecting
    } catch {}
    location.href = localeUrl(next) + location.hash;
  });
}

const reduceMotion = matchMedia("(prefers-reduced-motion: reduce)").matches;
const $ = <T extends Element = HTMLElement>(sel: string, root: ParentNode = document) => root.querySelector(sel) as T;
const $$ = <T extends Element = HTMLElement>(sel: string, root: ParentNode = document) =>
  Array.from(root.querySelectorAll(sel)) as T[];

// ---- store links -------------------------------------------------------------------------

for (const a of $$<HTMLAnchorElement>("[data-store]")) {
  const url = LINKS[a.dataset.store as keyof typeof LINKS];
  if (url) {
    a.href = url;
    a.rel = "noopener";
  } else {
    a.textContent = t(a.dataset.store === "appStore" ? "cta.appstore_soon" : "cta.windows_soon");
    a.setAttribute("aria-disabled", "true");
  }
}

// ---- smooth scroll -------------------------------------------------------------------------
// Lenis interpolates wheel input, which is what makes notched Windows mice glide like a trackpad.
// It drives ScrollTrigger from GSAP's ticker so the two never disagree about the scroll position.

let lenis: Lenis | null = null;
if (!reduceMotion) {
  lenis = new Lenis({ lerp: 0.09, wheelMultiplier: 0.9, smoothWheel: true });
  lenis.on("scroll", ScrollTrigger.update);
  gsap.ticker.add((t) => lenis!.raf(t * 1000));
  gsap.ticker.lagSmoothing(0);
}

for (const a of $$<HTMLAnchorElement>('a[href^="#"]')) {
  a.addEventListener("click", (e) => {
    if (a.getAttribute("aria-disabled") === "true" && a.hash === "#download" && location.hash === "#download") return;
    const target = a.hash === "#top" ? document.body : document.querySelector<HTMLElement>(a.hash);
    if (!target) return;
    e.preventDefault();
    if (lenis) lenis.scrollTo(target, { duration: 1.6 });
    else target.scrollIntoView();
    history.replaceState(null, "", a.hash === "#top" ? location.pathname : a.hash);
  });
}

// ---- nav background once past the hero ----------------------------------------------------

const nav = $(".nav");
ScrollTrigger.create({
  start: () => window.innerHeight * 0.6,
  end: "max",
  onToggle: (self) => nav.classList.toggle("is-solid", self.isActive),
});

// ---- 3D field --------------------------------------------------------------------------------

const field = createField($<HTMLCanvasElement>(".field"), { mirror: isRtl });

if (field) {
  field.setMotion(!reduceMotion);
  window.addEventListener(
    "pointermove",
    (e) => {
      field.pointer.x = (e.clientX / window.innerWidth) * 2 - 1;
      field.pointer.y = (e.clientY / window.innerHeight) * 2 - 1;
    },
    { passive: true },
  );
}

// ---- headline letters ------------------------------------------------------------------------

// Arabic letters join to their neighbours; wrapping each in its own box would break the
// joins, so those headlines animate word by word instead.
const SPLIT_WORDS = fontGroup === "arabic";

function split(el: HTMLElement): HTMLElement[] {
  const text = (el.textContent ?? "").trim();
  el.setAttribute("aria-label", text.replace(/ /g, " "));
  el.textContent = "";
  const chars: HTMLElement[] = [];
  text.split(/(\s+)/).forEach((part) => {
    if (/^\s+$/.test(part)) {
      el.append(part.includes(" ") ? " " : " ");
      return;
    }
    const word = document.createElement("span");
    word.className = "split-word";
    word.setAttribute("aria-hidden", "true");
    for (const ch of SPLIT_WORDS ? [part] : Array.from(part)) {
      const mask = document.createElement("span");
      mask.className = "split-mask";
      const c = document.createElement("span");
      c.className = "split-char";
      c.textContent = ch;
      mask.append(c);
      word.append(mask);
      chars.push(c);
    }
    el.append(word);
  });
  return chars;
}

const heroChars = split($(".hero-title"));
const downloadChars = split($(".download-title"));

// ---- page-load: the one orchestrated moment ----------------------------------------------------
// The samples rise from silence while the headline sets, then the copy settles in.

/** Multiplies the scroll state during the intro, so the two never fight over the same values. */
const intro = { amp: 1, visible: 1 };

if (!reduceMotion) {
  gsap
    .timeline({ defaults: { ease: "expo.out" } })
    .fromTo(intro, { amp: 0, visible: 0 }, { amp: 1, visible: 1, duration: 2.6, ease: "power3.out" }, 0)
    .from(heroChars, { yPercent: 110, duration: 1.4, stagger: 0.05 }, 0.25)
    .from(".hero-lede", { autoAlpha: 0, y: 16, duration: 1.2 }, 0.9)
    .from(".hero .cta-row .btn", { autoAlpha: 0, y: 16, duration: 1.1, stagger: 0.08 }, 1.0)
    .from(".nav", { autoAlpha: 0, duration: 1.2, ease: "power2.out" }, 1.1);
}

// ---- scroll choreography ---------------------------------------------------------------------
// Each section owns a camera/signal keyframe, reached while the section scrolls into view.
// The field state is a pure function of scroll position — walk the segments in page order and
// stop at the first unfinished one — so jumping anywhere (reload mid-page, anchor links) lands on
// exactly the right frame. Per-frame easing towards that target gives the camera its weight.

type Segment = { st: ScrollTrigger; apply: (from: FieldState, p: number) => FieldState };
const segments: Segment[] = [];
const easeInOut = gsap.parseEase("power2.inOut");

function mix(a: FieldState, b: FieldState, t: number): FieldState {
  const out = { ...a };
  for (const k of Object.keys(a) as (keyof FieldState)[]) out[k] = a[k] + (b[k] - a[k]) * t;
  return out;
}

function stateAtScroll(): FieldState {
  let cur: FieldState = { ...HERO };
  for (const seg of segments) {
    const p = seg.st.progress;
    if (p <= 0) break;
    cur = seg.apply(cur, p);
    if (p < 1) break;
  }
  return cur;
}

if (field) {
  gsap.ticker.add(() => {
    const target = stateAtScroll();
    target.amp *= intro.amp;
    target.visible *= intro.visible;
    const k = reduceMotion ? 1 : 0.075;
    const s = field.state;
    for (const key of Object.keys(target) as (keyof FieldState)[]) {
      // Bit depth snaps, like it does on a real DAC; everything else glides.
      s[key] = key === "levels" ? target[key] : s[key] + (target[key] - s[key]) * k;
    }
  });
}

const PATH: FieldState = { ...HERO, camX: 0, camY: 0.35, camZ: 12, lookX: 0, lookY: 0, lookZ: 0, focus: 1 };
// Side-on, a few rows only: denser sampling and the 1-bit DSD pulses read at a glance.
const FORMATS_START: FieldState = { ...HERO, camX: 0, camY: 1.4, camZ: 12.5, lookX: 0, lookY: -0.9, lookZ: 0, focus: 0.8 };
// Densities are visual (stems per wave cycle), not literal: the jump to DSD would not fit a screen.
const RATE_STEPS: { hz: number; caption: string; density: number; levels: number; dsd?: number }[] = [
  { hz: 44.1, caption: "rates.cd", density: 1, levels: 7 },
  { hz: 48, caption: "rates.video", density: 1.08, levels: 7 },
  { hz: 88.2, caption: "rates.hires_cd", density: 1.45, levels: 8 },
  { hz: 96, caption: "rates.hires_common", density: 1.52, levels: 8 },
  { hz: 176.4, caption: "rates.master", density: 2.1, levels: 9 },
  { hz: 192, caption: "rates.master", density: 2.2, levels: 9 },
  { hz: 352.8, caption: "rates.dxd", density: 3.0, levels: 10 },
  { hz: 384, caption: "rates.dxd", density: 3.1, levels: 10 },
  { hz: 705.6, caption: "rates.limit", density: 4.2, levels: 11 },
  { hz: 768, caption: "rates.limit", density: 4.4, levels: 11 },
  { hz: 0, caption: "rates.dsd", density: 5.2, levels: 1, dsd: 64 },
  { hz: 0, caption: "rates.dsd", density: 5.8, levels: 1, dsd: 128 },
  { hz: 0, caption: "rates.dsd", density: 6.4, levels: 1, dsd: 256 },
  { hz: 0, caption: "rates.dsd", density: 7.0, levels: 1, dsd: 512 },
  { hz: 0, caption: "rates.dsd", density: 7.6, levels: 1, dsd: 1024 },
];
const decimal = new Intl.NumberFormat(numberLocale, { maximumFractionDigits: 1 });
const RATES = RATE_STEPS.map((r) => ({
  ...r,
  v: r.dsd ? `DSD${r.dsd}` : decimal.format(r.hz),
  unit: r.dsd ? "" : t("rates.khz"),
  caption: t(r.caption).replace("{mhz}", decimal.format(((r.dsd ?? 0) * 44.1) / 1000)),
}));
const ENHANCE: FieldState = {
  ...HERO,
  camX: 7,
  camY: 3.2,
  camZ: 9.5,
  lookX: -2,
  lookY: -0.6,
  lookZ: 0,
  process: 1,
  density: 1.3,
  levels: 40, // processing runs in floating point: the steps smooth out
};
const APP: FieldState = { ...ENHANCE, visible: 0.28, camY: 5, lookY: -1.5 };
const DOWNLOAD: FieldState = { ...HERO, camX: -4, camY: 1.6, camZ: 12, lookX: 3, visible: 0.9 };

function travel(trigger: string, to: FieldState, start = "top bottom", end = "top top") {
  segments.push({ st: ScrollTrigger.create({ trigger, start, end }), apply: (from, p) => mix(from, to, easeInOut(p)) });
}

// Signal path: pinned while the signal travels file → player → DAC.
const pathNodes = $$(".chain-node");
const samplesEl = $("[data-samples]");
const SAMPLES_PER_TRACK = 96_000 * 2 * 245; // 24/96 stereo, a four-minute track
const fmt = new Intl.NumberFormat(numberLocale);

travel(".path", PATH);
gsap
  .timeline({
    scrollTrigger: {
      trigger: ".path",
      start: "top top",
      end: () => `+=${window.innerHeight * 1.6}`,
      pin: true,
      scrub: reduceMotion ? true : 0.6,
      invalidateOnRefresh: true,
      onUpdate: (self) => {
        const p = self.progress;
        pathNodes.forEach((n, i) => n.classList.toggle("is-lit", p >= i * 0.42 + 0.02));
        samplesEl.textContent = fmt.format(Math.round(Math.max(0, (p - 0.05) / 0.95) * SAMPLES_PER_TRACK));
      },
    },
  })
  .fromTo(".wire-fill", { "--p": 0 }, { "--p": 1, ease: "none", duration: 0.85 }, 0.02)
  .to({}, { duration: 0.13 });

// Formats: pinned, the "tuning dial" needle sweeps through every rate the engine plays.
const ticks = $("[data-ticks]");
const needle = $("[data-needle]");
const rateEl = $("[data-rate]");
const unitEl = $("[data-unit]");
const captionEl = $("[data-caption]");
const majors: HTMLElement[] = [];
const MINOR_PER_STEP = 4;
const steps = RATES.length - 1;
for (let i = 0; i <= steps * MINOR_PER_STEP; i++) {
  const t = document.createElement("div");
  const major = i % MINOR_PER_STEP === 0;
  t.className = major ? "dial-tick is-major" : "dial-tick";
  t.style.left = `${(i / (steps * MINOR_PER_STEP)) * 100}%`;
  if (major) {
    const label = document.createElement("span");
    const r = RATES[i / MINOR_PER_STEP];
    label.textContent = r.v;
    t.append(label);
    majors.push(t);
  }
  ticks.append(t);
}

let shownRate = -1;
function showRate(index: number) {
  if (index === shownRate) return;
  shownRate = index;
  const r = RATES[index];
  rateEl.textContent = r.v;
  unitEl.textContent = r.unit;
  captionEl.textContent = r.caption;
  majors.forEach((m, i) => m.classList.toggle("is-on", i === index));
}
showRate(0);

travel(".formats", FORMATS_START);
const formatsPin = ScrollTrigger.create({
  trigger: ".formats",
  start: "top top",
  end: () => `+=${window.innerHeight * 2.4}`,
  pin: true,
  scrub: true,
  invalidateOnRefresh: true,
  onUpdate: (self) => {
    const pos = self.progress * steps;
    const i = Math.min(steps, Math.round(pos));
    showRate(i);
    gsap.set(needle, { left: `${self.progress * 100}%` });
  },
});
segments.push({
  st: formatsPin,
  apply: (from, p) => {
    const pos = p * steps;
    const a = RATES[Math.floor(pos)];
    const b = RATES[Math.min(steps, Math.floor(pos) + 1)];
    const f = pos - Math.floor(pos);
    return { ...from, density: a.density + (b.density - a.density) * f, levels: RATES[Math.min(steps, Math.round(pos))].levels };
  },
});

// Enhancers: the whole field turns violet — the same colour the app uses for "processed".
travel(".enhance", ENHANCE, "top bottom", "top 15%");
for (const li of $$(".enhance-list li")) {
  ScrollTrigger.create({
    trigger: li,
    start: "top 62%",
    end: "bottom 38%",
    onToggle: (self) => li.classList.toggle("is-on", self.isActive),
  });
}

// App: dim the field behind the screenshot; the window tilts upright as it arrives.
travel(".app", APP, "top bottom", "top 30%");
if (!reduceMotion) {
  gsap.fromTo(
    ".app-shot img",
    { rotateX: 22, scale: 0.9, y: 60 },
    {
      rotateX: 0,
      scale: 1,
      y: 0,
      ease: "none",
      scrollTrigger: { trigger: ".app-shot", start: "top 95%", end: "top 25%", scrub: 1 },
    },
  );
}

// Download: back to the opening view, back to brass.
travel(".download", DOWNLOAD, "top bottom", "top 20%");
if (!reduceMotion) {
  gsap.from(downloadChars, {
    yPercent: 110,
    stagger: 0.04,
    duration: 1.2,
    ease: "expo.out",
    scrollTrigger: { trigger: ".download", start: "top 55%", toggleActions: "play none none reverse" },
  });
}

// Fonts change line breaks and therefore every trigger position.
fontsReady.then(() => document.fonts?.ready).then(() => ScrollTrigger.refresh());
