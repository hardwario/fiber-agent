//! Sticker-add GATT characteristic (FB0D).
//!
//! Thin transport layer over the shared `add_lorawan_sticker` path: parse and
//! validate the FB0D JSON, build an OTAA `ActivationMode`, and hand off to the
//! same full add the MQTT command uses. The credentials are decoded from NFC
//! by the phone; this side only validates shape and enrolls.
//!
//! FB0D is write + read: the write performs the enrollment, the read returns
//! the structured result of the most recent write (mirrors the FB01
//! auth-response pattern) so the app can confirm without a list characteristic.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::libs::mqtt::messages::ActivationMode;

/// Max raw FB0D write payload (bytes). A well-formed request is ~200 B; cap at
/// 4 KiB so a malicious peer cannot push megabytes through `serde_json` before
/// validation rejects it.
pub const MAX_PAYLOAD_BYTES: usize = 4096;

/// Max length (chars) for free-form string fields (`name`, `serial_number`).
/// These flow into YAML on disk and into log lines; 64 is comfortably above
/// real-world labels and small enough to keep logs readable.
pub const MAX_STR_CHARS: usize = 64;

/// All-zero JoinEUI default — mirrors `messages::default_join_eui` so a phone
/// that omits it stays compatible with the OTAA command.
fn default_join_eui() -> String {
    "0000000000000000".to_string()
}

/// FB0D write payload. Fields map 1:1 onto `MqttCommand::AddLoRaWANSticker`
/// (OTAA only) — all decoded from the sticker's NFC tag by the phone.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StickerAddRequest {
    pub deveui: String,
    #[serde(default = "default_join_eui")]
    pub joineui: String,
    pub appkey: String,
    pub name: String,
    pub serial_number: String,
}

/// FB0D read payload — the state of the most recent enrollment.
///
/// The write returns immediately (the add — ChirpStack gRPC + disk — can take
/// seconds, longer than a BLE write-response ACK), so the enrollment runs in
/// the background. The client polls this via FB0D read: `pending=true` while it
/// runs, then `pending=false` with the final `success`/`message`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StickerAddResponse {
    /// True while the enrollment is still running (poll again).
    #[serde(default)]
    pub pending: bool,
    pub success: bool,
    pub message: String,
    pub deveui: String,
}

/// Per-`ServiceState` slot holding the most recent FB0D result. Scoped to a
/// single GATT-server instance (not a process-global), so the slot can be
/// reset on BLE disconnect and one client cannot read another's result.
pub type SharedResult = Arc<Mutex<StickerAddResponse>>;

pub fn new_slot() -> SharedResult {
    Arc::new(Mutex::new(StickerAddResponse::default()))
}

/// Read the current slot. Returns the default response if the lock is
/// poisoned — the caller treats that as "no result yet".
pub fn read(slot: &SharedResult) -> StickerAddResponse {
    slot.lock().map(|g| g.clone()).unwrap_or_default()
}

/// Overwrite the slot. Recovers from a poisoned lock so a panicked prior
/// holder cannot strand the slot.
pub fn store(slot: &SharedResult, resp: StickerAddResponse) {
    let mut g = match slot.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    *g = resp;
}

/// Reset the slot to the default (no result). Called on BLE disconnect.
pub fn reset(slot: &SharedResult) {
    store(slot, StickerAddResponse::default());
}

/// Atomic "begin a new enrollment" gate.
///
/// If the slot is free (`pending == false`), transitions it to
/// `pending=true` for `deveui` and returns `true`. If another enrollment is
/// still pending, leaves the slot untouched and returns `false` — the caller
/// must reject the new write so a slow add cannot be clobbered by spam.
pub fn try_begin(slot: &SharedResult, deveui: String) -> bool {
    let mut g = match slot.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if g.pending {
        return false;
    }
    *g = StickerAddResponse {
        pending: true,
        success: false,
        message: "enrolling".to_string(),
        deveui,
    };
    true
}

/// True iff `s` is exactly `len` hex digits.
fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Validated, normalized command components ready for `add_lorawan_sticker`.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedAdd {
    pub dev_eui: String,
    pub name: String,
    pub serial_number: String,
    pub activation: ActivationMode,
}

/// Validate the request and build the OTAA command components.
///
/// Hex fields are length-checked (deveui/joineui 16, appkey 32) and lowercased;
/// `name` and `serial_number` are trimmed, must be non-empty, must not exceed
/// `MAX_STR_CHARS`, and must not contain ASCII control characters (newlines
/// would break YAML quoting and inject into log lines). Returns a human-
/// readable reason on failure (surfaced in the FB0D response `message`).
pub fn prepare(req: &StickerAddRequest) -> Result<PreparedAdd, String> {
    let dev_eui = req.deveui.trim().to_lowercase();
    let join_eui = req.joineui.trim().to_lowercase();
    let app_key = req.appkey.trim().to_lowercase();

    if !is_hex(&dev_eui, 16) {
        return Err("deveui must be 16 hex chars".to_string());
    }
    // An all-zero DevEUI is not a real device. (An all-zero JoinEUI, by
    // contrast, is the legitimate default and is allowed.)
    if dev_eui.bytes().all(|b| b == b'0') {
        return Err("deveui must not be all zeros".to_string());
    }
    if !is_hex(&join_eui, 16) {
        return Err("joineui must be 16 hex chars".to_string());
    }
    if !is_hex(&app_key, 32) {
        return Err("appkey must be 32 hex chars".to_string());
    }
    let name = check_label(req.name.trim(), "name")?;
    let serial_number = check_label(req.serial_number.trim(), "serial_number")?;

    Ok(PreparedAdd {
        dev_eui,
        name,
        serial_number,
        activation: ActivationMode::Otaa { app_key, join_eui },
    })
}

