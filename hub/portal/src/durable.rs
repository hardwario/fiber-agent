//! The one place that decides *delayed* versus *lost*.
//!
//! `portal.db` has eight connections on one file, and a full `VACUUM` holds
//! its write lock for the whole rewrite. Before this module every writer
//! answered a refused write the same way — `tracing::warn!` and carry on — so
//! routine maintenance discarded alarm rows, notification history and audit
//! entries (measured 2026-08-24: 1401 of 1542 write attempts refused during one
//! run). The rule this module implements is that data may be *delayed* —
//! buffered, retried, shown later — but never dropped.
//!
//! ## Spill where there is no caller to tell; retry where there is
//!
//! That distinction decides which helper a write site uses, and it is the
//! load-bearing design decision here:
//!
//! - A **background** writer — the persist thread, the notifier, the audit
//!   logger — has nobody to report a failure to. Its work vanishes silently, so
//!   it goes through [`SpillGate::write_or_spill`]: retry, and if that does not
//!   clear, append to the durable spill journal for replay. The helpers below
//!   are the portal-side row types that use it.
//! - A **request-scoped** writer — a settings form, a pairing POST — already
//!   surfaces its failure to the person who asked, who can retry. It needs only
//!   [`portal_storage::with_retry`] so an ordinary lock contention does not
//!   become a user-visible error in the first place.
//! - **Idempotent housekeeping** — expiring sessions, backfill bookkeeping —
//!   self-heals on the next tick. Retry only; a missed sweep is not lost data.
//!
//! ## Why the gate is not what makes this correct
//!
//! [`SpillGate`] lets a writer skip an attempt that is known to be doomed while
//! a rewrite runs. It is an optimisation. Correctness comes from the fallback:
//! a writer that races the phase transition blocks on `busy_timeout`, fails,
//! and spills anyway. There is deliberately no lock coupling the phase check to
//! the write.

use portal_storage::{
    Landed, NewAlarmEvent, NewAuditEntry, NewReportHistory, SpillGate, SpillKind, Storage,
    StorageError,
};

/// Insert an audit row, or spill it.
///
/// Audit rows are evidence: `plan/portal/sqlite-vacuum.md` §3.3 names them
/// among the things a clinician or auditor could miss, and they are never
/// pruned by design.
pub fn insert_audit(
    gate: &SpillGate,
    storage: &Storage,
    entry: &NewAuditEntry,
) -> Result<Landed, StorageError> {
    let payload = serde_json::to_vec(entry)?;
    gate.write_or_spill(SpillKind::Audit, &payload, || {
        storage.insert_audit(entry).map(|_| ())
    })
}

/// Insert a notification-delivery history row, or spill it.
pub fn insert_report_history(
    gate: &SpillGate,
    storage: &Storage,
    row: &NewReportHistory,
) -> Result<Landed, StorageError> {
    let payload = serde_json::to_vec(row)?;
    gate.write_or_spill(SpillKind::ReportHistory, &payload, || {
        storage.insert_report_history(row).map(|_| ())
    })
}

/// One liveness edge — a device falling silent, or coming back — resolved to
/// exactly the values the alarm row is built from.
///
/// Resolved *before* it reaches the journal, not at replay: `display_name` and
/// `display_location` come off the live state snapshot, and a device can be
/// forgotten between the edge and the replay. A row that replayed with an empty
/// name would read on `/alarms` as a different device from the one on `/nodes`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LivenessEdge {
    pub hostname: String,
    pub sensor_id: String,
    /// `true` opens an outage row; `false` stamps `cleared_at` on the open one.
    pub opening: bool,
    pub message: String,
    /// When the device actually changed verdict, not when the row committed —
    /// a replayed outage keeps the moment it began.
    pub at: String,
    pub display_name: String,
    pub display_location: Option<String>,
}

/// What [`write_liveness`] did with an edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivenessOutcome {
    /// A row was opened or cleared.
    Wrote,
    /// The table already said this; nothing to do. A second `WentOffline` with
    /// a row still open, or a `CameBack` with no outage of ours to close.
    Redundant,
    /// Deferred to the journal. The replay applies it and sends the nudge then
    /// — see `persist::drain_spill`.
    Spilled,
}

/// Apply one liveness edge. Idempotent in both directions, which is what makes
/// it legal to replay: the decision is taken against the *current* open alarm,
/// so applying an edge twice is a no-op the second time.
///
/// `Ok(true)` when a row was opened or cleared.
pub fn apply_liveness(storage: &Storage, edge: &LivenessEdge) -> Result<bool, StorageError> {
    let open = storage.active_alarm(&edge.hostname, &edge.sensor_id)?;
    match (edge.opening, open) {
        // A fresh outage: one row, warning severity, no value and no bound —
        // there is nothing measured here, only silence.
        (true, None) => {
            storage.insert_alarm_event(&NewAlarmEvent {
                hostname: edge.hostname.clone(),
                sensor_id: edge.sensor_id.clone(),
                sensor_name: Some(edge.display_name.clone()),
                sensor_location: edge.display_location.clone(),
                alarm_type: "OFFLINE".to_string(),
                severity: "warning".to_string(),
                from_state: Some("NORMAL".to_string()),
                to_state: Some("DISCONNECTED".to_string()),
                message: Some(edge.message.clone()),
                value: None,
                threshold: None,
                triggered_at: edge.at.clone(),
                cleared_at: None,
                acknowledged_at: None,
                acknowledged_by_user_id: None,
                acknowledged_by_username: None,
                // Device-side idempotency keys are for fPort-3 frames; this row
                // is the portal's own verdict.
                native_key: None,
            })?;
            Ok(true)
        }
        (true, Some(_)) => Ok(false),
        (false, Some(open)) => storage.clear_alarm(&open.id, &edge.at, None),
        (false, None) => Ok(false),
    }
}

