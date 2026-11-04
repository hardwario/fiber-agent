//! Regenerates `crates/fiber-protocol/vectors/*.json` — the one fixture set
//! three implementations are tested against (plan/portal/mqtt/design.md
//! §10: `fiber-protocol`, `control-core`'s verifier, and the mock).
//!
//! ```sh
//! cargo run -p fiber-protocol --example gen_vectors
//! ```
//!
//! Every case is deterministic: a fixed signing seed, fixed nonces,
//! request ids and timestamps. Re-running it on an unchanged tree must
//! produce a byte-identical file, and the tests in `vectors.rs` fail if the
//! checked-in file and the code disagree — so a wire-shape change shows up
//! as a fixture diff in review rather than as a silent re-derivation.
//!
//! The first request case and the last confirm case are design.md §3.2's
//! two worked examples, verbatim, so the document itself pins two of the
//! canonical strings.

use ed25519_dalek::{Signer, SigningKey};
use fiber_protocol::certificate::{
    canonical_certificate_message, sign_certificate, CertificatePayload,
};
use fiber_protocol::envelope::ENVELOPE_VERSION;
use fiber_protocol::registry::{self, Level};
use fiber_protocol::{SignedConfirm, SignedRequest};
use serde_json::{json, Map, Value};

/// The fixed test seed (design.md §10). Also the CA seed the certificate
/// vector is signed under, which is why `certificate.json` and
/// `envelope_v2.json` quote the same public key.
const SEED: [u8; 32] = [7u8; 32];

fn base64_sign(key: &SigningKey, message: &str) -> String {
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine as _;
    BASE64.encode(key.sign(message.as_bytes()).to_bytes())
}

#[allow(clippy::too_many_arguments)]
fn request(
    key: &SigningKey,
    name: &str,
    note: &str,
    command_type: &str,
    level: Level,
    nonce: &str,
    params: Value,
    reason: &str,
    request_id: &str,
    signer_id: &str,
    timestamp: i64,
) -> Value {
    let mut req = SignedRequest {
        command: "config_request".to_string(),
        v: ENVELOPE_VERSION,
        command_type: command_type.to_string(),
        level,
        nonce: nonce.to_string(),
        params,
        reason: reason.to_string(),
        request_id: request_id.to_string(),
        signer_id: signer_id.to_string(),
        timestamp,
        signature: String::new(),
        certificate: None,
    };
    let canonical = req.canonical_message();
    req.signature = base64_sign(key, &canonical);
    json!({ "name": name, "note": note, "envelope": req, "canonical": canonical })
}

#[allow(clippy::too_many_arguments)]
fn confirm(
    key: &SigningKey,
    name: &str,
    note: &str,
    challenge_id: &str,
    confirmation: &str,
    level: Level,
    nonce: &str,
    signer_id: &str,
    timestamp: i64,
    command_type: &str,
) -> Value {
    let mut c = SignedConfirm {
        command: "config_confirm".to_string(),
        v: ENVELOPE_VERSION,
        challenge_id: challenge_id.to_string(),
        confirmation: confirmation.to_string(),
        level,
        nonce: nonce.to_string(),
        signer_id: signer_id.to_string(),
        timestamp,
        command_type: Some(command_type.to_string()),
        params: None,
        reason: None,
        signature: String::new(),
        certificate: None,
    };
    let canonical = c.canonical_message();
    c.signature = base64_sign(key, &canonical);
    json!({ "name": name, "note": note, "envelope": c, "canonical": canonical })
}

