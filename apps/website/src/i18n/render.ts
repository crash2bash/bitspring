// Build-time page rendering (runs in Node, from vite.config.ts). index.html is a template:
//   {{key}}          → the translated string, HTML-escaped (safe in text and attributes)
//   <!--@name-->     → generated markup (alternate links, language options, …)
// Every language gets a real static page, so search engines and link previews see translated
// text without running any JavaScript.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import { DEFAULT_LOCALE, LOCALES, localeUrl, type Locale } from "./locales";

type Dict = Record<string, string>;

/** Strings main.ts needs at runtime; shipped inline so there is no extra request. */
const RUNTIME_KEYS = /^(cta\.(appstore|windows)_soon|rates\.)/;

const dir = fileURLToPath(new URL("./locales/", import.meta.url));

export function loadDict(l: Locale): Dict {
  return JSON.parse(readFileSync(`${dir}${l.path || "en"}.json`, "utf8"));
}

const escapeHtml = (s: string) =>
  s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

const escapeJson = (v: unknown) => JSON.stringify(v).replace(/</g, "\\u003c");

/** Absolute URL when the site origin is known (hreflang and og:url want absolute ones). */
const abs = (site: string, path: string) => (site ? site.replace(/\/$/, "") + path : path);

// Runs before first paint on the root page only: send visitors to their language unless they
// already picked one in the switcher. Mirrors matchLocale() in locales.ts.
function redirectScript(): string {
  const paths = LOCALES.map((l) => l.path).filter(Boolean);
  return `<script>(function(){var P=${escapeJson(paths)};function m(t){t=t.toLowerCase();var l=t.split("-")[0];if(l==="zh")return /-(hant|tw|hk|mo)\\b/.test(t)?"zh-hant":"zh-hans";if(l==="no"||l==="nn")l="nb";if(l==="pt")return"pt-br";if(l==="en")return"";return P.indexOf(l)>=0?l:null}var p=null;try{p=localStorage.getItem("lang")}catch(e){}if(p===null){var ls=navigator.languages||[navigator.language||""];for(var i=0;i<ls.length&&p===null;i++)p=m(ls[i]||"")}if(p&&P.indexOf(p)>=0)location.replace("/"+p+"/"+location.hash)})()</script>`;
}

export function renderPage(template: string, locale: Locale, opts: { site: string }): string {
  const dict = loadDict(locale);
  const fallback = locale === DEFAULT_LOCALE ? dict : loadDict(DEFAULT_LOCALE);
  const t = (key: string) => {
    const v = dict[key] ?? fallback[key];
    if (v === undefined) throw new Error(`i18n: missing key "${key}" (${locale.tag})`);
    return v;
  };

  const runtime: Dict = {};
  for (const k of Object.keys(fallback)) if (RUNTIME_KEYS.test(k)) runtime[k] = t(k);

  const blocks: Record<string, string> = {
    alternates: [
      ...LOCALES.map((l) => `<link rel="alternate" hreflang="${l.tag}" href="${abs(opts.site, localeUrl(l))}" />`),
      `<link rel="alternate" hreflang="x-default" href="${abs(opts.site, "/")}" />`,
      `<link rel="canonical" href="${abs(opts.site, localeUrl(locale))}" />`,
    ].join("\n    "),
    og: [
      `<meta property="og:url" content="${abs(opts.site, localeUrl(locale))}" />`,
      `<meta property="og:image" content="${abs(opts.site, "/og.png")}" />`,
      `<meta property="og:locale" content="${locale.tag.replace("-", "_")}" />`,
    ].join("\n    "),
    redirect: locale === DEFAULT_LOCALE ? redirectScript() : "",
    "lang-options": LOCALES.map(
      (l) =>
        `<option value="${l.path}" lang="${l.tag}"${l === locale ? " selected" : ""}>${escapeHtml(l.name)}</option>`,
    ).join(""),
    runtime: `<script type="application/json" id="i18n">${escapeJson({ numbers: locale.numbers, strings: runtime })}</script>`,
  };

  return template
    .replace(/<html[^>]*>/, `<html lang="${locale.tag}" dir="${locale.dir}" data-font="${locale.font}">`)
    .replace(/<!--@([\w-]+)-->/g, (_, name: string) => {
      if (!(name in blocks)) throw new Error(`i18n: unknown block "${name}"`);
      return blocks[name];
    })
    .replace(/\{\{\s*([\w.]+)\s*\}\}/g, (_, key: string) => escapeHtml(t(key)));
}
