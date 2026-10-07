#!/usr/bin/env python3
"""Generate every app icon XGView ships from one full-bleed master PNG.

The master is authored once and full-bleed: a square that fills its canvas edge
to edge, with no rounded corners and no transparent margin. Each platform then
gets the treatment it actually needs, because they do not agree on what an icon
is and which of them re-shape it:

    Windows (.ico)         rounded corners - Windows draws the bitmap as-is
    Linux (hicolor PNG)    same rounded corners as Windows
    macOS (.icns/iconset)  artwork scaled and centered with a margin, then
                           clipped to a squircle, matching the platform's icons
    Android legacy mipmap  rounded corners; only API < 26 ever shows these
    Android adaptive fg    full-bleed and square: the launcher masks it, and a
                           corner drawn here would survive as a second curve
                           where that mask cuts across it
    Android TV banner      center-cropped to 320x180, no corners
    Google Play 512        full-bleed; the store applies its own mask

The corner radius, the macOS margin and the squircle exponent are all
parameters (see --help), so the shape is tuned without touching the master.
numpy, when installed, makes the macOS squircle exact; without it the script
falls back to a rounded rectangle close enough to pass at icon sizes.

Outputs:

    assets/icons/windows/xgview.ico          16/24/32/48/64/128/256 (PNG entries)
    assets/icons/linux/hicolor/<N>x<N>/apps/xgview.png
                                             16/24/32/48/64/128/256/512
    assets/icons/macos/xgview.icns           PNG-carrying icns, 32..1024
    assets/icons/macos/xgview.iconset/*.png  for `iconutil -c icns`
    assets/icons/android/play_store_512.png  Google Play listing icon
    crates/monitor_android/android/res/mipmap-<density>/...
                                             launcher, round and adaptive layers

The sources are fixed:

    assets/xgview.png          1024x1024 (or larger) full-bleed square master
    assets/xgview-banner.png   1920x1080 (16:9) Android TV banner

so a plain `python scripts/make-icons.py` regenerates every icon from them.
`--source` / `--banner` override the fixed paths for a one-off run.

`--blank` takes the place of `--source` and writes the same set of files, all
of them fully transparent: it is how the repository's placeholder artwork is
generated, and how it is regenerated from scratch. Under `--blank` the banner
is a transparent placeholder too, and the fixed banner source is ignored. Do
not run it once real artwork is in place - it overwrites it.

Requirements: Python 3 with Pillow (`pip install pillow`); numpy optional.

Examples (forward slashes work on every platform, including Windows):
    python scripts/make-icons.py
    python scripts/make-icons.py --corner-radius 0.22 --macos-margin 0.08
    python scripts/make-icons.py --source artwork/draft.png
    python scripts/make-icons.py --blank
"""

from __future__ import annotations

import argparse
import functools
import io
import struct
import sys
from dataclasses import dataclass
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageOps

try:
    import numpy as np
except ImportError:  # the macOS squircle falls back to a rounded rectangle
    np = None

APP_NAME = "xgview"

# The artwork lives in the repository, one file of each aspect ratio, and every
# icon derives from it: `python scripts/make-icons.py` with no arguments is the
# whole workflow. Only `assets/icons/` is generated; these two are inputs.
DEFAULT_SOURCE = Path("assets") / "xgview.png"
DEFAULT_BANNER = Path("assets") / "xgview-banner.png"

# Sizes are pixels. Nothing here should be changed without changing the
# referencing platform resource (the .rc, the .desktop file, the manifest).
LINUX_SIZES = (16, 24, 32, 48, 64, 128, 256, 512)
WINDOWS_ICO_SIZES = (16, 24, 32, 48, 64, 128, 256)

# Icon Composer type code -> pixel size. The modern PNG-carrying codes are used
# throughout; the duplicated sizes are the 1x and 2x entries macOS registers
# under different logical scales, and both are expected.
MACOS_ICNS_ENTRIES = (
    ("ic11", 32),    # 16pt @2x
    ("ic12", 64),    # 32pt @2x
    ("ic07", 128),
    ("ic13", 256),   # 128pt @2x
    ("ic08", 256),
    ("ic14", 512),   # 256pt @2x
    ("ic09", 512),
    ("ic10", 1024),  # 512pt @2x
)

