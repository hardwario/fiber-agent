//! The Northbridge image, its manifest and the `app_valid` record —
//! `northbridge_update.md` §3.1, §3.2 and §10 (Contracts).
//!
//! Everything here is checked before the flasher sends a single byte to the Northbridge: a
//! corrupt or mismatched image is refused while the Northbridge is still running.

use std::path::Path;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::{Component, Error, Kind, Result, Transport};

/// Flash base of the STM32WL55JC.
pub const FLASH_BASE: u32 = 0x0800_0000;
/// Flash size: 256 KB.
pub const FLASH_SIZE: u32 = 256 * 1024;
/// Flash page: 2 KB.
pub const PAGE_SIZE: u32 = 2048;
/// The programming unit: a double word. Every write length and address is a multiple of it.
pub const PROGRAM_UNIT: u32 = 8;

/// `nbboot`: page 0, never erased or written by the flasher.
pub const STUB_PAGE: u16 = 0;
/// The app (`slot0`): pages 1..=118.
pub const APP_FIRST_PAGE: u16 = 1;
pub const APP_LAST_PAGE: u16 = 118;
/// The `app_valid` record's page.
pub const RECORD_PAGE: u16 = 119;
/// `storage`: pages 120..=127, never erased by the flasher.
pub const STORAGE_FIRST_PAGE: u16 = 120;

/// Where the app is linked and written.
pub const APP_ADDR: u32 = FLASH_BASE + APP_FIRST_PAGE as u32 * PAGE_SIZE;
/// The largest image: 118 pages.
pub const APP_MAX_LEN: u32 = (APP_LAST_PAGE - APP_FIRST_PAGE + 1) as u32 * PAGE_SIZE;
/// The `app_valid` record.
pub const RECORD_ADDR: u32 = FLASH_BASE + RECORD_PAGE as u32 * PAGE_SIZE;
/// The record's length; the rest of its page stays 0xFF.
pub const RECORD_LEN: usize = 32;
/// The stub's address: `GO` starts here, so the stub validates the new app before it runs.
pub const STUB_ADDR: u32 = FLASH_BASE;

/// The record's magic, "NBVA" in memory order.
pub const RECORD_MAGIC: u32 = 0x4156_424E;
/// The record format this flasher writes.
pub const RECORD_FORMAT: u32 = 1;
/// The manifest format this flasher reads.
pub const MANIFEST_FORMAT: u32 = 1;
/// The only board a Northbridge image may be built for.
pub const BOARD: &str = "stm32wl5moc";

// The §10 address table, restated as arithmetic so a typo in one constant cannot pass.
const _: () = assert!(APP_ADDR == 0x0800_0800);
const _: () = assert!(APP_MAX_LEN == 241_664);
const _: () = assert!(RECORD_ADDR == 0x0803_B800);
const _: () = assert!(FLASH_BASE + STORAGE_FIRST_PAGE as u32 * PAGE_SIZE == 0x0803_C000);
const _: () = assert!(APP_ADDR + APP_MAX_LEN == RECORD_ADDR);
const _: () = assert!((RECORD_LEN as u32).is_multiple_of(PROGRAM_UNIT));

/// Is `page` one the flasher may erase? Only `slot0` and the record page: never the stub,
/// never `storage`.
pub const fn erasable(page: u16) -> bool {
    page >= APP_FIRST_PAGE && page <= RECORD_PAGE
}

/// CRC-32/ISO-HDLC (zlib `crc32`): the image CRC and the record CRC of §10.
pub fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::new(
        Kind::InvalidArgument,
        Transport::P2p,
        Component::NorthbridgeFirmware,
        msg,
    )
}

/// `northbridge.json` as it is on disk (§10).
#[derive(Debug, Deserialize)]
struct RawManifest {
    format: u32,
    board: String,
    load_addr: u64,
    fw_version: String,
    git_sha: String,
    image_len: u64,
    crc32: String,
    sha256: String,
}

