//! ChirpStack's MQTT integration events — the LoRaWAN entry point of the §7 decode path.
//!
//! # The marshaler decision, and the evidence for it
//!
//! ChirpStack's `[integration.mqtt]` can marshal its events as protobuf or as JSON. **v2 takes
//! JSON, and deserializes it with `chirpstack_api`'s own generated model** rather than a
//! hand-written serde mirror of the fields we happen to use.
//!
//! The `json` feature exists at the pinned 4.19.0 and is **on by default** — verified in the
//! vendored manifest (`[features] json = ["pbjson", "pbjson-types", "serde"]`, and `default =
//! ["api", "json"]`), and `src/integration.rs` includes the generated
//! `integration.serde.rs` under `#[cfg(feature = "json")]`. `build.rs` runs `pbjson_build`
//! over the `.integration` and `.gw` descriptor sets with `.ignore_unknown_fields()`, so the
//! generated `Deserialize` tolerates fields a future ChirpStack adds instead of failing the
//! whole event. `control-radio` already depends on `chirpstack_api = "4.19.0"` with default
//! features, so **this costs no Cargo change at all** — the impls are already linked in.
//!
//! Both sides of the wire are then the *same* pbjson mapping: ChirpStack serializes the
//! protobuf message through pbjson and this deserializes it through pbjson, so camelCase
//! field names, base64 `bytes`, RFC3339 `Timestamp`s and enum-by-name all round-trip by
//! construction rather than by our reading of a docs sample.
//!
//! Consequence for the renderer: `chirpstack.toml` must carry `[integration.mqtt] json =
//! true`. The template being written alongside this work package
//! (`portal/packaging/templates/chirpstack/chirpstack.toml.j2`) already does, and
//! `tests/integration/chirpstack.toml` has since the compose stack was written — so there is
//! no mismatch to reconcile. A Hub rendered with `json = false` would deliver protobuf on the
//! same topics and every event here would fail to parse; [`parse_uplink`] therefore reports
//! that possibility by name rather than as a bare parse error.
//!
//! # Why a module at all, if the types are generated
//!
//! Three things the generated types do not give us, and that every consumer would otherwise
//! re-invent: the topic grammar (`application/{app_id}/device/{dev_eui}/event/{kind}`),
//! the normalization of `deviceInfo.devEui` into an [`Eui`], and `Timestamp` → the RFC3339
//! spelling the northbound envelope uses. They live here so `tasks/uplink.rs` deals in
//! `control_radio` vocabulary and touches `chirpstack_api` nowhere.

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::lorawan::eui::Eui;
use serde_json::Value;

/// ChirpStack's OTAA join. Modelled so the LoRaWAN `info` carries the session's `dev_addr` and
/// `joined_at` as the P2P one does (node_registry.md §5, amended 2026-09-28, Q-N2).
pub use chirpstack_api::integration::JoinEvent;
/// The downlink transmission acknowledgement. Modelled for the same reason `up` is: the
/// fPort-85 command engine needs to know a downlink actually left the gateway, and `txack` is
/// the only event that says so.
pub use chirpstack_api::integration::TxAckEvent;
/// ChirpStack's uplink event. Re-exported rather than mirrored — see the module note.
pub use chirpstack_api::integration::UplinkEvent;

/// Which `event/{kind}` topic a message arrived on.
///
/// Closed, like every other vocabulary in this crate: an event ChirpStack adds later is
/// [`EventKind::Other`] and is skipped by name, never silently routed into the uplink path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    /// `event/up` — an uplink, the §7 decode path's input.
    Up,
    /// `event/txack` — a downlink left the gateway.
    TxAck,
    /// `event/join` — ChirpStack accepted an OTAA JoinRequest and assigned a `devAddr`.
    Join,
    /// `event/log` — ChirpStack's own report about the device, e.g. a queued downlink it
    /// discarded at transmit time (#129).
    Log,
    /// `event/status`, `event/ack`, … — not consumed in phase 1.
    Other(String),
}

impl EventKind {
    pub fn as_str(&self) -> &str {
        match self {
            EventKind::Up => "up",
            EventKind::TxAck => "txack",
            EventKind::Join => "join",
            EventKind::Log => "log",
            EventKind::Other(s) => s.as_str(),
        }
    }
}

