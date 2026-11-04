// MQTT message publishing with JSON formatting

use chrono::{DateTime, Utc};
use rumqttc::{AsyncClient, QoS};
use serde_json::json;

use crate::libs::alarms::AlarmState;
use crate::libs::config::{PublishConfig, QosOverrides};

use super::messages::MqttMessage;
use super::topics::TopicBuilder;

/// How many of a sensor's most-recent buffered events to include in the
/// periodic combined snapshot at the least aggressive rung of
/// `LORAWAN_SENSORS_CAP_LADDER`. This only bounds the periodic broadcast
/// snapshot — the in-memory backlog itself
/// (`lorawan::state::MAX_RECENT_EVENTS` = 32) is unaffected — and
/// alarm-type events are preferentially retained over routine ones when a
/// tighter rung must trim further (see `select_events`), since `events[]`
/// is the only channel carrying Node native alarms to the viewer's
/// Alarms/email pipeline.
const LORAWAN_SENSORS_EVENTS_CAP: usize = 8;

/// How many gateways' reception detail to keep per sensor at the least
/// aggressive rung. The top-level `rssi`/`snr` fields already carry the
/// strongest receiver's numbers; this only adds secondary detail.
const LORAWAN_SENSORS_GATEWAYS_CAP: usize = 5;

/// Safe byte budget for the combined `lorawan/sensors` publish, comfortably
/// under the 20480-byte outgoing-packet ceiling explicitly configured in
/// `mqtt/monitor.rs::create_mqtt_options`, with headroom for the topic name
/// and MQTT framing overhead. A single sensor with a full 32-event backlog
/// and several gateways was already ~14KB before this cap existed — see the
/// commit this constant was introduced in.
const LORAWAN_SENSORS_PAYLOAD_BUDGET_BYTES: usize = 8192;

/// The actual wire ceiling this payload must never exceed, matching
/// `MQTT_MAX_PACKET_SIZE_BYTES` in `mqtt/monitor.rs::create_mqtt_options`
/// exactly (kept as a separate constant, not shared across the module
/// boundary, since publisher.rs has no dependency on monitor.rs — if you
/// change one, change the other). `LORAWAN_SENSORS_PAYLOAD_BUDGET_BYTES`
/// above is a *comfortable target* the ladder tries to stay under; this is
/// the hard limit only the minimal-fields last resort is judged against,
/// since a pathologically large fleet may legitimately need more than the
/// comfortable budget just for `dev_eui`+safety-fields, and that's fine as
/// long as it still fits on the wire at all.
const LORAWAN_SENSORS_HARD_CEILING_BYTES: usize = 20 * 1024;

/// Ladder of (events_cap, gateways_cap) pairs tried in order, most detailed
/// first, until the combined payload fits under
/// `LORAWAN_SENSORS_PAYLOAD_BUDGET_BYTES`. Gateways (pure diagnostics)
/// degrade before events (alarm-bearing) — see `build_lorawan_sensors_payload`
/// for what happens if every rung still exceeds budget.
const LORAWAN_SENSORS_CAP_LADDER: &[(usize, usize)] = &[
    (LORAWAN_SENSORS_EVENTS_CAP, LORAWAN_SENSORS_GATEWAYS_CAP),
    (LORAWAN_SENSORS_EVENTS_CAP, 1),
    (4, 1),
    (2, 1),
    (1, 0),
    (0, 0),
];

/// Selects up to `cap` of a sensor's events for the periodic snapshot,
/// preferring `event_type == "alarm"` entries (the only ones the viewer's
/// Alarms/email pipeline consumes) over routine ones
/// (`boot`/`orientation`/`hall_active`) when the buffer must be trimmed.
/// `events` is chronological, oldest-first (see `state.rs`'s
/// push_back/pop_front ring buffer) — this restores that order in the
/// output, with alarms filled in newest-first before routine events take
/// any remaining slots.
fn select_events(
    events: &[crate::libs::lorawan::chirpstack::NodeEvent],
    cap: usize,
) -> Vec<crate::libs::lorawan::chirpstack::NodeEvent> {
    if cap == 0 || events.is_empty() {
        return Vec::new();
    }
    if events.len() <= cap {
        return events.to_vec();
    }

    let mut alarm_idx: Vec<usize> = Vec::new();
    let mut other_idx: Vec<usize> = Vec::new();
    for (i, e) in events.iter().enumerate() {
        if e.event_type == "alarm" {
            alarm_idx.push(i);
        } else {
            other_idx.push(i);
        }
    }
    // Higher index = more recent; take newest of each group first.
    alarm_idx.reverse();
    other_idx.reverse();

    let mut selected: Vec<usize> = alarm_idx.into_iter().take(cap).collect();
    if selected.len() < cap {
        let remaining = cap - selected.len();
        selected.extend(other_idx.into_iter().take(remaining));
    }

    selected.sort_unstable();
    selected.into_iter().map(|i| events[i].clone()).collect()
}

/// Serializes one sensor's data, capping its `events`/`gateways` detail. The
/// caps are parameters (rather than always using the module constants) so
/// `build_lorawan_sensors_payload` can try progressively more aggressive
/// rungs of `LORAWAN_SENSORS_CAP_LADDER` if the result comes out over budget.
fn lorawan_sensor_json(
    s: &super::messages::LoRaWANSensorPayload,
    events_cap: usize,
    gateways_cap: usize,
) -> serde_json::Value {
    let events = select_events(&s.events, events_cap);

    let mut gateways = s.gateways.clone();
    gateways.sort_by(|a, b| b.rssi.unwrap_or(i32::MIN).cmp(&a.rssi.unwrap_or(i32::MIN)));
    gateways.truncate(gateways_cap);

    json!({
        "dev_eui": s.dev_eui,
        "name": s.name,
        "serial_number": s.serial_number,
        "location": s.location,
        "fields": s.fields,
        "field_alarm_states": s.field_alarm_states,
        "field_thresholds": s.field_thresholds,
        "counters": s.counters,
        "events": events,
        // Additive per-gateway reception detail. `rssi`/`snr` stay the
        // strongest receiver so existing consumers are unaffected;
        // `gateways[]` is what tells you WHICH gateway heard the frame.
        "gateways": gateways,
        // `fcnt` is lifted out of `counters` to a top-level field so a
        // consumer does not have to know it lives under a counter name.
        "fcnt": s.counters.get("fCnt").copied(),
        "dr": s.dr,
        "downlink_gateway_id": s.downlink_gateway_id,
        "rssi": s.rssi,
        "snr": s.snr,
        "last_seen": s.last_seen,
        "alarm_state": s.alarm_state,
    })
}

/// Absolute-last-resort fields when even the most aggressive ladder rung
/// still exceeds budget (pathologically many sensors). The sensor *list*
/// itself must never shrink — the viewer's `sync_lorawan_sensors`
/// reconciliation deletes any Node missing from a snapshot — so this keeps
/// only what's needed for that reconciliation plus current alarm-relevant
/// state, dropping every diagnostic/detail field.
fn lorawan_sensor_json_minimal(s: &super::messages::LoRaWANSensorPayload) -> serde_json::Value {
    json!({
        "dev_eui": s.dev_eui,
        "name": s.name,
        "fields": s.fields,
        "field_alarm_states": s.field_alarm_states,
        "field_thresholds": s.field_thresholds,
        "counters": s.counters,
        "alarm_state": s.alarm_state,
        "last_seen": s.last_seen,
    })
}

/// Builds the `lorawan/sensors` payload, guaranteed to stay under
/// `LORAWAN_SENSORS_PAYLOAD_BUDGET_BYTES` in all but pathological cases.
/// Tries each rung of `LORAWAN_SENSORS_CAP_LADDER` in order (most detail
/// first); if the combined result is still too big at every rung — e.g. a
/// gateway with dozens of paired Nodes — falls back to
/// `lorawan_sensor_json_minimal`, which always preserves every sensor and
/// its safety-relevant fields (`dev_eui`, `fields`, `field_alarm_states`,
/// `field_thresholds`, `counters`, `alarm_state`) even though it drops
/// events/gateways/diagnostics entirely. This minimal fallback is only
/// held to `LORAWAN_SENSORS_HARD_CEILING_BYTES` — the real wire limit — not
/// the tighter comfortable budget the ladder itself targets.
fn build_lorawan_sensors_payload(
    sensors: &[super::messages::LoRaWANSensorPayload],
    timestamp: &str,
) -> String {
    for &(events_cap, gateways_cap) in LORAWAN_SENSORS_CAP_LADDER {
        let sensors_data: Vec<_> = sensors
            .iter()
            .map(|s| lorawan_sensor_json(s, events_cap, gateways_cap))
            .collect();
        let payload = json!({ "timestamp": timestamp, "sensors": sensors_data }).to_string();
        if payload.len() <= LORAWAN_SENSORS_PAYLOAD_BUDGET_BYTES {
            return payload;
        }
        eprintln!(
            "[MQTT Publisher] lorawan/sensors payload {} bytes exceeds budget {} at events_cap={} gateways_cap={} ({} sensors); trying a more aggressive cap",
            payload.len(),
            LORAWAN_SENSORS_PAYLOAD_BUDGET_BYTES,
            events_cap,
            gateways_cap,
            sensors.len()
        );
    }

    let sensors_data: Vec<_> = sensors.iter().map(lorawan_sensor_json_minimal).collect();
    let payload = json!({ "timestamp": timestamp, "sensors": sensors_data }).to_string();
    if payload.len() > LORAWAN_SENSORS_HARD_CEILING_BYTES {
        let dev_euis: Vec<&str> = sensors.iter().map(|s| s.dev_eui.as_str()).collect();
        eprintln!(
            "[MQTT Publisher] lorawan/sensors payload still {} bytes after minimal-fields fallback (hard ceiling {}, {} sensors: {:?}); publishing anyway",
            payload.len(),
            LORAWAN_SENSORS_HARD_CEILING_BYTES,
            sensors.len(),
            dev_euis
        );
    }
    payload
}

/// Safe byte budget for the combined `eye/sensors` publish. Unlike LoRaWAN
/// sensors — whose size is dominated by a variable-length `events`/`gateways`
/// backlog — a BLE beacon tag is fixed-size (~493 bytes at full detail), so
/// the payload grows purely with fleet size. 16KB keeps full detail for ~32
/// tags while leaving 4KB of headroom under
/// `EYE_SENSORS_HARD_CEILING_BYTES` for the topic name and MQTT framing.
const EYE_SENSORS_PAYLOAD_BUDGET_BYTES: usize = 16 * 1024;

/// The actual wire ceiling this payload must never exceed, matching
/// `MQTT_MAX_PACKET_SIZE_BYTES` in `mqtt/monitor.rs::create_mqtt_options`
/// exactly (kept as a separate constant, not shared across the module
/// boundary, since publisher.rs has no dependency on monitor.rs — if you
/// change one, change the other). `EYE_SENSORS_PAYLOAD_BUDGET_BYTES` above
/// is a *comfortable target* the ladder tries to stay under; this is the
/// hard limit only the minimal-fields last resort is judged against, since a
/// pathologically large enrolled fleet may legitimately need more than the
/// comfortable budget just for `mac`+safety-fields, and that's fine as long
/// as it still fits on the wire at all.
const EYE_SENSORS_HARD_CEILING_BYTES: usize = 20 * 1024;

/// How much per-tag detail a rung of `EYE_SENSORS_CAP_LADDER` serializes.
/// There is deliberately no `Minimal` variant: `beacon_tag_json_minimal` is
/// not a rung but the last resort past the bottom of the ladder, judged only
/// against `EYE_SENSORS_HARD_CEILING_BYTES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EyeTagDetail {
    /// Every field the Hub knows — see `beacon_tag_json`.
    Full,
    /// Diagnostics dropped, alarm/identity/safety fields kept — see
    /// `beacon_tag_json_trimmed`.
    Trimmed,
}

/// Ladder of (detail, live-discovered cap) rungs tried in order, most
/// detailed first, until the combined payload fits under
/// `EYE_SENSORS_PAYLOAD_BUDGET_BYTES`.
///
/// Detail degrades before *anything* is dropped, and when tags must finally
/// be dropped only **live discovered strangers** — beacons the Hub merely
/// overheard, that are neither enrolled nor pending a portal-side prune —
/// are eligible. `None` = uncapped. See `build_eye_sensors_payload` for what
/// happens when even the bottom rung still exceeds budget.
const EYE_SENSORS_CAP_LADDER: &[(EyeTagDetail, Option<usize>)] = &[
    (EyeTagDetail::Full, None),
    (EyeTagDetail::Trimmed, None),
    (EyeTagDetail::Trimmed, Some(16)),
    (EyeTagDetail::Trimmed, Some(4)),
    (EyeTagDetail::Trimmed, Some(0)),
];

