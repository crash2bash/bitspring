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
import { downloadUrl, fontGroup, fontsReady, isRtl, numberLocale, t } from "./i18n/runtime";
import { detectPlatform, isAvailable, type Platform } from "./platform";
import { HERO, type FieldState } from "./field-state";
import type { Field } from "./scene";

gsap.registerPlugin(ScrollTrigger);

const reduceMotion = matchMedia("(prefers-reduced-motion: reduce)").matches;
const $ = <T extends Element = HTMLElement>(sel: string, root: ParentNode = document) => root.querySelector(sel) as T;
const $$ = <T extends Element = HTMLElement>(sel: string, root: ParentNode = document) =>
  Array.from(root.querySelectorAll(sel)) as T[];

// ---- download buttons ------------------------------------------------------------------------
// The main button offers the build for the visitor's device: an iPhone goes straight to the
// App Store, Windows gets the installer (the Store is one click further, on the download page), a Mac goes to the download page to choose between the
// App Store and a .dmg, Linux to choose between the AppImage and a .deb. "Other platforms" always leads to that page.

const platform: Platform = detectPlatform();

function primaryDownload(p: Platform): { href: string; label: string; direct: boolean } {
  const soon = (label: string) => ({ href: downloadUrl, label: t(label), direct: false });
  switch (p) {
    case "ios":
      return LINKS.iosAppStore ? { href: LINKS.iosAppStore, label: t("cta.ios"), direct: true } : soon("cta.ios_soon");
    case "windows":
      return LINKS.windowsFile ? { href: LINKS.windowsFile, label: t("cta.windows"), direct: true } : soon("cta.windows_soon");
    case "android":
      return LINKS.android ? { href: LINKS.android, label: t("cta.android"), direct: true } : soon("cta.android_soon");
    case "mac":
      return { href: downloadUrl, label: t(isAvailable("mac") ? "cta.mac" : "cta.mac_soon"), direct: false };
    // Two formats (AppImage, .deb): the download page explains which.
    case "linux":
      return { href: downloadUrl, label: t(isAvailable("linux") ? "cta.linux" : "cta.linux_soon"), direct: false };
    default:
      return { href: downloadUrl, label: t("cta.all_platforms"), direct: false };
  }
}

const primary = primaryDownload(platform);
for (const a of $$<HTMLAnchorElement>("[data-download-primary]")) {
  a.href = primary.href;
  a.textContent = primary.label;
  if (primary.direct) a.rel = "noopener";
}

// The platform list under "Free": what is out already, what is still coming.
for (const li of $$<HTMLElement>("[data-platform]")) {
  const p = li.dataset.platform as Platform;
  if (isAvailable(p)) continue;
  li.classList.add("is-soon");
  const status = li.querySelector("[data-status]");
  if (status) status.textContent = t("dl.soon");
}
const androidStatus = document.querySelector('[data-platform="android"] [data-status]');
if (androidStatus && LINKS.android) androidStatus.textContent = /play\.google\./.test(LINKS.android) ? "Google Play" : "APK";
// Mac and Windows: name the store and/or the file, whichever are actually out.
const both = (store: string | null, storeName: string, file: string | null, fileName: string) =>
  [store && storeName, file && fileName].filter(Boolean).join(" · ");
const macStatus = document.querySelector('[data-platform="mac"] [data-status]');
if (macStatus && isAvailable("mac")) macStatus.textContent = both(LINKS.macAppStore, "Mac App Store", LINKS.macFile, ".dmg");
const winStatus = document.querySelector('[data-platform="windows"] [data-status]');
if (winStatus && isAvailable("windows"))
  winStatus.textContent = both(LINKS.windowsStore, "Microsoft Store", LINKS.windowsFile, ".exe");

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

// ---- FAQ ---------------------------------------------------------------------------------------
// An answer opening or closing changes the page's height, which moves every scroll trigger
// below it (the download section's camera move).

for (const d of $$<HTMLDetailsElement>(".faq-item")) d.addEventListener("toggle", () => ScrollTrigger.refresh());

// ---- 3D field --------------------------------------------------------------------------------

