//! What the front-panel LCD needs to know about Beacons.
//!
//! The overview screen can render a configured `ble` row for a Beacon by MAC — temperature,
//! humidity, battery, alarm state. That rendering is the agent's, and it stays: it is bound to a
//! 20×4 character display and a `custom_lines` layout the crate knows nothing about.
//!
//! What moved is where the values come from. v1 read a process-global `EYE_STATE` that its own
//! monitor thread owned; there is no such thread now, so the values come from the crate's snapshot
//! — the same one that feeds `eye/sensors` — through the seams registry.
//!
//! **Empty is a valid answer.** A Hub with `ble.mode: off`, no controller, or one still booting has
//! no snapshot. The overview renders configured rows as placeholders in that case rather than
//! dropping them, which is what tells an operator the row is configured but silent.

use std::collections::HashMap;

use crate::libs::lorawan::state::LoRaWANAlarmState;

/// One Beacon, in the shape the overview renderer expects.
///
/// Deliberately the same field set as v1's `beacon::state::BeaconTagState` minus the four fields
/// only its monitor used (`provision_attempts`, `last_archived_ts`, `last_download_ts`,
/// `provisioning`'s enum): those were scheduler bookkeeping, and the crate owns the scheduler now.
/// Keeping the rest identical is what lets `overview.rs` stay untouched.
#[derive(Debug, Clone, PartialEq)]
pub struct BeaconTagState {
    pub mac: String,
    pub name: Option<String>,
    pub temperature_c: Option<f32>,
    pub humidity_pct: Option<u8>,
    pub battery_mv: Option<u16>,
    pub low_battery: bool,
    pub magnet_present: bool,
    pub magnet_detected: bool,
    pub moving: Option<bool>,
    pub movement_count: Option<u16>,
    pub pitch_deg: Option<i8>,
    pub roll_deg: Option<i16>,
    pub rssi: Option<i16>,
    /// Unix seconds of the last advertising frame seen, or `None` if never.
    pub last_seen_ts: Option<i64>,
    /// EN12830 recorder present (white Beacon)? `None` until first probed.
    pub is_en12830: Option<bool>,
    /// Per-field alarm state (`temperature`/`humidity`), from the configured thresholds.
    pub field_alarm_states: HashMap<String, LoRaWANAlarmState>,
    /// Aggregate (worst-of-fields) alarm state.
    pub alarm_state: LoRaWANAlarmState,
}

impl BeaconTagState {
    /// A Beacon with an identity and nothing heard yet.
    ///
    /// The state a row is in between "an operator enrolled this MAC" and "its first advertisement
    /// arrived": named, and honest about knowing nothing else. The overview renders it as a
    /// placeholder rather than dropping the row, which is what tells the operator the Beacon is
    /// configured but silent.
    pub fn new(mac: String, name: Option<String>) -> BeaconTagState {
        BeaconTagState {
            mac,
            name,
            temperature_c: None,
            humidity_pct: None,
            battery_mv: None,
            low_battery: false,
            magnet_present: false,
            magnet_detected: false,
            moving: None,
            movement_count: None,
            pitch_deg: None,
            roll_deg: None,
            rssi: None,
            last_seen_ts: None,
            is_en12830: None,
            field_alarm_states: HashMap::new(),
            alarm_state: LoRaWANAlarmState::Disconnected,
        }
    }
}

/// Every enrolled Beacon the crate currently knows about, sorted by MAC.
///
/// Sorted because the overview addresses rows by MAC and an operator reading two Hubs side by side
/// should see the same order on both.
pub fn display_snapshot() -> Vec<BeaconTagState> {
    let Some(seams) = super::registry::seams() else {
        return Vec::new();
    };
    let Some(central) = seams.central.as_ref() else {
        return Vec::new();
    };
    let Ok(section) = control_ble::host::ConfigStore::load(seams.host.config.as_ref()) else {
        return Vec::new();
    };
    let enrolled = section
        .tags
        .iter()
        .filter_map(|t| control_ble::mac::MacAddr::parse(&t.mac).ok())
        .collect();
    let now = seams.host.clock.now_unix();
    let cfg = control_ble::config::CentralConfig::default();
    let snapshot = central.snapshot(now, &cfg, &seams.host.device.hostname(), &enrolled);

    let mut tags: Vec<BeaconTagState> = snapshot
        .tags
        .iter()
        // A stranger the Hub merely overheard is not something to put on the panel: the LCD shows
        // this Hub's fleet, and a discovered Beacon has not been adopted into it.
        .filter(|t| !t.discovered)
        .map(from_payload)
        .collect();
    // The central only knows a name it heard advertised; fall back to the one
    // the Beacon was enrolled under.
    for tag in tags.iter_mut().filter(|t| t.name.is_none()) {
        tag.name = section
            .tags
            .iter()
            .find(|t| t.mac.eq_ignore_ascii_case(&tag.mac))
            .and_then(|t| t.name.clone());
    }
    tags.sort_by(|a, b| a.mac.cmp(&b.mac));
    tags
}

