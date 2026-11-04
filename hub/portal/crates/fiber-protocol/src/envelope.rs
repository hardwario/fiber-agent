//! The Ed25519 signed-command envelope. mqtt.md "The signed envelope"
//! section and quirk #11 ("Signing canonicalization is Python-shaped").
//!
//! Canonicalization is byte-exact with `app/auth/command_signer.py`
//! (`sign_command`/`sign_device_command`/`sign_with_user_key`, all three
//! identical): the pre-image is JSON-serialized with **recursively**
//! sorted object keys and compact separators (`,`/`:`), matching Python's
//! `json.dumps(obj, sort_keys=True, separators=(",", ":"))`.
//!
//! # The v2 envelope (plan/portal/mqtt/design.md §3)
//!
//! Two fields ride **inside** the signature: `v` (the integer `2`) and
//! `level` (the verb's [`Level`], from the registry — never from the
//! caller, design.md §4.4 step 4). The outer `command` discriminator stays
//! unsigned and therefore may only route, never authorize.
//!
//! - `config_request` pre-image is **exactly** the 9 fields
//!   `{command_type, level, nonce, params, reason, request_id, signer_id,
//!   timestamp, v}`. `command` and `algorithm` are never included — the
//!   latter exists only on the internal model / HTTP response, never on
//!   the wire.
//! - `config_confirm` pre-image is **exactly** the 7 fields
//!   `{challenge_id, confirmation, level, nonce, signer_id, timestamp,
//!   v}`; `command_type`/`params`/`reason` ride along on the wire but are
//!   informational and unsigned. The confirm repeats the request's level so
//!   a confirm cannot escalate: the device checks it against the level it
//!   recorded on the pending challenge (design.md §3.3).
//! - `reason` is a **required string**; `""` is the absent value, never
//!   `null`. v1's device canonicalized a missing reason as `null` while the
//!   portal always emitted `""` — v2 states the rule so the asymmetry
//!   cannot come back.
//! - Key sorting is structural on both sides, so `level` landing between
//!   `command_type` and `nonce` (request) / between `confirmation` and
//!   `nonce` (confirm), and `v` landing last, costs no code — it changes
//!   only the vectors (`vectors/envelope_v2.json`, design.md §10).
//!
//! # RESOLVED — float parity (was an OPEN ISSUE)
//!
//! The concern was that `canonical_json`'s `serde_json::Number::to_string`
//! formatting might not match the verifier's float formatting
//! byte-for-byte. It doesn't need to match by coincidence — it's
//! structurally guaranteed, because the agent no longer verifies against
//! a Python-formatted string at all:
//!
//! - The agent's `application/Cargo.toml` builds `serde_json` with the
//!   **`arbitrary_precision`** feature. Under that feature, a `Number`
//!   parsed from JSON text stores the *original decimal text* verbatim
//!   and re-serializes it unchanged — it does not round-trip through
//!   `f64` at all (that's the feature's whole purpose: lossless numbers).
//! - The agent reconstructs its canonical pre-image by taking the
//!   `params: Value` it parsed straight off the wire, recursively
//!   re-sorting object keys (`AuthorizationManager::sort_json_keys`,
//!   `application/src/libs/authorization/manager.rs:387-399`, called from
//!   `build_canonical_request_message`, `:402-432`) and calling
//!   `serde_json::to_string` on it. Key sorting never touches number
//!   text, so every float in `params` reaches the agent's verification
//!   string **exactly as it appeared on the wire**.
//! - On this crate's side, [`sign_request`] signs a pre-image built from
//!   the *same* `params: Value` that then gets moved into
//!   `SignedRequest.params` for wire serialization (see `sign_request`
//!   below: `params.clone()` for the pre-image, `params` for the wire
//!   struct) — both serialized through the same `serde_json` `Number`
//!   formatting path (ryu-based shortest round-trip for `f64`), so the
//!   number text this crate signs is identical to the number text that
//!   goes out on the wire.
//!
//! Chain those two facts together: whatever number text this crate's
//! `canonical_json` produces during signing is exactly the text that
//! reaches the agent, and the agent's `arbitrary_precision` verifier
//! echoes that text back unchanged into its own canonical string. Parity
//! doesn't depend on ryu matching Python `repr()`, or even on ryu
//! matching itself across crate versions — it's a wire-text echo, not a
//! re-derivation. The float tests below pin this crate's ryu-formatted
//! output for representative values (`21.5`, `0.1`, `-18.2`, `2.675`,
//! `1e16`, `1e-7`) as a regression guard on the half of the chain this
//! crate controls.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

