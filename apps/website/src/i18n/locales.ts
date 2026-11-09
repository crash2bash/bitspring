// Every language the site is published in. Shared by the build (which renders one static page
// per entry) and the browser (language switcher, number formatting, fonts).

/** Which extra font a script needs on top of Barlow (Barlow covers Latin and Vietnamese only). */
export type FontGroup = "latin" | "cyrillic" | "greek" | "arabic" | "sc" | "tc" | "jp" | "kr";

export interface Locale {
  /** BCP 47 tag for <html lang> and hreflang. */
  tag: string;
  /** URL segment and dictionary file name; "" is the site root. */
  path: string;
  /** Name in its own language, for the switcher. */
  name: string;
  dir: "ltr" | "rtl";
  font: FontGroup;
  /** Locale for Intl number formatting (Arabic keeps Western digits, as spec sheets do). */
  numbers: string;
}

const L = (tag: string, path: string, name: string, font: FontGroup = "latin", extra: Partial<Locale> = {}): Locale => ({
  tag,
  path,
  name,
  dir: "ltr",
  font,
  numbers: tag,
  ...extra,
});

export const LOCALES: Locale[] = [
  L("en", "", "English"),
  L("de", "de", "Deutsch"),
  L("fr", "fr", "Français"),
  L("es", "es", "Español"),
  L("it", "it", "Italiano"),
  L("pt-BR", "pt-br", "Português"),
  L("nl", "nl", "Nederlands"),
  L("sv", "sv", "Svenska"),
  L("da", "da", "Dansk"),
  L("nb", "nb", "Norsk"),
  L("fi", "fi", "Suomi"),
  L("pl", "pl", "Polski"),
  L("cs", "cs", "Čeština"),
  L("hu", "hu", "Magyar"),
  L("ro", "ro", "Română"),
  L("el", "el", "Ελληνικά", "greek"),
  L("tr", "tr", "Türkçe"),
  L("uk", "uk", "Українська", "cyrillic"),
  L("ru", "ru", "Русский", "cyrillic"),
  L("ar", "ar", "العربية", "arabic", { dir: "rtl", numbers: "ar-u-nu-latn" }),
  L("zh-Hans", "zh-hans", "简体中文", "sc"),
  L("zh-Hant", "zh-hant", "繁體中文", "tc"),
  L("ja", "ja", "日本語", "jp"),
  L("ko", "ko", "한국어", "kr"),
];

export const DEFAULT_LOCALE = LOCALES[0];

/** Site-relative URL of a locale's page. */
export const localeUrl = (l: Locale) => (l.path ? `/${l.path}/` : "/");

/** Best locale for a browser language tag such as "pt-PT", "zh-TW" or "no". */
export function matchLocale(tag: string): Locale | null {
  const t = tag.toLowerCase();
  const [lang, ...rest] = t.split("-");
  if (lang === "zh") {
    const traditional = rest.some((r) => r === "hant" || r === "tw" || r === "hk" || r === "mo");
    return LOCALES.find((l) => l.path === (traditional ? "zh-hant" : "zh-hans")) ?? null;
  }
  if (lang === "no" || lang === "nn" || lang === "nb") return LOCALES.find((l) => l.path === "nb") ?? null;
  return LOCALES.find((l) => l.tag.toLowerCase().split("-")[0] === lang) ?? null;
}