/// Serializes one BLE beacon tag at full detail — every field the Hub tracks,
/// which is what `eye/sensors` carried unconditionally before the ladder
/// existed. Wire names are load-bearing: they map onto
/// `fiber_protocol::payloads::eye::EyeTag`.
///
/// **One exception, since 2026-09-08:** `is_en12830` is no longer a field on
/// that struct at all. The portal dropped the EN12830 concept (there is one
/// kind of Beacon and there never will be another — devices.beacon.md §1,
/// control.ble.md's 2026-09-01 amendment), and `EyeTag` does not deny
/// unknown fields, so the key deserializes away silently. It is still
/// published here deliberately: this is the portal declining to read a
/// field, not a wire change, and `detect_eye_tag` remains in mqtt.md's
/// `SIGNED_COMMANDS` table with this module still answering it.
fn beacon_tag_json(t: &super::messages::BeaconTagPayload) -> serde_json::Value {
    json!({
        "gateway": t.gateway,
        "mac": t.mac,
        "name": t.name,
        "temperature_c": t.temperature_c,
        "humidity_pct": t.humidity_pct,
        "battery_mv": t.battery_mv,
        "low_battery": t.low_battery,
        "magnet_present": t.magnet_present,
        "magnet_detected": t.magnet_detected,
        "moving": t.moving,
        "movement_count": t.movement_count,
        "pitch_deg": t.pitch_deg,
        "roll_deg": t.roll_deg,
        "rssi": t.rssi,
        "last_seen_ts": t.last_seen_ts,
        "stale": t.stale,
        "provisioning": t.provisioning,
        "discovered": t.discovered,
        "is_en12830": t.is_en12830,
        "field_alarm_states": t.field_alarm_states,
        "alarm_state": t.alarm_state,
        // Present as `null` when unverified or cleared, so the full rung is the
        // one that can tell the portal to forget a configuration; the trimmed
        // rungs omit the key and the portal keeps what it last saw.
        "ble_config": t.ble_config,
    })
}

/// Serializes one tag with the purely diagnostic fields dropped:
/// `gateway`, `magnet_present`, `magnet_detected`, `moving`,
/// `movement_count`, `pitch_deg`, `roll_deg`, `rssi`, `is_en12830`,
/// `ble_config`.
///
/// Safe to omit because the consumer — `fiber_protocol::payloads::eye::EyeTag`
/// — types every field but `mac` as `Option`, so a missing key deserializes to
/// `None`, and the portal's store *merges* these fields (a `None` leaves the
/// last-known value in place rather than clobbering it). `is_en12830` is
/// safer still: it is not a field on that struct any more (see
/// [`beacon_tag_json`]), so dropping it changes nothing either way. `ble_config`
/// is merged the same way — an absent key keeps the last verified configuration,
/// only an explicit `null` clears it. What survives here is
/// everything that is either identity (`mac`, `name`), a live safety reading
/// (`temperature_c`, `humidity_pct`, `battery_mv`, `low_battery`,
/// `last_seen_ts`), alarm state (`field_alarm_states`, `alarm_state`), or one
/// of the fields the store assigns **verbatim** rather than merging (`stale`,
/// `provisioning`, `discovered`, `field_alarm_states`) — see
/// `beacon_tag_json_minimal` for why those four can never be dropped.
fn beacon_tag_json_trimmed(t: &super::messages::BeaconTagPayload) -> serde_json::Value {
    json!({
        "mac": t.mac,
        "name": t.name,
        "temperature_c": t.temperature_c,
        "humidity_pct": t.humidity_pct,
        "battery_mv": t.battery_mv,
        "low_battery": t.low_battery,
        "last_seen_ts": t.last_seen_ts,
        // Kept because a Server computes the Beacons' home Hubs from it (decision #42).
        "rssi": t.rssi,
        "stale": t.stale,
        "provisioning": t.provisioning,
        "discovered": t.discovered,
        "field_alarm_states": t.field_alarm_states,
        "alarm_state": t.alarm_state,
    })
}

/// Absolute-last-resort fields when even the bottom rung of
/// `EYE_SENSORS_CAP_LADDER` still exceeds budget (pathologically many
/// enrolled tags). Keeps exactly `mac`, `name`, `temperature_c`,
/// `humidity_pct`, `low_battery`, `last_seen_ts`, `stale`, `provisioning`,
/// `discovered`, `field_alarm_states` and `alarm_state`.
///
/// The four the portal's store assigns **verbatim** — `stale`,
/// `provisioning`, `discovered`, `field_alarm_states` — are the reason this
/// set cannot shrink further. Portal ingest is upsert-only: it never deletes a
/// beacon just because it went missing from a snapshot, so a tag carrying
/// `stale: true` *and* `discovered: true` is the portal's only instruction to
/// prune an overheard stranger. Drop that tag (or either of those two flags)
/// and the ghost beacon is stranded in the portal forever. `alarm_state` /
/// `field_alarm_states` and the safety readings stay for the same reason they
/// do in the LoRaWAN minimal fallback: they are what the alarm pipeline reads.
fn beacon_tag_json_minimal(t: &super::messages::BeaconTagPayload) -> serde_json::Value {
    json!({
        "mac": t.mac,
        "name": t.name,
        "temperature_c": t.temperature_c,
        "humidity_pct": t.humidity_pct,
        "low_battery": t.low_battery,
        "last_seen_ts": t.last_seen_ts,
        "stale": t.stale,
        "provisioning": t.provisioning,
        "discovered": t.discovered,
        "field_alarm_states": t.field_alarm_states,
        "alarm_state": t.alarm_state,
    })
}

/// A tag the ladder is allowed to drop: a stranger currently advertising.
/// See `build_eye_sensors_payload` for the three-way classification.
fn is_live_stranger_tag(t: &super::messages::BeaconTagPayload) -> bool {
    t.discovered && !t.stale
}

/// `discovered_capped` as it goes on the wire.
///
/// One rule beyond the OR the callers compute: **an empty `tags` array is
/// never capped**. `tags: []` is the empty snapshot — the portal's instruction
/// to prune every discovered-only beacon it holds — and a cap flag on it would
/// say "there are strangers we did not send you" about a snapshot that is
/// authoritative precisely because it is empty.
fn capped_flag(tags_data: &[serde_json::Value], capped: bool) -> bool {
    !tags_data.is_empty() && capped
}

/// Builds the `eye/sensors` payload, guaranteed to stay under
/// `EYE_SENSORS_PAYLOAD_BUDGET_BYTES` in all but pathological cases.
///
/// Tags fall into three classes, and only one of them is ever droppable:
///
/// * **enrolled** (`!discovered`) — registered in `eye.tags`; the operator's
///   fleet. Never dropped.
/// * **stale-discovered** (`stale && discovered`) — an overheard stranger that
///   has since gone quiet. Never dropped, because this is the *only* prune
///   instruction the portal ever receives (ingest is upsert-only); omitting it
///   strands a ghost beacon permanently.
/// * **live-discovered** — a stranger currently advertising nearby. Pure
///   adoption-candidate noise, unbounded in a busy building, and the only
///   class the ladder is allowed to cap.
///
/// Each rung of `EYE_SENSORS_CAP_LADDER` is tried in order: detail is
/// sacrificed first (full → trimmed), then live-discovered strangers are
/// capped progressively (uncapped → 16 → 4 → 0), keeping the newest by
/// `last_seen_ts`. Output preserves the tags' original relative order at every
/// rung — dropped strangers are filtered out in place, never re-sorted — so
/// consecutive snapshots stay diffable by eye.
///
/// If even the bottom rung is over budget, falls back to
/// `beacon_tag_json_minimal` over enrolled + stale-discovered tags only. That
/// fallback is judged solely against `EYE_SENSORS_HARD_CEILING_BYTES` — the
/// real wire limit — not the tighter comfortable budget; if it *still* does
/// not fit, the payload is published anyway rather than silently withholding
/// alarm state from the portal.
fn build_eye_sensors_payload(
    tags: &[super::messages::BeaconTagPayload],
    timestamp: &str,
    host_capped: bool,
) -> String {
    // Classify once. `discovered == false` is the enrolled/registered axis —
    // see `BeaconTagPayload::discovered` for why `provisioning` cannot answer
    // this question on its own.
    let is_live_stranger: Vec<bool> = tags.iter().map(is_live_stranger_tag).collect();
    let enrolled_count = tags.iter().filter(|t| !t.discovered).count();
    let stale_stranger_count = tags.iter().filter(|t| t.discovered && t.stale).count();
    let live_stranger_count = is_live_stranger.iter().filter(|&&s| s).count();

    // Live strangers ranked newest-first, so a cap keeps the freshest
    // adoption candidates; ties break on original position for determinism.
    let mut ranked_strangers: Vec<usize> =
        (0..tags.len()).filter(|&i| is_live_stranger[i]).collect();
    ranked_strangers.sort_by(|&a, &b| {
        tags[b]
            .last_seen_ts
            .unwrap_or(i64::MIN)
            .cmp(&tags[a].last_seen_ts.unwrap_or(i64::MIN))
            .then(a.cmp(&b))
    });

    for &(detail, stranger_cap) in EYE_SENSORS_CAP_LADDER {
        // Mask of which live strangers survive this rung; enrolled and
        // stale-discovered tags are unconditionally kept.
        let mut keep = is_live_stranger.clone();
        if let Some(cap) = stranger_cap {
            for &i in ranked_strangers.iter().skip(cap) {
                keep[i] = false;
            }
        }

        let tags_data: Vec<_> = tags
            .iter()
            .enumerate()
            .filter(|&(i, _)| !is_live_stranger[i] || keep[i])
            .map(|(_, t)| match detail {
                EyeTagDetail::Full => beacon_tag_json(t),
                EyeTagDetail::Trimmed => beacon_tag_json_trimmed(t),
            })
            .collect();
        // The flag on the wire is the OR of the two layers that can drop a
        // stranger: the host's own `discovered_max`, and this rung. It is
        // computed per rung rather than once, because which rung wins is
        // exactly what decides whether *this* snapshot dropped anybody
        // (devices.backend.md §22.1).
        let rung_dropped = keep.iter().zip(&is_live_stranger).any(|(k, s)| *s && !*k);
        let payload = json!({
            "timestamp": timestamp,
            "discovered_capped": capped_flag(&tags_data, host_capped || rung_dropped),
            "tags": tags_data,
        })
        .to_string();
        if payload.len() <= EYE_SENSORS_PAYLOAD_BUDGET_BYTES {
            return payload;
        }
        eprintln!(
            "[MQTT Publisher] eye/sensors payload {} bytes exceeds budget {} at detail={:?} live_discovered_cap={:?} ({} tags: {} enrolled, {} stale-discovered, {} live-discovered); trying a more aggressive rung",
            payload.len(),
            EYE_SENSORS_PAYLOAD_BUDGET_BYTES,
            detail,
            stranger_cap,
            tags.len(),
            enrolled_count,
            stale_stranger_count,
            live_stranger_count
        );
    }

    let tags_data: Vec<_> = tags
        .iter()
        .filter(|t| !is_live_stranger_tag(t))
        .map(beacon_tag_json_minimal)
        .collect();
    let kept = tags_data.len();
    // The minimal fallback drops **every** live stranger by construction, so
    // it reports a cap whenever there was one to drop.
    let payload = json!({
        "timestamp": timestamp,
        "discovered_capped": capped_flag(&tags_data, host_capped || live_stranger_count > 0),
        "tags": tags_data,
    })
    .to_string();
    if payload.len() > EYE_SENSORS_HARD_CEILING_BYTES {
        let macs: Vec<&str> = tags
            .iter()
            .filter(|t| !is_live_stranger_tag(t))
            .map(|t| t.mac.as_str())
            .collect();
        eprintln!(
            "[MQTT Publisher] eye/sensors payload still {} bytes after minimal-fields fallback (hard ceiling {}, {} undroppable tags: {:?}); publishing anyway",
            payload.len(),
            EYE_SENSORS_HARD_CEILING_BYTES,
            kept,
            macs
        );
    }
    payload
}

/// Warn threshold for the retained `config/state` document. Unlike the
/// `lorawan/sensors` and `eye/sensors` snapshots, this topic is published
/// **retained at QoS 1**, so an oversize document is not one dropped message
/// but a standing fault: the broker replays it on every portal reconnect,
/// rumqttc's event loop errors on the outsized packet, and the connection is
/// torn down again. The document grows with the paired Node count (~150 bytes
/// per `lorawan_sensors` entry, with `field_thresholds` the only nested
/// grower), so this warns well before the 20480-byte wire ceiling
/// (`CONFIG_STATE_HARD_CEILING_BYTES`) — early enough for a fleet drifting
/// toward it to be noticed. Real bounding — per-entity retained topics, so
/// the document stops scaling with fleet size at all — is deferred to the
/// planned wire-shape redesign; `build_config_state_payload` is a guard, not
/// that fix.
const CONFIG_STATE_WARN_BYTES: usize = 16 * 1024;

/// The actual wire ceiling this payload must never exceed, matching
/// `MQTT_MAX_PACKET_SIZE_BYTES` in `mqtt/monitor.rs::create_mqtt_options`
/// exactly (kept as a separate constant, not shared across the module
/// boundary, since publisher.rs has no dependency on monitor.rs — if you
/// change one, change the other). `CONFIG_STATE_WARN_BYTES` above is the
/// point at which the size is merely logged; this is the limit at which
/// `build_config_state_payload` actually drops `field_thresholds`.
const CONFIG_STATE_HARD_CEILING_BYTES: usize = 20 * 1024;

