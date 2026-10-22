#!/usr/bin/env python3
"""Package the hicolor PNG set into Windows and macOS icon containers.

The Linux hicolor tree is the source of truth for the rendered sizes:

    packaging/linux/icons/hicolor/<size>x<size>/apps/luban-notepad.png

Run this after regenerating those PNGs to refresh the platform-specific files.
Only the Python standard library is required.
"""

from __future__ import annotations

import struct
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
HICOLOR = ROOT / "packaging/linux/icons/hicolor"
WINDOWS_ICO = ROOT / "packaging/windows/luban-notepad.ico"
MACOS_ICNS = ROOT / "packaging/macos/luban-notepad.icns"

# ICO dimensions are stored in one byte, with 0 meaning 256.
ICO_SIZES = (16, 24, 32, 48, 64, 128, 256)

# ICNS type -> pixel size. The icp* entries cover the classic 16/32/64 px
# slots; ic07..ic14 cover 128..1024 px including the @2x representations.
ICNS_TYPES = (
    ("icp4", 16),
    ("icp5", 32),
    ("icp6", 64),
    ("ic07", 128),
    ("ic08", 256),
    ("ic09", 512),
    ("ic10", 1024),
    ("ic11", 32),
    ("ic12", 64),
    ("ic13", 256),
    ("ic14", 512),
)


def png_bytes(size: int) -> bytes:
    path = HICOLOR / f"{size}x{size}/apps/luban-notepad.png"
    data = path.read_bytes()
    if not data.startswith(b"\x89PNG\r\n\x1a\n"):
        raise SystemExit(f"{path} is not a PNG file")
    return data


def write_ico(path: Path, entries: list[tuple[int, bytes]]) -> None:
    header = struct.pack("<HHH", 0, 1, len(entries))
    offset = 6 + 16 * len(entries)
    directory = bytearray()
    payload = bytearray()
    for size, data in entries:
        dimension = 0 if size >= 256 else size
        directory += struct.pack(
            "<BBBBHHII",
            dimension,
            dimension,
            0,
            0,
            1,
            32,
            len(data),
            offset,
        )
        offset += len(data)
        payload += data
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(header + directory + payload)


def write_icns(path: Path, entries: list[tuple[str, bytes]]) -> None:
    body = bytearray()
    for ostype, data in entries:
        body += ostype.encode("ascii")
        body += struct.pack(">I", 8 + len(data))
        body += data
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(b"icns" + struct.pack(">I", 8 + len(body)) + body)


def main() -> None:
    ico_entries = [(size, png_bytes(size)) for size in ICO_SIZES]
    write_ico(WINDOWS_ICO, ico_entries)
    print(f"wrote {WINDOWS_ICO.relative_to(ROOT)} ({len(ico_entries)} sizes)")

    icns_entries = [(ostype, png_bytes(size)) for ostype, size in ICNS_TYPES]
    write_icns(MACOS_ICNS, icns_entries)
    print(f"wrote {MACOS_ICNS.relative_to(ROOT)} ({len(icns_entries)} entries)")


if __name__ == "__main__":
    main()
