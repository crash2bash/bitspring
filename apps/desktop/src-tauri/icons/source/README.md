# BitSpring icon sources

The mark is a B drawn as sampled audio: a spine, and horizontal sample stems whose heads trace the
outline of a bold B (flat top bar, upper bowl, waist, wider lower bowl, flat bottom).

- `bitspring.svg`: full-bleed square. iPhone App Store icon and Android launcher icons, through
  `npx tauri icon` (from apps/desktop). The iPhone PNGs must stay without an alpha channel.
- `bitspring-mac.svg`: Apple's macOS grid (824 px rounded square on 1024, soft shadow). Source of
  icon.icns and the 32/128/256/icon.png files.
- `bitspring-windows.svg`: the same tile with tight margins. Source of icon.ico.
- `bitspring-android-foreground.svg`: the artwork shrunk into the adaptive-icon safe circle; the
  background colour is #121D23 (res/values/ic_launcher_background.xml).

The website's header mark and favicon are a seven-sample version of the same B
(apps/website/index.html and public/favicon.png).

To regenerate: render each SVG at 1024 × 1024 (e.g. headless Chrome), run `npx tauri icon` on the
full-bleed PNG, then replace icon.icns / icon.ico / the desktop PNGs from the Mac and Windows
renders, re-encode the iPhone PNGs as plain RGB, and resize the Android foreground into each
mipmap folder.
