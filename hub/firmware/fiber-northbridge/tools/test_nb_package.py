# SPDX-License-Identifier: Apache-2.0
"""Tests of nb_app_valid.py and nb_package.py: `python3 -m pytest tools/`, or run this file
directly when pytest is not installed.

PINNED_RECORD is the same 32 B vector tests/nbboot_record checks the C side (the stub's
nb_app_valid.c) against, so the Hub, the CI and the stub agree on the record byte for byte.
"""

from __future__ import annotations

import json
import sys
import tempfile
import zlib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import nb_app_valid as nbv  # noqa: E402
import nb_package  # noqa: E402

PINNED_IMAGE = bytes((i * 7 + 3) & 0xFF for i in range(64))
PINNED_RECORD = bytes.fromhex(
    "4e425641" "01000000" "40000000" "f0ecd9cb" "00040000" "4d3c2b1a" "01fe7df5" "ffffffff")


def fake_app(size: int = 1000) -> bytes:
    """A zephyr.bin-like image: SP in RAM, Reset inside the image at 0x0800_0800."""
    vt = (0x2000_8000).to_bytes(4, "little") + (nbv.APP_ADDR + 0x101).to_bytes(4, "little")
    return vt + bytes((i * 13) & 0xFF for i in range(size - len(vt)))


def test_crc32_is_zlib():
    assert nbv.crc32(b"123456789") == 0xCBF43926
    assert nbv.crc32(PINNED_IMAGE) == zlib.crc32(PINNED_IMAGE) == 0xCBD9ECF0


def test_pinned_record():
    rec = nbv.build_record(PINNED_IMAGE, (0, 4, 0), 0x1A2B3C4D)
    assert rec == PINNED_RECORD
    assert len(rec) == nbv.RECORD_LEN
    assert rec[:4] == b"NBVA"
    r = nbv.parse_record(rec)
    assert r == nbv.Record(64, 0xCBD9ECF0, (0, 4, 0), 0x1A2B3C4D)
    assert nbv.app_valid(rec, PINNED_IMAGE + b"\xff" * 100)


def test_record_rejects():
    good = bytearray(PINNED_RECORD)
    for off in (0, 4, 8, 12, 16, 20, 24):
        bad = bytearray(good)
        bad[off] ^= 0x01
        # Any bit of bytes 0..27 breaks rec_crc32 or, for rec_crc32 itself, the match.
        assert nbv.parse_record(bytes(bad)) is None, off
    reserved = bytearray(good)
    reserved[28] = 0
    assert nbv.parse_record(bytes(reserved)) is not None, "reserved is not checked"
    assert nbv.parse_record(bytes(good[:31])) is None
    assert not nbv.app_valid(bytes(good), PINNED_IMAGE[:-1] + b"\x00")
    assert not nbv.app_valid(bytes(good), PINNED_IMAGE[:63])


def test_image_len_range():
    for n, ok in ((0, False), (1, True), (nbv.APP_MAX_LEN, True), (nbv.APP_MAX_LEN + 1, False)):
        try:
            rec = nbv.Record(n, 0, (0, 4, 0), 0).pack()
        except ValueError:
            assert not ok, n
            continue
        assert ok, n
        assert nbv.parse_record(rec) is not None


def test_package_pads_and_checks():
    image = nb_package.package_image(fake_app(1001))
    assert len(image) == 1008 and image[1001:] == b"\xff" * 7
    assert nb_package.package_image(fake_app(1000)) == fake_app(1000)
    for bad in (fake_app(4),
                (0x0800_0000).to_bytes(4, "little") + fake_app()[4:],   # SP not in RAM
                fake_app()[:4] + (0x0800_0101).to_bytes(4, "little") + fake_app()[8:],  # at 0x0
                fake_app(nbv.APP_MAX_LEN + 1)):
        try:
            nb_package.package_image(bad)
        except nb_package.PackageError:
            continue
        raise AssertionError("accepted a bad image")


