//! Orphaned alarms: open rows whose device, Hub or rule no longer exists
//! (plan/portal/monitoring.md, amendment 2026-10-14).
//!
//! An alarm ends when a reading says the value is back. A row whose device
//! was removed, whose Hub is gone or was renamed, or whose bound was removed
//! or switched off never gets that reading, so it stayed Active for good
//! (seen on the bench Hub: removed Beacons, and every row of
//! `fiber-4e970f4a` after the Hub came back as `proximos-4e970f4a`).
//!
//! [`classify`] decides per row from an [`Inventory`] of what exists now;
//! [`sweep`] applies the verdicts: a renamed Hub's rows move to the new
//! hostname, everything else orphaned is closed with an `end_reason` (V37)
//! and an audit entry. No notification goes out: nothing was measured.
//!
//! The sweep is conservative where the live registry is incomplete. The
//! registry is rebuilt from retained messages after a start, so "not in the
//! snapshot" alone never closes anything: a Hub must also be unpaired and
//! no cluster member, and a device counts as removed only on an online Hub
//! that lists devices of that kind. Running it again changes nothing.

use std::collections::{HashMap, HashSet};

use portal_storage::{AlarmEvent, NewAuditEntry, Storage, StorageError};
use portal_storage::{THRESHOLD_SCOPE_EYE, THRESHOLD_SCOPE_NODE};

use super::{alarm_target, AlarmTarget};
use crate::state::PortalState;

/// Why the portal closed an alarm itself; stored in `alarm_events.end_reason`
/// and shown as `alarms.end_reason.<as_str>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    DeviceRemoved,
    HubRemoved,
    /// The Hub came back under another hostname that already holds an open
    /// alarm for the same sensor; the old row is the duplicate.
    HubRenamed,
    RuleRemoved,
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            EndReason::DeviceRemoved => "device_removed",
            EndReason::HubRemoved => "hub_removed",
            EndReason::HubRenamed => "hub_renamed",
            EndReason::RuleRemoved => "rule_removed",
        }
    }
}

/// What to do with one open alarm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Keep,
    /// Move the row to this (renamed) Hub hostname.
    Rekey(String),
    Close(EndReason),
}

/// One Hub of the live registry.
#[derive(Debug, Clone, Default)]
pub struct LiveHub {
    pub hostname: String,
    pub online: bool,
    pub has_nodes: bool,
    pub has_beacons: bool,
}

/// What exists now, for [`classify`].
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    pub live_hubs: Vec<LiveHub>,
    /// Lowercase hostnames the portal still holds a pairing or a cluster row
    /// for, whether or not they are in the live registry.
    pub persisted_hubs: HashSet<String>,
    /// Lowercase node keys any live Hub (or the cluster) lists.
    pub nodes: HashSet<String>,
    /// Uppercase MACs any live Hub (or the cluster) lists.
    pub beacons: HashSet<String>,
    /// Per `(scope, entity)`: the labels of the fields that have a rule, an
    /// enabled row with at least one bound. Entities missing here are not
    /// judged (nothing was loaded for them).
    pub rule_labels: HashMap<(String, String), HashSet<String>>,
}

impl Inventory {
    fn live(&self, hostname: &str) -> Option<&LiveHub> {
        self.live_hubs
            .iter()
            .find(|h| h.hostname.eq_ignore_ascii_case(hostname.trim()))
    }
}

/// The serial a Hub hostname ends in (`fiber-4e970f4a`, `proximos-4e970f4a`):
/// the part after the last `-`, when it is at least six hex digits. The
/// hostname scheme changed (decision #17/#21), the serial did not.
pub fn hub_serial(hostname: &str) -> Option<String> {
    let (_, tail) = hostname.trim().rsplit_once('-')?;
    (tail.len() >= 6 && tail.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| tail.to_ascii_lowercase())
}

/// The label the engine keys a value alarm by, for one stored field name.
fn field_label(scope: &str, field: &str) -> String {
    let label = if scope == THRESHOLD_SCOPE_EYE {
        fiber_protocol::fields::eye::eye_field(field).map(|f| f.label)
    } else {
        fiber_protocol::fields::lorawan::lorawan_field(field).map(|f| f.label)
    };
    label.map_or_else(|| field.to_string(), str::to_string)
}