use crate::certificate::Certificate;
use crate::registry::{self, Level};

#[derive(Debug, thiserror::Error)]
pub enum EnvelopeError {
    #[error("signature does not verify")]
    InvalidSignature,
    #[error("invalid base64 signature: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("signature must be exactly 64 bytes, got {0}")]
    SignatureLength(usize),
}

/// The signed envelope version. A request without `v`, or with any other
/// value, is rejected by the device before any crypto with an
/// `unsupported_envelope` error — there is no dual-accept past P4
/// (design.md §3.2).
pub const ENVELOPE_VERSION: u8 = 2;

/// Wire form of a signed `config_request`, published to
/// `commands/config/request`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedRequest {
    pub command: String,
    /// Signed. Always [`ENVELOPE_VERSION`].
    pub v: u8,
    pub command_type: String,
    /// Signed. The verb's level, taken from
    /// [`crate::registry`] — the device checks this claim against its own
    /// table before any crypto runs.
    pub level: Level,
    pub nonce: String,
    pub params: Value,
    pub reason: String,
    pub request_id: String,
    pub signer_id: String,
    pub timestamp: i64,
    pub signature: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub certificate: Option<Certificate>,
}

/// Wire form of a signed `config_confirm`, published to
/// `commands/config/confirm`. `command_type`/`params`/`reason` ride along
/// informational and unsigned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedConfirm {
    pub command: String,
    /// Signed. Always [`ENVELOPE_VERSION`].
    pub v: u8,
    pub challenge_id: String,
    pub confirmation: String,
    /// Signed. The level of the verb the challenge was minted for — a
    /// confirm cannot escalate past its own request.
    pub level: Level,
    pub nonce: String,
    pub signer_id: String,
    pub timestamp: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub signature: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub certificate: Option<Certificate>,
}

impl SignedRequest {
    /// The exact string this envelope's signature covers — the device's
    /// `build_canonical_request_message` by another name, and the string
    /// `vectors/envelope_v2.json` pins for all three implementations
    /// (design.md §10).
    pub fn canonical_message(&self) -> String {
        canonical_json(&request_preimage(
            &self.command_type,
            self.level,
            &self.nonce,
            &self.params,
            &self.reason,
            &self.request_id,
            &self.signer_id,
            self.timestamp,
        ))
    }
}

impl SignedConfirm {
    /// The exact string this confirm's signature covers. See
    /// [`SignedRequest::canonical_message`].
    pub fn canonical_message(&self) -> String {
        canonical_json(&confirm_preimage(
            &self.challenge_id,
            &self.confirmation,
            self.level,
            &self.nonce,
            &self.signer_id,
            self.timestamp,
        ))
    }
}

/// Recursively sort object keys and render compact JSON (`,`/`:`
/// separators, no whitespace) — the canonical form signed and verified by
/// both ends of the protocol.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => out.push_str(&serde_json::to_string(s).unwrap_or_default()),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key.as_str()).unwrap_or_default());
                out.push(':');
                // Key came from map.keys(), so the lookup always succeeds.
                if let Some(v) = map.get(key.as_str()) {
                    write_canonical(v, out);
                }
            }
            out.push('}');
        }
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 32-char lowercase hex nonce (`secrets.token_hex(16)` equivalent).
fn generate_nonce() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// The 9 signed fields of a `config_request` (design.md §3.2).
/// [`canonical_json`] sorts structurally, so insertion order here is a
/// reading aid, not the wire order.
#[allow(clippy::too_many_arguments)]
fn request_preimage(
    command_type: &str,
    level: Level,
    nonce: &str,
    params: &Value,
    reason: &str,
    request_id: &str,
    signer_id: &str,
    timestamp: i64,
) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(
        "command_type".to_string(),
        Value::String(command_type.to_string()),
    );
    map.insert(
        "level".to_string(),
        Value::String(level.as_str().to_string()),
    );
    map.insert("nonce".to_string(), Value::String(nonce.to_string()));
    map.insert("params".to_string(), params.clone());
    map.insert("reason".to_string(), Value::String(reason.to_string()));
    map.insert(
        "request_id".to_string(),
        Value::String(request_id.to_string()),
    );
    map.insert(
        "signer_id".to_string(),
        Value::String(signer_id.to_string()),
    );
    map.insert("timestamp".to_string(), Value::from(timestamp));
    map.insert("v".to_string(), Value::from(ENVELOPE_VERSION));
    Value::Object(map)
}