fn check_label(s: &str, field: &str) -> Result<String, String> {
    if s.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if s.chars().count() > MAX_STR_CHARS {
        return Err(format!("{field} too long (>{} chars)", MAX_STR_CHARS));
    }
    if s.chars().any(|c| c.is_control()) {
        return Err(format!("{field} must not contain control characters"));
    }
    Ok(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> StickerAddRequest {
        StickerAddRequest {
            deveui: "0011223344556677".to_string(),
            joineui: "8899AABBCCDDEEFF".to_string(),
            appkey: "00112233445566778899AABBCCDDEEFF".to_string(),
            name: "Fridge 1".to_string(),
            serial_number: "SN-001".to_string(),
        }
    }

    #[test]
    fn prepare_ok_lowercases_and_builds_otaa() {
        let p = prepare(&req()).unwrap();
        assert_eq!(p.dev_eui, "0011223344556677");
        assert_eq!(p.name, "Fridge 1");
        assert_eq!(p.serial_number, "SN-001");
        match p.activation {
            ActivationMode::Otaa { app_key, join_eui } => {
                assert_eq!(app_key, "00112233445566778899aabbccddeeff");
                assert_eq!(join_eui, "8899aabbccddeeff");
            }
            _ => panic!("expected OTAA"),
        }
    }

    #[test]
    fn prepare_rejects_bad_deveui() {
        let mut r = req();
        r.deveui = "00112233".to_string(); // too short
        assert!(prepare(&r).is_err());
        r.deveui = "001122334455667g".to_string(); // non-hex
        assert!(prepare(&r).is_err());
    }

    #[test]
    fn prepare_rejects_all_zero_deveui() {
        let mut r = req();
        r.deveui = "0000000000000000".to_string();
        assert!(prepare(&r).unwrap_err().contains("all zeros"));
        // all-zero joineui is the legitimate default and must still pass
        let mut r2 = req();
        r2.joineui = "0000000000000000".to_string();
        assert!(prepare(&r2).is_ok());
    }

    #[test]
    fn prepare_rejects_bad_appkey() {
        let mut r = req();
        r.appkey = "deadbeef".to_string();
        assert!(prepare(&r).unwrap_err().contains("appkey"));
    }

    #[test]
    fn prepare_rejects_empty_name_and_serial() {
        let mut r = req();
        r.name = "  ".to_string();
        assert!(prepare(&r).unwrap_err().contains("name"));
        let mut r2 = req();
        r2.serial_number = String::new();
        assert!(prepare(&r2).unwrap_err().contains("serial_number"));
    }

    #[test]
    fn prepare_rejects_oversized_name() {
        let mut r = req();
        r.name = "a".repeat(MAX_STR_CHARS + 1);
        let err = prepare(&r).unwrap_err();
        assert!(err.contains("name") && err.contains("too long"));
    }

    #[test]
    fn prepare_rejects_control_chars_in_serial() {
        let mut r = req();
        r.serial_number = "SN\n001".to_string();
        let err = prepare(&r).unwrap_err();
        assert!(err.contains("serial_number") && err.contains("control"));
    }

    #[test]
    fn deserialize_defaults_joineui() {
        let json = r#"{"deveui":"0011223344556677","appkey":"00112233445566778899aabbccddeeff","name":"x","serial_number":"s"}"#;
        let r: StickerAddRequest = serde_json::from_str(json).unwrap();
        assert_eq!(r.joineui, "0000000000000000");
    }

    #[test]
    fn deserialize_rejects_unknown_fields() {
        let json = r#"{"deveui":"0011223344556677","appkey":"00112233445566778899aabbccddeeff","name":"x","serial_number":"s","sneaky":"x"}"#;
        let r: Result<StickerAddRequest, _> = serde_json::from_str(json);
        assert!(r.is_err(), "unknown fields must be rejected");
    }

    #[test]
    fn try_begin_blocks_second_caller_while_pending() {
        let slot = new_slot();
        assert!(try_begin(&slot, "deveui-a".to_string()));
        // A second caller arrives while A is still running.
        assert!(
            !try_begin(&slot, "deveui-b".to_string()),
            "try_begin must refuse a second enrollment while one is pending"
        );
        // The pending state is for A; B did not clobber it.
        let cur = read(&slot);
        assert!(cur.pending);
        assert_eq!(cur.deveui, "deveui-a");
    }

    #[test]
    fn store_clears_pending_and_try_begin_succeeds_again() {
        let slot = new_slot();
        assert!(try_begin(&slot, "deveui-a".to_string()));
        // Finish the enrollment.
        store(
            &slot,
            StickerAddResponse {
                pending: false,
                success: true,
                message: "sticker enrolled".to_string(),
                deveui: "deveui-a".to_string(),
            },
        );
        // Now a new enrollment is allowed.
        assert!(try_begin(&slot, "deveui-b".to_string()));
        assert_eq!(read(&slot).deveui, "deveui-b");
    }

    #[test]
    fn reset_clears_pending_slot() {
        let slot = new_slot();
        assert!(try_begin(&slot, "deveui-a".to_string()));
        reset(&slot);
        let r = read(&slot);
        assert!(!r.pending);
        assert!(r.deveui.is_empty());
    }
}