/// One event topic, taken apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventTopic {
    pub application_id: String,
    /// The topic's `{dev_eui}` segment, **as written**. Deliberately not an [`Eui`]: the
    /// authoritative identity is `deviceInfo.devEui` inside the payload, and a topic whose
    /// segment is malformed must not stop the payload from being read.
    pub dev_eui: String,
    pub kind: EventKind,
}

/// Parse `application/{app_id}/device/{dev_eui}/event/{kind}`.
///
/// `None` for anything that is not that shape — the daemon shares one broker connection with
/// the gateway-state subscription, so "not an integration event" is an ordinary outcome and
/// not an error.
pub fn parse_event_topic(topic: &str) -> Option<EventTopic> {
    let parts: Vec<&str> = topic.split('/').collect();
    if parts.len() != 6 {
        return None;
    }
    if parts[0] != "application" || parts[2] != "device" || parts[4] != "event" {
        return None;
    }
    if parts[1].is_empty() || parts[3].is_empty() || parts[5].is_empty() {
        return None;
    }
    Some(EventTopic {
        application_id: parts[1].to_string(),
        dev_eui: parts[3].to_string(),
        kind: match parts[5] {
            "up" => EventKind::Up,
            "txack" => EventKind::TxAck,
            "join" => EventKind::Join,
            "log" => EventKind::Log,
            other => EventKind::Other(other.to_string()),
        },
    })
}

fn parse_err(what: &str, e: serde_json::Error) -> Error {
    Error::new(
        Kind::Transport,
        Transport::Lorawan,
        Component::ChirpstackService,
        format!(
            "cannot read a ChirpStack {what} event as JSON: {e}; chirpstack.toml must set \
             [integration.mqtt] json = true — with the protobuf marshaler this topic carries \
             binary and nothing here can decode it"
        ),
    )
}

/// ChirpStack's numbers, restated as strings so pbjson can still read them when
/// `serde_json` is built with `arbitrary_precision`.
///
/// `control-core` enables that feature deliberately — it verifies MDR command envelopes
/// against the wire's own number text, so a number must not round-trip through `f64`
/// (`fiber-protocol::envelope`, "float parity"). Cargo unifies features per package across
/// the workspace, so this crate is compiled with it too whenever the two are linked, which
/// `cargo test --workspace` already does and `proximos-control` will once `control-core`
/// joins that process.
///
/// Under the feature, `deserialize_any` hands a number to the visitor as a one-key map
/// (`$serde_json::private::Number`), and `pbjson::private::NumberDeserialize` is an
/// untagged `string | number` enum, so it matches neither variant: every numeric field of
/// every event fails to parse. Proto3's JSON mapping accepts a number written as a string
/// for every numeric field and pbjson implements that branch, so restating the numbers
/// first makes the parse produce the same event with the feature or without it.
fn numbers_as_strings(v: &mut Value) {
    match v {
        Value::Number(n) => *v = Value::String(n.to_string()),
        Value::Array(items) => items.iter_mut().for_each(numbers_as_strings),
        Value::Object(fields) => fields.values_mut().for_each(numbers_as_strings),
        _ => {}
    }
}

/// Deserialize an `event/up` payload.
pub fn parse_uplink(payload: &[u8]) -> Result<UplinkEvent> {
    let mut v: Value = serde_json::from_slice(payload).map_err(|e| parse_err("uplink", e))?;
    // `object` is ChirpStack's codec output, a `google.protobuf.Struct` whose numbers are
    // genuine `f64`s — the one place where the restatement above would change a value's
    // type rather than its spelling. This Hub decodes frames itself (§7) and reads the
    // field nowhere, and under `arbitrary_precision` the Struct cannot be deserialized at
    // all, so it is dropped rather than carried in mistyped. Configure a codec on the
    // device profile and this is the line to revisit.
    if let Value::Object(fields) = &mut v {
        fields.remove("object");
    }
    numbers_as_strings(&mut v);
    serde_json::from_value(v).map_err(|e| parse_err("uplink", e))
}

/// Deserialize an `event/txack` payload.
pub fn parse_txack(payload: &[u8]) -> Result<TxAckEvent> {
    let mut v: Value = serde_json::from_slice(payload).map_err(|e| parse_err("txack", e))?;
    numbers_as_strings(&mut v);
    serde_json::from_value(v).map_err(|e| parse_err("txack", e))
}

/// Deserialize an `event/join` payload.
pub fn parse_join(payload: &[u8]) -> Result<JoinEvent> {
    let mut v: Value = serde_json::from_slice(payload).map_err(|e| parse_err("join", e))?;
    numbers_as_strings(&mut v);
    serde_json::from_value(v).map_err(|e| parse_err("join", e))
}

