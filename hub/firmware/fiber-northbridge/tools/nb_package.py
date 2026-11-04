#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Package a Northbridge app build for the update through the Hub.

proximos-v2 plan/control/radio/northbridge_update.md §3.2, §10. From the app's zephyr.bin
(linked at 0x0800_0800) it writes:

  northbridge.bin   the app padded with 0xFF to a multiple of 8 B (the flash programming unit)
  northbridge.json  {format, board, load_addr, fw_version, git_sha, image_len, crc32, sha256}

and on request the 32 B app_valid record (--record) and an Intel HEX for a J-Link flash of the
whole layout: nbboot (--nbboot) + the app + the record (--jlink-hex). app/CMakeLists.txt runs it
after every app build; `west flash` then programs that hex, so a bench flash leaves a valid
record behind.

    nb_package.py --bin build/zephyr/zephyr.bin --out-dir build/zephyr \\
        [--fw-version 0.4.0 --git-sha 1a2b3c4d] [--nbboot build/nbboot/nbboot.bin] [--jlink-hex x.hex]

Without --fw-version / --git-sha both come from build/zephyr/nb_build.json, which the app's
CMake writes at configure time next to zephyr.bin.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import nb_app_valid as nbv  # noqa: E402

BOARD = "stm32wl5moc"
MANIFEST_FORMAT = 1
RAM_START = 0x2000_0000
RAM_END = 0x2000_FFF0  # the reserved tail (boot request word) is not the app's


class PackageError(Exception):
    pass


def pad8(data: bytes) -> bytes:
    return data + b"\xff" * (-len(data) % 8)


def parse_version(s: str) -> tuple[int, int, int]:
    m = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", s)
    if not m or any(int(g) > 255 for g in m.groups()):
        raise PackageError(f"fw version {s!r}: want major.minor.patch, each 0..255")
    return tuple(int(g) for g in m.groups())


def parse_sha(s: str) -> str:
    """APP_GIT_SHA as 8 lowercase hex digits, padded and cut as app/CMakeLists.txt does."""
    s = s.lower()
    if not re.fullmatch(r"[0-9a-f]{1,40}", s):
        raise PackageError(f"git sha {s!r}: want hex digits")
    return (s + "00000000")[:8]


def build_info(app_bin: Path) -> dict:
    """nb_build.json, written next to zephyr.bin by app/CMakeLists.txt: fw_version, git_sha."""
    path = app_bin.parent / "nb_build.json"
    try:
        info = json.loads(path.read_text())
    except (OSError, ValueError) as e:
        raise PackageError(f"{path}: {e} (or pass --fw-version and --git-sha)") from e
    if not isinstance(info, dict):
        raise PackageError(f"{path}: not an object")
    return info


def check_vectors(image: bytes, base: int, what: str) -> None:
    """The image starts with a Cortex-M vector table for `base`: SP in RAM, Reset inside."""
    if len(image) < 8:
        raise PackageError(f"{what}: {len(image)} B, no vector table")
    sp = int.from_bytes(image[0:4], "little")
    reset = int.from_bytes(image[4:8], "little")
    if not RAM_START < sp <= RAM_END:
        raise PackageError(f"{what}: initial SP 0x{sp:08x} not in RAM")
    if not reset & 1 or not base <= (reset & ~1) < base + len(image):
        raise PackageError(f"{what}: reset vector 0x{reset:08x} not in the image at "
                           f"0x{base:08x} (linked for another address?)")


def package_image(app_bin: bytes) -> bytes:
    """northbridge.bin from zephyr.bin."""
    check_vectors(app_bin, nbv.APP_ADDR, "app")
    image = pad8(app_bin)
    if len(image) > nbv.APP_MAX_LEN:
        raise PackageError(f"app: {len(image)} B, slot0 holds {nbv.APP_MAX_LEN} B")
    return image


