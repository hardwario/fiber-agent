//! P1's acceptance tests (plan/portal/mqtt/design.md §14 P1).
//!
//! Two halves:
//!
//! 1. **The fixture.** `fiber-protocol/vectors/envelope_v2.json` is the one
//!    file three implementations are tested against (design.md §10). Before
//!    v2, `control-core` had **no** test asserting its canonical output at
//!    all — byte-exactness with the portal was asserted in prose. The tests
//!    below feed the fixture's wire JSON through the real parser and
//!    `build_canonical_request_message` and compare the string the device
//!    would sign against. The file is read by relative path on purpose:
//!    this crate takes no dependency on `fiber-protocol`, so de-vendoring
//!    the contract stays the separate decision monolith.md §11.2 records.
//!
//! 2. **The behaviour.** A `read` request executes and answers with no
//!    challenge; a `manage` request mints one; a manager signing a
//!    `destructive` verb is refused on the level, not on the permission it
//!    does hold; the eight ex-`PLAIN` verbs work signed; and a v1-shaped
//!    request is refused outright — P4 deleted `accept_v1_envelope`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};

use super::level::{Envelope, Level};
use super::manager::{AuthError, AuthorizationManager, ENVELOPE_VERSION};
use crate::libs::crypto::{
    CARegistry, CertificateAuthority, NonceTracker, SignatureVerifier, UserCertificate,
};
use crate::libs::mqtt::messages::{MqttCommand, MqttMessage};
use crate::libs::mqtt::publisher::MqttPublisher;
use crate::libs::mqtt::subscriber::MqttSubscriber;

// ---------------------------------------------------------------- fixtures

fn vectors_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fiber-protocol/vectors")
        .join(name)
}

fn load_vectors(name: &str) -> Value {
    let path = vectors_path(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "reading {}: {e} — the vectors are checked in next to the portal's \
             contract crate (design.md §10)",
            path.display()
        )
    });
    serde_json::from_str(&text).expect("the vectors are JSON")
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// A manager with its own CA, its own nonce store and its own audit db, so
/// tests cannot replay each other's nonces.
struct Hub {
    manager: AuthorizationManager,
    ca_seed: [u8; 32],
    /// The audit database, so a test can read the row the manager wrote
    /// rather than trusting the `Result` it returned.
    audit_db: PathBuf,
    _dir: tempfile::TempDir,
}

impl Hub {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let ca_seed = [42u8; 32];
        let ca_public = hex::encode(SigningKey::from_bytes(&ca_seed).verifying_key().to_bytes());

        let mut registry = CARegistry::load_from_file(&dir.path().join("no-such-registry.yaml"))
            .expect("an absent registry file loads empty");
        registry.add_ca(CertificateAuthority {
            ca_id: "device_ca".to_string(),
            ca_public_key_ed25519: ca_public,
            trusted_since: "2026-01-01T00:00:00Z".to_string(),
            enabled: true,
            description: None,
        });

        let nonce_tracker = Arc::new(Mutex::new(
            NonceTracker::new(&dir.path().join("nonces.db"), 600, 100).expect("nonce tracker"),
        ));
        let verifier = Arc::new(SignatureVerifier::new(
            Arc::new(Mutex::new(registry)),
            nonce_tracker,
            60,
        ));