/// The value-alarm label of a Node or Beacon alarm, `None` for every other
/// class (native fPort-3, liveness, configuration warning, battery flag).
fn value_label(alarm: &AlarmEvent) -> Option<(&'static str, String, String)> {
    if !matches!(alarm.alarm_type.as_str(), "HIGH" | "LOW") {
        return None;
    }
    match alarm_target(&alarm.sensor_id) {
        AlarmTarget::Node(key) => {
            let prefix = portal_domain::node_sensor_prefix(&key);
            let rest = alarm.sensor_id.get(prefix.len()..)?;
            if rest.is_empty() || rest.contains(':') || rest == "liveness" {
                return None;
            }
            Some((THRESHOLD_SCOPE_NODE, key.to_lowercase(), rest.to_string()))
        }
        AlarmTarget::Beacon(mac) => {
            let label = alarm.sensor_id.rsplit_once(':')?.1;
            // Only the fields the portal grades itself have a rule; the
            // battery flag comes graded from the Hub.
            let graded = fiber_protocol::fields::eye::eye_registry()
                .iter()
                .any(|f| f.thresholdable && f.label == label);
            graded.then(|| (THRESHOLD_SCOPE_EYE, mac.to_uppercase(), label.to_string()))
        }
        AlarmTarget::Line(_) | AlarmTarget::Hub => None,
    }
}

/// The verdict for one open alarm.
pub fn classify(alarm: &AlarmEvent, inv: &Inventory) -> Verdict {
    let hub = match inv.live(&alarm.hostname) {
        Some(hub) => hub,
        None => {
            // A renamed Hub: one live Hub under another name, same serial.
            if let Some(serial) = hub_serial(&alarm.hostname) {
                let mut same = inv.live_hubs.iter().filter(|h| {
                    hub_serial(&h.hostname).as_deref() == Some(serial.as_str())
                        && !h.hostname.eq_ignore_ascii_case(alarm.hostname.trim())
                });
                if let (Some(target), None) = (same.next(), same.next()) {
                    return Verdict::Rekey(target.hostname.clone());
                }
            }
            if inv
                .persisted_hubs
                .contains(&alarm.hostname.trim().to_ascii_lowercase())
            {
                // Paired but silent since the start: its state is unknown,
                // not gone (decisions.md #30d).
                return Verdict::Keep;
            }
            return Verdict::Close(EndReason::HubRemoved);
        }
    };

    let present = match alarm_target(&alarm.sensor_id) {
        AlarmTarget::Node(key) => {
            let known = inv.nodes.contains(&key.to_lowercase());
            if !known {
                return if hub.online && hub.has_nodes {
                    Verdict::Close(EndReason::DeviceRemoved)
                } else {
                    Verdict::Keep
                };
            }
            true
        }
        AlarmTarget::Beacon(mac) => {
            let known = inv.beacons.contains(&mac.to_uppercase());
            if !known {
                return if hub.online && hub.has_beacons {
                    Verdict::Close(EndReason::DeviceRemoved)
                } else {
                    Verdict::Keep
                };
            }
            true
        }
        AlarmTarget::Line(_) | AlarmTarget::Hub => false,
    };

    if present {
        if let Some((scope, entity, label)) = value_label(alarm) {
            if let Some(labels) = inv.rule_labels.get(&(scope.to_string(), entity)) {
                if !labels.contains(&label) {
                    return Verdict::Close(EndReason::RuleRemoved);
                }
            }
        }
    }
    Verdict::Keep
}

