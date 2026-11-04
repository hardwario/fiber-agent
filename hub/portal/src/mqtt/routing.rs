//! Pure topic-to-[`IngestEvent`] routing, factored out of the rumqttc event
//! loop ([`super::run_ingest`]) so it can be unit-tested without a live
//! broker. No I/O, no logging, no counters here — [`run_ingest`] is the only
//! caller and owns all side effects ([`RouteResult`] tells it exactly what
//! to log/count).

use fiber_protocol::payloads::alarm_event::AlarmEventMsg;
use fiber_protocol::payloads::config_challenge::ConfigChallengeMsg;
use fiber_protocol::payloads::config_response::ConfigResponseMsg;
use fiber_protocol::payloads::config_state::ConfigStateMsg;
use fiber_protocol::payloads::errors::DeviceErrorMsg;
use fiber_protocol::payloads::export::{ExportEnvelope, Probe1mData};
use fiber_protocol::payloads::eye::EyeSensorsMsg;
use fiber_protocol::payloads::eye_aggregated::EyeAggregatedMsg;
use fiber_protocol::payloads::lorawan::{
    LorawanSensorsMsg, NodeConfigMsg, NodeFullConfigMsg, NodeHistoryMsg,
};
use fiber_protocol::payloads::pair_invite::PairInviteMsg;
use fiber_protocol::payloads::pair_response::PairResponseMsg;
use fiber_protocol::payloads::sensors_aggregated::SensorsAggregatedMsg;
use fiber_protocol::payloads::standby::StandbyMsg;
use fiber_protocol::payloads::system_info::SystemInfoMsg;
use fiber_protocol::{extract_hostname, parse_status};

use crate::state::radio::GatewaysReport;
use crate::state::{IngestEvent, NodeConfigDumpMsg, NodeDeviceInfoMsg, NodeReadingMsg};

/// Outcome of routing one inbound `Publish`. `run_ingest` matches on this to
/// decide what to log, what to count, and whether to call `store.apply()`.
/// No `PartialEq`/`Clone`: `IngestEvent` (owned by the state module) derives
/// neither, and `run_ingest` only ever consumes one `RouteResult` by value.
#[derive(Debug)]
pub enum RouteResult {
    /// Decoded into a typed event ready for `StateStore::apply`. Boxed:
    /// `IngestEvent` itself runs ~450 bytes (it embeds whole payload
    /// structs), which would otherwise size every `RouteResult` — including
    /// the zero-payload `Unhandled`/`Ignored`/`Tombstone` variants — to
    /// match the largest one.
    Event(Box<IngestEvent>),
    /// A topic the builder subscribes to but no handler routes anywhere —
    /// `responses/#` (only `config/challenge|response`/`pair/response` are
    /// handled, not the wider `responses/#`/`pair/#` families), the
    /// portal's own echoed `pair/request`, and any `lorawan/sensors/{dev_eui}/…`
    /// subtopic outside the five M5 handles (`config`/`full-config`/`info`/
    /// `command`/`history`) — logged at debug and counted, subscription
    /// stays live. `config/challenge`, `config/response`, and
    /// `pair/response` are handled below (M4-C: they route to
    /// [`IngestEvent::ConfigChallenge`]/[`ConfigResponse`]/[`PairResponse`],
    /// not here).
    Unhandled,
    /// A non-`probe_1m` export stream (`sticker`, `eye`, `probe`, `alarm`,
    /// `probe_1m_replay`, …) — ledger-only in v1, out of scope for M1's
    /// StateStore. Also `lorawan/gateways` (external LoRaWAN gateways, not
    /// Nodes) — out of phase-1 scope per team agreement 2026-08-18, revisit
    /// if/when external-gateway visibility becomes a portal requirement.
    /// Skipped without logging or counting.
    Ignored,
    /// Empty retained payload on a topic where that means "no config yet"
    /// rather than a decode failure (every handled topic except `status`,
    /// where empty is a meaningful "offline").
    Tombstone,
    /// JSON (or, for `export/probe_1m/{line}`, the topic's line segment)
    /// failed to parse. Carries a human-readable reason for the `warn` log.
    DecodeError(String),
}

/// Routes one `(topic, payload)` publish. `prefix` is the configured
/// `topic_prefix` (mqtt.md: hostname extraction must honour it, not
/// hardcode `fiber` like v1's regex did).
pub fn route_publish(topic: &str, payload: &[u8], prefix: &str) -> RouteResult {
    // The v2 radio family is checked first and separately: it is rooted at a **fixed**
    // `proximos/` level (control_radio::northbound::ROOT), not at the configured
    // `topic_prefix` every other topic hangs off, so `extract_hostname` — which exists to
    // honour that prefix — would reject it before it could be routed.
    if let Some(result) = route_radio_node(topic, payload) {
        return result;
    }
    if let Some(result) = route_radio_diagnostics(topic, payload) {
        return result;
    }

    // Same reason, one topic rather than a family: `radio/gateways` is a fixed literal with
    // no hostname level at all (devices.hub.md §12 A2), so it is matched exactly, before
    // `extract_hostname` gets a chance to reject it.
    if topic == control_radio::northbound::GATEWAYS_TOPIC {
        return route_radio_gateways(payload);
    }

    let Some(topic_hostname) = extract_hostname(topic, prefix) else {
        return RouteResult::Unhandled;
    };

    // extract_hostname already proved `topic == "{prefix}/{hostname}/..."`,
    // so both strips below are infallible.
    let suffix = topic
        .strip_prefix(prefix)
        .and_then(|s| s.strip_prefix('/'))
        .and_then(|s| s.strip_prefix(topic_hostname))
        .and_then(|s| s.strip_prefix('/'))
        .unwrap_or("");
    // One spelling from here on (decision #21), whatever the topic carried.
    let hostname = topic_hostname.to_ascii_lowercase();

    match suffix {
        "status" => RouteResult::Event(Box::new(IngestEvent::Status {
            hostname,
            status: parse_status(payload),
        })),

        "sensors/aggregated" => decode_or_tombstone(payload, |p: SensorsAggregatedMsg| {
            IngestEvent::SensorsAggregated {
                hostname,
                payload: p,
            }
        }),

        // Every sample, same shape (plan/control/storage_intervals.md §4):
        // the live view and alarm reconciliation, never a stored reading.
        "sensors/summary" => decode_or_tombstone(payload, |p: SensorsAggregatedMsg| {
            IngestEvent::SensorsLive {
                hostname,
                payload: p,
            }
        }),

        "system/info" => decode_or_tombstone(payload, |p: SystemInfoMsg| IngestEvent::SystemInfo {
            hostname,
            payload: Box::new(p),
        }),

        "alarms/events" => {
            decode_or_tombstone(payload, |p: AlarmEventMsg| IngestEvent::AlarmEvent {
                hostname,
                payload: p,
            })
        }

        "power/standby" | "power/events/standby" => {
            decode_or_tombstone(payload, |p: StandbyMsg| IngestEvent::Standby {
                hostname,
                payload: p,
            })
        }

        "config/state" => {
            decode_or_tombstone(payload, |p: ConfigStateMsg| IngestEvent::ConfigState {
                hostname,
                payload: p,
            })
        }

        "errors" => decode_or_tombstone(payload, |p: DeviceErrorMsg| IngestEvent::DeviceError {
            hostname,
            payload: p,
        }),

        // M4-C: signed-command handshake + pairing reply, routed to
        // CommandRelay via the ingest loop's third sink (mqtt::dispatch),
        // not applied to StateStore (state::store::apply_locked ignores
        // them). Not retained topics, so an empty payload here would be
        // unusual, but decode_or_tombstone's fallback is still the right
        // default rather than a hard decode error.
        "config/challenge" => decode_or_tombstone(payload, |p: ConfigChallengeMsg| {
            IngestEvent::ConfigChallenge {
                hostname,
                payload: p,
            }
        }),

        "config/response" => decode_or_tombstone(payload, |p: ConfigResponseMsg| {
            IngestEvent::ConfigResponse {
                hostname,
                payload: p,
            }
        }),

        "pair/response" => {
            decode_or_tombstone(payload, |p: PairResponseMsg| IngestEvent::PairResponse {
                hostname,
                payload: p,
            })
        }

        // The one inbound pairing topic that is not a reply (GitLab #61). Already covered by the
        // `{prefix}/+/pair/#` subscription, so it arrives without a subscribe-list change — which
        // is also why it was landing in `Unhandled` until this arm existed.
        "pair/invite" => decode_or_tombstone(payload, |p: PairInviteMsg| IngestEvent::PairInvite {
            hostname,
            payload: p,
        }),

        "lorawan/sensors" => decode_or_tombstone(payload, |p: LorawanSensorsMsg| {
            IngestEvent::LorawanSensors {
                hostname,
                payload: p,
            }
        }),

        // External LoRaWAN gateways, not Nodes — out of phase-1 scope (team
        // agreement 2026-08-18). Still subscribed (topics.rs) so it doesn't
        // fall through to Unhandled and get logged/counted every publish.
        "lorawan/gateways" => RouteResult::Ignored,

        "eye/sensors" => decode_or_tombstone(payload, |p: EyeSensorsMsg| IngestEvent::EyeSensors {
            hostname,
            payload: p,
        }),

        "eye/aggregated" => {
            decode_or_tombstone(payload, |p: EyeAggregatedMsg| IngestEvent::EyeAggregated {
                hostname,
                payload: p,
            })
        }

        // B5 (§8 WP5.1, §20) removed the EN12830 type probe from the
        // portal. `control-core` still answers `detect_eye_tag` and still
        // publishes the result here, and `topics.rs` still subscribes, so
        // this is `Ignored` rather than deleted: dropping the arm entirely
        // would put every such publish through Unhandled's log-and-count.
        "eye/detect" => RouteResult::Ignored,

        // Decision #42: the Server's registry and home Hubs. A tombstone (an empty retained
        // publish) is a cluster that no longer publishes them.
        "eye/homes" => {
            decode_or_tombstone(payload, |p: fiber_protocol::payloads::eye::EyeHomesMsg| {
                IngestEvent::EyeHomes {
                    hostname,
                    payload: p,
                }
            })
        }
        // The Server's per-Hub views: the Hubs read them, the portal reads `eye/homes`.
        "eye/cluster" => RouteResult::Ignored,

        "eye/history" => {
            decode_or_tombstone(payload, |p: crate::beacon_history::BeaconHistoryMsg| {
                IngestEvent::BeaconHistory {
                    hostname,
                    payload: p,
                }
            })
        }

        _ if suffix.starts_with("lorawan/sensors/") => {
            route_lorawan_sensor(&suffix["lorawan/sensors/".len()..], payload, hostname)
        }

        _ if suffix.starts_with("export/") => {
            route_export(&suffix["export/".len()..], payload, hostname)
        }

        // responses/#, pair/request (the portal's own echo): subscribed per
        // topics.rs but no M1 handler.
        _ => RouteResult::Unhandled,
    }
}