fn envelope_vectors(key: &SigningKey) -> Value {
    let pubkey = hex::encode(key.verifying_key().to_bytes());

    let requests = vec![
        request(
            key,
            "destructive_factory_reset",
            "design.md §3.2's first worked example, byte for byte",
            "factory_reset",
            Level::Destructive,
            "0123456789abcdef0123456789abcdef",
            json!({"post_action": "reboot"}),
            "",
            "req-1234",
            "admin",
            1736775060,
        ),
        request(
            key,
            "read_empty_params",
            "a read verb with no params — `{}`, never null, and `reason` \"\"",
            "get_sticker_info",
            Level::Read,
            "1111111111111111aaaaaaaaaaaaaaaa",
            json!({}),
            "",
            "req-read-1",
            "viewer",
            1736775061,
        ),
        request(
            key,
            "operate_silence_buzzer",
            "the verb that had no permission at all in v1 (design.md §4.2 ★)",
            "silence_buzzer",
            Level::Operate,
            "2222222222222222bbbbbbbbbbbbbbbb",
            json!({"line": 1}),
            "alarm acknowledged by nurse on shift",
            "req-op-1",
            "manager",
            1736775062,
        ),
        request(
            key,
            "manage_set_threshold",
            "the common manage case",
            "set_threshold",
            Level::Manage,
            "3333333333333333cccccccccccccccc",
            json!({"line": 2, "warning_high": 8.0, "alarm_high": 10.0}),
            "",
            "req-man-1",
            "manager",
            1736775063,
        ),
        request(
            key,
            "admin_add_signer",
            "level admin",
            "add_signer",
            Level::Admin,
            "4444444444444444dddddddddddddddd",
            json!({"signer_id": "nurse01", "role": "viewer"}),
            "",
            "req-adm-1",
            "admin",
            1736775064,
        ),
        request(
            key,
            "nested_params",
            "recursive key sorting: nested objects inside an array inside an \
             object, every level out of order at the source",
            "set_lorawan_sensor_config",
            Level::Manage,
            "5555555555555555eeeeeeeeeeeeeeee",
            json!({
                "zulu": 1,
                "alpha": {"z": [{"n": 2, "m": 1}, {"b": false, "a": true}], "y": {"x": null}},
                "mike": [3, 1, 2]
            }),
            "",
            "req-nest-1",
            "admin",
            1736775065,
        ),
        request(
            key,
            "non_ascii_reason",
            "status.md: Python's json.dumps escapes non-ASCII and serde_json \
             does not — the one known way the three implementations could \
             disagree, pinned rather than assumed (design.md §10)",
            "set_device_label",
            Level::Manage,
            "6666666666666666ffffffffffffffff",
            json!({"label": "Lednice č. 3 — sklad"}),
            "Přejmenováno správcem: teplotní čidlo \"A\" → \"B\"",
            "req-utf8-1",
            "admin",
            1736775066,
        ),
        // The six float pins, moved here out of envelope.rs's own tests
        // (design.md §3.2's last bullet).
        float_pin(key, "float_21_5", json!(21.5), 1736775070),
        float_pin(key, "float_0_1", json!(0.1), 1736775071),
        float_pin(key, "float_negative_18_2", json!(-18.2), 1736775072),
        float_pin(key, "float_2_675", json!(2.675), 1736775073),
        float_pin(key, "float_1e16", json!(1e16), 1736775074),
        float_pin(key, "float_1e_minus_7", json!(1e-7), 1736775075),
    ];

    let confirms = vec![
        confirm(
            key,
            "manage_confirm",
            "the confirm half of manage_set_threshold",
            "chal-manage-1",
            "APPROVED",
            Level::Manage,
            "7777777777777777aaaaaaaaaaaaaaaa",
            "manager",
            1736775080,
            "set_threshold",
        ),
        confirm(
            key,
            "admin_confirm_rejected",
            "REJECTED is signed the same way APPROVED is",
            "chal-admin-1",
            "REJECTED",
            Level::Admin,
            "8888888888888888bbbbbbbbbbbbbbbb",
            "admin",
            1736775081,
            "add_signer",
        ),
        confirm(
            key,
            "destructive_confirm",
            "design.md §3.2's second worked example, byte for byte",
            "chal-5678",
            "APPROVED",
            Level::Destructive,
            "fedcba9876543210fedcba9876543210",
            "admin",
            1736775090,
            "factory_reset",
        ),
    ];

    json!({
        "_source": "plan/portal/mqtt/design.md §3.2, §10 — regenerate with \
                    `cargo run -p fiber-protocol --example gen_vectors`",
        "_consumers": [
            "fiber-protocol (crates/fiber-protocol/tests/vectors.rs)",
            "control-core (the device verifier's canonical-string test)",
            "dev/mock/mock_publisher.py"
        ],
        "envelope_version": ENVELOPE_VERSION,
        "signing_key": { "seed_hex": hex::encode(SEED), "public_key_ed25519": pubkey },
        "requests": requests,
        "confirms": confirms
    })
}