/// Builds the [`Inventory`] for `alarms` from the live snapshot and storage.
pub fn inventory(
    storage: &Storage,
    snapshot: &PortalState,
    alarms: &[AlarmEvent],
) -> Result<Inventory, StorageError> {
    let mut inv = Inventory::default();
    for dev in snapshot.devices.values() {
        inv.live_hubs.push(LiveHub {
            hostname: dev.hostname.trim().to_string(),
            online: dev.online,
            has_nodes: !dev.nodes.is_empty(),
            has_beacons: !dev.ble_sensors.is_empty(),
        });
        inv.nodes.extend(dev.nodes.keys().map(|k| k.to_lowercase()));
        inv.beacons
            .extend(dev.ble_sensors.keys().map(|m| m.trim().to_uppercase()));
    }

    let mut hosts: HashSet<String> = HashSet::new();
    let mut entities: HashSet<(&'static str, String)> = HashSet::new();
    for a in alarms {
        hosts.insert(a.hostname.trim().to_string());
        match alarm_target(&a.sensor_id) {
            AlarmTarget::Node(key) => {
                let key = key.to_lowercase();
                if !inv.nodes.contains(&key) && snapshot.hub_of_node(&key).is_some() {
                    inv.nodes.insert(key.clone());
                }
            }
            AlarmTarget::Beacon(mac) => {
                let mac = mac.to_uppercase();
                if !inv.beacons.contains(&mac) && snapshot.hub_of_beacon(&mac).is_some() {
                    inv.beacons.insert(mac.clone());
                }
            }
            _ => {}
        }
        if let Some((scope, entity, _)) = value_label(a) {
            entities.insert((scope, entity));
        }
    }
    for host in hosts {
        let paired = storage.get_device_identity(&host)?.is_some()
            || storage.get_cluster_client(&host)?.is_some();
        if paired {
            inv.persisted_hubs.insert(host.to_ascii_lowercase());
        }
    }
    let live: HashSet<String> = inv
        .live_hubs
        .iter()
        .map(|h| h.hostname.to_ascii_lowercase())
        .collect();
    for (scope, entity) in entities {
        let labels = storage
            .field_thresholds_any_hub(scope, &entity)?
            .into_iter()
            .filter(|t| {
                // Only rows under a live Hub count: a renamed Hub's rows stay
                // under the old hostname, which nothing grades against.
                live.contains(&t.hostname.trim().to_ascii_lowercase())
                    && t.enabled
                    && [
                        t.critical_low,
                        t.warning_low,
                        t.warning_high,
                        t.critical_high,
                    ]
                    .iter()
                    .any(Option::is_some)
                    && (scope != THRESHOLD_SCOPE_NODE
                        || crate::threshold_defaults::node_field_is_graded(&t.field))
            })
            .map(|t| field_label(scope, &t.field))
            .collect();
        inv.rule_labels.insert((scope.to_string(), entity), labels);
    }
    Ok(inv)
}

/// What one [`sweep`] changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub closed: Vec<(String, EndReason)>,
    pub rekeyed: Vec<(String, String)>,
    /// Hostnames whose rows changed, for the SSE nudge.
    pub hosts: Vec<String>,
}

