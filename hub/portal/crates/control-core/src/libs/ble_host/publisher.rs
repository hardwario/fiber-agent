//! `Publisher` over the agent's existing MQTT channel.
//!
//! The wire does not change. `control_ble::host::BeaconPayload` and this crate's
//! `BeaconTagPayload` carry the same 21 fields under the same names, because the portal's parser
//! (`fiber_protocol::payloads::eye`) was written against them and a rename here would be a silent
//! break of a shipped consumer. This file is the field-for-field map and nothing else.
//!
//! Two type differences are deliberate and handled here, not in the crate:
//!
//! - **`f64` → `f32` for temperature.** The crate keeps the decoded value at full precision; the v1
//!   wire has always been `f32`. Narrowing at the edge means the crate's own arithmetic (threshold
//!   comparison, archive decode) never inherits a wire artefact.
//! - **`Option<bool>` → `bool` for the three flag fields.** The crate models "never heard" as
//!   `None` (UI-10: a beacon that has not been seen must not claim flag knowledge). The v1 wire has
//!   no null there, so `None` becomes `false` — the same thing v1 published for an unheard beacon.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use control_ble::host::{
    AggregatedSnapshot, BeaconPayload, DegradedState, DetectResult, HistoryMessage, Publisher,
    SensorsSnapshot,
};

use crate::libs::mqtt::messages::{BeaconTagPayload, MqttMessage};

/// How long one `eye/history` message waits for room in a full MQTT queue.
const HISTORY_SEND_WAIT: Duration = Duration::from_secs(2);

pub struct MqttPublisher {
    tx: crossbeam::channel::Sender<MqttMessage>,
    /// Set when a history message timed out: the queue is not draining, so the rest of that burst
    /// is dropped at once instead of each waiting out [`HISTORY_SEND_WAIT`] in turn.
    history_stalled: AtomicBool,
}

impl MqttPublisher {
    pub fn new(tx: crossbeam::channel::Sender<MqttMessage>) -> MqttPublisher {
        MqttPublisher {
            tx,
            history_stalled: AtomicBool::new(false),
        }
    }

    /// A dropped message is a logged warning, never a panic and never a retry loop: the MQTT thread
    /// owning the receiver can be down (CP-2 degrade-loudly), and a scan tick must not block on it.
    fn send(&self, what: &str, msg: MqttMessage) {
        if let Err(e) = self.tx.try_send(msg) {
            eprintln!("[ble_host] dropped a {what} publish: {e}");
        }
    }
}

/// The map. Public so the equivalence test can call it without a channel.
pub fn to_tag_payload(p: &BeaconPayload) -> BeaconTagPayload {
    BeaconTagPayload {
        gateway: p.gateway.clone(),
        mac: p.mac.clone(),
        name: p.name.clone(),
        temperature_c: p.temperature_c.map(|c| c as f32),
        humidity_pct: p.humidity_pct,
        battery_mv: p.battery_mv,
        low_battery: p.low_battery.unwrap_or(false),
        magnet_present: p.magnet_present.unwrap_or(false),
        magnet_detected: p.magnet_detected.unwrap_or(false),
        moving: p.moving,
        movement_count: p.movement_count,
        pitch_deg: p.pitch_deg,
        roll_deg: p.roll_deg,
        rssi: p.rssi,
        last_seen_ts: p.last_seen_ts,
        stale: p.stale,
        provisioning: p.provisioning.clone(),
        discovered: p.discovered,
        is_en12830: p.is_en12830,
        field_alarm_states: p
            .field_alarm_states
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        alarm_state: p.alarm_state.clone(),
        ble_config: p.ble_config.clone(),
    }
}

impl Publisher for MqttPublisher {
    fn publish_sensors(&self, snapshot: &SensorsSnapshot) {
        let tags: Vec<BeaconTagPayload> = snapshot.tags.iter().map(to_tag_payload).collect();
        self.send(
            "eye/sensors",
            MqttMessage::PublishBeaconSensorData {
                tags,
                discovered_capped: snapshot.discovered_capped,
            },
        );
    }

