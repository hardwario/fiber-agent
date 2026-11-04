//! MAC identity — `plan/control/ble/plan.md` §5 and §9.5: **`--mac` is the one identifier**,
//! normalized to uppercase colon-separated on every surface, so no translation layer appears
//! anywhere. Accepted on input in any case, with or without separators; anything that is not
//! exactly six octets is `InvalidArgument`, **never a silent truncation or pad**.
//!
//! §9.5's other half — the address is a *stable* identity on BTSMP1 hardware, so the raw MAC is the
//! key — is what lets this be a plain newtype with no resolution table behind it.

use crate::error::{Component, Error, Kind, Result};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MacAddr([u8; 6]);

impl MacAddr {
    /// Parse any of `7c:d9:f4:13:10:de`, `7C-D9-F4-13-10-DE`, `7CD9F41310DE`.
    ///
    /// The rule is exactly six octets: separators (`:` and `-`) are stripped, then the remainder
    /// must be 12 hex digits. Five octets, seven octets, or a non-hex digit are all
    /// `InvalidArgument` — the one thing this must never do is truncate or pad to fit.
    pub fn parse(s: &str) -> Result<MacAddr> {
        let raw = s.trim();
        let hex: String = raw.chars().filter(|c| *c != ':' && *c != '-').collect();
        if hex.len() != 12 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::new(
                Kind::InvalidArgument,
                None,
                Component::BeaconRegistry,
                format!(
                    "{raw:?} is not a MAC address: expected exactly six hex octets, optionally \
                     separated by ':' or '-' (for example 7C:D9:F4:13:10:DE)"
                ),
            ));
        }
        let mut bytes = [0u8; 6];
        for (i, b) in bytes.iter_mut().enumerate() {
            // 12 validated hex digits: each 2-char slice parses.
            *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|e| {
                Error::new(
                    Kind::InvalidArgument,
                    None,
                    Component::BeaconRegistry,
                    format!("{raw:?} is not a MAC address: {e}"),
                )
            })?;
        }
        Ok(MacAddr(bytes))
    }

    pub const fn octets(&self) -> [u8; 6] {
        self.0
    }

    /// `AA-BB-CC-DD-EE-FF` — a filename component, because `:` is hostile in paths.
    pub fn as_file_stem(&self) -> String {
        let o = self.0;
        format!(
            "{:02X}-{:02X}-{:02X}-{:02X}-{:02X}-{:02X}",
            o[0], o[1], o[2], o[3], o[4], o[5]
        )
    }

    pub const fn to_bluer(&self) -> bluer::Address {
        bluer::Address::new(self.0)
    }

    pub const fn from_bluer(addr: bluer::Address) -> MacAddr {
        MacAddr(addr.0)
    }
}

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let o = self.0;
        write!(
            f,
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            o[0], o[1], o[2], o[3], o[4], o[5]
        )
    }
}

impl From<[u8; 6]> for MacAddr {
    fn from(v: [u8; 6]) -> Self {
        MacAddr(v)
    }
}

impl From<MacAddr> for [u8; 6] {
    fn from(v: MacAddr) -> Self {
        v.0
    }
}

impl From<bluer::Address> for MacAddr {
    fn from(a: bluer::Address) -> Self {
        MacAddr(a.0)
    }
}

impl From<MacAddr> for bluer::Address {
    fn from(m: MacAddr) -> Self {
        bluer::Address::new(m.0)
    }
}

impl std::str::FromStr for MacAddr {
    type Err = Error;
    fn from_str(s: &str) -> Result<MacAddr> {
        MacAddr::parse(s)
    }
}

/// Serde is the canonical string, both ways — so a `beacons.json` round-trip cannot change case.
impl serde::Serialize for MacAddr {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for MacAddr {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<MacAddr, D::Error> {
        let s = String::deserialize(d)?;
        MacAddr::parse(&s).map_err(|e| serde::de::Error::custom(e.message().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANON: &str = "7C:D9:F4:13:10:DE";

    #[test]
    fn every_accepted_spelling_normalizes_to_the_canonical_one() {
        for input in [
            "7c:d9:f4:13:10:de",
            "7C:D9:F4:13:10:DE",
            "7CD9F41310DE",
            "7cd9f41310de",
            "7C-D9-F4-13-10-DE",
            "7c-d9-f4-13-10-de",
            "  7c:d9:f4:13:10:de  ",
        ] {
            let m = MacAddr::parse(input).unwrap_or_else(|e| panic!("{input:?}: {e}"));
            assert_eq!(m.to_string(), CANON, "{input:?}");
        }
    }

    #[test]
    fn anything_that_is_not_six_octets_is_invalid_argument() {
        for bad in [
            "AA:BB:CC:DD:EE",    // five octets — never padded
            "AABBCCDDEEFF00",    // seven octets — never truncated
            "GG:HH:II:JJ:KK:LL", // not hex
            "not-a-mac",
            "",
            "7C:D9:F4:13:10:D",   // eleven digits
            "7C:D9:F4:13:10:DEE", // thirteen digits
        ] {
            let e = MacAddr::parse(bad).unwrap_err();
            assert_eq!(e.kind(), Kind::InvalidArgument, "{bad:?}");
            assert_eq!(e.component(), Component::BeaconRegistry, "{bad:?}");
        }
    }

    #[test]
    fn the_file_stem_replaces_colons_so_it_is_path_safe() {
        let m = MacAddr::parse(CANON).unwrap();
        assert_eq!(m.as_file_stem(), "7C-D9-F4-13-10-DE");
        assert!(!m.as_file_stem().contains(':'));
        assert!(!m.as_file_stem().contains('/'));
    }

    #[test]
    fn the_bluer_conversion_round_trips_both_ways() {
        let m = MacAddr::parse(CANON).unwrap();
        assert_eq!(m.to_bluer().0, [0x7C, 0xD9, 0xF4, 0x13, 0x10, 0xDE]);
        assert_eq!(MacAddr::from_bluer(m.to_bluer()), m);
        let via_into: bluer::Address = m.into();
        assert_eq!(MacAddr::from(via_into), m);
    }

    #[test]
    fn serde_is_the_canonical_string_in_both_directions() {
        let m = MacAddr::parse("7c:d9:f4:13:10:de").unwrap();
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, format!("\"{CANON}\""));
        // Lower case in, canonical out: a stored file cannot drift in case.
        let back: MacAddr = serde_json::from_str("\"7c:d9:f4:13:10:de\"").unwrap();
        assert_eq!(back, m);
        assert_eq!(back.to_string(), CANON);
        assert!(serde_json::from_str::<MacAddr>("\"nope\"").is_err());
    }

    #[test]
    fn ordering_is_by_octet_so_snapshots_sort_the_way_the_page_walk_expects() {
        let mut v = [
            MacAddr::parse("FF:00:00:00:00:00").unwrap(),
            MacAddr::parse("00:00:00:00:00:01").unwrap(),
            MacAddr::parse("00:00:00:00:00:00").unwrap(),
        ];
        v.sort();
        assert_eq!(
            v.iter().map(|m| m.to_string()).collect::<Vec<_>>(),
            vec![
                "00:00:00:00:00:00",
                "00:00:00:00:00:01",
                "FF:00:00:00:00:00"
            ]
        );
    }
}
