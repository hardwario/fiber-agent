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

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::libs::mqtt::messages::ActivationMode;

/// All-zero JoinEUI default — mirrors `messages::default_join_eui` so a phone
/// that omits it stays compatible with the OTAA command.
fn default_join_eui() -> String {
    "0000000000000000".to_string()
}

/// FB0D write payload. Fields map 1:1 onto `MqttCommand::AddLoRaWANSticker`
/// (OTAA only) — all decoded from the sticker's NFC tag by the phone.
#[derive(Clone, Debug, Deserialize)]
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

/// Most recent enrollment result, surfaced on FB0D read. `None` before the
/// first write → read returns the `Default` (success=false, empty).
static LAST_RESULT: Mutex<Option<StickerAddResponse>> = Mutex::new(None);

pub fn set_last_result(result: StickerAddResponse) {
    if let Ok(mut guard) = LAST_RESULT.lock() {
        *guard = Some(result);
    }
}

pub fn last_result() -> StickerAddResponse {
    LAST_RESULT
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default()
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
/// name and serial_number must be non-empty. Returns a human-readable reason on
/// failure (surfaced in the FB0D response `message`).
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
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err("name must not be empty".to_string());
    }
    let serial_number = req.serial_number.trim().to_string();
    if serial_number.is_empty() {
        return Err("serial_number must not be empty".to_string());
    }

    Ok(PreparedAdd {
        dev_eui,
        name,
        serial_number,
        activation: ActivationMode::Otaa { app_key, join_eui },
    })
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
    fn deserialize_defaults_joineui() {
        let json = r#"{"deveui":"0011223344556677","appkey":"00112233445566778899aabbccddeeff","name":"x","serial_number":"s"}"#;
        let r: StickerAddRequest = serde_json::from_str(json).unwrap();
        assert_eq!(r.joineui, "0000000000000000");
    }

    #[test]
    fn last_result_roundtrip() {
        set_last_result(StickerAddResponse {
            pending: false,
            success: true,
            message: "ok".to_string(),
            deveui: "0011223344556677".to_string(),
        });
        let r = last_result();
        assert!(r.success);
        assert!(!r.pending);
        assert_eq!(r.deveui, "0011223344556677");
    }
}