/// Builds the retained `config/state` document, with a size guard rather
/// than a full degradation ladder: the document is the authoritative Node
/// **and** Beacon inventory (a hostname's `lorawan_sensors` list here *is*
/// the full set, and so is `eye_tags` for the enrolled Beacons; absence means
/// removed — see `fiber-protocol`'s `config_state.rs` module docs), so no
/// entry may ever be dropped to save bytes.
///
/// `eye_tags` is `Option` all the way down for that reason: an empty array is
/// an instruction to prune the fleet, so a Hub that cannot read its own
/// `eye:` section omits the key rather than publishing one.
///
/// * Under `CONFIG_STATE_WARN_BYTES`: published verbatim, silently.
/// * Over it: logged, with the entry counts needed to see what is growing.
/// * Over `CONFIG_STATE_HARD_CEILING_BYTES`: rebuilt **once** with
///   `field_thresholds` omitted from every `lorawan_sensors` entry — the only
///   unbounded nested grower — and logged.
/// * Still over after that: logged and published anyway, same last-resort
///   semantics as `build_lorawan_sensors_payload` /
///   `build_eye_sensors_payload`, since withholding the document entirely
///   would leave the portal with a stale inventory.
#[allow(clippy::too_many_arguments)]
fn build_config_state_payload(
    timestamp: &str,
    led_brightness: u8,
    screen_brightness: u8,
    screen_timeout_secs: u32,
    buzzer_volume: u8,
    system_info_interval_s: u64,
    device_label: &str,
    sensors: &[super::messages::SensorConfigData],
    lorawan_sensors: &[super::messages::LoRaWANSensorConfigData],
    sample_interval_ms: u64,
    aggregation_interval_ms: u64,
    report_interval_ms: u64,
    beacon_enabled: bool,
    beacon_auto_provision: bool,
    beacon_auto_discover: bool,
    eye_tags: Option<&[super::messages::EyeTagConfigData]>,
    display_lines: &[crate::libs::config::DisplayLine],
) -> String {
    let sensors_data: Vec<serde_json::Value> = sensors
        .iter()
        .map(|sensor| {
            json!({
                "line": sensor.line,
                "name": sensor.name,
                "location": sensor.location,
                "enabled": sensor.enabled,
                "has_override": sensor.has_override,
                "thresholds": {
                    "critical_low_celsius": sensor.thresholds.critical_low_celsius,
                    "low_alarm_celsius": sensor.thresholds.low_alarm_celsius,
                    "warning_low_celsius": sensor.thresholds.warning_low_celsius,
                    "warning_high_celsius": sensor.thresholds.warning_high_celsius,
                    "high_alarm_celsius": sensor.thresholds.high_alarm_celsius,
                    "critical_high_celsius": sensor.thresholds.critical_high_celsius,
                    "hysteresis_celsius": sensor.thresholds.hysteresis_celsius,
                    "enabled": sensor.thresholds.enabled,
                },
            })
        })
        .collect();

    let lorawan_sensors_data: Vec<serde_json::Value> = lorawan_sensors
        .iter()
        .map(|s| {
            json!({
                "dev_eui": s.dev_eui,
                "name": s.name,
                "serial_number": s.serial_number,
                "location": s.location,
                "enabled": s.enabled,
                "field_thresholds": s.field_thresholds,
            })
        })
        .collect();

    // The enrolled inventory. Present as an array — including an empty one —
    // whenever the Hub knows its own `eye.tags`; the key is absent entirely
    // when it does not, because the portal prunes on `[]` and makes no
    // statement on an absent key (`ble_host::config_store::enrolled_tags`,
    // `fiber-protocol`'s `config_state.rs`, devices.backend.md §22.2).
    let eye_tags_data: Option<Vec<serde_json::Value>> = eye_tags.map(|tags| {
        tags.iter()
            .map(|t| json!({ "mac": t.mac, "name": t.name }))
            .collect()
    });

    let mut document = json!({
        "timestamp": timestamp,
        "led_brightness": led_brightness,
        "screen_brightness": screen_brightness,
        "screen_timeout_secs": screen_timeout_secs,
        "buzzer_volume": buzzer_volume,
        "system_info_interval_s": system_info_interval_s,
        "device_label": device_label,
        "sensors": sensors_data,
        "lorawan_sensors": lorawan_sensors_data,
        "intervals": {
            "sample_interval_ms": sample_interval_ms,
            "aggregation_interval_ms": aggregation_interval_ms,
            "report_interval_ms": report_interval_ms,
        },
        "eye": {
            "enabled": beacon_enabled,
            "auto_provision": beacon_auto_provision,
            "auto_discover": beacon_auto_discover,
        },
    });
    if let Some(tags) = eye_tags_data {
        document["eye_tags"] = serde_json::Value::Array(tags);
    }
    // The LCD rows the Hub renders, always present: `[]` is the Hub saying it
    // shows the built-in layout, which the portal needs to untick its picker
    // (an absent key would leave the portal's last list standing). Serialized
    // through `DisplayLine`'s own serde, the same wire shape as
    // `set_display_lines` and `fiber_protocol::DisplayLine` (GitLab #74).
    document["display_lines"] = serde_json::to_value(display_lines).unwrap_or_else(|_| json!([]));

    let payload = document.to_string();
    if payload.len() <= CONFIG_STATE_WARN_BYTES {
        return payload;
    }

    let with_thresholds = lorawan_sensors
        .iter()
        .filter(|s| !s.field_thresholds.is_empty())
        .count();
    eprintln!(
        "[MQTT Publisher] config/state payload {} bytes exceeds warn threshold {} ({} sensors, {} lorawan_sensors, {} of them carrying field_thresholds); this topic is retained at QoS 1, so an oversize replays on every portal reconnect",
        payload.len(),
        CONFIG_STATE_WARN_BYTES,
        sensors.len(),
        lorawan_sensors.len(),
        with_thresholds
    );
    if payload.len() <= CONFIG_STATE_HARD_CEILING_BYTES {
        return payload;
    }

    // One-step fallback: drop `field_thresholds` from every `lorawan_sensors`
    // entry. Verified parse-safe against the consumer: every field of
    // `ConfigStateMsg` in `fiber-protocol/src/payloads/config_state.rs` is
    // `Option<_>` or `#[serde(default)]`, and
    // `LorawanSensorConfigEntry::field_thresholds` is specifically
    // `Option<Vec<FieldThreshold>>`, so an omitted key deserializes to `None`
    // instead of failing. `None` there means "not authoritative for this
    // Node" (that field's doc comment; `thresholds_authoritative` at
    // handlers.py:2610), which is the honest reading of a document whose
    // thresholds did not fit — the portal keeps its own values rather than
    // being told a wrong set.
    let mut trimmed = document;
    if let Some(entries) = trimmed["lorawan_sensors"].as_array_mut() {
        for entry in entries.iter_mut() {
            if let Some(obj) = entry.as_object_mut() {
                obj.remove("field_thresholds");
            }
        }
    }
    let trimmed_payload = trimmed.to_string();
    eprintln!(
        "[MQTT Publisher] config/state payload {} bytes exceeds hard ceiling {}; rebuilt without field_thresholds ({} of {} lorawan_sensors entries carried them), now {} bytes",
        payload.len(),
        CONFIG_STATE_HARD_CEILING_BYTES,
        with_thresholds,
        lorawan_sensors.len(),
        trimmed_payload.len()
    );
    if trimmed_payload.len() > CONFIG_STATE_HARD_CEILING_BYTES {
        eprintln!(
            "[MQTT Publisher] config/state payload still {} bytes after dropping field_thresholds (hard ceiling {}, {} sensors, {} lorawan_sensors); publishing anyway",
            trimmed_payload.len(),
            CONFIG_STATE_HARD_CEILING_BYTES,
            sensors.len(),
            lorawan_sensors.len()
        );
    }
    trimmed_payload
}

/// Message publisher that formats and publishes MQTT messages
pub struct MqttPublisher {
    client: AsyncClient,
    topics: TopicBuilder,
    qos_overrides: QosOverrides,
}

impl MqttPublisher {
    /// Create a new MQTT publisher
    pub fn new(client: AsyncClient, topics: TopicBuilder, config: &PublishConfig) -> Self {
        Self {
            client,
            topics,
            qos_overrides: config.qos_overrides.clone(),
        }
    }

    /// Get current timestamp as ISO 8601 string
    pub(crate) fn timestamp() -> String {
        let now: DateTime<Utc> = Utc::now();
        now.to_rfc3339()
    }

    /// Convert QoS u8 to rumqttc::QoS
    fn qos_from_u8(qos: u8) -> QoS {
        match qos {
            0 => QoS::AtMostOnce,
            1 => QoS::AtLeastOnce,
            2 => QoS::ExactlyOnce,
            _ => QoS::AtMostOnce,
        }
    }

    /// Publish a message to a topic
    async fn publish(
        &self,
        topic: String,
        payload: String,
        qos: QoS,
        retain: bool,
    ) -> Result<(), String> {
        self.client
            .publish(topic.clone(), qos, retain, payload.as_bytes())
            .await
            .map_err(|e| format!("Failed to publish to {}: {}", topic, e))
    }

    /// Handle incoming MQTT messages for publishing
    pub async fn handle_message(&self, msg: MqttMessage) -> Result<(), String> {
        match msg {
            MqttMessage::PublishAlarmEvent {
                line,
                name,
                from_state,
                to_state,
                temperature,
            } => {
                self.publish_alarm_event(line, &name, from_state, to_state, temperature)
                    .await
            }

            MqttMessage::PublishSystemAlarmEvent {
                alarm_type,
                name,
                from_state,
                to_state,
                message,
            } => {
                self.publish_system_alarm_event(
                    &alarm_type,
                    &name,
                    &from_state,
                    &to_state,
                    &message,
                )
                .await
            }

            MqttMessage::PublishAccelerometerEvent {
                x_g,
                y_g,
                z_g,
                position,
            } => {
                self.publish_accelerometer_event(x_g, y_g, z_g, position)
                    .await
            }

            MqttMessage::PublishStandbyState {
                standby,
                reason,
                requested_by,
                entered_at,
                vin_mv,
            } => {
                self.publish_standby_state(
                    standby,
                    &reason,
                    &requested_by,
                    entered_at.as_deref(),
                    vin_mv,
                )
                .await
            }

            MqttMessage::PublishSystemStatus {
                hostname,
                device_label,
                version,
                uptime_seconds,
                battery_mv,
                battery_percent,
                vin_mv,
                on_dc_power,
                last_dc_loss_time,
                wifi_connected,
                wifi_signal_dbm,
                wifi_ip,
                ethernet_connected,
                ethernet_ip,
                storage_total_bytes,
                storage_available_bytes,
                storage_used_percent,
                lorawan_gateway_present,
                lorawan_concentratord_running,
                lorawan_chirpstack_running,
                lorawan_sensor_count,
                setup_provisioned,
                setup_role,
            } => {
                self.publish_system_status(
                    &hostname,
                    &device_label,
                    &version,
                    uptime_seconds,
                    battery_mv,
                    battery_percent,
                    vin_mv,
                    on_dc_power,
                    last_dc_loss_time,
                    wifi_connected,
                    wifi_signal_dbm,
                    wifi_ip,
                    ethernet_connected,
                    ethernet_ip,
                    storage_total_bytes,
                    storage_available_bytes,
                    storage_used_percent,
                    lorawan_gateway_present,
                    lorawan_concentratord_running,
                    lorawan_chirpstack_running,
                    lorawan_sensor_count,
                    setup_provisioned,
                    setup_role,
                )
                .await
            }

            MqttMessage::PublishAggregatedSensorData {
                period,
                names,
                locations,
            } => {
                let topic = self.topics.sensors_aggregated();
                let qos = Self::qos_from_u8(self.qos_overrides.sensor_readings);
                self.publish_aggregated_sensor_data(period, &names, &locations, topic, qos)
                    .await
            }

            MqttMessage::PublishSensorSummary {
                period,
                names,
                locations,
            } => {
                // Live and superseded by the next sample: losing one is cheaper
                // than queueing a backlog of stale values.
                let topic = self.topics.sensors_summary();
                self.publish_aggregated_sensor_data(
                    period,
                    &names,
                    &locations,
                    topic,
                    QoS::AtMostOnce,
                )
                .await
            }

            MqttMessage::PublishConfigChallenge {
                challenge_id,
                request_id,
                signer_id,
                expires_at,
                preview,
            } => {
                self.publish_config_challenge(
                    &challenge_id,
                    &request_id,
                    &signer_id,
                    expires_at,
                    preview,
                )
                .await
            }

            MqttMessage::PublishConfigResponse {
                challenge_id,
                request_id,
                status,
                applied_at,
                effective_at,
                message,
                result,
            } => {
                self.publish_config_response(
                    &challenge_id,
                    &request_id,
                    &status,
                    applied_at,
                    effective_at,
                    &message,
                    result.as_ref(),
                )
                .await
            }

            MqttMessage::PublishSensorConfig { sensors } => {
                self.publish_sensor_config(sensors).await
            }

            MqttMessage::PublishIntervalConfig {
                sample_interval_ms,
                aggregation_interval_ms,
                report_interval_ms,
            } => {
                self.publish_interval_config(
                    sample_interval_ms,
                    aggregation_interval_ms,
                    report_interval_ms,
                )
                .await
            }

            MqttMessage::PublishConfigState {
                led_brightness,
                screen_brightness,
                screen_timeout_secs,
                buzzer_volume,
                system_info_interval_s,
                device_label,
                sensors,
                lorawan_sensors,
                sample_interval_ms,
                aggregation_interval_ms,
                report_interval_ms,
                beacon_enabled,
                beacon_auto_provision,
                beacon_auto_discover,
                eye_tags,
                display_lines,
            } => {
                self.publish_config_state(
                    led_brightness,
                    screen_brightness,
                    screen_timeout_secs,
                    buzzer_volume,
                    system_info_interval_s,
                    &device_label,
                    sensors,
                    lorawan_sensors,
                    sample_interval_ms,
                    aggregation_interval_ms,
                    report_interval_ms,
                    beacon_enabled,
                    beacon_auto_provision,
                    beacon_auto_discover,
                    eye_tags,
                    display_lines,
                )
                .await
            }

            MqttMessage::PublishLoRaWANSensorData { sensors } => {
                self.publish_lorawan_sensors(sensors).await
            }

            MqttMessage::PublishLoRaWANGatewayData { gateways } => {
                self.publish_lorawan_gateways(gateways).await
            }

            MqttMessage::PublishNodeConfig {
                dev_eui,
                config,
                page_index,
                page_count,
                last_seq,
                last_result,
            } => {
                self.publish_node_config(
                    dev_eui,
                    config,
                    page_index,
                    page_count,
                    last_seq,
                    last_result,
                )
                .await
            }

            MqttMessage::PublishNodeFullConfig {
                dev_eui,
                config,
                page_count,
                last_seq,
                read_status,
                missing,
            } => {
                self.publish_node_full_config(
                    dev_eui,
                    config,
                    page_count,
                    last_seq,
                    read_status,
                    missing,
                )
                .await
            }

            MqttMessage::PublishNodeInfo { dev_eui, info } => {
                self.publish_node_info(dev_eui, info).await
            }

            MqttMessage::ClearNodeInfo { dev_eui } => self.clear_node_info(&dev_eui).await,

            MqttMessage::PublishNodeCommandResult {
                dev_eui,
                command,
                seq,
                result,
                expect,
                detail,
                fault_key,
            } => {
                self.publish_node_command_result(
                    dev_eui, command, seq, result, expect, detail, fault_key,
                )
                .await
            }

            MqttMessage::PublishNodeHistory {
                dev_eui,
                frame_index,
                frame_count,
                records,
            } => {
                self.publish_node_history(dev_eui, frame_index, frame_count, records)
                    .await
            }

            MqttMessage::PublishBeaconSensorData {
                tags,
                discovered_capped,
            } => self.publish_beacon_sensors(tags, discovered_capped).await,

            MqttMessage::PublishBeaconAggregated { payload } => {
                self.publish_beacon_aggregated(payload).await
            }

            MqttMessage::PublishBeaconDetectResult {
                mac,
                is_en12830,
                status,
            } => {
                self.publish_beacon_detect_result(&mac, is_en12830, &status)
                    .await
            }

            MqttMessage::PublishBeaconHistory { payload } => {
                self.publish_beacon_history(&payload).await
            }

            MqttMessage::PublishBeaconCluster { hub, payload } => {
                let topic = self.topics.beacon_cluster_of(&hub);
                self.publish(topic, payload, QoS::AtLeastOnce, true).await
            }

            MqttMessage::PublishClusterRadio { hub, payload } => {
                let topic = self.topics.cluster_radio_of(&hub);
                self.publish(topic, payload, QoS::AtLeastOnce, true).await
            }

            MqttMessage::PublishBeaconHomes { payload } => {
                let topic = self.topics.beacon_homes();
                self.publish(topic, payload, QoS::AtLeastOnce, true).await
            }

            MqttMessage::PublishPairingResponse(response) => {
                self.publish_pairing_response(&response).await
            }

            MqttMessage::PublishPairingError(error) => self.publish_pairing_error(&error).await,

            MqttMessage::PublishPairingInvite { code, origin } => {
                self.publish_pairing_invite(&code, &origin).await
            }

            // Internal messages, not published
            MqttMessage::SetConnectionState(_) | MqttMessage::Shutdown => Ok(()),
        }
    }