/// Classifies every open alarm and applies the verdicts. Idempotent.
pub fn sweep(
    storage: &Storage,
    snapshot: &PortalState,
    now_iso: &str,
) -> Result<SweepReport, StorageError> {
    let alarms = storage.all_active_alarms()?;
    let inv = inventory(storage, snapshot, &alarms)?;
    let mut report = SweepReport::default();
    // Rekeys first, against the rows the new hostname already holds.
    let mut open_keys: HashSet<(String, String)> = alarms
        .iter()
        .map(|a| (a.hostname.to_ascii_lowercase(), a.sensor_id.clone()))
        .collect();
    for alarm in &alarms {
        let verdict = classify(alarm, &inv);
        let (verdict, new_host) = match verdict {
            Verdict::Keep => continue,
            Verdict::Rekey(to) => {
                let key = (to.to_ascii_lowercase(), alarm.sensor_id.clone());
                if open_keys.contains(&key) {
                    (Verdict::Close(EndReason::HubRenamed), Some(to))
                } else {
                    open_keys.insert(key);
                    (Verdict::Rekey(to.clone()), Some(to))
                }
            }
            close => (close, None),
        };
        match verdict {
            Verdict::Rekey(to) => {
                if storage.rekey_alarm(&alarm.id, &to)? {
                    audit(storage, "alarm_rekey", alarm, None, Some(&to))?;
                    report.rekeyed.push((alarm.id.clone(), to.clone()));
                    report.hosts.push(to.clone());
                    // The moved row is judged again under its new Hub in the
                    // same pass: its device and rule must exist there too.
                    let mut moved = alarm.clone();
                    moved.hostname = to;
                    if let Verdict::Close(reason) = classify(&moved, &inv) {
                        if storage.close_alarm_with_reason(&moved.id, now_iso, reason.as_str())? {
                            audit(storage, "alarm_auto_close", &moved, Some(reason), None)?;
                            report.closed.push((moved.id.clone(), reason));
                        }
                    }
                }
            }
            Verdict::Close(reason) => {
                if storage.close_alarm_with_reason(&alarm.id, now_iso, reason.as_str())? {
                    audit(
                        storage,
                        "alarm_auto_close",
                        alarm,
                        Some(reason),
                        new_host.as_deref(),
                    )?;
                    report.closed.push((alarm.id.clone(), reason));
                    report.hosts.push(alarm.hostname.clone());
                }
            }
            Verdict::Keep => {}
        }
    }
    report.hosts.sort();
    report.hosts.dedup();
    Ok(report)
}