/// Apply one liveness edge, or spill it.
///
/// Background writer with no second chance: the edge arrives on a `broadcast`
/// channel that the offline detector only fires on an actual change of verdict,
/// so nothing re-announces it. A refused `WentOffline` loses the outage; a
/// refused `CameBack` is worse — the rest of the portal answers "is this device
/// offline" with `cleared_at IS NULL`, so the device reads as offline forever.
pub fn write_liveness(
    gate: &SpillGate,
    storage: &Storage,
    edge: &LivenessEdge,
) -> Result<LivenessOutcome, StorageError> {
    let payload = serde_json::to_vec(edge)?;
    let mut wrote = false;
    let landed = gate.write_or_spill(SpillKind::Liveness, &payload, || {
        wrote = apply_liveness(storage, edge)?;
        Ok(())
    })?;
    Ok(match landed {
        Landed::Committed if wrote => LivenessOutcome::Wrote,
        Landed::Committed => LivenessOutcome::Redundant,
        Landed::Spilled(_) => LivenessOutcome::Spilled,
    })
}

/// Replay one spilled row that this module encoded.
///
/// The ingest (`Mqtt`) kind is replayed by `persist.rs`, which owns the decoder
/// and the alarm-engine side effects; this handles the two plain-row kinds.
pub fn replay_row(storage: &Storage, kind: SpillKind, payload: &[u8]) -> Result<(), StorageError> {
    match kind {
        SpillKind::Audit => {
            let entry: NewAuditEntry = serde_json::from_slice(payload)?;
            storage.insert_audit(&entry).map(|_| ())
        }
        SpillKind::ReportHistory => {
            let row: NewReportHistory = serde_json::from_slice(payload)?;
            storage.insert_report_history(&row).map(|_| ())
        }
        // Both are replayed by `persist.rs`: it owns the ingest decoder, and it
        // holds the nudge sender these kinds' side effects fire on.
        SpillKind::Mqtt | SpillKind::Liveness => {
            unreachable!("persist.rs replays the ingest and liveness kinds")
        }
    }
}

/// Encode a raw MQTT publish for the spill journal.
///
/// The ingest path stores the publish, not the parsed [`crate::state::IngestEvent`]:
/// replay re-runs `mqtt::routing::route_publish`, so there is exactly one
/// decoder in the tree and no serde derives on `fiber-protocol` /
/// `control_radio` wire types. Length-prefixed rather than JSON because the
/// payload is arbitrary bytes and must come back byte-exact.
pub fn encode_publish(topic: &str, payload: &[u8]) -> Vec<u8> {
    let topic = topic.as_bytes();
    let mut out = Vec::with_capacity(4 + topic.len() + payload.len());
    out.extend_from_slice(&(topic.len() as u32).to_le_bytes());
    out.extend_from_slice(topic);
    out.extend_from_slice(payload);
    out
}

/// Inverse of [`encode_publish`]. `None` for a truncated or malformed row,
/// which the caller must treat as an error rather than a skip.
pub fn decode_publish(bytes: &[u8]) -> Option<(String, Vec<u8>)> {
    let len_bytes: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    let topic_len = u32::from_le_bytes(len_bytes) as usize;
    let topic = bytes.get(4..4 + topic_len)?;
    let payload = bytes.get(4 + topic_len..)?;
    Some((String::from_utf8(topic.to_vec()).ok()?, payload.to_vec()))
}

/// A journal in `data_dir` for tests that construct a service directly.
///
/// Tests open plain (unkeyed) databases, same as every other `Storage::open`
/// call in the test suite.
#[cfg(test)]
pub fn test_gate(data_dir: &str) -> std::sync::Arc<SpillGate> {
    std::fs::create_dir_all(data_dir).expect("create data dir for the test spill journal");
    std::sync::Arc::new(
        SpillGate::open(std::path::Path::new(data_dir), None).expect("open test spill journal"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_publish_round_trips_byte_exactly() {
        let payload = vec![0u8, 255, 128, b'{', b'}'];
        let encoded = encode_publish("proximos/hub-1/sensors/aggregated", &payload);
        let (topic, decoded) = decode_publish(&encoded).expect("decode");

        assert_eq!(topic, "proximos/hub-1/sensors/aggregated");
        assert_eq!(
            decoded, payload,
            "the replay must see the same bytes the broker delivered"
        );
    }

    #[test]
    fn an_empty_payload_round_trips() {
        let encoded = encode_publish("proximos/hub-1/status", &[]);
        let (topic, decoded) = decode_publish(&encoded).expect("decode");
        assert_eq!(topic, "proximos/hub-1/status");
        assert!(
            decoded.is_empty(),
            "a tombstone is a real, routable publish"
        );
    }

    #[test]
    fn a_truncated_row_is_refused_rather_than_guessed() {
        let encoded = encode_publish("proximos/hub-1/status", b"ok");
        assert!(
            decode_publish(&encoded[..3]).is_none(),
            "a short row must not decode into a plausible-looking publish"
        );

        let mut lying = encoded.clone();
        lying[0] = 200; // topic length past the end of the buffer
        assert!(decode_publish(&lying).is_none());
    }
}
