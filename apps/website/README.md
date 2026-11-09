# Hifi website

Landing page for the player: Vite + TypeScript, three.js for the sample field, GSAP ScrollTrigger
for scroll choreography, Lenis for smooth scrolling (including notched mouse wheels on Windows).

```bash
npm install
npm run dev      # http://127.0.0.1:5180
npm run build    # static site in dist/, deploy anywhere
```

Store links live in `src/config.ts`; until they are set, the download buttons read "Скоро …".
`public/app.webp` is a screenshot of the desktop UI (mock data, 1120×740 @2x).
