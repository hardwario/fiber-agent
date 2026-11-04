#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Package a Southbridge app build for the update through the Hub.

proximos-v2 plan/system/southbridge_update.md §3.2. From the app's zephyr.bin (linked at
0x0800_0000, the whole image; the SB has no bootloader of its own) it writes:

  southbridge.bin   the app padded with 0xFF to a multiple of 8 B (the flash programming unit)
  southbridge.json  {format, board, load_addr, fw_version, git_sha, image_len, crc32, sha256}

    sb_package.py --bin build/zephyr/zephyr.bin --out-dir sb-package \\
        [--fw-version 1.1.0 --git-sha 1a2b3c4d]

Without --fw-version / --git-sha both come from build/zephyr/sb_build.json, which the app's
CMake writes at configure time next to zephyr.bin. The Hub compares fw_version + git_sha with
what the SB answers to VERSION ("south-zephyr <fw_version> g<git_sha>").
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import zlib
from pathlib import Path

BOARD = "fiber_southbridge"
MANIFEST_FORMAT = 1
FLASH_BASE = 0x0800_0000
FLASH_SIZE = 32 * 1024  # STM32C031C6
RAM_START = 0x2000_0000
RAM_END = 0x2000_3000  # 12 KB


class PackageError(Exception):
    pass


def pad8(data: bytes) -> bytes:
    return data + b"\xff" * (-len(data) % 8)


def parse_version(s: str) -> str:
    if not re.fullmatch(r"\d+\.\d+\.\d+", s):
        raise PackageError(f"fw version {s!r}: want major.minor.patch")
    return s


def parse_sha(s: str) -> str:
    """8 lowercase hex digits, padded and cut as app/CMakeLists.txt does."""
    s = s.lower()
    if not re.fullmatch(r"[0-9a-f]{1,40}", s):
        raise PackageError(f"git sha {s!r}: want hex digits")
    return (s + "00000000")[:8]


def build_info(app_bin: Path) -> dict:
    path = app_bin.parent / "sb_build.json"
    try:
        info = json.loads(path.read_text())
    except (OSError, ValueError) as e:
        raise PackageError(f"{path}: {e} (or pass --fw-version and --git-sha)") from e
    if not isinstance(info, dict):
        raise PackageError(f"{path}: not an object")
    return info


def check_vectors(image: bytes) -> None:
    """A Cortex-M vector table for FLASH_BASE: SP in RAM, Reset inside the image."""
    if len(image) < 8:
        raise PackageError(f"app: {len(image)} B, no vector table")
    sp = int.from_bytes(image[0:4], "little")
    reset = int.from_bytes(image[4:8], "little")
    if not RAM_START < sp <= RAM_END:
        raise PackageError(f"app: initial SP 0x{sp:08x} not in RAM")
    if not reset & 1 or not FLASH_BASE <= (reset & ~1) < FLASH_BASE + len(image):
        raise PackageError(f"app: reset vector 0x{reset:08x} not in the image at "
                           f"0x{FLASH_BASE:08x} (linked for another address?)")


def package_image(app_bin: bytes) -> bytes:
    check_vectors(app_bin)
    image = pad8(app_bin)
    if len(image) > FLASH_SIZE:
        raise PackageError(f"app: {len(image)} B, the flash holds {FLASH_SIZE} B")
    return image


def manifest(image: bytes, fw_version: str, git_sha: str) -> dict:
    return {
        "format": MANIFEST_FORMAT,
        "board": BOARD,
        "load_addr": FLASH_BASE,
        "fw_version": fw_version,
        "git_sha": git_sha,
        "image_len": len(image),
        "crc32": f"0x{zlib.crc32(image) & 0xFFFFFFFF:08x}",
        "sha256": hashlib.sha256(image).hexdigest(),
    }


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--bin", required=True, type=Path, help="the app's zephyr.bin")
    ap.add_argument("--fw-version", help="major.minor.patch; default: the build's sb_build.json")
    ap.add_argument("--git-sha", help="hex, padded/cut to 8; default: the build's sb_build.json")
    ap.add_argument("--out-dir", "--out", required=True, type=Path)
    args = ap.parse_args(argv)

    try:
        info = {} if args.fw_version and args.git_sha else build_info(args.bin)
        fw_version = parse_version(args.fw_version or str(info.get("fw_version", "")))
        git_sha = parse_sha(args.git_sha or str(info.get("git_sha", "")))
        image = package_image(args.bin.read_bytes())
    except (PackageError, OSError) as e:
        print(f"sb_package: {e}", file=sys.stderr)
        return 1

    args.out_dir.mkdir(parents=True, exist_ok=True)
    (args.out_dir / "southbridge.bin").write_bytes(image)
    (args.out_dir / "southbridge.json").write_text(
        json.dumps(manifest(image, fw_version, git_sha), indent=2) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
