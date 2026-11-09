// Checks every UI dictionary against the English source: same keys, same {placeholders},
// and every plural form the language's CLDR rules need. Run: npm run check:locales
import { readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const dir = join(dirname(fileURLToPath(import.meta.url)), "../src/lib/locales");
const TAGS = { "pt-br": "pt-BR", "zh-hans": "zh-Hans", "zh-hant": "zh-Hant" };
const EXPECTED = ["en", "de", "fr", "es", "it", "pt-br", "nl", "sv", "da", "nb", "fi", "pl", "cs", "hu", "ro", "el", "tr", "uk", "ru", "ar", "zh-hans", "zh-hant", "ja", "ko"];

const load = (code) => JSON.parse(readFileSync(join(dir, `${code}.json`), "utf8"));
const en = load("en");
const holes = (s) => [...s.matchAll(/\{(\w+)\}/g)].map((m) => m[1]).sort().join(",");
const forms = (v) => (typeof v === "string" ? [v] : Object.values(v));

let errors = 0;
const fail = (code, msg) => {
  errors++;
  console.error(`${code}: ${msg}`);
};

const present = readdirSync(dir).filter((f) => f.endsWith(".json")).map((f) => f.slice(0, -5));
for (const code of EXPECTED) if (!present.includes(code)) fail(code, "dictionary missing");

for (const code of present) {
  const d = load(code);
  const rules = new Intl.PluralRules(TAGS[code] ?? code).resolvedOptions().pluralCategories;
  for (const key of Object.keys(en)) {
    const v = d[key];
    if (v === undefined) {
      fail(code, `missing ${key}`);
      continue;
    }
    const src = en[key];
    if (typeof src !== typeof v) {
      fail(code, `${key}: should be ${typeof src === "string" ? "a string" : "plural forms"}`);
      continue;
    }
    // Every form must use the same placeholders as the English (plural forms may drop {n}
    // where the language spells the count out, e.g. Arabic "one" / "two").
    const want = holes(forms(src)[forms(src).length - 1]);
    for (const f of forms(v)) {
      if (typeof f !== "string" || !f.trim()) fail(code, `${key}: empty`);
      const have = holes(f);
      const ok = typeof src === "string" ? have === want : have === want || have === want.replace(/(^|,)n(,|$)/, "$1$2").replace(/^,|,$/g, "");
      if (!ok) fail(code, `${key}: placeholders {${have}} instead of {${want}}`);
    }
    if (typeof v === "object") {
      for (const r of rules) if (!(r in v)) fail(code, `${key}: missing plural form "${r}"`);
      for (const r of Object.keys(v)) if (!rules.includes(r)) fail(code, `${key}: "${r}" is not a plural form of ${code}`);
    }
  }
  for (const key of Object.keys(d)) if (!(key in en)) fail(code, `unknown key ${key}`);
  if (d["signal.bitPerfect"] !== "Bit-perfect") fail(code, `signal.bitPerfect must stay "Bit-perfect"`);
}

if (errors) {
  console.error(`\n${errors} problem(s)`);
  process.exit(1);
}
console.log(`${present.length} dictionaries OK`);