def manifest(image: bytes, fw_version: str, git_sha: str, board: str = BOARD) -> dict:
    """northbridge.json; crc32 is the record's (zlib), sha256 the file's."""
    return {
        "format": MANIFEST_FORMAT,
        "board": board,
        "load_addr": nbv.APP_ADDR,
        "fw_version": fw_version,
        "git_sha": git_sha,
        "image_len": len(image),
        "crc32": f"0x{nbv.crc32(image):08x}",
        "sha256": hashlib.sha256(image).hexdigest(),
    }


def intel_hex(segments: list[tuple[int, bytes]]) -> str:
    """Intel HEX (type 00/04/01) of (address, data) segments, 16 B per line."""
    def line(rtype: int, addr16: int, data: bytes) -> str:
        rec = bytes([len(data), addr16 >> 8, addr16 & 0xFF, rtype]) + data
        return ":" + (rec + bytes([-sum(rec) & 0xFF])).hex().upper()

    out = []
    upper = None
    for base, data in sorted(segments):
        for off in range(0, len(data), 16):
            addr = base + off
            if addr >> 16 != upper:
                upper = addr >> 16
                out.append(line(0x04, 0, upper.to_bytes(2, "big")))
            # A line never crosses a 64 KiB boundary: segments and lines are 16 B aligned.
            out.append(line(0x00, addr & 0xFFFF, data[off:off + 16]))
    out.append(line(0x01, 0, b""))
    return "\n".join(out) + "\n"


def jlink_segments(image: bytes, record: bytes, nbboot: bytes | None) -> list[tuple[int, bytes]]:
    segs = [(nbv.APP_ADDR, image), (nbv.APP_VALID_ADDR, record)]
    if nbboot is not None:
        if len(nbboot) > nbv.BOOT_STUB_SIZE:
            raise PackageError(f"nbboot: {len(nbboot)} B, page 0 holds {nbv.BOOT_STUB_SIZE} B")
        check_vectors(nbboot, nbv.FLASH_BASE, "nbboot")
        segs.insert(0, (nbv.FLASH_BASE, pad8(nbboot)))
    return segs


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--bin", required=True, type=Path, help="the app's zephyr.bin")
    ap.add_argument("--fw-version",
                    help="major.minor.patch (FW_VERSION_*); default: the build's nb_build.json")
    ap.add_argument("--git-sha",
                    help="APP_GIT_SHA, hex (padded/cut to 8); default: the build's nb_build.json")
    ap.add_argument("--board", default=BOARD)
    ap.add_argument("--out-dir", "--out", required=True, type=Path,
                    help="where northbridge.bin and northbridge.json go")
    ap.add_argument("--record", type=Path, help="also write the 32 B app_valid record here")
    ap.add_argument("--nbboot", type=Path, help="nbboot.bin to put into --jlink-hex")
    ap.add_argument("--jlink-hex", type=Path,
                    help="also write an Intel HEX of [nbboot +] app + record here")
    args = ap.parse_args(argv)

    try:
        if args.fw_version is None or args.git_sha is None:
            info = build_info(args.bin)
            args.fw_version = args.fw_version or str(info.get("fw_version", ""))
            args.git_sha = args.git_sha or str(info.get("git_sha", ""))
        fw_ver = parse_version(args.fw_version)
        git_sha = parse_sha(args.git_sha)
        image = package_image(args.bin.read_bytes())
        record = nbv.build_record(image, fw_ver, int(git_sha, 16))
        man = manifest(image, args.fw_version, git_sha, args.board)
        nbboot = args.nbboot.read_bytes() if args.nbboot else None
        segs = jlink_segments(image, record, nbboot) if args.jlink_hex else None
    except (OSError, PackageError) as e:
        print(f"nb_package: {e}", file=sys.stderr)
        return 1

    args.out_dir.mkdir(parents=True, exist_ok=True)
    (args.out_dir / "northbridge.bin").write_bytes(image)
    (args.out_dir / "northbridge.json").write_text(json.dumps(man) + "\n")
    if args.record:
        args.record.write_bytes(record)
    if segs is not None:
        args.jlink_hex.write_text(intel_hex(segs))
    print(f"nb_package: northbridge.bin {man['image_len']} B, crc32 {man['crc32']}, "
          f"fw {args.fw_version}+{git_sha}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
