//! The TOWER radio frame and key derivation — #470 §4, §6.2, §6.3.
//!
//! Adopted verbatim from TOWER wire v1 (`hardwario/tower` `24259e3`):
//!
//! ```text
//! ver_type(1: ver<<5 | type) flags(1) src(4 LE) dest(4 LE) counter(4 LE)   14 B, all AAD
//! ciphertext(n)                                                          n ≤ 74
//! tag(8)
//! ```
//!
//! AES-128-CCM, N = 13, L = 2, M = 8, nonce `src(4 LE) ‖ counter(4 LE) ‖ bulk_idx(3) = 0 ‖
//! 00 00`. Types Data `0`, Ack `1`; flag `CONFIRMED` is bit 0.
//!
//! On the Hub only two frames are sealed or opened here: the JoinRequest and the JoinAccept,
//! both under the Node's `join_key` (the Northbridge never holds one). Every other frame is
//! the Northbridge's to open; the central sees its plaintext in `EVT_TWR_UPLINK`. The
//! [`open`] / [`seal`] pair is general because the KATs pin it for every frame shape.

use aes::Aes128;
// `AeadInPlace` is deprecated in favour of `AeadInOut`, but the detached seal/open pair the
// wire format needs is only on the older trait in this release.
#[allow(deprecated)]
use ccm::aead::AeadInPlace;
use ccm::aead::KeyInit;
use ccm::consts::{U13, U8};
use ccm::Ccm;
use cmac::digest::KeyInit as CmacKeyInit;
use cmac::{Cmac, Mac};

use crate::error::{Component, Error, Kind, Result, Transport};

/// TOWER frames: AES-128-CCM with an 8-byte tag and a 13-byte nonce.
type TowerCcm = Ccm<Aes128, U8, U13>;

/// The wire version this codec speaks (the `ver` of `ver_type`).
pub const VERSION: u8 = 1;
/// Header length; the whole header is the CCM AAD.
pub const HEADER_LEN: usize = 14;
/// CCM tag length (M = 8).
pub const TAG_LEN: usize = 8;
/// Largest frame: the TOWER 96 B frame, one MTU on both PHY profiles (sticker-firmware #486
/// U-1, 2026-10-12; was 100 B on lora, #470 D14). The Northbridge drops a longer one on receive.
pub const MAX_FRAME: usize = 96;
/// Largest plaintext a frame carries: `MAX_FRAME` − header − tag. Also `TWR_QUEUE_PUSH`'s cap.
pub const MAX_PAYLOAD: usize = MAX_FRAME - HEADER_LEN - TAG_LEN;

/// Frame types (the low 5 bits of `ver_type`).
pub mod frame_type {
    pub const DATA: u8 = 0;
    pub const ACK: u8 = 1;
}

/// `flags` bits.
pub mod flags {
    /// The sender wants a TOWER ACK.
    pub const CONFIRMED: u8 = 1 << 0;
}

/// Domain-separation label of the join key (#470 §6.2).
pub const LABEL_JOIN: &[u8] = b"HIO-TWR-JOIN";
/// Domain-separation label of the session key (#470 §6.2).
pub const LABEL_SES: &[u8] = b"HIO-TWR-SES";

/// The TOWER header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub frame_type: u8,
    pub flags: u8,
    pub src: u32,
    pub dest: u32,
    pub counter: u32,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[0] = (VERSION << 5) | (self.frame_type & 0x1f);
        h[1] = self.flags;
        h[2..6].copy_from_slice(&self.src.to_le_bytes());
        h[6..10].copy_from_slice(&self.dest.to_le_bytes());
        h[10..14].copy_from_slice(&self.counter.to_le_bytes());
        h
    }

    /// Parse the header of a frame, refusing another wire version and a frame too short to
    /// carry a tag. Nothing is authenticated yet.
    pub fn peek(frame: &[u8]) -> Result<Self> {
        if frame.len() < HEADER_LEN + TAG_LEN {
            return Err(frame_err(format!(
                "a {} B frame is shorter than a P2P frame header and tag ({} B)",
                frame.len(),
                HEADER_LEN + TAG_LEN
            )));
        }
        let ver = frame[0] >> 5;
        if ver != VERSION {
            return Err(frame_err(format!(
                "P2P frame version {ver}, this central speaks {VERSION}"
            )));
        }
        let u32le =
            |o: usize| u32::from_le_bytes([frame[o], frame[o + 1], frame[o + 2], frame[o + 3]]);
        Ok(Header {
            frame_type: frame[0] & 0x1f,
            flags: frame[1],
            src: u32le(2),
            dest: u32le(6),
            counter: u32le(10),
        })
    }

    pub const fn is_confirmed(&self) -> bool {
        self.flags & flags::CONFIRMED != 0
    }
}

