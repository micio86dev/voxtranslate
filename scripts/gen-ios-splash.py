#!/usr/bin/env python3
"""Generate Apple PWA startup images (apple-touch-startup-image).

Renders a rounded white app-icon tile (from client/public/icon.png) centred on
the app's dark shell colour (#0a0b10) — matching the Android splash — at every
modern iPhone/iPad resolution, in portrait and landscape.

Pure-Python PNG for the antialiased rounded master, then `sips` (native, fast)
to resize + centre-pad per device. No third-party deps. macOS only (sips).

Run from the repo root:  python3 scripts/gen-ios-splash.py
"""
import binascii
import os
import struct
import subprocess
import zlib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ICON = os.path.join(ROOT, "client", "public", "icon.png")
OUTDIR = os.path.join(ROOT, "client", "public", "splash")
MASTER = os.path.join(OUTDIR, "_master.png")  # temp, removed at the end
BG = (10, 11, 16)  # #0a0b10 — the app shell --bg
BG_HEX = "0A0B10"

# (device-width pt, device-height pt, device-pixel-ratio) in portrait.
# Covers the current iPhone/iPad line-up plus recent models.
DEVICES = [
    (440, 956, 3),    # iPhone 16 Pro Max
    (402, 874, 3),    # iPhone 16 Pro
    (430, 932, 3),    # iPhone 15/14 Pro Max, 15 Plus
    (393, 852, 3),    # iPhone 16, 15/14 Pro, 15
    (428, 926, 3),    # iPhone 14 Plus, 13/12 Pro Max
    (390, 844, 3),    # iPhone 14/13/12 (+Pro)
    (375, 812, 3),    # iPhone 13 mini, 12 mini, 11 Pro, XS, X
    (414, 896, 3),    # iPhone 11 Pro Max, XS Max
    (414, 896, 2),    # iPhone 11, XR
    (414, 736, 3),    # iPhone 8/7/6s Plus
    (375, 667, 2),    # iPhone SE 2/3, 8/7/6s
    (320, 568, 2),    # iPhone SE 1, 5s
    (1032, 1376, 2),  # iPad Pro 13" (M4)
    (1024, 1366, 2),  # iPad Pro 12.9"
    (834, 1210, 2),   # iPad Pro 11" (M4)
    (834, 1194, 2),   # iPad Pro 11" / 10.5"
    (820, 1180, 2),   # iPad Air 10.9"
    (810, 1080, 2),   # iPad 10.2"
    (834, 1112, 2),   # iPad Pro 10.5"
    (768, 1024, 2),   # iPad Mini / Air / 9.7"
    (744, 1133, 2),   # iPad Mini 6
]


def decode_png_rgb(path):
    """Minimal PNG decoder → (w, h, RGB bytes). Handles 8-bit RGB/RGBA."""
    f = open(path, "rb").read()
    assert f[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
    i, idat, W, H, ct = 8, b"", None, None, None
    while i < len(f):
        ln = struct.unpack(">I", f[i:i + 4])[0]
        typ, data = f[i + 4:i + 8], f[i + 8:i + 8 + ln]
        if typ == b"IHDR":
            W, H, _bd, ct = struct.unpack(">IIBB", data[:10])
        elif typ == b"IDAT":
            idat += data
        elif typ == b"IEND":
            break
        i += 8 + ln + 4
    raw = zlib.decompress(idat)
    ch = {2: 3, 6: 4}[ct]
    stride = W * ch

    def paeth(a, b, c):
        p = a + b - c
        pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
        return a if pa <= pb and pa <= pc else (b if pb <= pc else c)

    out, prev, pos = bytearray(), bytes(stride), 0
    for _y in range(H):
        ft = raw[pos]; pos += 1
        line = bytearray(raw[pos:pos + stride]); pos += stride
        if ft:
            for x in range(stride):
                a = line[x - ch] if x >= ch else 0
                b = prev[x]
                c = prev[x - ch] if x >= ch else 0
                if ft == 1:
                    line[x] = (line[x] + a) & 255
                elif ft == 2:
                    line[x] = (line[x] + b) & 255
                elif ft == 3:
                    line[x] = (line[x] + ((a + b) >> 1)) & 255
                elif ft == 4:
                    line[x] = (line[x] + paeth(a, b, c)) & 255
        out += line
        prev = bytes(line)
    if ch == 4:  # drop alpha
        rgb = bytearray()
        for p in range(0, len(out), 4):
            rgb += out[p:p + 3]
        out = rgb
    return W, H, bytes(out)


def write_png_rgb(path, w, h, px):
    def chunk(typ, data):
        return (struct.pack(">I", len(data)) + typ + data +
                struct.pack(">I", binascii.crc32(typ + data) & 0xffffffff))
    sig = b"\x89PNG\r\n\x1a\n"
    ihdr = struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)
    stride = w * 3
    raw = bytearray()
    for y in range(h):
        raw.append(0)
        raw += px[y * stride:(y + 1) * stride]
    idat = zlib.compress(bytes(raw), 9)
    with open(path, "wb") as fh:
        fh.write(sig + chunk(b"IHDR", ihdr) + chunk(b"IDAT", idat) + chunk(b"IEND", b""))