/// A parsed and structurally checked `northbridge.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// `fw_version`: major, minor, patch.
    pub fw_version: [u8; 3],
    /// `git_sha` as the number the firmware builds in (`APP_GIT_SHA`).
    pub git_sha: u32,
    pub image_len: u32,
    /// CRC-32/ISO-HDLC of the image.
    pub crc32: u32,
    pub sha256: [u8; 32],
}

fn hex_u32(s: &str, field: &str) -> Result<u32> {
    let digits = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    if digits.is_empty() || digits.len() > 8 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid(format!(
            "northbridge.json: {field} {s:?} is not a 32-bit hex value"
        )));
    }
    u32::from_str_radix(digits, 16)
        .map_err(|e| invalid(format!("northbridge.json: {field} {s:?}: {e}")))
}

fn hex_sha256(s: &str) -> Result<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 || !b.iter().all(u8::is_ascii_hexdigit) {
        return Err(invalid(format!(
            "northbridge.json: sha256 {s:?} is not 64 hex digits"
        )));
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        let pair = std::str::from_utf8(&b[2 * i..2 * i + 2])
            .map_err(|e| invalid(format!("northbridge.json: sha256: {e}")))?;
        *o = u8::from_str_radix(pair, 16)
            .map_err(|e| invalid(format!("northbridge.json: sha256: {e}")))?;
    }
    Ok(out)
}

fn version(s: &str) -> Result<[u8; 3]> {
    let parts: Vec<&str> = s.split('.').collect();
    let bad = || invalid(format!("northbridge.json: fw_version {s:?} is not x.y.z"));
    if parts.len() != 3 {
        return Err(bad());
    }
    let mut v = [0u8; 3];
    for (o, p) in v.iter_mut().zip(&parts) {
        *o = p.parse().map_err(|_| bad())?;
    }
    Ok(v)
}

impl Manifest {
    /// Parse `northbridge.json` and check what can be checked without the image: format,
    /// board, load address, and an `image_len` that fits `slot0` in whole double words.
    /// Unknown keys are ignored, so the manifest can grow.
    pub fn parse(json: &[u8]) -> Result<Self> {
        let raw: RawManifest =
            serde_json::from_slice(json).map_err(|e| invalid(format!("northbridge.json: {e}")))?;
        if raw.format != MANIFEST_FORMAT {
            return Err(invalid(format!(
                "northbridge.json: format {}, this Hub reads {MANIFEST_FORMAT}",
                raw.format
            )));
        }
        if raw.board != BOARD {
            return Err(invalid(format!(
                "northbridge.json: board {:?}, a Northbridge is {BOARD:?}",
                raw.board
            )));
        }
        if raw.load_addr != u64::from(APP_ADDR) {
            return Err(invalid(format!(
                "northbridge.json: load_addr {:#x}, the app is linked at {APP_ADDR:#x}",
                raw.load_addr
            )));
        }
        let image_len = u32::try_from(raw.image_len)
            .ok()
            .filter(|&l| l > 0 && l <= APP_MAX_LEN)
            .ok_or_else(|| {
                invalid(format!(
                    "northbridge.json: image_len {} is not in (0, {APP_MAX_LEN}]",
                    raw.image_len
                ))
            })?;
        if !image_len.is_multiple_of(PROGRAM_UNIT) {
            return Err(invalid(format!(
                "northbridge.json: image_len {image_len} is not a multiple of {PROGRAM_UNIT}"
            )));
        }
        if raw.git_sha.len() != 8 {
            return Err(invalid(format!(
                "northbridge.json: git_sha {:?} is not 8 hex digits",
                raw.git_sha
            )));
        }
        Ok(Manifest {
            fw_version: version(&raw.fw_version)?,
            git_sha: hex_u32(&raw.git_sha, "git_sha")?,
            image_len,
            crc32: hex_u32(&raw.crc32, "crc32")?,
            sha256: hex_sha256(&raw.sha256)?,
        })
    }

