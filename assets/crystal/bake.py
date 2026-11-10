"""Bakes the account rail's crystals (ui/widgets.slint Lamp, ui/rail.slint) from the HD crystal
model that drew the Downloads gauge (crystal-gauge-art/_src/crystal_model.py).

  lamps-<h>.png    the account lamp at h px tall: 66, 99 and 132 (1x, 1.5x and 2x of the 66 px
                   lamp) and 55, 83 and 110 (the small window's 55 px lamp). Five looks across
                   (unlit, ready, in game, needs a code, error), the dark theme's row over the
                   light theme's.
  empty-<h>.png    the first-run rail's unlit crystal at 120, 180 and 240 px: dark, then light.
  mark-<h>.png     the brand mark's lit crystal at 24, 36 and 48 px: dark over light.

Each picture is the model at that exact height (Lanczos from one master render, with a fresh
exact-coverage outline), because the renderer samples a scaled picture at its nearest pixel. The
looks are the model's own lit, unlit and light-theme unlit, recoloured per state from the Theme
lamp colours (ui/theme.slint): unlit glass is lifted a little toward lamp-off, in game lifts the
blue toward ice-hi, a code is warn's amber, an error danger's coral. The dark theme's tones carry the hue; the light theme's darker tones draw
the fine rim that keeps a lit crystal's pale facets off a white rail, as the gauge's lit-light
does.

usage: python -I bake.py <crystal-gauge-art folder>     (writes beside itself)
needs: numpy, pillow, scipy
"""
import sys
from pathlib import Path

import numpy as np
from PIL import Image

HERE = Path(__file__).resolve().parent

LAMP_HEIGHTS = (66, 99, 132, 55, 83, 110)
EMPTY_HEIGHTS = (120, 180, 240)
MARK_HEIGHTS = (24, 36, 48)
MASTER_H = 1024


def rgb(text: str) -> np.ndarray:
    return np.array([int(text[i : i + 2], 16) for i in (0, 2, 4)], np.float32) / 255.0


# Theme (ui/theme.slint): dark value, light value.
ICE_HI = rgb("D2EEFB"), rgb("14587F")
FAINT = rgb("6B7C92"), rgb("7D8EA1")
WARN = rgb("F2BC62"), rgb("96650A")
DANGER = rgb("F2A090"), rgb("B4472F")
# The gauge's lit-light rim (make_gauge.py).
READY_RIM = np.array([0.07, 0.27, 0.74], np.float32)


def luma(c: np.ndarray) -> np.ndarray:
    return c @ np.array([0.2126, 0.7152, 0.0722], np.float32)


def tinted(lit: np.ndarray, alpha: np.ndarray, tone: np.ndarray) -> np.ndarray:
    """The lit crystal in another hue: its brightness carried through a ramp from a deep shade
    of `tone`, through `tone`, to white in the highlights."""
    y = luma(lit)
    inside = y[alpha > 0.5]
    # The shades deepen and saturate (the tone raised to a power), the bright half is the tone.
    lo, low, mid, hi = np.percentile(inside, [2, 40, 75, 97])
    stops = [(lo, tone**1.8 * 0.42), (low, tone**1.4 * 0.82), (mid, tone), (hi, tone + (1 - tone) * 0.6),
             (max(hi + 1e-3, 1.0), np.ones(3, np.float32))]
    xs = np.array([s for s, _ in stops])
    return np.clip(np.stack([np.interp(y, xs, [c[i] for _, c in stops]) for i in range(3)], -1), 0, 1)


def lifted(c: np.ndarray, tone: np.ndarray, amount: float = 0.38) -> np.ndarray:
    """Brighter: a screen of `tone` over the crystal."""
    return 1 - (1 - c) * (1 - amount * tone)


def unlit(dim: np.ndarray) -> np.ndarray:
    """The dark theme's unlit glass, lifted a little toward lamp-off (faint) so a signed-out
    lamp still reads at the rail's size."""
    return lifted(dim, FAINT[0], 0.22)


def rimmed(c: np.ndarray, depth: np.ndarray, tone: np.ndarray, h: int) -> np.ndarray:
    """A fine rim along the outline, about a pixel wide at any height."""
    k = (0.55 * np.exp(-((depth / 1.1) ** 2)) + 0.08 * np.exp(-depth / 3.0))[..., None]
    return c * (1 - k) + tone * k


def main() -> None:
    art = Path(sys.argv[1]).resolve()
    sys.path.insert(0, str(art / "_src"))
    import crystal_model as cm  # noqa: E402

    master = cm.render(MASTER_H, 3)

    def at(key: str, h: int) -> tuple[np.ndarray, np.ndarray]:
        return cm.scaled(master[key], h)

    def save(name: str, cells: list[list[tuple[np.ndarray, np.ndarray]]]) -> None:
        rows = [np.concatenate([cm.to_rgba8(c, a, 2.0) for c, a in row], 1) for row in cells]
        path = HERE / name
        Image.fromarray(np.concatenate(rows, 0), "RGBA").save(path, optimize=True, compress_level=9)
        print(f"{path.stat().st_size:7d} {name}")

    for h in LAMP_HEIGHTS:
        lit, alpha = at("lit", h)
        dim, _ = at("dim", h)
        dim_l, _ = at("dim_light", h)
        depth = cm.outline_depth(lit.shape[1], h)
        game = lifted(lit, ICE_HI[0])
        code = tinted(lit, alpha, WARN[0])
        error = tinted(lit, alpha, DANGER[0])
        dark = [unlit(dim), lit, game, code, error]
        light = [dim_l, rimmed(lit, depth, READY_RIM, h), rimmed(game, depth, ICE_HI[1], h),
                 rimmed(code, depth, WARN[1], h), rimmed(error, depth, DANGER[1], h)]
        save(f"lamps-{h}.png", [[(c, alpha) for c in dark], [(c, alpha) for c in light]])

    for h in EMPTY_HEIGHTS:
        dim, alpha = at("dim", h)
        dim_l, _ = at("dim_light", h)
        save(f"empty-{h}.png", [[(unlit(dim), alpha), (dim_l, alpha)]])

    for h in MARK_HEIGHTS:
        lit, alpha = at("lit", h)
        depth = cm.outline_depth(lit.shape[1], h)
        save(f"mark-{h}.png", [[(lit, alpha)], [(rimmed(lit, depth, READY_RIM, h), alpha)]])


if __name__ == "__main__":
    main()
