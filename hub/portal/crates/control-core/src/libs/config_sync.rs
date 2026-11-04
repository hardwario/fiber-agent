//! Hub side of the Portal -> Hub configuration pull
//! (plan/portal/config_sync.md §5–§8, phase S3; decision #43).
//!
//! [`SyncClient`] holds the last applied `{gen, hash}`, persisted under
//! `/data`, builds the [`SyncRequest`] the monitor publishes (boot, broker
//! reconnect, every [`REQUEST_PERIOD`], and on a hint newer than applied),
//! decides what to do with a [`SyncResponse`], and drives the per-item
//! apply into a [`SyncAck`]. The MQTT and the per-item verify/execute live
//! in `mqtt/monitor.rs`; this module is pure apart from the state file, so
//! the rules are unit-tested here.
//!
//! The Portal always wins (§8): a document is applied item by item through
//! the same setters as a live command, overwriting any local drift silently.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fiber_protocol::config_sync::{
    document_hash, is_desired_verb, DesiredDocument, SyncAck, SyncHint, SyncItem, SyncItemResult,
    SyncRequest, SyncResponse, SyncTarget,
};
use serde::{Deserialize, Serialize};

/// Where the applied generation lives; survives reboot and RAUC slot swaps. Under
/// /data/fiber because fiber.service (ProtectSystem=strict) may write only there.
pub const DEFAULT_STATE_PATH: &str = "/data/fiber/config_sync/state.json";

/// Periodic request interval (§5, Q13).
pub const REQUEST_PERIOD: Duration = Duration::from_secs(600);

/// First wait for an answer before the request is sent again; doubles per
/// unanswered request up to [`REQUEST_PERIOD`]. A request in flight also
/// absorbs reconnect and hint triggers, so a flapping link or a burst of
/// hints never turns into a request storm (HIL 2026-10-15).
pub const RETRY_MIN: Duration = Duration::from_secs(30);

const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";

/// Error text for Node items until phase S4 delivers them.
pub const NODE_DEFERRED: &str = "deferred: node items are delivered in phase S4";

/// Last applied document generation and hash. `gen` 0 + empty hash means
/// "never synced" (fresh `/data`), which the Portal answers with a document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedState {
    pub gen: u64,
    pub hash: String,
}

/// What to do with a response.
#[derive(Debug)]
pub enum Plan {
    /// Nothing to apply.
    UpToDate,
    /// Refused as a whole; logged, no ack.
    Rejected(String),
    /// Apply this document and ack it.
    Apply(DesiredDocument),
}

pub struct SyncClient {
    path: PathBuf,
    boot_id: String,
    applied: AppliedState,
    /// When the unanswered request was sent.
    in_flight: Option<Instant>,
    retry: Duration,
}

