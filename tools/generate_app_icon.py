#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""Regenerate the Plyra Terminal launcher assets with Pillow and LLVM tools."""

import argparse
from io import BytesIO
from pathlib import Path
import struct
import subprocess
import tempfile

from PIL import Image, ImageDraw, ImageFilter


ROOT = Path(__file__).resolve().parents[1]
SIZE = 1024
RESAMPLE = Image.Resampling.LANCZOS


def tile_mask(inset, radius):
    mask = Image.new("L", (SIZE, SIZE))
    ImageDraw.Draw(mask).rounded_rectangle(
        (inset, inset, SIZE - 1 - inset, SIZE - 1 - inset), radius, fill=255
    )
    return mask


def gradient(top, bottom):
    strip = Image.new("RGB", (1, SIZE))
    strip.putdata([
        tuple(round(a + (b - a) * y / (SIZE - 1)) for a, b in zip(top, bottom))
        for y in range(SIZE)
    ])
    return strip.resize((SIZE, SIZE)).convert("RGBA")


def draw_icon():
    # A graphite glass tile with a beveled edge and a centered terminal prompt.
    icon = Image.new("RGBA", (SIZE, SIZE))
    rim = gradient((139, 181, 196), (27, 43, 57))
    icon.paste(rim, mask=tile_mask(60, 226))
    face = gradient((47, 65, 80), (10, 16, 24))
    icon.paste(face, mask=tile_mask(73, 214))
    inner = (109, 109, 914, 914)
    ImageDraw.Draw(icon).rounded_rectangle(inner, 180, outline=(80, 111, 128), width=3)

    prompt = Image.new("L", (SIZE, SIZE))
    stroke = ImageDraw.Draw(prompt)
    points = [(322, 350), (480, 506), (322, 662)]
    width = 78
    stroke.line(points, fill=255, width=width, joint="curve")
    for x, y in points:
        radius = width // 2
        stroke.ellipse((x - radius, y - radius, x + radius, y + radius), fill=255)
    stroke.rounded_rectangle((546, 624, 741, 700), 38, fill=255)

    halo = Image.new("RGBA", icon.size, (55, 235, 255))
    halo.putalpha(prompt.filter(ImageFilter.GaussianBlur(51)).point(lambda p: p // 2))
    icon.alpha_composite(halo)
    glyph = gradient((217, 255, 252), (91, 224, 230))
    glyph.putalpha(prompt)
    icon.alpha_composite(glyph)
    return icon


def save_icns(icon, path):
    variants = [(b"icp4", 16), (b"icp5", 32), (b"icp6", 64), (b"ic07", 128),
                (b"ic08", 256), (b"ic09", 512), (b"ic10", 1024), (b"ic11", 32),
                (b"ic12", 64), (b"ic13", 256), (b"ic14", 512)]
    chunks = []
    for code, size in variants:
        payload = BytesIO()
        icon.resize((size, size), RESAMPLE).save(payload, "PNG", optimize=True)
        data = payload.getvalue()
        chunks.append(code + struct.pack(">I", len(data) + 8) + data)
    body = b"".join(chunks)
    path.write_bytes(b"icns" + struct.pack(">I", len(body) + 8) + body)


def write_windows_resources(directory, llvm_tools):
    with tempfile.TemporaryDirectory(prefix="glass-icon-") as temporary:
        stage = Path(temporary)
        (stage / "glass-terminal.ico").write_bytes((directory / "glass-terminal.ico").read_bytes())
        (stage / "glass-terminal.rc").write_text('1 ICON "glass-terminal.ico"\n', encoding="ascii")
        subprocess.run([llvm_tools / "llvm-rc", "/fo", "glass-terminal.res", "glass-terminal.rc"],
                       cwd=stage, check=True)
        for architecture, machine in (("x86_64", "X64"), ("aarch64", "ARM64")):
            output = directory / f"windows-{architecture}.obj"
            subprocess.run([llvm_tools / "llvm-cvtres", f"/MACHINE:{machine}", "/TIMESTAMP:0",
                            f"/OUT:{output}", "glass-terminal.res"], cwd=stage, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--llvm-tools", type=Path, help="Directory containing llvm-rc and llvm-cvtres")
    tools = parser.parse_args().llvm_tools
    tools = tools.resolve() if tools else Path("")
    directory = ROOT / "assets/app-icon"
    directory.mkdir(exist_ok=True)
    icon = draw_icon()
    icon.resize((256, 256), RESAMPLE).save(directory / "glass-terminal.png", optimize=True)
    icon.save(directory / "glass-terminal.ico", sizes=[(s, s) for s in (16, 24, 32, 48, 64, 128, 256)])
    save_icns(icon, directory / "GlassTerminal.icns")
    (directory / "window-64.rgba").write_bytes(icon.resize((64, 64), RESAMPLE).tobytes())
    write_windows_resources(directory, tools)

    resources = ROOT / "apps/android/res"
    legacy = resources / "mipmap-xxxhdpi"
    legacy.mkdir(exist_ok=True)
    icon.resize((192, 192), RESAMPLE).save(legacy / "glass_terminal_icon.png", optimize=True)
    foreground = resources / "drawable-xxxhdpi"
    foreground.mkdir(exist_ok=True)
    # Android clips its 108dp foreground layers inside the central 72dp mask.
    layer = Image.new("RGBA", (432, 432))
    layer.alpha_composite(icon.resize((272, 272), RESAMPLE), (80, 80))
    layer.save(foreground / "glass_terminal_icon_foreground.png", optimize=True)


if __name__ == "__main__":
    main()

# FVPROV1 4837 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMWQxNDk0NTdiYzhmYzgzYjc4ODQ1NjExNTljYzY5NTBmZTBkN2EzNmUwZTM1YmFlNzU0ZTQ2MDJiM2M5MTZhZSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMWYwMjMyMjhhMzNjNWY3NThmYzEwMjAxNDJmMTIyM2FlZDdkNjI5YmI4NDNjMTUyN2Q4NTRjZDI4Y2YwYThlOTVkMzE5NDA0OTdlMTg3NmEzYjdmMDY0OTBhNTlmNjMxNTQ0YTk2NzFjYjZmNWQ0NjIxZWU5MjRkNmFlZDVkMGEifV19XQ== FVPROV1-END
