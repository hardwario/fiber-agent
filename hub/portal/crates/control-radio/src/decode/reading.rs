//! The `NodeReading` envelope — `plan/control/radio/plan.md` §7 step 3.
//!
//! One shape for both transports and for both northbound areas: an fPort-2 decode publishes it
//! on `reading`, an fPort-3 batch publishes it on `alarm`, and P2P hands the same struct out of
//! its own frame decode. `portal` reuses this type directly (WP-E) rather than re-declaring it,
//! which is the whole reason it is a struct and not an ad-hoc `serde_json::Value`.
//!
//! # From the registry's flat map to fields/counters/events
//!
//! [`crate::decode::decode_uplink`] returns the **reference decoder's own shape** — one flat
//! map, deliberately, because the parity contract is field-for-field identity with
//! `ttn.js@v1.4.0` including *presence* (an absent sensor omits its key; an enabled sensor with
//! no valid sample emits an explicit `null`). §7 step 3's envelope is a different shape:
//! `fields: map<name, f64>` plus `counters: map<name, u64>` plus `events[]`.
//!
//! [`from_decoded`] is the one place that translation happens, and its rules are stated rather
//! than implied, because every one of them loses information if it is picked carelessly:
//!
//! - **`*_count` → `counters`.** A monotonically increasing count is not a measurement; the
//!   portal stores the two differently and a count landing in `fields` would be graphed as if
//!   it were one.
//! - **Other numbers → `fields`.**
//! - **Booleans → `fields`, as `1.0`/`0.0`.** The tempting alternative — emit an event when
//!   true — cannot distinguish `boot: false` from "this device does not report boot at all",
//!   and that distinction is exactly what the flat map went to the trouble of preserving.
//! - **`null` → a `no_data` event naming the field, and *no* entry in `fields`.** This is the
//!   "fitted but broken" case the decoder's null is for. Dropping it silently would make a
//!   failed sensor indistinguishable from an absent one.
//! - **`w1_sensors[]` → per-slot fields `w1_{slot}_{channel}`**, plus one `w1_slot` event
//!   carrying the slot's declared type. The ROM-bound slots are the only nested structure the
//!   fPort-2 decode produces, and flattening them by slot number is what lets a consumer plot
//!   "slot 2's temperature" without knowing the 1-Wire vocabulary. *Amended 2026-10-09
//!   (v2.0.0, `node-firmware-v2.0.0.md` §1.1):* the slot's channels arrive in `values`, and
//!   each is named by `sensor_types::hub_field` (`w1_2_temperature_aux`); a counter channel of
//!   the registry goes to `counters`. Only a slot 1..=4 of a known 1-Wire type is flattened:
//!   anything else (slot 0 or missing, which would land on motherboard series; an unknown or
//!   non-1-Wire type) keeps its `w1_slot` event and adds a `decode_warning`. A channel the
//!   registry does not know (a raw `ch{N}`) has no name to store it under: not flattened, one
//!   `decode_warning` per slot (§3). *Amended 2026-10-14:* a pre-1.5 reading is no longer
//!   told apart (v2.0.0 is the minimum firmware).
//! - **Alarm events carry `field`** (v2.0.0): the Hub field of the alarm's slot and channel,
//!   the series it is about, absent for a sensor mismatch or a channel the registry lacks.
//! - **Decoder warnings → a `decode_warning` event.** §7 step 1 makes an unknown payload
//!   version a warning-and-continue, and a warning that reached nobody is the same as no
//!   warning at all. The envelope has no `warnings` field (§7 step 3 does not list one), so
//!   this is where it goes.
//!
//! # `message_id`
//!
//! §7 step 3: *"a stable dedup identifier derived from `dev_eui` + `received_at`, so a
//! retransmit or a second gateway's copy never produces a second record."*
//!
//! [`message_id`] takes **ChirpStack's own `deduplicationId` when the event carries one**, and
//! falls back to the documented `{dev_eui}:{received_at}` derivation when it does not (P2P,
//! and any event predating the field). The reason is that `deduplicationId` *is* the fact the
//! plan is asking for, stated by the system that knows it: it is the identity of one
//! deduplication set, so every gateway's copy of a frame shares it by construction rather than
//! by our hoping two `time` values agree to the microsecond. The derived form remains the
//! contract for everything that has no such id, and both are opaque strings to the consumer.
//!
//! ⚠️ Plan amendment owed: §7 step 3 names only the derived form. Preferring the LNS's own
//! dedup id is strictly stronger and never weaker, but the sentence should say so.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::decode::sensor_types;
use crate::decode::Decoded;

/// One gateway's reception of a single uplink.
///
/// A Node frame is broadcast and typically heard by several gateways at once; ChirpStack
/// aggregates them into `rxInfo[]`, order not guaranteed — **the consumer sorts** (§7 step 3).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GatewayRx {
    pub gateway_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rssi: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snr: Option<f64>,
}

