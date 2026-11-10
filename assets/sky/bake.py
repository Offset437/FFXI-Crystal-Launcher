"""Bakes the Home hero's derived pictures (ui/sky.slint). Slint's software renderer has no
filters or blend modes, blends in 8-bit steps without dithering and samples a scaled image at
its nearest pixel, so everything it can't do well at run time is done here, once.

  sky-<phase>.png                  the sky's gradient, smoothed and dithered (RGB). Phases:
                                   night, dawn, day, dusk and light (the light theme's sky)
  glow-<phase>.png                 the glow behind the party, dithered (RGBA): the top 5/8 of
                                   its square, all the band ever shows
  fade-<phase>.png                 the band's foot melting into the page (the same)
  crowd-<w>.png                    the party's mask (crowd-mask.png, the portal art's alpha) at
                                   the hero's width at 1x, 1.5x and 2x: 860/1290/1720, and the
                                   small window's 640/960 (at 2x it takes 1290, within 1 %)
  crowd-solid-<w>.png              the same party as a solid silhouette, for the bright skies
  wordmark-{dark,light}-<w>.png    the FFXI wordmark at 1x, 1.5x and 2x: 252/378/504 wide, and
                                   the small window's 204/306/408
  stars-<w>.png                    the star field at 1.5x and 2x (stars.png is 1x)
  soft-<phase>.png                 the hero's elliptical falloff around the party (alpha, tinted
                                   with the window colour at run time)

Why the dithering. The gradients run over hundreds of pixels between colours a few 8-bit steps
apart (the night sky's blue climbs 34 steps over the band), so the renderer's own gradients and
alpha blends show every step as a band. Each sky, glow and fade is computed in floating point and
rounded to 8 bits through a blue-noise threshold, which breaks the steps into grain finer than a
step. A fade's alpha is dithered by as much as it takes to move the colour under it by one step
(row by row, from the colours it fades between), so it neither bands on a dark sky nor grains
on a bright one. Nearest-pixel scaling keeps the grain, so one picture serves every display
scale; the gradient and fade pictures are stretched to the band.

Why the stop curve. A CSS gradient is straight between stops, so its slope jumps at each stop and
the eye sees a line there (Mach bands). The sky's colour profile is the CSS one, smoothed with a
Gaussian a few percent of the band tall, which keeps every stop's colour where it was in all but
the corners. The foot fade is a smoothstep for the same reason.

Why a second silhouette. The art's alpha is line work at about 80 % strength with open areas
(faces, cloth, armour) and it thins out toward the ground. On the night sky that reads as a dark
shape. On the bright dawn, day and dusk skies the open areas show the sky at full strength, and
at the hero's size the party reads as a lit, see-through drawing; the approved sky-times render
shows it at half that size, where it reads as a dark silhouette. The solid mask keeps the art's
outline, line work and fade toward the ground, brings the line work close to full strength and
fills the open areas inside the outline to just under it, so only faint relief is left.

The colours and geometry below mirror ui/sky.slint; change both together.

usage: python -I bake.py            (run from anywhere; reads and writes beside itself)
needs: numpy, pillow, scipy
"""
from pathlib import Path

import numpy as np
from PIL import Image
from scipy import ndimage as ndi

SKY = Path(__file__).resolve().parent
ASSETS = SKY.parent

# The solid silhouette's strength: the alpha the line work reaches, its gain over the art's own
# level, and how far the open areas inside the outline are filled (1 = flat). The art thins out
# toward the ground; the silhouette does too, but later (the art's fade raised to LINE_FADE,
# the fill by FILL_FADE more), so it stays a dark shape down to the band's own fade.
PEAK = 0.95
GAIN = 1.15
FILL = 0.9
LINE_FADE = 0.6
FILL_FADE = 0.6

# The band (Theme.sky-h) and the party (SkyBackdrop's crowd-*), normal and small window.
BAND_H = 404
CROWD = {"normal": (860, 334, 30, 36), "small": (640, 249, 34, 30)}  # w, h, right, foot
SCALES = (1.0, 1.5, 2.0)


def hex_rgba(text: str) -> tuple[np.ndarray, float]:
    text = text.lstrip("#")
    rgb = np.array([int(text[i : i + 2], 16) for i in (0, 2, 4)], np.float64)
    return rgb, (int(text[6:8], 16) / 255.0 if len(text) == 8 else 1.0)


