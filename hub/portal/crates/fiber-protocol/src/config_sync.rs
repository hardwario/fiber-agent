//! Portal -> Hub configuration sync: topics and document schema
//! (plan/portal/config_sync.md §5, §6; decision #43).
//!
//! The Hub pulls: it publishes a [`SyncRequest`] on
//! `{prefix}/{hub}/config/sync/req`, the Portal answers on `.../resp` with a
//! [`SyncResponse`] (up to date, or the full [`DesiredDocument`]), the Hub
//! applies it and reports per key on `.../ack` ([`SyncAck`]). After a save the
//! Portal publishes an unsigned [`SyncHint`] on `.../hint` so the Hub asks at
//! once; a hint carries no data, so a forged one only causes a request.
//!
//! Each [`SyncItem`] carries an ordinary signed `config_request`
//! ([`SignedRequest`]), minted fresh at send time with the editor's key (§6);
//! the Hub verifies it like any relayed command.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use std::collections::BTreeMap;

use crate::certificate::Certificate;
use crate::envelope::{canonical_json, SignedRequest};

/// Topic suffixes, published as `{prefix}/{hub}/{suffix}`.
pub const CONFIG_SYNC_REQ: &str = "config/sync/req";
pub const CONFIG_SYNC_RESP: &str = "config/sync/resp";
pub const CONFIG_SYNC_ACK: &str = "config/sync/ack";
pub const CONFIG_SYNC_HINT: &str = "config/sync/hint";

/// Largest `resp` body the Portal publishes. The Hub's monitor accepts
/// 256 KiB incoming; half of that leaves room for growth and framing. A
/// larger document is refused by the Portal (logged, `sync:` alarm), never
/// sent: an oversized packet makes rumqttc drop the whole connection.
pub const MAX_RESP_BYTES: usize = 128 * 1024;

/// The four sync topics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncTopic {
    /// Hub -> Portal.
    Req,
    /// Portal -> Hub.
    Resp,
    /// Hub -> Portal.
    Ack,
    /// Portal -> Hub, unsigned.
    Hint,
}

impl SyncTopic {
    pub fn suffix(self) -> &'static str {
        match self {
            SyncTopic::Req => CONFIG_SYNC_REQ,
            SyncTopic::Resp => CONFIG_SYNC_RESP,
            SyncTopic::Ack => CONFIG_SYNC_ACK,
            SyncTopic::Hint => CONFIG_SYNC_HINT,
        }
    }

    /// `{prefix}/{hub}/config/sync/{req|resp|ack|hint}`.
    pub fn topic(self, prefix: &str, hub: &str) -> String {
        format!("{prefix}/{hub}/{}", self.suffix())
    }
}

/// Parse a sync topic into `(hub, kind)`; `None` for anything else.
pub fn parse_sync_topic<'a>(topic: &'a str, prefix: &str) -> Option<(&'a str, SyncTopic)> {
    let rest = topic.strip_prefix(prefix)?.strip_prefix('/')?;
    let (hub, suffix) = rest.split_once('/')?;
    if hub.is_empty() {
        return None;
    }
    let kind = [
        SyncTopic::Req,
        SyncTopic::Resp,
        SyncTopic::Ack,
        SyncTopic::Hint,
    ]
    .into_iter()
    .find(|k| k.suffix() == suffix)?;
    Some((hub, kind))
}

/// Verbs that are desired state (§2): idempotent and readable back.
/// `set_network_config` and every one-shot verb are deliberately absent.
/// The Beacon interval rides on `set_interval`.
pub const DESIRED_VERBS: &[&str] = &[
    // Hub
    "set_threshold",
    "set_sensor_name",
    "set_sensor_location",
    "set_interval",
    "set_sample_interval",
    "set_system_info_interval",
    "set_display_lines",
    "set_screen_brightness",
    "set_screen_timeout",
    "set_led_brightness",
    "set_buzzer_volume",
    "set_alarm_pattern",
    // Beacon
    "set_eye_field_threshold",
    "set_device_label",
    // Node
    "set_lorawan_field_threshold",
    "set_lorawan_sensor_config",
    "set_sticker_config",
];