    /// Check `image` against this manifest: length, CRC-32 and sha256. Nothing is sent to the
    /// Northbridge before this passes.
    pub fn check_image(&self, image: &[u8]) -> Result<()> {
        if image.len() != self.image_len as usize {
            return Err(invalid(format!(
                "northbridge.bin is {} B, its manifest says {}",
                image.len(),
                self.image_len
            )));
        }
        let sha: [u8; 32] = Sha256::digest(image).into();
        if sha != self.sha256 {
            return Err(invalid(
                "northbridge.bin does not match the sha256 of its manifest",
            ));
        }
        let crc = crc32(image);
        if crc != self.crc32 {
            return Err(invalid(format!(
                "northbridge.bin CRC-32 {crc:#010x}, its manifest says {:#010x}",
                self.crc32
            )));
        }
        Ok(())
    }

    /// `git_sha` as `EVT_BOOT` and `GET_INFO` carry it: big-endian, display order.
    pub fn git_sha_bytes(&self) -> [u8; 4] {
        self.git_sha.to_be_bytes()
    }

    /// `0.4.0+1a2b3c4d`, for logs and the status document.
    pub fn version_string(&self) -> String {
        let [a, b, c] = self.fw_version;
        format!("{a}.{b}.{c}+{:08x}", self.git_sha)
    }
}

/// Read `northbridge.bin` and `northbridge.json` and check them against each other.
pub fn load_files(bin: &Path, manifest: &Path) -> Result<(Vec<u8>, Manifest)> {
    let json = std::fs::read(manifest)
        .map_err(|e| invalid(format!("cannot read {}: {e}", manifest.display())))?;
    let m = Manifest::parse(&json)?;
    let image =
        std::fs::read(bin).map_err(|e| invalid(format!("cannot read {}: {e}", bin.display())))?;
    m.check_image(&image)?;
    Ok((image, m))
}

/// The 32-B `app_valid` record of §10, little-endian, built from the manifest.
pub fn app_valid_record(m: &Manifest) -> [u8; RECORD_LEN] {
    let mut r = [0xFFu8; RECORD_LEN];
    r[0..4].copy_from_slice(&RECORD_MAGIC.to_le_bytes());
    r[4..8].copy_from_slice(&RECORD_FORMAT.to_le_bytes());
    r[8..12].copy_from_slice(&m.image_len.to_le_bytes());
    r[12..16].copy_from_slice(&m.crc32.to_le_bytes());
    r[16..19].copy_from_slice(&m.fw_version);
    r[19] = 0;
    r[20..24].copy_from_slice(&m.git_sha.to_le_bytes());
    let rec_crc = crc32(&r[0..24]);
    r[24..28].copy_from_slice(&rec_crc.to_le_bytes());
    // 28..32 reserved, already 0xFF.
    r
}

