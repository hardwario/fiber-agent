//! EYE Tag Add GATT characteristic (FB0E).
//!
//! Lets the manager-app pair a Teltonika EYE (BTSMP1) BLE sensor tag with this
//! FIBER: the app scans the tag's MAC from its QR label and writes it here; the
//! FIBER then discovers and provisions the tag itself over BLE (the FIBER is a
//! peripheral for the phone and a central for the tags). Mirrors FB0D
//! "Sticker Add" — see issue #84.
//!
//! FB0E is write + read: the write enrolls the tag into `eye.tags[]` via the
//! same `add_eye_tag` path the MQTT command uses, and the read returns the
//! structured result of the most recent write (so the app can confirm without a
//! list characteristic — the FB0D / FB01 pattern). Unlike FB0D, enrollment is a
//! synchronous local YAML write (no ChirpStack gRPC), so there is no background
//! task and no `pending` poll: the write applies inline and the read is final.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// Max raw FB0E write payload (bytes). A well-formed request is a few dozen
/// bytes; cap well above that so a malicious peer cannot push megabytes through
/// `serde_json` before validation rejects it.
pub const MAX_PAYLOAD_BYTES: usize = 1024;

/// Max length (chars) for the optional `name` label. It flows into YAML on disk
/// and into log lines; 64 is comfortably above real-world labels.
pub const MAX_NAME_CHARS: usize = 64;

/// FB0E write payload: the tag MAC (required, normalized `AA:BB:CC:DD:EE:FF`)
/// plus an optional cosmetic name.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EyeTagAddRequest {
    pub mac: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// FB0E read payload — the result of the most recent enrollment. Matches the
/// issue #84 contract (`{ "success": true, "message": "" }`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EyeTagAddResponse {
    pub success: bool,
    pub message: String,
}

/// Per-`ServiceState` slot holding the most recent FB0E result. Scoped to a
/// single GATT-server instance (not process-global) so it can be reset on BLE
/// disconnect and one client cannot read another's result.
pub type SharedResult = Arc<Mutex<EyeTagAddResponse>>;

pub fn new_slot() -> SharedResult {
    Arc::new(Mutex::new(EyeTagAddResponse::default()))
}

/// Read the current slot. Returns the default response if the lock is poisoned.
pub fn read(slot: &SharedResult) -> EyeTagAddResponse {
    slot.lock().map(|g| g.clone()).unwrap_or_default()
}

/// Overwrite the slot, recovering from a poisoned lock.
pub fn store(slot: &SharedResult, resp: EyeTagAddResponse) {
    let mut g = match slot.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    *g = resp;
}

/// Reset the slot to the default (no result). Called on BLE disconnect.
pub fn reset(slot: &SharedResult) {
    store(slot, EyeTagAddResponse::default());
}

/// Validated, normalized enrollment request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedEyeAdd {
    /// Uppercase `AA:BB:CC:DD:EE:FF`.
    pub mac: String,
    /// Cleaned optional label (`None` if omitted or blank).
    pub name: Option<String>,
}

/// Validate + normalize the request.
///
/// The MAC is trimmed, uppercased, and checked with the same
/// [`crate::libs::eye::state::is_valid_mac`] used at every EYE MQTT choke point,
/// so the FB0E path cannot slip a malformed MAC past (`parse_mac` in the
/// recorder path would otherwise coerce bad hex to `00:..`). `name`, if present,
/// is trimmed; a blank name becomes `None`; otherwise it must be
/// `<= MAX_NAME_CHARS` and free of ASCII control characters (which would break
/// YAML quoting or inject into log lines). Returns a human-readable reason on
/// failure (surfaced in the FB0E response `message`).
pub fn prepare(req: &EyeTagAddRequest) -> Result<PreparedEyeAdd, String> {
    let mac = req.mac.trim().to_uppercase();
    if !crate::libs::eye::state::is_valid_mac(&mac) {
        return Err(format!("invalid MAC address: {mac}"));
    }
    let name = match req
        .name
        .as_ref()
        .map(|n| n.trim())
        .filter(|n| !n.is_empty())
    {
        None => None,
        Some(n) => {
            if n.chars().count() > MAX_NAME_CHARS {
                return Err(format!("name too long (>{MAX_NAME_CHARS} chars)"));
            }
            if n.chars().any(|c| c.is_control()) {
                return Err("name must not contain control characters".to_string());
            }
            Some(n.to_string())
        }
    };
    Ok(PreparedEyeAdd { mac, name })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_ok_uppercases_mac_and_keeps_name() {
        let p = prepare(&EyeTagAddRequest {
            mac: "7c:d9:f4:10:00:00".to_string(),
            name: Some("Lobby sensor".to_string()),
        })
        .unwrap();
        assert_eq!(p.mac, "7C:D9:F4:10:00:00");
        assert_eq!(p.name.as_deref(), Some("Lobby sensor"));
    }

    #[test]
    fn prepare_blank_name_becomes_none() {
        let p = prepare(&EyeTagAddRequest {
            mac: "AA:BB:CC:DD:EE:FF".to_string(),
            name: Some("   ".to_string()),
        })
        .unwrap();
        assert_eq!(p.name, None);
        // A missing name field also yields None.
        let p2 = prepare(&EyeTagAddRequest {
            mac: "AA:BB:CC:DD:EE:FF".to_string(),
            name: None,
        })
        .unwrap();
        assert_eq!(p2.name, None);
    }

    #[test]
    fn prepare_rejects_bad_mac() {
        for bad in [
            "not-a-mac",
            "AA:BB:CC:DD:EE",
            "AABBCCDDEEFF",
            "GG:BB:CC:DD:EE:FF",
        ] {
            let r = prepare(&EyeTagAddRequest {
                mac: bad.to_string(),
                name: None,
            });
            assert!(r.is_err(), "{bad} must be rejected");
            assert!(r.unwrap_err().contains("invalid MAC"));
        }
    }

    #[test]
    fn prepare_rejects_oversized_name() {
        let r = prepare(&EyeTagAddRequest {
            mac: "AA:BB:CC:DD:EE:FF".to_string(),
            name: Some("a".repeat(MAX_NAME_CHARS + 1)),
        });
        assert!(r.unwrap_err().contains("too long"));
    }

    #[test]
    fn prepare_rejects_control_chars_in_name() {
        let r = prepare(&EyeTagAddRequest {
            mac: "AA:BB:CC:DD:EE:FF".to_string(),
            name: Some("Lobby\nsensor".to_string()),
        });
        assert!(r.unwrap_err().contains("control"));
    }

    #[test]
    fn deserialize_defaults_name_and_rejects_unknown_fields() {
        let ok: EyeTagAddRequest = serde_json::from_str(r#"{"mac":"AA:BB:CC:DD:EE:FF"}"#).unwrap();
        assert_eq!(ok.name, None);
        let bad: Result<EyeTagAddRequest, _> =
            serde_json::from_str(r#"{"mac":"AA:BB:CC:DD:EE:FF","sneaky":1}"#);
        assert!(bad.is_err(), "unknown fields must be rejected");
    }

    #[test]
    fn slot_store_read_reset_roundtrip() {
        let slot = new_slot();
        assert_eq!(read(&slot), EyeTagAddResponse::default());
        store(
            &slot,
            EyeTagAddResponse {
                success: true,
                message: "ok".into(),
            },
        );
        assert_eq!(
            read(&slot),
            EyeTagAddResponse {
                success: true,
                message: "ok".into()
            }
        );
        reset(&slot);
        assert!(!read(&slot).success);
        assert!(read(&slot).message.is_empty());
    }
}