/// One thing that happened, as opposed to something that was measured.
///
/// `type` + `ts` + whatever the event kind carries, flattened — the shape v1's northbound
/// events already had, kept so `portal`'s reader is one shape and not two.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NodeEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub ts: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl NodeEvent {
    pub fn new(event_type: &str, ts: &str, extra: Map<String, Value>) -> Self {
        NodeEvent {
            event_type: event_type.to_string(),
            ts: ts.to_string(),
            extra,
        }
    }
}

/// §7 step 3's envelope, in its field order.
///
/// `BTreeMap` rather than `HashMap` throughout: this document is a golden-file baseline and a
/// published MQTT payload, so its key order has to be the same on every run.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NodeReading {
    /// Stable dedup identifier — see the module note.
    pub message_id: String,
    /// The wire identity: 16 lowercase hex on LoRaWAN, the registered identity on P2P.
    pub dev_eui: String,
    /// §7 step 3's "P2P: plus `serial`". Filled on LoRaWAN too whenever this Hub holds an
    /// enrollment record, because the operator-facing identifier is the serial and a consumer
    /// that has it does not have to join anything to show it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    pub fields: BTreeMap<String, f64>,
    pub counters: BTreeMap<String, u64>,
    pub events: Vec<NodeEvent>,
    /// Every gateway that heard this uplink. Order is not guaranteed; on P2P a single-element
    /// list for this Hub.
    pub gateways: Vec<GatewayRx>,
    /// LoRaWAN data-rate index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dr: Option<u32>,
    /// Back-compat scalar signal — the **best-RSSI** gateway of [`NodeReading::gateways`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rssi: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snr: Option<f64>,
    /// RFC3339.
    pub received_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fport: Option<u8>,
    /// A paged telemetry report (sticker-firmware #480): this frame is page `page_index`
    /// (0-based) of `page_count`, one slice of a snapshot, and carries only that slice's fields.
    /// The Hub publishes every page as it arrives, so a lost page loses only its own fields; a
    /// consumer merges by field, never treating a page as the whole report. Absent on a
    /// complete report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_count: Option<u32>,
}

/// The page of a paged telemetry report, `(page_index, page_count)`; `None` for a complete one
/// (no page fields, or a count of 0 or 1).
pub fn telemetry_page(decoded: &Decoded) -> Option<(u32, u32)> {
    let num = |k: &str| {
        decoded
            .data
            .get(k)
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
    };
    let count = num("page_count").filter(|c| *c > 1)?;
    Some((num("page_index").unwrap_or(0), count))
}