/// Does `r` hold a record the stub accepts, apart from the image CRC over flash? Returns
/// `(image_len, image_crc32)`. Used by the simulated Northbridge and by tests.
pub fn parse_record(r: &[u8]) -> Option<(u32, u32)> {
    let u32le = |o: usize| {
        r.get(o..o + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let len = u32le(8)?;
    let ok = u32le(0)? == RECORD_MAGIC
        && u32le(4)? == RECORD_FORMAT
        && u32le(24)? == crc32(r.get(0..24)?)
        && len > 0
        && len <= APP_MAX_LEN;
    ok.then_some((len, u32le(12)?))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A deterministic test image of `len` bytes.
    pub(crate) fn test_image(len: usize, seed: u32) -> Vec<u8> {
        let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x >> 24) as u8
            })
            .collect()
    }

    pub(crate) fn sha_hex(image: &[u8]) -> String {
        Sha256::digest(image)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// The manifest JSON CI would write for `image`.
    pub(crate) fn manifest_json(image: &[u8], fw: &str, git_sha: &str) -> String {
        format!(
            r#"{{"format":1, "board":"stm32wl5moc", "load_addr":134219776, "fw_version":"{fw}", "git_sha":"{git_sha}", "image_len":{}, "crc32":"{:#010x}", "sha256":"{}"}}"#,
            image.len(),
            crc32(image),
            sha_hex(image)
        )
    }

    pub(crate) fn manifest_for(image: &[u8], fw: &str, git_sha: &str) -> Manifest {
        Manifest::parse(manifest_json(image, fw, git_sha).as_bytes()).expect("manifest")
    }

    #[test]
    fn crc32_is_iso_hdlc() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn the_erasable_pages_are_slot0_and_the_record_only() {
        assert!(!erasable(STUB_PAGE));
        assert!(erasable(1) && erasable(118) && erasable(RECORD_PAGE));
        assert!((STORAGE_FIRST_PAGE..=127).all(|p| !erasable(p)));
        assert!(!erasable(u16::MAX));
    }

    #[test]
    fn a_manifest_parses() {
        let image = test_image(1000, 1);
        let m =
            Manifest::parse(manifest_json(&image, "0.4.0", "1a2b3c4d").as_bytes()).expect("parses");
        assert_eq!(m.fw_version, [0, 4, 0]);
        assert_eq!(m.git_sha, 0x1a2b_3c4d);
        assert_eq!(m.git_sha_bytes(), [0x1a, 0x2b, 0x3c, 0x4d]);
        assert_eq!(m.image_len, 1000);
        assert_eq!(m.crc32, crc32(&image));
        assert_eq!(m.version_string(), "0.4.0+1a2b3c4d");
        m.check_image(&image).expect("the image matches");
    }

    #[test]
    fn a_bad_manifest_is_refused() {
        let image = test_image(1000, 1);
        let good = manifest_json(&image, "0.4.0", "1a2b3c4d");
        for (from, to, why) in [
            (
                "\"board\":\"stm32wl5moc\"",
                "\"board\":\"stm32c031\"",
                "board",
            ),
            ("\"format\":1", "\"format\":2", "format"),
            ("134219776", "134217728", "load_addr"),
            ("\"image_len\":1000", "\"image_len\":1004", "multiple of 8"),
            ("\"image_len\":1000", "\"image_len\":0", "zero length"),
            ("\"image_len\":1000", "\"image_len\":241672", "too long"),
            (
                "\"fw_version\":\"0.4.0\"",
                "\"fw_version\":\"0.4\"",
                "version",
            ),
            (
                "\"git_sha\":\"1a2b3c4d\"",
                "\"git_sha\":\"1a2b3c4\"",
                "short sha",
            ),
            (
                "\"git_sha\":\"1a2b3c4d\"",
                "\"git_sha\":\"1a2b3c4g\"",
                "non-hex sha",
            ),
            ("\"sha256\":\"", "\"sha256\":\"zz", "sha256"),
            ("\"crc32\":", "\"crc\":", "missing crc32"),
        ] {
            let bad = good.replace(from, to);
            assert_ne!(bad, good, "{why}: the replacement applies");
            let err = Manifest::parse(bad.as_bytes()).expect_err(why);
            assert_eq!(err.kind(), Kind::InvalidArgument, "{why}");
            assert_eq!(err.component(), Component::NorthbridgeFirmware, "{why}");
        }
        assert!(Manifest::parse(b"{not json").is_err());
    }

    #[test]
    fn an_image_that_does_not_match_its_manifest_is_refused() {
        let image = test_image(1000, 1);
        let m = manifest_for(&image, "0.4.0", "1a2b3c4d");
        let mut flipped = image.clone();
        flipped[500] ^= 1;
        let e = m.check_image(&flipped).expect_err("sha256");
        assert!(e.message().contains("sha256"), "{}", e.message());
        assert!(m.check_image(&image[..992]).is_err(), "length");

        // A manifest whose sha256 is right but whose CRC-32 is not.
        let wrong_crc = manifest_json(&image, "0.4.0", "1a2b3c4d").replace(
            &format!("{:#010x}", crc32(&image)),
            &format!("{:#010x}", crc32(&image) ^ 1),
        );
        let m = Manifest::parse(wrong_crc.as_bytes()).expect("parses");
        let e = m.check_image(&image).expect_err("crc32");
        assert!(e.message().contains("CRC-32"), "{}", e.message());
    }

    #[test]
    fn load_files_reads_and_checks_both() {
        let dir = std::env::temp_dir().join(format!("nb-flash-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let image = test_image(2048, 7);
        let (bin, json) = (dir.join("northbridge.bin"), dir.join("northbridge.json"));
        std::fs::write(&bin, &image).expect("bin");
        std::fs::write(&json, manifest_json(&image, "0.4.0", "1a2b3c4d")).expect("json");
        let (got, m) = load_files(&bin, &json).expect("loads");
        assert_eq!((got, m.image_len), (image.clone(), 2048));

        std::fs::write(&bin, &image[..2040]).expect("bin");
        assert_eq!(
            load_files(&bin, &json).expect_err("short").kind(),
            Kind::InvalidArgument
        );
        assert!(load_files(&dir.join("missing.bin"), &json).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The record of §10, byte by byte, for a fixed manifest.
    #[test]
    fn the_record_is_exactly_the_contract() {
        let m = Manifest {
            fw_version: [0, 4, 0],
            git_sha: 0x1a2b_3c4d,
            image_len: 0x0001_8A08,
            crc32: 0xDEAD_BEEF,
            sha256: [0; 32],
        };
        let r = app_valid_record(&m);
        let want_head: [u8; 24] = [
            0x4E, 0x42, 0x56, 0x41, // magic "NBVA"
            0x01, 0x00, 0x00, 0x00, // format 1
            0x08, 0x8A, 0x01, 0x00, // image_len
            0xEF, 0xBE, 0xAD, 0xDE, // image_crc32
            0x00, 0x04, 0x00, 0x00, // fw_ver 0.4.0 + pad
            0x4D, 0x3C, 0x2B, 0x1A, // git_sha u32 LE
        ];
        assert_eq!(&r[0..24], &want_head);
        assert_eq!(&r[0..4], b"NBVA");
        // zlib crc32 of the 24 bytes above, computed independently (Python `zlib.crc32`).
        assert_eq!(&r[24..28], &0x5B3C_CFDBu32.to_le_bytes());
        assert_eq!(&r[28..32], &[0xFF; 4]);
        assert_eq!(parse_record(&r), Some((0x0001_8A08, 0xDEAD_BEEF)));

        let mut bad = r;
        bad[8] ^= 1;
        assert_eq!(parse_record(&bad), None, "rec_crc32 covers the fields");
    }

    /// The firmware side's own output: the manifest and `app_valid.bin` that `nb_package.py`
    /// made for a real 0.4.0 build (2026-10-08). The Hub's record must be the same 32 bytes,
    /// or the stub would refuse every image the Hub writes.
    #[test]
    fn the_record_matches_what_nb_package_py_writes() {
        let json = br#"{"format": 1, "board": "stm32wl5moc", "load_addr": 134219776, "fw_version": "0.4.0", "git_sha": "794c9c4c", "image_len": 102192, "crc32": "0x0c3b24d1", "sha256": "efdd1493a67d0726b264931233fad3d6f85270f01d60a0b2becdcc52b605483d"}"#;
        let m = Manifest::parse(json).expect("nb_package.py's manifest parses");
        let want: [u8; RECORD_LEN] = [
            0x4E, 0x42, 0x56, 0x41, 0x01, 0x00, 0x00, 0x00, 0x30, 0x8F, 0x01, 0x00, 0xD1, 0x24,
            0x3B, 0x0C, 0x00, 0x04, 0x00, 0x00, 0x4C, 0x9C, 0x4C, 0x79, 0xA0, 0x81, 0x97, 0x45,
            0xFF, 0xFF, 0xFF, 0xFF,
        ];
        assert_eq!(app_valid_record(&m), want);
    }
}
