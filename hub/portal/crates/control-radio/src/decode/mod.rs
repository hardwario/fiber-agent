//! Uplink processing — `plan/control/radio/plan.md` §7.
//!
//! Decode runs **inside `control_radio`**, for both transports: ChirpStack's MQTT integration
//! event carries the raw bytes, and P2P frames enter the same path from the central service.
//! It is never a ChirpStack per-device-profile JavaScript codec — that rule is binding
//! (decision #8 §5), and it stays true once ChirpStack is centralized, because one Rust
//! registry serves every Client's gateways.

pub mod alarm_rule;
pub mod command;
pub mod command_encode;
pub mod device_status;
pub mod history;
pub mod node;
pub mod node_params;
pub mod pb;
pub mod reading;
pub mod registry;
pub mod sensor_types;
pub mod snapshot;

use serde_json::{Map, Value};

/// `APP_PROTO_VERSION` — the one-byte prefix on fPort 2/3/85 payloads. fPorts 1 and 10 are
/// unversioned.
pub const APP_PROTO_VERSION: u8 = 0x01;

/// A decoded uplink, in the reference decoder's own shape so parity is a direct comparison.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Decoded {
    pub data: Map<String, Value>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

/// Strip and validate the version prefix.
///
/// **This deliberately follows the reference decoder, not `plan.md` §7 step 1.** The plan says
/// an unexpected version is "log + skip the frame"; `ttn.js` warns and decodes the remainder
/// best-effort, and `node-firmware-v1.4.0.md` §1 says the same. The pinned repo is the wire
/// truth by that document's own rule ("on any conflict the pinned repo wins"), and the parity
/// acceptance requires field-for-field identity — a skipped frame fails it outright. Recorded
/// in the plan-feedback set as a correction plan.md §7 needs.
fn strip_version(bytes: &[u8], warnings: &mut Vec<String>) -> Vec<u8> {
    let (rest, warning) = strip_version_prefix(bytes);
    if let Some(w) = warning {
        warnings.push(w);
    }
    rest
}

/// The one place the `APP_PROTO_VERSION` rule lives, for every consumer of a versioned
/// payload: the field decoder above, the LoRaWAN fPort-85 command path
/// (`tasks::uplink`), and the P2P `0x55` correlation (`p2p::runtime`). The node prefixes
/// fPort 2/3/85 — and therefore the P2P `0x02`/`0x03`/`0x55` mirrors — with this byte
/// (`sticker-firmware` `app_cmd.h`); a `Response` decoded from offset 0 misreads it as a
/// protobuf tag and comes back with `seq = 0`, which is exactly the bench failure of
/// 2026-09-08 (the central kept a delivered command in flight forever). Returns the
/// remainder and, when the byte is missing or unknown, a warning — the remainder is still
/// decoded best-effort, following the reference decoder as documented on `strip_version`.
pub fn strip_version_prefix(bytes: &[u8]) -> (Vec<u8>, Option<String>) {
    match bytes.split_first() {
        None => {
            // An empty payload decoded to `{}` silently, which reads as "a frame with no
            // fields set" rather than "there was nothing to decode".
            (
                Vec::new(),
                Some("empty payload: nothing to decode".to_string()),
            )
        }
        Some((&v, rest)) if v == APP_PROTO_VERSION => (rest.to_vec(), None),
        Some((&v, rest)) => (
            rest.to_vec(),
            Some(format!("unknown payload version 0x{v:x}")),
        ),
    }
}

/// The `"node"` decoder's entry point, as stored in the registry.
///
/// fPort 85 is the command engine's, never the field decoder's, so it is not handled here.
fn node_decode(f_port: u8, payload: &[u8]) -> Map<String, Value> {
    match f_port {
        2 => node::decode_telemetry(payload),
        3 => node::decode_alarm_batch(payload),
        _ => Map::new(),
    }
}

/// Decode one uplink through a named registry decoder.
///
/// Returns `None` for a frame this path does not own: fPort 1 (the legacy bitmap, dropped in
/// v2 — the minimum supported firmware is v2.0.0), fPort 10 (calibration, log-and-skip in
/// phase 1), fPort 85 (the command engine's), and anything unrecognised.
pub fn decode_uplink(decoder: &str, f_port: u8, raw: &[u8]) -> Option<Decoded> {
    if !matches!(f_port, 2 | 3) {
        return None;
    }
    let decode = registry::lookup(decoder)?;
    let mut warnings = Vec::new();
    let payload = strip_version(raw, &mut warnings);
    Some(Decoded {
        data: decode(f_port, &payload),
        warnings,
        errors: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_byte_is_stripped_and_a_bad_one_only_warns() {
        let mut w = Vec::new();
        assert_eq!(strip_version(&[0x01, 0xaa, 0xbb], &mut w), vec![0xaa, 0xbb]);
        assert!(w.is_empty());

        let mut w = Vec::new();
        assert_eq!(strip_version(&[0x02, 0xaa], &mut w), vec![0xaa]);
        assert_eq!(w, vec!["unknown payload version 0x2"]);
    }

    #[test]
    fn an_empty_payload_does_not_panic() {
        let mut w = Vec::new();
        assert!(strip_version(&[], &mut w).is_empty());
    }

    /// fPorts this path does not own return None rather than an empty decode, so a caller
    /// cannot mistake "not mine" for "decoded to nothing".
    #[test]
    fn only_field_ports_are_decoded_here() {
        assert!(decode_uplink("node", 2, &[0x01]).is_some());
        assert!(decode_uplink("node", 3, &[0x01]).is_some());
        for port in [1u8, 10, 85, 0, 99] {
            assert!(
                decode_uplink("node", port, &[0x01]).is_none(),
                "fPort {port}"
            );
        }
    }

    #[test]
    fn an_unknown_decoder_name_yields_nothing() {
        assert!(decode_uplink("no-such-decoder", 2, &[0x01, 0x08, 0x64]).is_none());
    }
}

#[cfg(test)]
mod empty_payload_tests {
    /// F17: an empty payload decoded to `{}` with no warning, which reads as "a frame with no
    /// fields set" rather than "there was nothing to decode".
    #[test]
    fn an_empty_payload_warns_rather_than_decoding_silently() {
        let d = super::decode_uplink("node", 2, &[]).expect("fPort 2 is a known port");
        assert!(
            d.warnings.iter().any(|w| w.contains("empty payload")),
            "warnings name the empty payload: {:?}",
            d.warnings
        );
    }
}