fn frame_err(msg: String) -> Error {
    Error::new(
        Kind::Transport,
        Transport::P2p,
        Component::NorthbridgeFirmware,
        msg,
    )
}

/// The CCM nonce: `src(4 LE) ‖ counter(4 LE) ‖ bulk_idx(3) = 0 ‖ 00 00`.
pub fn nonce(src: u32, counter: u32) -> [u8; 13] {
    let mut n = [0u8; 13];
    n[0..4].copy_from_slice(&src.to_le_bytes());
    n[4..8].copy_from_slice(&counter.to_le_bytes());
    n
}

/// Seal a frame: header ‖ ciphertext ‖ tag.
#[allow(deprecated)]
pub fn seal(key: &[u8; 16], header: &Header, plaintext: &[u8]) -> Result<Vec<u8>> {
    if plaintext.len() > MAX_PAYLOAD {
        return Err(frame_err(format!(
            "payload is {} B, past the {MAX_PAYLOAD} B a P2P frame carries",
            plaintext.len()
        )));
    }
    let aad = header.encode();
    let cipher = TowerCcm::new(key.into());
    let mut buf = plaintext.to_vec();
    let tag = cipher
        .encrypt_in_place_detached(&nonce(header.src, header.counter).into(), &aad, &mut buf)
        .map_err(|_| frame_err("AES-CCM encryption failed".into()))?;
    let mut out = Vec::with_capacity(HEADER_LEN + buf.len() + TAG_LEN);
    out.extend_from_slice(&aad);
    out.extend_from_slice(&buf);
    out.extend_from_slice(&tag);
    Ok(out)
}

/// Open a frame: verify the tag over header ‖ ciphertext and return the header and plaintext.
#[allow(deprecated)]
pub fn open(key: &[u8; 16], frame: &[u8]) -> Result<(Header, Vec<u8>)> {
    let header = Header::peek(frame)?;
    let (aad, rest) = frame.split_at(HEADER_LEN);
    let (body, tag) = rest.split_at(rest.len() - TAG_LEN);
    let tag: [u8; TAG_LEN] = tag
        .try_into()
        .map_err(|_| frame_err("authentication tag is the wrong length".into()))?;
    let cipher = TowerCcm::new(key.into());
    let mut buf = body.to_vec();
    cipher
        .decrypt_in_place_detached(
            &nonce(header.src, header.counter).into(),
            aad,
            &mut buf,
            (&tag).into(),
        )
        .map_err(|_| {
            frame_err("AES-CCM tag did not verify: the frame is forged or corrupt".into())
        })?;
    Ok((header, buf))
}

/// CMAC over `label ‖ 0x01 ‖ context`, zero-padded to 32 B — the construction of
/// the earlier P2P wire's session key with a TOWER label. Both inputs fit one 32 B block pair.
fn kdf(app_key: &[u8; 16], label: &[u8], context: &[&[u8]]) -> [u8; 16] {
    let mut input = Vec::with_capacity(32);
    input.extend_from_slice(label);
    input.push(0x01);
    for part in context {
        input.extend_from_slice(part);
    }
    input.resize(input.len().max(32).next_multiple_of(16), 0);
    cmac(app_key, &input)
}