// three.js is most of this page's script, so it loads as its own chunk after the text has
// started animating: the headline doesn't wait for it. The intro fades the field in from black,
// so a field arriving a moment later joins that fade rather than popping in.
let field: Field | null = null;
import("./scene")
  .then(({ createField }) => {
    field = createField($<HTMLCanvasElement>(".field"), { mirror: isRtl });
    if (!field) return;
    field.setMotion(!reduceMotion);
    // Start where the page already is (mid-intro, or scrolled down on a reload), not at the hero.
    Object.assign(field.state, fieldTarget());
    window.addEventListener(
      "pointermove",
      (e) => {
        if (!field) return;
        field.pointer.x = (e.clientX / window.innerWidth) * 2 - 1;
        field.pointer.y = (e.clientY / window.innerHeight) * 2 - 1;
      },
      { passive: true },
    );
  })
  .catch(() => {}); // Offline mid-load or no WebGL: the page reads fine on its own background.

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
  // Every element takes its start state right now (immediateRender), and synchronously (lazy
  // would wait for GSAP's first tick, a few frames later): nothing may show at rest first and
  // then jump back to the start.
  gsap
    .timeline({ defaults: { ease: "expo.out", immediateRender: true, lazy: false } })
    .fromTo(intro, { amp: 0, visible: 0 }, { amp: 1, visible: 1, duration: 2.6, ease: "power3.out" }, 0)
    .from(heroChars, { yPercent: 110, duration: 1.4, stagger: 0.05 }, 0.25)
    .from(".hero-lede", { autoAlpha: 0, y: 16, duration: 1.2 }, 0.9)
    .from(".hero .cta-row .btn", { autoAlpha: 0, y: 16, duration: 1.1, stagger: 0.08 }, 1.0)
    .from(".nav", { autoAlpha: 0, duration: 1.2, ease: "power2.out" }, 1.1);
}
// index.html hid these before the first paint; from here on the timeline owns them.
document.documentElement.classList.remove("intro");

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

/** Where the field should be right now: the scroll position's state, faded by the intro. */
function fieldTarget(): FieldState {
  const target = stateAtScroll();
  target.amp *= intro.amp;
  target.visible *= intro.visible;
  return target;
}

gsap.ticker.add(() => {
  if (!field) return;
  const target = fieldTarget();
  const k = reduceMotion ? 1 : 0.075;
  const s = field.state;
  for (const key of Object.keys(target) as (keyof FieldState)[]) {
    // Bit depth snaps, like it does on a real DAC; everything else glides.
    s[key] = key === "levels" ? target[key] : s[key] + (target[key] - s[key]) * k;
  }
});

// Dimmed and lowered: the field runs behind the steps' text here, which has to stay readable.
const PATH: FieldState = { ...HERO, camX: 0, camY: 0.35, camZ: 12, lookX: 0, lookY: 1.1, lookZ: 0, focus: 1, visible: 0.45 };
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
    ".app-frame",
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

// The app's screens: tabs for Now Playing, Library and Equalizer (the ones drawn in the screenshot
// answer clicks too), the equalizer switch under Now Playing (brass bit-perfect ↔ violet
// processing), and Glass | Classic for all of them. One screenshot per screen and look
// (public/screens, from apps/desktop/scripts/site-screenshots.mjs).
type Screen = "player" | "library" | "eq";
type Look = "glass" | "classic";
const appShot = $(".app-shot");
const shots = $$<HTMLImageElement>(".shot", appShot);
const view = { screen: "player" as Screen, look: "glass" as Look, eq: false };
const ALT: Record<string, string> = {
  player: shots[0].alt,
  processing: t("app.alt_processing"),
  library: t("app.alt_library"),
  eq: t("app.alt_eq"),
};
// Phones get the phone app's screens in a phone-shaped frame: Android ones on Android
// (<name>-android.webp, 1082×2402), the iPhone ones everywhere else (<name>-phone.webp,
// 1179×2556). The width matches the frame's breakpoint in style.css.
const phoneShots = matchMedia("(max-width: 700px)");
const phoneKind = platform === "android" ? "android" : "phone";
appShot.dataset.device = phoneKind;
const fileOf = (look: Look, screen: Screen, eq: boolean) =>
  (screen === "player" && eq ? "processing" : screen) + "-" + look + (phoneShots.matches ? `-${phoneKind}` : "");
const srcOf = (look: Look, screen: Screen, eq: boolean) => `/screens/${fileOf(look, screen, eq)}.webp`;

/** Crossfade: the next shot decodes in the hidden layer, then takes over. */
let showing = 0;
async function render() {
  appShot.dataset.screen = view.screen;
  appShot.dataset.look = view.look;
  appShot.dataset.eq = view.eq ? "on" : "off";
  const src = srcOf(view.look, view.screen, view.eq);
  const cur = shots[showing];
  // currentSrc: the first shot's src attribute is the desktop fallback, its srcset chose the file.
  if (cur.currentSrc && new URL(cur.currentSrc).pathname === src) return;
  if (!cur.currentSrc && cur.getAttribute("src") === src) return;
  const next = shots[1 - showing];
  // A srcset would override src.
  next.removeAttribute("srcset");
  next.removeAttribute("sizes");
  next.src = src;
  // Decoding can stall while the page isn't being drawn (a background tab): don't wait forever.
  await Promise.race([next.decode().catch(() => {}), new Promise((r) => setTimeout(r, 1500))]);
  if (next.getAttribute("src") !== src) return; // a later click won
  next.alt = ALT[view.screen === "player" && view.eq ? "processing" : view.screen];
  cur.alt = "";
  next.classList.add("is-on");
  cur.classList.remove("is-on");
  showing = 1 - showing;
}