/// ChirpStack's `event/log`, the fields this Hub reads.
///
/// Not the generated `LogEvent`, unlike the other events: its `level` and `code` are proto
/// enums, and pbjson refuses an enum name it does not know. ChirpStack has grown that list
/// release by release (`EXPIRED`, `F_CNT_DOWN`, …), and a log event whose code arrived after
/// 4.19.0 must still be logged rather than fail to parse. So both stay strings, spelled as
/// the JSON marshaler writes them; a field the marshaler omits (proto3 drops a default, so
/// `INFO` and `UNKNOWN` can be absent) reads as that default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEvent {
    /// `INFO`, `WARNING` or `ERROR`.
    pub level: String,
    /// e.g. [`LOG_CODE_DOWNLINK_PAYLOAD_SIZE`].
    pub code: String,
    pub description: String,
    /// ChirpStack's `context` map, e.g. `max_payload_size`, `item_size`, `queue_item_id`.
    pub context: std::collections::BTreeMap<String, String>,
    /// `deviceInfo.devEui`, normalized; by the same rule as [`uplink_dev_eui`].
    pub dev_eui: Eui,
    /// The event's `time`, RFC3339, when it carries one.
    pub time: Option<String>,
}

/// The log code ChirpStack raises when it deletes a queue item larger than the data rate's
/// maximum payload, at the moment it would have transmitted it.
pub const LOG_CODE_DOWNLINK_PAYLOAD_SIZE: &str = "DOWNLINK_PAYLOAD_SIZE";

impl LogEvent {
    /// A `context` value read as a number, `None` when absent or not one.
    pub fn context_u64(&self, key: &str) -> Option<u64> {
        self.context.get(key).and_then(|v| v.trim().parse().ok())
    }
}

/// Deserialize an `event/log` payload.
pub fn parse_log(payload: &[u8]) -> Result<LogEvent> {
    let v: Value = serde_json::from_slice(payload).map_err(|e| parse_err("log", e))?;
    let text = |key: &str, default: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .unwrap_or(default)
            .to_string()
    };
    // The map's values are strings in the proto; a number is taken as written rather than
    // refused, since nothing here depends on the distinction.
    let context = v
        .get("context")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, v)| {
                    let s = match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    (k.clone(), s)
                })
                .collect()
        })
        .unwrap_or_default();
    let raw_eui = v
        .get("deviceInfo")
        .and_then(|d| d.get("devEui"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if raw_eui.is_empty() {
        return Err(Error::new(
            Kind::InvalidArgument,
            Transport::Lorawan,
            Component::ChirpstackService,
            "the log event carries no deviceInfo.devEui, so there is no device to attribute \
             it to"
                .to_string(),
        ));
    }
    let time = match v.get("time") {
        Some(Value::String(t)) => Some(
            serde_json::from_value::<chirpstack_api::pbjson_types::Timestamp>(Value::String(
                t.clone(),
            ))
            .map(|ts| format_timestamp(&ts))
            .unwrap_or_else(|_| t.clone()),
        ),
        _ => None,
    };
    Ok(LogEvent {
        level: text("level", "INFO"),
        code: text("code", "UNKNOWN"),
        description: text("description", ""),
        context,
        dev_eui: Eui::parse(raw_eui)?,
        time,
    })
}

/// The uplink's wire identity, normalized.
///
/// Read from `deviceInfo.devEui`, never from the topic: the payload is what ChirpStack
/// actually decoded the frame as, and a subscriber that trusted the topic segment would be
/// trusting a string a misconfigured bridge could rewrite.
pub fn uplink_dev_eui(ev: &UplinkEvent) -> Result<Eui> {
    device_dev_eui(ev.device_info.as_ref(), "uplink")
}

/// The join's wire identity, by the same rule as [`uplink_dev_eui`].
pub fn join_dev_eui(ev: &JoinEvent) -> Result<Eui> {
    device_dev_eui(ev.device_info.as_ref(), "join")
}

fn device_dev_eui(
    info: Option<&chirpstack_api::integration::DeviceInfo>,
    what: &str,
) -> Result<Eui> {
    let raw = info.map(|d| d.dev_eui.as_str()).unwrap_or("");
    if raw.is_empty() {
        return Err(Error::new(
            Kind::InvalidArgument,
            Transport::Lorawan,
            Component::ChirpstackService,
            format!(
                "the {what} event carries no deviceInfo.devEui, so there is no device to \
                 attribute it to"
            ),
        ));
    }
    Eui::parse(raw)
}

/// The `devAddr` a join assigned, in ChirpStack's own spelling: 8 lowercase hex digits.
///
/// Kept as text, not a number: it is what ChirpStack's UI and log print, so an operator can
/// search for it as read off the Portal. Case is folded because nothing guarantees it on the
/// wire and two spellings of one address would read as two sessions. Anything that is not 32
/// bits of hex is refused, since a session with no address is not a join.
pub fn join_dev_addr(ev: &JoinEvent) -> Result<String> {
    let raw = ev.dev_addr.trim();
    if raw.len() == 8 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(raw.to_ascii_lowercase());
    }
    Err(Error::new(
        Kind::InvalidArgument,
        Transport::Lorawan,
        Component::ChirpstackService,
        format!("the join event's devAddr {raw:?} is not 8 hex digits"),
    ))
}