/// AES-128-CMAC (NIST SP 800-38B), the PRF of the key derivation.
pub fn cmac(key: &[u8; 16], data: &[u8]) -> [u8; 16] {
    let mut mac = <Cmac<Aes128> as CmacKeyInit>::new_from_slice(key)
        .unwrap_or_else(|_| unreachable!("a 16-byte key is always valid for AES-128-CMAC"));
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// `join_key = CMAC(app_key, "HIO-TWR-JOIN" ‖ 0x01 ‖ DevEUI(8, MSB-first) ‖ pad32)`.
pub fn join_key(app_key: &[u8; 16], dev_eui: &[u8; 8]) -> [u8; 16] {
    kdf(app_key, LABEL_JOIN, &[dev_eui])
}

/// `session_key = CMAC(app_key, "HIO-TWR-SES" ‖ 0x01 ‖ dev_nonce(4 BE) ‖ central_nonce(4 BE) ‖
/// DevEUI(8) ‖ pad32)`.
pub fn session_key(
    app_key: &[u8; 16],
    dev_nonce: u32,
    central_nonce: u32,
    dev_eui: &[u8; 8],
) -> [u8; 16] {
    kdf(
        app_key,
        LABEL_SES,
        &[
            &dev_nonce.to_be_bytes(),
            &central_nonce.to_be_bytes(),
            dev_eui,
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_KAT: &str = include_str!("../../../tests/fixtures/tower_frame_kat.json");
    const JOIN_KAT: &str = include_str!("../../../tests/fixtures/tower_join_kat.json");

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn key(s: &str) -> [u8; 16] {
        hex(s).try_into().unwrap()
    }

    fn header_of(v: &serde_json::Value) -> Header {
        Header {
            frame_type: v["frame_type"].as_u64().unwrap_or(0) as u8,
            flags: v["flags"].as_u64().unwrap_or(0) as u8,
            src: v["src"].as_u64().unwrap() as u32,
            dest: v["dest"].as_u64().unwrap() as u32,
            counter: v["counter"].as_u64().unwrap() as u32,
        }
    }

    /// Every vector of the frame KAT, byte for byte: header, nonce, ciphertext, tag, frame —
    /// sealed here, and opened back to the plaintext.
    #[test]
    fn every_frame_kat_vector_seals_and_opens_byte_for_byte() {
        let kat: serde_json::Value = serde_json::from_str(FRAME_KAT).unwrap();
        let k = key(kat["key"].as_str().unwrap());
        let mut seen = 0;
        for (name, v) in kat.as_object().unwrap() {
            if !v.is_object() {
                continue;
            }
            seen += 1;
            let h = header_of(v);
            let pt = hex(v["plaintext"].as_str().unwrap());
            assert_eq!(
                h.encode().to_vec(),
                hex(v["header"].as_str().unwrap()),
                "{name} header"
            );
            assert_eq!(
                nonce(h.src, h.counter).to_vec(),
                hex(v["nonce"].as_str().unwrap()),
                "{name} nonce"
            );
            let frame = seal(&k, &h, &pt).unwrap();
            assert_eq!(frame, hex(v["frame"].as_str().unwrap()), "{name} frame");
            assert_eq!(
                frame.len() as u64,
                v["frame_len"].as_u64().unwrap(),
                "{name} len"
            );
            let ct_len = pt.len();
            assert_eq!(
                frame[HEADER_LEN..HEADER_LEN + ct_len].to_vec(),
                hex(v["ciphertext"].as_str().unwrap()),
                "{name} ciphertext"
            );
            assert_eq!(
                frame[HEADER_LEN + ct_len..].to_vec(),
                hex(v["tag"].as_str().unwrap()),
                "{name} tag"
            );
            let (oh, opened) = open(&k, &frame).unwrap();
            assert_eq!(oh, h, "{name} header back");
            assert_eq!(opened, pt, "{name} plaintext back");
        }
        assert_eq!(seen, 9, "the fixture's nine vectors");
    }

    #[test]
    fn a_flipped_bit_anywhere_fails_the_tag() {
        let kat: serde_json::Value = serde_json::from_str(FRAME_KAT).unwrap();
        let k = key(kat["key"].as_str().unwrap());
        let frame = hex(kat["uplink_confirmed"]["frame"].as_str().unwrap());
        for i in 0..frame.len() {
            let mut bad = frame.clone();
            bad[i] ^= 0x01;
            // Byte 0 carries the version: flipping bit 0 changes the type, which the tag
            // still catches.
            assert!(open(&k, &bad).is_err(), "bit flip at byte {i} was accepted");
        }
        let mut wrong_key = k;
        wrong_key[0] ^= 1;
        assert!(open(&wrong_key, &frame).is_err());
    }

    #[test]
    fn another_wire_version_and_a_short_frame_are_refused_before_any_crypto() {
        let mut f = vec![0u8; HEADER_LEN + TAG_LEN];
        f[0] = 2 << 5;
        assert!(Header::peek(&f).is_err());
        assert!(Header::peek(&[0x20; HEADER_LEN + TAG_LEN - 1]).is_err());
        f[0] = 1 << 5;
        assert!(Header::peek(&f).is_ok());
    }

    #[test]
    fn a_payload_past_the_mtu_is_refused() {
        let h = Header {
            frame_type: frame_type::DATA,
            flags: 0,
            src: 1,
            dest: 2,
            counter: 3,
        };
        assert_eq!(MAX_FRAME, 96, "the TOWER frame, #486 U-1");
        assert_eq!(MAX_PAYLOAD, 74, "96 B TOWER MTU - 14 B header - 8 B tag");
        assert_eq!(
            seal(&[0; 16], &h, &[0; MAX_PAYLOAD]).unwrap().len(),
            MAX_FRAME
        );
        assert!(seal(&[0; 16], &h, &[0; MAX_PAYLOAD + 1]).is_err());
    }

    /// The join KAT: both keys, and the JoinRequest, JoinAccept and first uplink frames.
    #[test]
    fn the_join_kat_keys_and_frames_match() {
        let kat: serde_json::Value = serde_json::from_str(JOIN_KAT).unwrap();
        let app_key = key(kat["app_key"].as_str().unwrap());
        let dev_eui: [u8; 8] = hex(kat["dev_eui"].as_str().unwrap()).try_into().unwrap();
        let dev_nonce = kat["dev_nonce"].as_u64().unwrap() as u32;
        let central_nonce = kat["central_nonce"].as_u64().unwrap() as u32;
        let jk = join_key(&app_key, &dev_eui);
        assert_eq!(jk, key(kat["join_key"].as_str().unwrap()));
        let sk = session_key(&app_key, dev_nonce, central_nonce, &dev_eui);
        assert_eq!(sk, key(kat["session_key"].as_str().unwrap()));
        assert_eq!(
            super::super::addr_of(kat["dev_eui"].as_str().unwrap()),
            kat["addr"].as_u64().map(|a| a as u32)
        );

        for (name, k) in [
            ("join_request", jk),
            ("join_accept", jk),
            ("first_uplink", sk),
        ] {
            let v = &kat[name];
            let frame = hex(v["frame"].as_str().unwrap());
            let (h, pt) = open(&k, &frame).unwrap();
            assert_eq!(pt, hex(v["plaintext"].as_str().unwrap()), "{name}");
            assert_eq!(h.src as u64, v["src"].as_u64().unwrap(), "{name}");
            assert_eq!(h.dest as u64, v["dest"].as_u64().unwrap(), "{name}");
            assert_eq!(h.counter as u64, v["counter"].as_u64().unwrap(), "{name}");
            assert_eq!(seal(&k, &h, &pt).unwrap(), frame, "{name} reseal");
        }
        let first = Header::peek(&hex(kat["first_uplink"]["frame"].as_str().unwrap())).unwrap();
        assert!(first.is_confirmed());
        assert_eq!(first.frame_type, frame_type::DATA);
    }

    /// RFC 4493's AES-CMAC test vectors, the standard check for SP 800-38B.
    #[test]
    fn cmac_matches_the_rfc_4493_vectors() {
        let k = key("2b7e151628aed2a6abf7158809cf4f3c");
        assert_eq!(
            cmac(&k, &[]).to_vec(),
            hex("bb1d6929e95937287fa37d129b756746")
        );
        assert_eq!(
            cmac(&k, &hex("6bc1bee22e409f96e93d7e117393172a")).to_vec(),
            hex("070a16b46b4d4144f79bdd9dd04a287c")
        );
    }
}