    /// Publish aggregated sensor data for a completed period (or, on
    /// `sensors/summary`, one live sample cycle in the same shape)
    async fn publish_aggregated_sensor_data(
        &self,
        period: crate::libs::sensors::aggregation::AggregationPeriod,
        names: &[String; 8],
        locations: &[Option<String>; 8],
        topic: String,
        qos: QoS,
    ) -> Result<(), String> {
        // Build JSON payload with all 8 sensors
        let sensors_data: Vec<serde_json::Value> = period.sensors.iter()
            .map(|sensor| {
                // Only include valid sensor data
                let temp_data = if sensor.sample_count > 0 {
                    json!({
                        "min_celsius": sensor.min_temp_celsius,
                        "max_celsius": sensor.max_temp_celsius,
                        "avg_celsius": sensor.avg_temp_celsius,
                    })
                } else {
                    serde_json::Value::Null
                };

                // Get sensor name and location
                let name = &names[sensor.line as usize];
                let location = &locations[sensor.line as usize];

                json!({
                    "line": sensor.line,
                    "name": name,
                    "location": location,
                    "sample_count": sensor.sample_count,
                    "disconnected_count": sensor.disconnected_count,
                    "temperature": temp_data,
                    "alarm_counts": {
                        "normal": sensor.alarm_counts.normal,
                        "warning": sensor.alarm_counts.warning,
                        "critical": sensor.alarm_counts.critical,
                        "disconnected": sensor.alarm_counts.disconnected,
                        "reconnecting": sensor.alarm_counts.reconnecting,
                    },
                    "dominant_alarm_state": format!("{:?}", sensor.dominant_alarm_state()).to_uppercase(),
                    "alarm_triggered_at": sensor.alarm_triggered_at.map(|ts| {
                        DateTime::<Utc>::from_timestamp(ts as i64, 0)
                            .map(|dt| dt.to_rfc3339())
                            .unwrap_or_default()
                    }),
                })
            })
            .collect();

        let payload = json!({
            "timestamp": Self::timestamp(),
            "period_start_ts": period.period_start_ts,
            "period_end_ts": period.period_end_ts,
            "duration_sec": period.period_end_ts - period.period_start_ts,
            "sensors": sensors_data,
        });

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish alarm event
    async fn publish_alarm_event(
        &self,
        line: u8,
        name: &str,
        from_state: AlarmState,
        to_state: AlarmState,
        temperature: f32,
    ) -> Result<(), String> {
        let payload = json!({
            "timestamp": Self::timestamp(),
            "line": line,
            "name": name,
            "from_state": format!("{:?}", from_state).to_uppercase(),
            "to_state": format!("{:?}", to_state).to_uppercase(),
            "temperature_celsius": temperature,
            "event_type": "alarm_transition",
        });

        let topic = self.topics.alarms_events();
        let qos = Self::qos_from_u8(self.qos_overrides.alarm_events);

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish system-level alarm event (power, wifi, ethernet)
    async fn publish_system_alarm_event(
        &self,
        alarm_type: &str,
        name: &str,
        from_state: &str,
        to_state: &str,
        message: &str,
    ) -> Result<(), String> {
        let payload = json!({
            "timestamp": Self::timestamp(),
            "line": 0,
            "name": name,
            "from_state": from_state,
            "to_state": to_state,
            "temperature_celsius": null,
            "event_type": "system_alarm",
            "alarm_type": alarm_type,
            "message": message,
        });

        let topic = self.topics.alarms_events();
        let qos = Self::qos_from_u8(self.qos_overrides.alarm_events);

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish the device's standby state, plus a one-off event for the edge.
    ///
    /// The state topic is **retained**: a device in standby is off for as long as
    /// nobody reconnects PoE, which can be days, and a Viewer that subscribes in
    /// the meantime must still be able to tell "switched off by Dr Jane at 14:02"
    /// from "stopped answering". The non-retained event topic is what a
    /// subscriber already listening sees as it happens.
    async fn publish_standby_state(
        &self,
        standby: bool,
        reason: &str,
        requested_by: &str,
        entered_at: Option<&str>,
        vin_mv: u16,
    ) -> Result<(), String> {
        let payload = json!({
            "timestamp": Self::timestamp(),
            "standby": standby,
            "reason": reason,
            "requested_by": requested_by,
            "entered_at": entered_at,
            "vin_mv": vin_mv,
        });
        let body = payload.to_string();
        let qos = Self::qos_from_u8(self.qos_overrides.alarm_events);

        // State first: if only one of the two makes it out, the durable one is
        // the one worth having.
        self.publish(self.topics.power_standby(), body.clone(), qos, true)
            .await?;
        self.publish(self.topics.power_events_standby(), body, qos, false)
            .await
    }

    /// Publish accelerometer motion transition event
    async fn publish_accelerometer_event(
        &self,
        x_g: f32,
        y_g: f32,
        z_g: f32,
        position: u8,
    ) -> Result<(), String> {
        let payload = json!({
            "timestamp": Self::timestamp(),
            "event_type": "motion_transition",
            "x_g": x_g,
            "y_g": y_g,
            "z_g": z_g,
            "position": position,
        });

        let topic = self.topics.accelerometer_events();
        // Motion is high-frequency, non-critical telemetry — losing one
        // transition is harmless since the next event refreshes state.
        let qos = QoS::AtMostOnce;

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish combined system status (power, network, storage, uptime, lorawan)
    #[allow(clippy::too_many_arguments)]
    async fn publish_system_status(
        &self,
        hostname: &str,
        device_label: &str,
        version: &str,
        uptime_seconds: u64,
        battery_mv: u16,
        battery_percent: u8,
        vin_mv: u16,
        on_dc_power: bool,
        last_dc_loss_time: Option<u64>,
        wifi_connected: bool,
        wifi_signal_dbm: i32,
        wifi_ip: Option<String>,
        ethernet_connected: bool,
        ethernet_ip: Option<String>,
        storage_total_bytes: u64,
        storage_available_bytes: u64,
        storage_used_percent: u8,
        lorawan_gateway_present: Option<bool>,
        lorawan_concentratord_running: Option<bool>,
        lorawan_chirpstack_running: Option<bool>,
        lorawan_sensor_count: usize,
        setup_provisioned: bool,
        setup_role: Option<String>,
    ) -> Result<(), String> {
        // Format uptime in human-readable form
        let days = uptime_seconds / 86400;
        let hours = (uptime_seconds % 86400) / 3600;
        let minutes = (uptime_seconds % 3600) / 60;
        let secs = uptime_seconds % 60;
        let uptime_human = if days > 0 {
            format!("{}d {}h {}m {}s", days, hours, minutes, secs)
        } else if hours > 0 {
            format!("{}h {}m {}s", hours, minutes, secs)
        } else if minutes > 0 {
            format!("{}m {}s", minutes, secs)
        } else {
            format!("{}s", secs)
        };

        // Determine battery status
        let battery_status = if battery_percent < 5 {
            "critical"
        } else if battery_percent < 20 {
            "low"
        } else {
            "normal"
        };

        // Format last AC loss timestamp
        let last_dc_loss = last_dc_loss_time
            .and_then(|ts| DateTime::<Utc>::from_timestamp(ts as i64, 0).map(|dt| dt.to_rfc3339()));

        let firmware_version_str = if cfg!(feature = "dev-platform") {
            format!("{}-dev", version)
        } else {
            version.to_string()
        };

        let mut payload = json!({
            "timestamp": Self::timestamp(),
            "hostname": hostname,
            "device_label": device_label,
            "firmware_version": firmware_version_str,
            "dev_mode": cfg!(feature = "dev-platform"),
            "uptime_seconds": uptime_seconds,
            "uptime_human": uptime_human,
            "power": {
                "battery": {
                    "voltage_mv": battery_mv,
                    "percentage": battery_percent,
                    "status": battery_status,
                },
                "dc": {
                    "voltage_mv": vin_mv,
                    "connected": on_dc_power,
                },
                "last_dc_loss": last_dc_loss,
            },
            "network": {
                "wifi": {
                    "connected": wifi_connected,
                    "signal_dbm": wifi_signal_dbm,
                    "ip": wifi_ip,
                },
                "ethernet": {
                    "connected": ethernet_connected,
                    "ip": ethernet_ip,
                },
                "has_internet": wifi_connected || ethernet_connected,
            },
            "storage": {
                "data_partition": {
                    "total_bytes": storage_total_bytes,
                    "available_bytes": storage_available_bytes,
                    "used_percent": storage_used_percent,
                },
            },
            "lorawan": {
                // The rendered radio.mode (off/lorawan/p2p/dual), derived here like gateway_eui
                // below rather than threaded through this function's signature: it is
                // one small YAML read of /run/proximos/control/config.yaml, not caller state.
                // Lets the portal distinguish a missing concentrator that is a fault (lorawan/dual)
                // from one that is normal (off/p2p). `null` on a Hub with no rendered radio config.
                "mode": crate::libs::radio_gate::current_radio_mode(),
                // v1's three unit booleans. Where radio.mode names a transport they come from
                // the v2 LoRaWAN document, not systemctl (`radio_gate::current_lorawan_units`),
                // and are `null` while it is missing or stale: unknown, not down.
                "gateway_present": lorawan_gateway_present,
                "concentratord_running": lorawan_concentratord_running,
                "chirpstack_running": lorawan_chirpstack_running,
                "sensor_count": lorawan_sensor_count,
                // The Hub's own concentrator EUI, as control-radio's `create` converged it
                // (`radio_gate::own_gateway_eui`), derived here like `mode` above. The portal
                // tells its own gateway from external ones by it, and an operator reads it off
                // a Client to register it on the Server (cluster.md §7). `null` until a
                // gateway has been converged. The v1 port's `cluster` sibling (leader/follower
                // role, CA PEM, SAN list) is retired with that port (cluster.md §3); a Client
                // is told apart by `setup.role` below.
                "gateway_eui": crate::libs::radio_gate::current_gateway_eui(),
                // The Hub's default JoinEUI (`radio.join_eui`), `null` without a v2
                // radio config (#86). The portal prefills Node registration with it; a
                // blank field there means this value.
                "join_eui": crate::libs::radio_gate::current_default_join_eui(),
            },
            // The `setup:` section, republished so the portal can show it and keep no copy
            // (GitLab #58, decision #4). An additive sibling key: no schema_version bump, and
            // nothing above changes meaning.
            //
            // `role` is a plain null until setup completes — the same null FB16 serves, so the
            // two surfaces cannot drift into disagreeing about what "no role yet" looks like.
            "setup": {
                "provisioned": setup_provisioned,
                "role": setup_role,
            },
        });
        // The unit's hardware identity from its device tree — CPU serial, compute module
        // model and revision (portal devices.hub.md §3.1, amendment 2026-09-28). Derived here
        // like `mode` above, and another additive sibling key. Left out whole when nothing
        // could be read (a dev host), and each field is left out on its own: absent, not null,
        // so the portal shows "—" and nothing is invented.
        if let Some(hardware) = crate::libs::hardware::current().to_json() {
            payload["hardware"] = hardware;
        }
        // The Southbridge's firmware and its update (plan/system/southbridge_update.md §3.5,
        // amended 2026-10-09). Additive like `hardware`; left out when nothing is known.
        if let Some(southbridge) = crate::libs::southbridge_update::status_json() {
            payload["southbridge"] = southbridge;
        }

        let topic = self.topics.system_info();
        let qos = QoS::AtLeastOnce;

        self.publish(topic, payload.to_string(), qos, true).await
    }

    /// Publish device online status (for Last Will and Testament)
    pub async fn publish_online_status(&self) -> Result<(), String> {
        let payload = json!({
            "status": "online",
            "timestamp": Self::timestamp(),
            "dev_mode": cfg!(feature = "dev-platform"),
        });

        let topic = self.topics.status();
        let qos = QoS::AtLeastOnce;

        self.publish(topic, payload.to_string(), qos, true).await
    }

    /// The `errors`-topic document, as it goes on the wire.
    ///
    /// Split out of [`publish_error`](Self::publish_error) so the shape can
    /// be asserted without a broker. It is the shape design.md §3.2 names
    /// for `unsupported_envelope`, and the tests that only ever checked the
    /// `Result` string are why it went unchecked through P4.
    pub fn error_payload(command: &str, error: &str, message: &str) -> serde_json::Value {
        json!({
            "timestamp": Self::timestamp(),
            "command": command,
            "error": error,
            "message": message,
        })
    }

    /// Publish error message
    pub async fn publish_error(
        &self,
        command: &str,
        error: &str,
        message: &str,
    ) -> Result<(), String> {
        let payload = Self::error_payload(command, error, message);

        let topic = self.topics.errors();
        let qos = QoS::AtLeastOnce;

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish configuration challenge (preview of changes)
    pub async fn publish_config_challenge(
        &self,
        challenge_id: &str,
        request_id: &str,
        signer_id: &str,
        expires_at: i64,
        preview: serde_json::Value,
    ) -> Result<(), String> {
        let payload = json!({
            "timestamp": Self::timestamp(),
            "challenge_id": challenge_id,
            "request_id": request_id,
            "signer_id": signer_id,
            "expires_at": expires_at,
            "expires_at_iso": DateTime::<Utc>::from_timestamp(expires_at, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default(),
            "preview": preview,
            "status": "awaiting_confirmation",
        });

        let topic = self.topics.config_challenge();
        let qos = QoS::ExactlyOnce; // QoS 2 for critical configuration messages

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// The `config/response` payload, exactly as [`Self::publish_config_response`] publishes it.
    /// Its own function so a test can hold the bytes that go on the wire, which every subscriber
    /// of the loopback listener can read (`cluster_clients` tests: no password in them).
    pub fn config_response_payload(
        challenge_id: &str,
        request_id: &str,
        status: &str,
        applied_at: Option<i64>,
        effective_at: Option<i64>,
        message: &str,
        result: Option<&crate::libs::mqtt::messages::ResultDoc>,
    ) -> serde_json::Value {
        let mut payload = json!({
            "timestamp": Self::timestamp(),
            "challenge_id": challenge_id,
            "request_id": request_id,
            "status": status,
            "message": message,
        });

        // A verb's structured answer (plan/control/cluster.md §7). Only on the wire: this
        // function logs nothing of the payload, and `ResultDoc`'s Debug prints no values.
        if let Some(result) = result {
            payload["result"] = result.0.clone();
        }

        // Add applied_at if present
        if let Some(ts) = applied_at {
            payload["applied_at"] = json!(ts);
            payload["applied_at_iso"] = json!(DateTime::<Utc>::from_timestamp(ts, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default());
        }

        // Add effective_at if present
        if let Some(ts) = effective_at {
            payload["effective_at"] = json!(ts);
            payload["effective_at_iso"] = json!(DateTime::<Utc>::from_timestamp(ts, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default());
        }

        payload
    }

    /// Publish configuration response (success/error after applying)
    pub async fn publish_config_response(
        &self,
        challenge_id: &str,
        request_id: &str,
        status: &str,
        applied_at: Option<i64>,
        effective_at: Option<i64>,
        message: &str,
        result: Option<&crate::libs::mqtt::messages::ResultDoc>,
    ) -> Result<(), String> {
        let payload = Self::config_response_payload(
            challenge_id,
            request_id,
            status,
            applied_at,
            effective_at,
            message,
            result,
        );

        let topic = self.topics.config_response();
        let qos = QoS::ExactlyOnce; // QoS 2 for critical configuration messages

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish sensor configuration data (all 8 sensors)
    pub async fn publish_sensor_config(
        &self,
        sensors: Vec<super::messages::SensorConfigData>,
    ) -> Result<(), String> {
        let sensors_data: Vec<serde_json::Value> = sensors
            .iter()
            .map(|sensor| {
                json!({
                    "line": sensor.line,
                    "name": sensor.name,
                    "location": sensor.location,
                    "enabled": sensor.enabled,
                    "has_override": sensor.has_override,
                    "thresholds": {
                        "critical_low_celsius": sensor.thresholds.critical_low_celsius,
                        "low_alarm_celsius": sensor.thresholds.low_alarm_celsius,
                        "warning_low_celsius": sensor.thresholds.warning_low_celsius,
                        "warning_high_celsius": sensor.thresholds.warning_high_celsius,
                        "high_alarm_celsius": sensor.thresholds.high_alarm_celsius,
                        "critical_high_celsius": sensor.thresholds.critical_high_celsius,
                        "hysteresis_celsius": sensor.thresholds.hysteresis_celsius,
                    "enabled": sensor.thresholds.enabled,
                    },
                })
            })
            .collect();

        let payload = json!({
            "timestamp": Self::timestamp(),
            "sensors": sensors_data,
        });

        let topic = self.topics.responses_sensor_config();
        let qos = QoS::AtLeastOnce; // QoS 1 for query responses

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish interval configuration data
    pub async fn publish_interval_config(
        &self,
        sample_interval_ms: u64,
        aggregation_interval_ms: u64,
        report_interval_ms: u64,
    ) -> Result<(), String> {
        let payload = json!({
            "timestamp": Self::timestamp(),
            "intervals": {
                "sample_interval_ms": sample_interval_ms,
                "aggregation_interval_ms": aggregation_interval_ms,
                "report_interval_ms": report_interval_ms,
            },
        });

        let topic = self.topics.responses_interval_config();
        let qos = QoS::AtLeastOnce; // QoS 1 for query responses

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish full device config state to config/state topic
    ///
    /// The document is size-bounded by `build_config_state_payload`: this
    /// topic is retained at QoS 1, so an oversize replays on every portal
    /// reconnect rather than being dropped once — see that function.
    #[allow(clippy::too_many_arguments)]
    pub async fn publish_config_state(
        &self,
        led_brightness: u8,
        screen_brightness: u8,
        screen_timeout_secs: u32,
        buzzer_volume: u8,
        system_info_interval_s: u64,
        device_label: &str,
        sensors: Vec<super::messages::SensorConfigData>,
        lorawan_sensors: Vec<super::messages::LoRaWANSensorConfigData>,
        sample_interval_ms: u64,
        aggregation_interval_ms: u64,
        report_interval_ms: u64,
        beacon_enabled: bool,
        beacon_auto_provision: bool,
        beacon_auto_discover: bool,
        eye_tags: Option<Vec<super::messages::EyeTagConfigData>>,
        display_lines: Vec<crate::libs::config::DisplayLine>,
    ) -> Result<(), String> {
        let payload = build_config_state_payload(
            &Self::timestamp(),
            led_brightness,
            screen_brightness,
            screen_timeout_secs,
            buzzer_volume,
            system_info_interval_s,
            device_label,
            &sensors,
            &lorawan_sensors,
            sample_interval_ms,
            aggregation_interval_ms,
            report_interval_ms,
            beacon_enabled,
            beacon_auto_provision,
            beacon_auto_discover,
            eye_tags.as_deref(),
            &display_lines,
        );

        let topic = self.topics.config_state();
        let qos = QoS::AtLeastOnce;

        self.publish(topic, payload, qos, true).await
    }

    /// Publish LoRaWAN sensor data
    async fn publish_lorawan_sensors(
        &self,
        sensors: Vec<super::messages::LoRaWANSensorPayload>,
    ) -> Result<(), String> {
        let payload = build_lorawan_sensors_payload(&sensors, &Self::timestamp());
        let topic = self.topics.lorawan_sensors();
        let qos = Self::qos_from_u8(self.qos_overrides.sensor_readings);
        self.publish(topic, payload, qos, false).await
    }

    /// Publish external LoRaWAN gateway status
    async fn publish_lorawan_gateways(
        &self,
        gateways: Vec<super::messages::LoRaWANGatewayPayload>,
    ) -> Result<(), String> {
        let gateways_data: Vec<serde_json::Value> = gateways
            .iter()
            .map(|g| {
                json!({
                    "gateway_eui": g.gateway_eui,
                    "name": g.name,
                    "online": g.online,
                    "last_seen": g.last_seen,
                })
            })
            .collect();

        let payload = json!({
            "timestamp": Self::timestamp(),
            "gateways": gateways_data,
        });

        let topic = self.topics.lorawan_gateways();
        let qos = Self::qos_from_u8(self.qos_overrides.sensor_readings);

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish EYE BLE tag sensor data.
    ///
    /// The snapshot is size-bounded by `build_eye_sensors_payload`, which
    /// degrades per-tag detail and then caps overheard strangers rather than
    /// letting the fleet grow the payload past the client's outgoing-packet
    /// ceiling — see that function for the guarantees it keeps.
    async fn publish_beacon_sensors(
        &self,
        tags: Vec<super::messages::BeaconTagPayload>,
        host_capped: bool,
    ) -> Result<(), String> {
        let payload = build_eye_sensors_payload(&tags, &Self::timestamp(), host_capped);

        let topic = self.topics.beacon_sensors();
        let qos = Self::qos_from_u8(self.qos_overrides.sensor_readings);

        self.publish(topic, payload, qos, false).await
    }

    /// Publish a NODE's fPort-85 config read-back to
    /// `lorawan/sensors/<dev_eui>/config` (Feature C). `last_ack` lets the viewer
    /// render pending / awaiting-Ack / ok for a preceding write.
    async fn publish_node_config(
        &self,
        dev_eui: String,
        config: std::collections::BTreeMap<String, serde_json::Value>,
        page_index: u32,
        page_count: u32,
        last_seq: u32,
        last_result: String,
    ) -> Result<(), String> {
        let payload = json!({
            "dev_eui": dev_eui,
            "config": config,
            "page_index": page_index,
            "page_count": page_count,
            "synced_at": Self::timestamp(),
            "last_ack": { "seq": last_seq, "result": last_result },
        });

        let topic = self.topics.lorawan_sensor_config(&dev_eui);
        // Evict any retained value left on this topic by an older build before
        // publishing the real, non-retained snapshot. `/config` is deliberately
        // not retained — a config read is a point-in-time answer, and a retained
        // one is served to every new subscriber as if it were current. An earlier
        // build did retain it, and the leftover survives in the broker until
        // something overwrites it: measured on FIBER-CE3D59F8, a fresh subscriber
        // with no command in flight was handed a snapshot stamped four hours
        // earlier, which the viewer would render as the live config. The empty
        // retained payload is the standard tombstone, same as `clear_node_info`.
        let _ = self
            .publish(topic.clone(), String::new(), QoS::AtLeastOnce, true)
            .await;
        self.publish(topic, payload.to_string(), QoS::AtLeastOnce, false)
            .await
    }

    /// Publish the full non-secret config read-back. `chunks_done`/`chunks_total`
    /// are derived from `missing` rather than tracked separately: the reader
    /// batches at most `MAX_FIELDS_PER_GETPARAM` keys per chunk, so the counts a
    /// progress bar needs are just "how many keys landed out of how many asked".
    async fn publish_node_full_config(
        &self,
        dev_eui: String,
        config: std::collections::BTreeMap<String, serde_json::Value>,
        page_count: u32,
        last_seq: u32,
        read_status: String,
        missing: Vec<String>,
    ) -> Result<(), String> {
        let asked = config.len() + missing.len();
        let payload = json!({
            "dev_eui": dev_eui,
            "config": config,
            "page_count": page_count,
            "synced_at": Self::timestamp(),
            "last_seq": last_seq,
            "read_status": read_status,
            "missing": missing,
            "chunks_done": config.len(),
            "chunks_total": asked,
        });

        let topic = self.topics.lorawan_sensor_full_config(&dev_eui);
        self.publish(topic, payload.to_string(), QoS::AtLeastOnce, false)
            .await
    }

    /// Publish a NODE's fPort-85 device info to
    /// `lorawan/sensors/<dev_eui>/info` (#65), **retained**.
    ///
    /// Retained because this is device identity plus last-known health, and a
    /// node only reports every `interval_report` (900 s by default): a viewer
    /// that reconnects has to be able to render a firmware version and health
    /// flags immediately rather than showing blanks until someone re-queries.
    ///
    /// `info` arrives already projected by `node_config::info_to_json`, which
    /// reduces `claim_token` to `has_claim_token`. That redaction matters here
    /// specifically *because* the message is retained: the broker replays it to
    /// every future subscriber, so a secret published once would leak indefinitely.
    async fn publish_node_info(
        &self,
        dev_eui: String,
        info: serde_json::Value,
    ) -> Result<(), String> {
        let topic = self.topics.lorawan_sensor_info(&dev_eui);
        self.publish(topic, info.to_string(), QoS::AtLeastOnce, true)
            .await
    }

    /// Clear the retained device-info for a decommissioned node. Without this a
    /// removed device's info would be replayed to new subscribers forever.
    async fn clear_node_info(&self, dev_eui: &str) -> Result<(), String> {
        let topic = self.topics.lorawan_sensor_info(dev_eui);
        self.publish(topic, String::new(), QoS::AtLeastOnce, true)
            .await
    }

    /// Publish the outcome of a NODE control command (#71) to
    /// `lorawan/sensors/<dev_eui>/command`.
    ///
    /// `expect` is what makes this payload honest. Three of the five commands
    /// cannot report success at the moment they are acknowledged: `force_send`
    /// sends no fPort-85 reply at all, an empty-body `clock_sync` answers with a
    /// deferred `Info`, and `reboot`/`device_reset` restart 8 s later. Carrying the
    /// outstanding expectation lets the viewer render "requested" rather than
    /// claiming a success it cannot know about yet.
    #[allow(clippy::too_many_arguments)]
    async fn publish_node_command_result(
        &self,
        dev_eui: String,
        command: String,
        seq: u32,
        result: String,
        expect: Option<String>,
        detail: Option<String>,
        fault_key: Option<String>,
    ) -> Result<(), String> {
        let payload = json!({
            "dev_eui": dev_eui,
            "command": command,
            "seq": seq,
            "result": result,
            "expect": expect,
            "detail": detail,
            "fault_key": fault_key,
            "ts": Self::timestamp(),
        });
        let topic = self.topics.lorawan_sensor_command(&dev_eui);
        self.publish(topic, payload.to_string(), QoS::AtLeastOnce, false)
            .await
    }

    /// Publish one page of a NODE's on-device history to
    /// `lorawan/sensors/<dev_eui>/history` (Feature D).
    async fn publish_node_history(
        &self,
        dev_eui: String,
        frame_index: u32,
        frame_count: u32,
        records: Vec<serde_json::Value>,
    ) -> Result<(), String> {
        let payload = json!({
            "dev_eui": dev_eui,
            "frame_index": frame_index,
            "frame_count": frame_count,
            "records": records,
        });

        let topic = self.topics.lorawan_sensor_history(&dev_eui);
        self.publish(topic, payload.to_string(), QoS::AtLeastOnce, false)
            .await
    }

    /// Publish the result of a detect_eye_tag probe on `eye/detect`.
    /// One Beacon storage window (`plan/control/storage_intervals.md` §5).
    /// QoS 1 and never retained, like `sensors/aggregated`: a window is a
    /// measurement, and a replayed copy is deduped by its `message_id`.
    async fn publish_beacon_aggregated(&self, payload: serde_json::Value) -> Result<(), String> {
        let topic = self.topics.beacon_aggregated();
        self.publish(topic, payload.to_string(), QoS::AtLeastOnce, false)
            .await
    }

    async fn publish_beacon_detect_result(
        &self,
        mac: &str,
        is_en12830: Option<bool>,
        status: &str,
    ) -> Result<(), String> {
        let payload = json!({
            "timestamp": Self::timestamp(),
            "mac": mac,
            "is_en12830": is_en12830,
            "status": status,
        });

        let topic = self.topics.beacon_detect();
        let qos = Self::qos_from_u8(self.qos_overrides.sensor_readings);

        self.publish(topic, payload.to_string(), qos, false).await
    }

    /// Publish one archive-download message on `eye/history`. QoS 1 and not retained: the topic is a
    /// stream of a job's progress and records, and a retained copy would replay a finished job to
    /// every new subscriber (beacon_history.md §2.2).
    async fn publish_beacon_history(&self, payload: &serde_json::Value) -> Result<(), String> {
        let topic = self.topics.beacon_history();
        self.publish(topic, payload.to_string(), QoS::AtLeastOnce, false)
            .await
    }

    /// Publish pairing response (success)
    pub async fn publish_pairing_response(
        &self,
        response: &crate::libs::pairing::messages::PairingResponse,
    ) -> Result<(), String> {
        let payload = serde_json::to_string(&response)
            .map_err(|e| format!("Failed to serialize pairing response: {}", e))?;

        let topic = self.topics.pair_response();
        let qos = QoS::ExactlyOnce; // QoS 2 for pairing (critical)

        self.publish(topic, payload, qos, false).await
    }

    /// Ask the co-located portal to pair with this device (GitLab #61).
    ///
    /// **Never retained**, and that is the whole safety property: a retained invite would re-pair
    /// the portal on every reconnect, long after the operator walked away and the front-panel
    /// window closed. QoS 1 rather than the pairing reply's QoS 2 — a duplicate invite is harmless
    /// (the window is single-use and the second attempt simply finds it spent), while a lost one
    /// is an operator standing at a Hub wondering why nothing happened.
    pub async fn publish_pairing_invite(&self, code: &str, origin: &str) -> Result<(), String> {
        let payload = serde_json::json!({ "code": code, "origin": origin }).to_string();
        self.publish(self.topics.pair_invite(), payload, QoS::AtLeastOnce, false)
            .await
    }

    /// Publish pairing error
    pub async fn publish_pairing_error(
        &self,
        error: &crate::libs::pairing::messages::PairingError,
    ) -> Result<(), String> {
        let payload = serde_json::to_string(&error)
            .map_err(|e| format!("Failed to serialize pairing error: {}", e))?;

        let topic = self.topics.pair_response();
        let qos = QoS::ExactlyOnce; // QoS 2 for pairing (critical)

        self.publish(topic, payload, qos, false).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timestamp_format() {
        let ts = MqttPublisher::timestamp();
        // Should be ISO 8601 format (e.g., "2025-12-11T14:35:22Z")
        assert!(ts.contains('T'));
        assert!(ts.len() > 10);
    }

    #[test]
    fn test_qos_conversion() {
        assert!(matches!(MqttPublisher::qos_from_u8(0), QoS::AtMostOnce));
        assert!(matches!(MqttPublisher::qos_from_u8(1), QoS::AtLeastOnce));
        assert!(matches!(MqttPublisher::qos_from_u8(2), QoS::ExactlyOnce));
    }

    fn sensor_with_backlog(
        dev_eui: &str,
        event_count: usize,
        gateway_count: usize,
    ) -> super::super::messages::LoRaWANSensorPayload {
        use crate::libs::lorawan::chirpstack::{GatewayRx, NodeEvent};

        let events = (0..event_count)
            .map(|i| NodeEvent {
                event_type: format!("event-{}", i),
                ts: format!("2026-08-14T02:{:02}:00Z", i % 60),
                // Realistic-sized nested payload, matching what a real alarm/
                // status event embeds via #[serde(flatten)] in production.
                extra: serde_json::json!({"detail": "x".repeat(200), "seq": i}),
            })
            .collect();

        let gateways = (0..gateway_count)
            .map(|i| GatewayRx {
                gateway_id: format!("gw-{:016}", i),
                rssi: Some(-40 - i as i32),
                snr: Some(9.5 - i as f32),
            })
            .collect();

        super::super::messages::LoRaWANSensorPayload {
            dev_eui: dev_eui.to_string(),
            name: "Test Node".to_string(),
            serial_number: Some("123456".to_string()),
            location: None,
            fields: std::collections::HashMap::from([("temperature".to_string(), 22.5)]),
            field_alarm_states: std::collections::HashMap::new(),
            field_thresholds: vec![],
            counters: std::collections::HashMap::new(),
            events,
            gateways,
            dr: Some(5),
            downlink_gateway_id: None,
            rssi: Some(-42),
            snr: Some(9.0),
            last_seen: Some("2026-08-14T02:18:39Z".to_string()),
            alarm_state: "OK".to_string(),
        }
    }

    #[test]
    fn lorawan_sensors_payload_stays_under_budget_with_a_full_event_backlog() {
        // 32 events matches MAX_RECENT_EVENTS; 10 gateways is a generous
        // real-world upper bound. This single sensor alone reproduced the
        // live ~14KB failure before the fix.
        let sensors = vec![sensor_with_backlog("587607079406ab88", 32, 10)];
        let payload = build_lorawan_sensors_payload(&sensors, "2026-08-14T02:20:00Z");
        assert!(
            payload.len() <= LORAWAN_SENSORS_PAYLOAD_BUDGET_BYTES,
            "payload was {} bytes, budget is {}",
            payload.len(),
            LORAWAN_SENSORS_PAYLOAD_BUDGET_BYTES
        );
    }

    #[test]
    fn lorawan_sensors_payload_caps_events_to_the_configured_limit() {
        let sensors = vec![sensor_with_backlog("587607079406ab88", 32, 1)];
        let payload = build_lorawan_sensors_payload(&sensors, "2026-08-14T02:20:00Z");
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let events = parsed["sensors"][0]["events"].as_array().unwrap();
        assert_eq!(events.len(), LORAWAN_SENSORS_EVENTS_CAP);
    }

    #[test]
    fn lorawan_sensors_payload_keeps_the_most_recent_events_when_capping() {
        let sensors = vec![sensor_with_backlog("587607079406ab88", 32, 1)];
        let payload = build_lorawan_sensors_payload(&sensors, "2026-08-14T02:20:00Z");
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let events = parsed["sensors"][0]["events"].as_array().unwrap();
        // Original events are named "event-0".."event-31"; capping to the
        // last N must keep "event-31" (most recent), not "event-0".
        let last = events.last().unwrap()["type"].as_str().unwrap();
        assert_eq!(last, format!("event-{}", 31));
    }

    #[test]
    fn lorawan_sensors_payload_keeps_essential_fields_even_when_capped() {
        let sensors = vec![sensor_with_backlog("587607079406ab88", 32, 10)];
        let payload = build_lorawan_sensors_payload(&sensors, "2026-08-14T02:20:00Z");
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let sensor = &parsed["sensors"][0];
        assert_eq!(sensor["dev_eui"], "587607079406ab88");
        assert_eq!(sensor["alarm_state"], "OK");
        assert_eq!(sensor["fields"]["temperature"], 22.5);
    }

    #[test]
    fn lorawan_sensors_payload_degrades_gracefully_with_many_sensors() {
        // 50 sensors each carrying a full backlog: even after per-sensor
        // capping this would exceed the budget, so the ladder must reach
        // its minimal-fields fallback, and the essential fields must still
        // all be present, and the payload must actually fit.
        let sensors: Vec<_> = (0..50)
            .map(|i| sensor_with_backlog(&format!("dev-eui-{:016}", i), 32, 10))
            .collect();
        let payload = build_lorawan_sensors_payload(&sensors, "2026-08-14T02:20:00Z");
        assert!(
            payload.len() <= LORAWAN_SENSORS_HARD_CEILING_BYTES,
            "payload was {} bytes, hard ceiling is {}",
            payload.len(),
            LORAWAN_SENSORS_HARD_CEILING_BYTES
        );
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["sensors"].as_array().unwrap().len(), 50);
        for sensor in parsed["sensors"].as_array().unwrap() {
            assert!(sensor["dev_eui"].as_str().unwrap().starts_with("dev-eui-"));
            assert_eq!(sensor["alarm_state"], "OK");
        }
    }

    /// Builds a sensor whose events are realistically-sized alarm reports
    /// (matching `node_payload.rs`'s ~186-byte serialized `NodeEvent` for a
    /// real fPort-3 AlarmReport), reproducing the exact live payload shape
    /// the final whole-branch review measured tripping the old two-tier
    /// fallback at 4 paired Nodes.
    fn sensor_with_realistic_alarm_backlog(
        dev_eui: &str,
    ) -> super::super::messages::LoRaWANSensorPayload {
        use crate::libs::lorawan::chirpstack::{GatewayRx, NodeEvent};

        let events = (0..8)
            .map(|i| NodeEvent {
                event_type: "alarm".to_string(),
                ts: format!("2026-08-14T02:{:02}:00Z", i % 60),
                extra: serde_json::json!({
                    "field": "temperature", "state": "CRITICAL_HIGH",
                    "value": 41.2, "threshold": 41.0, "line": 1,
                }),
            })
            .collect();
        let gateways = (0..5)
            .map(|i| GatewayRx {
                gateway_id: format!("gw-{:016}", i),
                rssi: Some(-40 - i as i32),
                snr: Some(9.5 - i as f32),
            })
            .collect();

        super::super::messages::LoRaWANSensorPayload {
            dev_eui: dev_eui.to_string(),
            name: "Test Node".to_string(),
            serial_number: Some("123456".to_string()),
            location: None,
            fields: std::collections::HashMap::from([
                ("temperature".to_string(), 41.2),
                ("humidity".to_string(), 55.0),
                ("battery".to_string(), 3.6),
            ]),
            field_alarm_states: std::collections::HashMap::from([(
                "temperature".to_string(),
                "CRITICAL_HIGH".to_string(),
            )]),
            field_thresholds: vec![],
            counters: std::collections::HashMap::new(),
            events,
            gateways,
            dr: Some(5),
            downlink_gateway_id: None,
            rssi: Some(-42),
            snr: Some(9.0),
            last_seen: Some("2026-08-14T02:18:39Z".to_string()),
            alarm_state: "CRITICAL".to_string(),
        }
    }

    #[test]
    fn lorawan_sensors_payload_keeps_alarm_events_for_a_realistic_multi_node_hub() {
        // 4 paired Nodes with realistic alarm-event sizes reproduces the
        // exact scenario the final review found tripping the old two-tier
        // fallback into events_cap=0 forever. The ladder must instead
        // degrade gateways first and still deliver at least one alarm event
        // per sensor.
        let sensors: Vec<_> = (0..4)
            .map(|i| sensor_with_realistic_alarm_backlog(&format!("dev-eui-{:016}", i)))
            .collect();
        let payload = build_lorawan_sensors_payload(&sensors, "2026-08-14T02:20:00Z");
        assert!(
            payload.len() <= LORAWAN_SENSORS_PAYLOAD_BUDGET_BYTES,
            "payload was {} bytes, budget is {}",
            payload.len(),
            LORAWAN_SENSORS_PAYLOAD_BUDGET_BYTES
        );
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        for sensor in parsed["sensors"].as_array().unwrap() {
            let events = sensor["events"].as_array().unwrap();
            assert!(
                !events.is_empty(),
                "expected at least one alarm event to survive capping for {:?}, got none",
                sensor["dev_eui"]
            );
        }
    }

    #[test]
    fn select_events_prefers_alarm_events_over_routine_ones_when_capping() {
        use crate::libs::lorawan::chirpstack::NodeEvent;

        // Oldest-first, as the real backlog is stored: 2 routine events,
        // then 2 alarm events, then 2 more routine events.
        let events: Vec<NodeEvent> = vec![
            ("boot", 0),
            ("orientation", 1),
            ("alarm", 2),
            ("alarm", 3),
            ("hall_active", 4),
            ("boot", 5),
        ]
        .into_iter()
        .map(|(t, i)| NodeEvent {
            event_type: t.to_string(),
            ts: format!("2026-08-14T02:{:02}:00Z", i),
            extra: serde_json::json!({"seq": i}),
        })
        .collect();

        // Cap of 2: without alarm-priority this would keep the 2 most
        // recent by position ("hall_active", "boot") and drop both alarms.
        let selected = select_events(&events, 2);
        assert_eq!(selected.len(), 2);
        assert!(
            selected.iter().all(|e| e.event_type == "alarm"),
            "expected both alarm events to be preferentially retained, got {:?}",
            selected.iter().map(|e| &e.event_type).collect::<Vec<_>>()
        );
    }

    /// Builds one realistically-sized BLE beacon tag. `i` drives the MAC,
    /// name and `last_seen_ts` so ordering is testable; `discovered`/`stale`
    /// place the tag into one of `build_eye_sensors_payload`'s three classes.
    fn beacon(i: usize, discovered: bool, stale: bool) -> super::super::messages::BeaconTagPayload {
        super::super::messages::BeaconTagPayload {
            gateway: "fiber-0123456789ab".to_string(),
            mac: format!(
                "AA:BB:CC:{:02X}:{:02X}:{:02X}",
                i / 65536,
                (i / 256) % 256,
                i % 256
            ),
            name: Some(format!("Beacon {:02}", i)),
            // Halves are exact in f32, so the serialized length does not
            // wobble with float-repr noise across fixture sizes.
            temperature_c: Some(20.0 + (i % 10) as f32 * 0.5),
            humidity_pct: Some(40 + (i % 20) as u8),
            battery_mv: Some(3000 - (i % 500) as u16),
            low_battery: false,
            magnet_present: true,
            magnet_detected: false,
            moving: Some(false),
            movement_count: Some(i as u16),
            pitch_deg: Some(2),
            roll_deg: Some(-3),
            rssi: Some(-60 - (i % 30) as i16),
            last_seen_ts: Some(1_736_775_000 + i as i64),
            stale,
            provisioning: "provisioned".to_string(),
            discovered,
            is_en12830: Some(true),
            field_alarm_states: std::collections::HashMap::from([
                ("temperature".to_string(), "NORMAL".to_string()),
                ("humidity".to_string(), "NORMAL".to_string()),
            ]),
            alarm_state: "NORMAL".to_string(),
            ble_config: None,
        }
    }

    #[test]
    fn eye_sensors_payload_keeps_full_detail_for_a_small_fleet() {
        // The common case: a handful of enrolled tags must lose nothing.
        let tags: Vec<_> = (0..10).map(|i| beacon(i, false, false)).collect();
        let payload = build_eye_sensors_payload(&tags, "2026-08-28T10:00:00Z", false);
        assert!(
            payload.contains("\"pitch_deg\""),
            "small fleet should not have been trimmed: {} bytes",
            payload.len()
        );
        assert!(
            payload.len() <= EYE_SENSORS_PAYLOAD_BUDGET_BYTES,
            "payload was {} bytes, budget is {}",
            payload.len(),
            EYE_SENSORS_PAYLOAD_BUDGET_BYTES
        );
    }

    #[test]
    fn a_verified_configuration_rides_the_full_rung_and_the_trimmed_rungs_leave_it_out() {
        let mut t = beacon(1, false, false);
        assert!(
            beacon_tag_json(&t)["ble_config"].is_null(),
            "unverified is present-as-null on the full rung — the one rung that can clear it"
        );
        t.ble_config = Some(control_ble::host::BleConfigPayload {
            advertising_interval_ms: 10_000,
            tx_power_dbm: 8,
            protocol_type: 2,
            protocol: "eye_sensor".to_string(),
            sensors: Some(
                [
                    ("temperature", true),
                    ("humidity", true),
                    ("magnet", false),
                    ("movement", false),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            ),
            read_at: 1_790_106_435,
            verified: false,
            recording: Some(true),
            logging_interval_min: Some(15),
        });
        let full = beacon_tag_json(&t);
        assert_eq!(full["ble_config"]["advertising_interval_ms"], 10_000);
        // The recorder read-back rides the same object (beacon_history.md §3).
        assert_eq!(full["ble_config"]["recording"], true);
        assert_eq!(full["ble_config"]["logging_interval_min"], 15);
        assert_eq!(full["ble_config"]["protocol"], "eye_sensor");
        assert_eq!(
            full["ble_config"]["sensors"],
            json!({"temperature": true, "humidity": true, "magnet": false, "movement": false})
        );
        for rung in [beacon_tag_json_trimmed(&t), beacon_tag_json_minimal(&t)] {
            assert!(
                rung.get("ble_config").is_none(),
                "an absent key keeps the portal's last-known configuration: {rung}"
            );
        }
    }

    #[test]
    fn eye_sensors_payload_trims_detail_before_dropping_any_tag() {
        // Sized to overflow the budget at full detail but fit once the
        // diagnostic fields are gone — the second rung of the ladder.
        let tags: Vec<_> = (0..40).map(|i| beacon(i, false, false)).collect();
        let payload = build_eye_sensors_payload(&tags, "2026-08-28T10:00:00Z", false);
        assert!(
            payload.len() <= EYE_SENSORS_PAYLOAD_BUDGET_BYTES,
            "payload was {} bytes, budget is {}",
            payload.len(),
            EYE_SENSORS_PAYLOAD_BUDGET_BYTES
        );
        assert!(
            !payload.contains("\"pitch_deg\""),
            "expected pitch_deg trimmed"
        );
        // rssi survives the trim: a Server computes home Hubs from it (decision #42).
        assert!(payload.contains("\"rssi\""), "expected rssi kept");
        for t in &tags {
            assert!(
                payload.contains(&t.mac),
                "tag {} was dropped, not trimmed",
                t.mac
            );
        }
    }

    #[test]
    fn eye_sensors_payload_never_drops_enrolled_or_stale_discovered_tags() {
        // Deep-rung pressure: a modest enrolled fleet plus a few gone-quiet
        // strangers, buried under a flood of live ones.
        let mut tags: Vec<_> = (0..20).map(|i| beacon(i, false, false)).collect();
        tags.extend((100..110).map(|i| beacon(i, true, true)));
        tags.extend((1000..1200).map(|i| beacon(i, true, false)));

        let payload = build_eye_sensors_payload(&tags, "2026-08-28T10:00:00Z", false);
        for t in tags.iter().filter(|t| !t.discovered || t.stale) {
            assert!(
                payload.contains(&t.mac),
                "undroppable tag {} (discovered={}, stale={}) missing from payload",
                t.mac,
                t.discovered,
                t.stale
            );
        }
    }

    #[test]
    fn eye_sensors_payload_caps_live_strangers_keeping_the_newest() {
        // 100 live strangers cannot fit even trimmed, so the ladder must cap
        // them — and the survivors must be the freshest ones, since those are
        // the plausible adoption candidates.
        let tags: Vec<_> = (0..100).map(|i| beacon(i, true, false)).collect();
        let payload = build_eye_sensors_payload(&tags, "2026-08-28T10:00:00Z", false);
        assert!(
            payload.len() <= EYE_SENSORS_PAYLOAD_BUDGET_BYTES,
            "payload was {} bytes, budget is {}",
            payload.len(),
            EYE_SENSORS_PAYLOAD_BUDGET_BYTES
        );

        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let kept = parsed["tags"].as_array().unwrap();
        assert!(!kept.is_empty() && kept.len() < tags.len());
        let mut kept_ts: Vec<i64> = kept
            .iter()
            .map(|t| t["last_seen_ts"].as_i64().unwrap())
            .collect();
        kept_ts.sort_unstable();
        // `last_seen_ts` is 1_736_775_000 + i, so the newest `kept.len()` are
        // exactly the tail of the range.
        let expected: Vec<i64> = ((tags.len() - kept.len())..tags.len())
            .map(|i| 1_736_775_000 + i as i64)
            .collect();
        assert_eq!(kept_ts, expected, "survivors were not the newest strangers");
    }

    #[test]
    fn eye_sensors_minimal_fallback_keeps_the_verbatim_assigned_fields() {
        // Too many enrolled tags for any rung, so the minimal last resort
        // runs. The portal's store assigns `stale`, `provisioning`,
        // `discovered` and `field_alarm_states` verbatim (no merge), so those
        // — plus `mac` and `alarm_state` — must survive even here.
        let tags: Vec<_> = (0..200).map(|i| beacon(i, false, false)).collect();
        let payload = build_eye_sensors_payload(&tags, "2026-08-28T10:00:00Z", false);
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let kept = parsed["tags"].as_array().unwrap();
        assert_eq!(
            kept.len(),
            tags.len(),
            "enrolled tags must never be dropped"
        );
        for tag in kept {
            for key in [
                "mac",
                "stale",
                "provisioning",
                "discovered",
                "field_alarm_states",
                "alarm_state",
            ] {
                assert!(
                    tag.get(key).is_some(),
                    "minimal fallback dropped {:?} from {:?}",
                    key,
                    tag["mac"]
                );
            }
        }
        // Diagnostics are gone at this depth, by design.
        assert!(!payload.contains("\"pitch_deg\""));
        assert!(!payload.contains("\"battery_mv\""));
    }

    #[test]
    fn eye_sensors_payload_degrades_gracefully_with_a_pathological_fleet() {
        // 120 enrolled tags is far past what one Hub sees in practice. The
        // tag *list* must never shrink — the portal has no other way to learn
        // about a tag — so if `mac`+safety-fields alone legitimately exceed
        // the wire ceiling we publish anyway and log, rather than dropping.
        let tags: Vec<_> = (0..120).map(|i| beacon(i, false, false)).collect();
        let payload = build_eye_sensors_payload(&tags, "2026-08-28T10:00:00Z", false);
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["tags"].as_array().unwrap().len(), 120);
        for t in &tags {
            assert!(
                payload.contains(&t.mac),
                "enrolled tag {} was dropped",
                t.mac
            );
        }
        // At ~283 bytes per minimal tag, 120 of them legitimately need more
        // than the wire ceiling — the fallback logs and publishes anyway
        // rather than dropping a tag, which is the documented behaviour. What
        // must hold is that nothing was silently withheld: if it *does* fit,
        // it fits with detail already stripped.
        if payload.len() <= EYE_SENSORS_HARD_CEILING_BYTES {
            assert!(!payload.contains("\"pitch_deg\""));
        }
    }

    /// B6 (devices.backend.md §22.1): the flag is on **every** rung, not
    /// only the ones that dropped somebody. `EyeSensorsMsg` defaults a
    /// missing key to `false`, so an omitted flag and "nothing was dropped"
    /// would be indistinguishable — and reconcile-by-absence prunes on
    /// membership, so the portal has to be able to tell them apart.
    #[test]
    fn every_eye_sensors_rung_carries_discovered_capped() {
        // One fixture per rung, smallest first: full detail, trimmed,
        // trimmed+capped, and the minimal fallback past the ladder's bottom.
        let rungs: Vec<Vec<_>> = vec![
            (0..10).map(|i| beacon(i, false, false)).collect(),
            (0..40).map(|i| beacon(i, false, false)).collect(),
            (0..80).map(|i| beacon(i, i >= 10, false)).collect(),
            (0..200).map(|i| beacon(i, false, false)).collect(),
        ];
        for (n, tags) in rungs.iter().enumerate() {
            for host_capped in [false, true] {
                let payload = build_eye_sensors_payload(tags, "2026-08-28T10:00:00Z", host_capped);
                let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
                assert!(
                    parsed["discovered_capped"].is_boolean(),
                    "rung {n} published no discovered_capped: {payload}"
                );
                if host_capped {
                    assert_eq!(
                        parsed["discovered_capped"],
                        serde_json::Value::Bool(true),
                        "rung {n} lost the host's own cap"
                    );
                }
            }
        }

        // `tags: []` is the empty snapshot — the portal's instruction to
        // prune every stranger it holds — and is never capped, whatever the
        // host said.
        let payload = build_eye_sensors_payload(&[], "2026-08-28T10:00:00Z", true);
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["discovered_capped"], serde_json::Value::Bool(false));
        assert!(parsed["tags"].as_array().unwrap().is_empty());
    }

    /// The other half of the OR: a rung that sheds live strangers to fit the
    /// budget has capped the snapshot itself, whatever the host reported.
    /// Without this the portal would prune the dropped strangers as absent
    /// and empty its own scan table on one oversized publish.
    #[test]
    fn a_rung_that_drops_strangers_reports_capped() {
        // Ten enrolled plus seventy live strangers: far past the ladder's
        // `Some(16)` rung, so strangers are certainly dropped.
        let tags: Vec<_> = (0..80).map(|i| beacon(i, i >= 10, false)).collect();
        let payload = build_eye_sensors_payload(&tags, "2026-08-28T10:00:00Z", false);
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let kept = parsed["tags"].as_array().unwrap().len();
        assert!(
            kept < tags.len(),
            "the fixture did not reach a capping rung"
        );
        assert_eq!(
            parsed["discovered_capped"],
            serde_json::Value::Bool(true),
            "a rung that dropped {} strangers reported no cap",
            tags.len() - kept
        );

        // ...and a snapshot that fits loses nobody and says so.
        let small: Vec<_> = (0..6).map(|i| beacon(i, i >= 2, false)).collect();
        let payload = build_eye_sensors_payload(&small, "2026-08-28T10:00:00Z", false);
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["tags"].as_array().unwrap().len(), small.len());
        assert_eq!(parsed["discovered_capped"], serde_json::Value::Bool(false));
    }

    /// One wired-probe config entry, as `publish_config_state` receives it.
    fn sensor_config(line: u8) -> super::super::messages::SensorConfigData {
        super::super::messages::SensorConfigData {
            line,
            name: format!("Probe {}", line),
            location: Some(format!("Shelf {}", line)),
            enabled: true,
            has_override: false,
            thresholds: crate::libs::alarms::AlarmThreshold::default_medical(),
        }
    }

    /// One paired Node's config entry. `threshold_fields` drives
    /// `field_thresholds`, the document's only unbounded nested grower; the
    /// bounds are non-round so every one of them serializes at full width.
    fn lorawan_sensor_config(
        i: usize,
        threshold_fields: usize,
    ) -> super::super::messages::LoRaWANSensorConfigData {
        let field_thresholds = (0..threshold_fields)
            .map(|n| crate::libs::config::FieldThreshold {
                field: format!("temperature_{}", n),
                critical_low: Some(-20.5),
                warning_low: Some(-10.5),
                warning_high: Some(30.5),
                critical_high: Some(40.5),
            })
            .collect();
        super::super::messages::LoRaWANSensorConfigData {
            dev_eui: format!("70b3d57ed{:07x}", i),
            name: Some(format!("Node {:03}", i)),
            serial_number: Some(format!("SN-{:06}", i)),
            location: Some(format!("Room {:03}", i)),
            enabled: true,
            field_thresholds,
        }
    }

    /// Calls the builder with fixed scalars, so a test varies only what
    /// actually drives the document's size: the two entity lists.
    fn config_state_payload(
        sensors: &[super::super::messages::SensorConfigData],
        lorawan_sensors: &[super::super::messages::LoRaWANSensorConfigData],
        eye_tags: Option<&[super::super::messages::EyeTagConfigData]>,
    ) -> String {
        build_config_state_payload(
            "2026-08-28T10:00:00Z",
            50,
            80,
            30,
            40,
            60,
            "Freezer 01",
            sensors,
            lorawan_sensors,
            10_000,
            60_000,
            60_000,
            true,
            false,
            true,
            eye_tags,
            &[],
        )
    }

    /// Same fixed scalars and an empty fleet, varying only the LCD rows.
    fn config_state_payload_with_display_lines(
        display_lines: &[crate::libs::config::DisplayLine],
    ) -> String {
        build_config_state_payload(
            "2026-08-28T10:00:00Z",
            50,
            80,
            30,
            40,
            60,
            "Freezer 01",
            &[],
            &[],
            10_000,
            60_000,
            60_000,
            true,
            false,
            true,
            None,
            display_lines,
        )
    }

    /// One enrolled Beacon, as `eye.tags` holds it.
    fn eye_tag_config(i: usize) -> super::super::messages::EyeTagConfigData {
        super::super::messages::EyeTagConfigData {
            mac: format!("EE:E0:00:00:00:{:02X}", i),
            name: Some(format!("Beacon {:02}", i)),
        }
    }

    #[test]
    fn config_state_payload_publishes_a_small_document_unchanged() {
        // The common case: 8 wired probes (the hardware maximum) and 8 paired
        // Nodes must lose nothing at all, thresholds included.
        let sensors: Vec<_> = (0..8u8).map(sensor_config).collect();
        let lorawan: Vec<_> = (0..8).map(|i| lorawan_sensor_config(i, 4)).collect();
        let payload = config_state_payload(&sensors, &lorawan, None);
        assert!(
            payload.len() <= CONFIG_STATE_WARN_BYTES,
            "payload was {} bytes, warn threshold is {}",
            payload.len(),
            CONFIG_STATE_WARN_BYTES
        );
        assert!(
            payload.contains("\"field_thresholds\""),
            "small document should not have been trimmed: {} bytes",
            payload.len()
        );
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["lorawan_sensors"].as_array().unwrap().len(), 8);
        assert_eq!(
            parsed["lorawan_sensors"][0]["field_thresholds"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        // The rest of the document is untouched by the guard.
        assert_eq!(parsed["device_label"], "Freezer 01");
        assert_eq!(parsed["sensors"].as_array().unwrap().len(), 8);
        assert_eq!(parsed["eye"]["enabled"], true);
    }

    #[test]
    fn config_state_payload_drops_field_thresholds_when_over_the_ceiling() {
        // 100 paired Nodes each carrying 4 field thresholds is ~47KB, far past
        // the 20KB wire ceiling — and retained at QoS 1, so it would replay on
        // every reconnect. The entry *list* may never shrink (this topic is the
        // authoritative Node inventory: absence means removed), so the single
        // fallback step is dropping `field_thresholds`.
        let sensors: Vec<_> = (0..8u8).map(sensor_config).collect();
        let lorawan: Vec<_> = (0..100).map(|i| lorawan_sensor_config(i, 4)).collect();
        let payload = config_state_payload(&sensors, &lorawan, None);
        assert!(
            !payload.contains("\"field_thresholds\""),
            "field_thresholds survived the fallback: {} bytes",
            payload.len()
        );
        // Every Node is still listed, with its identifying fields — omitting
        // `field_thresholds` is parse-safe on the portal side, where the field
        // is `Option<Vec<FieldThreshold>>` and every other `ConfigStateMsg`
        // field is `Option`/`#[serde(default)]`
        // (`fiber-protocol/src/payloads/config_state.rs`).
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let entries = parsed["lorawan_sensors"].as_array().unwrap();
        assert_eq!(entries.len(), 100);
        for (entry, expected) in entries.iter().zip(&lorawan) {
            assert_eq!(entry["dev_eui"], expected.dev_eui);
            assert!(entry.get("field_thresholds").is_none());
            assert_eq!(entry["enabled"], true);
        }
        // With the only unbounded grower gone, this fleet fits; if a fleet ever
        // did not, the guard logs and publishes anyway rather than withholding
        // the inventory (see `build_config_state_payload`).
        assert!(
            payload.len() <= CONFIG_STATE_HARD_CEILING_BYTES,
            "payload was {} bytes after dropping field_thresholds, hard ceiling is {}",
            payload.len(),
            CONFIG_STATE_HARD_CEILING_BYTES
        );
    }

    /// devices.backend.md §22.2. The portal prunes an enrolled Beacon that is
    /// missing from this key, so the three states have to be distinguishable
    /// on the wire: a list, an authoritative empty list, and no statement.
    #[test]
    fn config_state_payload_carries_the_enrolled_eye_tags() {
        let sensors: Vec<_> = (0..8u8).map(sensor_config).collect();
        let lorawan: Vec<_> = (0..2).map(|i| lorawan_sensor_config(i, 2)).collect();
        let tags: Vec<_> = (1..=3).map(eye_tag_config).collect();

        let payload = config_state_payload(&sensors, &lorawan, Some(&tags));
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let entries = parsed["eye_tags"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["mac"], "EE:E0:00:00:00:01");
        assert_eq!(entries[0]["name"], "Beacon 01");
        // `location` is not in the Hub's own `eye.tags` schema
        // (`control_ble::config::EyeTag`), so the key is omitted rather than
        // published as an invented null.
        assert!(entries[0].get("location").is_none());

        // A Hub with nothing enrolled says so — `[]` is the instruction to
        // prune every registered Beacon, and it has to be reachable.
        let payload = config_state_payload(&sensors, &lorawan, Some(&[]));
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(
            parsed["eye_tags"],
            serde_json::json!([]),
            "an empty enrolled set must publish as [], not as an absent key"
        );

        // And a Hub that cannot read its own `eye:` section omits the key,
        // which the portal reads as "no statement" rather than "prune all".
        let payload = config_state_payload(&sensors, &lorawan, None);
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert!(parsed.get("eye_tags").is_none());
    }

    /// The size fallback drops `field_thresholds` and nothing else — the two
    /// inventories in this document are both authoritative, so neither list
    /// may shrink to save bytes.
    #[test]
    fn the_size_fallback_never_drops_an_enrolled_eye_tag() {
        let sensors: Vec<_> = (0..8u8).map(sensor_config).collect();
        let lorawan: Vec<_> = (0..100).map(|i| lorawan_sensor_config(i, 4)).collect();
        let tags: Vec<_> = (1..=4).map(eye_tag_config).collect();
        let payload = config_state_payload(&sensors, &lorawan, Some(&tags));
        assert!(
            !payload.contains("\"field_thresholds\""),
            "the fixture did not reach the fallback rung"
        );
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["eye_tags"].as_array().unwrap().len(), 4);
        assert_eq!(parsed["display_lines"], serde_json::json!([]));
    }

    /// GitLab #74: the portal's display picker pre-checks from
    /// `config/state.display_lines`, so the key has to be there and has to
    /// parse as the portal's own `fiber_protocol::DisplayLine`.
    #[test]
    fn config_state_payload_carries_the_display_lines_in_the_portal_shape() {
        use crate::libs::config::{DisplayLine, DisplayLineFormat, DisplayLineSource};
        let line =
            |source, line, dev_eui: Option<&str>, mac: Option<&str>, field: &str| DisplayLine {
                source,
                line,
                dev_eui: dev_eui.map(str::to_string),
                mac: mac.map(str::to_string),
                field: field.to_string(),
                label: None,
                format: DisplayLineFormat::default(),
            };
        let lines = vec![
            line(
                DisplayLineSource::Ds18b20,
                Some(3),
                None,
                None,
                "temperature",
            ),
            line(
                DisplayLineSource::Node,
                None,
                Some("70b3d57ed0000001"),
                None,
                "humidity",
            ),
            line(
                DisplayLineSource::Ble,
                None,
                None,
                Some("EE:E0:00:00:00:01"),
                "temperature",
            ),
        ];

        let payload = config_state_payload_with_display_lines(&lines);
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let sources: Vec<_> = parsed["display_lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["source"].as_str().unwrap())
            .collect();
        assert_eq!(sources, ["ds18b20", "sticker", "ble"]);

        let msg: fiber_protocol::ConfigStateMsg = serde_json::from_str(&payload).unwrap();
        let got = msg.display_lines.expect("display_lines must be present");
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].source, fiber_protocol::DisplayLineSource::Ds18b20);
        assert_eq!(got[0].line, Some(3));
        assert_eq!(got[1].source, fiber_protocol::DisplayLineSource::Node);
        assert_eq!(got[1].dev_eui.as_deref(), Some("70b3d57ed0000001"));
        assert_eq!(got[1].field, "humidity");
        assert_eq!(got[2].source, fiber_protocol::DisplayLineSource::Ble);
        assert_eq!(got[2].mac.as_deref(), Some("EE:E0:00:00:00:01"));
    }

    /// The built-in layout is `[]`, not an absent key: the portal keeps its
    /// last list on an absent key, so a cleared selection would stay ticked.
    #[test]
    fn config_state_payload_publishes_no_display_lines_as_an_empty_list() {
        let payload = config_state_payload_with_display_lines(&[]);
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["display_lines"], serde_json::json!([]));
    }
}