# The names `iconutil` expects in a .iconset directory, by pixel size.
MACOS_ICONSET = {
    16: ("icon_16x16.png",),
    32: ("icon_16x16@2x.png", "icon_32x32.png"),
    64: ("icon_32x32@2x.png",),
    128: ("icon_128x128.png",),
    256: ("icon_128x128@2x.png", "icon_256x256.png"),
    512: ("icon_256x256@2x.png", "icon_512x512.png"),
    1024: ("icon_512x512@2x.png",),
}

# density -> (legacy launcher px, adaptive foreground canvas px). The adaptive
# canvas is 108dp; the visible artwork is only the middle 72dp of it.
ANDROID_DENSITIES = {
    "mdpi": (48, 108),
    "hdpi": (72, 162),
    "xhdpi": (96, 216),
    "xxhdpi": (144, 288),
    "xxxhdpi": (192, 432),
}
ADAPTIVE_CONTENT_RATIO = 72 / 108

TV_BANNER_SIZE = (320, 180)
PLAY_STORE_ICON_SIZE = 512

# Above this the master is assumed to be detailed enough for every size; below
# it the 1024 (and possibly 512) output is upscaled and looks soft.
RECOMMENDED_MASTER = 1024

# Shape defaults, all as a fraction of the side. 18% is the modern desktop
# rounded rectangle; a 10% margin fills 80% of the canvas the way macOS icons
# do; exponent 5 is close to the continuous-curvature squircle Apple uses (2
# would be a plain ellipse, larger tends toward a square).
DEFAULT_CORNER_RADIUS = 0.18
DEFAULT_MACOS_MARGIN = 0.10
DEFAULT_MACOS_SQUIRCLE = 5.0

# Masks are drawn at this multiple and downsampled, PIL drawing no
# anti-aliased shape of its own; hard corners are visible at 128px and above.
SUPERSAMPLE = 4

# How a rounded rectangle approximates the squircle when numpy is unavailable:
# the corner radius macOS uses for the same shape, relative to the side.
ROUNDED_RECT_SQUIRCLE_RATIO = 0.222


@dataclass(frozen=True)
class Style:
    """How the full-bleed master is re-shaped for each platform."""

    corner_radius: float
    macos_margin: float
    macos_squircle: float


def log(message: str) -> None:
    print(f"  {message}")


def step(message: str) -> None:
    print(f"==> {message}")


def load_square(path: Path) -> Image.Image:
    """Open `path` as a square RGBA image, refusing anything else.

    A non-square master would either be cropped or stretched silently, and both
    are decisions the artwork's author should make, not this script.
    """
    try:
        image = Image.open(path)
    except FileNotFoundError:
        raise SystemExit(f"source image not found: {path}") from None
    image = image.convert("RGBA")
    width, height = image.size
    if width != height:
        raise SystemExit(
            f"source image must be square, got {width}x{height}: {path}\n"
            "export it square (full-bleed) and run again"
        )
    if width < RECOMMENDED_MASTER:
        print(
            f"warning: source is {width}x{width}, below the recommended "
            f"{RECOMMENDED_MASTER}x{RECOMMENDED_MASTER}; large icons will be upscaled",
            file=sys.stderr,
        )
    return image


def resize(image: Image.Image, size: int) -> Image.Image:
    if image.size == (size, size):
        return image
    return image.resize((size, size), Image.LANCZOS)


def png_bytes(image: Image.Image) -> bytes:
    buffer = io.BytesIO()
    image.save(buffer, format="PNG", optimize=True)
    return buffer.getvalue()


