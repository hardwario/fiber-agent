//! EUI normalization — `plan/control/radio/plan.md` §9.5.
//!
//! > **LoRaWAN Node *wire* identity = `dev_eui`**, normalized to **16 lowercase hex
//! > characters** everywhere — in topics, JSON, ChirpStack calls, and storage keys. Gateway
//! > identity = gateway EUI, same normalization. **Normalization rules, exactly**: accept a
//! > hex string in any letter case, optionally containing `:` or `-` separators; strip the
//! > separators, lowercase the rest; the result must be exactly 16 hex characters (8 bytes)
//! > — anything else is `InvalidArgument`, never a silent truncation or pad.
//!
//! The "never a silent truncation or pad" clause is the whole point: an EUI that is one
//! nibble short must be rejected, not zero-extended into a different device's identity.

use crate::error::{Component, Error, Kind, Result, Transport};

/// A `dev_eui` or gateway EUI in its one canonical form: exactly 16 lowercase hex digits.
///
/// The type exists so a normalized EUI cannot be confused with an operator-supplied string
/// anywhere downstream — construction is the only way in, and construction normalizes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct Eui(String);

impl Eui {
    /// Normalize an operator-supplied EUI.
    pub fn parse(input: &str) -> Result<Self> {
        let stripped: String = input.chars().filter(|c| *c != ':' && *c != '-').collect();

        let invalid = |why: &str| {
            Error::new(
                Kind::InvalidArgument,
                Transport::Lorawan,
                Component::EnrollmentRecord,
                format!("invalid EUI {input:?}: {why}"),
            )
        };

        if !stripped.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(invalid(
                "expected hex digits, optionally separated by ':' or '-'",
            ));
        }
        if stripped.len() != 16 {
            return Err(invalid(&format!(
                "expected exactly 16 hex digits (8 bytes) after removing separators, got {}",
                stripped.len()
            )));
        }
        Ok(Eui(stripped.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The inverse of [`Eui::to_bytes`]: 8 raw bytes back to the canonical 16 lowercase hex
    /// digits.
    ///
    /// Infallible — every byte array is a valid EUI, and the canonical form is exactly what
    /// `parse` would produce for the same bytes. This is the P2P join path's way in: a
    /// JoinRequest carries `dev_eui` as 8 MSB-first bytes (`p2p.md` §8.2, GitLab #73), and
    /// the registry is keyed by the parsed text form.
    pub fn from_bytes(bytes: [u8; 8]) -> Self {
        let mut s = String::with_capacity(16);
        for b in bytes {
            use std::fmt::Write as _;
            // Infallible: writing into a String never fails.
            let _ = write!(s, "{b:02x}");
        }
        Eui(s)
    }

    /// The 8 raw bytes, big-endian as written.
    pub fn to_bytes(&self) -> [u8; 8] {
        let mut out = [0u8; 8];
        for (i, byte) in out.iter_mut().enumerate() {
            // Safe by construction: `parse` guarantees 16 ASCII hex digits.
            let hi = hex_val(self.0.as_bytes()[i * 2]);
            let lo = hex_val(self.0.as_bytes()[i * 2 + 1]);
            *byte = (hi << 4) | lo;
        }
        out
    }
}

const fn hex_val(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

impl std::fmt::Display for Eui {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for Eui {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Eui::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_any_case_and_either_separator() {
        for input in [
            "0011223344556677",
            "0011223344556677".to_uppercase().as_str(),
            "00:11:22:33:44:55:66:77",
            "00-11-22-33-44-55-66-77",
            "00:11-22:33-44:55-66:77",
            "0011223344556677".to_string().as_str(),
        ] {
            assert_eq!(
                Eui::parse(input).expect("valid").as_str(),
                "0011223344556677",
                "input {input:?}"
            );
        }
        assert_eq!(
            Eui::parse("AaBbCcDdEeFf0011").expect("valid").as_str(),
            "aabbccddeeff0011"
        );
    }

    /// The clause that matters most: no silent truncation, no silent pad.
    #[test]
    fn wrong_length_is_rejected_never_padded_or_truncated() {
        for input in [
            "",
            "0",
            "001122334455667",   // 15 — one nibble short
            "00112233445566770", // 17
            "00112233445566778899",
            "00:11:22:33:44:55:66", // 14 after stripping
        ] {
            let err = Eui::parse(input).expect_err("must reject {input:?}");
            assert_eq!(err.kind(), Kind::InvalidArgument, "input {input:?}");
            assert!(err.message().contains("16 hex digits"), "input {input:?}");
        }
    }

    #[test]
    fn non_hex_is_rejected() {
        for input in [
            "00112233445566zz",
            "0011223344556677 ",
            "0x00112233445566",
            "gg11223344556677",
        ] {
            let err = Eui::parse(input).expect_err("must reject");
            assert_eq!(err.kind(), Kind::InvalidArgument, "input {input:?}");
        }
    }

    #[test]
    fn normalization_is_idempotent() {
        let once = Eui::parse("00:11:AA:bb:44:55:66:77").expect("valid");
        let twice = Eui::parse(once.as_str()).expect("valid");
        assert_eq!(once, twice);
    }

    #[test]
    fn bytes_are_big_endian_as_written() {
        assert_eq!(
            Eui::parse("0011223344556677").expect("valid").to_bytes(),
            [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77]
        );
        assert_eq!(
            Eui::parse("ffeeddccbbaa9988").expect("valid").to_bytes(),
            [0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99, 0x88]
        );
    }

    #[test]
    fn from_bytes_round_trips_through_to_bytes_and_is_lowercase() {
        const DEV_EUI: [u8; 8] = [0x70, 0xb3, 0xd5, 0x7e, 0xd0, 0x00, 0x0a, 0xbe];

        let e = Eui::from_bytes(DEV_EUI);
        assert_eq!(e.as_str(), "70b3d57ed0000abe");
        assert_eq!(e.to_bytes(), DEV_EUI);

        // The on-air bytes and the operator's spelling land on the same value — this is
        // what lets the P2P runtime look a JoinRequest's `dev_eui` up in a registry keyed
        // by what `node-add --dev-eui` normalized.
        assert_eq!(
            Eui::parse("70:B3:D5:7E:D0:00:0A:BE").expect("valid"),
            Eui::from_bytes(DEV_EUI)
        );

        // Every byte value renders as two lowercase digits, including the ones that need a
        // leading zero and the ones with letters.
        assert_eq!(
            Eui::from_bytes([0x00, 0x0f, 0xa0, 0xff, 0x01, 0x10, 0xab, 0xcd]).as_str(),
            "000fa0ff0110abcd"
        );
    }

    #[test]
    fn serde_round_trips_through_the_canonical_form() {
        let e: Eui = serde_json::from_str("\"00:11:22:33:44:55:66:77\"").expect("deserializes");
        assert_eq!(e.as_str(), "0011223344556677");
        assert_eq!(
            serde_json::to_string(&e).expect("serializes"),
            "\"0011223344556677\""
        );
        // A malformed EUI in stored JSON is an error, not a silently-kept bad value.
        assert!(serde_json::from_str::<Eui>("\"nope\"").is_err());
    }
}
