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
import { createField, HERO, type FieldState } from "./scene";

gsap.registerPlugin(ScrollTrigger);

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
    a.textContent = a.dataset.store === "appStore" ? "Скоро в Mac App Store" : "Скоро для Windows";
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

const field = createField($<HTMLCanvasElement>(".field"));

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

function split(el: HTMLElement): HTMLElement[] {
  const text = el.textContent ?? "";
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
    for (const ch of part) {
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
const RATES = [
  { v: "44,1", unit: "кГц", caption: "Качество CD", density: 1, levels: 7 },
  { v: "48", unit: "кГц", caption: "Видео и студийная запись", density: 1.08, levels: 7 },
  { v: "88,2", unit: "кГц", caption: "Hi-Res, кратно CD", density: 1.45, levels: 8 },
  { v: "96", unit: "кГц", caption: "Hi-Res, самый частый", density: 1.52, levels: 8 },
  { v: "176,4", unit: "кГц", caption: "Студийный мастер", density: 2.1, levels: 9 },
  { v: "192", unit: "кГц", caption: "Студийный мастер", density: 2.2, levels: 9 },
  { v: "352,8", unit: "кГц", caption: "DXD", density: 3.0, levels: 10 },
  { v: "384", unit: "кГц", caption: "DXD", density: 3.1, levels: 10 },
  { v: "705,6", unit: "кГц", caption: "Предел современных ЦАП", density: 4.2, levels: 11 },
  { v: "768", unit: "кГц", caption: "Предел современных ЦАП", density: 4.4, levels: 11 },
  { v: "DSD64", unit: "", caption: "Один бит, 2,8 МГц", density: 5.2, levels: 1 },
  { v: "DSD128", unit: "", caption: "Один бит, 5,6 МГц", density: 5.8, levels: 1 },
  { v: "DSD256", unit: "", caption: "Один бит, 11,2 МГц", density: 6.4, levels: 1 },
  { v: "DSD512", unit: "", caption: "Один бит, 22,6 МГц", density: 7.0, levels: 1 },
  { v: "DSD1024", unit: "", caption: "Один бит, 45,2 МГц", density: 7.6, levels: 1 },
];
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
const fmt = new Intl.NumberFormat("ru-RU");

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
document.fonts?.ready.then(() => ScrollTrigger.refresh());