/// When the Node joined, as the northbound envelope spells it. `None` when the event carries
/// no `time`, as for an uplink ([`uplink_received_at`]).
pub fn join_time(ev: &JoinEvent) -> Option<String> {
    ev.time.as_ref().map(format_timestamp)
}

/// `Timestamp` → the RFC3339 spelling the northbound envelope uses.
///
/// Sub-second precision is kept when the event carries it. That is not cosmetic: `received_at`
/// is half of the `message_id` derivation ([`crate::decode::reading`]), and truncating to whole
/// seconds would collapse two genuinely distinct uplinks inside one second into one record.
/// Microseconds, because that is the precision ChirpStack emits.
pub fn format_timestamp(ts: &chirpstack_api::pbjson_types::Timestamp) -> String {
    let base = crate::lorawan::gateway::format_rfc3339(ts.seconds);
    if ts.nanos <= 0 {
        return base;
    }
    let micros = (ts.nanos as u32) / 1_000;
    if micros == 0 {
        return base;
    }
    // `format_rfc3339` always ends in `Z`; splice the fraction in before it.
    match base.strip_suffix('Z') {
        Some(head) => format!("{head}.{micros:06}Z"),
        None => base,
    }
}

/// When the uplink was received, as the northbound envelope spells it.
///
/// `None` when the event carries no `time` at all. The caller decides what to do about it —
/// [`crate::tasks::uplink`] substitutes the local clock, because a reading with no timestamp
/// is worse than one stamped a few milliseconds late.
pub fn uplink_received_at(ev: &UplinkEvent) -> Option<String> {
    ev.time.as_ref().map(format_timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    const UP: &str = include_str!("../../tests/fixtures/chirpstack-up-event.json");
    const JOIN: &str = include_str!("../../tests/fixtures/chirpstack-join-event.json");
    const LOG: &str = include_str!("../../tests/fixtures/chirpstack-log-event.json");

    #[test]
    fn the_topic_grammar_is_the_documented_one() {
        let t = parse_event_topic("application/app-1/device/70b3d57ed80051b2/event/up")
            .expect("matches");
        assert_eq!(t.application_id, "app-1");
        assert_eq!(t.dev_eui, "70b3d57ed80051b2");
        assert_eq!(t.kind, EventKind::Up);

        let t = parse_event_topic("application/app-1/device/aa/event/txack").expect("matches");
        assert_eq!(t.kind, EventKind::TxAck);

        let t = parse_event_topic("application/app-1/device/aa/event/join").expect("matches");
        assert_eq!(t.kind, EventKind::Join);
        assert_eq!(t.kind.as_str(), "join");

        // A kind we do not consume is named, not guessed at.
        let t = parse_event_topic("application/app-1/device/aa/event/status").expect("matches");
        assert_eq!(t.kind, EventKind::Other("status".into()));
        assert_eq!(t.kind.as_str(), "status");
    }

    /// The daemon shares one connection with the gateway-state subscription, so a topic that
    /// is not an integration event is an ordinary outcome — `None`, never a parse error.
    #[test]
    fn unrelated_topics_do_not_match() {
        for t in [
            "eu868/gateway/0011223344556677/state/conn",
            "application/app-1/device/aa/event",
            "application/app-1/device/aa/event/up/extra",
            "application//device/aa/event/up",
            "proximos/hub/nodes/aa/reading",
            "",
        ] {
            assert!(parse_event_topic(t).is_none(), "must not match: {t:?}");
        }
    }

    /// The whole marshaler decision, asserted against a realistic 4.x event rather than
    /// argued from the manifest: base64 `data`, camelCase names, a `+00:00` offset timestamp,
    /// enum-by-name (`crcStatus`, `deviceClassEnabled`) and a float `snr` all have to land.
    #[test]
    fn a_realistic_chirpstack_4x_up_event_deserializes() {
        let ev = parse_uplink(UP.as_bytes()).expect("the json marshaler's own shape");

        assert_eq!(ev.f_port, 2);
        assert_eq!(ev.dr, 5);
        assert_eq!(ev.f_cnt, 8);
        assert_eq!(ev.deduplication_id, "ee59a0b7-6b0a-4b23-a4d0-3f1c15a4b1a2");
        // `data` is base64 on the wire and bytes here — the version byte plus the frame.
        assert_eq!(ev.data.first(), Some(&0x01));
        assert_eq!(ev.data.len(), 18);

        assert_eq!(
            uplink_dev_eui(&ev).expect("normalizes").as_str(),
            "70b3d57ed80051b2"
        );

        assert_eq!(ev.rx_info.len(), 2, "both receivers survive");
        assert_eq!(ev.rx_info[0].gateway_id, "24e124fffefd3bda");
        assert_eq!(ev.rx_info[0].rssi, -41);
        assert!((ev.rx_info[0].snr - 13.5).abs() < 1e-6);
        assert!((ev.rx_info[1].snr - 4.25).abs() < 1e-6);
    }

    /// `ignore_unknown_fields()` is what keeps a ChirpStack point release from taking the
    /// decode path down; asserted, not assumed.
    #[test]
    fn an_unknown_field_does_not_fail_the_event() {
        let mut v: serde_json::Value = serde_json::from_str(UP).expect("fixture parses");
        v.as_object_mut()
            .expect("object")
            .insert("somethingNewIn420".into(), serde_json::json!({"a": 1}));
        let raw = serde_json::to_vec(&v).expect("re-serializes");
        parse_uplink(&raw).expect("an unknown field is tolerated, not fatal");
    }

    /// The protobuf marshaler on the same topic is the one misconfiguration that makes every
    /// uplink vanish, so the refusal has to name it.
    #[test]
    fn a_protobuf_payload_is_refused_by_name() {
        let err = parse_uplink(&[0x0a, 0x24, 0xff, 0xfe]).expect_err("not JSON");
        assert_eq!(err.kind(), Kind::Transport);
        assert!(
            err.message().contains("json = true"),
            "the remedy is named: {}",
            err.message()
        );
    }

    #[test]
    fn an_event_without_a_dev_eui_is_refused_rather_than_attributed_to_nothing() {
        let mut v: serde_json::Value = serde_json::from_str(UP).expect("fixture parses");
        v["deviceInfo"]["devEui"] = serde_json::json!("");
        let raw = serde_json::to_vec(&v).expect("re-serializes");
        let ev = parse_uplink(&raw).expect("still an event");
        let err = uplink_dev_eui(&ev).expect_err("no identity");
        assert_eq!(err.kind(), Kind::InvalidArgument);
    }

    /// Sub-second precision is kept: it is half of the `message_id` derivation, and whole
    /// seconds would collapse two distinct uplinks in the same second into one record.
    #[test]
    fn the_timestamp_keeps_its_sub_second_precision() {
        let ev = parse_uplink(UP.as_bytes()).expect("parses");
        assert_eq!(
            uplink_received_at(&ev).as_deref(),
            Some("2026-08-27T09:59:19.187803Z")
        );

        // A whole-second timestamp stays whole — no spurious `.000000`.
        let ts = chirpstack_api::pbjson_types::Timestamp {
            seconds: 1_787_824_759,
            nanos: 0,
        };
        assert_eq!(format_timestamp(&ts), "2026-08-27T09:59:19Z");
    }

    #[test]
    fn an_event_without_a_time_reports_none_rather_than_the_epoch() {
        let mut v: serde_json::Value = serde_json::from_str(UP).expect("fixture parses");
        v.as_object_mut().expect("object").remove("time");
        let raw = serde_json::to_vec(&v).expect("re-serializes");
        let ev = parse_uplink(&raw).expect("still an event");
        assert_eq!(uplink_received_at(&ev), None, "1970 is not a reading time");
    }

    /// Q-N2: a ChirpStack 4.x `event/join` in the JSON marshaler's shape. The address and the
    /// time are what the LoRaWAN `info` gains; nanoseconds on the wire come out as the
    /// microseconds every other stamp here carries.
    #[test]
    fn a_realistic_chirpstack_4x_join_event_deserializes() {
        let ev = parse_join(JOIN.as_bytes()).expect("the json marshaler's own shape");
        assert_eq!(
            join_dev_eui(&ev).expect("normalizes").as_str(),
            "5876070000000413"
        );
        assert_eq!(join_dev_addr(&ev).expect("an address"), "01ab23cd");
        assert_eq!(
            join_time(&ev).as_deref(),
            Some("2026-09-27T21:04:11.482161Z")
        );
    }

    #[test]
    fn a_join_address_is_folded_to_lowercase_and_a_malformed_one_is_refused() {
        let mut v: serde_json::Value = serde_json::from_str(JOIN).expect("fixture parses");
        v["devAddr"] = serde_json::json!("01AB23CD");
        let ev = parse_join(&serde_json::to_vec(&v).expect("ser")).expect("parses");
        assert_eq!(join_dev_addr(&ev).expect("an address"), "01ab23cd");

        for bad in ["", "01ab23", "01ab23cdef", "01ab23cg"] {
            v["devAddr"] = serde_json::json!(bad);
            let ev = parse_join(&serde_json::to_vec(&v).expect("ser")).expect("parses");
            let err = join_dev_addr(&ev).expect_err(bad);
            assert_eq!(err.kind(), Kind::InvalidArgument, "{bad:?}");
        }
    }

    #[test]
    fn the_log_topic_is_its_own_kind() {
        let t = parse_event_topic("application/app-1/device/aa/event/log").expect("matches");
        assert_eq!(t.kind, EventKind::Log);
        assert_eq!(t.kind.as_str(), "log");
    }

    /// The 4.x JSON marshaler's shape of the discard #129 is about: camelCase keys, the
    /// enums by name, the context values as strings.
    #[test]
    fn a_realistic_chirpstack_4x_log_event_parses() {
        let ev = parse_log(LOG.as_bytes()).expect("parses");
        assert_eq!(ev.level, "ERROR");
        assert_eq!(ev.code, LOG_CODE_DOWNLINK_PAYLOAD_SIZE);
        assert_eq!(
            ev.description,
            "Device queue-item discarded because it exceeds the max. payload size"
        );
        assert_eq!(ev.dev_eui.as_str(), "70b3d57ed80051b2");
        assert_eq!(ev.context_u64("max_payload_size"), Some(51));
        assert_eq!(ev.context_u64("item_size"), Some(88));
        assert_eq!(
            ev.context.get("queue_item_id").map(String::as_str),
            Some("1b0d7f4e-63a2-4c55-9a0e-2f3b7c9d8e11")
        );
        assert_eq!(ev.time.as_deref(), Some("2026-10-13T12:08:18.412345Z"));
    }

    /// A code newer than the pinned crate, and the defaults proto3 leaves out, still parse.
    #[test]
    fn a_log_event_with_an_unknown_code_or_omitted_defaults_parses() {
        let mut v: serde_json::Value = serde_json::from_str(LOG).expect("json");
        v["code"] = serde_json::json!("SOMETHING_FROM_4_30");
        let ev = parse_log(&serde_json::to_vec(&v).expect("ser")).expect("parses");
        assert_eq!(ev.code, "SOMETHING_FROM_4_30");

        let obj = v.as_object_mut().expect("object");
        obj.remove("level");
        obj.remove("code");
        obj.remove("context");
        obj.remove("time");
        let ev = parse_log(&serde_json::to_vec(&v).expect("ser")).expect("parses");
        assert_eq!((ev.level.as_str(), ev.code.as_str()), ("INFO", "UNKNOWN"));
        assert!(ev.context.is_empty());
        assert_eq!(ev.time, None);
    }

    #[test]
    fn a_log_event_without_a_dev_eui_is_refused() {
        let mut v: serde_json::Value = serde_json::from_str(LOG).expect("json");
        v["deviceInfo"]["devEui"] = serde_json::json!("");
        let err = parse_log(&serde_json::to_vec(&v).expect("ser")).expect_err("no devEui");
        assert_eq!(err.kind(), Kind::InvalidArgument);
        assert!(parse_log(b"\x08\x01").is_err());
    }
}
