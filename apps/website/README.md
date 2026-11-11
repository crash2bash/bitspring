# BitSpring website

Landing page for the player: Vite + TypeScript, three.js for the sample field, GSAP ScrollTrigger
for scroll choreography, Lenis for smooth scrolling (including notched mouse wheels on Windows).

```bash
npm install
npm run dev      # http://127.0.0.1:5180 (and /download/, /de/, /ja/download/, …)
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

## Downloads

`/download/` (and `/<code>/download/`) puts the build for the visitor's device first
(`src/platform.ts`):

- iPhone / iPad: straight to the App Store. The exception is the site's own "Other platforms"
  button, which links to `/download/#all` so iPhone visitors can look around.
- Mac: a choice between the Mac App Store and the .dmg file.
- Windows: the .exe installer; Android: Google Play or .apk.
- `/download/#windows` etc. features that platform; `?platform=mac` overrides detection.

The home page's main button does the same in one step (iPhone → App Store, Windows → .exe,
Mac → the choice on `/download/`).

## Hosting (Vercel)

`vercel.json` at the repository root builds this folder, so importing the repo into Vercel needs
no settings. Deploys are skipped when a commit doesn't touch `apps/website`.

## Before release

- Store and file links: `LINKS` in `src/config.ts` (until set, buttons read "Coming …").
- Site origin: `SITE_URL` in `src/config.ts`, for absolute hreflang/canonical/og URLs.

`public/app.webp` is a screenshot of the desktop UI (mock data, 1120×740 @2x).
