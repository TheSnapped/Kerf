"""Renders the Kerf icon: a waveform sliced by a glowing blade.

Writes kerf.ico (16-256 px, PNG-compressed entries), kerf.png (256 px, used for
the window icon) and kerf_preview.png (512 px). Needs only numpy.

    python assets/make_icon.py
"""
import struct
import zlib
from pathlib import Path

import numpy as np

OUT = Path(__file__).parent


def hexrgb(h):
    return np.array([int(h[i:i + 2], 16) / 255 for i in (1, 3, 5)])


BG_TOP, BG_BOTTOM = hexrgb("#2a1d63"), hexrgb("#0c0a22")
GLOW = hexrgb("#7b5cff")
WAVE_STOPS = [hexrgb("#2ef2c8"), hexrgb("#4fa8ff"), hexrgb("#b56bff"), hexrgb("#ff5fa8")]
BLADE_GLOW = hexrgb("#ffb35c")


def gradient(stops, t):
    t = np.clip(t, 0, 1) * (len(stops) - 1)
    i = np.minimum(t.astype(int), len(stops) - 2)
    f = (t - i)[..., None]
    a = np.array(stops)[i]
    b = np.array(stops)[i + 1]
    return a + (b - a) * f


def coverage(d, px):
    """Anti-aliased coverage from a signed distance (normalized units)."""
    return np.clip(0.5 - d / px, 0, 1)


def sd_round_box(x, y, cx, cy, hw, hh, r):
    qx = np.abs(x - cx) - (hw - r)
    qy = np.abs(y - cy) - (hh - r)
    outside = np.hypot(np.maximum(qx, 0), np.maximum(qy, 0))
    return outside + np.minimum(np.maximum(qx, qy), 0) - r


def sd_vcapsule(x, y, cx, y0, y1, r):
    cy = np.clip(y, y0, y1)
    return np.hypot(x - cx, y - cy) - r


def over(dst_rgb, dst_a, src_rgb, src_a):
    src_a = src_a[..., None]
    out_a = src_a + dst_a[..., None] * (1 - src_a)
    rgb = (src_rgb * src_a + dst_rgb * dst_a[..., None] * (1 - src_a)) / np.maximum(out_a, 1e-6)
    return rgb, out_a[..., 0]


def render(size, small):
    """`small` selects the simplified design (fewer, bolder bars) for tiny icons."""
    px = 1.0 / size
    c = (np.arange(size) + 0.5) / size
    x, y = np.meshgrid(c, c)

    # Rounded-square tile
    margin = 0.03 if small else 0.045
    tile = sd_round_box(x, y, 0.5, 0.5, 0.5 - margin, 0.5 - margin, 0.23)
    tile_a = coverage(tile, px)
    rgb = BG_TOP + (BG_BOTTOM - BG_TOP) * y[..., None]
    # Soft violet glow behind the waveform, and a faint top sheen.
    glow = np.exp(-((x - 0.42) ** 2 + (y - 0.48) ** 2) / 0.09)[..., None]
    rgb = rgb + GLOW * 0.35 * glow
    rgb = rgb + 0.06 * np.clip(1 - y / 0.35, 0, 1)[..., None]
    alpha = tile_a.copy()

    # The cut: a line through the centre, tilted like a blade stroke.
    angle = np.radians(62)
    dx, dy = np.cos(angle), -np.sin(angle)  # along the blade (pointing up-right)
    nx, ny = -dy, dx                         # normal
    s = (x - 0.5) * nx + (y - 0.5) * ny      # signed side of the cut
    along = (x - 0.5) * dx + (y - 0.5) * dy

    # Right half slides along the blade, as if the two pieces were pushed apart.
    slide = 0.05 if not small else 0.06
    right = s > 0
    wx = np.where(right, x - dx * slide, x)
    wy = np.where(right, y - dy * slide, y)

    # Waveform bars
    heights = [0.16, 0.30, 0.48, 0.66, 0.54, 0.74, 0.44, 0.28, 0.14] if not small else [0.30, 0.62, 0.70, 0.46, 0.24]
    n = len(heights)
    span = 0.62 if not small else 0.64
    pitch = span / (n - 1)
    radius = pitch * (0.30 if not small else 0.36)
    wave_d = np.full(x.shape, 9.0)
    for i, h in enumerate(heights):
        cx = 0.5 - span / 2 + i * pitch
        wave_d = np.minimum(wave_d, sd_vcapsule(wx, wy, cx, 0.5 - h / 2 + radius, 0.5 + h / 2 - radius, radius))
    gap = 0.045 if not small else 0.09
    # Bars everywhere except inside the kerf (the gap the blade leaves).
    wave_a = coverage(wave_d, px) * coverage(gap / 2 - np.abs(s), px)
    wave_rgb = gradient(WAVE_STOPS, (wx - 0.18) / 0.64)
    # Slight vertical shading on the bars for depth.
    wave_rgb = wave_rgb * (1.08 - 0.25 * np.abs(wy - 0.5))[..., None]
    rgb, alpha = over(rgb, alpha, np.clip(wave_rgb, 0, 1), wave_a * tile_a)

    # Blade: hot white core with an amber/pink glow, fading toward its ends.
    length = 0.40
    ends = np.clip(1 - (np.abs(along) / length) ** 4, 0, 1)
    if not small:
        halo = np.exp(-(s / 0.03) ** 2) * ends * 0.55
        rgb, alpha = over(rgb, alpha, np.broadcast_to(BLADE_GLOW, rgb.shape), halo * tile_a)
    core_w = 0.006 if not small else 0.03
    core = coverage(np.abs(s) - core_w, px) * ends
    rgb, alpha = over(rgb, alpha, np.broadcast_to(np.array([1.0, 0.97, 0.92]), rgb.shape), core * tile_a)

    rgba = np.dstack([np.clip(rgb, 0, 1), np.clip(alpha, 0, 1)])
    return (rgba * 255 + 0.5).astype(np.uint8)


def render_ss(size, ss=4):
    """Supersampled render, box-filtered down: smoother glow and bar edges."""
    big = render(size * ss, size <= 32).astype(np.float64) / 255
    a = big[..., 3:4]
    premul = np.dstack([big[..., :3] * a, a])
    small = premul.reshape(size, ss, size, ss, 4).mean(axis=(1, 3))
    rgb = small[..., :3] / np.maximum(small[..., 3:4], 1e-6)
    out = np.dstack([rgb, small[..., 3:4]])
    return (np.clip(out, 0, 1) * 255 + 0.5).astype(np.uint8)


def png_bytes(rgba):
    h, w, _ = rgba.shape
    raw = b"".join(b"\x00" + rgba[row].tobytes() for row in range(h))

    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))


def ico_bytes(images):
    header = struct.pack("<HHH", 0, 1, len(images))
    entries, blobs = b"", b""
    offset = 6 + 16 * len(images)
    for size, png in images:
        dim = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(png), offset)
        blobs += png
        offset += len(png)
    return header + entries + blobs


if __name__ == "__main__":
    sizes = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]
    images = [(s, png_bytes(render_ss(s, 8 if s <= 64 else 4))) for s in sizes]
    (OUT / "kerf.ico").write_bytes(ico_bytes(images))
    (OUT / "kerf.png").write_bytes(dict(images)[256])
    (OUT / "kerf_preview.png").write_bytes(png_bytes(render_ss(512, 2)))
    print("wrote", ", ".join(f"{s}px" for s in sizes))
