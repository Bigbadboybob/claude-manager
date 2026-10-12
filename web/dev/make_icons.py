"""Generate the PWA icons (no image libraries): a rounded speech bubble with
three dots on the app's dark background. Run: python3 web/dev/make_icons.py"""
import math
import struct
import zlib
from pathlib import Path

BG = (17, 20, 27)        # --bg
BUBBLE = (88, 166, 255)  # --accent
DOT = (17, 20, 27)


def png(size: int, maskable: bool) -> bytes:
    rows = []
    pad = 0.18 if maskable else 0.12  # maskable icons keep a safe zone
    x0, y0, x1, y1 = pad, pad + 0.04, 1 - pad, 1 - pad - 0.12
    r = 0.12
    for py in range(size):
        row = bytearray([0])
        for px in range(size):
            u, v = (px + 0.5) / size, (py + 0.5) / size
            # Rounded rectangle body.
            cx = min(max(u, x0 + r), x1 - r)
            cy = min(max(v, y0 + r), y1 - r)
            inside = math.hypot(u - cx, v - cy) <= r and x0 <= u <= x1 and y0 <= v <= y1
            # Tail: a triangle under the lower-left of the bubble.
            tx, ty = x0 + 0.14, y1
            if not inside and ty <= v <= ty + 0.12:
                t = (v - ty) / 0.12
                inside = tx <= u <= tx + 0.14 * (1 - t)
            colour = BUBBLE if inside else BG
            # Three dots.
            for k in range(3):
                dx = 0.5 + (k - 1) * 0.17
                if math.hypot(u - dx, v - (y0 + y1) / 2) <= 0.055:
                    colour = DOT
            row += bytes(colour)
        rows.append(bytes(row))
    raw = zlib.compress(b"".join(rows), 9)

    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", raw) + chunk(b"IEND", b""))


out = Path(__file__).resolve().parent.parent / "static" / "icons"
out.mkdir(parents=True, exist_ok=True)
for name, size, maskable in [("icon-192.png", 192, False), ("icon-512.png", 512, False),
                             ("icon-maskable-512.png", 512, True), ("apple-touch-icon.png", 180, True)]:
    (out / name).write_bytes(png(size, maskable))
    print(name)