/// The cached `message` after `page` arrives — the `node-get message` view of a paged report
/// (GitLab #126). `page` and `cached` are serialized [`NodeReading`]s.
///
/// Each page carries only its own slice, so caching it as it stands made `node-get message`
/// answer with the last page alone. When `page` continues the cached report — both paged with
/// the same `page_count`, the same device, and `page` a later index — the cached fields,
/// counters and events are folded under it: the newer page wins a key both carry, events are
/// concatenated, and the envelope (`message_id`, link quality, `received_at`, `page_index`) is
/// the newer page's. Page 0, an unpaged frame, or anything else that cannot continue the cached
/// report replaces it, exactly as before. Only the cache is folded; every page is still
/// published as its own reading (node-firmware-v2.0.0.md §12 (B)).
pub fn fold_message_page(cached: Option<&Value>, page: Value) -> Value {
    let Some(cached) = cached else {
        return page;
    };
    let index = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64);
    let continues = matches!(
        (
            index(&page, "page_index"),
            index(&page, "page_count"),
            index(cached, "page_index"),
            index(cached, "page_count"),
        ),
        (Some(i), Some(n), Some(ci), Some(cn)) if n == cn && i > ci && i < n
    ) && page.get("dev_eui") == cached.get("dev_eui");
    if !continues {
        return page;
    }
    let mut merged = page;
    for key in ["fields", "counters"] {
        let (Some(held), Some(Value::Object(fresh))) = (
            cached.get(key).and_then(Value::as_object),
            merged.get_mut(key),
        ) else {
            continue;
        };
        for (k, v) in held {
            fresh.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    if let (Some(held), Some(Value::Array(fresh))) = (
        cached.get("events").and_then(Value::as_array),
        merged.get_mut("events"),
    ) {
        let mut events = held.clone();
        events.append(fresh);
        *fresh = events;
    }
    merged
}

/// §7 step 3's dedup identifier.
///
/// `deduplication_id` wins when it is non-empty; otherwise the documented `dev_eui` +
/// `received_at` derivation. See the module note for why.
pub fn message_id(dev_eui: &str, received_at: &str, deduplication_id: Option<&str>) -> String {
    match deduplication_id {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => format!("{dev_eui}:{received_at}"),
    }
}

/// The best-RSSI gateway of a reception set.
///
/// A gateway reporting no RSSI sorts last rather than winning by accident — "did not say" is
/// not "loudest".
pub fn best_gateway(gateways: &[GatewayRx]) -> Option<&GatewayRx> {
    gateways.iter().max_by_key(|g| g.rssi.unwrap_or(i32::MIN))
}

/// Split one registry decode into the envelope's `fields`/`counters`/`events`.
///
/// The rules are the module note's; `ts` stamps every event this frame produces.
pub fn from_decoded(
    decoded: &Decoded,
    ts: &str,
) -> (BTreeMap<String, f64>, BTreeMap<String, u64>, Vec<NodeEvent>) {
    let mut fields = BTreeMap::new();
    let mut counters = BTreeMap::new();
    let mut events = Vec::new();

    for w in &decoded.warnings {
        let mut extra = Map::new();
        extra.insert("message".into(), Value::from(w.clone()));
        events.push(NodeEvent::new("decode_warning", ts, extra));
    }
    for e in &decoded.errors {
        let mut extra = Map::new();
        extra.insert("message".into(), Value::from(e.clone()));
        events.push(NodeEvent::new("decode_error", ts, extra));
    }

    // fPort 3's batch is a different shape from fPort 2's flat telemetry, and it is the only
    // one that carries an `alarms` array. Handled first so its scalars (`base_time`, `total`,
    // `time_synced`, `truncated`) do not leak into `fields` as if they were measurements.
    if let Some(Value::Array(alarms)) = decoded.data.get("alarms") {
        let mut batch = Map::new();
        for key in [
            "base_time",
            "total",
            "time_synced",
            "truncated",
            "page_index",
            "page_count",
            "pages",
        ] {
            if let Some(v) = decoded.data.get(key) {
                batch.insert(key.to_string(), v.clone());
            }
        }
        events.push(NodeEvent::new("alarm_batch", ts, batch));
        for a in alarms {
            if let Value::Object(map) = a {
                // The decoder names the threshold side `type` (`low`/`high`/`trigger`/
                // `no_data`), which is the envelope's own key: flattened, it overwrites
                // `"alarm"` on the wire and no consumer can find the alarm. It travels as
                // `side` instead.
                let mut map = map.clone();
                if let Some(side) = map.remove("type") {
                    map.insert("side".to_string(), side);
                }
                if let Some(field) = alarm_field(&map) {
                    map.insert("field".to_string(), Value::from(field));
                }
                events.push(NodeEvent::new("alarm", ts, map));
            }
        }
        return (fields, counters, events);
    }

    for (key, value) in &decoded.data {
        // Paging is the envelope's (`NodeReading::page_index`), not a measurement.
        if matches!(key.as_str(), "page_index" | "page_count" | "pages") {
            continue;
        }
        match value {
            Value::Null => {
                let mut extra = Map::new();
                extra.insert("field".into(), Value::from(key.clone()));
                events.push(NodeEvent::new("no_data", ts, extra));
            }
            Value::Bool(b) => {
                fields.insert(key.clone(), if *b { 1.0 } else { 0.0 });
            }
            Value::Number(n) => insert_number(key, n, &mut fields, &mut counters),
            Value::Array(items) if key == "w1_sensors" => {
                for item in items {
                    if let Value::Object(slot) = item {
                        w1_slot(slot, ts, &mut fields, &mut counters, &mut events);
                    }
                }
            }
            // Nothing else is produced by a registered decoder today; ignored rather than
            // coerced, so a future decoder adding a string field cannot silently become `0.0`.
            _ => {}
        }
    }

    (fields, counters, events)
}

/// A count is not a measurement — §7's `counters` exists precisely so the portal does not graph
/// one as the other.
fn is_counter(key: &str) -> bool {
    key.ends_with("_count")
}

fn insert_number(
    key: &str,
    n: &serde_json::Number,
    fields: &mut BTreeMap<String, f64>,
    counters: &mut BTreeMap<String, u64>,
) {
    if is_counter(key) {
        // A negative or fractional "count" is not one; it falls through to `fields` rather
        // than being clamped into a `u64` that says something the device did not.
        if let Some(u) = n.as_u64() {
            counters.insert(key.to_string(), u);
            return;
        }
    }
    if let Some(f) = n.as_f64() {
        fields.insert(key.to_string(), f);
    }
}

/// The Hub field an alarm event is about: its `slot` label, `channel` name and, on a 1-Wire
/// slot, `sensor_type` name, through the registry. `None` for a mismatch (`channel: null`) or
/// anything the registry does not know.
fn alarm_field(alarm: &Map<String, Value>) -> Option<String> {
    let slot = sensor_types::slot_of_label(alarm.get("slot")?.as_str()?)?;
    let channel = alarm.get("channel")?.as_str()?;
    let t = if slot == 0 {
        sensor_types::sensor_type(sensor_types::MOTHERBOARD)?
    } else {
        let name = alarm.get("sensor_type")?.as_str()?;
        sensor_types::TYPES.iter().find(|t| t.name == name)?
    };
    let c = t.channels.iter().find(|c| c.name == channel)?;
    Some(sensor_types::hub_field_of(slot, c))
}

fn warning(ts: &str, message: impl Into<String>) -> NodeEvent {
    let mut extra = Map::new();
    extra.insert("message".into(), Value::from(message.into()));
    NodeEvent::new("decode_warning", ts, extra)
}

/// One ROM-bound 1-Wire slot, flattened as `w1_{slot}_{channel}` (§1.1).
fn w1_slot(
    slot: &Map<String, Value>,
    ts: &str,
    fields: &mut BTreeMap<String, f64>,
    counters: &mut BTreeMap<String, u64>,
    events: &mut Vec<NodeEvent>,
) {
    let index = slot.get("slot").and_then(Value::as_u64).unwrap_or(0);
    let type_id = slot.get("type").and_then(Value::as_u64);

    let mut declared = Map::new();
    declared.insert("slot".into(), Value::from(index));
    // `type` is the event's own tag in the flattened envelope; a sensor type stored under
    // the same key overwrites it with a number and the portal rejects the whole reading.
    for (key, as_key) in [("type", "sensor_type"), ("type_name", "sensor_type_name")] {
        if let Some(v) = slot.get(key) {
            declared.insert(as_key.to_string(), v.clone());
        }
    }
    events.push(NodeEvent::new("w1_slot", ts, declared));

    // Slot 0 or a missing slot would name motherboard series; a type that is not a 1-Wire
    // type of the registry has no channels to name the values by.
    let Some((index, t)) = u8::try_from(index)
        .ok()
        .filter(|i| (1..=sensor_types::W1_SLOTS).contains(i))
        .zip(
            type_id
                .and_then(|id| u8::try_from(id).ok())
                .and_then(sensor_types::sensor_type)
                .filter(|t| t.is_w1()),
        )
    else {
        let ty = type_id.map_or_else(|| "none".to_string(), |id| id.to_string());
        events.push(warning(
            ts,
            format!(
                "1-Wire reading on slot {index} with type {ty}: not a 1-Wire slot of a known \
                 type, values dropped"
            ),
        ));
        return;
    };

    let Some(Value::Object(values)) = slot.get("values") else {
        return;
    };
    let mut unknown = Vec::new();
    for (key, value) in values {
        let Some(channel) = t.channel_named(key) else {
            unknown.push(key.as_str());
            continue;
        };
        let name = sensor_types::hub_field_of(index, channel);
        match value {
            Value::Null => {
                let mut extra = Map::new();
                extra.insert("field".into(), Value::from(name));
                events.push(NodeEvent::new("no_data", ts, extra));
            }
            Value::Bool(b) => {
                fields.insert(name, if *b { 1.0 } else { 0.0 });
            }
            Value::Number(n) => {
                if let (true, Some(u)) = (channel.counter(), n.as_u64()) {
                    counters.insert(name, u);
                } else {
                    insert_number(&name, n, fields, counters);
                }
            }
            _ => {}
        }
    }
    if !unknown.is_empty() {
        events.push(warning(
            ts,
            format!(
                "1-Wire slot {index} ({}): channels unknown to this Hub not stored: {}",
                t.name,
                unknown.join(", ")
            ),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    /// #126: only a later page of the same report folds into the cached message.
    #[test]
    fn a_message_page_folds_only_into_its_own_report() {
        let page = |i: u32, n: u32, field: &str| {
            serde_json::json!({
                "dev_eui": "aa", "fields": {field: 1.0}, "counters": {"fCnt": i},
                "events": [{"type": field, "ts": "T"}], "page_index": i, "page_count": n,
            })
        };
        let merged = fold_message_page(Some(&page(0, 3, "a")), page(1, 3, "b"));
        assert_eq!(merged["fields"], serde_json::json!({"a": 1.0, "b": 1.0}));
        assert_eq!(
            merged["counters"]["fCnt"], 1,
            "the newer page wins a shared key"
        );
        assert_eq!(merged["events"].as_array().unwrap().len(), 2);
        assert_eq!(merged["page_index"], 1);
        let merged = fold_message_page(Some(&merged), page(2, 3, "c"));
        assert_eq!(merged["fields"].as_object().unwrap().len(), 3);

        for (cached, next) in [
            (page(1, 3, "a"), page(0, 3, "b")), // a new report's page 0
            (page(1, 3, "a"), page(1, 3, "b")), // a page seen twice
            (page(0, 3, "a"), page(1, 2, "b")), // another page count
            (
                page(0, 3, "a"),
                serde_json::json!({"dev_eui": "aa", "fields": {"b": 1.0}}),
            ),
        ] {
            assert_eq!(fold_message_page(Some(&cached), next.clone()), next);
        }
        let other =
            serde_json::json!({"dev_eui": "bb", "fields": {}, "page_index": 1, "page_count": 3});
        assert_eq!(
            fold_message_page(Some(&page(0, 3, "a")), other.clone()),
            other
        );
        assert_eq!(fold_message_page(None, page(1, 3, "b")), page(1, 3, "b"));
    }

    /// sticker-firmware #480's HIL captures (EU868, machine probe on s1): at DR0 the report
    /// comes as two pages, each its own reading with its page in the envelope and no paging
    /// key among the fields; together they carry every field the one DR5 frame carries.
    #[test]
    fn telemetry_pages_are_partial_readings_that_add_up_to_the_report() {
        let decode = |hex: &str| crate::decode::decode_uplink("node", 2, &unhex(hex)).expect("2");
        let p1 = decode("01089e01102718fc26205628ffffffff0f30ffffffff0f4800900100980100a00100a80100d00100e80102");
        let p2 = decode("01da01160801100358fb03620da226a2019c047e005c9a0cfe08e00101e80102");
        let whole = decode("01089e011026189827205528ffffffff0f30ffffffff0f4800900100980100a00100a80100d00100da01170801100358fb03620eb026a2019c04dc04004caa0c9c09");
        assert_eq!(
            telemetry_page(&p1),
            Some((0, 2)),
            "page_index 0 is proto3-absent"
        );
        assert_eq!(telemetry_page(&p2), Some((1, 2)));
        assert_eq!(telemetry_page(&whole), None);
        let keys = |d: &Decoded| {
            let (fields, counters, _) = from_decoded(d, "2026-10-09T15:00:00Z");
            assert!(!fields.contains_key("page_index") && !fields.contains_key("page_count"));
            fields
                .into_keys()
                .chain(counters.into_keys())
                .filter(|k| k != "boot")
                .collect::<std::collections::BTreeSet<_>>()
        };
        let (k1, k2) = (keys(&p1), keys(&p2));
        assert!(
            k1.iter().all(|k| !k.starts_with("w1_")),
            "page 1: the board"
        );
        assert!(
            k2.iter().all(|k| k.starts_with("w1_1_")),
            "page 2: the probe"
        );
        let union: std::collections::BTreeSet<_> = k1.union(&k2).cloned().collect();
        assert_eq!(union, keys(&whole));
    }

    fn decoded(json: &str) -> Decoded {
        Decoded {
            data: serde_json::from_str(json).expect("fixture"),
            warnings: Vec::new(),
            errors: Vec::new(),
        }
    }

    #[test]
    fn counts_are_counters_and_measurements_are_fields() {
        let d = decoded(r#"{"voltage":3.36,"temperature":23.65,"hall_left_count":7}"#);
        let (fields, counters, events) = from_decoded(&d, "T");
        assert_eq!(counters.get("hall_left_count"), Some(&7));
        assert!(!fields.contains_key("hall_left_count"));
        assert_eq!(fields.get("voltage"), Some(&3.36));
        assert_eq!(fields.get("temperature"), Some(&23.65));
        assert!(events.is_empty());
    }

    /// A boolean is a two-valued measurement, not an event: an event could not distinguish
    /// `boot: false` from a device that does not report `boot` at all.
    #[test]
    fn booleans_are_fields_so_false_stays_distinguishable_from_absent() {
        let d = decoded(r#"{"boot":false,"hall_left_is_active":true}"#);
        let (fields, _, _) = from_decoded(&d, "T");
        assert_eq!(fields.get("boot"), Some(&0.0));
        assert_eq!(fields.get("hall_left_is_active"), Some(&1.0));

        let d = decoded(r#"{"hall_left_is_active":true}"#);
        let (fields, _, _) = from_decoded(&d, "T");
        assert!(
            !fields.contains_key("boot"),
            "a device that does not report boot has no boot field"
        );
    }

    /// The decoder's explicit `null` means "fitted and broken". Dropping it would make a
    /// failed sensor indistinguishable from an absent one.
    #[test]
    fn a_null_reading_becomes_a_no_data_event_not_a_missing_field() {
        let d = decoded(r#"{"temperature":null,"voltage":3.0}"#);
        let (fields, _, events) = from_decoded(&d, "2026-08-27T10:00:00Z");
        assert!(!fields.contains_key("temperature"));
        let nd: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == "no_data")
            .collect();
        assert_eq!(nd.len(), 1);
        assert_eq!(nd[0].extra.get("field"), Some(&Value::from("temperature")));
        assert_eq!(nd[0].ts, "2026-08-27T10:00:00Z");
    }

    #[test]
    fn one_wire_slots_flatten_by_slot_number() {
        let d = decoded(
            r#"{"w1_sensors":[{"slot":1,"type":2,"type_name":"dallas",
                               "values":{"temperature":21.5},"units":{"temperature":"degC"}},
                              {"slot":2,"type":3,"type_name":"machine-probe",
                               "values":{"temperature":null,"temperature-aux":19.25,"tilt":1,
                                         "ch12":7},
                               "units":{"temperature":"degC"}}]}"#,
        );
        let (fields, _, events) = from_decoded(&d, "T");
        assert_eq!(fields.get("w1_1_temperature"), Some(&21.5));
        assert_eq!(fields.get("w1_2_tilt"), Some(&1.0));
        assert_eq!(fields.get("w1_2_temperature_aux"), Some(&19.25));
        assert!(
            !fields.keys().any(|k| k.contains("ch12")),
            "an unknown channel has no name to store it under (§3)"
        );
        let warnings: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == "decode_warning")
            .collect();
        assert_eq!(warnings.len(), 1, "logged once per slot: {warnings:?}");
        assert!(warnings[0].extra["message"]
            .as_str()
            .is_some_and(|m| m.contains("slot 2") && m.contains("ch12")));
        assert!(!fields.contains_key("w1_2_temperature"));
        assert!(!fields.keys().any(|k| k.contains("units")));

        let slots: Vec<_> = events
            .iter()
            .filter(|e| e.event_type == "w1_slot")
            .collect();
        assert_eq!(slots.len(), 2);
        assert_eq!(
            slots[1].extra.get("sensor_type_name"),
            Some(&Value::from("machine-probe"))
        );
        assert!(
            slots.iter().all(|e| !e.extra.contains_key("type")),
            "a sensor type under `type` would replace the event tag"
        );
        let json = serde_json::to_string(&slots[0]).unwrap();
        let back: NodeEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.event_type, "w1_slot");
        assert!(events.iter().any(|e| e.event_type == "no_data"
            && e.extra.get("field") == Some(&Value::from("w1_2_temperature"))));
    }

    /// Slot 0, a missing slot, an unknown type or a non-1-Wire type: the values would land on
    /// motherboard series or have no channel to be named by. The slot event stays, the values
    /// become one warning.
    #[test]
    fn a_reading_off_the_one_wire_slots_or_types_never_reaches_fields() {
        for json in [
            r#"{"temperature":22.0,"w1_sensors":[{"slot":0,"type":2,
                "values":{"temperature":99.0}}]}"#,
            r#"{"temperature":22.0,"w1_sensors":[{"type":2,"values":{"temperature":99.0}}]}"#,
            r#"{"temperature":22.0,"w1_sensors":[{"slot":5,"type":2,
                "values":{"temperature":99.0}}]}"#,
            // The motherboard type is in the registry but is not a 1-Wire type.
            r#"{"temperature":22.0,"w1_sensors":[{"slot":1,"type":1,
                "values":{"temperature":99.0}}]}"#,
            r#"{"temperature":22.0,"w1_sensors":[{"slot":1,"type":77,
                "values":{"ch0":99}}]}"#,
            r#"{"temperature":22.0,"w1_sensors":[{"slot":1,"values":{"temperature":null}}]}"#,
        ] {
            let (fields, counters, events) = from_decoded(&decoded(json), "T");
            assert_eq!(fields.len(), 1, "{json}: {fields:?}");
            assert_eq!(fields.get("temperature"), Some(&22.0), "{json}");
            assert!(counters.is_empty(), "{json}");
            assert!(
                events.iter().all(|e| e.event_type != "no_data"),
                "{json}: {events:?}"
            );
            assert_eq!(
                events.iter().filter(|e| e.event_type == "w1_slot").count(),
                1,
                "{json}"
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|e| e.event_type == "decode_warning")
                    .count(),
                1,
                "{json}"
            );
        }
    }

    /// A 1-Wire event on channel 0 omits field 10 (proto3 zero) and names its series.
    #[test]
    fn a_w1_alarm_event_on_channel_0_names_its_field() {
        let bytes = [
            0x01, 0x08, 0x64, 0x10, 0x01, 0x1a, 0x06, 0x08, 0x01, 0x48, 0x02, 0x58, 0x02,
        ];
        let d = crate::decode::decode_uplink("node", 3, &bytes).expect("fPort 3");
        let (_, _, events) = from_decoded(&d, "T");
        let alarm = events
            .iter()
            .find(|e| e.event_type == "alarm")
            .expect("alarm");
        assert_eq!(
            alarm.extra.get("field"),
            Some(&Value::from("w1_1_temperature"))
        );
        assert!(events.iter().all(|e| e.event_type != "decode_warning"));
    }

    /// v2.0.0 (#396): an analog GP input's voltage, telemetry 30/31 in mV, is a field in V;
    /// the retired flat probe fields 10..17 decode to nothing.
    #[test]
    fn an_analog_input_voltage_is_a_field_and_retired_fields_are_ignored() {
        // version 1; 30 = 3300 mV (f0 01 e4 19); 31 = 0; 10 = sint 2150 (50 cc 21).
        let bytes = [
            0x01, 0xf0, 0x01, 0xe4, 0x19, 0xf8, 0x01, 0x00, 0x50, 0xcc, 0x21,
        ];
        let d = crate::decode::decode_uplink("node", 2, &bytes).expect("fPort 2");
        let (fields, _, _) = from_decoded(&d, "T");
        assert_eq!(fields.get("input_a_voltage"), Some(&3.3));
        assert_eq!(fields.get("input_b_voltage"), Some(&0.0));
        assert!(!fields.contains_key("ext_temperature_1"), "{fields:?}");
    }

    /// §7 step 1 makes an unknown payload version a warning-and-continue. A warning nobody
    /// receives is the same as no warning, and the envelope has no `warnings` field.
    #[test]
    fn decoder_warnings_travel_as_events() {
        let d = Decoded {
            data: serde_json::from_str(r#"{"voltage":3.0}"#).expect("fixture"),
            warnings: vec!["unknown payload version 0x2".into()],
            errors: Vec::new(),
        };
        let (fields, _, events) = from_decoded(&d, "T");
        assert_eq!(fields.get("voltage"), Some(&3.0));
        let w = events
            .iter()
            .find(|e| e.event_type == "decode_warning")
            .expect("the warning reaches a consumer");
        assert_eq!(
            w.extra.get("message"),
            Some(&Value::from("unknown payload version 0x2"))
        );
    }

    /// An fPort-3 batch's scalars describe the batch, not the device — they must not land in
    /// `fields` as though `total` were a measurement.
    #[test]
    fn an_alarm_batch_becomes_events_and_leaves_fields_empty() {
        let d = decoded(
            r#"{"base_time":1787824000,"total":2,"time_synced":true,"truncated":false,
                "alarms":[{"slot":"mb","channel":"temperature","rule":0,
                           "event":"activate","type":"high","value":31.5,"time":1787824010},
                          {"slot":"s1","channel":"temperature-aux","sensor_type":"machine-probe",
                           "rule":3,"event":"deactivate","type":"low","value":18,
                           "time":1787824020},
                          {"slot":"s2","channel":null,"sensor_type":"dallas","rule":null,
                           "event":"activate","type":"sensor_mismatch","value":3,
                           "time":1787824030,"detected_type":"machine-probe"}]}"#,
        );
        let (fields, counters, events) = from_decoded(&d, "T");
        assert!(fields.is_empty(), "no measurement leaked: {fields:?}");
        assert!(counters.is_empty());

        let batch = events
            .iter()
            .find(|e| e.event_type == "alarm_batch")
            .expect("the batch header survives");
        assert_eq!(batch.extra.get("total"), Some(&Value::from(2)));
        assert_eq!(batch.extra.get("time_synced"), Some(&Value::Bool(true)));

        let alarms: Vec<_> = events.iter().filter(|e| e.event_type == "alarm").collect();
        assert_eq!(alarms.len(), 3);
        // The decoder's own `type` key is the threshold side; it travels as `side` so the
        // event envelope's `type` stays `"alarm"`.
        assert_eq!(alarms[0].extra.get("side"), Some(&Value::from("high")));
        assert_eq!(alarms[0].extra.get("type"), None);
        assert_eq!(alarms[0].extra.get("slot"), Some(&Value::from("mb")));
        // `field` names the series the alarm is about (v2.0.0).
        assert_eq!(
            alarms[0].extra.get("field"),
            Some(&Value::from("temperature"))
        );
        assert_eq!(
            alarms[1].extra.get("field"),
            Some(&Value::from("w1_1_temperature_aux"))
        );
        assert_eq!(
            alarms[2].extra.get("field"),
            None,
            "a mismatch is about the slot"
        );
    }

    /// What a consumer reads is the published JSON, not the struct: an alarm must still be
    /// `"alarm"` after the round trip, with its side intact (Nodes test 2026-09-23, where
    /// `type: "high"` won the collision and the portal raised no alarm at all).
    #[test]
    fn an_alarm_event_is_still_an_alarm_after_the_json_round_trip() {
        let d = decoded(
            r#"{"base_time":1787824000,"total":1,"time_synced":true,"truncated":false,
                "alarms":[{"slot":"mb","channel":"temperature","rule":0,
                           "event":"activate","type":"high","value":31.5,"time":1787824010}]}"#,
        );
        let (_, _, events) = from_decoded(&d, "T");
        let wire = serde_json::to_value(&events).expect("serializes");
        let back: Vec<NodeEvent> = serde_json::from_value(wire).expect("deserializes");

        let alarm = back
            .iter()
            .find(|e| e.event_type == "alarm")
            .expect("the alarm survives the wire");
        assert_eq!(alarm.extra.get("side"), Some(&Value::from("high")));
        assert_eq!(alarm.extra.get("event"), Some(&Value::from("activate")));
    }

    #[test]
    fn the_best_gateway_is_the_loudest_and_a_silent_one_never_wins() {
        let gws = vec![
            GatewayRx {
                gateway_id: "quiet".into(),
                rssi: Some(-87),
                snr: Some(4.25),
            },
            GatewayRx {
                gateway_id: "loud".into(),
                rssi: Some(-41),
                snr: Some(13.5),
            },
            GatewayRx {
                gateway_id: "silent".into(),
                rssi: None,
                snr: None,
            },
        ];
        let best = best_gateway(&gws).expect("some gateway");
        assert_eq!(best.gateway_id, "loud");

        // "Did not say" is not "loudest".
        let only_silent = vec![GatewayRx {
            gateway_id: "silent".into(),
            rssi: None,
            snr: None,
        }];
        assert_eq!(
            best_gateway(&only_silent).map(|g| g.gateway_id.as_str()),
            Some("silent")
        );
        assert_eq!(best_gateway(&[]), None);
    }

    #[test]
    fn the_dedup_id_wins_and_the_derivation_is_the_fallback() {
        assert_eq!(
            message_id("aabb", "2026-08-27T10:00:00Z", Some("uuid-1")),
            "uuid-1"
        );
        assert_eq!(
            message_id("aabb", "2026-08-27T10:00:00Z", Some("")),
            "aabb:2026-08-27T10:00:00Z",
            "an empty id is no id — proto3 strings are never absent, only empty"
        );
        assert_eq!(
            message_id("aabb", "2026-08-27T10:00:00Z", None),
            "aabb:2026-08-27T10:00:00Z"
        );
    }

    /// The envelope is a published payload and a golden baseline, so its key order has to be
    /// stable across runs — which is why every map here is a `BTreeMap`.
    #[test]
    fn the_envelope_serializes_deterministically_and_round_trips() {
        let r = NodeReading {
            message_id: "uuid-1".into(),
            dev_eui: "70b3d57ed80051b2".into(),
            serial: Some("SN-1".into()),
            fields: [("voltage".to_string(), 3.36), ("boot".to_string(), 0.0)]
                .into_iter()
                .collect(),
            counters: [("fCnt".to_string(), 8u64)].into_iter().collect(),
            events: Vec::new(),
            gateways: vec![GatewayRx {
                gateway_id: "24e124fffefd3bda".into(),
                rssi: Some(-41),
                snr: Some(13.5),
            }],
            dr: Some(5),
            rssi: Some(-41),
            snr: Some(13.5),
            received_at: "2026-08-27T09:59:19.187803Z".into(),
            fport: Some(2),
            page_index: None,
            page_count: None,
        };
        let a = serde_json::to_string(&r).expect("serializes");
        let b = serde_json::to_string(&r).expect("serializes");
        assert_eq!(a, b);
        assert!(a.contains(r#""fields":{"boot":0.0,"voltage":3.36}"#), "{a}");

        let back: NodeReading = serde_json::from_str(&a).expect("round trips");
        assert_eq!(back, r);
    }

    /// Optional fields are omitted rather than emitted as `null`: a P2P reading has no `dr`,
    /// and a consumer must be able to tell "this transport has no such concept" from "the
    /// value was not measured".
    #[test]
    fn absent_optionals_are_omitted_not_nulled() {
        let r = NodeReading {
            message_id: "SN-1:2026-08-27T10:00:00Z".into(),
            dev_eui: "70b3d57ed80051b2".into(),
            serial: None,
            fields: BTreeMap::new(),
            counters: BTreeMap::new(),
            events: Vec::new(),
            gateways: Vec::new(),
            dr: None,
            rssi: None,
            snr: None,
            received_at: "2026-08-27T10:00:00Z".into(),
            fport: None,
            page_index: None,
            page_count: None,
        };
        let s = serde_json::to_string(&r).expect("serializes");
        for key in ["\"dr\"", "\"rssi\"", "\"snr\"", "\"fport\"", "\"serial\""] {
            assert!(!s.contains(key), "{key} must be omitted, not null: {s}");
        }
    }
}