        let audit_db = dir.path().join("audit.db");
        Self {
            manager: AuthorizationManager::new(verifier, &audit_db, 300, 10),
            ca_seed,
            audit_db,
            _dir: dir,
        }
    }

    /// `(verification_status, applied, error_msg)` of the one audit row for
    /// `challenge_id`.
    fn audit_row(&self, challenge_id: &str) -> (String, i64, Option<String>) {
        let conn = rusqlite::Connection::open(&self.audit_db).expect("audit db");
        conn.query_row(
            "SELECT verification_status, applied, error_msg FROM config_changes WHERE challenge_id = ?",
            rusqlite::params![challenge_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("one audit row")
    }

    /// A signer holding exactly `permissions`, with a certificate this
    /// hub's CA signed.
    fn signer(&self, signer_id: &str, role: &str, permissions: &[&str]) -> Signer_ {
        let seed = {
            let mut seed = [0u8; 32];
            for (i, b) in signer_id.bytes().enumerate().take(32) {
                seed[i] = b;
            }
            seed[31] = 1; // never all-zero
            seed
        };
        let key = SigningKey::from_bytes(&seed);
        let mut certificate = UserCertificate {
            signer_id: signer_id.to_string(),
            full_name: format!("{signer_id} (test)"),
            role: role.to_string(),
            public_key_ed25519: hex::encode(key.verifying_key().to_bytes()),
            permissions: permissions.iter().map(|p| p.to_string()).collect(),
            issued_at: "2026-01-01T00:00:00Z".to_string(),
            expires_at: "2099-01-01T00:00:00Z".to_string(),
            issuer: "device_ca".to_string(),
            certificate_signature: String::new(),
        };
        let ca = SigningKey::from_bytes(&self.ca_seed);
        certificate.certificate_signature = BASE64.encode(
            ca.sign(certificate.build_canonical_message().as_bytes())
                .to_bytes(),
        );
        Signer_ { key, certificate }
    }

    /// The manager-shaped signer of design.md §4.3: every permission a
    /// manager's certificate carries, and `level:` up to `manage`.
    fn manager_signer(&self) -> Signer_ {
        self.signer(
            "manager@fiber.local",
            "manager",
            &[
                "set_threshold",
                "set_lorawan_sensor_config",
                "get_status",
                "get_sensor_config",
                "get_interval",
                "silence_buzzer",
                "beacon.manage",
                "level:read",
                "level:operate",
                "level:manage",
            ],
        )
    }

    fn admin_signer(&self) -> Signer_ {
        self.signer(
            "admin@fiber.local",
            "admin",
            &[
                "set_threshold",
                "set_lorawan_sensor_config",
                "factory_reset",
                "get_status",
                "get_sensor_config",
                "get_interval",
                "silence_buzzer",
                "beacon.manage",
                "level:read",
                "level:operate",
                "level:manage",
                "level:admin",
                "level:destructive",
            ],
        )
    }

    /// An admin who also holds the two 2026-09-16 permissions: `setup.manage` (GitLab #58,
    /// BLE twin FB17) and `export.manage` (#59, FB18).
    ///
    /// A fixture of its own rather than two more strings on [`Hub::admin_signer`], because the
    /// tests below need a signer that has every *level* those verbs claim and **lacks** their
    /// permission — otherwise "refused on the permission" cannot be told apart from "refused on
    /// the level", which is the distinction `a_manager_signing_a_destructive_verb_is_refused_on_
    /// the_level` exists to draw. `admin_signer` is that signer, unchanged.
    fn commissioning_signer(&self) -> Signer_ {
        self.signer(
            "commissioner@fiber.local",
            "admin",
            &[
                "setup.manage",
                "export.manage",
                "get_status",
                "level:read",
                "level:operate",
                "level:manage",
                "level:admin",
            ],
        )
    }
}

struct Signer_ {
    key: SigningKey,
    certificate: UserCertificate,
}

/// Build the wire JSON of a signed `config_request`, signing whatever
/// pre-image `envelope` implies — so a test can also forge a level the
/// table disagrees with.
fn request_json(
    signer: &Signer_,
    envelope: Envelope,
    command_type: &str,
    params: Value,
    nonce: &str,
) -> Value {
    let request_id = format!("req-{nonce}");
    let timestamp = now();
    let mut pre = serde_json::Map::new();
    pre.insert("command_type".into(), json!(command_type));
    pre.insert("nonce".into(), json!(nonce));
    pre.insert("params".into(), params.clone());
    pre.insert("reason".into(), json!(""));
    pre.insert("request_id".into(), json!(request_id));
    pre.insert("signer_id".into(), json!(signer.certificate.signer_id));
    pre.insert("timestamp".into(), json!(timestamp));
    if let Envelope::V2(level) = envelope {
        pre.insert("level".into(), json!(level.as_str()));
        pre.insert("v".into(), json!(ENVELOPE_VERSION));
    }
    // serde_json::Map preserves insertion order unless `preserve_order` is
    // on; the device sorts with a BTreeMap, so sort here explicitly rather
    // than relying on either.
    let sorted: std::collections::BTreeMap<String, Value> = pre.clone().into_iter().collect();
    let canonical = serde_json::to_string(&sorted).unwrap();
    let signature = BASE64.encode(signer.key.sign(canonical.as_bytes()).to_bytes());

    let mut wire = json!({
        "command": "config_request",
        "command_type": command_type,
        "nonce": nonce,
        "params": params,
        "reason": "",
        "request_id": request_id,
        "signer_id": signer.certificate.signer_id,
        "timestamp": timestamp,
        "signature": signature,
        "certificate": signer.certificate,
    });
    if let Envelope::V2(level) = envelope {
        wire["v"] = json!(ENVELOPE_VERSION);
        wire["level"] = json!(level.as_str());
    }
    wire
}

/// Parse through the real subscriber, then hand it to the manager — the
/// whole inbound path, not just the half under test.
fn submit(
    hub: &Hub,
    subscriber: &mut MqttSubscriber,
    wire: &Value,
) -> Result<(MqttMessage, Option<MqttCommand>), String> {
    let payload = serde_json::to_vec(wire).unwrap();
    let parsed = subscriber.parse_command("proximos/host/commands/config/request", &payload)?;
    let MqttCommand::ConfigRequest {
        envelope,
        request_id,
        command_type,
        params,
        reason,
        signer_id,
        signature,
        timestamp,
        nonce,
        certificate,
    } = parsed
    else {
        panic!("expected a ConfigRequest");
    };
    hub.manager
        .process_config_request(
            envelope,
            request_id,
            command_type,
            params,
            reason,
            signer_id,
            signature,
            timestamp,
            nonce,
            &certificate,
        )
        .map_err(|e| e.to_string())
}

fn subscriber() -> MqttSubscriber {
    MqttSubscriber::new(1000, false)
}

// ------------------------------------------------- 1. the fixture strings

/// P1's first acceptance criterion: the fixture's canonical string matches
/// `build_canonical_request_message` for every case. The device builds it
/// from the wire JSON it parsed, exactly as it does in production.
#[test]
fn every_request_vector_rebuilds_its_canonical_string() {
    let hub = Hub::new();
    let vectors = load_vectors("envelope_v2.json");
    let cases = vectors["requests"].as_array().expect("requests");
    assert!(cases.len() >= 12, "design.md §10 asks for ~12 cases");

    for case in cases {
        let name = case["name"].as_str().expect("name");
        let wire = &case["envelope"];
        let level = Level::parse(wire["level"].as_str().expect("level")).expect("a known level");

        let built = hub.manager.build_canonical_request_message(
            Envelope::V2(level),
            wire["request_id"].as_str().unwrap(),
            wire["command_type"].as_str().unwrap(),
            &wire["params"],
            &Some(wire["reason"].as_str().unwrap().to_string()),
            wire["signer_id"].as_str().unwrap(),
            wire["timestamp"].as_i64().unwrap(),
            wire["nonce"].as_str().unwrap(),
        );
        assert_eq!(
            built,
            case["canonical"].as_str().unwrap(),
            "{name}: the device's canonical string differs from the portal's"
        );
    }
}

#[test]
fn every_confirm_vector_rebuilds_its_canonical_string() {
    let hub = Hub::new();
    let vectors = load_vectors("envelope_v2.json");
    for case in vectors["confirms"].as_array().expect("confirms") {
        let name = case["name"].as_str().expect("name");
        let wire = &case["envelope"];
        let level = Level::parse(wire["level"].as_str().unwrap()).expect("a known level");

        let built = hub.manager.build_canonical_confirm_message(
            Envelope::V2(level),
            wire["challenge_id"].as_str().unwrap(),
            wire["confirmation"].as_str().unwrap(),
            wire["signer_id"].as_str().unwrap(),
            wire["timestamp"].as_i64().unwrap(),
            wire["nonce"].as_str().unwrap(),
        );
        assert_eq!(built, case["canonical"].as_str().unwrap(), "{name}");
    }
}

/// The fixture's signatures verify against the device's own rebuilt string
/// — the property that actually matters, and the one prose used to assert.
#[test]
fn every_request_vector_verifies_against_the_devices_own_string() {
    use ed25519_dalek::{Verifier, VerifyingKey};

    let hub = Hub::new();
    let vectors = load_vectors("envelope_v2.json");
    let key_bytes: [u8; 32] = hex::decode(
        vectors["signing_key"]["public_key_ed25519"]
            .as_str()
            .unwrap(),
    )
    .unwrap()
    .try_into()
    .unwrap();
    let key = VerifyingKey::from_bytes(&key_bytes).unwrap();

    for case in vectors["requests"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let wire = &case["envelope"];
        let level = Level::parse(wire["level"].as_str().unwrap()).unwrap();
        let built = hub.manager.build_canonical_request_message(
            Envelope::V2(level),
            wire["request_id"].as_str().unwrap(),
            wire["command_type"].as_str().unwrap(),
            &wire["params"],
            &Some(wire["reason"].as_str().unwrap().to_string()),
            wire["signer_id"].as_str().unwrap(),
            wire["timestamp"].as_i64().unwrap(),
            wire["nonce"].as_str().unwrap(),
        );
        let signature_bytes: [u8; 64] = BASE64
            .decode(wire["signature"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        key.verify(
            built.as_bytes(),
            &ed25519_dalek::Signature::from_bytes(&signature_bytes),
        )
        .unwrap_or_else(|e| panic!("{name}: the portal's signature must verify here: {e}"));
    }
}

/// The two tables agree: every row of `catalogue.json` resolves here to the
/// same permission and the same level. This is the assertion that makes the
/// fixture load-bearing rather than documentation (design.md §10).
#[test]
fn the_device_table_agrees_with_the_catalogue() {
    let hub = Hub::new();
    let catalogue = load_vectors("catalogue.json");
    let rows = catalogue["commands"].as_array().expect("commands");
    assert_eq!(
        rows.len(),
        63,
        "the portal's 63 rows — 52 plus node-add, node-remove, node-move, setup-complete (#58), \
         export-destination-set (#59), radio-set-mode (R7), cluster-client-add/remove \
         (cluster.md §7), northbridge-update (northbridge_update.md §3.4) and southbridge-update \
         (southbridge_update.md §3.7), set_sample_interval (config_sync.md Q19), cluster-client-gateway \
         (#164), less the retired \
         set_lorawan_cluster (cluster.md §3)"
    );

    for row in rows {
        let command_type = row["command_type"].as_str().unwrap();
        let (permission, level) = hub
            .manager
            .command_type_to_spec(command_type)
            .unwrap_or_else(|e| panic!("{command_type} has no arm on the device: {e}"));
        assert_eq!(
            permission,
            row["permission"].as_str().unwrap(),
            "{command_type}"
        );
        assert_eq!(
            level.as_str(),
            row["level"].as_str().unwrap(),
            "{command_type}"
        );
        assert_eq!(
            level.is_single_shot(),
            row["single_shot"].as_bool().unwrap(),
            "{command_type}"
        );
    }
}

/// The device's table is a superset: the five arms with no portal publisher
/// (design.md §4.2's "device-only" entries) are still reachable.
#[test]
fn the_device_only_arms_have_levels_too() {
    let hub = Hub::new();
    for (command_type, expected) in [
        ("get_status", Level::Read),
        ("get_info", Level::Read),
        ("set_screen", Level::Operate),
        ("beacon-detect", Level::Operate),
        ("flush_storage", Level::Destructive),
    ] {
        let (_, level) = hub.manager.command_type_to_spec(command_type).unwrap();
        assert_eq!(level, expected, "{command_type}");
    }
}

// ------------------------------------------------------- 2. the behaviour

/// A `read` request is executed on the spot and answered on
/// `config/response` — no challenge, no slot held (design.md §5.1).
#[test]
fn a_read_request_answers_with_a_response_and_no_challenge() {
    let hub = Hub::new();
    let signer = hub.manager_signer();
    let mut sub = subscriber();

    let wire = request_json(
        &signer,
        Envelope::V2(Level::Read),
        "get_sticker_info",
        json!({"dev_eui": "aabbccddeeff0011"}),
        "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1",
    );
    let (message, command) = submit(&hub, &mut sub, &wire).expect("a read must be accepted");

    match message {
        MqttMessage::PublishConfigResponse { status, .. } => assert_eq!(status, "SUCCESS"),
        other => panic!("expected a config/response, got {other:?}"),
    }
    assert!(
        matches!(command, Some(MqttCommand::GetNodeInfo { .. })),
        "the read resolves to the same command its unsigned arm produced"
    );
    assert_eq!(
        hub.manager.active_challenge_count(),
        0,
        "single-shot holds no challenge slot"
    );
}

/// An `operate` request is single-shot too — `silence_buzzer`, which
/// answered nothing at all in v1, now answers `config/response`.
#[test]
fn an_operate_request_is_single_shot_and_now_answers() {
    let hub = Hub::new();
    let signer = hub.manager_signer();
    let mut sub = subscriber();

    let wire = request_json(
        &signer,
        Envelope::V2(Level::Operate),
        "silence_buzzer",
        json!({}),
        "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2",
    );
    let (message, command) = submit(&hub, &mut sub, &wire).expect("an operate must be accepted");
    assert!(matches!(message, MqttMessage::PublishConfigResponse { .. }));
    assert!(matches!(command, Some(MqttCommand::SilenceBuzzer)));
    assert_eq!(hub.manager.active_challenge_count(), 0);
}

/// The audit row says what the device **did**, not what it was about to do.
///
/// Until the 2026-09-15 second review `execute_single_shot` (and the confirm
/// path before it) wrote `applied = 1` the moment the signature verified,
/// and nothing corrected the row when execution failed — so a
/// `silence_buzzer` whose buzzer manager was absent went on the wire as
/// `ERROR` and into `config_changes` as applied by that clinician. The
/// manager now records the verification verdict with `applied = 0`, and the
/// monitor writes the execution result through `log_config_execution` from
/// the same place it decides what `config/response` says.
#[test]
fn a_verified_command_is_not_recorded_as_applied_until_it_has_run() {
    let hub = Hub::new();
    let signer = hub.manager_signer();
    let mut sub = subscriber();

    let wire = request_json(
        &signer,
        Envelope::V2(Level::Operate),
        "silence_buzzer",
        json!({}),
        "b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3",
    );
    let (message, _) = submit(&hub, &mut sub, &wire).expect("an operate must be accepted");
    let MqttMessage::PublishConfigResponse { challenge_id, .. } = message else {
        panic!("single-shot answers config/response");
    };

    // Verified, not yet run: SUCCESS is the verification verdict, `applied`
    // must still be 0.
    let (status, applied, error) = hub.audit_row(&challenge_id);
    assert_eq!(status, "SUCCESS");
    assert_eq!(
        applied, 0,
        "verification alone must not mark the row applied"
    );
    assert_eq!(error, None);

    // Execution failed: still not applied, and the row says why.
    hub.manager
        .log_config_execution(&challenge_id, Err("buzzer manager not available"))
        .expect("record failure");
    let (status, applied, error) = hub.audit_row(&challenge_id);
    assert_eq!(
        status, "SUCCESS",
        "the signature did verify; that fact stands"
    );
    assert_eq!(applied, 0);
    assert_eq!(error.as_deref(), Some("buzzer manager not available"));

    // A second command that does run is the only way a row becomes applied.
    let wire = request_json(
        &signer,
        Envelope::V2(Level::Operate),
        "silence_buzzer",
        json!({}),
        "b4b4b4b4b4b4b4b4b4b4b4b4b4b4b4b4",
    );
    let (message, _) = submit(&hub, &mut sub, &wire).expect("accepted");
    let MqttMessage::PublishConfigResponse { challenge_id, .. } = message else {
        panic!("single-shot answers config/response");
    };
    hub.manager
        .log_config_execution(&challenge_id, Ok(()))
        .expect("record success");
    let (_, applied, error) = hub.audit_row(&challenge_id);
    assert_eq!(applied, 1);
    assert_eq!(error, None);
}

/// `manage` and above keep challenge/confirm, unchanged.
#[test]
fn a_manage_request_mints_a_challenge() {
    let hub = Hub::new();
    let signer = hub.manager_signer();
    let mut sub = subscriber();

    let wire = request_json(
        &signer,
        Envelope::V2(Level::Manage),
        "set_threshold",
        json!({"line": 1, "thresholds": {"critical_low": -30.0, "warning_low": 2.0,
                                         "warning_high": 8.0, "critical_high": 30.0}}),
        "c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3",
    );
    let (message, command) = submit(&hub, &mut sub, &wire).expect("a manage must be accepted");
    assert!(matches!(
        message,
        MqttMessage::PublishConfigChallenge { .. }
    ));
    assert!(command.is_none(), "nothing runs until the signer confirms");
    assert_eq!(hub.manager.active_challenge_count(), 1);
}

/// The mechanism of design.md §4.3, and P1's fourth acceptance criterion: a
/// manager's certificate carries `set_lorawan_sensor_config`, which **is**
/// `sticker_device_reset`'s permission — so the verb check passes and the
/// request is refused on the missing `level:destructive` instead. Without
/// the pseudo-permission this request would be accepted.
#[test]
fn a_manager_signing_a_destructive_verb_is_refused_on_the_level() {
    let hub = Hub::new();
    let signer = hub.manager_signer();
    let mut sub = subscriber();

    assert!(
        signer
            .certificate
            .has_permission("set_lorawan_sensor_config"),
        "the premise: the manager does hold the verb's own permission"
    );

    let wire = request_json(
        &signer,
        Envelope::V2(Level::Destructive),
        "sticker_device_reset",
        json!({"dev_eui": "aabbccddeeff0011"}),
        "d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4",
    );
    let err = submit(&hub, &mut sub, &wire).expect_err("a manager must not reach destructive");
    assert!(
        err.contains("level:destructive"),
        "refused on the level, not on a permission the signer holds: {err}"
    );

    // And the same verb from an admin, whose certificate carries the string.
    let admin = hub.admin_signer();
    let wire = request_json(
        &admin,
        Envelope::V2(Level::Destructive),
        "sticker_device_reset",
        json!({"dev_eui": "aabbccddeeff0011"}),
        "d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5",
    );
    submit(&hub, &mut sub, &wire).expect("an admin holds level:destructive");
}

/// The signed claim must be the level the table says. A caller that relabels
/// a destructive verb as `read` — to dodge the `level:destructive` check —
/// is refused before any crypto runs (design.md §3.3 step 4), even though
/// the signature over that forged pre-image is perfectly valid.
#[test]
fn a_forged_level_is_refused_before_the_signature_is_checked() {
    let hub = Hub::new();
    let signer = hub.manager_signer();
    let mut sub = subscriber();

    let wire = request_json(
        &signer,
        Envelope::V2(Level::Read), // the lie
        "sticker_device_reset",    // the table says destructive
        json!({"dev_eui": "aabbccddeeff0011"}),
        "e6e6e6e6e6e6e6e6e6e6e6e6e6e6e6e6",
    );
    let err = submit(&hub, &mut sub, &wire).expect_err("a relabelled verb must be refused");
    assert!(err.contains("level mismatch"), "{err}");
    assert!(
        err.contains("destructive") && err.contains("read"),
        "the error names both the table's level and the claim: {err}"
    );
}

/// P1's fifth acceptance criterion: all eight verbs that arrived unsigned in
/// v1 work signed, and every one of them is single-shot.
#[test]
fn the_eight_ex_plain_verbs_succeed_signed() {
    let hub = Hub::new();
    let signer = hub.manager_signer();
    let mut sub = subscriber();

    let node = json!({"dev_eui": "aabbccddeeff0011"});
    let cases: [(&str, Level, Value); 8] = [
        ("get_sensor_config", Level::Read, json!({})),
        ("get_interval", Level::Read, json!({})),
        ("get_sticker_config", Level::Read, node.clone()),
        ("get_sticker_info", Level::Read, node.clone()),
        ("get_sticker_full_config", Level::Read, node.clone()),
        ("get_sticker_history", Level::Read, node.clone()),
        ("silence_buzzer", Level::Operate, json!({})),
        ("sticker_force_send", Level::Operate, node.clone()),
    ];

    for (i, (command_type, level, params)) in cases.into_iter().enumerate() {
        let nonce = format!("{:0>32}", format!("f{i}"));
        let wire = request_json(&signer, Envelope::V2(level), command_type, params, &nonce);
        let (message, command) = submit(&hub, &mut sub, &wire)
            .unwrap_or_else(|e| panic!("{command_type} must succeed signed: {e}"));
        assert!(
            matches!(message, MqttMessage::PublishConfigResponse { .. }),
            "{command_type} answers on config/response"
        );
        assert!(command.is_some(), "{command_type} resolves to a command");
    }
    assert_eq!(
        hub.manager.active_challenge_count(),
        0,
        "none of the eight holds a challenge slot"
    );
}

/// **P4: `v == 2` is mandatory.** P1's `accept_v1_envelope` flag is gone with
/// its field and its one builder, so `a_v1_request_still_works_while_the_flag_is_on`
/// went with it — there is nothing left to be on. What remains is the refusal,
/// and it is now unconditional.
///
/// A v1-shaped *signed* request carries neither `v` nor `level`, signs the old
/// seven-key pre-image, and would claim no level at all — so there is no
/// sensible way to route it through a table that decides single-shot by level.
/// It gets design.md §3.2's message, the only v1-shaped thing a v2 device says.
#[test]
fn a_v1_shaped_signed_request_is_rejected() {
    let hub = Hub::new();
    let signer = hub.manager_signer();
    let mut sub = subscriber();

    let wire = request_json(
        &signer,
        Envelope::V1,
        "set_threshold",
        json!({"line": 1, "thresholds": {"critical_low": -30.0, "warning_low": 2.0,
                                         "warning_high": 8.0, "critical_high": 30.0}}),
        "2222222222222222222222222222bbbb",
    );
    assert!(
        wire.get("v").is_none() && wire.get("level").is_none(),
        "the fixture must really be v1-shaped for this to test anything"
    );

    let err = submit(&hub, &mut sub, &wire).expect_err("v1 must be refused");
    assert!(err.contains("unsupported_envelope"), "{err}");
    assert!(err.contains("v=2 required"), "{err}");
}

/// **P4: the flat dialect is gone.** `{"command": "get_sticker_info", …}` was a
/// live, unauthenticated path in v1 — one of eight — and `parse_command` had a
/// parser for each. They are deleted, and `command` is left as what it always
/// should have been: an unsigned *routing* selector with exactly two values
/// (Q2 §3 rejected it as a security marker precisely because it is unsigned).
///
/// The answer is `unsupported_envelope` rather than "unknown command type"
/// because the verb is not what is wrong — the device still knows
/// `get_sticker_info` perfectly well, as a signed `read` row. What is wrong is
/// the shape it arrived in, and that is what the sender needs told.
#[test]
fn a_flat_command_is_rejected_with_unsupported_envelope() {
    let mut sub = subscriber();

    for flat in [
        // The five that were live and unauthenticated.
        br#"{"command": "get_sticker_info", "dev_eui": "aabbccddeeff0011"}"#.to_vec(),
        br#"{"command": "get_sticker_config", "dev_eui": "aabbccddeeff0011"}"#.to_vec(),
        br#"{"command": "get_sticker_full_config", "dev_eui": "aabbccddeeff0011"}"#.to_vec(),
        br#"{"command": "get_sticker_history", "dev_eui": "aabbccddeeff0011"}"#.to_vec(),
        br#"{"command": "sticker_force_send", "dev_eui": "aabbccddeeff0011"}"#.to_vec(),
        // The inert ones. Inert was a property of the dispatch loop dropping
        // them, not of the parser refusing them, and §11's risk file counts a
        // parser that builds a privileged command from an unsigned payload as
        // a hazard source whether or not today's call graph happens to use it.
        br#"{"command": "flush_storage"}"#.to_vec(),
        br#"{"command": "restart"}"#.to_vec(),
        br#"{"command": "get_status"}"#.to_vec(),
        br#"{"command": "get_info"}"#.to_vec(),
        br#"{"command": "get_sensor_config"}"#.to_vec(),
        br#"{"command": "get_interval"}"#.to_vec(),
        br#"{"command": "silence_buzzer"}"#.to_vec(),
        br#"{"command": "set_screen", "enabled": true}"#.to_vec(),
        br#"{"command": "set_interval", "interval_ms": 1000}"#.to_vec(),
        br#"{"command": "history_request", "request_id": "x", "from_ts": 0, "to_ts": 60}"#.to_vec(),
        br#"{"command": "set_threshold", "line": 1}"#.to_vec(),
    ] {
        let err = sub
            .parse_command("commands/whatever", &flat)
            .expect_err("no flat verb survives P4");
        assert!(
            err.contains("unsupported_envelope"),
            "flat payload was not refused as an envelope problem: {err} \n  payload: {}",
            String::from_utf8_lossy(&flat)
        );
    }
}

/// **What the device actually puts on the `errors` topic**, which is the half
/// of `unsupported_envelope` nothing tested until now.
///
/// The two tests above assert on the `Result` string — and the `Result` was
/// always right. The publish site was not: every parse failure went out as
/// `publish_error("unknown", "parse_error", &err)`, so the one answer
/// design.md §3.2 writes out in full arrived with `command: "unknown"`,
/// `error: "parse_error"`, and the slug buried inside `message`. The mock
/// implements §3.2 literally, so `e2e_client.py`'s two assertions on that
/// document passed against the mock and would have failed against a Hub —
/// the exact divergence class P3 exists to catch, on the one case it missed.
///
/// This test asserts the document, not the string.
#[test]
fn an_unsupported_envelope_reaches_the_errors_topic_in_the_shape_the_contract_names() {
    let mut sub = subscriber();

    // Every route into `unsupported_envelope`: a flat verb, a v1-shaped
    // signed request (neither `v` nor `level`), and a wrong `v`.
    let flat = sub
        .parse_command("commands/whatever", br#"{"command": "get_sticker_info"}"#)
        .expect_err("flat is refused");
    let v1_shaped = sub
        .parse_command(
            "commands/config/request",
            br#"{"command": "config_request", "command_type": "set_threshold"}"#,
        )
        .expect_err("a v1-shaped request is refused");

    // …and, since the second review, a `level` travelling without its `v`:
    // whatever else is in the frame, not-`v: 2` is an unsupported envelope.
    let level_without_v = sub
        .parse_command(
            "commands/config/request",
            br#"{"command": "config_request", "level": "read", "command_type": "get_sticker_info"}"#,
        )
        .expect_err("a level with no v is refused");

    for err in [&flat, &v1_shaped, &level_without_v] {
        let (command, error, message) = MqttSubscriber::classify_parse_error(err);
        let payload = MqttPublisher::error_payload(command, error, &message);

        assert_eq!(
            payload["command"], "config_request",
            "design.md §3.2 names the value; it is not the dialect that arrived, \
             because an unsupported envelope is one whose selector we do not trust"
        );
        assert_eq!(payload["error"], "unsupported_envelope");
        assert_eq!(
            payload["message"], "v=2 required",
            "the slug belongs in `error`, not folded into the prose"
        );
        assert!(
            payload.get("timestamp").is_some(),
            "the errors document keeps its four fields"
        );
    }

    // A wrong version says so — more than §3.2 asks for, and useful, but the
    // slug and the `command` must not drift for it.
    let wrong_v = sub
        .parse_command(
            "commands/config/request",
            br#"{"command": "config_request", "v": 3, "level": "read"}"#,
        )
        .expect_err("v=3 is refused");
    let (command, error, message) = MqttSubscriber::classify_parse_error(&wrong_v);
    let payload = MqttPublisher::error_payload(command, error, &message);
    assert_eq!(payload["command"], "config_request");
    assert_eq!(payload["error"], "unsupported_envelope");
    assert_eq!(payload["message"], "v=2 required, got 3");

    // A v2 frame whose `level` is missing or not in the enum is design.md
    // §3.2's `InvalidCommand` — the envelope is fine, the claim inside it is
    // not — and goes out exactly as the manager's own `InvalidCommand`
    // does, under the dialect that arrived. The mock says the same words.
    for (wire, expected) in [
        (
            br#"{"command": "config_request", "v": 2, "command_type": "get_sticker_info"}"#
                .as_slice(),
            "Invalid command: unknown level: <missing>",
        ),
        (
            br#"{"command": "config_request", "v": 2, "level": "root"}"#.as_slice(),
            "Invalid command: unknown level: root",
        ),
        (
            br#"{"command": "config_request", "v": 2, "level": 5}"#.as_slice(),
            "Invalid command: unknown level: 5",
        ),
    ] {
        let err = sub
            .parse_command("commands/config/request", wire)
            .expect_err("a bad level is refused");
        let (command, error, message) = MqttSubscriber::classify_parse_error(&err);
        let payload = MqttPublisher::error_payload(command, error, &message);
        assert_eq!(payload["command"], "config_request", "{err}");
        assert_eq!(payload["error"], "authorization_failed", "{err}");
        assert_eq!(payload["message"], expected, "{err}");
    }
    // …and on the confirm dialect the document names the confirm.
    let err = sub
        .parse_command(
            "commands/config/confirm",
            br#"{"command": "config_confirm", "v": 2, "level": "root"}"#,
        )
        .expect_err("a bad level on a confirm is refused");
    let (command, error, message) = MqttSubscriber::classify_parse_error(&err);
    assert_eq!(command, "config_confirm");
    assert_eq!(error, "authorization_failed");
    assert_eq!(message, "Invalid command: unknown level: root");

    // Anything that is *not* an envelope problem keeps the generic answer —
    // the classifier must not widen the slug to every parse failure.
    let not_json = sub
        .parse_command("commands/config/request", b"{")
        .expect_err("malformed JSON is refused");
    let (command, error, _) = MqttSubscriber::classify_parse_error(&not_json);
    assert_eq!(command, "unknown");
    assert_eq!(error, "parse_error");
}

/// `v` and `level` travel together. A half-v2 envelope signs a pre-image
/// neither side would rebuild, so it is refused — and, since the second
/// review, refused the way design.md §3.2 says: `v` is judged first and
/// alone, so a `level` with no `v` is an unsupported envelope; a `v: 2` with
/// no `level` is a well-formed envelope carrying an invalid claim, so it is
/// `InvalidCommand`. The document each produces is asserted in
/// `an_unsupported_envelope_reaches_the_errors_topic_in_the_shape_the_contract_names`;
/// this test keeps the whole inbound path (subscriber → manager) honest.
#[test]
fn a_half_v2_envelope_is_refused() {
    let hub = Hub::new();
    let signer = hub.manager_signer();
    let mut sub = subscriber();

    let mut wire = request_json(
        &signer,
        Envelope::V2(Level::Manage),
        "set_threshold",
        json!({"line": 1}),
        "3333333333333333333333333333cccc",
    );
    let full = wire.clone();

    wire.as_object_mut().unwrap().remove("level");
    let err = submit(&hub, &mut sub, &wire).expect_err("a v with no level is refused");
    assert!(err.contains("unknown level: <missing>"), "{err}");

    let mut wire = full.clone();
    wire.as_object_mut().unwrap().remove("v");
    let err = submit(&hub, &mut sub, &wire).expect_err("a level with no v is refused");
    assert!(err.contains("unsupported_envelope"), "{err}");

    let mut wire = full;
    wire["v"] = json!(1);
    let err = submit(&hub, &mut sub, &wire).expect_err("v=1 is refused");
    assert!(err.contains("unsupported_envelope"), "{err}");
}

/// `reason` is a required string in v2 — `""` when absent, never `null` or
/// missing. v1's device canonicalized a missing reason as `null` while the
/// portal always emitted `""`; the rule is what stops that asymmetry
/// returning (design.md §3.2).
#[test]
fn a_v2_request_without_a_string_reason_is_refused() {
    let hub = Hub::new();
    let signer = hub.manager_signer();
    let mut sub = subscriber();

    let base = request_json(
        &signer,
        Envelope::V2(Level::Manage),
        "set_threshold",
        json!({"line": 1}),
        "4444444444444444444444444444dddd",
    );

    for bad in [Value::Null, json!(0), json!({})] {
        let mut wire = base.clone();
        wire["reason"] = bad.clone();
        let err = submit(&hub, &mut sub, &wire).expect_err("reason must be a string");
        assert!(err.contains("'reason'"), "for {bad}: {err}");
    }

    let mut wire = base;
    wire.as_object_mut().unwrap().remove("reason");
    let err = submit(&hub, &mut sub, &wire).expect_err("reason must be present");
    assert!(err.contains("'reason'"), "{err}");
}

/// An unknown verb is still rejected before any crypto runs — the level
/// changed the table's shape, not that property (design.md §3.3 step 3).
#[test]
fn an_unknown_verb_is_still_rejected_before_crypto() {
    let hub = Hub::new();
    assert!(matches!(
        hub.manager.command_type_to_spec("restart_device"),
        Err(AuthError::InvalidCommand(_))
    ));
    for withdrawn in [
        "add_eye_tag",
        "remove_eye_tag",
        "detect_eye_tag",
        "set_eye_config",
    ] {
        assert!(
            hub.manager.command_type_to_spec(withdrawn).is_err(),
            "{withdrawn}: none of the v1 *_eye_* wire names exists in v2"
        );
    }
}

// ------------------------------------------------------ 3. the confirm half

/// `node-move` (plan/portal/control.radio.md §7, amendment 2026-09-27) through the whole signed
/// path. A manager's certificate may carry `node.manage` — it registers Nodes — and is refused on
/// the missing `level:admin`: moving a Node is an admin's, like removing one. An admin's request
/// mints a challenge and confirms into the command the monitor hands to `radio_nodes`.
#[test]
fn a_signed_node_move_is_an_admins_and_confirms_into_the_move() {
    let hub = Hub::new();
    let mut sub = subscriber();
    let params = json!({"dev_eui": "5876070000005722", "to": "p2p"});

    let manager = hub.signer(
        "manager@fiber.local",
        "manager",
        &["node.manage", "level:read", "level:operate", "level:manage"],
    );
    let wire = request_json(
        &manager,
        Envelope::V2(Level::Admin),
        "node-move",
        params.clone(),
        "70707070707070707070707070707001",
    );
    let err = submit(&hub, &mut sub, &wire).expect_err("a manager must not move a Node");
    assert!(err.contains("level:admin"), "refused on the level: {err}");
    assert_eq!(hub.manager.active_challenge_count(), 0);

    let admin = hub.signer(
        "admin@fiber.local",
        "admin",
        &[
            "node.manage",
            "level:read",
            "level:operate",
            "level:manage",
            "level:admin",
            "level:destructive",
        ],
    );
    let wire = request_json(
        &admin,
        Envelope::V2(Level::Admin),
        "node-move",
        params,
        "70707070707070707070707070707002",
    );
    let (message, command) = submit(&hub, &mut sub, &wire).expect("an admin holds level:admin");
    let MqttMessage::PublishConfigChallenge { challenge_id, .. } = message else {
        panic!("an admin verb mints a challenge");
    };
    assert!(command.is_none(), "nothing runs until the signer confirms");

    let wire = confirm_json(
        &admin,
        Envelope::V2(Level::Admin),
        &challenge_id,
        "APPROVED",
        "70707070707070707070707070707003",
    );
    let (_, command) = submit_confirm(&hub, &mut sub, &wire).expect("the confirm is accepted");
    match command {
        Some(MqttCommand::NodeMove { dev_eui, to }) => {
            assert_eq!(dev_eui, "5876070000005722");
            assert_eq!(to, "p2p");
        }
        other => panic!("expected NodeMove, got {other:?}"),
    }
}

/// Build the wire JSON of a signed `config_confirm`.
fn confirm_json(
    signer: &Signer_,
    envelope: Envelope,
    challenge_id: &str,
    confirmation: &str,
    nonce: &str,
) -> Value {
    let timestamp = now();
    let mut pre = std::collections::BTreeMap::new();
    pre.insert("challenge_id".to_string(), json!(challenge_id));
    pre.insert("confirmation".to_string(), json!(confirmation));
    pre.insert("nonce".to_string(), json!(nonce));
    pre.insert("signer_id".to_string(), json!(signer.certificate.signer_id));
    pre.insert("timestamp".to_string(), json!(timestamp));
    if let Envelope::V2(level) = envelope {
        pre.insert("level".to_string(), json!(level.as_str()));
        pre.insert("v".to_string(), json!(ENVELOPE_VERSION));
    }
    let canonical = serde_json::to_string(&pre).unwrap();
    let signature = BASE64.encode(signer.key.sign(canonical.as_bytes()).to_bytes());

    let mut wire = json!({
        "command": "config_confirm",
        "challenge_id": challenge_id,
        "confirmation": confirmation,
        "nonce": nonce,
        "signer_id": signer.certificate.signer_id,
        "timestamp": timestamp,
        "signature": signature,
        "certificate": signer.certificate,
    });
    if let Envelope::V2(level) = envelope {
        wire["v"] = json!(ENVELOPE_VERSION);
        wire["level"] = json!(level.as_str());
    }
    wire
}

fn submit_confirm(
    hub: &Hub,
    subscriber: &mut MqttSubscriber,
    wire: &Value,
) -> Result<(MqttMessage, Option<MqttCommand>), String> {
    let payload = serde_json::to_vec(wire).unwrap();
    let parsed = subscriber.parse_command("proximos/host/commands/config/confirm", &payload)?;
    let MqttCommand::ConfigConfirm {
        envelope,
        challenge_id,
        confirmation,
        signer_id,
        signature,
        timestamp,
        nonce,
        certificate,
    } = parsed
    else {
        panic!("expected a ConfigConfirm");
    };
    hub.manager
        .process_config_confirm(
            envelope,
            challenge_id,
            confirmation,
            signer_id,
            signature,
            timestamp,
            nonce,
            &certificate,
        )
        .map_err(|e| e.to_string())
}

/// Mint a `manage` challenge and return its id.
fn open_challenge(hub: &Hub, sub: &mut MqttSubscriber, signer: &Signer_, nonce: &str) -> String {
    let wire = request_json(
        signer,
        Envelope::V2(Level::Manage),
        "set_threshold",
        json!({"line": 1, "thresholds": {"critical_low": -30.0, "warning_low": 2.0,
                                         "warning_high": 8.0, "critical_high": 30.0}}),
        nonce,
    );
    match submit(hub, sub, &wire).expect("the manage request is accepted") {
        (MqttMessage::PublishConfigChallenge { challenge_id, .. }, None) => challenge_id,
        other => panic!("expected a challenge, got {other:?}"),
    }
}

/// The full round trip still works end to end under v2.
#[test]
fn a_manage_challenge_confirms_at_its_own_level() {
    let hub = Hub::new();
    let signer = hub.manager_signer();
    let mut sub = subscriber();

    let challenge_id = open_challenge(&hub, &mut sub, &signer, "5555555555555555555555555555eeee");
    let wire = confirm_json(
        &signer,
        Envelope::V2(Level::Manage),
        &challenge_id,
        "APPROVED",
        "6666666666666666666666666666ffff",
    );
    let (message, command) =
        submit_confirm(&hub, &mut sub, &wire).expect("the confirm is accepted");
    assert!(matches!(message, MqttMessage::PublishConfigResponse { .. }));
    assert!(matches!(
        command,
        Some(MqttCommand::SetSensorThreshold { .. })
    ));
    assert_eq!(hub.manager.active_challenge_count(), 0);
}

/// A confirm cannot escalate past its own request: the level it signs must
/// be the level the challenge was minted at (design.md §3.3).
#[test]
fn a_confirm_cannot_escalate_past_its_challenge() {
    let hub = Hub::new();
    let signer = hub.admin_signer();
    let mut sub = subscriber();

    let challenge_id = open_challenge(&hub, &mut sub, &signer, "7777777777777777777777777777aaaa");
    let wire = confirm_json(
        &signer,
        Envelope::V2(Level::Destructive), // the challenge was manage
        &challenge_id,
        "APPROVED",
        "8888888888888888888888888888bbbb",
    );
    let err = submit_confirm(&hub, &mut sub, &wire).expect_err("a relabelled confirm is refused");
    assert!(err.contains("level mismatch"), "{err}");
    assert_eq!(
        hub.manager.active_challenge_count(),
        1,
        "the challenge outlives the bad confirm rather than being consumed by it"
    );
}

/// The two halves must speak the same envelope: a v1 confirm cannot close a
/// v2 challenge, because they do not sign the same fields.
///
/// Since P4 it does not get as far as the level check — `parse_envelope`
/// refuses it outright — which is a strictly earlier and cheaper refusal for
/// the same reason. What the test pins is unchanged: the challenge survives.
#[test]
fn a_v1_confirm_cannot_close_a_v2_challenge() {
    let hub = Hub::new();
    let signer = hub.manager_signer();
    let mut sub = subscriber();

    let challenge_id = open_challenge(&hub, &mut sub, &signer, "9999999999999999999999999999cccc");
    let wire = confirm_json(
        &signer,
        Envelope::V1,
        &challenge_id,
        "APPROVED",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaadddd",
    );
    let err = submit_confirm(&hub, &mut sub, &wire).expect_err("a v1 confirm is refused");
    assert!(err.contains("unsupported_envelope"), "{err}");
    assert_eq!(
        hub.manager.active_challenge_count(),
        1,
        "and the challenge it could not close is still open"
    );
}

// ------------------------------- 4. the two 2026-09-16 verbs, end to end

/// `setup-complete` (GitLab #58) — the signed twin of BLE FB17 — through the whole signed path:
/// request → challenge → confirm → the `MqttCommand` the monitor dispatches.
///
/// The verb had rows in three registries and an arm in `build_command_from_challenge`, and
/// nothing drove it. What this pins beyond "it works" is the level: `manage`, so it mints a
/// challenge rather than executing single-shot, and the role survives the round trip through
/// the challenge store rather than being rebuilt from a default.
#[test]
fn a_signed_setup_complete_confirms_into_the_command_the_applier_takes() {
    let hub = Hub::new();
    let signer = hub.commissioning_signer();
    let mut sub = subscriber();

    let wire = request_json(
        &signer,
        Envelope::V2(Level::Manage),
        "setup-complete",
        json!({"role": "client"}),
        "58585858585858585858585858585801",
    );
    let (message, command) = submit(&hub, &mut sub, &wire).expect("setup.manage is held");
    let MqttMessage::PublishConfigChallenge { challenge_id, .. } = message else {
        panic!("a manage verb mints a challenge");
    };
    assert!(command.is_none(), "nothing runs until the signer confirms");

    let wire = confirm_json(
        &signer,
        Envelope::V2(Level::Manage),
        &challenge_id,
        "APPROVED",
        "58585858585858585858585858585802",
    );
    let (_, command) = submit_confirm(&hub, &mut sub, &wire).expect("the confirm is accepted");
    match command {
        Some(MqttCommand::SetupComplete { role }) => assert_eq!(role, "client"),
        other => panic!("expected SetupComplete, got {other:?}"),
    }
}

/// `export-destination-set` (GitLab #59) — the signed twin of BLE FB18.
///
/// `admin`, not `manage`: this decides where a site's measurement data leaves the device for.
/// The optional fields default the same way on both surfaces, which is what makes the
/// both-surfaces test in `tests/export_destination_simulated.rs` a comparison of the same input.
#[test]
fn a_signed_export_destination_set_confirms_into_the_command_the_applier_takes() {
    let hub = Hub::new();
    let signer = hub.commissioning_signer();
    let mut sub = subscriber();

    let wire = request_json(
        &signer,
        Envelope::V2(Level::Admin),
        "export-destination-set",
        json!({"host": "hub-1.local", "username": "fiber", "password": "s3cret"}),
        "59595959595959595959595959595901",
    );
    let (message, command) = submit(&hub, &mut sub, &wire).expect("export.manage is held");
    let MqttMessage::PublishConfigChallenge { challenge_id, .. } = message else {
        panic!("an admin verb mints a challenge");
    };
    assert!(command.is_none());

    let wire = confirm_json(
        &signer,
        Envelope::V2(Level::Admin),
        &challenge_id,
        "APPROVED",
        "59595959595959595959595959595902",
    );
    let (_, command) = submit_confirm(&hub, &mut sub, &wire).expect("the confirm is accepted");
    match command {
        Some(MqttCommand::SetExportDestination {
            host,
            username,
            password,
            enabled,
        }) => {
            assert_eq!(host, "hub-1.local");
            assert_eq!(username, "fiber");
            assert_eq!(password, "s3cret");
            assert!(
                enabled,
                "absent `enabled` means the operator is turning it on"
            );
        }
        other => panic!("expected SetExportDestination, got {other:?}"),
    }
}

/// Neither verb rides in on a certificate that predates it.
///
/// `admin_signer` holds `level:admin` and `level:manage`, so each request clears the level check
/// and is refused on the verb's own permission — the reason these two are separate permissions
/// rather than reuses of `set_device_label` (§4.2, and the registry's note on the row): a
/// certificate that authorizes renaming a Hub must not thereby authorize declaring it
/// commissioned, or redirecting where its data goes.
#[test]
fn the_two_new_verbs_are_refused_on_their_own_permission() {
    let hub = Hub::new();
    let signer = hub.admin_signer();
    let mut sub = subscriber();

    for (i, (command_type, level, permission, params)) in [
        (
            "setup-complete",
            Level::Manage,
            "setup.manage",
            json!({"role": "server"}),
        ),
        (
            "export-destination-set",
            Level::Admin,
            "export.manage",
            json!({"host": "hub-1.local"}),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        assert!(
            !signer.certificate.has_permission(permission),
            "the premise: an admin certificate issued before {command_type} carries no {permission}"
        );

        let nonce = format!("{:0>32}", format!("ded{i}"));
        let wire = request_json(&signer, Envelope::V2(level), command_type, params, &nonce);
        let err = submit(&hub, &mut sub, &wire)
            .expect_err("the verb's own permission is not held, so the request is refused");
        assert!(
            err.contains(permission),
            "{command_type} must be refused on {permission}, not on something else: {err}"
        );
        assert_eq!(
            hub.manager.active_challenge_count(),
            0,
            "{command_type}: a refused request holds no challenge slot"
        );
    }
}

/// `radio-set-mode` (plan/control/radio/node_registry.md §11, amended R7): admin, its own
/// `radio.manage` permission, and the mode checked at the one construction site.
#[test]
fn a_signed_radio_set_mode_confirms_into_the_mode_and_needs_its_own_permission() {
    let hub = Hub::new();
    let signer = hub.signer(
        "radio@fiber.local",
        "admin",
        &[
            "radio.manage",
            "get_status",
            "level:read",
            "level:operate",
            "level:manage",
            "level:admin",
        ],
    );
    let mut sub = subscriber();

    let wire = request_json(
        &signer,
        Envelope::V2(Level::Admin),
        "radio-set-mode",
        json!({"mode": "p2p"}),
        "71717171717171717171717171717101",
    );
    let (message, command) = submit(&hub, &mut sub, &wire).expect("radio.manage is held");
    let MqttMessage::PublishConfigChallenge { challenge_id, .. } = message else {
        panic!("an admin verb mints a challenge");
    };
    assert!(command.is_none());
    let wire = confirm_json(
        &signer,
        Envelope::V2(Level::Admin),
        &challenge_id,
        "APPROVED",
        "71717171717171717171717171717102",
    );
    let (_, command) = submit_confirm(&hub, &mut sub, &wire).expect("the confirm is accepted");
    match command {
        Some(MqttCommand::SetRadioMode { mode }) => assert_eq!(mode, "p2p"),
        other => panic!("expected SetRadioMode, got {other:?}"),
    }

    // A mode that is not one of the four never becomes a command.
    let wire = request_json(
        &signer,
        Envelope::V2(Level::Admin),
        "radio-set-mode",
        json!({"mode": "both"}),
        "71717171717171717171717171717103",
    );
    let refused = match submit(&hub, &mut sub, &wire) {
        Err(e) => e,
        Ok((MqttMessage::PublishConfigChallenge { challenge_id, .. }, _)) => {
            let wire = confirm_json(
                &signer,
                Envelope::V2(Level::Admin),
                &challenge_id,
                "APPROVED",
                "71717171717171717171717171717104",
            );
            match submit_confirm(&hub, &mut sub, &wire) {
                Err(e) => e,
                Ok((_, Some(command))) => panic!("a bad mode built {command:?}"),
                Ok((message, None)) => format!("{message:?}"),
            }
        }
        Ok(other) => panic!("unexpected {other:?}"),
    };
    assert!(refused.contains("both"), "{refused}");

    // An admin certificate issued before the verb carries no radio.manage.
    let old = hub.admin_signer();
    assert!(!old.certificate.has_permission("radio.manage"));
    let wire = request_json(
        &old,
        Envelope::V2(Level::Admin),
        "radio-set-mode",
        json!({"mode": "p2p"}),
        "71717171717171717171717171717105",
    );
    let err = submit(&hub, &mut sub, &wire).expect_err("refused on its own permission");
    assert!(err.contains("radio.manage"), "{err}");
}

/// `northbridge-update` (plan/control/radio/northbridge_update.md §3.4, amended 2026-10-09):
/// admin, `radio.manage` like `radio-set-mode`, `force` a strict boolean defaulting to false.
#[test]
fn a_signed_northbridge_update_confirms_into_the_request_and_needs_the_radio_permission() {
    let hub = Hub::new();
    let signer = hub.signer(
        "nb@fiber.local",
        "admin",
        &[
            "radio.manage",
            "get_status",
            "level:read",
            "level:operate",
            "level:manage",
            "level:admin",
        ],
    );
    let mut sub = subscriber();
    let mut confirmed = |params: Value, id: u8| {
        let wire = request_json(
            &signer,
            Envelope::V2(Level::Admin),
            "northbridge-update",
            params,
            &format!("727272727272727272727272727272{id:02x}"),
        );
        let (message, command) = submit(&hub, &mut sub, &wire)?;
        let MqttMessage::PublishConfigChallenge { challenge_id, .. } = message else {
            panic!("an admin verb mints a challenge");
        };
        assert!(command.is_none());
        let wire = confirm_json(
            &signer,
            Envelope::V2(Level::Admin),
            &challenge_id,
            "APPROVED",
            &format!("737373737373737373737373737373{id:02x}"),
        );
        submit_confirm(&hub, &mut sub, &wire).map(|(_, c)| c)
    };
    for (params, want, id) in [
        (json!({"force": true}), true, 1),
        (json!({"force": false}), false, 2),
        (json!({}), false, 3),
    ] {
        match confirmed(params.clone(), id) {
            Ok(Some(MqttCommand::NorthbridgeUpdate { force })) => {
                assert_eq!(force, want, "{params}")
            }
            other => panic!("{params}: expected NorthbridgeUpdate, got {other:?}"),
        }
    }
    // `"yes"` is neither: refused, not guessed.
    match confirmed(json!({"force": "yes"}), 4) {
        Ok(Some(command)) => panic!("a non-boolean force built {command:?}"),
        Err(e) => assert!(e.contains("force"), "{e}"),
        Ok(None) => {}
    }

    // A Node-management certificate, however high its level, is not enough.
    let old = hub.admin_signer();
    assert!(!old.certificate.has_permission("radio.manage"));
    let wire = request_json(
        &old,
        Envelope::V2(Level::Admin),
        "northbridge-update",
        json!({"force": false}),
        "74747474747474747474747474747401",
    );
    let err = submit(&hub, &mut sub, &wire).expect_err("refused on its own permission");
    assert!(err.contains("radio.manage"), "{err}");
}

/// `southbridge-update` (plan/system/southbridge_update.md §3.7): like the Northbridge's verb,
/// admin, `radio.manage`, `force` a strict boolean defaulting to false.
#[test]
fn a_signed_southbridge_update_confirms_into_the_request_and_needs_the_radio_permission() {
    let hub = Hub::new();
    let signer = hub.signer(
        "sb@fiber.local",
        "admin",
        &[
            "radio.manage",
            "get_status",
            "level:read",
            "level:operate",
            "level:manage",
            "level:admin",
        ],
    );
    let mut sub = subscriber();
    let mut confirmed = |params: Value, id: u8| {
        let wire = request_json(
            &signer,
            Envelope::V2(Level::Admin),
            "southbridge-update",
            params,
            &format!("757575757575757575757575757575{id:02x}"),
        );
        let (message, command) = submit(&hub, &mut sub, &wire)?;
        let MqttMessage::PublishConfigChallenge { challenge_id, .. } = message else {
            panic!("an admin verb mints a challenge");
        };
        assert!(command.is_none());
        let wire = confirm_json(
            &signer,
            Envelope::V2(Level::Admin),
            &challenge_id,
            "APPROVED",
            &format!("767676767676767676767676767676{id:02x}"),
        );
        submit_confirm(&hub, &mut sub, &wire).map(|(_, c)| c)
    };
    for (params, want, id) in [
        (json!({"force": true}), true, 1),
        (json!({"force": false}), false, 2),
        (json!({}), false, 3),
    ] {
        match confirmed(params.clone(), id) {
            Ok(Some(MqttCommand::SouthbridgeUpdate { force })) => {
                assert_eq!(force, want, "{params}")
            }
            other => panic!("{params}: expected SouthbridgeUpdate, got {other:?}"),
        }
    }
    // `"yes"` is neither: refused, not guessed.
    match confirmed(json!({"force": "yes"}), 4) {
        Ok(Some(command)) => panic!("a non-boolean force built {command:?}"),
        Err(e) => assert!(e.contains("force"), "{e}"),
        Ok(None) => {}
    }

    // A Node-management certificate, however high its level, is not enough.
    let old = hub.admin_signer();
    assert!(!old.certificate.has_permission("radio.manage"));
    let wire = request_json(
        &old,
        Envelope::V2(Level::Admin),
        "southbridge-update",
        json!({"force": false}),
        "77777777777777777777777777777701",
    );
    let err = submit(&hub, &mut sub, &wire).expect_err("refused on its own permission");
    assert!(err.contains("radio.manage"), "{err}");
}

/// `cluster-client-add` / `-remove` (plan/control/cluster.md §7): admin, their own
/// `cluster.manage` permission, the hostname, EUI and `reply_key` checked at the one construction
/// site, the seal bound to the signed request's id, and a preview that names the Client and never
/// a credential (none exists yet when it is shown) nor the reply key.
/// An X25519 public key, as the portal sends it (`fiber_protocol::sealed_reply`): made up.
const REPLY_KEY: &str = "1wWpoC1yfr-Mw9EeK1ML134Q3CxHu5Otu_l1wzNrSy0";

#[test]
fn a_signed_cluster_client_add_confirms_into_the_request_and_needs_its_own_permission() {
    let hub = Hub::new();
    let signer = hub.signer(
        "cluster@fiber.local",
        "admin",
        &[
            "cluster.manage",
            "get_status",
            "level:read",
            "level:operate",
            "level:manage",
            "level:admin",
        ],
    );
    let mut sub = subscriber();

    let wire = request_json(
        &signer,
        Envelope::V2(Level::Admin),
        "cluster-client-add",
        json!({
            "hostname": "PROXIMOS-5E6F7A8B",
            "gateway_eui": "00:16:C0:01:FF:10:A2:35",
            "reply_key": REPLY_KEY,
        }),
        "72727272727272727272727272727201",
    );
    let (message, command) = submit(&hub, &mut sub, &wire).expect("cluster.manage is held");
    let MqttMessage::PublishConfigChallenge {
        challenge_id,
        preview,
        ..
    } = message
    else {
        panic!("an admin verb mints a challenge");
    };
    assert!(command.is_none());
    let preview = preview.to_string();
    assert!(
        preview.contains("PROXIMOS-5E6F7A8B") || preview.contains("proximos-5e6f7a8b"),
        "{preview}"
    );
    assert!(!preview.to_lowercase().contains("password\":"), "{preview}");
    assert!(
        !preview.contains(REPLY_KEY),
        "the reply key is named, not shown: {preview}"
    );
    let wire = confirm_json(
        &signer,
        Envelope::V2(Level::Admin),
        &challenge_id,
        "APPROVED",
        "72727272727272727272727272727202",
    );
    let (_, command) = submit_confirm(&hub, &mut sub, &wire).expect("the confirm is accepted");
    match command {
        Some(MqttCommand::ClusterClientAdd {
            hostname,
            gateway_eui,
            reply,
        }) => {
            assert_eq!(hostname, "proximos-5e6f7a8b", "lowercased (decision #21)");
            assert_eq!(gateway_eui.as_deref(), Some("0016c001ff10a235"));
            assert_eq!(reply.key.as_base64(), REPLY_KEY);
            assert_eq!(
                reply.request_id, "req-72727272727272727272727272727201",
                "the seal is bound to the signed request's own id"
            );
        }
        other => panic!("expected ClusterClientAdd, got {other:?}"),
    }

    // Removal: the hostname alone.
    let wire = request_json(
        &signer,
        Envelope::V2(Level::Admin),
        "cluster-client-remove",
        json!({"hostname": "proximos-5e6f7a8b"}),
        "72727272727272727272727272727203",
    );
    let (message, _) = submit(&hub, &mut sub, &wire).expect("accepted");
    let MqttMessage::PublishConfigChallenge { challenge_id, .. } = message else {
        panic!("a challenge");
    };
    let wire = confirm_json(
        &signer,
        Envelope::V2(Level::Admin),
        &challenge_id,
        "APPROVED",
        "72727272727272727272727272727204",
    );
    let (_, command) = submit_confirm(&hub, &mut sub, &wire).expect("confirmed");
    assert!(
        matches!(command, Some(MqttCommand::ClusterClientRemove { ref hostname }) if hostname == "proximos-5e6f7a8b"),
        "{command:?}"
    );

    // A hostname that is not decision #17's never becomes a command.
    for (i, params) in [
        json!({"hostname": "hub-5e6f7a8b", "reply_key": REPLY_KEY}),
        json!({"hostname": "proximos-5e6f7a8b", "gateway_eui": "nope", "reply_key": REPLY_KEY}),
        json!({"reply_key": REPLY_KEY}),
        // No clear-text fallback: an add without a usable reply key is refused.
        json!({"hostname": "proximos-5e6f7a8b"}),
        json!({"hostname": "proximos-5e6f7a8b", "reply_key": "AAAA"}),
    ]
    .into_iter()
    .enumerate()
    {
        let wire = request_json(
            &signer,
            Envelope::V2(Level::Admin),
            "cluster-client-add",
            params,
            &format!("727272727272727272727272727273{i:02}"),
        );
        let refused = match submit(&hub, &mut sub, &wire) {
            Err(e) => e,
            Ok((MqttMessage::PublishConfigChallenge { challenge_id, .. }, _)) => {
                let wire = confirm_json(
                    &signer,
                    Envelope::V2(Level::Admin),
                    &challenge_id,
                    "APPROVED",
                    &format!("727272727272727272727272727274{i:02}"),
                );
                match submit_confirm(&hub, &mut sub, &wire) {
                    Err(e) => e,
                    Ok((_, Some(command))) => panic!("a bad request built {command:?}"),
                    Ok((message, None)) => format!("{message:?}"),
                }
            }
            Ok(other) => panic!("unexpected {other:?}"),
        };
        assert!(
            refused.contains("hostname")
                || refused.contains("gateway_eui")
                || refused.contains("reply_key"),
            "{refused}"
        );
    }

    // An admin certificate issued before the verb carries no cluster.manage.
    let old = hub.admin_signer();
    assert!(!old.certificate.has_permission("cluster.manage"));
    let wire = request_json(
        &old,
        Envelope::V2(Level::Admin),
        "cluster-client-add",
        json!({"hostname": "proximos-5e6f7a8b", "reply_key": REPLY_KEY}),
        "72727272727272727272727272727205",
    );
    let err = submit(&hub, &mut sub, &wire).expect_err("refused on its own permission");
    assert!(err.contains("cluster.manage"), "{err}");
}

// ------------------------------------------------- config sync items (S3)

/// Parse a wire request and hand it to `process_sync_item`, as the monitor
/// does for each item of a config-sync document.
fn submit_sync(hub: &Hub, wire: &Value) -> Result<(String, MqttCommand), String> {
    let MqttCommand::ConfigRequest {
        envelope,
        request_id,
        command_type,
        params,
        reason,
        signer_id,
        signature,
        timestamp,
        nonce,
        certificate,
    } = subscriber().parse_config_request(wire)?
    else {
        panic!("expected a ConfigRequest");
    };
    hub.manager
        .process_sync_item(
            envelope,
            request_id,
            command_type,
            params,
            reason,
            signer_id,
            signature,
            timestamp,
            nonce,
            &certificate,
        )
        .map_err(|e| e.to_string())
}

fn threshold_params() -> Value {
    json!({"line": 1, "thresholds": {"critical_low": -30.0, "warning_low": 2.0,
                                     "warning_high": 8.0, "critical_high": 30.0}})
}

/// A manage-level sync item runs without a challenge; the next item with its
/// own nonce is not a replay, a tampered one fails alone, and a redelivered
/// item is refused as a replay.
#[test]
fn sync_items_skip_the_challenge_but_keep_every_check() {
    let hub = Hub::new();
    let signer = hub.manager_signer();
    let level = Envelope::V2(Level::Manage);

    let a = request_json(
        &signer,
        level,
        "set_threshold",
        threshold_params(),
        "11111111111111111111111111111111",
    );
    let (audit_id, cmd) = submit_sync(&hub, &a).expect("a signed manage item applies");
    assert!(!matches!(cmd, MqttCommand::ConfigRequest { .. }));
    assert_eq!(hub.manager.active_challenge_count(), 0);
    assert_eq!(hub.audit_row(&audit_id).0, "SUCCESS");

    let mut bad = request_json(
        &signer,
        level,
        "set_threshold",
        threshold_params(),
        "22222222222222222222222222222222",
    );
    bad["params"]["line"] = json!(2);
    assert!(submit_sync(&hub, &bad).unwrap_err().contains("Signature"));

    let c = request_json(
        &signer,
        level,
        "set_threshold",
        threshold_params(),
        "33333333333333333333333333333333",
    );
    submit_sync(&hub, &c).expect("a later item with its own nonce is not a replay");

    assert!(
        submit_sync(&hub, &a).is_err(),
        "a redelivered item is a replay"
    );
}