impl SyncClient {
    /// Load state from `path`; a missing or unreadable file starts at gen 0.
    pub fn load(path: impl Into<PathBuf>, boot_id: impl Into<String>) -> Self {
        let path = path.into();
        let applied = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            path,
            boot_id: boot_id.into(),
            applied,
            in_flight: None,
            retry: RETRY_MIN,
        }
    }

    /// The production client: [`DEFAULT_STATE_PATH`] and the kernel boot id.
    pub fn open_default() -> Self {
        let boot_id = fs::read_to_string(BOOT_ID_PATH)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        Self::load(DEFAULT_STATE_PATH, boot_id)
    }

    pub fn applied(&self) -> &AppliedState {
        &self.applied
    }

    pub fn request(&self) -> SyncRequest {
        SyncRequest {
            gen: self.applied.gen,
            hash: self.applied.hash.clone(),
            boot_id: self.boot_id.clone(),
        }
    }

    /// Whether a request may go out now: none is in flight, or the one in
    /// flight has waited its retry interval.
    pub fn request_due(&self, now: Instant) -> bool {
        self.in_flight
            .is_none_or(|t| now.duration_since(t) >= self.retry)
    }

    /// A request is unanswered.
    pub fn awaiting(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Record a sent request; a resend after no answer doubles the wait.
    pub fn mark_requested(&mut self, now: Instant) {
        if self.in_flight.is_some() {
            self.retry = (self.retry * 2).min(REQUEST_PERIOD);
        }
        self.in_flight = Some(now);
    }

    /// Any response arrived: nothing in flight, back to the short wait.
    pub fn mark_answered(&mut self) {
        self.in_flight = None;
        self.retry = RETRY_MIN;
    }

    /// A hint at or below the applied generation carries nothing new.
    pub fn wants_request_on_hint(&self, hint: &SyncHint) -> bool {
        hint.gen > self.applied.gen
    }

    pub fn plan(&self, resp: SyncResponse) -> Plan {
        match resp {
            SyncResponse::UpToDate { .. } => Plan::UpToDate,
            SyncResponse::Document(doc) => {
                if doc.gen < self.applied.gen {
                    return Plan::Rejected(format!(
                        "document gen {} is lower than applied {}",
                        doc.gen, self.applied.gen
                    ));
                }
                if doc.gen == self.applied.gen && doc.hash == self.applied.hash {
                    // A redelivered copy of what is already applied.
                    return Plan::UpToDate;
                }
                let computed = document_hash(&doc.items);
                if computed != doc.hash {
                    return Plan::Rejected(format!(
                        "document hash {} does not match its items ({computed})",
                        doc.hash
                    ));
                }
                Plan::Apply(doc)
            }
        }
    }

    /// Apply `doc` item by item through `apply_item` (verify + execute) and
    /// build the ack. The generation is persisted only when every Hub and
    /// Beacon item succeeded, so a failed item is retried on the next
    /// request (a hint for the same gen is then still honoured). Node items
    /// are reported as [`NODE_DEFERRED`] and do not block persistence.
    pub fn apply<F>(&mut self, doc: &DesiredDocument, apply_item: F) -> SyncAck
    where
        F: FnMut(&SyncItem) -> Result<(), String>,
    {
        self.apply_full(doc, &[], apply_item, |_| Ok(()))
    }

    /// [`Self::apply`], then prune: the Portal leaves disabled bands out of
    /// the document, so a band the Hub holds (`held_keys`, read before the
    /// apply) that is of a synced kind ([`is_prunable_key`]) and absent from
    /// the full document is deleted through `delete_key`, silently (drift,
    /// §8), and reported in the ack under its key (`ok`, or the error).
    pub fn apply_full<F, D>(
        &mut self,
        doc: &DesiredDocument,
        held_keys: &[String],
        mut apply_item: F,
        mut delete_key: D,
    ) -> SyncAck
    where
        F: FnMut(&SyncItem) -> Result<(), String>,
        D: FnMut(&str) -> Result<(), String>,
    {
        let mut all_ok = true;
        let mut results: Vec<SyncItemResult> = doc
            .items
            .iter()
            .map(|item| {
                let outcome = if matches!(item.target, SyncTarget::Node { .. }) {
                    return SyncItemResult {
                        key: item.key.clone(),
                        ok: false,
                        error: Some(NODE_DEFERRED.to_string()),
                    };
                } else if !is_desired_verb(&item.command) {
                    Err(format!("{} is not a desired-state verb", item.command))
                } else if item.command != item.request.command_type {
                    Err(format!(
                        "item command {} differs from signed {}",
                        item.command, item.request.command_type
                    ))
                } else {
                    apply_item(item)
                };
                if outcome.is_err() {
                    all_ok = false;
                }
                SyncItemResult {
                    key: item.key.clone(),
                    ok: outcome.is_ok(),
                    error: outcome.err(),
                }
            })
            .collect();
        for key in held_keys {
            if !is_prunable_key(key) || doc.items.iter().any(|i| &i.key == key) {
                continue;
            }
            let outcome = delete_key(key);
            if outcome.is_err() {
                all_ok = false;
            }
            results.push(SyncItemResult {
                key: key.clone(),
                ok: outcome.is_ok(),
                error: outcome.err(),
            });
        }
        if all_ok {
            let next = AppliedState {
                gen: doc.gen,
                hash: doc.hash.clone(),
            };
            if let Err(e) = self.persist(&next) {
                eprintln!("[ConfigSync] failed to persist applied state: {e}");
            }
            self.applied = next;
        }
        SyncAck {
            gen: doc.gen,
            hash: doc.hash.clone(),
            results,
        }
    }

    fn persist(&self, state: &AppliedState) -> std::io::Result<()> {
        write_atomic(&self.path, &serde_json::to_vec(state)?)
    }
}