/// Closes the open alarms of one removed Node or Beacon right away, with
/// `device_removed` and an audit entry each. Returns how many it closed.
pub fn close_device_alarms(
    storage: &Storage,
    hostname: &str,
    target: &AlarmTarget,
    now_iso: &str,
) -> Result<usize, StorageError> {
    let mut closed = 0;
    for alarm in storage.active_alarms_any_case(hostname)? {
        let same = match (alarm_target(&alarm.sensor_id), target) {
            (AlarmTarget::Node(a), AlarmTarget::Node(b)) => a.eq_ignore_ascii_case(b),
            (AlarmTarget::Beacon(a), AlarmTarget::Beacon(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        };
        if same
            && storage.close_alarm_with_reason(
                &alarm.id,
                now_iso,
                EndReason::DeviceRemoved.as_str(),
            )?
        {
            audit(
                storage,
                "alarm_auto_close",
                &alarm,
                Some(EndReason::DeviceRemoved),
                None,
            )?;
            closed += 1;
        }
    }
    Ok(closed)
}

fn audit(
    storage: &Storage,
    event_type: &str,
    alarm: &AlarmEvent,
    reason: Option<EndReason>,
    hostname_to: Option<&str>,
) -> Result<(), StorageError> {
    let details = serde_json::json!({
        "alarm_id": alarm.id,
        "hostname": alarm.hostname,
        "sensor_id": alarm.sensor_id,
        "end_reason": reason.map(EndReason::as_str),
        "hostname_to": hostname_to,
    });
    storage.insert_audit(&NewAuditEntry {
        event_type: event_type.to_string(),
        username: Some("system".to_string()),
        user_id: None,
        ip: None,
        target: Some(alarm.sensor_id.clone()),
        details: Some(details.to_string()),
        success: true,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alarm(host: &str, sensor_id: &str, alarm_type: &str) -> AlarmEvent {
        AlarmEvent {
            id: format!("{host}/{sensor_id}"),
            hostname: host.into(),
            sensor_id: sensor_id.into(),
            sensor_name: None,
            sensor_location: None,
            alarm_type: alarm_type.into(),
            severity: "warning".into(),
            from_state: None,
            to_state: None,
            message: None,
            value: None,
            threshold: None,
            triggered_at: "2026-10-14T00:00:00Z".into(),
            cleared_at: None,
            acknowledged_at: None,
            acknowledged_by_user_id: None,
            acknowledged_by_username: None,
            native_key: None,
            created_at: "2026-10-14T00:00:00Z".into(),
            max_severity: "warning".into(),
            end_reason: None,
        }
    }

    fn hub(name: &str) -> LiveHub {
        LiveHub {
            hostname: name.into(),
            online: true,
            has_nodes: true,
            has_beacons: true,
        }
    }

    fn inv() -> Inventory {
        let mut inv = Inventory {
            live_hubs: vec![hub("proximos-4e970f4a")],
            ..Default::default()
        };
        inv.nodes.insert("aa11".into());
        inv.beacons.insert("AA:BB:CC:DD:EE:01".into());
        inv.rule_labels.insert(
            (THRESHOLD_SCOPE_NODE.into(), "aa11".into()),
            ["Temperature".to_string()].into(),
        );
        inv.rule_labels.insert(
            (THRESHOLD_SCOPE_EYE.into(), "AA:BB:CC:DD:EE:01".into()),
            HashSet::new(),
        );
        inv
    }

    #[test]
    fn the_serial_is_the_hex_tail() {
        assert_eq!(hub_serial("fiber-4e970f4a").as_deref(), Some("4e970f4a"));
        assert_eq!(hub_serial("proximos-4E970F4A").as_deref(), Some("4e970f4a"));
        assert_eq!(hub_serial("hub1"), None);
        assert_eq!(hub_serial("my-hub"), None);
    }

    #[test]
    fn a_removed_beacon_closes_its_alarms() {
        let a = alarm(
            "proximos-4e970f4a",
            "eye:AA:BB:CC:DD:EE:02:Temperature",
            "HIGH",
        );
        assert_eq!(
            classify(&a, &inv()),
            Verdict::Close(EndReason::DeviceRemoved)
        );
        let a = alarm(
            "proximos-4e970f4a",
            "eye:AA:BB:CC:DD:EE:02:liveness",
            "OFFLINE",
        );
        assert_eq!(
            classify(&a, &inv()),
            Verdict::Close(EndReason::DeviceRemoved)
        );
    }

    #[test]
    fn a_removed_node_closes_its_alarms() {
        let a = alarm("proximos-4e970f4a", "lrw:bb22:Temperature", "HIGH");
        assert_eq!(
            classify(&a, &inv()),
            Verdict::Close(EndReason::DeviceRemoved)
        );
    }

    #[test]
    fn a_device_is_not_judged_on_a_hub_that_lists_none_or_is_offline() {
        let mut i = inv();
        i.live_hubs[0].has_beacons = false;
        let a = alarm(
            "proximos-4e970f4a",
            "eye:AA:BB:CC:DD:EE:02:Temperature",
            "HIGH",
        );
        assert_eq!(classify(&a, &i), Verdict::Keep);
        let mut i = inv();
        i.live_hubs[0].online = false;
        assert_eq!(classify(&a, &i), Verdict::Keep);
    }

    #[test]
    fn a_renamed_hub_rekeys_its_alarms() {
        let a = alarm(
            "fiber-4e970f4a",
            "eye:AA:BB:CC:DD:EE:01:Temperature",
            "HIGH",
        );
        let mut i = inv();
        i.persisted_hubs.insert("fiber-4e970f4a".into());
        assert_eq!(classify(&a, &i), Verdict::Rekey("proximos-4e970f4a".into()));
        let s = alarm("fiber-4e970f4a", "sys:wifi", "WIFI_DISCONNECT");
        assert_eq!(classify(&s, &i), Verdict::Rekey("proximos-4e970f4a".into()));
    }

    #[test]
    fn an_unknown_hub_closes_but_a_paired_silent_one_waits() {
        let a = alarm("fiber-00000001", "line-1", "HIGH");
        assert_eq!(classify(&a, &inv()), Verdict::Close(EndReason::HubRemoved));
        let mut i = inv();
        i.persisted_hubs.insert("fiber-00000001".into());
        assert_eq!(classify(&a, &i), Verdict::Keep);
    }

    #[test]
    fn a_removed_rule_closes_the_value_alarm_only() {
        let i = inv();
        let kept = alarm("proximos-4e970f4a", "lrw:aa11:Temperature", "HIGH");
        assert_eq!(classify(&kept, &i), Verdict::Keep);
        let gone = alarm("proximos-4e970f4a", "lrw:aa11:Humidity", "LOW");
        assert_eq!(classify(&gone, &i), Verdict::Close(EndReason::RuleRemoved));
        let beacon = alarm(
            "proximos-4e970f4a",
            "eye:AA:BB:CC:DD:EE:01:Temperature",
            "HIGH",
        );
        assert_eq!(
            classify(&beacon, &i),
            Verdict::Close(EndReason::RuleRemoved)
        );
        // Native, liveness and the battery flag have no portal rule.
        for (id, t) in [
            ("lrw:aa11:native:t1", "HIGH"),
            ("lrw:aa11:liveness", "OFFLINE"),
            ("eye:AA:BB:CC:DD:EE:01:Battery", "LOW"),
        ] {
            assert_eq!(
                classify(&alarm("proximos-4e970f4a", id, t), &i),
                Verdict::Keep,
                "{id}"
            );
        }
    }

    // ---- sweep against a real database --------------------------------

    use crate::state::models::{BeaconState, DeviceState, NodeState};
    use portal_storage::{NewAlarmEvent, NewFieldThreshold};

    fn storage() -> (tempfile::TempDir, Storage) {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = Storage::open(&dir.path().join("portal.db"), None).expect("open");
        (dir, s)
    }

    fn open(s: &Storage, host: &str, sensor_id: &str, alarm_type: &str) -> String {
        s.insert_alarm_event(&NewAlarmEvent {
            hostname: host.into(),
            sensor_id: sensor_id.into(),
            sensor_name: None,
            sensor_location: None,
            alarm_type: alarm_type.into(),
            severity: "warning".into(),
            from_state: None,
            to_state: None,
            message: None,
            value: None,
            threshold: None,
            triggered_at: "2026-10-14T00:00:00Z".into(),
            cleared_at: None,
            acknowledged_at: None,
            acknowledged_by_user_id: None,
            acknowledged_by_username: None,
            native_key: None,
        })
        .expect("insert")
    }

    fn snapshot() -> PortalState {
        let mut dev = DeviceState::new("proximos-4e970f4a".into());
        dev.online = true;
        dev.nodes
            .insert("aa11".into(), NodeState::new("aa11".into(), 0));
        dev.ble_sensors.insert(
            "AA:BB:CC:DD:EE:01".into(),
            BeaconState::new("AA:BB:CC:DD:EE:01".into(), 0),
        );
        let mut st = PortalState::default();
        st.devices.insert(dev.hostname.clone(), dev);
        st
    }

    #[test]
    fn the_sweep_closes_rekeys_audits_and_is_idempotent() {
        let (_d, s) = storage();
        s.upsert_field_threshold(
            &NewFieldThreshold {
                hostname: "proximos-4e970f4a".into(),
                scope: THRESHOLD_SCOPE_NODE.into(),
                entity: "aa11".into(),
                field: "temperature".into(),
                critical_low: None,
                warning_low: None,
                warning_high: Some(30.0),
                critical_high: None,
                enabled: true,
            },
            Some("t"),
        )
        .expect("rule");
        let kept = open(&s, "proximos-4e970f4a", "lrw:aa11:Temperature", "HIGH");
        let rule = open(&s, "proximos-4e970f4a", "lrw:aa11:Humidity", "HIGH");
        let gone = open(
            &s,
            "proximos-4e970f4a",
            "eye:AA:BB:CC:DD:EE:09:Temperature",
            "HIGH",
        );
        let moved = open(&s, "fiber-4e970f4a", "sys:wifi", "WIFI_DISCONNECT");
        let dup_old = open(&s, "fiber-4e970f4a", "sys:ethernet", "ETHERNET_DISCONNECT");
        let dup_new = open(
            &s,
            "proximos-4e970f4a",
            "sys:ethernet",
            "ETHERNET_DISCONNECT",
        );
        let lost = open(&s, "fiber-00000001", "line-1", "HIGH");
        let r = sweep(&s, &snapshot(), "2026-10-14T01:00:00Z").expect("sweep");
        let reason = |id: &str| s.get_alarm(id).unwrap().unwrap().end_reason;
        assert_eq!(reason(&kept), None);
        assert!(s.get_alarm(&kept).unwrap().unwrap().cleared_at.is_none());
        assert_eq!(reason(&rule).as_deref(), Some("rule_removed"));
        assert_eq!(reason(&gone).as_deref(), Some("device_removed"));
        assert_eq!(reason(&lost).as_deref(), Some("hub_removed"));
        assert_eq!(reason(&dup_old).as_deref(), Some("hub_renamed"));
        assert!(s.get_alarm(&dup_new).unwrap().unwrap().cleared_at.is_none());
        let m = s.get_alarm(&moved).unwrap().unwrap();
        assert_eq!(m.hostname, "proximos-4e970f4a");
        assert!(m.cleared_at.is_none());
        assert_eq!(r.closed.len(), 4);
        assert_eq!(r.rekeyed.len(), 1);
        let (audits, _) = s
            .list_audit(&portal_storage::AuditFilter {
                event_type: Some("alarm_auto_close".into()),
                limit: 100,
                ..Default::default()
            })
            .expect("audit");
        assert_eq!(audits.len(), 4);

        let again = sweep(&s, &snapshot(), "2026-10-14T02:00:00Z").expect("again");
        assert_eq!(again, SweepReport::default());
    }

    /// HIL 182: a renamed Hub's value alarm whose rule exists only under the
    /// old hostname is moved and then closed in the same pass; one whose rule
    /// exists under the new hostname is moved and kept.
    #[test]
    fn a_rekeyed_alarm_is_judged_again_under_its_new_hub() {
        let (_d, s) = storage();
        let rule = |host: &str, field: &str| {
            s.upsert_field_threshold(
                &NewFieldThreshold {
                    hostname: host.into(),
                    scope: THRESHOLD_SCOPE_NODE.into(),
                    entity: "aa11".into(),
                    field: field.into(),
                    critical_low: Some(10.0),
                    warning_low: None,
                    warning_high: None,
                    critical_high: None,
                    enabled: true,
                },
                Some("t"),
            )
            .expect("rule");
        };
        rule("fiber-4e970f4a", "temperature");
        rule("proximos-4e970f4a", "humidity");
        let stale = open(&s, "fiber-4e970f4a", "lrw:aa11:Temperature", "LOW");
        let live = open(&s, "fiber-4e970f4a", "lrw:aa11:Humidity", "LOW");

        let r = sweep(&s, &snapshot(), "2026-10-14T01:00:00Z").expect("sweep");
        let st = s.get_alarm(&stale).unwrap().unwrap();
        assert_eq!(st.hostname, "proximos-4e970f4a");
        assert_eq!(st.end_reason.as_deref(), Some("rule_removed"));
        assert!(st.cleared_at.is_some());
        let lv = s.get_alarm(&live).unwrap().unwrap();
        assert_eq!(lv.hostname, "proximos-4e970f4a");
        assert!(lv.cleared_at.is_none());
        assert_eq!((r.rekeyed.len(), r.closed.len()), (2, 1));
        let again = sweep(&s, &snapshot(), "2026-10-14T02:00:00Z").expect("again");
        assert_eq!(again, SweepReport::default());
    }

    #[test]
    fn removing_a_device_closes_only_its_alarms() {
        let (_d, s) = storage();
        let mine = open(&s, "h1", "eye:AA:BB:CC:DD:EE:01:Temperature", "HIGH");
        let other = open(&s, "h1", "eye:AA:BB:CC:DD:EE:02:Temperature", "HIGH");
        let n = close_device_alarms(
            &s,
            "h1",
            &AlarmTarget::Beacon("aa:bb:cc:dd:ee:01".into()),
            "2026-10-14T01:00:00Z",
        )
        .expect("close");
        assert_eq!(n, 1);
        assert_eq!(
            s.get_alarm(&mine).unwrap().unwrap().end_reason.as_deref(),
            Some("device_removed")
        );
        assert!(s.get_alarm(&other).unwrap().unwrap().cleared_at.is_none());
    }
}
