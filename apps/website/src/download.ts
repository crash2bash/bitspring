// The download page: puts the build for the visitor's device first. On an iPhone or iPad that
// arrived from elsewhere (a shared link, a QR code), it goes straight to the App Store.

import "@fontsource/barlow/400.css";
import "@fontsource/barlow/500.css";
import "@fontsource/barlow-semi-condensed/300.css";
import "@fontsource/barlow-semi-condensed/500.css";
import "./style.css";

import { LINKS } from "./config";
import { t } from "./i18n/runtime";
import { detectPlatform, type Platform } from "./platform";

type LinkKey = keyof typeof LINKS;

// ---- options: live links, or "coming soon" ---------------------------------------------------

for (const a of document.querySelectorAll<HTMLAnchorElement>("[data-link]")) {
  const url = LINKS[a.dataset.link as LinkKey];
  if (url) {
    a.href = url;
    a.rel = "noopener";
  } else {
    a.removeAttribute("href");
    a.classList.add("is-soon");
    a.setAttribute("aria-disabled", "true");
    const note = a.querySelector(".dl-option-note");
    if (note) note.textContent = t("dl.soon");
  }
}

// ---- which platform goes first ---------------------------------------------------------------
// A link to a specific platform (/download/#windows, from the home page's list) wins over
// detection, so "I'm on a Mac but want the Windows build" works.

const PLATFORMS: Platform[] = ["mac", "windows", "ios", "android"];
const detected = detectPlatform();
const slot = document.querySelector<HTMLElement>("[data-featured]")!;
const list = document.querySelector<HTMLElement>(".dl-list")!;
const otherTitle = document.querySelector<HTMLElement>("[data-other-title]")!;
const sections = PLATFORMS.map((p) => document.getElementById(p)!);

function hashed(): Platform | null {
  const h = location.hash.slice(1) as Platform;
  return PLATFORMS.includes(h) ? h : null;
}

function feature(p: Platform | null) {
  for (const s of sections) {
    s.classList.remove("is-featured");
    list.append(s); // back into the list, in the original order
  }
  const s = p && document.getElementById(p);
  if (s) {
    s.classList.add("is-featured");
    slot.append(s);
  }
  otherTitle.textContent = t(s ? "dl.other" : "dl.choose");
}

const featured = hashed() ?? (PLATFORMS.includes(detected) ? detected : null);
feature(featured);
addEventListener("hashchange", () => feature(hashed() ?? (PLATFORMS.includes(detected) ? detected : null)));

// ---- iPhone / iPad: straight to the App Store --------------------------------------------------
// Any link to this page sent from an iPhone lands in the App Store. The one exception is the
// site's own "Other platforms" button (#all): that visitor wants to look around.

if (detected === "ios" && featured === "ios" && LINKS.iosAppStore && location.hash !== "#all") {
  document.querySelector<HTMLElement>("[data-redirect]")!.hidden = false;
  location.replace(LINKS.iosAppStore);
}

// ---- backdrop: a quiet stem plot, the home page's motif, drawn once ---------------------------

const svg = document.querySelector<SVGSVGElement>("[data-stems]");
if (svg) {
  const n = 96;
  const w = 1000;
  const h = 260;
  svg.setAttribute("viewBox", `0 0 ${w} ${h}`);
  svg.setAttribute("preserveAspectRatio", "none");
  const base = h * 0.62;
  let d = "";
  let heads = "";
  for (let i = 0; i < n; i++) {
    const x = ((i + 0.5) / n) * w;
    const s = 0.55 * Math.sin(i * 0.21) + 0.28 * Math.sin(i * 0.53 + 1.3) + 0.17 * Math.sin(i * 1.1 + 0.4);
    const q = Math.round(s * 7) / 7; // quantised, like real samples
    const y = base - q * h * 0.5;
    d += `M${x.toFixed(1)} ${base}V${y.toFixed(1)}`;
    heads += `M${x.toFixed(1)} ${y.toFixed(1)}h0`;
  }
  // Heads are zero-length round-capped strokes: they stay round however the SVG is stretched.
  const ns = "http://www.w3.org/2000/svg";
  for (const [cls, path] of [["stems", d], ["heads", heads]]) {
    const el = document.createElementNS(ns, "path");
    el.setAttribute("class", cls);
    el.setAttribute("d", path);
    svg.append(el);
  }
}