BG = {"dark": hex_rgba("0A0F18")[0], "light": hex_rgba("F4F7FA")[0]}

# Each sky as ui/sky.slint draws it: the stops (colour, position), the glow's centre and middle
# colours and the middle's place, whether it follows the sky-times tile (dawn, day, dusk), and
# the party's colour and opacity.
PHASES = {
    "night": dict(stops=[("050914", 0.0), ("091327", 0.50), ("0D1B36", 0.82)], glow=("56A0FF80", "4080EC36", 0.46),
                  tile=False, crowd=("04070D", 1.0), theme="dark"),
    "dawn": dict(stops=[("141B3F", 0.0), ("4A3A6E", 0.40), ("B9727E", 0.70), ("E9A982", 0.88)], glow=("FFCEA09E", "FFAA8238", 0.5),
                 tile=True, crowd=("0B0A16", 1.0), theme="dark"),
    "day": dict(stops=[("0B2A55", 0.0), ("1B5590", 0.44), ("5C9CD2", 0.80)], glow=("CEE8FF99", "96C8FF38", 0.5),
                tile=True, crowd=("06101F", 1.0), theme="dark"),
    "dusk": dict(stops=[("0B1030", 0.0), ("27215A", 0.36), ("7A3F73", 0.64), ("E08A5B", 0.86)], glow=("FFD69694", "FFB47833", 0.5),
                 tile=True, crowd=("0A0A1C", 1.0), theme="dark"),
    "light": dict(stops=[("A9D3F2", 0.0), ("CBE4F7", 0.44), ("E9F3FB", 0.78)], glow=("FFFFFFF2", "FFFFFF80", 0.46),
                  tile=False, crowd=("234B69", 0.62), theme="light"),
}

# The sky picture's size: as tall as the band at 1.5x (the renderer scales it to the band), and
# wide enough that stretching it never draws the grain out into dashes.
SKY_W, SKY_H = 1024, 606
# The glow is drawn as a square, but the band never shows below its top 5/8.
GLOW_N, GLOW_TOP = 512, 0.625
FADE_W, FADE_H = 2048, 128
SMOOTH = 0.035  # the stop curve's Gaussian, in band heights
# The glow's alpha grain, in steps peak to peak. While the sky changes the incoming glow is drawn
# at a fraction of its opacity, and the renderer scales its 8-bit alpha by that fraction with
# truncation, which would swallow a one-step grain and bring the rings back. Two and a half
# steps survive down to about a third; at full opacity they move the colour by about a step.
GLOW_SPREAD = 2.5


def blue_noise(n: int = 64, sigma: float = 1.9, seed: int = 7) -> np.ndarray:
    """An n x n blue-noise threshold map in [0, 1) (void and cluster, periodic)."""
    rng = np.random.default_rng(seed)
    d = np.minimum(np.arange(n), n - np.arange(n)).astype(np.float64)
    kernel = np.exp(-(d[:, None] ** 2 + d[None, :] ** 2) / (2 * sigma * sigma))

    def energy(points: np.ndarray) -> np.ndarray:
        return np.real(np.fft.ifft2(np.fft.fft2(points) * np.fft.fft2(kernel)))

    pattern = rng.random((n, n)) < 0.1
    # Even the initial points out: move the tightest cluster to the largest void until stable.
    while True:
        e = energy(pattern.astype(np.float64))
        cluster = np.unravel_index(np.argmax(np.where(pattern, e, -np.inf)), e.shape)
        pattern[cluster] = False
        e = energy(pattern.astype(np.float64))
        void = np.unravel_index(np.argmin(np.where(pattern, np.inf, e)), e.shape)
        if void == cluster:
            pattern[cluster] = True
            break
        pattern[void] = True
    ones = int(pattern.sum())
    rank = np.zeros((n, n), np.int64)
    # Rank the initial points from the tightest cluster down, then fill the voids upward.
    work = pattern.copy()
    e = energy(work.astype(np.float64))
    for r in range(ones - 1, -1, -1):
        cluster = np.unravel_index(np.argmax(np.where(work, e, -np.inf)), e.shape)
        work[cluster] = False
        e -= np.roll(np.roll(kernel, cluster[0], 0), cluster[1], 1)
        rank[cluster] = r
    work = pattern.copy()
    e = energy(work.astype(np.float64))
    for r in range(ones, n * n):
        void = np.unravel_index(np.argmin(np.where(work, np.inf, e)), e.shape)
        work[void] = True
        e += np.roll(np.roll(kernel, void[0], 0), void[1], 1)
        rank[void] = r
    return (rank.astype(np.float64) + 0.5) / (n * n)


