//! The parser registry — `plan/control/ble/plan.md` §7.
//!
//! §7 makes this a registry rather than a match arm on purpose: `beacon-btsmp1.md` §4 says variant
//! proliferation is the normal case for this hardware, so "a two-variant world should not be
//! hardcoded anywhere". Adding a variant is a row here plus a module; nothing else changes.
//!
//! Lookup is **not** a fallback. An unknown name returns `None` rather than quietly decoding as
//! BTSMP1, because guessing a layout is the one failure mode §7 rejects outright.

use super::{btsmp1, ParseFn};

/// Every parser this build can run, by its stable name.
pub const PARSERS: &[(&str, ParseFn)] = &[("btsmp1", btsmp1::parse)];

/// The parser function for `name`, or `None` if this build has none.
pub fn lookup(name: &str) -> Option<ParseFn> {
    entry(name).map(|(_, f)| f)
}

/// The canonical name and function together — the name is `&'static str` so a decode can carry its
/// provenance without allocating.
pub fn entry(name: &str) -> Option<(&'static str, ParseFn)> {
    PARSERS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(n, f)| (*n, *f))
}

/// The name every beacon decodes with today. A per-beacon parser choice is what the registry makes
/// possible; nothing needs it yet, so there is one default rather than a config key nobody sets.
pub const DEFAULT_PARSER: &str = "btsmp1";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_parser_is_registered() {
        assert!(lookup(DEFAULT_PARSER).is_some());
    }

    #[test]
    fn names_are_unique_so_a_lookup_is_unambiguous() {
        let mut names: Vec<&str> = PARSERS.iter().map(|(n, _)| *n).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate parser name in the registry");
    }

    #[test]
    fn an_unknown_name_is_none_and_never_a_silent_fallback() {
        assert!(lookup("").is_none());
        assert!(lookup("BTSMP1").is_none(), "the name is case-sensitive");
        assert!(lookup("btsmp2").is_none());
    }

    #[test]
    fn decode_with_distinguishes_no_such_parser_from_a_rejected_frame() {
        use crate::decode::{decode_with, ParseError};
        assert!(
            decode_with("nope", &[0x01, 0x00]).is_none(),
            "no such parser"
        );
        let out = decode_with("btsmp1", &[0x01]).expect("the parser exists");
        assert_eq!(out.unwrap_err(), ParseError::TooShort);
        let out = decode_with("btsmp1", &[0x01, 0x02, 0x2A]).expect("the parser exists");
        let decoded = out.expect("decodes");
        assert_eq!(decoded.parser, "btsmp1");
        assert_eq!(decoded.reading.humidity_pct, Some(42));
    }
}