fn float_pin(key: &SigningKey, name: &str, value: Value, timestamp: i64) -> Value {
    request(
        key,
        name,
        "float canonicalization pin — the number text this crate signs is \
         the number text that goes on the wire, and the device's \
         arbitrary_precision verifier echoes it back unchanged",
        "set_lorawan_field_threshold",
        Level::Manage,
        "99999999999999990000000000000000",
        json!({"dev_eui": "aabbccddeeff0011", "field": "temperature", "warning_high": value}),
        "",
        "req-float",
        "manager",
        timestamp,
    )
}

fn catalogue_vectors() -> Value {
    let rows: Vec<Value> = registry::all()
        .iter()
        .map(|spec| {
            json!({
                "command_type": spec.command_type,
                "permission": spec.permission,
                "level": spec.level,
                "single_shot": spec.level.is_single_shot(),
            })
        })
        .collect();
    json!({
        "_source": "plan/portal/mqtt/design.md §4.2, §10 — regenerate with \
                    `cargo run -p fiber-protocol --example gen_vectors`",
        "_note": "The portal's rows. The device's table is a superset: \
                  get_status, get_info, set_screen, flush_storage and \
                  beacon-detect have device arms but no portal publisher, \
                  so they carry no row here (design.md §4.2's device-only \
                  entries).",
        "level_permissions": registry::Level::ALL
            .iter()
            .map(|l| (l.as_str().to_string(), Value::String(l.permission())))
            .collect::<Map<String, Value>>(),
        "commands": rows
    })
}

fn certificate_vectors(key: &SigningKey) -> Value {
    // Unchanged from certificate.rs's own fixture, so the review can see
    // the certificate did not move (design.md §3.4, §10).
    let payload = CertificatePayload {
        signer_id: "admin@fiber.local".to_string(),
        full_name: "Admin User".to_string(),
        role: "admin".to_string(),
        public_key_ed25519: "aa".repeat(32),
        permissions: vec!["get_status".to_string(), "set_threshold".to_string()],
        issued_at: "2026-01-01T00:00:00Z".to_string(),
        expires_at: "2027-01-01T00:00:00Z".to_string(),
        issuer: "device_ca".to_string(),
    };
    json!({
        "_source": "plan/portal/mqtt/design.md §3.4, §10 — the eight-field \
                    canonical form is unchanged in v2; this file exists so \
                    the review can see that it did not move",
        "ca_public_key_ed25519": hex::encode(key.verifying_key().to_bytes()),
        "payload": payload,
        "canonical": canonical_certificate_message(&payload),
        "certificate_signature": sign_certificate(&SEED, &payload),
    })
}

fn write(path: &str, value: &Value) {
    let mut text = serde_json::to_string_pretty(value).expect("vectors serialize");
    text.push('\n');
    std::fs::write(path, text).unwrap_or_else(|e| panic!("writing {path}: {e}"));
    println!("wrote {path}");
}

fn main() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/vectors");
    let key = SigningKey::from_bytes(&SEED);
    write(&format!("{dir}/envelope_v2.json"), &envelope_vectors(&key));
    write(&format!("{dir}/catalogue.json"), &catalogue_vectors());
    write(
        &format!("{dir}/certificate.json"),
        &certificate_vectors(&key),
    );
}
