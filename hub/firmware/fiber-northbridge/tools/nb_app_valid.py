# SPDX-License-Identifier: Apache-2.0
"""The Northbridge app_valid record (proximos-v2 plan/control/radio/northbridge_update.md §10).

The Python twin of nbboot/src/nb_app_valid.c: the reference that builds the 32 B record the
Hub's flasher writes last (at 0x0803_B800) and that nb_package.py puts into the J-Link hex, and
the acceptance rule of the boot stub. Standard library only.

Record, little-endian, 32 B (the rest of the page stays 0xFF):

    0 magic u32 0x4156424E ("NBVA")   4 format u32 1
    8 image_len u32                  12 image_crc32 u32 (zlib crc32 of the image)
   16 fw_ver u8[3] + pad 0           20 git_sha u32 (path-scoped short SHA)
   24 rec_crc32 u32 (bytes 0..23)    28 reserved u32 0xFFFFFFFF
"""

from __future__ import annotations

import struct
import zlib
from dataclasses import dataclass

FLASH_BASE = 0x0800_0000
PAGE_SIZE = 0x800
APP_ADDR = 0x0800_0800
APP_MAX_LEN = 241_664  # 118 pages
APP_VALID_ADDR = 0x0803_B800
STORAGE_ADDR = 0x0803_C000
BOOT_STUB_SIZE = 0x800

RECORD_LEN = 32
MAGIC = 0x4156_424E
FORMAT = 1
_HEAD = struct.Struct("<IIII3sBI")  # magic, format, image_len, image_crc32, fw_ver, pad, git_sha


def crc32(data: bytes) -> int:
    """CRC-32/ISO-HDLC, as zlib.crc32 and the stub compute it."""
    return zlib.crc32(data) & 0xFFFF_FFFF


@dataclass(frozen=True)
class Record:
    image_len: int
    image_crc32: int
    fw_ver: tuple[int, int, int]
    git_sha: int

    def pack(self) -> bytes:
        """The 32 B record."""
        if not 0 < self.image_len <= APP_MAX_LEN:
            raise ValueError(f"image_len {self.image_len} not in (0, {APP_MAX_LEN}]")
        if any(not 0 <= v <= 255 for v in self.fw_ver):
            raise ValueError(f"fw_ver {self.fw_ver}")
        head = _HEAD.pack(MAGIC, FORMAT, self.image_len, self.image_crc32 & 0xFFFF_FFFF,
                          bytes(self.fw_ver), 0, self.git_sha & 0xFFFF_FFFF)
        return head + struct.pack("<II", crc32(head), 0xFFFF_FFFF)


def build_record(image: bytes, fw_ver: tuple[int, int, int], git_sha: int) -> bytes:
    """The record for `image` (northbridge.bin, already padded)."""
    return Record(len(image), crc32(image), tuple(fw_ver), git_sha).pack()


def parse_record(rec: bytes) -> Record | None:
    """The record's fields when magic, format, rec_crc32 and the image_len range hold."""
    if len(rec) < RECORD_LEN:
        return None
    magic, fmt, image_len, image_crc, fw_ver, _pad, git_sha = _HEAD.unpack_from(rec)
    (rec_crc,) = struct.unpack_from("<I", rec, 24)
    if (magic != MAGIC or fmt != FORMAT or rec_crc != crc32(rec[:24])
            or not 0 < image_len <= APP_MAX_LEN):
        return None
    return Record(image_len, image_crc, tuple(fw_ver), git_sha)


def app_valid(rec: bytes, slot0: bytes) -> bool:
    """The stub's rule: the record parses and crc32(slot0[:image_len]) matches it."""
    r = parse_record(rec)
    return (r is not None and len(slot0) >= r.image_len
            and crc32(slot0[:r.image_len]) == r.image_crc32)
