"""Master images for the BitSpring icon, from ChatGPT's render (bitspring-chatgpt.webp).

Usage: python build_masters.py bitspring-chatgpt.webp <out-dir>   (needs Pillow and numpy)

The render is a finished macOS-style tile on white, slightly stretched vertically and
with a background that is a smooth top-left-to-bottom-right gradient. Every platform
needs a different framing (full-bleed for iPhone, Apple's grid for Mac, a small safe
zone for Android), so the spiral is lifted out and recomposed:

1. Undo the vertical stretch so the spiral is round.
2. Fit the tile's background with a quadratic surface, measured away from the spiral.
3. For each framing, paint that surface across the whole square and lay the spiral
   (with the background directly around it) on top through a feathered circle. The two
   backgrounds agree there, so the seam does not show.
"""
import sys
from pathlib import Path

import numpy as np
from PIL import Image, ImageChops, ImageDraw, ImageFilter

src_path, out_dir = Path(sys.argv[1]), Path(sys.argv[2])
out_dir.mkdir(parents=True, exist_ok=True)

src = Image.open(src_path).convert("RGB")
W, H = src.size
STRETCH = 676 / 707  # the spiral's measured width / height
src = src.resize((W, round(H * STRETCH)), Image.LANCZOS)
rgb = np.asarray(src).astype(np.float64)
lum = rgb.mean(axis=2)
h, w = lum.shape

# The spiral: gold is where red clearly exceeds blue.
gold = (rgb[..., 0] - rgb[..., 2]) > 60
ys, xs = np.where(gold)
CX, CY = (xs.min() + xs.max()) / 2, (ys.min() + ys.max()) / 2
SPIRAL_D = max(xs.max() - xs.min(), ys.max() - ys.min())

# The tile: dark, and well inside its edge (the rim has a bevel highlight).
tile = lum < 150
for _ in range(30):
    tile = tile & np.roll(tile, 1, 0) & np.roll(tile, -1, 0) & np.roll(tile, 1, 1) & np.roll(tile, -1, 1)
yy, xx = np.mgrid[0:h, 0:w]
dist = np.hypot(xx - CX, yy - CY)
sample = tile & (dist > SPIRAL_D / 2 + 25)


def basis(x, y):
    x, y = (x - CX) / 500, (y - CY) / 500
    return np.stack([np.ones_like(x), x, y, x * x, x * y, y * y], axis=-1)


A = basis(xx[sample], yy[sample])
coef = [np.linalg.lstsq(A, rgb[..., c][sample], rcond=None)[0] for c in range(3)]
fit = np.stack([basis(xx, yy) @ coef[c] for c in range(3)], axis=-1)
resid = np.abs(fit - rgb)[sample].mean()
LO, HI = rgb[sample].min(axis=0), rgb[sample].max(axis=0)
print(f"spiral centre ({CX:.0f}, {CY:.0f}) diameter {SPIRAL_D}; background fit error {resid:.2f}/255")


def compose(size, frac):
    """A square `size` px, background everywhere, spiral `frac` of the width."""
    k = frac * size / SPIRAL_D  # output px per source px
    oy, ox = np.mgrid[0:size, 0:size]
    sx, sy = CX + (ox + 0.5 - size / 2) / k, CY + (oy + 0.5 - size / 2) / k
    bg = np.clip(np.stack([basis(sx, sy) @ coef[c] for c in range(3)], axis=-1), LO, HI)

    art = src.transform(
        (size, size),
        Image.AFFINE,
        (1 / k, 0, CX - size / 2 / k, 0, 1 / k, CY - size / 2 / k),
        resample=Image.BICUBIC,
    )
    art = np.asarray(art).astype(np.float64)
    r = np.hypot(sx - CX, sy - CY)
    inner, feather = SPIRAL_D / 2 + 20, 18
    m = np.clip((inner + feather - r) / feather, 0, 1)[..., None]
    out = bg * (1 - m) + art * m
    return Image.fromarray(out.round().astype(np.uint8), "RGB")


def tile_icon(full, canvas, x, size, radius):
    """`full` scaled into a rounded square, with the soft drop shadow the old sources used."""
    S = 4  # supersample the mask edge
    mask = Image.new("L", (canvas * S, canvas * S), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        [x * S, x * S, (x + size) * S - 1, (x + size) * S - 1], radius * S, fill=255
    )
    mask = mask.resize((canvas, canvas), Image.LANCZOS)
    art = Image.new("RGB", (canvas, canvas))
    art.paste(full.resize((size, size), Image.LANCZOS), (x, x))

    shadow_a = ImageChops.offset(mask, 0, 10).filter(ImageFilter.GaussianBlur(14))
    shadow_a = shadow_a.point(lambda v: round(v * 0.32))
    out = Image.new("RGBA", (canvas, canvas), (0, 0, 0, 0))
    out.putalpha(shadow_a)
    tile_layer = art.convert("RGBA")
    tile_layer.putalpha(mask)
    return Image.alpha_composite(out, tile_layer)


full = compose(1024, 0.70)
full.save(out_dir / "full-bleed.png")  # iPhone, Android legacy, tauri icon input
tile_icon(full, 1024, 100, 824, 185).save(out_dir / "mac.png")  # Apple's grid
tile_icon(full, 1024, 32, 960, 190).save(out_dir / "windows.png")  # tight margins

# Android adaptive foreground: the spiral must sit inside the 66/108 safe circle. The
# layer is opaque, so the background colour only shows while the launcher animates it;
# it is set to the average of this image's border.
android = compose(1024, 0.56)
android.save(out_dir / "android-foreground.png")
a = np.asarray(android).astype(np.float64)
border = np.concatenate([a[:8], a[-8:], a[:, :8].transpose(1, 0, 2), a[:, -8:].transpose(1, 0, 2)])
edge = border.reshape(-1, 3).mean(axis=0)
print("android background", "#%02X%02X%02X" % tuple(int(round(v)) for v in edge))