/// The 7 signed fields of a `config_confirm` (design.md §3.2).
fn confirm_preimage(
    challenge_id: &str,
    confirmation: &str,
    level: Level,
    nonce: &str,
    signer_id: &str,
    timestamp: i64,
) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(
        "challenge_id".to_string(),
        Value::String(challenge_id.to_string()),
    );
    map.insert(
        "confirmation".to_string(),
        Value::String(confirmation.to_string()),
    );
    map.insert(
        "level".to_string(),
        Value::String(level.as_str().to_string()),
    );
    map.insert("nonce".to_string(), Value::String(nonce.to_string()));
    map.insert(
        "signer_id".to_string(),
        Value::String(signer_id.to_string()),
    );
    map.insert("timestamp".to_string(), Value::from(timestamp));
    map.insert("v".to_string(), Value::from(ENVELOPE_VERSION));
    Value::Object(map)
}

fn signature_from_base64(signature_b64: &str) -> Result<Signature, EnvelopeError> {
    let bytes = BASE64.decode(signature_b64)?;
    let len = bytes.len();
    let arr: [u8; 64] = bytes
        .try_into()
        .map_err(|_| EnvelopeError::SignatureLength(len))?;
    Ok(Signature::from_bytes(&arr))
}

/// Sign a `config_request`. `reason` defaults to `""` (never `null`) when
/// `None`.
///
/// `level` is the verb's level. Prefer [`build_request`], which reads it
/// from the registry instead of taking it from a caller — design.md
/// §4.4 step 4. This lower-level entry point exists for the vector
/// generator and for tests that need to sign a level the registry would
/// not choose (a forged-claim case, say).
pub fn sign_request(
    signing_key: &SigningKey,
    signer_id: &str,
    command_type: &str,
    level: Level,
    params: Value,
    reason: Option<&str>,
) -> SignedRequest {
    let nonce = generate_nonce();
    let request_id = uuid::Uuid::new_v4().to_string();
    let timestamp = now_unix();
    let reason = reason.unwrap_or("").to_string();

    let pre_image = request_preimage(
        command_type,
        level,
        &nonce,
        &params,
        &reason,
        &request_id,
        signer_id,
        timestamp,
    );
    let message = canonical_json(&pre_image);
    let signature = signing_key.sign(message.as_bytes());

    SignedRequest {
        command: "config_request".to_string(),
        v: ENVELOPE_VERSION,
        command_type: command_type.to_string(),
        level,
        nonce,
        params,
        reason,
        request_id,
        signer_id: signer_id.to_string(),
        timestamp,
        signature: BASE64.encode(signature.to_bytes()),
        certificate: None,
    }
}

/// Verify a [`SignedRequest`] against its reconstructed 9-field pre-image.
///
/// This proves the signature covers the `level` and `v` the envelope
/// carries. It does **not** decide whether that level is the right one for
/// the verb — that check belongs to the table at each end (design.md §3.3
/// step 4 on the device, §4.4 on the portal).
pub fn verify_request(
    req: &SignedRequest,
    verifying_key: &VerifyingKey,
) -> Result<(), EnvelopeError> {
    let message = req.canonical_message();
    let signature = signature_from_base64(&req.signature)?;
    verifying_key
        .verify(message.as_bytes(), &signature)
        .map_err(|_| EnvelopeError::InvalidSignature)
}

/// Sign a `config_confirm`. `command_type`/`params`/`reason` are carried
/// on the wire as informational fields only — they are not part of the
/// signed pre-image.
#[allow(clippy::too_many_arguments)]
pub fn sign_confirm(
    signing_key: &SigningKey,
    signer_id: &str,
    challenge_id: &str,
    confirmation: &str,
    level: Level,
    command_type: Option<&str>,
    params: Option<Value>,
    reason: Option<&str>,
) -> SignedConfirm {
    let nonce = generate_nonce();
    let timestamp = now_unix();

    let pre_image = confirm_preimage(
        challenge_id,
        confirmation,
        level,
        &nonce,
        signer_id,
        timestamp,
    );
    let message = canonical_json(&pre_image);
    let signature = signing_key.sign(message.as_bytes());

    SignedConfirm {
        command: "config_confirm".to_string(),
        v: ENVELOPE_VERSION,
        challenge_id: challenge_id.to_string(),
        confirmation: confirmation.to_string(),
        level,
        nonce,
        signer_id: signer_id.to_string(),
        timestamp,
        command_type: command_type.map(|s| s.to_string()),
        params,
        reason: reason.map(|s| s.to_string()),
        signature: BASE64.encode(signature.to_bytes()),
        certificate: None,
    }
}

