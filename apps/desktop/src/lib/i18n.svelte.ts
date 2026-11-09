// UI language: one flat dictionary per locale (src/lib/locales/<code>.json), English as the
// source and fallback. The language follows the OS unless picked by hand in Settings.
//
// A dictionary value is either a string with {named} placeholders, or — where a count is
// shown — an object of CLDR plural forms ({ one, few, many, other, … }) picked with
// Intl.PluralRules, so "1 трек / 3 трека / 5 треков" and Arabic's six forms come out right.

export type PluralForms = Partial<Record<Intl.LDMLPluralRule, string>>;
type Dict = Record<string, string | PluralForms>;

export interface Locale {
  /** Dictionary file name and the value stored for a manual choice. */
  code: string;
  /** BCP 47 tag for <html lang>, Intl collation and plural rules. */
  tag: string;
  /** Name in its own language, for the picker. */
  name: string;
  dir: "ltr" | "rtl";
  /** Locale for number formatting (Arabic keeps Western digits, as spec sheets do). */
  numbers: string;
}

const L = (code: string, tag: string, name: string, extra: Partial<Locale> = {}): Locale => ({
  code,
  tag,
  name,
  dir: "ltr",
  numbers: tag,
  ...extra,
});

/** The same 24 languages as the website, in the same order. */
export const LOCALES: Locale[] = [
  L("en", "en", "English"),
  L("de", "de", "Deutsch"),
  L("fr", "fr", "Français"),
  L("es", "es", "Español"),
  L("it", "it", "Italiano"),
  L("pt-br", "pt-BR", "Português"),
  L("nl", "nl", "Nederlands"),
  L("sv", "sv", "Svenska"),
  L("da", "da", "Dansk"),
  L("nb", "nb", "Norsk"),
  L("fi", "fi", "Suomi"),
  L("pl", "pl", "Polski"),
  L("cs", "cs", "Čeština"),
  L("hu", "hu", "Magyar"),
  L("ro", "ro", "Română"),
  L("el", "el", "Ελληνικά"),
  L("tr", "tr", "Türkçe"),
  L("uk", "uk", "Українська"),
  L("ru", "ru", "Русский"),
  L("ar", "ar", "العربية", { dir: "rtl", numbers: "ar-u-nu-latn" }),
  L("zh-hans", "zh-Hans", "简体中文"),
  L("zh-hant", "zh-Hant", "繁體中文"),
  L("ja", "ja", "日本語"),
  L("ko", "ko", "한국어"),
];
const EN = LOCALES[0];

const DICTS: Record<string, Dict> = Object.fromEntries(
  Object.entries(import.meta.glob<Dict>("./locales/*.json", { eager: true, import: "default" })).map(([path, d]) => [
    path.replace(/^.*\/|\.json$/g, ""),
    d,
  ]),
);

/** Best locale for an OS/browser language tag such as "pt-PT", "zh-TW", "no" or "sr-Latn". */
export function matchLocale(tag: string): Locale | null {
  const [lang, ...rest] = tag.toLowerCase().replace(/_/g, "-").split("-");
  if (lang === "zh") {
    const traditional = rest.some((r) => r === "hant" || r === "tw" || r === "hk" || r === "mo");
    return LOCALES.find((l) => l.code === (traditional ? "zh-hant" : "zh-hans")) ?? null;
  }
  if (lang === "no" || lang === "nn" || lang === "nb") return LOCALES.find((l) => l.code === "nb") ?? null;
  return LOCALES.find((l) => l.tag.toLowerCase().split("-")[0] === lang) ?? null;
}

/** First of the user's languages that we have, else English. */
function pick(tags: readonly string[]): Locale {
  for (const t of tags) {
    const l = matchLocale(t);
    if (l) return l;
  }
  return EN;
}

const PREF_KEY = "language";
function loadPref(): string {
  try {
    const v = localStorage.getItem(PREF_KEY);
    return v && LOCALES.some((l) => l.code === v) ? v : "auto";
  } catch {
    return "auto";
  }
}

const browserTags = typeof navigator !== "undefined" ? (navigator.languages?.length ? navigator.languages : [navigator.language]) : [];

const state = $state({
  /** "auto" (follow the OS) or a locale code. */
  pref: loadPref(),
  /** The OS's language as we best know it. */
  system: pick(browserTags),
});

const current = $derived(state.pref === "auto" ? state.system : (LOCALES.find((l) => l.code === state.pref) ?? EN));
const plurals = $derived(new Intl.PluralRules(current.tag));
const listSep = $derived(lookup("list.sep") as string);

export const i18n = {
  get locale(): Locale {
    return current;
  },
  /** The language the OS asks for (shown next to "System" in the picker). */
  get system(): Locale {
    return state.system;
  },
  get pref(): string {
    return state.pref;
  },
  set pref(v: string) {
    state.pref = v;
    try {
      localStorage.setItem(PREF_KEY, v);
    } catch {}
  },
};

/** Use the OS's own language list (from the backend, which knows it better than the web view). */
export function setSystemLanguages(tags: readonly string[]) {
  if (tags.length) state.system = pick(tags);
}

// Keep <html lang dir> in step, for screen readers, hyphenation and right-to-left layout.
$effect.root(() => {
  $effect(() => {
    document.documentElement.lang = current.tag;
    document.documentElement.dir = current.dir;
  });
});

function lookup(key: string): string | PluralForms | undefined {
  return DICTS[current.code]?.[key] ?? DICTS.en?.[key];
}

function fill(s: string, params?: Record<string, string | number>): string {
  if (!params) return s;
  return s.replace(/\{(\w+)\}/g, (m, k) => (k in params ? String(params[k]) : m));
}

/** Translate `key`, filling {placeholders}. Numbers in params are used as given — format them with num(). */
export function t(key: string, params?: Record<string, string | number>): string {
  const v = lookup(key);
  if (typeof v === "string") return fill(v, params);
  if (v) return fill(v.other ?? Object.values(v)[0] ?? key, params);
  if (import.meta.env.DEV) console.warn(`i18n: missing key ${key}`);
  return key;
}

/** Translate a counted phrase: picks the plural form for `n`, which fills {n} (formatted). */
export function tn(key: string, n: number, params?: Record<string, string | number>): string {
  const v = lookup(key);
  const all = { n: num(n), ...params };
  if (v && typeof v === "object") {
    const form = v[plurals.select(n)] ?? v.other ?? "";
    return fill(form, all);
  }
  return t(key, all);
}

/** A number in the UI language's style ("44.1", "44,1"). */
export function num(n: number, opts: Intl.NumberFormatOptions = { maximumFractionDigits: 1 }): string {
  return n.toLocaleString(current.numbers, opts);
}

/** Join list items the way the UI language does (", " / "、" / "، "). */
export function list(items: (string | null | undefined | false)[]): string {
  return items.filter(Boolean).join(listSep);
}

/** Run sentences together: with a space, except in Chinese and Japanese, which use none. */
export function sentences(items: (string | null | undefined | false)[]): string {
  return items.filter(Boolean).join(/^(zh|ja)/.test(current.code) ? "" : " ");
}

/** Sorting by the UI language's alphabet, digits compared as numbers. */
export function collator(): Intl.Collator {
  return new Intl.Collator(current.tag, { numeric: true, sensitivity: "base" });
}
