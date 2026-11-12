# BitSpring icon sources

The mark is a gold vortex: thin strands that leave an outer ring and curl into a small central hole,
like a spring seen end-on. It was drawn by ChatGPT's image model (`bitspring-chatgpt.webp`, a
finished macOS-style tile on white) and every icon is built from that one render.

`build_masters.py` lifts the spiral out of the render and recomposes it for each platform. It
undoes a slight vertical stretch in the render (so the spiral is round), fits the tile's gradient
background with a smooth surface, and lays the spiral back onto that surface at whatever size a
platform needs. It writes four 1024 × 1024 masters:

- `full-bleed.png`: square, no corners, spiral at 70% of the width. iPhone, Android legacy
  launcher icons, Windows Store squares, and the website's home-screen icon.
- `mac.png`: Apple's macOS grid (824 px rounded square on 1024, soft shadow). icon.icns and the
  32/64/128/256/icon.png desktop files.
- `windows.png`: the same tile with tight margins. icon.ico and the website favicon.
- `android-foreground.png`: spiral at 56%, inside the adaptive-icon safe circle. The background
  colour (res/values/ic_launcher_background.xml) is the average of this image's border, #1C2830.

To regenerate:

1. `python build_masters.py bitspring-chatgpt.webp <out>` (needs Pillow and numpy).
2. `npx tauri icon <out>/<master>.png -o <tmp>` (from apps/desktop) once per master, then copy:
   icons/32x32, 64x64, 128x128, 128x128@2x, icon.png and icon.icns from the Mac set; icon.ico from
   the Windows set; Square*Logo.png and StoreLogo.png, the Android ic_launcher and
   ic_launcher_round PNGs, and the iPhone AppIcon PNGs from the full-bleed set.
3. Re-encode the iPhone PNGs as plain RGB: the App Store refuses an icon with an alpha channel.
4. Resize android-foreground.png to 108/162/216/324/432 px into each mipmap folder as
   ic_launcher_foreground.png.
5. Website: favicon.png is windows.png at 128 px, apple-touch-icon.png is full-bleed.png at 180 px.

At 16 px the strands merge into a gold disc with a dark centre; at 32 px it reads as a ring with
a hole. That is expected for a mark this fine.

The website's header mark (apps/website/index.html, download/, privacy/) is a hand-drawn
seven-strand version of the same vortex: a ring and seven half-circles, each leaving the ring
tangentially and landing on the hole, all turning the same way as the icon. og.png is the site's
hero with that header mark.