/// Verify a [`SignedConfirm`] against its reconstructed 7-field pre-image.
pub fn verify_confirm(
    confirm: &SignedConfirm,
    verifying_key: &VerifyingKey,
) -> Result<(), EnvelopeError> {
    let message = confirm.canonical_message();
    let signature = signature_from_base64(&confirm.signature)?;
    verifying_key
        .verify(message.as_bytes(), &signature)
        .map_err(|_| EnvelopeError::InvalidSignature)
}

/// Build and sign a `config_request` ready to publish to
/// `commands/config/request`, generating nonce/request_id/timestamp and
/// attaching an optional certificate. Thin wrapper over [`sign_request`]
/// for the relay: `seed` is the signer's raw 32-byte Ed25519 seed (e.g.
/// from [`crate::pairing::decrypt_pair_response`]).
///
/// The `level` is read from [`crate::registry`], never taken from the
/// caller (design.md §4.4 step 4) — which is also why an unknown
/// `command_type` returns `None` here rather than signing something the
/// device has no row for. There is no fallback.
pub fn build_request(
    command_type: &str,
    params: Value,
    reason: Option<&str>,
    signer_id: &str,
    seed: &[u8; 32],
    certificate: Option<&Certificate>,
) -> Option<SignedRequest> {
    let level = registry::lookup(command_type)?.level;
    let signing_key = SigningKey::from_bytes(seed);
    let mut request = sign_request(&signing_key, signer_id, command_type, level, params, reason);
    request.certificate = certificate.cloned();
    Some(request)
}