/// `proximos/{host}/nodes/{dev_eui}/{reading|alarm|info|device_info|config|command|history}` — the v2 radio northbound family
/// (plan/control/radio/plan.md §9.4), published per node by `proximos-control`. One family
/// for both transports: the radio path is a field of the `info` payload, not a topic level.
///
/// `None` when the topic is not one of ours at all, so the caller falls through to the
/// configured-prefix tree. The topic is parsed by [`control_radio::northbound::parse_node_topic`] —
/// the producer's own inverse of the builder that publishes it — rather than by a `split('/')`
/// here, which would be a second copy of the shape free to drift from it.
///
/// `command` deliberately routes to `Ignored`: per-frame command results have no portal
/// consumer on this family. Ignored rather than `Unhandled` so a Node under active
/// configuration does not fill the diagnostics counters with a topic nobody is waiting for —
/// the same treatment `lorawan/gateways` gets below.
/// `proximos/{hostname}/radio/northbridge` and `…/radio/lorawan` — the radio status
/// documents (radio diagnostics §3/§4a). Kept as JSON; an empty payload withdraws one.
fn route_radio_diagnostics(topic: &str, payload: &[u8]) -> Option<RouteResult> {
    use crate::state::events::RadioDiagKind;
    let rest = topic
        .strip_prefix(control_radio::northbound::ROOT)?
        .strip_prefix('/')?;
    let (hostname, what) = rest.split_once('/')?;
    let kind = match what {
        "radio/northbridge" => RadioDiagKind::Northbridge,
        "radio/lorawan" => RadioDiagKind::Lorawan,
        _ => return None,
    };
    if hostname.is_empty() || hostname.contains('/') {
        return None;
    }
    let doc = if payload.is_empty() {
        None
    } else {
        match serde_json::from_slice::<serde_json::Value>(payload) {
            Ok(v) if v.is_object() => Some(v),
            Ok(_) => return Some(RouteResult::DecodeError("not a JSON object".into())),
            Err(e) => return Some(RouteResult::DecodeError(e.to_string())),
        }
    };
    Some(RouteResult::Event(Box::new(
        IngestEvent::RadioDiagnostics {
            hostname: hostname.to_string(),
            kind,
            doc,
        },
    )))
}

fn route_radio_node(topic: &str, payload: &[u8]) -> Option<RouteResult> {
    use control_radio::northbound::{parse_node_topic, Area};

    let parsed = parse_node_topic(topic)?;
    let hostname = parsed.hostname.to_ascii_lowercase();
    // Lowercased on the way in, exactly like `route_lorawan_sensor` does: the wire convention
    // is a lowercase dev_eui, but the topic segment's actual case is not guaranteed, and
    // `node_key` is the portal's join key across both ingest paths.
    let node_key = parsed.id.to_lowercase();

    Some(match parsed.area {
        Area::Reading => {
            decode_or_tombstone(payload, |p: NodeReadingMsg| IngestEvent::NodeReading {
                hostname,
                node_key,
                payload: Box::new(p),
            })
        }
        Area::Alarm => decode_or_tombstone(payload, |p: NodeReadingMsg| IngestEvent::NodeAlarm {
            hostname,
            node_key,
            payload: Box::new(p),
        }),
        // Retained: an empty payload is the removal tombstone `node-remove` publishes, which
        // is *data* (Value::Null), not a tombstone-shaped nothing to skip. Same contract as
        // `lorawan/sensors/{dev_eui}/info` below, and the same reason it cannot reuse
        // `decode_or_tombstone`.
        Area::Info => {
            if payload.is_empty() {
                return Some(RouteResult::Event(Box::new(IngestEvent::NodeInventory {
                    hostname,
                    node_key,
                    payload: serde_json::Value::Null,
                })));
            }
            match serde_json::from_slice::<serde_json::Value>(payload) {
                Ok(value) => RouteResult::Event(Box::new(IngestEvent::NodeInventory {
                    hostname,
                    node_key,
                    payload: value,
                })),
                Err(err) => RouteResult::DecodeError(err.to_string()),
            }
        }
        // Retained, like `info`: an empty payload is the removal tombstone `node-remove`
        // publishes, and it has to reach the store as a removal (`None`), not be skipped.
        Area::DeviceInfo => retained_document(payload, |p: Option<NodeDeviceInfoMsg>| {
            IngestEvent::NodeDeviceInfo {
                hostname,
                node_key,
                payload: p.map(Box::new),
            }
        }),
        Area::Config => retained_document(payload, |p: Option<NodeConfigDumpMsg>| {
            IngestEvent::NodeDeviceConfig {
                hostname,
                node_key,
                payload: p.map(Box::new),
            }
        }),
        // Not retained: one document per frame. An empty payload carries nothing to route.
        Area::Command => {
            if payload.is_empty() {
                return Some(RouteResult::Tombstone);
            }
            match serde_json::from_slice::<serde_json::Value>(payload) {
                Ok(value) => RouteResult::Event(Box::new(IngestEvent::NodeCommand {
                    hostname,
                    node_key,
                    payload: value,
                })),
                Err(err) => RouteResult::DecodeError(err.to_string()),
            }
        }
        // Retained, like `device_info`; an empty payload is the removal tombstone.
        Area::DeviceStatus => retained_document(payload, |p: Option<serde_json::Value>| {
            IngestEvent::NodeDeviceStatus {
                hostname,
                node_key,
                payload: p,
            }
        }),
        // Not retained: one decoded replay frame, rows for the database.
        Area::History => {
            decode_or_tombstone(payload, |p: NodeHistoryMsg| IngestEvent::NodeHistory {
                hostname,
                node_key,
                payload: p,
            })
        }
    })
}

