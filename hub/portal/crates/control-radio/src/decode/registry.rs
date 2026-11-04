//! The decoder registry — `plan/control/radio/plan.md` §4.
//!
//! > `decoder: "node"` is a lookup key into a compiled-in Rust registry, not a file path and
//! > not a dynamically loaded plugin.
//!
//! Adding a device type is a new module plus one entry here — a normal build and deploy.
//! Reassigning which registered decoder a profile uses is a config change with no rebuild.
//!
//! Two alternatives were rejected in the plan and stay rejected: an embedded JS interpreter,
//! and `libloading` of `.so` files — the latter has no stable Rust ABI across compiler
//! versions and is in direct tension with decision #6's "no runtime path discovery, declared
//! surface".

use serde_json::{Map, Value};

/// A decoder turns one uplink's bytes, on one fPort, into named fields.
pub type DecodeFn = fn(f_port: u8, payload: &[u8]) -> Map<String, Value>;

/// Every registered decoder. The `decoder:` key in a device profile indexes this.
static DECODERS: &[(&str, DecodeFn)] = &[("node", crate::decode::node_decode)];

pub fn lookup(name: &str) -> Option<DecodeFn> {
    DECODERS.iter().find(|(k, _)| *k == name).map(|(_, f)| *f)
}

/// Every registered name, for `status` and for the config-validation test.
pub fn registered() -> Vec<&'static str> {
    DECODERS.iter().map(|(k, _)| *k).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_node_decoder_is_registered() {
        assert!(lookup("node").is_some());
        assert_eq!(registered(), vec!["node"]);
    }

    /// An unregistered name resolves to nothing rather than falling back to a default — no
    /// transport may be left with an implicit decoder (§4).
    #[test]
    fn an_unknown_decoder_does_not_fall_back() {
        assert!(lookup("nope").is_none());
        assert!(lookup("").is_none());
        assert!(
            lookup("NODE").is_none(),
            "lookup is exact, not case-insensitive"
        );
    }
}