def save_png(image: Image.Image, path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    image.save(path, format="PNG", optimize=True)


def apply_mask(image: Image.Image, mask: Image.Image) -> Image.Image:
    """Intersect the image's own alpha with `mask`, keeping the smaller of the two.

    Multiplying rather than replacing keeps any transparency the artwork already
    has - a soft edge stays soft instead of being cut to a hard shape.
    """
    out = image.copy()
    out.putalpha(ImageChops.multiply(out.getchannel("A"), mask))
    return out


def rounded_mask(size: int, radius_ratio: float) -> Image.Image:
    supersampled = size * SUPERSAMPLE
    radius = max(0.0, min(radius_ratio * supersampled, supersampled / 2))
    mask = Image.new("L", (supersampled, supersampled), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (0, 0, supersampled - 1, supersampled - 1), radius=radius, fill=255
    )
    return mask.resize((size, size), Image.LANCZOS)


def squircle_mask(size: int, exponent: float) -> Image.Image:
    """A superellipse |x|^n + |y|^n <= 1, the shape macOS icons are cut to."""
    supersampled = size * SUPERSAMPLE
    if np is not None:
        coords = np.linspace(-1.0, 1.0, supersampled, dtype=np.float32)
        powers = np.abs(coords) ** exponent
        inside = (powers[:, None] + powers[None, :]) <= 1.0
        mask = Image.fromarray(np.where(inside, 255, 0).astype("uint8"), "L")
    else:
        print(
            "warning: numpy not installed; approximating the macOS squircle "
            "with a rounded rectangle",
            file=sys.stderr,
        )
        radius = ROUNDED_RECT_SQUIRCLE_RATIO * supersampled
        mask = Image.new("L", (supersampled, supersampled), 0)
        ImageDraw.Draw(mask).rounded_rectangle(
            (0, 0, supersampled - 1, supersampled - 1), radius=radius, fill=255
        )
    return mask.resize((size, size), Image.LANCZOS)


def ellipse_mask(size: int) -> Image.Image:
    supersampled = size * SUPERSAMPLE
    mask = Image.new("L", (supersampled, supersampled), 0)
    ImageDraw.Draw(mask).ellipse((0, 0, supersampled - 1, supersampled - 1), fill=255)
    return mask.resize((size, size), Image.LANCZOS)


def rounded(image: Image.Image, radius_ratio: float) -> Image.Image:
    """`image` with rounded corners, sized to its own square canvas."""
    return apply_mask(image, rounded_mask(image.size[0], radius_ratio))


def circular(image: Image.Image) -> Image.Image:
    """Clip `image` to a circle, the shape a round launcher icon draws."""
    return apply_mask(image, ellipse_mask(image.size[0]))


def macos_icon(master: Image.Image, size: int, style: Style) -> Image.Image:
    """One macOS icon: squircle-clipped artwork on a transparent canvas.

    macOS leaves its icons sitting on a transparent margin rather than filling
    the frame, so the artwork is scaled into the middle and the rest stays
    empty - a full-bleed square would read as oversized among system icons.
    """
    content = max(1, round(size * (1 - 2 * style.macos_margin)))
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    artwork = apply_mask(
        resize(master, content), squircle_mask(content, style.macos_squircle)
    )
    offset = (size - content) // 2
    canvas.alpha_composite(artwork, (offset, offset))
    return canvas


def adaptive_foreground(image: Image.Image, canvas: int) -> Image.Image:
    """Center the full-bleed artwork on a transparent adaptive-icon canvas.

    A launcher may mask the outer 18dp of the 108dp canvas, so the artwork only
    fills the inner 72dp. No corner of our own is drawn: the launcher's mask is
    the only edge, and anything drawn here would end up as a second curve
    wherever that mask cuts across it.
    """
    content = max(1, round(canvas * ADAPTIVE_CONTENT_RATIO))
    layer = Image.new("RGBA", (canvas, canvas), (0, 0, 0, 0))
    offset = (canvas - content) // 2
    layer.alpha_composite(resize(image, content), (offset, offset))
    return layer


def write_icns(render, path: Path) -> None:
    """Write an .icns wrapping PNG data for each registered size.

    `render(size)` supplies one already-shaped icon. The container is assembled
    by hand rather than through Pillow's ICNS writer, which only emits a fixed
    subset of the type codes and would drop the 1024 entry the Retina instance
    needs.
    """
    body = bytearray()
    for type_code, size in MACOS_ICNS_ENTRIES:
        data = png_bytes(render(size))
        body += type_code.encode("ascii")
        body += struct.pack(">I", len(data) + 8)  # length includes this header
        body += data

    container = bytearray(b"icns")
    container += struct.pack(">I", len(body) + 8)
    container += body

    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(container)


def write_ico(image: Image.Image, path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    image.save(path, format="ICO", sizes=[(s, s) for s in WINDOWS_ICO_SIZES])


def write_windows(master: Image.Image, repo: Path, style: Style) -> None:
    step(f"Windows (ico: 16..256, corner radius {style.corner_radius:.0%})")
    path = repo / "assets" / "icons" / "windows" / f"{APP_NAME}.ico"
    write_ico(rounded(master, style.corner_radius), path)
    log(str(path.relative_to(repo)))


def write_linux(master: Image.Image, repo: Path, style: Style) -> None:
    step(f"Linux (hicolor PNG: 16..512, corner radius {style.corner_radius:.0%})")
    artwork = rounded(master, style.corner_radius)
    for size in LINUX_SIZES:
        path = (
            repo
            / "assets"
            / "icons"
            / "linux"
            / "hicolor"
            / f"{size}x{size}"
            / "apps"
            / f"{APP_NAME}.png"
        )
        save_png(resize(artwork, size), path)
        log(str(path.relative_to(repo)))


def write_macos(master: Image.Image, repo: Path, style: Style) -> None:
    step(
        f"macOS (icns + iconset, content {1 - 2 * style.macos_margin:.0%}, "
        f"squircle n={style.macos_squircle:g})"
    )

    # Each entry is rendered at its own size so the squircle is rasterised at
    # full resolution instead of being downsampled from one large master; the
    # cache keeps the duplicated 256 / 512 entries from being drawn twice.
    @functools.lru_cache(maxsize=None)
    def render(size: int) -> Image.Image:
        return macos_icon(master, size, style)

    icns = repo / "assets" / "icons" / "macos" / f"{APP_NAME}.icns"
    write_icns(render, icns)
    log(str(icns.relative_to(repo)))

    iconset = repo / "assets" / "icons" / "macos" / f"{APP_NAME}.iconset"
    for size, names in MACOS_ICONSET.items():
        for name in names:
            save_png(render(size), iconset / name)
    log(f"{iconset.relative_to(repo)}/ ({len(MACOS_ICONSET)} sizes)")


def write_android(master: Image.Image, repo: Path, style: Style) -> None:
    step("Android (legacy rounded; adaptive full-bleed, masked by the launcher)")
    res = repo / "crates" / "monitor_android" / "android" / "res"
    legacy_art = rounded(master, style.corner_radius)
    for density, (legacy, canvas) in ANDROID_DENSITIES.items():
        directory = res / f"mipmap-{density}"
        save_png(resize(legacy_art, legacy), directory / "ic_launcher.png")
        save_png(circular(resize(master, legacy)), directory / "ic_launcher_round.png")
        save_png(
            adaptive_foreground(master, canvas), directory / "ic_launcher_foreground.png"
        )
    log(f"{res.relative_to(repo)}/mipmap-* ({len(ANDROID_DENSITIES)} densities)")

    step("Google Play listing icon (512, full-bleed)")
    play = repo / "assets" / "icons" / "android" / "play_store_512.png"
    save_png(resize(master, PLAY_STORE_ICON_SIZE), play)
    log(str(play.relative_to(repo)))


def banner_path(repo: Path) -> Path:
    return (
        repo
        / "crates"
        / "monitor_android"
        / "android"
        / "res"
        / "drawable-xhdpi"
        / "ic_launcher_banner.png"
    )


def write_banner(banner_source: Path, repo: Path) -> None:
    step("Android TV banner (320x180)")
    try:
        banner = Image.open(banner_source).convert("RGBA")
    except FileNotFoundError:
        raise SystemExit(f"banner image not found: {banner_source}") from None

    width, height = banner.size
    if abs(width / height - 16 / 9) > 0.01:
        print(
            f"warning: banner is {width}x{height}, not 16:9; it will be "
            "center-cropped to 320x180",
            file=sys.stderr,
        )
    fitted = ImageOps.fit(banner, TV_BANNER_SIZE, Image.LANCZOS)

    path = banner_path(repo)
    save_png(fitted, path)
    log(str(path.relative_to(repo)))


def write_blank_banner(repo: Path) -> None:
    step("Android TV banner (320x180, blank)")
    path = banner_path(repo)
    save_png(Image.new("RGBA", TV_BANNER_SIZE, (0, 0, 0, 0)), path)
    log(str(path.relative_to(repo)))


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Generate every XGView app icon from one full-bleed master PNG.",
    )
    parser.add_argument(
        "--source",
        type=Path,
        help="full-bleed square master PNG (1024x1024 or larger recommended). "
        f"Default: {DEFAULT_SOURCE} (relative to --repo).",
    )
    parser.add_argument(
        "--blank",
        action="store_true",
        help="write transparent placeholders instead of deriving from a master; "
        "takes the place of --source.",
    )
    parser.add_argument(
        "--banner",
        type=Path,
        help="Android TV banner PNG, 16:9 (1920x1080 or similar). "
        f"Default: {DEFAULT_BANNER} (relative to --repo), skipped if missing; "
        "blank under --blank.",
    )
    parser.add_argument(
        "--corner-radius",
        type=float,
        default=DEFAULT_CORNER_RADIUS,
        metavar="RATIO",
        help="corner radius of the Windows / Linux icons and Android's legacy "
        "launcher, as a fraction of the side (0 is square). "
        f"Default: {DEFAULT_CORNER_RADIUS}.",
    )
    parser.add_argument(
        "--macos-margin",
        type=float,
        default=DEFAULT_MACOS_MARGIN,
        metavar="RATIO",
        help="transparent margin around the macOS artwork, per side, as a "
        f"fraction of the side. Default: {DEFAULT_MACOS_MARGIN}.",
    )
    parser.add_argument(
        "--macos-squircle",
        type=float,
        default=DEFAULT_MACOS_SQUIRCLE,
        metavar="N",
        help="exponent of the macOS squircle superellipse: 2 is an ellipse, "
        f"larger tends toward a square. Default: {DEFAULT_MACOS_SQUIRCLE:g}.",
    )
    parser.add_argument(
        "--repo",
        type=Path,
        default=Path(__file__).resolve().parent.parent,
        help="repository root (default: the script's grandparent directory)",
    )
    return parser.parse_args(argv)


def style_from_args(args: argparse.Namespace) -> Style:
    if not 0.0 <= args.corner_radius <= 0.5:
        raise SystemExit("--corner-radius must be between 0 and 0.5")
    if not 0.0 <= args.macos_margin < 0.5:
        raise SystemExit("--macos-margin must be between 0 (inclusive) and 0.5")
    if args.macos_squircle <= 0:
        raise SystemExit("--macos-squircle must be positive")
    return Style(
        corner_radius=args.corner_radius,
        macos_margin=args.macos_margin,
        macos_squircle=args.macos_squircle,
    )


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv if argv is not None else sys.argv[1:])
    repo = args.repo.resolve()
    style = style_from_args(args)

    if args.blank and args.source is not None:
        raise SystemExit("--blank and --source are mutually exclusive")

    if args.blank:
        master = Image.new("RGBA", (RECOMMENDED_MASTER, RECOMMENDED_MASTER), (0, 0, 0, 0))
        print(f"master: blank {RECOMMENDED_MASTER}x{RECOMMENDED_MASTER} transparent")
    else:
        source = args.source if args.source is not None else repo / DEFAULT_SOURCE
        master = load_square(source)
        print(f"master: {source} ({master.size[0]}x{master.size[1]})")

    write_windows(master, repo, style)
    write_linux(master, repo, style)
    write_macos(master, repo, style)
    write_android(master, repo, style)

    if args.banner is not None:
        write_banner(args.banner, repo)
    elif args.blank:
        write_blank_banner(repo)
    else:
        banner = repo / DEFAULT_BANNER
        if banner.exists():
            write_banner(banner, repo)
        else:
            print(f"banner: {banner} not found, existing Android TV banner kept")

    print()
    step("Done - rebuild to pick the icons up (an incremental build is enough)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())