/// A retained document on the v2 family: an empty payload is the removal tombstone and arrives
/// as `None`; anything else must parse.
fn retained_document<T, F>(payload: &[u8], into_event: F) -> RouteResult
where
    T: serde::de::DeserializeOwned,
    F: FnOnce(Option<T>) -> IngestEvent,
{
    if payload.is_empty() {
        return RouteResult::Event(Box::new(into_event(None)));
    }
    match serde_json::from_slice::<T>(payload) {
        Ok(parsed) => RouteResult::Event(Box::new(into_event(Some(parsed)))),
        Err(err) => RouteResult::DecodeError(err.to_string()),
    }
}

/// `radio/gateways` — the cluster-wide retained gateway report (devices.hub.md §12 A2),
/// parsed into the portal's own deserialization mirror ([`crate::state::radio`]).
fn route_radio_gateways(payload: &[u8]) -> RouteResult {
    // An empty retained payload **clears the stored report** rather than being skipped as a
    // tombstone: the topic is retained and cluster-wide, so an empty publish is the producer
    // withdrawing the report, and `RouteResult::Tombstone` (which the ingest loop drops
    // without touching the store) would leave a stale gateway table on screen for as long as
    // the portal runs. devices.hub.md §10 wants the opposite: no report reads as "radio
    // health unavailable", never as offline gateways.
    if payload.is_empty() {
        return RouteResult::Event(Box::new(IngestEvent::RadioGateways { report: None }));
    }
    match serde_json::from_slice::<GatewaysReport>(payload) {
        Ok(report) => RouteResult::Event(Box::new(IngestEvent::RadioGateways {
            report: Some(report),
        })),
        Err(err) => RouteResult::DecodeError(err.to_string()),
    }
}

/// `lorawan/sensors/{dev_eui}/{config|full-config|info|command|history}`.
/// The topic segment (still literally the device's dev_eui on the wire) is
/// lowercased here (mqtt.md: "dev_eui lowercase" is the wire convention, but
/// the topic segment's actual case isn't guaranteed) and carried inward as
/// the portal's internal `node_key` — `StateStore` also lowercases
/// defensively on apply, so this is belt-and-braces, not the only place it
/// happens.
fn route_lorawan_sensor(rest: &str, payload: &[u8], hostname: String) -> RouteResult {
    let mut parts = rest.splitn(2, '/');
    let Some(node_key) = parts.next().filter(|s| !s.is_empty()) else {
        return RouteResult::Unhandled;
    };
    let node_key = node_key.to_lowercase();
    let Some(subtopic) = parts.next() else {
        return RouteResult::Unhandled;
    };

    match subtopic {
        "config" => decode_or_tombstone(payload, |p: NodeConfigMsg| IngestEvent::StickerConfig {
            hostname,
            node_key,
            payload: p,
        }),

        "full-config" => decode_or_tombstone(payload, |p: NodeFullConfigMsg| {
            IngestEvent::StickerFullConfig {
                hostname,
                node_key,
                payload: p,
            }
        }),

        "command" => decode_or_tombstone(payload, |p: serde_json::Value| {
            IngestEvent::StickerCommandResult {
                hostname,
                node_key,
                payload: p,
            }
        }),

        "history" => {
            decode_or_tombstone(payload, |p: NodeHistoryMsg| IngestEvent::StickerHistory {
                hostname,
                node_key,
                payload: p,
            })
        }

        // Unlike every other topic here, an empty payload on `info` is
        // itself meaningful data (`Value::Null`), not a tombstone — the
        // store treats that Null as the signal to drop the cached entry
        // (IngestEvent::StickerInfo doc comment). So this arm can't reuse
        // decode_or_tombstone's empty-is-Tombstone default.
        "info" => {
            if payload.is_empty() {
                return RouteResult::Event(Box::new(IngestEvent::StickerInfo {
                    hostname,
                    node_key,
                    payload: serde_json::Value::Null,
                }));
            }
            match serde_json::from_slice::<serde_json::Value>(payload) {
                Ok(value) => RouteResult::Event(Box::new(IngestEvent::StickerInfo {
                    hostname,
                    node_key,
                    payload: value,
                })),
                Err(err) => RouteResult::DecodeError(err.to_string()),
            }
        }

        _ => RouteResult::Unhandled,
    }
}

/// `export/{stream}/{key...}` — only `probe_1m/{line}` is handled in M1;
/// every other export stream (`sticker`, `eye`, `probe`, `alarm`,
/// `probe_1m_replay`) is ledger-only and silently skipped (not this
/// binary's job — ledger dedup lives in portal-storage).
fn route_export(rest: &str, payload: &[u8], hostname: String) -> RouteResult {
    let mut parts = rest.splitn(2, '/');
    let stream = parts.next().unwrap_or("");
    let key = parts.next().unwrap_or("");

    if stream != "probe_1m" {
        return RouteResult::Ignored;
    }

    let line: u8 = match key.parse() {
        Ok(line) => line,
        Err(err) => return RouteResult::DecodeError(format!("bad probe_1m line {key:?}: {err}")),
    };

    if payload.is_empty() {
        return RouteResult::Tombstone;
    }

    match serde_json::from_slice::<ExportEnvelope<Probe1mData>>(payload) {
        Ok(envelope) => RouteResult::Event(Box::new(IngestEvent::Probe1m {
            hostname,
            line,
            payload: envelope.data,
        })),
        Err(err) => RouteResult::DecodeError(err.to_string()),
    }
}