/// Build and sign a `config_confirm` ready to publish to
/// `commands/config/confirm`, generating nonce/timestamp and attaching an
/// optional certificate. Thin wrapper over [`sign_confirm`].
///
/// `command_type` is required here even though it rides the wire as an
/// informational, unsigned field: it is what the level is looked up from,
/// so the confirm repeats exactly the level its own request carried.
/// `None` for an unknown verb, as in [`build_request`].
#[allow(clippy::too_many_arguments)]
pub fn build_confirm(
    challenge_id: &str,
    confirmation: &str,
    signer_id: &str,
    seed: &[u8; 32],
    command_type: &str,
    params: Option<Value>,
    reason: Option<&str>,
    certificate: Option<&Certificate>,
) -> Option<SignedConfirm> {
    let level = registry::lookup(command_type)?.level;
    let signing_key = SigningKey::from_bytes(seed);
    let mut confirm = sign_confirm(
        &signing_key,
        signer_id,
        challenge_id,
        confirmation,
        level,
        Some(command_type),
        params,
        reason,
    );
    confirm.certificate = certificate.cloned();
    Some(confirm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;
    use serde_json::json;

    fn keypair() -> (SigningKey, VerifyingKey) {
        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        (signing_key, verifying_key)
    }

    // --- canonical_json ---

    #[test]
    fn canonical_json_sorts_keys_recursively_and_is_compact() {
        let value = json!({
            "b": 1,
            "a": { "z": true, "y": "hello world" },
            "c": [3, 1, 2]
        });
        assert_eq!(
            canonical_json(&value),
            r#"{"a":{"y":"hello world","z":true},"b":1,"c":[3,1,2]}"#
        );
    }

    #[test]
    fn canonical_json_handles_strings_ints_and_nesting() {
        let value = json!({
            "signer_id": "admin",
            "timestamp": 1736775060,
            "params": { "lines": [], "reason": null }
        });
        assert_eq!(
            canonical_json(&value),
            r#"{"params":{"lines":[],"reason":null},"signer_id":"admin","timestamp":1736775060}"#
        );
    }

    /// design.md §3.2's first worked example, byte for byte. `level` sorts
    /// between `command_type` and `nonce`; `v` sorts last, after
    /// `timestamp`.
    #[test]
    fn request_preimage_matches_hand_written_canonical_string() {
        let params = json!({"post_action": "reboot"});
        let pre = request_preimage(
            "factory_reset",
            Level::Destructive,
            "0123456789abcdef0123456789abcdef",
            &params,
            "",
            "req-1234",
            "admin",
            1736775060,
        );
        let expected = r#"{"command_type":"factory_reset","level":"destructive","nonce":"0123456789abcdef0123456789abcdef","params":{"post_action":"reboot"},"reason":"","request_id":"req-1234","signer_id":"admin","timestamp":1736775060,"v":2}"#;
        assert_eq!(canonical_json(&pre), expected);
    }

    /// design.md §3.2's second worked example, byte for byte. `level` sorts
    /// between `confirmation` and `nonce`.
    #[test]
    fn confirm_preimage_matches_hand_written_canonical_string() {
        let pre = confirm_preimage(
            "chal-5678",
            "APPROVED",
            Level::Destructive,
            "fedcba9876543210fedcba9876543210",
            "admin",
            1736775090,
        );
        let expected = r#"{"challenge_id":"chal-5678","confirmation":"APPROVED","level":"destructive","nonce":"fedcba9876543210fedcba9876543210","signer_id":"admin","timestamp":1736775090,"v":2}"#;
        assert_eq!(canonical_json(&pre), expected);
    }

    // --- command/algorithm exclusion ---

    #[test]
    fn request_preimage_excludes_command_and_algorithm() {
        let pre = request_preimage(
            "restart_application",
            Level::Destructive,
            "n",
            &json!({}),
            "",
            "r",
            "s",
            1,
        );
        let obj = pre.as_object().expect("preimage is an object");
        assert_eq!(obj.len(), 9, "exactly the 9 documented fields");
        assert!(!obj.contains_key("command"));
        assert!(!obj.contains_key("algorithm"));
        assert_eq!(obj["v"], json!(2));
        assert_eq!(obj["level"], json!("destructive"));
    }

    #[test]
    fn confirm_preimage_excludes_command_and_algorithm() {
        let pre = confirm_preimage("c", "APPROVED", Level::Manage, "n", "s", 1);
        let obj = pre.as_object().expect("preimage is an object");
        assert_eq!(obj.len(), 7, "exactly the 7 documented fields");
        assert!(!obj.contains_key("command"));
        assert!(!obj.contains_key("algorithm"));
        assert_eq!(obj["v"], json!(2));
        assert_eq!(obj["level"], json!("manage"));
    }

    // --- sign / verify roundtrip ---

    #[test]
    fn request_sign_verify_roundtrip() {
        let (signing_key, verifying_key) = keypair();
        let req = sign_request(
            &signing_key,
            "admin",
            "restart_application",
            Level::Destructive,
            json!({}),
            None,
        );

        assert_eq!(req.command, "config_request");
        assert_eq!(req.v, ENVELOPE_VERSION);
        assert_eq!(req.level, Level::Destructive);
        assert_eq!(
            req.reason, "",
            "reason defaults to empty string, never null"
        );
        assert_eq!(req.nonce.len(), 32);
        assert!(req
            .nonce
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));

        verify_request(&req, &verifying_key).expect("valid signature must verify");

        let mut tampered = req.clone();
        tampered.params = json!({"tampered": true});
        assert!(verify_request(&tampered, &verifying_key).is_err());
    }

    #[test]
    fn confirm_sign_verify_roundtrip() {
        let (signing_key, verifying_key) = keypair();
        let confirm = sign_confirm(
            &signing_key,
            "admin",
            "chal-1",
            "APPROVED",
            Level::Destructive,
            Some("restart_application"),
            Some(json!({})),
            Some("Rebooted from the Portal by admin"),
        );

        assert_eq!(confirm.command, "config_confirm");
        assert_eq!(confirm.v, ENVELOPE_VERSION);
        verify_confirm(&confirm, &verifying_key).expect("valid signature must verify");

        let mut tampered = confirm.clone();
        tampered.confirmation = "REJECTED".to_string();
        assert!(verify_confirm(&tampered, &verifying_key).is_err());
    }

    #[test]
    fn wrong_key_fails_verification() {
        let (signing_key, _) = keypair();
        let (_, other_verifying_key) = keypair();
        let req = sign_request(
            &signing_key,
            "admin",
            "get_status",
            Level::Read,
            json!({}),
            None,
        );
        assert!(verify_request(&req, &other_verifying_key).is_err());
    }

    // --- build_request / build_confirm ---

    #[test]
    fn build_request_generates_fresh_ids_and_verifies() {
        let seed = [11u8; 32];
        let verifying_key = SigningKey::from_bytes(&seed).verifying_key();

        let req = build_request(
            "restart_application",
            json!({}),
            Some("Rebooted from the Portal by admin"),
            "admin",
            &seed,
            None,
        )
        .expect("restart_application is in the registry");

        assert_eq!(req.command, "config_request");
        assert_eq!(req.command_type, "restart_application");
        assert!(!req.request_id.is_empty());
        verify_request(&req, &verifying_key).expect("build_request output must verify");
    }

    /// The level is the registry's, not the caller's (design.md §4.4 step
    /// 4): `build_request` takes no level argument at all, and an unknown
    /// verb gets `None` rather than a signed envelope the device has no
    /// row for.
    #[test]
    fn build_request_takes_its_level_from_the_registry() {
        let seed = [11u8; 32];
        for (command_type, expected) in [
            ("get_sticker_info", Level::Read),
            ("silence_buzzer", Level::Operate),
            ("set_threshold", Level::Manage),
            ("add_signer", Level::Admin),
            ("factory_reset", Level::Destructive),
        ] {
            let req = build_request(command_type, json!({}), None, "admin", &seed, None)
                .unwrap_or_else(|| panic!("{command_type} must be in the registry"));
            assert_eq!(req.level, expected, "{command_type}");
        }
        assert!(
            build_request("no_such_verb", json!({}), None, "admin", &seed, None).is_none(),
            "an unknown verb must not produce a signed envelope"
        );
    }

    #[test]
    fn build_confirm_attaches_certificate_and_verifies() {
        use crate::certificate::{Certificate, CertificatePayload};

        let seed = [12u8; 32];
        let verifying_key = SigningKey::from_bytes(&seed).verifying_key();

        let certificate = Certificate {
            payload: CertificatePayload {
                signer_id: "admin".to_string(),
                full_name: "Admin User".to_string(),
                role: "admin".to_string(),
                public_key_ed25519: hex::encode(verifying_key.to_bytes()),
                permissions: vec!["restart_application".to_string()],
                issued_at: "2026-01-01T00:00:00Z".to_string(),
                expires_at: "2027-01-01T00:00:00Z".to_string(),
                issuer: "device_ca".to_string(),
            },
            certificate_signature: "unrelated-to-command-signing".to_string(),
        };

        let confirm = build_confirm(
            "chal-1",
            "APPROVED",
            "admin",
            &seed,
            "restart_application",
            Some(json!({})),
            None,
            Some(&certificate),
        )
        .expect("restart_application is in the registry");

        assert_eq!(confirm.command, "config_confirm");
        assert_eq!(
            confirm.level,
            Level::Destructive,
            "the confirm repeats its request's level"
        );
        assert_eq!(
            confirm.certificate.as_ref().unwrap().payload.signer_id,
            "admin"
        );
        verify_confirm(&confirm, &verifying_key).expect("build_confirm output must verify");
    }

    // --- float canonicalization parity (see module doc "RESOLVED — float parity") ---

    #[test]
    fn canonical_json_float_21_5() {
        assert_eq!(canonical_json(&json!({"v": 21.5})), r#"{"v":21.5}"#);
    }

    #[test]
    fn canonical_json_float_0_1() {
        assert_eq!(canonical_json(&json!({"v": 0.1})), r#"{"v":0.1}"#);
    }

    #[test]
    fn canonical_json_float_negative_18_2() {
        assert_eq!(canonical_json(&json!({"v": -18.2})), r#"{"v":-18.2}"#);
    }

    #[test]
    fn canonical_json_float_2_675() {
        assert_eq!(canonical_json(&json!({"v": 2.675})), r#"{"v":2.675}"#);
    }

    #[test]
    fn canonical_json_float_1e16() {
        // ryu emits an explicit '+' on positive exponents.
        assert_eq!(canonical_json(&json!({"v": 1e16})), r#"{"v":1e+16}"#);
    }

    #[test]
    fn canonical_json_float_1e_minus_7() {
        assert_eq!(canonical_json(&json!({"v": 1e-7})), r#"{"v":1e-7}"#);
    }

    /// A signed request whose `params` carry a float round-trips through
    /// sign/verify — proving the float text is stable under the exact
    /// pre-image construction the agent also builds against (see the
    /// module doc's "RESOLVED — float parity" wire-echo argument).
    #[test]
    fn request_with_float_param_signs_and_verifies() {
        let (signing_key, verifying_key) = keypair();
        let req = sign_request(
            &signing_key,
            "admin",
            "set_lorawan_field_threshold",
            Level::Manage,
            json!({"dev_eui": "aabbccddeeff0011", "field": "temperature", "warning_high": 21.5}),
            None,
        );
        verify_request(&req, &verifying_key).expect("float params must not break verification");
    }
}