/// Band keys the Hub deletes when a full document no longer holds them:
/// a wired line's band `hub/threshold/<line>` and a Beacon field band
/// `beacon/<MAC>/threshold/<field>`. Nothing else is ever pruned.
pub fn is_prunable_key(key: &str) -> bool {
    if let Some(line) = key.strip_prefix("hub/threshold/") {
        return line.parse::<u8>().is_ok();
    }
    if let Some(rest) = key.strip_prefix("beacon/") {
        let parts: Vec<&str> = rest.split('/').collect();
        return parts.len() == 3
            && !parts[0].is_empty()
            && parts[1] == "threshold"
            && !parts[2].is_empty();
    }
    false
}

/// What a prunable key deletes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrunedBand {
    Line(u8),
    BeaconField { mac: String, field: String },
}

pub fn parse_prunable_key(key: &str) -> Option<PrunedBand> {
    if !is_prunable_key(key) {
        return None;
    }
    if let Some(line) = key.strip_prefix("hub/threshold/") {
        return line.parse().ok().map(PrunedBand::Line);
    }
    let mut parts = key.strip_prefix("beacon/")?.split('/');
    let mac = parts.next()?.to_string();
    parts.next()?;
    let field = parts.next()?.to_string();
    Some(PrunedBand::BeaconField { mac, field })
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fiber_protocol::envelope::build_request;
    use serde_json::json;

    fn item(key: &str, target: SyncTarget, verb: &str, value: i64) -> SyncItem {
        let request =
            build_request(verb, json!({"value": value}), None, "alice", &[7; 32], None).unwrap();
        SyncItem {
            key: key.into(),
            target,
            command: verb.into(),
            request,
            editor: "alice".into(),
        }
    }

    fn doc(gen: u64, items: Vec<SyncItem>) -> DesiredDocument {
        DesiredDocument::new(gen, items)
    }

    fn client(dir: &tempfile::TempDir) -> SyncClient {
        SyncClient::load(dir.path().join("sync/state.json"), "boot-1")
    }

    #[test]
    fn fresh_client_requests_gen_zero_with_boot_id() {
        let dir = tempfile::tempdir().unwrap();
        let req = client(&dir).request();
        assert_eq!(req.gen, 0);
        assert_eq!(req.hash, "");
        assert_eq!(req.boot_id, "boot-1");
    }

    #[test]
    fn lower_gen_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = client(&dir);
        let d5 = doc(
            5,
            vec![item("hub/a", SyncTarget::Hub, "set_led_brightness", 1)],
        );
        c.apply(&d5, |_| Ok(()));
        let d4 = doc(
            4,
            vec![item("hub/a", SyncTarget::Hub, "set_led_brightness", 2)],
        );
        assert!(matches!(
            c.plan(SyncResponse::Document(d4)),
            Plan::Rejected(_)
        ));
    }

    #[test]
    fn tampered_hash_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut d = doc(
            1,
            vec![item("hub/a", SyncTarget::Hub, "set_led_brightness", 1)],
        );
        d.hash = "00".into();
        assert!(matches!(
            client(&dir).plan(SyncResponse::Document(d)),
            Plan::Rejected(_)
        ));
    }

    #[test]
    fn up_to_date_and_redelivered_document_are_no_ops() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = client(&dir);
        assert!(matches!(
            c.plan(SyncResponse::UpToDate { gen: 0 }),
            Plan::UpToDate
        ));
        let d = doc(
            3,
            vec![item("hub/a", SyncTarget::Hub, "set_led_brightness", 1)],
        );
        c.apply(&d, |_| Ok(()));
        let mut calls = 0;
        match c.plan(SyncResponse::Document(d.clone())) {
            Plan::UpToDate => {}
            other => panic!("expected UpToDate, got {other:?}"),
        }
        // Nothing applied for an up-to-date answer.
        if let Plan::Apply(d) = c.plan(SyncResponse::UpToDate { gen: 3 }) {
            c.apply(&d, |_| {
                calls += 1;
                Ok(())
            });
        }
        assert_eq!(calls, 0);
    }

    #[test]
    fn a_bad_item_errors_while_the_others_apply_and_gen_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = client(&dir);
        let d = doc(
            2,
            vec![
                item("hub/a", SyncTarget::Hub, "set_led_brightness", 1),
                item("hub/b", SyncTarget::Hub, "set_buzzer_volume", 2),
                item(
                    "beacon/AA/label",
                    SyncTarget::Beacon { mac: "AA".into() },
                    "set_device_label",
                    3,
                ),
            ],
        );
        let mut applied = Vec::new();
        let ack = c.apply(&d, |i| {
            if i.key == "hub/b" {
                Err("Signature verification failed: bad".into())
            } else {
                applied.push(i.key.clone());
                Ok(())
            }
        });
        assert_eq!(applied, vec!["hub/a", "beacon/AA/label"]);
        assert!(ack.results[0].ok && ack.results[2].ok);
        assert!(!ack.results[1].ok);
        assert!(ack.results[1]
            .error
            .as_deref()
            .unwrap()
            .contains("Signature"));
        // Not persisted: the failed item is retried on the next request.
        assert_eq!(c.applied().gen, 0);
        assert!(c.wants_request_on_hint(&SyncHint { gen: 2 }));
    }

    #[test]
    fn node_items_are_deferred_without_blocking_the_gen() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = client(&dir);
        let d = doc(
            4,
            vec![
                item("hub/a", SyncTarget::Hub, "set_led_brightness", 1),
                item(
                    "node/01/threshold/t",
                    SyncTarget::Node {
                        dev_eui: "01".into(),
                    },
                    "set_lorawan_field_threshold",
                    1,
                ),
            ],
        );
        let ack = c.apply(&d, |i| {
            assert_eq!(i.key, "hub/a");
            Ok(())
        });
        assert_eq!(ack.results[1].error.as_deref(), Some(NODE_DEFERRED));
        assert_eq!(c.applied().gen, 4);
    }

    #[test]
    fn non_desired_or_mismatched_verbs_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = client(&dir);
        let mut mismatched = item("hub/x", SyncTarget::Hub, "set_led_brightness", 1);
        mismatched.command = "set_buzzer_volume".into();
        let d = doc(
            1,
            vec![
                item("hub/n", SyncTarget::Hub, "set_network_config", 1),
                mismatched,
            ],
        );
        let ack = c.apply(&d, |_| panic!("must not reach the applier"));
        assert!(ack.results.iter().all(|r| !r.ok));
    }

    #[test]
    fn persisted_gen_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let d = doc(
            9,
            vec![item("hub/a", SyncTarget::Hub, "set_led_brightness", 1)],
        );
        client(&dir).apply(&d, |_| Ok(()));
        let reloaded = SyncClient::load(dir.path().join("sync/state.json"), "boot-2");
        let req = reloaded.request();
        assert_eq!(req.gen, 9);
        assert_eq!(req.hash, d.hash);
        assert_eq!(req.boot_id, "boot-2");
    }

    #[test]
    fn held_bands_absent_from_the_document_are_deleted_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = client(&dir);
        let d = doc(
            3,
            vec![item("hub/threshold/1", SyncTarget::Hub, "set_threshold", 1)],
        );
        let held: Vec<String> = [
            "hub/threshold/1",
            "hub/threshold/2",
            "beacon/AA:BB/threshold/temperature",
            "hub/screen_brightness",
            "node/01/threshold/t",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut deleted = Vec::new();
        let ack = c.apply_full(
            &d,
            &held,
            |_| Ok(()),
            |k| {
                deleted.push(k.to_string());
                Ok(())
            },
        );
        assert_eq!(
            deleted,
            vec!["hub/threshold/2", "beacon/AA:BB/threshold/temperature"]
        );
        let keys: Vec<&str> = ack.results.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "hub/threshold/1",
                "hub/threshold/2",
                "beacon/AA:BB/threshold/temperature"
            ]
        );
        assert!(ack.results.iter().all(|r| r.ok));
        assert_eq!(c.applied().gen, 3);
    }

    #[test]
    fn a_failed_delete_is_reported_and_keeps_the_gen() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = client(&dir);
        let d = doc(2, vec![]);
        let ack = c.apply_full(
            &d,
            &["hub/threshold/4".to_string()],
            |_| Ok(()),
            |_| Err("no config".into()),
        );
        assert!(!ack.results[0].ok);
        assert_eq!(c.applied().gen, 0);
    }

    #[test]
    fn prunable_keys_parse() {
        assert_eq!(
            parse_prunable_key("hub/threshold/3"),
            Some(PrunedBand::Line(3))
        );
        assert_eq!(
            parse_prunable_key("beacon/AA:BB/threshold/humidity"),
            Some(PrunedBand::BeaconField {
                mac: "AA:BB".into(),
                field: "humidity".into()
            })
        );
        for k in [
            "hub/threshold/x",
            "hub/sensor_name/1",
            "beacon/AA/label",
            "node/1/threshold/t",
        ] {
            assert!(parse_prunable_key(k).is_none(), "{k}");
        }
    }

    #[test]
    fn requests_are_paced_while_unanswered() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = client(&dir);
        let t0 = Instant::now();
        assert!(c.request_due(t0));
        c.mark_requested(t0);
        // Reconnects and hints within the wait are absorbed.
        assert!(!c.request_due(t0 + Duration::from_secs(1)));
        assert!(!c.request_due(t0 + Duration::from_secs(29)));
        let t1 = t0 + RETRY_MIN;
        assert!(c.request_due(t1));
        c.mark_requested(t1);
        assert!(!c.request_due(t1 + Duration::from_secs(59)));
        assert!(c.request_due(t1 + Duration::from_secs(60)));
        // Backoff is capped at the periodic interval.
        let mut t = t1;
        for _ in 0..10 {
            t += REQUEST_PERIOD;
            c.mark_requested(t);
        }
        assert!(c.request_due(t + REQUEST_PERIOD));
        c.mark_answered();
        assert!(!c.awaiting());
        assert!(c.request_due(t));
        c.mark_requested(t);
        assert!(c.request_due(t + RETRY_MIN), "an answer resets the wait");
    }

    #[test]
    fn hint_triggers_request_only_when_newer() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = client(&dir);
        assert!(c.wants_request_on_hint(&SyncHint { gen: 1 }));
        c.apply(
            &doc(
                5,
                vec![item("hub/a", SyncTarget::Hub, "set_led_brightness", 1)],
            ),
            |_| Ok(()),
        );
        assert!(!c.wants_request_on_hint(&SyncHint { gen: 5 }));
        assert!(!c.wants_request_on_hint(&SyncHint { gen: 3 }));
        assert!(c.wants_request_on_hint(&SyncHint { gen: 6 }));
    }
}