/// Empty payload on a handled (non-`status`) topic is a tombstone, not a
/// decode failure (mqtt.md: retained-config topics use an empty payload to
/// mean "nothing here" rather than a JSON body). Otherwise decode and map.
fn decode_or_tombstone<T, F>(payload: &[u8], into_event: F) -> RouteResult
where
    T: serde::de::DeserializeOwned,
    F: FnOnce(T) -> IngestEvent,
{
    if payload.is_empty() {
        return RouteResult::Tombstone;
    }
    match serde_json::from_slice::<T>(payload) {
        Ok(parsed) => RouteResult::Event(Box::new(into_event(parsed))),
        Err(err) => RouteResult::DecodeError(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fiber_protocol::payloads::status::DeviceStatus;

    const PREFIX: &str = "proximos";

    fn assert_status(result: RouteResult, expect_hostname: &str, expect_status: DeviceStatus) {
        match result {
            RouteResult::Event(event) => match *event {
                IngestEvent::Status { hostname, status } => {
                    assert_eq!(hostname, expect_hostname);
                    assert_eq!(status, expect_status);
                }
                other => panic!("expected Status event, got {other:?}"),
            },
            other => panic!("expected Status event, got {other:?}"),
        }
    }

    /// Decision #21: whatever case the topic segment carries (an older v1 agent uppercased
    /// it), the event names the Hub in lowercase — every test below routes `FIBER-001`.
    #[test]
    fn the_topic_hostname_is_lowercased() {
        assert_status(
            route_publish("proximos/Fiber-4E970F4A/status", b"online", PREFIX),
            "fiber-4e970f4a",
            DeviceStatus::Online,
        );
    }

    #[test]
    fn status_json_online() {
        let result = route_publish(
            "proximos/FIBER-001/status",
            br#"{"status":"online","timestamp":"2026-01-13T14:30:00Z"}"#,
            PREFIX,
        );
        assert_status(result, "fiber-001", DeviceStatus::Online);
    }

    #[test]
    fn status_plain_text() {
        let result = route_publish("proximos/FIBER-001/status", b"offline", PREFIX);
        assert_status(result, "fiber-001", DeviceStatus::Offline);
    }

    #[test]
    fn status_empty_payload_is_offline_not_tombstone() {
        // Unlike every other topic, status's own empty-payload semantics
        // (parse_status) apply — it must not fall through to Tombstone.
        let result = route_publish("proximos/FIBER-001/status", b"", PREFIX);
        assert_status(result, "fiber-001", DeviceStatus::Offline);
    }

    /// The live sample rides the aggregated shape on its own route, so it can
    /// update the view without becoming a stored reading.
    #[test]
    fn sensors_summary_decodes_as_a_live_sample() {
        let json = br#"{
            "period_start_ts": 1736775000, "period_end_ts": 1736775002, "duration_sec": 2,
            "sensors": [ {
                "line": 3, "sample_count": 1, "disconnected_count": 0,
                "temperature": { "min_celsius": 4.5, "max_celsius": 4.5, "avg_celsius": 4.5 },
                "dominant_alarm_state": "NORMAL"
            } ]
        }"#;
        match route_publish("proximos/FIBER-001/sensors/summary", json, PREFIX) {
            RouteResult::Event(event) => match *event {
                IngestEvent::SensorsLive { hostname, payload } => {
                    assert_eq!(hostname, "fiber-001");
                    assert_eq!(payload.sensors[0].line, 3);
                }
                other => panic!("expected SensorsLive, got {other:?}"),
            },
            other => panic!("expected SensorsLive, got {other:?}"),
        }
    }

    #[test]
    fn sensors_aggregated_decodes() {
        let json = br#"{
            "timestamp": "2026-01-13T14:30:00Z",
            "period_start_ts": 1736775000, "period_end_ts": 1736775060, "duration_sec": 60,
            "sensors": [ {
                "line": 0, "name": "Sensor 1", "location": null,
                "sample_count": 60, "disconnected_count": 0,
                "temperature": { "min_celsius": 36.5, "max_celsius": 37.2, "avg_celsius": 36.8 },
                "alarm_counts": { "normal": 60, "warning": 0, "critical": 0, "disconnected": 0, "reconnecting": 0 },
                "dominant_alarm_state": "NORMAL", "alarm_triggered_at": null
            } ]
        }"#;
        let result = route_publish("proximos/FIBER-001/sensors/aggregated", json, PREFIX);
        match result {
            RouteResult::Event(event) => match *event {
                IngestEvent::SensorsAggregated { hostname, payload } => {
                    assert_eq!(hostname, "fiber-001");
                    assert_eq!(payload.sensors.len(), 1);
                }
                other => panic!("expected SensorsAggregated event, got {other:?}"),
            },
            other => panic!("expected SensorsAggregated event, got {other:?}"),
        }
    }

    #[test]
    fn system_info_decodes() {
        let json = br#"{
            "timestamp": "2026-01-13T14:30:00Z", "device_label": "Freezer 01",
            "firmware_version": "0.1.0", "uptime_seconds": 86400, "uptime_human": "1d",
            "power": { "battery": { "voltage_mv": 4200, "percentage": 85, "status": "normal" },
                       "dc": { "voltage_mv": 5000, "connected": true }, "last_dc_loss": null },
            "network": { "wifi": { "connected": true, "signal_dbm": -45, "ip": "192.168.1.100" },
                         "ethernet": { "connected": false, "ip": null }, "has_internet": true },
            "storage": { "data_partition": { "total_bytes": 100, "available_bytes": 50, "used_percent": 50 } }
        }"#;
        let result = route_publish("proximos/FIBER-001/system/info", json, PREFIX);
        match result {
            RouteResult::Event(event) => match *event {
                IngestEvent::SystemInfo { hostname, payload } => {
                    assert_eq!(hostname, "fiber-001");
                    assert_eq!(payload.device_label.as_deref(), Some("Freezer 01"));
                }
                other => panic!("expected SystemInfo event, got {other:?}"),
            },
            other => panic!("expected SystemInfo event, got {other:?}"),
        }
    }

    #[test]
    fn alarms_events_decodes() {
        let json = br#"{
            "timestamp": "2026-01-13T14:30:00Z", "line": 0, "name": "Sensor 1",
            "from_state": "NORMAL", "to_state": "WARNING", "temperature_celsius": 39.5,
            "event_type": "alarm_transition", "message": null, "alarm_type": null
        }"#;
        let result = route_publish("proximos/FIBER-001/alarms/events", json, PREFIX);
        match result {
            RouteResult::Event(event) => match *event {
                IngestEvent::AlarmEvent { hostname, payload } => {
                    assert_eq!(hostname, "fiber-001");
                    assert_eq!(payload.to_state, "WARNING");
                }
                other => panic!("expected AlarmEvent event, got {other:?}"),
            },
            other => panic!("expected AlarmEvent event, got {other:?}"),
        }
    }

    #[test]
    fn power_standby_and_power_events_standby_share_handler() {
        let json = br#"{
            "timestamp": "2026-08-05T12:00:00Z", "standby": true, "reason": "operator power_off",
            "requested_by": "dr.jane", "entered_at": "2026-08-05T12:00:00Z", "vin_mv": 0
        }"#;
        for topic in [
            "proximos/FIBER-001/power/standby",
            "proximos/FIBER-001/power/events/standby",
        ] {
            match route_publish(topic, json, PREFIX) {
                RouteResult::Event(event) => match *event {
                    IngestEvent::Standby { hostname, payload } => {
                        assert_eq!(hostname, "fiber-001");
                        assert!(payload.standby);
                        assert_eq!(payload.requested_by.as_deref(), Some("dr.jane"));
                    }
                    other => panic!("expected Standby event for {topic}, got {other:?}"),
                },
                other => panic!("expected Standby event for {topic}, got {other:?}"),
            }
        }
    }

    #[test]
    fn config_state_decodes() {
        let json = br#"{
            "timestamp": "2026-01-13T14:30:00Z", "led_brightness": 75,
            "sensors": [ { "line": 3, "name": "Freezer probe", "location": "Shelf A" } ]
        }"#;
        let result = route_publish("proximos/FIBER-001/config/state", json, PREFIX);
        match result {
            RouteResult::Event(event) => match *event {
                IngestEvent::ConfigState { hostname, payload } => {
                    assert_eq!(hostname, "fiber-001");
                    assert_eq!(payload.sensors.len(), 1);
                }
                other => panic!("expected ConfigState event, got {other:?}"),
            },
            other => panic!("expected ConfigState event, got {other:?}"),
        }
    }

    #[test]
    fn errors_decodes() {
        let json = br#"{"timestamp": "t", "command": "set_display_lines", "error": "invalid_params", "message": "too many lines"}"#;
        let result = route_publish("proximos/FIBER-001/errors", json, PREFIX);
        match result {
            RouteResult::Event(event) => match *event {
                IngestEvent::DeviceError { hostname, payload } => {
                    assert_eq!(hostname, "fiber-001");
                    assert_eq!(payload.command.as_deref(), Some("set_display_lines"));
                }
                other => panic!("expected DeviceError event, got {other:?}"),
            },
            other => panic!("expected DeviceError event, got {other:?}"),
        }
    }

    #[test]
    fn export_probe_1m_parses_line_from_topic() {
        let json = br#"{
            "message_id": "m1", "exported_at": 1736775060, "stream": "probe_1m", "stream_version": 1,
            "data": {
                "minute_ts": 1736775000, "sensor_line": 3,
                "min_c": 36.5, "avg_c": 36.8, "max_c": 37.2,
                "sample_count": 60, "disconnect_count": 0, "worst_alarm": "NORMAL", "data_hmac": null
            }
        }"#;
        let result = route_publish("proximos/FIBER-001/export/probe_1m/3", json, PREFIX);
        match result {
            RouteResult::Event(event) => match *event {
                IngestEvent::Probe1m {
                    hostname,
                    line,
                    payload,
                } => {
                    assert_eq!(hostname, "fiber-001");
                    assert_eq!(line, 3);
                    assert_eq!(payload.avg_c, Some(36.8));
                }
                other => panic!("expected Probe1m event, got {other:?}"),
            },
            other => panic!("expected Probe1m event, got {other:?}"),
        }
    }

    #[test]
    fn export_probe_1m_bad_line_is_decode_error() {
        let result = route_publish(
            "proximos/FIBER-001/export/probe_1m/not-a-number",
            b"{}",
            PREFIX,
        );
        assert!(matches!(result, RouteResult::DecodeError(_)));
    }

    #[test]
    fn export_non_probe_1m_streams_are_ignored() {
        for topic in [
            "proximos/FIBER-001/export/sticker/0011223344556677",
            "proximos/FIBER-001/export/eye/AA:BB:CC:DD:EE:FF",
            "proximos/FIBER-001/export/probe/0",
            "proximos/FIBER-001/export/alarm/0",
            "proximos/FIBER-001/export/probe_1m_replay/req-1/0",
        ] {
            assert!(
                matches!(
                    route_publish(topic, br#"{"message_id":"m1","data":{}}"#, PREFIX),
                    RouteResult::Ignored
                ),
                "topic {topic} should be silently ignored"
            );
        }
    }

    #[test]
    fn unhandled_topics_are_still_subscribed_but_unrouted() {
        for topic in [
            "proximos/FIBER-001/responses/sensor_config",
            "proximos/FIBER-001/pair/request",
            "proximos/FIBER-001/lorawan/sensors/aa11/unknown-subtopic",
            "proximos/FIBER-001/lorawan/sensors/",
        ] {
            assert!(
                matches!(route_publish(topic, b"{}", PREFIX), RouteResult::Unhandled),
                "topic {topic} should be Unhandled"
            );
        }
    }

    #[test]
    fn config_challenge_decodes() {
        let json = br#"{
            "challenge_id": "chal-1", "request_id": "req-1", "signer_id": "admin",
            "expires_at": 1736775060, "preview": {"command_type": "restart_application"}
        }"#;
        let result = route_publish("proximos/FIBER-001/config/challenge", json, PREFIX);
        match result {
            RouteResult::Event(event) => match *event {
                IngestEvent::ConfigChallenge { hostname, payload } => {
                    assert_eq!(hostname, "fiber-001");
                    assert_eq!(payload.challenge_id, "chal-1");
                    assert_eq!(payload.request_id.as_deref(), Some("req-1"));
                }
                other => panic!("expected ConfigChallenge event, got {other:?}"),
            },
            other => panic!("expected ConfigChallenge event, got {other:?}"),
        }
    }

    #[test]
    fn config_response_decodes() {
        let json = br#"{
            "challenge_id": "chal-1", "request_id": "req-1", "status": "SUCCESS",
            "applied_at": 1736775060, "effective_at": null, "message": "ok"
        }"#;
        let result = route_publish("proximos/FIBER-001/config/response", json, PREFIX);
        match result {
            RouteResult::Event(event) => match *event {
                IngestEvent::ConfigResponse { hostname, payload } => {
                    assert_eq!(hostname, "fiber-001");
                    assert_eq!(payload.status, "SUCCESS");
                }
                other => panic!("expected ConfigResponse event, got {other:?}"),
            },
            other => panic!("expected ConfigResponse event, got {other:?}"),
        }
    }

    #[test]
    fn pair_invite_decodes_and_carries_the_code_and_origin() {
        // GitLab #61. The one inbound pairing topic that is not a reply. It arrives under the
        // `{prefix}/+/pair/#` subscription that was already there, which is exactly why it used to
        // land in `Unhandled`.
        let json = br#"{"code": "23456X", "origin": "ble"}"#;
        match route_publish("proximos/FIBER-001/pair/invite", json, PREFIX) {
            RouteResult::Event(event) => match *event {
                IngestEvent::PairInvite { hostname, payload } => {
                    assert_eq!(
                        hostname, "fiber-001",
                        "the hostname comes from the topic, so a device can only invite a \
                         pairing with itself"
                    );
                    assert_eq!(payload.code.as_deref(), Some("23456X"));
                    assert_eq!(payload.origin.as_deref(), Some("ble"));
                }
                other => panic!("expected PairInvite event, got {other:?}"),
            },
            other => panic!("expected an event, got {other:?}"),
        }
    }

    #[test]
    fn a_pair_invite_with_no_code_still_decodes() {
        // The local path does not need one — the portal encrypts under the fixed local password
        // — so an absent code is a shape this must accept rather than a decode error.
        match route_publish("proximos/FIBER-001/pair/invite", b"{}", PREFIX) {
            RouteResult::Event(event) => match *event {
                IngestEvent::PairInvite { payload, .. } => {
                    assert_eq!(payload.code, None);
                    assert_eq!(payload.origin, None);
                }
                other => panic!("expected PairInvite event, got {other:?}"),
            },
            other => panic!("expected an event, got {other:?}"),
        }
    }

    #[test]
    fn pair_response_decodes() {
        let json = br#"{
            "request_id": "req-1", "success": true,
            "encrypted_private_key": {"ciphertext": "Y2lw", "salt": "c2Fs", "nonce": "bm9u"},
            "ca_public_key": "aa", "ca_id": "ca-1",
            "admin_certificate": {"signer_id": "admin"}, "error": null
        }"#;
        let result = route_publish("proximos/FIBER-001/pair/response", json, PREFIX);
        match result {
            RouteResult::Event(event) => match *event {
                IngestEvent::PairResponse { hostname, payload } => {
                    assert_eq!(hostname, "fiber-001");
                    assert!(payload.success);
                    assert_eq!(payload.ca_id.as_deref(), Some("ca-1"));
                }
                other => panic!("expected PairResponse event, got {other:?}"),
            },
            other => panic!("expected PairResponse event, got {other:?}"),
        }
    }

    #[test]
    fn empty_payload_on_config_challenge_and_response_and_pair_response_is_tombstone() {
        for topic in [
            "proximos/FIBER-001/config/challenge",
            "proximos/FIBER-001/config/response",
            "proximos/FIBER-001/pair/response",
        ] {
            assert!(
                matches!(route_publish(topic, b"", PREFIX), RouteResult::Tombstone),
                "topic {topic} should be Tombstone on empty payload"
            );
        }
    }

    #[test]
    fn unknown_topic_shape_is_unhandled() {
        let result = route_publish("proximos/FIBER-001/something/nobody/expects", b"{}", PREFIX);
        assert!(matches!(result, RouteResult::Unhandled));
    }

    #[test]
    fn malformed_json_is_decode_error() {
        let result = route_publish("proximos/FIBER-001/system/info", b"{not json", PREFIX);
        assert!(matches!(result, RouteResult::DecodeError(_)));
    }

    #[test]
    fn empty_payload_on_config_state_is_tombstone_not_decode_error() {
        let result = route_publish("proximos/FIBER-001/config/state", b"", PREFIX);
        assert!(matches!(result, RouteResult::Tombstone));
    }

    #[test]
    fn empty_payload_on_system_info_is_tombstone() {
        let result = route_publish("proximos/FIBER-001/system/info", b"", PREFIX);
        assert!(matches!(result, RouteResult::Tombstone));
    }

    #[test]
    fn topic_with_wrong_prefix_is_unhandled() {
        let result = route_publish("other/FIBER-001/status", b"online", PREFIX);
        assert!(matches!(result, RouteResult::Unhandled));
    }

    // ---- M5-E: lorawan/eye ------------------------------------------

    #[test]
    fn lorawan_sensors_decodes() {
        let json = br#"{
            "timestamp": "t",
            "sensors": [ {
                "dev_eui": "0011223344556677", "name": "Node 1", "serial_number": null,
                "location": null, "fields": { "temperature": 21.5 },
                "field_alarm_states": {}, "field_thresholds": null, "counters": {},
                "events": [], "rssi": null, "snr": null, "gateways": [],
                "fcnt": null, "dr": null, "downlink_gateway_id": null,
                "last_seen": "2026-01-13T14:30:00Z", "alarm_state": null
            } ]
        }"#;
        let result = route_publish("proximos/FIBER-001/lorawan/sensors", json, PREFIX);
        match result {
            RouteResult::Event(event) => match *event {
                IngestEvent::LorawanSensors { hostname, payload } => {
                    assert_eq!(hostname, "fiber-001");
                    assert_eq!(payload.sensors.len(), 1);
                }
                other => panic!("expected LorawanSensors event, got {other:?}"),
            },
            other => panic!("expected LorawanSensors event, got {other:?}"),
        }
    }

    #[test]
    fn lorawan_sensors_empty_payload_is_tombstone() {
        let result = route_publish("proximos/FIBER-001/lorawan/sensors", b"", PREFIX);
        assert!(matches!(result, RouteResult::Tombstone));
    }

    #[test]
    fn lorawan_gateways_is_ignored() {
        let result = route_publish(
            "proximos/FIBER-001/lorawan/gateways",
            br#"{"timestamp":"t","gateways":[]}"#,
            PREFIX,
        );
        assert!(matches!(result, RouteResult::Ignored));
    }

    #[test]
    fn eye_sensors_decodes() {
        let json = br#"{
            "timestamp": "t",
            "tags": [ {
                "gateway": null, "mac": "AA:BB:CC:DD:EE:FF", "name": null,
                "temperature_c": 21.5, "humidity_pct": null, "battery_mv": null,
                "low_battery": null, "magnet_present": null, "magnet_detected": null,
                "moving": null, "movement_count": null, "pitch_deg": null, "roll_deg": null,
                "rssi": null, "last_seen_ts": 1736775000, "stale": null,
                "provisioning": null, "discovered": null, "is_en12830": null,
                "field_alarm_states": {}, "alarm_state": null
            } ]
        }"#;
        let result = route_publish("proximos/FIBER-001/eye/sensors", json, PREFIX);
        match result {
            RouteResult::Event(event) => match *event {
                IngestEvent::EyeSensors { hostname, payload } => {
                    assert_eq!(hostname, "fiber-001");
                    assert_eq!(payload.tags.len(), 1);
                }
                other => panic!("expected EyeSensors event, got {other:?}"),
            },
            other => panic!("expected EyeSensors event, got {other:?}"),
        }
    }

    #[test]
    fn eye_sensors_empty_payload_is_tombstone() {
        let result = route_publish("proximos/FIBER-001/eye/sensors", b"", PREFIX);
        assert!(matches!(result, RouteResult::Tombstone));
    }

    #[test]
    fn eye_aggregated_decodes() {
        let json = br#"{
            "period_start_ts": 1791799200, "period_end_ts": 1791799800, "duration_sec": 600,
            "tags": [ {
                "mac": "AA:BB:CC:DD:EE:FF", "name": null,
                "message_id": "AA:BB:CC:DD:EE:FF-adv-1791799200",
                "window_start": 1791799200, "window_s": 600, "count": 60,
                "alarm_state": "NORMAL",
                "fields": { "temperature": { "min": 4.0, "avg": 4.5, "max": 5.0 } }
            } ]
        }"#;
        match route_publish("proximos/FIBER-001/eye/aggregated", json, PREFIX) {
            RouteResult::Event(event) => match *event {
                IngestEvent::EyeAggregated { hostname, payload } => {
                    assert_eq!(hostname, "fiber-001");
                    assert_eq!(payload.tags[0].avg("temperature"), Some(4.5));
                }
                other => panic!("expected EyeAggregated event, got {other:?}"),
            },
            other => panic!("expected EyeAggregated event, got {other:?}"),
        }
        assert!(matches!(
            route_publish("proximos/FIBER-001/eye/aggregated", b"", PREFIX),
            RouteResult::Tombstone
        ));
    }

    /// B5 (§20): the probe is gone from the portal, but the Hub still
    /// publishes its result. Ignored, not Unhandled — the distinction is
    /// what keeps a topic we deliberately stopped reading out of the
    /// "nobody routes this" log line on every publish.
    #[test]
    fn eye_detect_is_ignored_not_unhandled() {
        let json = br#"{"timestamp": "t", "mac": "AA:BB:CC:DD:EE:FF", "is_en12830": true, "status": "ok"}"#;
        assert!(matches!(
            route_publish("proximos/FIBER-001/eye/detect", json, PREFIX),
            RouteResult::Ignored
        ));
        assert!(matches!(
            route_publish("proximos/FIBER-001/eye/detect", b"", PREFIX),
            RouteResult::Ignored
        ));
    }

    /// beacon_history.md §2.2/§2.3, as the Hub publishes them (§8): every
    /// message carries `from`/`to`/`retry`/`reason` (null when unset), the
    /// `records` batch its rows, `done` keys the portal does not read.
    #[test]
    fn eye_history_decodes_progress_and_records() {
        let records = br#"{"job": "bh-1", "mac": "AA:BB:CC:DD:EE:FF", "state": "records",
            "from": null, "to": null, "retry": 0, "seq": 1,
            "records": [[1000, 4.5], [1900, null]]}"#;
        match route_publish("proximos/FIBER-001/eye/history", records, PREFIX) {
            RouteResult::Event(event) => match *event {
                IngestEvent::BeaconHistory { hostname, payload } => {
                    assert_eq!(hostname, "fiber-001");
                    assert_eq!(payload.seq, Some(1));
                    assert_eq!(payload.records, vec![(1000, Some(4.5)), (1900, None)]);
                }
                other => panic!("unexpected event {other:?}"),
            },
            _ => panic!("records message not routed"),
        }
        let done = br#"{"job": "bh-1", "mac": "AA:BB:CC:DD:EE:FF", "state": "done",
            "from": 1, "to": 2, "retry": 0, "reason": null, "received": 2, "expected": null,
            "batches": 1, "restart_ok": true, "sessions": 1, "rejected_chunks": 0,
            "future_dropped": 0, "crc_failed": null}"#;
        assert!(matches!(
            route_publish("proximos/FIBER-001/eye/history", done, PREFIX),
            RouteResult::Event(_)
        ));
    }

    #[test]
    fn lorawan_sensor_config_decodes_and_lowercases_dev_eui() {
        let json = br#"{"dev_eui": "AA11", "config": {}, "page_index": 0, "page_count": 1,
            "synced_at": null, "last_ack": null, "read_status": null, "missing": null,
            "error": null, "chunks_done": null, "chunks_total": null}"#;
        let result = route_publish(
            "proximos/FIBER-001/lorawan/sensors/AA11/config",
            json,
            PREFIX,
        );
        match result {
            RouteResult::Event(event) => match *event {
                IngestEvent::StickerConfig {
                    hostname, node_key, ..
                } => {
                    assert_eq!(hostname, "fiber-001");
                    assert_eq!(node_key, "aa11");
                }
                other => panic!("expected StickerConfig event, got {other:?}"),
            },
            other => panic!("expected StickerConfig event, got {other:?}"),
        }
    }

    #[test]
    fn lorawan_sensor_config_empty_payload_is_tombstone() {
        let result = route_publish(
            "proximos/FIBER-001/lorawan/sensors/aa11/config",
            b"",
            PREFIX,
        );
        assert!(matches!(result, RouteResult::Tombstone));
    }

    #[test]
    fn lorawan_sensor_full_config_decodes() {
        let json = br#"{"dev_eui": "aa11", "config": {}, "page_index": null, "page_count": null,
            "synced_at": null, "last_ack": null, "read_status": null, "missing": null,
            "error": null, "chunks_done": null, "chunks_total": null, "last_seq": 3}"#;
        let result = route_publish(
            "proximos/FIBER-001/lorawan/sensors/aa11/full-config",
            json,
            PREFIX,
        );
        match result {
            RouteResult::Event(event) => match *event {
                IngestEvent::StickerFullConfig {
                    hostname,
                    node_key,
                    payload,
                } => {
                    assert_eq!(hostname, "fiber-001");
                    assert_eq!(node_key, "aa11");
                    assert_eq!(payload.last_seq, Some(3));
                }
                other => panic!("expected StickerFullConfig event, got {other:?}"),
            },
            other => panic!("expected StickerFullConfig event, got {other:?}"),
        }
    }

    #[test]
    fn lorawan_sensor_full_config_empty_payload_is_tombstone() {
        let result = route_publish(
            "proximos/FIBER-001/lorawan/sensors/aa11/full-config",
            b"",
            PREFIX,
        );
        assert!(matches!(result, RouteResult::Tombstone));
    }

    #[test]
    fn lorawan_sensor_command_decodes_as_raw_value() {
        let json = br#"{"dev_eui": "aa11", "command": "sticker_reboot", "seq": 1, "result": "ok"}"#;
        let result = route_publish(
            "proximos/FIBER-001/lorawan/sensors/AA11/command",
            json,
            PREFIX,
        );
        match result {
            RouteResult::Event(event) => match *event {
                IngestEvent::StickerCommandResult {
                    hostname,
                    node_key,
                    payload,
                } => {
                    assert_eq!(hostname, "fiber-001");
                    assert_eq!(node_key, "aa11");
                    assert_eq!(payload["result"], "ok");
                }
                other => panic!("expected StickerCommandResult event, got {other:?}"),
            },
            other => panic!("expected StickerCommandResult event, got {other:?}"),
        }
    }

    #[test]
    fn lorawan_sensor_history_decodes() {
        let json = br#"{"dev_eui": "aa11", "frame_index": 0, "frame_count": 1,
            "records": [ {"time": 1736775000, "fields": {"temperature": 21.5}, "counters": {}} ]}"#;
        let result = route_publish(
            "proximos/FIBER-001/lorawan/sensors/AA11/history",
            json,
            PREFIX,
        );
        match result {
            RouteResult::Event(event) => match *event {
                IngestEvent::StickerHistory {
                    hostname,
                    node_key,
                    payload,
                } => {
                    assert_eq!(hostname, "fiber-001");
                    assert_eq!(node_key, "aa11");
                    assert_eq!(payload.records.len(), 1);
                }
                other => panic!("expected StickerHistory event, got {other:?}"),
            },
            other => panic!("expected StickerHistory event, got {other:?}"),
        }
    }

    #[test]
    fn lorawan_sensor_info_empty_payload_is_null_not_tombstone() {
        let result = route_publish("proximos/FIBER-001/lorawan/sensors/aa11/info", b"", PREFIX);
        match result {
            RouteResult::Event(event) => match *event {
                IngestEvent::StickerInfo {
                    hostname,
                    node_key,
                    payload,
                } => {
                    assert_eq!(hostname, "fiber-001");
                    assert_eq!(node_key, "aa11");
                    assert_eq!(payload, serde_json::Value::Null);
                }
                other => panic!("expected StickerInfo event, got {other:?}"),
            },
            other => panic!("expected StickerInfo event, got {other:?}"),
        }
    }

    #[test]
    fn lorawan_sensor_info_nonempty_decodes_as_raw_value() {
        let json = br#"{"dev_eui": "aa11", "seq": 3, "source": "unsolicited"}"#;
        let result = route_publish("proximos/FIBER-001/lorawan/sensors/AA11/info", json, PREFIX);
        match result {
            RouteResult::Event(event) => match *event {
                IngestEvent::StickerInfo {
                    hostname,
                    node_key,
                    payload,
                } => {
                    assert_eq!(hostname, "fiber-001");
                    assert_eq!(node_key, "aa11");
                    assert_eq!(payload["seq"], 3);
                }
                other => panic!("expected StickerInfo event, got {other:?}"),
            },
            other => panic!("expected StickerInfo event, got {other:?}"),
        }
    }

    #[test]
    fn lorawan_sensor_info_malformed_is_decode_error() {
        let result = route_publish(
            "proximos/FIBER-001/lorawan/sensors/aa11/info",
            b"{not json",
            PREFIX,
        );
        assert!(matches!(result, RouteResult::DecodeError(_)));
    }

    // ---- WP-E: the v2 radio northbound family ------------------------

    /// The envelope, as `proximos-control` publishes it. Serialized through the producer's
    /// own type so the fixture cannot drift from the wire.
    fn reading_json(message_id: &str, received_at: &str) -> Vec<u8> {
        let reading = NodeReadingMsg {
            message_id: message_id.to_string(),
            dev_eui: "58760700C0668FC7".to_string(),
            serial: Some("SN-1".to_string()),
            fields: [("temperature".to_string(), 21.5)].into_iter().collect(),
            counters: [("hall_left_count".to_string(), 7u64)]
                .into_iter()
                .collect(),
            events: Vec::new(),
            gateways: vec![control_radio::decode::reading::GatewayRx {
                gateway_id: "24e124fffefd3bda".to_string(),
                rssi: Some(-41),
                snr: Some(13.5),
            }],
            dr: Some(5),
            rssi: Some(-41),
            snr: Some(13.5),
            received_at: received_at.to_string(),
            fport: Some(2),
            page_index: None,
            page_count: None,
        };
        serde_json::to_vec(&reading).expect("serializes")
    }

    #[test]
    fn a_radio_reading_routes_and_lowercases_the_topic_identifier() {
        let json = reading_json("dedup-1", "2026-08-27T09:59:19Z");
        // The topic segment carries the wire's own casing; the portal's node_key is lowercase.
        let result = route_publish(
            "proximos/proximos-a1b2c3d4/nodes/58760700C0668FC7/reading",
            &json,
            PREFIX,
        );
        match result {
            RouteResult::Event(event) => match *event {
                IngestEvent::NodeReading {
                    hostname,
                    node_key,
                    payload,
                } => {
                    assert_eq!(hostname, "proximos-a1b2c3d4");
                    assert_eq!(node_key, "58760700c0668fc7");
                    assert_eq!(payload.message_id, "dedup-1");
                    assert_eq!(payload.fields.get("temperature"), Some(&21.5));
                }
                other => panic!("expected NodeReading, got {other:?}"),
            },
            other => panic!("expected NodeReading, got {other:?}"),
        }
    }

    #[test]
    fn a_radio_alarm_routes_to_its_own_variant() {
        let json = reading_json("alarm-1", "2026-08-27T09:59:19Z");
        match route_publish("proximos/hub-1/nodes/aa11/alarm", &json, PREFIX) {
            RouteResult::Event(event) => match *event {
                IngestEvent::NodeAlarm { node_key, .. } => assert_eq!(node_key, "aa11"),
                other => panic!("expected NodeAlarm, got {other:?}"),
            },
            other => panic!("expected NodeAlarm, got {other:?}"),
        }
    }

    /// The retained inventory document, and its tombstone. An empty payload here is **data**
    /// (`Value::Null`, the removal signal), not a tombstone-shaped nothing to skip — the same
    /// contract `lorawan/sensors/{dev_eui}/info` has.
    #[test]
    fn radio_info_upserts_and_an_empty_retained_payload_is_a_removal() {
        match route_publish(
            "proximos/hub-1/nodes/aa11/info",
            br#"{"serial_number":"SN-1","dev_eui":"aa11","transport":"lorawan"}"#,
            PREFIX,
        ) {
            RouteResult::Event(event) => match *event {
                IngestEvent::NodeInventory {
                    node_key, payload, ..
                } => {
                    assert_eq!(node_key, "aa11");
                    assert_eq!(payload["serial_number"], "SN-1");
                }
                other => panic!("expected NodeInventory, got {other:?}"),
            },
            other => panic!("expected NodeInventory, got {other:?}"),
        }

        match route_publish("proximos/hub-1/nodes/aa11/info", b"", PREFIX) {
            RouteResult::Event(event) => match *event {
                IngestEvent::NodeInventory { payload, .. } => {
                    assert_eq!(
                        payload,
                        serde_json::Value::Null,
                        "the tombstone must arrive as Null, not be skipped"
                    );
                }
                other => panic!("expected NodeInventory, got {other:?}"),
            },
            other => panic!("expected a removal event, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_radio_reading_is_a_tombstone_and_malformed_json_is_a_decode_error() {
        assert!(matches!(
            route_publish("proximos/hub-1/nodes/aa11/reading", b"", PREFIX),
            RouteResult::Tombstone
        ));
        assert!(matches!(
            route_publish("proximos/hub-1/nodes/aa11/reading", b"{not json", PREFIX),
            RouteResult::DecodeError(_)
        ));
    }

    /// Command results have no portal consumer on this family, so they are skipped silently
    /// rather than counted as unhandled — a Node under configuration must not fill the
    /// diagnostics counters.
    #[test]
    fn the_radio_command_area_routes_the_nodes_answers() {
        match route_publish(
            "proximos/hub-1/nodes/AA11/command",
            br#"{"kind":"ack","seq":22}"#,
            PREFIX,
        ) {
            RouteResult::Event(event) => match *event {
                IngestEvent::NodeCommand {
                    node_key, payload, ..
                } => {
                    assert_eq!(node_key, "aa11");
                    assert_eq!(payload["seq"], 22);
                }
                other => panic!("expected NodeCommand, got {other:?}"),
            },
            other => panic!("expected an event, got {other:?}"),
        }
        assert!(matches!(
            route_publish("proximos/hub-1/nodes/aa11/command", b"", PREFIX),
            RouteResult::Tombstone
        ));
    }

    /// p2p_history_replay.md §14: control-radio's decoded replay frame is the v1
    /// `NodeHistoryMsg` shape, extra keys and all, on its own v2 variant.
    #[test]
    fn the_radio_history_area_routes_a_replay_frame() {
        let doc = br#"{"dev_eui":"5876070000000413","seq":111,"frame_index":0,"frame_count":1,
            "interval_s":60,"records":[{"time":1790319991,
            "fields":{"temperature":27.07,"humidity":53.0},"counters":{}}]}"#;
        match route_publish(
            "proximos/fiber-4e970f4a/nodes/5876070000000413/history",
            doc,
            PREFIX,
        ) {
            RouteResult::Event(event) => match *event {
                IngestEvent::NodeHistory {
                    hostname,
                    node_key,
                    payload,
                } => {
                    assert_eq!(hostname, "fiber-4e970f4a");
                    assert_eq!(node_key, "5876070000000413");
                    assert_eq!(payload.frame_count, Some(1));
                    assert_eq!(payload.records.len(), 1);
                    assert_eq!(payload.records[0].time, Some(1_790_319_991));
                    assert_eq!(payload.records[0].fields["temperature"], Some(27.07));
                }
                other => panic!("expected NodeHistory, got {other:?}"),
            },
            other => panic!("expected an event, got {other:?}"),
        }
        assert!(matches!(
            route_publish("proximos/hub-1/nodes/aa11/history", b"", PREFIX),
            RouteResult::Tombstone
        ));
    }

    #[test]
    fn radio_device_info_routes_and_an_empty_retained_payload_is_a_removal() {
        match route_publish(
            "proximos/hub-1/nodes/AA11/device_info",
            br#"{"dev_eui":"aa11","received_at":"2026-09-22T19:09:04Z",
                 "device_info":{"fw_version":"1.4.0","battery":3458}}"#,
            PREFIX,
        ) {
            RouteResult::Event(event) => match *event {
                IngestEvent::NodeDeviceInfo {
                    hostname,
                    node_key,
                    payload,
                } => {
                    assert_eq!(hostname, "hub-1");
                    assert_eq!(node_key, "aa11", "lowercased like every node_key");
                    let doc = payload.expect("a document");
                    assert_eq!(doc.received_at, "2026-09-22T19:09:04Z");
                    assert_eq!(doc.device_info["fw_version"], "1.4.0");
                }
                other => panic!("expected NodeDeviceInfo, got {other:?}"),
            },
            other => panic!("expected an event, got {other:?}"),
        }
        match route_publish("proximos/hub-1/nodes/aa11/device_info", b"", PREFIX) {
            RouteResult::Event(event) => assert!(matches!(
                *event,
                IngestEvent::NodeDeviceInfo { payload: None, .. }
            )),
            other => panic!("the tombstone must arrive as a removal, got {other:?}"),
        }
        assert!(matches!(
            route_publish(
                "proximos/hub-1/nodes/aa11/device_info",
                b"{not json",
                PREFIX
            ),
            RouteResult::DecodeError(_)
        ));
    }

    #[test]
    fn radio_config_routes_and_an_empty_retained_payload_is_a_removal() {
        match route_publish(
            "proximos/hub-1/nodes/aa11/config",
            br#"{"dev_eui":"aa11","received_at":"2026-09-22T19:09:09Z","seq":0,
                 "page_count":1,"config":{"application.interval_report":900},
                 "w1_slot_type":["dallas","empty"],"body_hex":"1001"}"#,
            PREFIX,
        ) {
            RouteResult::Event(event) => match *event {
                IngestEvent::NodeDeviceConfig {
                    node_key, payload, ..
                } => {
                    assert_eq!(node_key, "aa11");
                    let doc = payload.expect("a document");
                    assert_eq!(doc.config["application.interval_report"], 900);
                    assert_eq!(
                        doc.w1_slot_type,
                        Some(vec!["dallas".to_string(), "empty".to_string()])
                    );
                }
                other => panic!("expected NodeDeviceConfig, got {other:?}"),
            },
            other => panic!("expected an event, got {other:?}"),
        }
        match route_publish("proximos/hub-1/nodes/aa11/config", b"", PREFIX) {
            RouteResult::Event(event) => assert!(matches!(
                *event,
                IngestEvent::NodeDeviceConfig { payload: None, .. }
            )),
            other => panic!("the tombstone must arrive as a removal, got {other:?}"),
        }
    }

    /// The radio family is checked before the prefix-based tree, so a topic that is *nearly*
    /// one of ours must not be swallowed by it — it falls through to the prefix tree and is
    /// judged there.
    ///
    /// **P5 changed what "judged there" means.** The two families used to have different
    /// roots (`proximos/` for radio, `fiber/` for everything else), so a radio near-miss
    /// fell through to a tree that could not match it either and came back `Unhandled`.
    /// Now that both roots are `proximos` (design.md §9), the fall-through lands *inside*
    /// the prefix tree, and `lorawan/gateways` is a topic that tree has a deliberate answer
    /// for: `Ignored`, per the team agreement of 2026-08-18 — external gateways are out of
    /// phase-1 scope, and `lorawan_gateways_is_ignored` below is where that is pinned.
    ///
    /// That is the right answer and not a regression: the same topic reached the same
    /// decision before, on the portal's own root. What the shared root removed is the
    /// accident that a radio-shaped topic could never be anything but unhandled.
    #[test]
    fn near_miss_radio_topics_fall_through_to_the_prefix_tree() {
        for topic in [
            "proximos/hub-1/lorawan/nodes/aa11/reading/extra",
            "proximos/hub-1/nodes/aa11/reading/extra",
            "proximos/hub-1/zigbee/nodes/aa11/reading",
            // The pre-#73 shape with a transport level: gone from the wire, not tolerated.
            "proximos/hub-1/lorawan/nodes/aa11/reading",
            "proximos/hub-1/p2p/nodes/aa11/info",
        ] {
            assert!(
                matches!(route_publish(topic, b"{}", PREFIX), RouteResult::Unhandled),
                "{topic}"
            );
        }
        assert!(
            matches!(
                route_publish("proximos/hub-1/lorawan/gateways", b"{}", PREFIX),
                RouteResult::Ignored
            ),
            "the prefix tree has its own answer for this one now that the roots agree"
        );
        // And the legacy path is untouched.
        assert_status(
            route_publish("proximos/FIBER-001/status", b"online", PREFIX),
            "fiber-001",
            DeviceStatus::Online,
        );
    }

    /// `radio/gateways` (devices.hub.md §12 A2): decoded on the fixed topic, cleared by the
    /// empty retained payload, rejected loudly on malformed JSON — and the v1 `lorawan/gateways`
    /// topic is still deliberately ignored, not confused with it.
    #[test]
    fn radio_gateways_decodes_clears_and_rejects() {
        let json = br#"{
            "timestamp": "2026-09-15T10:00:00Z",
            "api_status": "auth_failed",
            "gateways": [ {
                "gateway_eui": "70B3D54900AA0001", "name": "Local", "status": "online",
                "last_seen": "2026-09-15T09:59:41Z", "rssi": -67, "snr": 9.5,
                "radio": "lorawan"
            } ]
        }"#;
        match route_publish("radio/gateways", json, PREFIX) {
            RouteResult::Event(event) => match *event {
                IngestEvent::RadioGateways { report } => {
                    let report = report.expect("a report, not a clear");
                    assert_eq!(
                        report.api_status,
                        crate::state::radio::ApiStatus::AuthFailed
                    );
                    assert_eq!(report.gateways.len(), 1);
                    // Routing keeps the wire casing; the store lowercases it.
                    assert_eq!(report.gateways[0].gateway_eui, "70B3D54900AA0001");
                    assert_eq!(report.gateways[0].rssi, Some(-67));
                }
                other => panic!("expected RadioGateways, got {other:?}"),
            },
            other => panic!("expected RadioGateways, got {other:?}"),
        }

        match route_publish("radio/gateways", b"", PREFIX) {
            RouteResult::Event(event) => assert!(
                matches!(*event, IngestEvent::RadioGateways { report: None }),
                "an empty retained payload clears the report, it is not skipped"
            ),
            other => panic!("expected a clearing RadioGateways, got {other:?}"),
        }

        assert!(matches!(
            route_publish("radio/gateways", b"{\"api_status\":", PREFIX),
            RouteResult::DecodeError(_)
        ));
    }

    #[test]
    fn unknown_lorawan_sensor_subtopic_is_unhandled() {
        let result = route_publish(
            "proximos/FIBER-001/lorawan/sensors/aa11/whatever",
            b"{}",
            PREFIX,
        );
        assert!(matches!(result, RouteResult::Unhandled));
    }
}