def test_cli_outputs():
    with tempfile.TemporaryDirectory() as d:
        d = Path(d)
        (d / "zephyr.bin").write_bytes(fake_app(1001))
        stub = (0x2000_FFF0).to_bytes(4, "little") + (0x0800_00A1).to_bytes(4, "little")
        (d / "nbboot.bin").write_bytes(stub + b"\x00" * 592)
        rc = nb_package.main(["--bin", str(d / "zephyr.bin"), "--fw-version", "0.4.0",
                              "--git-sha", "1A2B3C4D", "--out-dir", str(d / "out"),
                              "--record", str(d / "rec.bin"), "--nbboot", str(d / "nbboot.bin"),
                              "--jlink-hex", str(d / "all.hex")])
        assert rc == 0
        image = (d / "out" / "northbridge.bin").read_bytes()
        assert len(image) == 1008
        man = json.loads((d / "out" / "northbridge.json").read_text())
        assert list(man) == ["format", "board", "load_addr", "fw_version", "git_sha",
                             "image_len", "crc32", "sha256"]
        assert man["format"] == 1 and man["board"] == "stm32wl5moc"
        assert man["load_addr"] == 134219776 == nbv.APP_ADDR
        assert man["fw_version"] == "0.4.0" and man["git_sha"] == "1a2b3c4d"
        assert man["image_len"] == 1008
        assert man["crc32"] == f"0x{zlib.crc32(image):08x}"
        rec = (d / "rec.bin").read_bytes()
        assert nbv.app_valid(rec, image)
        assert nbv.parse_record(rec).git_sha == 0x1A2B3C4D

        mem = read_hex((d / "all.hex").read_text())
        assert bytes(mem[a] for a in range(nbv.FLASH_BASE, nbv.FLASH_BASE + 600)) == \
            stub + b"\x00" * 592
        assert bytes(mem[a] for a in range(nbv.APP_ADDR, nbv.APP_ADDR + 1008)) == image
        assert bytes(mem[nbv.APP_VALID_ADDR + i] for i in range(32)) == rec
        assert len(mem) == 600 + 1008 + 32

        assert nb_package.main(["--bin", str(d / "zephyr.bin"), "--fw-version", "0.4",
                                "--git-sha", "1a2b3c4d", "--out-dir", str(d / "x")]) == 1


def test_cli_build_info():
    """--fw-version / --git-sha default to nb_build.json next to zephyr.bin; --out works."""
    with tempfile.TemporaryDirectory() as d:
        d = Path(d)
        (d / "zephyr.bin").write_bytes(fake_app(1000))
        assert nb_package.main(["--bin", str(d / "zephyr.bin"), "--out", str(d / "o")]) == 1
        (d / "nb_build.json").write_text('{"fw_version": "0.4.0", "git_sha": "ABC"}\n')
        assert nb_package.main(["--bin", str(d / "zephyr.bin"), "--out", str(d / "o")]) == 0
        man = json.loads((d / "o" / "northbridge.json").read_text())
        assert man["fw_version"] == "0.4.0" and man["git_sha"] == "abc00000"
        assert nb_package.main(["--bin", str(d / "zephyr.bin"), "--out", str(d / "p"),
                                "--git-sha", "0123456789abcdef"]) == 0
        assert json.loads((d / "p" / "northbridge.json").read_text())["git_sha"] == "01234567"


def test_record_git_sha_le():
    """git_sha sits at offset 20 as the numeric APP_GIT_SHA, u32 little-endian."""
    rec = nbv.build_record(PINNED_IMAGE, (0, 4, 0), 0x1A2B3C4D)
    assert rec[20:24] == bytes([0x4D, 0x3C, 0x2B, 0x1A])


def read_hex(text: str) -> dict[int, int]:
    """Intel HEX back to {address: byte}, checking every checksum."""
    mem, upper, eof = {}, 0, False
    for ln in text.splitlines():
        raw = bytes.fromhex(ln[1:])
        assert ln[0] == ":" and sum(raw) & 0xFF == 0 and raw[0] == len(raw) - 5
        addr, rtype, data = (raw[1] << 8) | raw[2], raw[3], raw[4:-1]
        if rtype == 0x04:
            upper = int.from_bytes(data, "big") << 16
        elif rtype == 0x00:
            for i, b in enumerate(data):
                mem[upper + addr + i] = b
        elif rtype == 0x01:
            eof = True
    assert eof
    return mem


if __name__ == "__main__":
    tests = [(n, f) for n, f in sorted(globals().items()) if n.startswith("test_")]
    for name, fn in tests:
        fn()
        print(f"ok   {name}")
    print(f"{len(tests)} passed")