/** Width and height for the shots' aspect ratio before they load. */
function sizeShots() {
  const [w, h] = !phoneShots.matches ? [2240, 1480] : phoneKind === "android" ? [1082, 2402] : [1179, 2556];
  for (const s of shots) {
    s.width = w;
    s.height = h;
  }
}
// The first shot arrives with a srcset that already picked desktop, iPhone or Android
// (index.html). It's left alone until render() reuses that element: touching it now would make
// the browser load it at once instead of when it scrolls near.
sizeShots();
phoneShots.addEventListener("change", () => {
  sizeShots();
  render();
  prefetch();
});

/** Fetch the current look's other screens ahead of time, so switching is instant. */
const fetched = new Set<string>();
function prefetch() {
  for (const screen of ["player", "library", "eq"] as Screen[])
    for (const eq of screen === "player" ? [false, true] : [false]) {
      const src = srcOf(view.look, screen, eq);
      if (fetched.has(src)) continue;
      fetched.add(src);
      new Image().src = src;
    }
}
new IntersectionObserver(
  (entries, o) => {
    if (!entries.some((e) => e.isIntersecting)) return;
    o.disconnect();
    prefetch();
  },
  { rootMargin: "600px 0px" },
).observe(appShot);

/** A segmented control (tabs or radios) with the sliding pill; arrow keys move like in any. */
function segmented(root: HTMLElement, attr: "aria-selected" | "aria-checked", onPick: (b: HTMLButtonElement) => void) {
  const buttons = $$<HTMLButtonElement>("button", root);
  const place = () => {
    const b = buttons.find((x) => x.getAttribute(attr) === "true")!;
    root.style.setProperty("--x", `${b.offsetLeft}px`);
    root.style.setProperty("--w", `${b.offsetWidth}px`);
    if (!root.classList.contains("is-placed")) requestAnimationFrame(() => root.classList.add("is-placed"));
  };
  const pick = (b: HTMLButtonElement) => {
    for (const x of buttons) {
      const on = x === b;
      x.setAttribute(attr, String(on));
      x.tabIndex = on ? 0 : -1;
    }
    place();
    b.scrollIntoView({ block: "nearest", inline: "nearest" });
    onPick(b);
  };
  // Labels change width as fonts arrive and when the window resizes.
  new ResizeObserver(place).observe(root);
  fontsReady.then(() => document.fonts?.ready).then(place);
  buttons.forEach((b, i) => {
    b.tabIndex = b.getAttribute(attr) === "true" ? 0 : -1;
    b.addEventListener("click", () => pick(b));
    b.addEventListener("keydown", (e) => {
      let step = ({ ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 } as Record<string, number>)[e.key];
      if (!step) return;
      e.preventDefault();
      // Right-to-left pages lay the options out from the right: left/right swap meaning.
      if (isRtl && (e.key === "ArrowLeft" || e.key === "ArrowRight")) step = -step;
      const next = buttons[(i + step + buttons.length) % buttons.length];
      pick(next);
      next.focus();
    });
  });
  return pick;
}

const tabs = $("[data-screen-tabs]");
const pickTab = segmented(tabs, "aria-selected", (b) => {
  view.screen = b.dataset.screen as Screen;
  $("#app-view").setAttribute("aria-labelledby", b.id);
  render();
});
segmented($("[data-look-switch]"), "aria-checked", (b) => {
  view.look = b.dataset.look as Look;
  render();
  prefetch();
});
for (const h of $$<HTMLButtonElement>(".hotspot", appShot)) {
  h.addEventListener("click", () => pickTab($<HTMLButtonElement>(`button[data-screen="${h.dataset.screen}"]`, tabs)));
}

const eqToggle = $<HTMLInputElement>("[data-eq-toggle]");
const eqState = $("[data-eq-state]");
eqToggle.addEventListener("change", () => {
  view.eq = eqToggle.checked;
  eqState.textContent = t(view.eq ? "app.state_processing" : "app.state_bitperfect");
  render();
});

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
