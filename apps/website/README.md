# Hifi website

Landing page for the player: Vite + TypeScript, three.js for the sample field, GSAP ScrollTrigger
for scroll choreography, Lenis for smooth scrolling (including notched mouse wheels on Windows).

```bash
npm install
npm run dev      # http://127.0.0.1:5180 (and /de/, /ja/, /ar/, …)
npm run build    # static site in dist/, deploy anywhere
```

## Languages

24 languages, each a pre-rendered static page: English at `/`, the rest at `/<code>/`
(`/de/`, `/pt-br/`, `/zh-hans/`, `/ar/`, …). The list lives in `src/i18n/locales.ts`, the
texts in `src/i18n/locales/<code>.json` (`en.json` is the source; missing keys fall back to it).

- `index.html` is a template: `{{key}}` is replaced with escaped text, `<!--@block-->` with
  generated markup (hreflang links, the language menu, runtime strings). See `src/i18n/render.ts`.
- The root page sends first-time visitors to their browser language; picking a language in the
  menu is remembered and stops that.
- Arabic is right-to-left: layout uses logical CSS properties and the 3D camera is mirrored.
- Barlow covers Latin only. Cyrillic and Greek use IBM Plex Sans, Arabic IBM Plex Sans Arabic,
  CJK Noto Sans SC/TC/JP/KR; each page loads only its own script's font.

## Before release

- Store links: `LINKS` in `src/config.ts` (until set, buttons read "Coming to …").
- Site origin: `SITE_URL` in `src/config.ts`, for absolute hreflang/canonical/og URLs.

`public/app.webp` is a screenshot of the desktop UI (mock data, 1120×740 @2x).
