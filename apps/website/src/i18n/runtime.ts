// Shared by every page: runtime strings, per-script fonts and the language menu.
// The page itself arrives already translated (see render.ts); only strings that JS writes at
// runtime come from the inline dictionary.

import { localeUrl, LOCALES, type FontGroup } from "./locales";

export const root = document.documentElement;
const runtime = JSON.parse(document.getElementById("i18n")?.textContent ?? "{}") as {
  numbers?: string;
  strings?: Record<string, string>;
};
export const t = (key: string) => runtime.strings?.[key] ?? key;
export const numberLocale = runtime.numbers ?? root.lang;
export const isRtl = root.dir === "rtl";
export const currentLocale = LOCALES.find((l) => l.tag === root.lang) ?? LOCALES[0];
/** Path of this page below the language root: "" (home) or "download/". */
export const page = root.dataset.page ?? "";
export const downloadUrl = localeUrl(currentLocale) + "download/";

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
export const fontGroup = (root.dataset.font ?? "latin") as FontGroup;
export const fontsReady = (SCRIPT_FONTS[fontGroup] ?? SCRIPT_FONTS.latin)();

const langSelect = document.querySelector<HTMLSelectElement>("[data-lang-select]");
const langCurrent = document.querySelector<HTMLElement>("[data-lang-current]");
if (langSelect && langCurrent) {
  langCurrent.textContent = (currentLocale.path || "en").split("-")[0].toUpperCase();
  langSelect.addEventListener("change", () => {
    const next = LOCALES.find((l) => l.path === langSelect.value);
    if (!next) return;
    try {
      localStorage.setItem("lang", next.path); // the root page stops auto-redirecting
    } catch {}
    location.href = localeUrl(next) + page + location.search + location.hash;
  });
}