/// The 1-Wire read interval a `set_sample_interval` may carry, in seconds
/// (§13 Q19). Capped at the smallest storage window (60 s), so it is valid
/// whatever window the Hub keeps; both the Portal and the Hub check it.
pub const SAMPLE_INTERVAL_S: std::ops::RangeInclusive<u64> = 1..=60;

/// Whether `verb` may appear in a [`DesiredDocument`].
pub fn is_desired_verb(verb: &str) -> bool {
    DESIRED_VERBS.contains(&verb)
}

/// Hub -> Portal: the Hub's last applied generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncRequest {
    pub gen: u64,
    pub hash: String,
    pub boot_id: String,
}

/// Portal -> Hub, unsigned: "ask now".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncHint {
    pub gen: u64,
}

/// Portal -> Hub answer to a [`SyncRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SyncResponse {
    UpToDate { gen: u64 },
    Document(DesiredDocument),
}

/// Which device a [`SyncItem`] configures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SyncTarget {
    Hub,
    Beacon { mac: String },
    Node { dev_eui: String },
}

/// One desired setting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncItem {
    /// Stable id, e.g. `hub/threshold/1`, `beacon/<MAC>/threshold/<field>`,
    /// `node/<deveui>/threshold/<field>`.
    pub key: String,
    pub target: SyncTarget,
    /// The verb; equals `request.command_type`.
    pub command: String,
    /// Signed by the editor at send time (§6).
    pub request: SignedRequest,
    /// The user who saved the value (audit `actor`).
    pub editor: String,
}

/// The full desired configuration of one Hub and its Nodes/Beacons.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesiredDocument {
    pub gen: u64,
    /// [`document_hash`] of `items`.
    pub hash: String,
    pub items: Vec<SyncItem>,
    /// Signer certificates by `signer_id`, hoisted out of the items so a
    /// document signed by a few editors carries each certificate once
    /// (amendment 2026-10-15, HIL: 22 items with inline certificates were
    /// 27 KB). An item whose `request.certificate` is absent takes it from
    /// here; not part of the hash, like every other signing detail.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub certificates: BTreeMap<String, Certificate>,
}

/// Per-key apply result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncItemResult {
    pub key: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Hub -> Portal after applying a document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncAck {
    pub gen: u64,
    pub hash: String,
    pub results: Vec<SyncItemResult>,
}

/// Canonical hash of the desired *values*: lowercase sha256 hex over the
/// canonical JSON of `[{key, command, params}, ...]` sorted by `key`.
///
/// Signatures, nonces, timestamps, request ids, certificates and the editor
/// are excluded on purpose: items are re-signed on every send (§6), so a
/// hash over them would never match. Two documents with the same values
/// hash equal whatever their order or signer.
pub fn document_hash(items: &[SyncItem]) -> String {
    let mut sorted: Vec<&SyncItem> = items.iter().collect();
    sorted.sort_by(|a, b| a.key.cmp(&b.key));
    let values: Vec<Value> = sorted
        .iter()
        .map(|i| json!({"key": i.key, "command": i.command, "params": i.request.params}))
        .collect();
    hex::encode(Sha256::digest(
        canonical_json(&Value::Array(values)).as_bytes(),
    ))
}

impl DesiredDocument {
    /// Build a document with its hash computed from `items`.
    pub fn new(gen: u64, items: Vec<SyncItem>) -> Self {
        let hash = document_hash(&items);
        Self {
            gen,
            hash,
            items,
            certificates: BTreeMap::new(),
        }
    }

    /// Move every item's certificate into [`Self::certificates`], once per
    /// signer. A signer seen with two different certificates keeps the
    /// later ones inline, so nothing is ever replaced.
    pub fn hoist_certificates(&mut self) {
        for item in &mut self.items {
            let Some(cert) = item.request.certificate.take() else {
                continue;
            };
            match self.certificates.get(&item.request.signer_id) {
                None => {
                    self.certificates
                        .insert(item.request.signer_id.clone(), cert);
                }
                Some(held) if *held == cert => {}
                Some(_) => item.request.certificate = Some(cert),
            }
        }
    }