    fn publish_aggregated(&self, window: &AggregatedSnapshot) {
        match serde_json::to_value(window) {
            Ok(payload) => self.send(
                "eye/aggregated",
                MqttMessage::PublishBeaconAggregated { payload },
            ),
            Err(e) => eprintln!("[ble_host] cannot serialise an eye/aggregated publish: {e}"),
        }
    }

    fn publish_detect(&self, result: &DetectResult) {
        self.send(
            "eye/detect",
            MqttMessage::PublishBeaconDetectResult {
                mac: result.mac.clone(),
                is_en12830: result.is_en12830,
                status: result.status.clone(),
            },
        );
    }

    fn publish_history(&self, message: &HistoryMessage) {
        let payload = match serde_json::to_value(message) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[ble_host] cannot serialise an eye/history message: {e}");
                return;
            }
        };
        let msg = MqttMessage::PublishBeaconHistory { payload };
        // A download's records are one burst of up to a few hundred messages; unlike a sensors
        // tick, a dropped batch is a hole in the archive the Portal cannot see. So a full queue is
        // waited on briefly — unless the last wait already timed out, which means the MQTT side is
        // stuck and waiting again would only stall the dial worker.
        let sent = match self.tx.try_send(msg) {
            Ok(()) => Ok(()),
            Err(crossbeam::channel::TrySendError::Full(msg))
                if !self.history_stalled.load(Ordering::Relaxed) =>
            {
                self.tx
                    .send_timeout(msg, HISTORY_SEND_WAIT)
                    .map_err(|e| e.to_string())
            }
            Err(e) => Err(e.to_string()),
        };
        self.history_stalled.store(sent.is_err(), Ordering::Relaxed);
        if let Err(e) = sent {
            eprintln!(
                "[ble_host] dropped an eye/history publish (job {}): {e}",
                message.job
            );
        }
    }

    fn publish_degraded(&self, state: &DegradedState) {
        // CP-2's degrade-loudly report. There is no BLE-specific alarm variant on the v1 wire, so
        // this rides `PublishSystemAlarmEvent` — the same path power/wifi/ethernet use — with a
        // `BLE_`-prefixed `alarm_type`. The portal treats an unknown type as informational, which
        // is the right default for a condition that does not stop the Hub measuring.
        self.send(
            "alarm",
            MqttMessage::PublishSystemAlarmEvent {
                alarm_type: format!("BLE_{}", state.condition.to_uppercase()),
                name: "Bluetooth".to_string(),
                from_state: if state.active { "NORMAL" } else { "CRITICAL" }.to_string(),
                to_state: if state.active { "CRITICAL" } else { "NORMAL" }.to_string(),
                message: match &state.mac {
                    Some(mac) => format!("{mac}: {}", state.detail),
                    None => state.detail.clone(),
                },
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn payload() -> BeaconPayload {
        BeaconPayload {
            gateway: "test-hub".into(),
            mac: "AA:BB:CC:DD:EE:FF".into(),
            name: Some("freezer".into()),
            temperature_c: Some(-18.25),
            humidity_pct: Some(41),
            battery_mv: Some(2950),
            low_battery: Some(false),
            magnet_present: Some(true),
            magnet_detected: Some(false),
            moving: Some(false),
            movement_count: Some(7),
            pitch_deg: Some(-3),
            roll_deg: Some(180),
            rssi: Some(-64),
            last_seen_ts: Some(1_700_000_000),
            stale: false,
            provisioning: "provisioned".into(),
            discovered: false,
            is_en12830: Some(true),
            field_alarm_states: BTreeMap::from([("temperature_c".to_string(), "ok".to_string())]),
            alarm_state: "ok".into(),
            ble_config: None,
        }
    }

    #[test]
    fn publisher_maps_to_the_existing_mqtt_message_variants() {
        let (tx, rx) = crossbeam::channel::unbounded();
        let p = MqttPublisher::new(tx);
        p.publish_sensors(&SensorsSnapshot {
            timestamp: "2026-08-26T10:00:00Z".into(),
            tags: vec![payload()],
            discovered_capped: false,
        });
        match rx.try_recv().expect("a message") {
            MqttMessage::PublishBeaconSensorData {
                tags,
                discovered_capped,
            } => {
                assert!(
                    !discovered_capped,
                    "the host's own flag rides the message, not a constant"
                );
                assert_eq!(tags.len(), 1);
                let t = &tags[0];
                assert_eq!(t.gateway, "test-hub");
                assert_eq!(t.mac, "AA:BB:CC:DD:EE:FF");
                assert_eq!(t.name.as_deref(), Some("freezer"));
                assert_eq!(t.temperature_c, Some(-18.25));
                assert_eq!(t.humidity_pct, Some(41));
                assert_eq!(t.battery_mv, Some(2950));
                assert!(!t.low_battery);
                assert!(t.magnet_present);
                assert!(!t.magnet_detected);
                assert_eq!(t.moving, Some(false));
                assert_eq!(t.movement_count, Some(7));
                assert_eq!(t.pitch_deg, Some(-3));
                assert_eq!(t.roll_deg, Some(180));
                assert_eq!(t.rssi, Some(-64));
                assert_eq!(t.last_seen_ts, Some(1_700_000_000));
                assert!(!t.stale);
                assert_eq!(t.provisioning, "provisioned");
                assert!(!t.discovered);
                assert_eq!(t.is_en12830, Some(true));
                assert_eq!(t.field_alarm_states.get("temperature_c").unwrap(), "ok");
                assert_eq!(t.alarm_state, "ok");
            }
            other => panic!("expected PublishBeaconSensorData, got {other:?}"),
        }
    }

    #[test]
    fn an_unheard_beacons_optional_flags_publish_as_false_the_way_v1_did() {
        let mut p = payload();
        p.low_battery = None;
        p.magnet_present = None;
        p.magnet_detected = None;
        let t = to_tag_payload(&p);
        assert!(!t.low_battery);
        assert!(!t.magnet_present);
        assert!(!t.magnet_detected);
    }

    #[test]
    fn detect_maps_to_the_detect_result_variant_including_the_undecided_case() {
        let (tx, rx) = crossbeam::channel::unbounded();
        let p = MqttPublisher::new(tx);
        p.publish_detect(&DetectResult {
            timestamp: "2026-08-26T10:00:00Z".into(),
            mac: "AA:BB:CC:DD:EE:FF".into(),
            is_en12830: None,
            status: "unreachable".into(),
        });
        match rx.try_recv().expect("a message") {
            MqttMessage::PublishBeaconDetectResult {
                mac,
                is_en12830,
                status,
            } => {
                assert_eq!(mac, "AA:BB:CC:DD:EE:FF");
                assert_eq!(
                    is_en12830, None,
                    "an undecided detect must not assert false"
                );
                assert_eq!(status, "unreachable");
            }
            other => panic!("expected PublishBeaconDetectResult, got {other:?}"),
        }
    }

    fn history_message(job: &str) -> HistoryMessage {
        control_ble::central::history::HistoryRequest::new(Some(1), Some(2), Some(job.into()), 0)
            .expect("request")
            .requested(&control_ble::mac::MacAddr::parse("AA:BB:CC:DD:EE:FF").expect("mac"))
    }

    #[test]
    fn a_history_message_rides_in_its_wire_shape() {
        let (tx, rx) = crossbeam::channel::unbounded();
        let p = MqttPublisher::new(tx);
        p.publish_history(&history_message("bh-1"));
        match rx.try_recv().expect("a message") {
            MqttMessage::PublishBeaconHistory { payload } => {
                assert_eq!(payload["state"], "requested");
                assert_eq!(payload["job"], "bh-1");
                assert_eq!(payload["mac"], "AA:BB:CC:DD:EE:FF");
                assert_eq!(
                    (payload["from"].clone(), payload["to"].clone()),
                    (1.into(), 2.into())
                );
            }
            other => panic!("expected PublishBeaconHistory, got {other:?}"),
        }
    }

    #[test]
    fn a_full_queue_is_waited_on_once_then_a_stalled_burst_drops_fast() {
        let (tx, rx) = crossbeam::channel::bounded(1);
        let p = MqttPublisher::new(tx);
        p.publish_history(&history_message("a"));
        let started = std::time::Instant::now();
        p.publish_history(&history_message("b")); // waits HISTORY_SEND_WAIT, then drops
        assert!(started.elapsed() >= HISTORY_SEND_WAIT);
        let started = std::time::Instant::now();
        p.publish_history(&history_message("c")); // stalled: dropped at once
        assert!(started.elapsed() < HISTORY_SEND_WAIT);
        // Room again: the next one goes through and clears the stall.
        assert!(rx.try_recv().is_ok());
        p.publish_history(&history_message("d"));
        match rx.try_recv().expect("d") {
            MqttMessage::PublishBeaconHistory { payload } => assert_eq!(payload["job"], "d"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(!p.history_stalled.load(Ordering::Relaxed));
    }

    #[test]
    fn a_degraded_report_names_the_condition_and_the_beacon() {
        let (tx, rx) = crossbeam::channel::unbounded();
        let p = MqttPublisher::new(tx);
        p.publish_degraded(&DegradedState {
            condition: "en12830_not_recording",
            mac: Some("AA:BB:CC:DD:EE:FF".into()),
            active: true,
            detail: "restart refused three times".into(),
        });
        match rx.try_recv().expect("a message") {
            MqttMessage::PublishSystemAlarmEvent {
                alarm_type,
                name,
                from_state,
                to_state,
                message,
            } => {
                assert_eq!(alarm_type, "BLE_EN12830_NOT_RECORDING");
                assert_eq!(name, "Bluetooth");
                assert_eq!(from_state, "NORMAL");
                assert_eq!(to_state, "CRITICAL", "active: true is the raise");
                assert!(message.starts_with("AA:BB:CC:DD:EE:FF: "), "{message}");
            }
            other => panic!("expected PublishSystemAlarmEvent, got {other:?}"),
        }
    }

    /// The crate's window serializes into what the portal parses
    /// (`fiber_protocol::payloads::eye_aggregated`), field for field.
    #[test]
    fn an_aggregated_window_rides_its_own_variant_in_the_portals_shape() {
        use control_ble::host::{BeaconAggregate, FieldStats, WindowAggregate};
        use fiber_protocol::payloads::eye_aggregated::EyeAggregatedMsg;
        let (tx, rx) = crossbeam::channel::unbounded();
        let p = MqttPublisher::new(tx);
        p.publish_aggregated(&AggregatedSnapshot {
            timestamp: "2026-10-12T10:10:00Z".into(),
            period_start_ts: 1_791_799_200,
            period_end_ts: 1_791_799_800,
            duration_sec: 600,
            tags: vec![BeaconAggregate {
                mac: "AA:BB:CC:DD:EE:FF".into(),
                name: Some("freezer".into()),
                message_id: "AA:BB:CC:DD:EE:FF-adv-1791799200".into(),
                window_start: 1_791_799_200,
                aggregate: WindowAggregate {
                    window_s: 600,
                    count: 60,
                    alarm_state: "NORMAL".into(),
                    fields: BTreeMap::from([(
                        "temperature".to_string(),
                        FieldStats {
                            min: -19.5,
                            avg: -18.25,
                            max: -17.0,
                        },
                    )]),
                    transition: None,
                },
            }],
        });
        match rx.try_recv().expect("a message") {
            MqttMessage::PublishBeaconAggregated { payload } => {
                let msg: EyeAggregatedMsg = serde_json::from_value(payload).expect("portal shape");
                assert_eq!(msg.period_start_ts, Some(1_791_799_200));
                let t = &msg.tags[0];
                assert_eq!(
                    t.message_id.as_deref(),
                    Some("AA:BB:CC:DD:EE:FF-adv-1791799200")
                );
                assert_eq!(t.window_s, Some(600));
                assert_eq!(t.count, 60);
                assert_eq!(t.avg("temperature"), Some(-18.25));
            }
            other => panic!("expected PublishBeaconAggregated, got {other:?}"),
        }
    }

    #[test]
    fn a_closed_receiver_is_a_warning_not_a_panic() {
        // The MQTT thread being down must not take the scan loop with it (CP-2).
        let (tx, rx) = crossbeam::channel::bounded(1);
        drop(rx);
        let p = MqttPublisher::new(tx);
        p.publish_sensors(&SensorsSnapshot {
            timestamp: "2026-08-26T10:00:00Z".into(),
            tags: vec![payload()],
            discovered_capped: false,
        });
    }
}