fn from_payload(p: &control_ble::host::BeaconPayload) -> BeaconTagState {
    BeaconTagState {
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
        is_en12830: p.is_en12830,
        field_alarm_states: p
            .field_alarm_states
            .iter()
            .map(|(k, v)| (k.clone(), alarm_from_str(v)))
            .collect(),
        alarm_state: alarm_from_str(&p.alarm_state),
    }
}

/// The crate publishes alarm state as the wire string; the LCD wants the agent's enum.
///
/// An unrecognised value becomes `Disconnected` rather than `Normal`: if the two vocabularies ever
/// drift, an operator must see "we do not know" on the panel, not a reassuring green.
fn alarm_from_str(s: &str) -> LoRaWANAlarmState {
    // control-ble publishes the level uppercase (`AlarmLevel::as_str`).
    match s.to_ascii_lowercase().as_str() {
        "normal" | "ok" => LoRaWANAlarmState::Normal,
        "warning" => LoRaWANAlarmState::Warning,
        "critical" => LoRaWANAlarmState::Critical,
        _ => LoRaWANAlarmState::Disconnected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hub_with_no_ble_runtime_shows_no_beacons_rather_than_failing() {
        // `ble.mode: off`, no controller, or still booting. The overview renders configured rows as
        // placeholders from an empty list; a panic here would take the display thread down.
        if super::super::registry::seams().is_none() {
            assert!(display_snapshot().is_empty());
        }
    }

    #[test]
    fn an_unknown_alarm_string_reads_as_disconnected_not_normal() {
        assert_eq!(alarm_from_str("normal"), LoRaWANAlarmState::Normal);
        assert_eq!(alarm_from_str("NORMAL"), LoRaWANAlarmState::Normal);
        assert_eq!(alarm_from_str("CRITICAL"), LoRaWANAlarmState::Critical);
        assert_eq!(alarm_from_str("ok"), LoRaWANAlarmState::Normal);
        assert_eq!(alarm_from_str("warning"), LoRaWANAlarmState::Warning);
        assert_eq!(alarm_from_str("critical"), LoRaWANAlarmState::Critical);
        assert_eq!(
            alarm_from_str("something-new"),
            LoRaWANAlarmState::Disconnected,
            "a vocabulary drift must not paint the panel green"
        );
    }

    #[test]
    fn the_payload_map_narrows_temperature_and_concretes_the_flags() {
        let p = control_ble::host::BeaconPayload {
            gateway: "hub".into(),
            mac: "AA:BB:CC:DD:EE:FF".into(),
            name: None,
            temperature_c: Some(-18.5),
            humidity_pct: Some(40),
            battery_mv: Some(2900),
            low_battery: None,
            magnet_present: None,
            magnet_detected: None,
            moving: None,
            movement_count: None,
            pitch_deg: None,
            roll_deg: None,
            rssi: Some(-70),
            last_seen_ts: Some(1_700_000_000),
            stale: false,
            provisioning: "provisioned".into(),
            discovered: false,
            is_en12830: None,
            field_alarm_states: Default::default(),
            alarm_state: "normal".into(),
            ble_config: None,
        };
        let t = from_payload(&p);
        assert_eq!(t.temperature_c, Some(-18.5));
        assert!(
            !t.low_battery,
            "never heard reads as not-low, as v1 published"
        );
        assert_eq!(t.alarm_state, LoRaWANAlarmState::Normal);
    }
}