    /// The item's signed request with its certificate restored from the
    /// document when it was hoisted.
    pub fn full_request(&self, item: &SyncItem) -> SignedRequest {
        let mut request = item.request.clone();
        if request.certificate.is_none() {
            request.certificate = self.certificates.get(&request.signer_id).cloned();
        }
        request
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::build_request;

    fn item(key: &str, verb: &str, params: Value, seed: u8) -> SyncItem {
        let request = build_request(verb, params, None, "alice", &[seed; 32], None).unwrap();
        SyncItem {
            key: key.into(),
            target: SyncTarget::Hub,
            command: verb.into(),
            request,
            editor: "alice".into(),
        }
    }

    fn items(seed: u8) -> Vec<SyncItem> {
        vec![
            item(
                "hub/threshold/1",
                "set_threshold",
                json!({"sensor": 1, "low": 2.0, "high": 8.0}),
                seed,
            ),
            item(
                "hub/screen_brightness",
                "set_screen_brightness",
                json!({"value": 50}),
                seed,
            ),
        ]
    }

    #[test]
    fn topics_build_and_parse() {
        for k in [
            SyncTopic::Req,
            SyncTopic::Resp,
            SyncTopic::Ack,
            SyncTopic::Hint,
        ] {
            let t = k.topic("proximos", "hub-1");
            assert_eq!(parse_sync_topic(&t, "proximos"), Some(("hub-1", k)));
        }
        assert_eq!(
            SyncTopic::Req.topic("proximos", "h"),
            "proximos/h/config/sync/req"
        );
        assert_eq!(
            parse_sync_topic("proximos/h/config/sync/x", "proximos"),
            None
        );
        assert_eq!(parse_sync_topic("proximos/h/config", "proximos"), None);
        assert_eq!(parse_sync_topic("acme/h/config/sync/req", "proximos"), None);
        assert_eq!(
            parse_sync_topic("proximos//config/sync/req", "proximos"),
            None
        );
    }

    #[test]
    fn desired_verbs() {
        assert!(is_desired_verb("set_threshold"));
        assert!(is_desired_verb("set_sticker_config"));
        assert!(!is_desired_verb("set_network_config"));
        assert!(!is_desired_verb("reboot"));
        for v in DESIRED_VERBS {
            assert!(crate::registry::lookup(v).is_some(), "{v} not in registry");
        }
    }

    #[test]
    fn roundtrip() {
        let doc = DesiredDocument::new(7, items(1));
        let resp = SyncResponse::Document(doc.clone());
        let s = serde_json::to_string(&resp).unwrap();
        match serde_json::from_str::<SyncResponse>(&s).unwrap() {
            SyncResponse::Document(d) => {
                assert_eq!(d.gen, 7);
                assert_eq!(d.hash, doc.hash);
                assert_eq!(document_hash(&d.items), doc.hash);
            }
            other => panic!("{other:?}"),
        }
        let up: SyncResponse = serde_json::from_str(r#"{"status":"up_to_date","gen":3}"#).unwrap();
        assert!(matches!(up, SyncResponse::UpToDate { gen: 3 }));

        let req = SyncRequest {
            gen: 1,
            hash: "ab".into(),
            boot_id: "b".into(),
        };
        let back: SyncRequest =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(back, req);

        let ack: SyncAck = serde_json::from_str(
            r#"{"gen":7,"hash":"x","results":[{"key":"a","ok":true},
                {"key":"b","ok":false,"error":"bad"}]}"#,
        )
        .unwrap();
        assert_eq!(ack.results[0].error, None);
        assert_eq!(ack.results[1].error.as_deref(), Some("bad"));
        let hint: SyncHint = serde_json::from_str(r#"{"gen":9}"#).unwrap();
        assert_eq!(hint.gen, 9);

        let t: SyncTarget = serde_json::from_str(r#"{"kind":"node","dev_eui":"0011"}"#).unwrap();
        assert_eq!(
            t,
            SyncTarget::Node {
                dev_eui: "0011".into()
            }
        );
    }

    #[test]
    fn hash_stable_under_reorder_and_resign() {
        let a = items(1);
        let mut b = items(2); // different key, fresh nonces/signatures
        b.reverse();
        assert_ne!(a[0].request.signature, b[1].request.signature);
        assert_eq!(document_hash(&a), document_hash(&b));
        assert_eq!(document_hash(&a).len(), 64);
    }

    #[test]
    fn hash_changes_with_value() {
        let a = items(1);
        let mut b = items(1);
        b[1].request.params = json!({"value": 51});
        assert_ne!(document_hash(&a), document_hash(&b));
    }

    fn cert(signer: &str) -> Certificate {
        use crate::certificate::{sign_certificate, CertificatePayload};
        let payload = CertificatePayload {
            signer_id: signer.into(),
            full_name: "Alice Example".into(),
            role: "manager".into(),
            public_key_ed25519: "ab".repeat(32),
            permissions: [
                "set_threshold",
                "set_sensor_name",
                "set_sensor_location",
                "set_interval",
                "set_system_info_interval",
                "set_display_lines",
                "set_screen_brightness",
                "set_screen_timeout",
                "set_led_brightness",
                "set_buzzer_volume",
                "set_alarm_pattern",
                "set_lorawan_sensor_config",
                "beacon.manage",
                "level:read",
                "level:operate",
                "level:manage",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            issued_at: "2026-10-01T00:00:00Z".into(),
            expires_at: "2027-10-01T00:00:00Z".into(),
            issuer: "device_ca".into(),
        };
        let certificate_signature = sign_certificate(&[9; 32], &payload);
        Certificate {
            payload,
            certificate_signature,
        }
    }

    /// The HIL document shape: 22 Hub items by one editor.
    fn hil_doc() -> DesiredDocument {
        let c = cert("alice@hub.local");
        let items = (0..22u8)
            .map(|i| {
                let (verb, params) = if i < 8 {
                    (
                        "set_threshold",
                        json!({"line": i, "thresholds": {"critical_low": -30.0,
                        "warning_low": 2.0, "warning_high": 8.0, "critical_high": 30.0,
                        "hysteresis": 0.5}}),
                    )
                } else if i < 16 {
                    (
                        "set_sensor_name",
                        json!({"line": i - 8, "name": format!("Fridge {i}")}),
                    )
                } else {
                    ("set_screen_brightness", json!({"value": 50}))
                };
                let request =
                    build_request(verb, params, None, "alice@hub.local", &[7; 32], Some(&c))
                        .unwrap();
                SyncItem {
                    key: format!("hub/{verb}/{i}"),
                    target: SyncTarget::Hub,
                    command: verb.into(),
                    request,
                    editor: "alice@hub.local".into(),
                }
            })
            .collect();
        DesiredDocument::new(5, items)
    }

    #[test]
    fn hoisted_certificates_shrink_the_document_and_restore() {
        let inline = hil_doc();
        let inline_len = serde_json::to_vec(&inline).unwrap().len();
        let mut doc = inline.clone();
        doc.hoist_certificates();
        let body = serde_json::to_vec(&SyncResponse::Document(doc.clone())).unwrap();
        println!("22 items: inline {inline_len} B, hoisted {} B", body.len());
        assert!(
            body.len() * 2 < inline_len,
            "{} vs {inline_len}",
            body.len()
        );
        assert!(body.len() < MAX_RESP_BYTES);
        assert_eq!(doc.certificates.len(), 1);
        assert_eq!(doc.hash, inline.hash, "hoisting does not change the hash");
        let back: DesiredDocument =
            serde_json::from_slice(&serde_json::to_vec(&doc).unwrap()).unwrap();
        for (a, b) in back.items.iter().zip(&inline.items) {
            let full = back.full_request(a);
            assert_eq!(full.certificate, b.request.certificate);
            assert_eq!(full.signature, b.request.signature);
        }
    }

    #[test]
    fn a_second_certificate_for_the_same_signer_stays_inline() {
        let mut doc = hil_doc();
        let mut other = cert("alice@hub.local");
        other.payload.role = "admin".into();
        doc.items[3].request.certificate = Some(other.clone());
        doc.hoist_certificates();
        assert_eq!(doc.items[3].request.certificate, Some(other));
        assert!(doc.items[4].request.certificate.is_none());
    }
}