def build_master():
    """Rounded white app-icon tile with corners filled with BG, antialiased."""
    W, H, src = decode_png_rgb(ICON)
    r = W * 0.2237  # iOS icon corner radius ratio
    cx = [(r, r), (W - r, r), (r, H - r), (W - r, H - r)]
    subs = (0.25, 0.75)
    out = bytearray(W * H * 3)

    def inside(px, py):
        # rounded-rect point test
        if px < r and py < r:
            dx, dy = px - cx[0][0], py - cx[0][1]
            return dx * dx + dy * dy <= r * r
        if px > W - r and py < r:
            dx, dy = px - cx[1][0], py - cx[1][1]
            return dx * dx + dy * dy <= r * r
        if px < r and py > H - r:
            dx, dy = px - cx[2][0], py - cx[2][1]
            return dx * dx + dy * dy <= r * r
        if px > W - r and py > H - r:
            dx, dy = px - cx[3][0], py - cx[3][1]
            return dx * dx + dy * dy <= r * r
        return True

    for y in range(H):
        row = y * W * 3
        for x in range(W):
            cov = 0
            for sx in subs:
                for sy in subs:
                    if inside(x + sx, y + sy):
                        cov += 1
            o = row + x * 3
            if cov == 4:
                out[o:o + 3] = src[o:o + 3]
            elif cov == 0:
                out[o], out[o + 1], out[o + 2] = BG
            else:
                a = cov / 4.0
                for k in range(3):
                    out[o + k] = round(src[o + k] * a + BG[k] * (1 - a))
    write_png_rgb(MASTER, W, H, bytes(out))


def sips(*args):
    subprocess.run(["sips", *args], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def main():
    os.makedirs(OUTDIR, exist_ok=True)
    print("building rounded master…")
    build_master()
    tmp = os.path.join(OUTDIR, "_tmp.png")
    n = 0
    for dw, dh, dpr in DEVICES:
        pw, ph = dw * dpr, dh * dpr            # portrait pixels
        L = max(180, min(560, round(min(pw, ph) * 0.27)))
        sips("--resampleHeightWidth", str(L), str(L), MASTER, "--out", tmp)
        # portrait: canvas ph(H) x pw(W)
        sips("--padToHeightWidth", str(ph), str(pw), "--padColor", BG_HEX,
             tmp, "--out", os.path.join(OUTDIR, f"apple-splash-{pw}x{ph}.png"))
        # landscape: canvas pw(H) x ph(W)
        sips("--padToHeightWidth", str(pw), str(ph), "--padColor", BG_HEX,
             tmp, "--out", os.path.join(OUTDIR, f"apple-splash-{ph}x{pw}.png"))
        n += 2
        print(f"  {dw}x{dh}@{dpr}x → {pw}x{ph} + {ph}x{pw}")
    os.remove(tmp)
    os.remove(MASTER)
    print(f"done: {n} splash images in {OUTDIR}")


if __name__ == "__main__":
    main()