NOISE = blue_noise()


def noise(h: int, w: int, offset: int = 0) -> np.ndarray:
    """The threshold map tiled over h x w; `offset` decorrelates the channels."""
    n = NOISE.shape[0]
    tiled = np.tile(NOISE, (h // n + 2, w // n + 2))
    o = (offset * 23) % n, (offset * 37) % n
    return tiled[o[0] : o[0] + h, o[1] : o[1] + w]


def dither(values: np.ndarray, spread: np.ndarray | float = 1.0, offset: int = 0) -> np.ndarray:
    """0..255 floats to 8 bits through the threshold map, `spread` steps peak to peak."""
    h, w = values.shape
    # Never wider than the room to 0 and 255: clipped grain would shift the mean (a fade that
    # ends with grain left over shows where its picture stops).
    spread = np.minimum(spread, 2.0 * np.minimum(values, 255.0 - values) + 1.0)
    return np.clip(np.floor(values + (noise(h, w, offset) - 0.5) * spread + 0.5), 0, 255).astype(np.uint8)


def gradient_geometry(tile: bool, crowd_w: float, crowd_foot: float, band_h: float) -> tuple[float, float, float]:
    """SkyBackdrop's start, run and foot: `.sky`'s gradient is the band; the tile's is taller,
    starts above the band and ends `foot` above its foot."""
    if not tile:
        return 0.0, 1.0, 0.0
    t = crowd_w / 420.0
    foot = max(0.0, 20.0 * t - crowd_foot)
    run = max(1.0, 214.0 * t / band_h)
    start = min(0.0, 1.0 - foot / band_h - run)
    return start, run, foot


def sky_profile(phase: str, rows: int) -> np.ndarray:
    """The sky's colour down the band (rows x 3, 0..255), smoothed at the stops."""
    p = PHASES[phase]
    w, _, _, foot = CROWD["normal"]
    start, run, _ = gradient_geometry(p["tile"], w, foot, BAND_H)
    stops = [(hex_rgba(c)[0], start + s * run) for c, s in p["stops"]] + [(BG[p["theme"]], start + run)]
    # Sampled on a longer run (the part above the band too), so the smoothing has real colour
    # to draw on at the top edge; the window colour carries on below.
    pad = 0.25
    y = (np.arange(int(rows * (1 + 2 * pad))) + 0.5) / rows - pad
    pos = np.array([s for _, s in stops])
    col = np.stack([np.interp(y, pos, [c[i] for c, _ in stops]) for i in range(3)], 1)
    col = ndi.gaussian_filter1d(col, SMOOTH * rows, axis=0, mode="nearest")
    top = int(rows * pad)
    return col[top : top + rows]


def glow_rgba(phase: str, n: int) -> np.ndarray:
    """CSS's radial-gradient(circle closest-side, c0, c1 mid, transparent), premultiplied as CSS
    mixes it, straight-alpha out, dithered (n x n x 4, 8-bit)."""
    c0, c1, mid = PHASES[phase]["glow"]
    (rgb0, a0), (rgb1, a1) = hex_rgba(c0), hex_rgba(c1)
    yy, xx = np.mgrid[0:n, 0:n]
    r = np.hypot(xx + 0.5 - n / 2, yy + 0.5 - n / 2) / (n / 2)
    p0 = np.append(rgb0 * a0, a0)
    p1 = np.append(rgb1 * a1, a1)
    u = np.clip(r / mid, 0, 1)[..., None]
    v = np.clip((r - mid) / (1 - mid), 0, 1)[..., None]
    pre = np.where((r < mid)[..., None], p0 * (1 - u) + p1 * u, p1 * (1 - v))
    alpha = pre[..., 3]
    rgb = np.where(alpha[..., None] > 1e-6, pre[..., :3] / np.maximum(alpha, 1e-6)[..., None], rgb1)
    out = np.dstack([dither(rgb[..., i], 1.0, i) for i in range(3)] + [dither(alpha * 255.0, GLOW_SPREAD, 3)])
    out[..., 3][r >= 1] = 0
    return out


def spread_for(diff: np.ndarray) -> np.ndarray:
    """How many alpha steps of grain move a colour `diff` steps away by one step."""
    return np.clip(255.0 / np.maximum(diff, 1e-3), 1.0, 24.0)


def fade_alpha(phase: str) -> np.ndarray:
    """The foot: transparent to the window colour (a smoothstep), solid on the last row. The
    grain follows the larger of the sky's and the party's distance from the window colour."""
    p = PHASES[phase]
    bg = BG[p["theme"]]
    w, _, _, foot = CROWD["normal"]
    _, _, tile_foot = gradient_geometry(p["tile"], w, foot, BAND_H)
    fade = 40.0 * w / 420.0 if p["tile"] else 80.0
    sky = sky_profile(phase, BAND_H)
    top = BAND_H - tile_foot - fade
    rows = np.clip(top + (np.arange(FADE_H) + 0.5) / FADE_H * fade, 0, BAND_H - 1).astype(int)
    crowd_rgb, crowd_a = hex_rgba(p["crowd"][0])[0], p["crowd"][1]
    party = crowd_rgb * crowd_a + sky[rows] * (1 - crowd_a)
    diff = np.maximum(np.abs(sky[rows] - bg).max(1), np.abs(party - bg).max(1))
    t = (np.arange(FADE_H) + 0.5) / FADE_H
    alpha = np.tile((t * t * (3 - 2 * t) * 255.0)[:, None], (1, FADE_W))
    out = dither(alpha, spread_for(diff)[:, None], 6)
    out[-1] = 255
    return out


# ---- The hero's soft light ----
#
# One elliptical falloff around the party (soft-<phase>.png, an alpha mask tinted with the
# window colour), so the sky meets the bar, the rail, the window's edge and the page with the
# same soft edge.

# The light's box: SOFT_W x BAND_H logical pixels, its right edge on the band's (it scales
# with the party in the small window). The centre sits behind the party; the radii are where
# the sky is gone, to the left, right, up and down; inside SOFT_CORE of them it is whole.
SOFT_W = 1180
SOFT_CENTRE = (SOFT_W - 400.0, 0.62 * BAND_H)
SOFT_RADII = (780.0, 560.0, 262.0, 330.0)  # left, right, up, down
SOFT_CORE = 0.12
SOFT_SCALE = 0.5  # texels per logical pixel
# The day sky is the brightest, so its falloff reads as a spotlight at the same radii: it reaches a
# tenth further to the right, up and down (not left, under the words) and eases in (alpha to this power), so the light thins into the page rather than
# stopping at an edge.
SOFT_DAY_REACH = 1.10
SOFT_DAY_EASE = 1.3


def smoothstep(t: np.ndarray) -> np.ndarray:
    t = np.clip(t, 0.0, 1.0)
    return t * t * (3.0 - 2.0 * t)


def soft_alpha(phase: str) -> np.ndarray:
    """The light's mask: the window colour's alpha over the light's box, clear around the party
    and whole along an ellipse, with the grain the sky under each row needs."""
    w, h = round(SOFT_W * SOFT_SCALE), round(BAND_H * SOFT_SCALE)
    y = ((np.arange(h) + 0.5) / SOFT_SCALE)[:, None]
    x = ((np.arange(w) + 0.5) / SOFT_SCALE)[None, :]
    dx, dy = x - SOFT_CENTRE[0], y - SOFT_CENTRE[1]
    reach = SOFT_DAY_REACH if phase == "day" else 1.0
    # The left stays put: the words sit there, and more light under them costs contrast.
    left, right, up, down = SOFT_RADII[0], *(v * reach for v in SOFT_RADII[1:])
    r = np.hypot(dx / np.where(dx < 0, left, right), dy / np.where(dy < 0, up, down))
    alpha = smoothstep((r - SOFT_CORE) / (1.0 - SOFT_CORE))
    alpha = (alpha ** SOFT_DAY_EASE if phase == "day" else alpha) * 255.0
    bg = BG[PHASES[phase]["theme"]]
    diff = np.abs(sky_profile(phase, h) - bg).max(1)
    out = dither(alpha, spread_for(diff)[:, None], 7)
    out[:, 0] = 255
    return out


def save_mask(alpha: np.ndarray, path: Path) -> None:
    """White with the given alpha (0..1 floats or 8-bit), as a grey+alpha PNG."""
    a = alpha if alpha.dtype == np.uint8 else np.clip(alpha * 255.0 + 0.5, 0, 255).astype(np.uint8)
    Image.fromarray(np.dstack([np.full_like(a, 255), a]), "LA").save(path, optimize=True)


def resized(alpha: np.ndarray, size: tuple[int, int]) -> np.ndarray:
    image = Image.fromarray(alpha.astype(np.float32), "F")
    return np.clip(np.asarray(image.resize(size, Image.LANCZOS)), 0.0, 1.0)


def envelope(art: np.ndarray) -> np.ndarray:
    """The art's fade toward the ground: each row band's line-work level (its 80th percentile),
    smoothed, never rising toward the bottom, as a fraction of the level at the top."""
    height = art.shape[0]
    level = np.full(height, np.nan, np.float32)
    for y in range(height):
        band = art[max(0, y - 12) : y + 13]
        band = band[band > 0.02]
        if band.size > 200:
            level[y] = np.percentile(band, 80)
    rows = np.arange(height)
    known = ~np.isnan(level)
    level = np.interp(rows, rows[known], level[known])
    level = ndi.gaussian_filter1d(level, 10)
    level = np.maximum.accumulate(level[::-1])[::-1]
    return level


def solid(art: np.ndarray) -> np.ndarray:
    level = envelope(art)
    top = float(level[:250].mean())
    fade = np.clip(level / top, 0.0, 1.0)[:, None]
    # The outline: everything the art touches, small gaps closed and enclosed areas filled,
    # softened by under a pixel so the resize has an edge to work with.
    inside = ndi.binary_closing(art > 0.03, structure=ndi.generate_binary_structure(2, 1), iterations=3)
    inside = ndi.gaussian_filter(ndi.binary_fill_holes(inside).astype(np.float32), 0.8)
    # The line work at full strength where the art has it at its own level.
    lines = np.clip(art / (top * np.maximum(fade, 0.12)) * GAIN, 0.0, 1.0)
    fill = inside * FILL * fade**FILL_FADE
    return np.clip(PEAK * fade**LINE_FADE * (fill + (1.0 - fill) * lines), 0.0, 1.0)


def premultiplied_resize(image: Image.Image, size: tuple[int, int], method: int) -> Image.Image:
    """Resized through premultiplied alpha, so the clear pixels' colour doesn't fringe the edges."""
    return image.convert("RGBa").resize(size, method).convert("RGBA")


def main() -> None:
    for phase in PHASES:
        col = sky_profile(phase, SKY_H)
        sky = np.dstack([dither(np.tile(col[:, i : i + 1], (1, SKY_W)), 1.0, i) for i in range(3)])
        Image.fromarray(sky, "RGB").save(SKY / f"sky-{phase}.png", optimize=True)
        Image.fromarray(glow_rgba(phase, GLOW_N)[: round(GLOW_N * GLOW_TOP)], "RGBA").save(SKY / f"glow-{phase}.png", optimize=True)
        save_mask(fade_alpha(phase), SKY / f"fade-{phase}.png")
        save_mask(soft_alpha(phase), SKY / f"soft-{phase}.png")

    art = np.asarray(Image.open(SKY / "crowd-mask.png").convert("LA").getchannel("A")).astype(np.float32) / 255.0
    full = solid(art)
    for name, (w, h, _, _) in CROWD.items():
        for scale in SCALES if name == "normal" else SCALES[:2]:
            size = (round(w * scale), round(h * scale))
            save_mask(resized(art, size), SKY / f"crowd-{size[0]}.png")
            save_mask(resized(full, size), SKY / f"crowd-solid-{size[0]}.png")

    for theme in ("dark", "light"):
        mark = Image.open(ASSETS / f"portal-wordmark-{theme}.png").convert("RGBA")
        # Hamming: of Pillow's filters the nearest to Chromium's own downscale in the renders.
        for width, height in ((252, 52), (204, 42)):
            for scale in SCALES:
                size = (round(width * scale), round(height * scale))
                premultiplied_resize(mark, size, Image.HAMMING).save(SKY / f"wordmark-{theme}-{size[0]}.png", optimize=True)

    stars = Image.open(SKY / "stars.png").convert("RGBA")
    for scale in SCALES[1:]:
        size = (round(stars.width * scale), round(stars.height * scale))
        premultiplied_resize(stars, size, Image.BICUBIC).save(SKY / f"stars-{size[0]}.png", optimize=True)

    for path in sorted(SKY.glob("*.png")):
        print(f"{path.stat().st_size:8d} {path.name}")


if __name__ == "__main__":
    main()
