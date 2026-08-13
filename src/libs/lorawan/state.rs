//! Shared LoRaWAN state management
//!
//! Thread-safe state for LoRaWAN gateway and sensor data.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};

use crate::libs::config::{
    effective_field_thresholds, FieldThreshold, FieldThresholdBounds, LoRaWANSensorConfig,
};

use super::chirpstack::{GatewayRx, StickerEvent, StickerReading};

const MAX_RECENT_EVENTS: usize = 32;

/// Alarm state for LoRaWAN sensors (4-level, matches DS18B20)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LoRaWANAlarmState {
    Normal,
    Warning,
    Critical,
    Disconnected,
}

impl std::fmt::Display for LoRaWANAlarmState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoRaWANAlarmState::Normal => write!(f, "NORMAL"),
            LoRaWANAlarmState::Warning => write!(f, "WARNING"),
            LoRaWANAlarmState::Critical => write!(f, "CRITICAL"),
            LoRaWANAlarmState::Disconnected => write!(f, "DISCONNECTED"),
        }
    }
}

impl LoRaWANAlarmState {
    pub fn worst(&self, other: &LoRaWANAlarmState) -> LoRaWANAlarmState {
        match (self, other) {
            (LoRaWANAlarmState::Disconnected, _) | (_, LoRaWANAlarmState::Disconnected) => {
                LoRaWANAlarmState::Disconnected
            }
            (LoRaWANAlarmState::Critical, _) | (_, LoRaWANAlarmState::Critical) => {
                LoRaWANAlarmState::Critical
            }
            (LoRaWANAlarmState::Warning, _) | (_, LoRaWANAlarmState::Warning) => {
                LoRaWANAlarmState::Warning
            }
            _ => LoRaWANAlarmState::Normal,
        }
    }
}

/// Classify a value against optional 4-level bounds. Shared with the EYE tag
/// alarm path (`eye::state::EyeTagState::evaluate_alarms`).
pub(crate) fn evaluate_threshold(
    value: f64,
    critical_low: Option<f64>,
    warning_low: Option<f64>,
    warning_high: Option<f64>,
    critical_high: Option<f64>,
) -> LoRaWANAlarmState {
    if let Some(cl) = critical_low {
        if value < cl {
            return LoRaWANAlarmState::Critical;
        }
    }
    if let Some(ch) = critical_high {
        if value > ch {
            return LoRaWANAlarmState::Critical;
        }
    }
    if let Some(wl) = warning_low {
        if value < wl {
            return LoRaWANAlarmState::Warning;
        }
    }
    if let Some(wh) = warning_high {
        if value > wh {
            return LoRaWANAlarmState::Warning;
        }
    }
    LoRaWANAlarmState::Normal
}

/// State for a single LoRaWAN sensor (generic field model)
#[derive(Debug, Clone)]
pub struct LoRaWANSensorState {
    pub dev_eui: String,
    pub name: String,
    pub serial_number: Option<String>,
    pub location: Option<String>,
    pub fields: HashMap<String, f64>,
    pub field_alarm_states: HashMap<String, LoRaWANAlarmState>,
    /// Effective per-field thresholds (per-sensor override merged over YAML
    /// defaults). Recomputed on every `evaluate_alarms`. Used by the MQTT
    /// publisher and the on-device display so they don't re-resolve.
    pub field_thresholds: Vec<FieldThreshold>,
    pub counters: HashMap<String, u64>,
    pub recent_events: VecDeque<StickerEvent>,
    /// Every gateway that received the latest uplink (replaced each uplink).
    pub gateways: Vec<GatewayRx>,
    /// LoRaWAN data-rate index of the latest uplink.
    pub dr: Option<i64>,
    /// Back-compat scalar = the best (strongest) gateway of `gateways`.
    pub rssi: Option<i32>,
    pub snr: Option<f32>,
    /// Gateway ChirpStack last used to transmit a downlink to this sticker (from
    /// `event/txack`) — the network's best-signal pick. Display-only; persists
    /// across uplinks and is only replaced by a newer txack. Distinct from the
    /// uplink `gateways`/`rssi` (who *received* the last uplink).
    pub downlink_gateway_id: Option<String>,
    pub last_seen: Option<String>,
    /// Unix seconds of the last few uplinks, oldest first, for deriving the
    /// sticker's *observed* reporting cadence — see `observed_interval_secs`.
    /// Bounded ring, in memory only: after a restart it refills within a few
    /// uplinks and callers fall back to their own default meanwhile.
    pub uplink_ring: VecDeque<i64>,
    pub alarm_state: LoRaWANAlarmState,
}

/// How many uplink instants to remember per sticker. Enough for a median over
/// several intervals without holding history nobody reads.
const UPLINK_RING_LEN: usize = 6;

/// The sticker's cadence from a set of uplink instants, in seconds.
///
/// `max(median gap, most recent gap)`, and the asymmetry is deliberate.
///
/// The median alone resists the two ways a single gap lies: an event-driven uplink
/// landing seconds after a periodic one would make a 15-minute sticker look like a
/// 10-second one, and one missed uplink would make it look twice as slow. But a
/// median also *lags* a real change — after `interval_report` goes 120 s -> 900 s
/// it takes four more uplinks (about an hour) for the median to follow, and every
/// read in that window would still be given a 120 s-sized timeout and fail.
///
/// Taking the most recent gap as a lower bound closes that window immediately,
/// and it is safe in only one direction: a timeout that is too long merely delays
/// reporting a failure, while one that is too short breaks a read that would have
/// worked. A burst uplink cannot shrink the result, because the median floors it.
fn cadence_from(times: &[i64]) -> Option<u64> {
    if times.len() < 2 {
        return None;
    }
    let mut sorted = times.to_vec();
    sorted.sort_unstable();
    let gaps: Vec<i64> = sorted
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|g| *g > 0)
        .collect();
    if gaps.is_empty() {
        return None;
    }
    let latest = *gaps.last().unwrap();
    let mut sorted_gaps = gaps.clone();
    sorted_gaps.sort_unstable();
    let mid = sorted_gaps.len() / 2;
    let median = if sorted_gaps.len() % 2 == 1 {
        sorted_gaps[mid]
    } else {
        (sorted_gaps[mid - 1] + sorted_gaps[mid]) / 2
    };
    Some(median.max(latest) as u64)
}

/// Parse an uplink timestamp into unix seconds. Readings carry whatever shape
/// ChirpStack sent (`…Z`, `…+00:00`, fractional seconds), so be tolerant: an
/// unparseable value simply does not contribute to the cadence.
fn unix_secs(ts: &str) -> Option<i64> {
    if ts.is_empty() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.timestamp())
}

/// The inverse of [`unix_secs`] — the same RFC3339 shape an uplink carries, so a
/// `last_seen` recovered from storage parses everywhere a live one does.
fn rfc3339(secs: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(secs, 0).map(|d| d.to_rfc3339())
}

impl LoRaWANSensorState {
    /// A sticker the device knows has reported before, but has not heard from
    /// since this process started.
    ///
    /// Without this a restart makes every sticker read as *never connected* — the
    /// row only exists once an uplink creates it, and the viewer's default for a
    /// missing row is `NeverConnected`. With a 15-minute cadence that is a
    /// quarter of an hour in which a fridge that has been reporting for months is
    /// displayed exactly like one that was never installed, and the two call for
    /// opposite actions: check the radio versus check the wiring.
    ///
    /// So the row carries the real `last_seen` from storage and the honest state,
    /// `Disconnected`. No values: the stored reading may be hours old, and showing
    /// a stale temperature as if it were current is worse than showing none. The
    /// first uplink replaces all of this.
    pub fn from_storage(
        dev_eui: &str,
        name: Option<&str>,
        serial_number: Option<&str>,
        location: Option<&str>,
        uplinks: &[i64],
    ) -> Option<Self> {
        let last = *uplinks.iter().max()?;
        Some(Self {
            dev_eui: dev_eui.to_string(),
            name: name.unwrap_or(dev_eui).to_string(),
            serial_number: serial_number.map(str::to_string),
            location: location.map(str::to_string),
            fields: HashMap::new(),
            field_alarm_states: HashMap::new(),
            field_thresholds: Vec::new(),
            counters: HashMap::new(),
            recent_events: VecDeque::new(),
            gateways: Vec::new(),
            dr: None,
            rssi: None,
            snr: None,
            downlink_gateway_id: None,
            last_seen: rfc3339(last),
            // Seeding the ring too would be wrong: `observed_interval_secs` is the
            // *live* cadence and must stay empty until this process has seen real
            // uplinks. The recovered cadence already lives in `cadence_hint`.
            uplink_ring: VecDeque::new(),
            alarm_state: LoRaWANAlarmState::Disconnected,
        })
    }

    pub fn from_reading(reading: &StickerReading) -> Self {
        let mut events: VecDeque<StickerEvent> = reading.events.iter().cloned().collect();
        while events.len() > MAX_RECENT_EVENTS {
            events.pop_front();
        }
        Self {
            dev_eui: reading.dev_eui.clone(),
            name: reading.device_name.clone(),
            serial_number: None,
            location: None,
            fields: reading.fields.clone(),
            field_alarm_states: HashMap::new(),
            field_thresholds: Vec::new(),
            counters: reading.counters.clone(),
            recent_events: events,
            gateways: reading.gateways.clone(),
            dr: reading.dr,
            rssi: reading.rssi,
            snr: reading.snr,
            downlink_gateway_id: None,
            last_seen: if reading.received_at.is_empty() {
                None
            } else {
                Some(reading.received_at.clone())
            },
            uplink_ring: unix_secs(&reading.received_at).into_iter().collect(),
            alarm_state: LoRaWANAlarmState::Normal,
        }
    }

    pub fn update_from_reading(&mut self, reading: &StickerReading) {
        if !reading.device_name.is_empty() {
            self.name = reading.device_name.clone();
        }
        for (k, v) in &reading.fields {
            self.fields.insert(k.clone(), *v);
        }
        for (k, v) in &reading.counters {
            self.counters.insert(k.clone(), *v);
        }
        for ev in &reading.events {
            self.recent_events.push_back(ev.clone());
            if self.recent_events.len() > MAX_RECENT_EVENTS {
                self.recent_events.pop_front();
            }
        }
        self.gateways = reading.gateways.clone();
        self.dr = reading.dr;
        self.rssi = reading.rssi;
        self.snr = reading.snr;
        if !reading.received_at.is_empty() {
            // Record the instant BEFORE overwriting last_seen: the ring is what makes
            // the sticker's real cadence observable, and a Class-A read/write can only
            // be answered in the window after an uplink, so that cadence bounds every
            // fPort-85 round trip.
            if let Some(ts) = unix_secs(&reading.received_at) {
                if self.uplink_ring.back() != Some(&ts) {
                    self.uplink_ring.push_back(ts);
                    while self.uplink_ring.len() > UPLINK_RING_LEN {
                        self.uplink_ring.pop_front();
                    }
                }
            }
            self.last_seen = Some(reading.received_at.clone());
        }
        self.field_alarm_states.clear();
        self.alarm_state = LoRaWANAlarmState::Normal;
    }

    /// The sticker's observed reporting cadence in seconds, or `None` until at
    /// least two uplinks have been seen.
    ///
    /// `max(median gap, most recent gap)` — see `cadence_from` for why the
    /// median alone is not enough and why the asymmetry is safe.
    pub fn observed_interval_secs(&self) -> Option<u64> {
        let times: Vec<i64> = self.uplink_ring.iter().copied().collect();
        cadence_from(&times)
    }

    pub fn evaluate_alarms(
        &mut self,
        config: Option<&LoRaWANSensorConfig>,
        defaults: &HashMap<String, FieldThresholdBounds>,
    ) {
        self.field_alarm_states.clear();
        self.field_thresholds.clear();
        if let Some(cfg) = config {
            if let Some(ref name) = cfg.name {
                self.name = name.clone();
            }
            self.serial_number = cfg.serial_number.clone();
            self.location = cfg.location.clone();
        }

        // Merge per-sensor overrides over YAML defaults. Stickers without a
        // matching config entry still pick up defaults — this is what gives
        // newly-paired stickers automatic alarming, mirroring DS18B20 probes.
        self.field_thresholds = effective_field_thresholds(config, defaults);

        for t in &self.field_thresholds {
            if let Some(&v) = self.fields.get(&t.field) {
                let s = evaluate_threshold(
                    v,
                    t.critical_low,
                    t.warning_low,
                    t.warning_high,
                    t.critical_high,
                );
                self.field_alarm_states.insert(t.field.clone(), s);
            }
        }
        // A row that has never carried a measurement in this process — the
        // `Disconnected` placeholder seeded from storage at start-up — has nothing
        // to fold, and folding an empty set yields `Normal`. That would announce a
        // silent sticker as healthy, which is the opposite of true, so leave its
        // state alone until an uplink gives it something to evaluate.
        //
        // Deliberately keyed on "has any measurement at all" rather than on the
        // alarm map being empty: a sticker that *is* reporting but has every
        // threshold removed must still fall back to Normal.
        if self.fields.is_empty() && self.counters.is_empty() {
            return;
        }
        self.alarm_state = self
            .field_alarm_states
            .values()
            .cloned()
            .fold(LoRaWANAlarmState::Normal, |a, b| a.worst(&b));
    }
}

/// Aggregate state for LoRaWAN gateway and all sensors
#[derive(Debug, Clone)]
pub struct LoRaWANState {
    pub gateway_present: bool,
    pub concentratord_running: bool,
    pub chirpstack_running: bool,
    pub sensors: HashMap<String, LoRaWANSensorState>,
    /// Reporting cadence (seconds) recovered from persisted uplinks at start-up,
    /// keyed by dev_eui.
    ///
    /// Kept beside `sensors` rather than inside them on purpose: a sensor row is
    /// created by the first uplink, so at start-up there is nowhere to put this —
    /// and inventing a row would make a sticker that has never reported look
    /// present. Consulted only until the live ring has two uplinks of its own.
    pub cadence_hint: HashMap<String, u64>,
}

impl LoRaWANState {
    pub fn new(gateway_present: bool) -> Self {
        Self {
            gateway_present,
            concentratord_running: false,
            chirpstack_running: false,
            sensors: HashMap::new(),
            cadence_hint: HashMap::new(),
        }
    }

    /// Record the cadence derived from persisted uplink timestamps (oldest first).
    ///
    /// Returns the cadence it stored, or `None` when the timestamps cannot yield
    /// one. Stored as a hint rather than pushed into a sensor row because no row
    /// exists until the sticker's first uplink after start-up.
    pub fn seed_cadence_hint(&mut self, dev_eui: &str, times: Vec<i64>) -> Option<u64> {
        let cadence = cadence_from(&times)?;
        self.cadence_hint.insert(dev_eui.to_string(), cadence);
        Some(cadence)
    }

    /// Give a sticker that has reported before a `Disconnected` row at start-up,
    /// so a restart does not report it as *never connected*.
    ///
    /// Only for stickers with stored uplinks — one that has genuinely never
    /// reported still gets no row, which is what makes `NeverConnected` mean
    /// something. Never overwrites a row a live uplink already created, so this is
    /// safe to call after the monitor is running.
    ///
    /// Returns the recovered `last_seen`, or `None` when nothing was seeded.
    pub fn seed_disconnected(
        &mut self,
        cfg: &LoRaWANSensorConfig,
        uplinks: &[i64],
    ) -> Option<String> {
        let dev_eui = cfg.dev_eui.to_lowercase();
        if self.sensors.contains_key(&dev_eui) {
            return None;
        }
        let row = LoRaWANSensorState::from_storage(
            &dev_eui,
            cfg.name.as_deref(),
            cfg.serial_number.as_deref(),
            cfg.location.as_deref(),
            uplinks,
        )?;
        let last_seen = row.last_seen.clone();
        self.sensors.insert(dev_eui, row);
        last_seen
    }

    /// The sticker's reporting cadence: what its live uplinks show, else the hint
    /// recovered from storage at start-up.
    pub fn cadence_secs(&self, dev_eui: &str) -> Option<u64> {
        self.sensors
            .get(dev_eui)
            .and_then(|s| s.observed_interval_secs())
            .or_else(|| self.cadence_hint.get(dev_eui).copied())
    }

    pub fn update_sensor(&mut self, reading: &StickerReading) {
        if !self.sensors.contains_key(&reading.dev_eui) {
            self.sensors.insert(
                reading.dev_eui.clone(),
                LoRaWANSensorState::from_reading(reading),
            );
        } else {
            self.sensors
                .get_mut(&reading.dev_eui)
                .unwrap()
                .update_from_reading(reading);
        }
    }

    /// Record the gateway ChirpStack used to transmit the last downlink to a
    /// sticker (from an `event/txack`). Display-only; persists across uplinks.
    /// Returns `false` (no-op) if the sticker has no state row yet — a txack
    /// before the first uplink; rows are created by uplinks.
    pub fn set_downlink_gateway(&mut self, dev_eui: &str, gateway_id: String) -> bool {
        match self.sensors.get_mut(dev_eui) {
            Some(s) => {
                s.downlink_gateway_id = Some(gateway_id);
                true
            }
            None => false,
        }
    }

    pub fn evaluate_alarms(
        &mut self,
        sensor_configs: &[LoRaWANSensorConfig],
        defaults: &HashMap<String, FieldThresholdBounds>,
    ) {
        for sensor in self.sensors.values_mut() {
            let config = sensor_configs.iter().find(|c| c.dev_eui == sensor.dev_eui);
            sensor.evaluate_alarms(config, defaults);
        }
    }

    pub fn check_timeouts(&mut self, timeout_secs: u64) {
        let now = chrono::Utc::now();
        for sensor in self.sensors.values_mut() {
            if let Some(ref last_seen) = sensor.last_seen {
                if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(last_seen) {
                    let elapsed = now.signed_duration_since(ts);
                    if elapsed.num_seconds() > timeout_secs as i64 {
                        sensor.alarm_state = LoRaWANAlarmState::Disconnected;
                        for v in sensor.field_alarm_states.values_mut() {
                            *v = LoRaWANAlarmState::Disconnected;
                        }
                    }
                }
            }
        }
    }
}

pub type SharedLoRaWANState = Arc<RwLock<LoRaWANState>>;

pub fn create_shared_lorawan_state(gateway_present: bool) -> SharedLoRaWANState {
    Arc::new(RwLock::new(LoRaWANState::new(gateway_present)))
}

pub type SharedLoRaWANSensorConfigs = Arc<RwLock<Vec<LoRaWANSensorConfig>>>;

pub fn create_shared_lorawan_sensor_configs(
    seed: Vec<LoRaWANSensorConfig>,
) -> SharedLoRaWANSensorConfigs {
    Arc::new(RwLock::new(seed))
}

/// Shared, immutable-at-runtime defaults loaded from `fiber.sensors.config.yaml`.
/// Wrapped in `Arc` (no `RwLock`) because YAML defaults are not mutated by
/// MQTT commands — to change them, the operator edits the file and restarts.
pub type SharedFieldThresholdDefaults = Arc<HashMap<String, FieldThresholdBounds>>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn reading_with_fields(dev_eui: &str, t: f64, h: f64) -> StickerReading {
        let mut fields = HashMap::new();
        fields.insert("temperature".into(), t);
        fields.insert("humidity".into(), h);
        StickerReading {
            dev_eui: dev_eui.into(),
            device_name: "test".into(),
            fields,
            counters: HashMap::new(),
            events: Vec::new(),
            gateways: Vec::new(),
            dr: None,
            rssi: Some(-80),
            snr: Some(7.0),
            received_at: "2026-05-12T10:00:00Z".into(),
            fport: Some(2),
        }
    }

    #[test]
    fn test_update_sensor_stores_fields() {
        let mut state = LoRaWANState::new(true);
        state.update_sensor(&reading_with_fields("aabb", 22.5, 48.0));
        let s = &state.sensors["aabb"];
        assert_eq!(s.fields.get("temperature").copied(), Some(22.5));
        assert_eq!(s.fields.get("humidity").copied(), Some(48.0));
    }

    #[test]
    fn test_evaluate_alarms_per_field() {
        let mut state = LoRaWANState::new(true);
        state.update_sensor(&reading_with_fields("aabb", 45.0, 50.0));
        let cfg = LoRaWANSensorConfig {
            dev_eui: "aabb".into(),
            name: Some("t".into()),
            serial_number: None,
            location: None,
            enabled: true,
            field_thresholds: vec![
                FieldThreshold {
                    field: "temperature".into(),
                    critical_low: Some(0.0),
                    warning_low: Some(10.0),
                    warning_high: Some(35.0),
                    critical_high: Some(40.0),
                },
                FieldThreshold {
                    field: "humidity".into(),
                    critical_low: None,
                    warning_low: Some(20.0),
                    warning_high: Some(80.0),
                    critical_high: Some(90.0),
                },
            ],
            disarmed_fields: Vec::new(),
        };
        state.evaluate_alarms(&[cfg], &HashMap::new());
        let s = &state.sensors["aabb"];
        assert_eq!(
            s.field_alarm_states["temperature"],
            LoRaWANAlarmState::Critical
        );
        assert_eq!(s.field_alarm_states["humidity"], LoRaWANAlarmState::Normal);
        assert_eq!(s.alarm_state, LoRaWANAlarmState::Critical);
    }

    #[test]
    fn test_no_threshold_means_no_alarm_entry() {
        let mut state = LoRaWANState::new(true);
        state.update_sensor(&reading_with_fields("aabb", 22.0, 50.0));
        let cfg = LoRaWANSensorConfig {
            dev_eui: "aabb".into(),
            name: None,
            serial_number: None,
            location: None,
            enabled: true,
            field_thresholds: vec![],
            disarmed_fields: Vec::new(),
        };
        state.evaluate_alarms(&[cfg], &HashMap::new());
        assert!(state.sensors["aabb"].field_alarm_states.is_empty());
        assert_eq!(state.sensors["aabb"].alarm_state, LoRaWANAlarmState::Normal);
    }

    #[test]
    fn test_yaml_defaults_apply_when_no_override() {
        let mut state = LoRaWANState::new(true);
        state.update_sensor(&reading_with_fields("aabb", 45.0, 50.0));
        let cfg = LoRaWANSensorConfig {
            dev_eui: "aabb".into(),
            name: None,
            serial_number: None,
            location: None,
            enabled: true,
            field_thresholds: vec![],
            disarmed_fields: Vec::new(),
        };
        let mut defaults: HashMap<String, FieldThresholdBounds> = HashMap::new();
        defaults.insert(
            "temperature".into(),
            FieldThresholdBounds {
                critical_low: Some(0.0),
                warning_low: Some(5.0),
                warning_high: Some(30.0),
                critical_high: Some(40.0),
            },
        );
        state.evaluate_alarms(&[cfg], &defaults);
        let s = &state.sensors["aabb"];
        assert_eq!(
            s.field_alarm_states["temperature"],
            LoRaWANAlarmState::Critical
        );
        assert_eq!(s.alarm_state, LoRaWANAlarmState::Critical);
        // Effective thresholds are surfaced so the publisher/display can read them.
        let t = s
            .field_thresholds
            .iter()
            .find(|t| t.field == "temperature")
            .unwrap();
        assert_eq!(t.critical_high, Some(40.0));
    }

    #[test]
    fn test_override_takes_precedence_over_default_per_bound() {
        let mut state = LoRaWANState::new(true);
        state.update_sensor(&reading_with_fields("aabb", 42.0, 50.0));
        let cfg = LoRaWANSensorConfig {
            dev_eui: "aabb".into(),
            name: None,
            serial_number: None,
            location: None,
            enabled: true,
            // Override only critical_high; other bounds come from defaults.
            field_thresholds: vec![FieldThreshold {
                field: "temperature".into(),
                critical_low: None,
                warning_low: None,
                warning_high: None,
                critical_high: Some(50.0),
            }],
            disarmed_fields: Vec::new(),
        };
        let mut defaults: HashMap<String, FieldThresholdBounds> = HashMap::new();
        defaults.insert(
            "temperature".into(),
            FieldThresholdBounds {
                critical_low: Some(0.0),
                warning_low: Some(5.0),
                warning_high: Some(30.0),
                critical_high: Some(40.0),
            },
        );
        state.evaluate_alarms(&[cfg], &defaults);
        let s = &state.sensors["aabb"];
        // 42°C: warning_high (30) exceeded but critical_high override is 50 → Warning.
        assert_eq!(
            s.field_alarm_states["temperature"],
            LoRaWANAlarmState::Warning
        );
    }

    #[test]
    fn test_alarm_state_worst() {
        assert_eq!(
            LoRaWANAlarmState::Normal.worst(&LoRaWANAlarmState::Warning),
            LoRaWANAlarmState::Warning
        );
        assert_eq!(
            LoRaWANAlarmState::Warning.worst(&LoRaWANAlarmState::Critical),
            LoRaWANAlarmState::Critical
        );
        assert_eq!(
            LoRaWANAlarmState::Critical.worst(&LoRaWANAlarmState::Disconnected),
            LoRaWANAlarmState::Disconnected
        );
    }

    #[test]
    fn shared_lorawan_sensor_configs_round_trip() {
        let cfgs = create_shared_lorawan_sensor_configs(vec![LoRaWANSensorConfig {
            dev_eui: "aabb".into(),
            name: Some("A".into()),
            serial_number: None,
            location: None,
            enabled: true,
            field_thresholds: vec![FieldThreshold {
                field: "temperature".into(),
                critical_low: Some(0.0),
                warning_low: None,
                warning_high: None,
                critical_high: Some(40.0),
            }],
            disarmed_fields: Vec::new(),
        }]);
        assert_eq!(cfgs.read().unwrap().len(), 1);
    }

    /// Build a state with a given set of uplink instants (unix seconds).
    fn state_with_uplinks(secs: &[i64]) -> LoRaWANSensorState {
        let mut st = LoRaWANSensorState::from_reading(&StickerReading {
            dev_eui: "aabb".into(),
            device_name: "t".into(),
            fields: HashMap::new(),
            counters: HashMap::new(),
            events: vec![],
            gateways: vec![],
            dr: None,
            rssi: None,
            snr: None,
            received_at: String::new(),
            fport: Some(2),
        });
        st.uplink_ring = secs.iter().copied().collect();
        st
    }

    #[test]
    fn observed_cadence_is_none_until_two_uplinks() {
        assert_eq!(state_with_uplinks(&[]).observed_interval_secs(), None);
        assert_eq!(state_with_uplinks(&[1_000]).observed_interval_secs(), None);
    }

    #[test]
    fn observed_cadence_is_the_gap_between_uplinks() {
        let st = state_with_uplinks(&[0, 900, 1800, 2700]);
        assert_eq!(st.observed_interval_secs(), Some(900));
    }

    #[test]
    fn observed_cadence_ignores_a_burst_uplink() {
        // An event-driven uplink 10 s after a periodic one must not make a
        // 15-minute sticker look like a 10-second one — that would shorten every
        // fPort-85 timeout derived from it straight back to the old floor.
        // Gaps are [900, 10, 890, 900] → median 895, i.e. still ~the real cadence.
        let cadence = state_with_uplinks(&[0, 900, 910, 1800, 2700])
            .observed_interval_secs()
            .expect("cadence");
        assert!(
            (800..=1000).contains(&cadence),
            "burst must not drag the cadence down, got {cadence}s"
        );
    }

    #[test]
    fn observed_cadence_follows_an_interval_increase_immediately() {
        // interval_report 120 s -> 900 s. A pure median would still report ~120 s
        // for another four uplinks, and every read in that hour would be given a
        // 120 s-sized fPort-85 timeout and fail. The newest gap is a lower bound.
        let st = state_with_uplinks(&[0, 120, 240, 360, 480, 1380]);
        assert_eq!(st.observed_interval_secs(), Some(900));
    }

    #[test]
    fn observed_cadence_is_not_shrunk_by_a_burst_as_the_latest_gap() {
        // The newest gap being tiny must NOT win — that is the case the median
        // floors, otherwise one event-driven uplink would collapse the cadence.
        let st = state_with_uplinks(&[0, 900, 1800, 2700, 2710]);
        let c = st.observed_interval_secs().expect("cadence");
        assert!(c >= 800, "a burst must not shrink the cadence, got {c}s");
    }

    #[test]
    fn observed_cadence_ignores_a_single_missed_uplink() {
        // One dropped uplink doubles a gap; the median must ignore it rather than
        // reporting the sticker as twice as slow.
        let st = state_with_uplinks(&[0, 900, 2700, 3600, 4500]);
        assert_eq!(st.observed_interval_secs(), Some(900));
    }

    #[test]
    fn uplink_ring_is_bounded_and_deduplicated() {
        let mut st = state_with_uplinks(&[]);
        let mut reading = StickerReading {
            dev_eui: "aabb".into(),
            device_name: "t".into(),
            fields: HashMap::new(),
            counters: HashMap::new(),
            events: vec![],
            gateways: vec![],
            dr: None,
            rssi: None,
            snr: None,
            received_at: String::new(),
            fport: Some(2),
        };
        // The gateway republishes the same uplink every ~30 s; the ring must not
        // fill with copies of one instant, or the cadence would collapse to zero.
        for _ in 0..3 {
            reading.received_at = "2026-08-04T10:00:00Z".into();
            st.update_from_reading(&reading);
        }
        assert_eq!(st.uplink_ring.len(), 1);

        for i in 0..20 {
            reading.received_at = format!("2026-08-04T10:{:02}:00Z", i + 1);
            st.update_from_reading(&reading);
        }
        assert_eq!(
            st.uplink_ring.len(),
            UPLINK_RING_LEN,
            "ring must stay bounded"
        );
        assert_eq!(st.observed_interval_secs(), Some(60));
    }

    fn cfg(dev_eui: &str, name: &str) -> LoRaWANSensorConfig {
        LoRaWANSensorConfig {
            dev_eui: dev_eui.to_string(),
            name: Some(name.to_string()),
            serial_number: None,
            location: None,
            enabled: true,
            field_thresholds: Vec::new(),
            disarmed_fields: Vec::new(),
        }
    }

    /// A restart must not turn a sticker that has been reporting for months into
    /// one that was never installed. Those two states call for opposite actions —
    /// check the radio versus check the wiring — and with a 15-minute cadence the
    /// wrong one is on screen for a quarter of an hour.
    #[test]
    fn a_sticker_with_history_comes_back_disconnected_not_never_connected() {
        let mut st = LoRaWANState::new(true);
        let c = cfg("58760700c0668fc7", "Input QA");
        // Newest is deliberately not last: storage returns newest-first.
        let uplinks = vec![1_786_000_000, 1_786_000_900, 1_785_999_100];

        let last = st
            .seed_disconnected(&c, &uplinks)
            .expect("a sticker with stored uplinks gets a row");

        let row = st.sensors.get("58760700c0668fc7").expect("row seeded");
        assert_eq!(row.alarm_state, LoRaWANAlarmState::Disconnected);
        assert_eq!(row.name, "Input QA");
        // The newest stored uplink, whatever order storage returned them in.
        assert_eq!(row.last_seen.as_deref(), rfc3339(1_786_000_900).as_deref());
        assert_eq!(last, rfc3339(1_786_000_900).unwrap());
        // No stale values pretending to be current, and no live cadence yet.
        assert!(
            row.fields.is_empty(),
            "a stored reading must not read as current"
        );
        assert!(
            row.uplink_ring.is_empty(),
            "the live cadence ring stays empty"
        );
        assert_eq!(row.observed_interval_secs(), None);
    }

    /// The seeded Disconnected must survive the periodic alarm pass. That pass
    /// folds `field_alarm_states` starting at Normal, and a placeholder row has
    /// none — so before this it announced every silent sticker as healthy, one
    /// evaluation tick after start-up. Measured on fiber-ce3d59f8: last_seen was
    /// recovered correctly and the state still read Normal.
    #[test]
    fn evaluating_a_seeded_row_does_not_declare_it_normal() {
        let mut st = LoRaWANState::new(true);
        let c = cfg("58760700c0668fc7", "Input QA");
        st.seed_disconnected(&c, &[1_786_000_900]).expect("seeded");

        st.evaluate_alarms(std::slice::from_ref(&c), &HashMap::new());

        let row = st.sensors.get("58760700c0668fc7").unwrap();
        assert_eq!(row.alarm_state, LoRaWANAlarmState::Disconnected);
        assert_eq!(row.last_seen.as_deref(), rfc3339(1_786_000_900).as_deref());
    }

    /// The other side of that guard: a sticker that IS reporting, with every
    /// threshold removed, must still fall back to Normal rather than freeze.
    #[test]
    fn a_reporting_sticker_with_no_thresholds_falls_back_to_normal() {
        let mut st = LoRaWANState::new(true);
        st.update_sensor(&reading_with_fields("aabb", 22.5, 48.0));
        if let Some(r) = st.sensors.get_mut("aabb") {
            r.alarm_state = LoRaWANAlarmState::Critical;
        }
        st.evaluate_alarms(&[], &HashMap::new());
        assert_eq!(st.sensors["aabb"].alarm_state, LoRaWANAlarmState::Normal);
    }

    /// The other half of the contract: absence still means "never reported", or
    /// NeverConnected would stop meaning anything.
    #[test]
    fn a_sticker_that_never_reported_gets_no_row() {
        let mut st = LoRaWANState::new(true);
        assert_eq!(
            st.seed_disconnected(&cfg("aabbccddeeff0011", "Fresh"), &[]),
            None
        );
        assert!(st.sensors.is_empty());
    }

    /// Safe to call after the monitor is running: a live uplink always wins.
    #[test]
    fn seeding_never_overwrites_a_live_row() {
        let mut st = LoRaWANState::new(true);
        let mut live = reading_with_fields("58760700c0668fc7", 26.8, 48.0);
        live.device_name = "Input QA".to_string();
        live.received_at = "2026-08-06T14:30:00+00:00".to_string();
        st.update_sensor(&live);

        assert_eq!(
            st.seed_disconnected(&cfg("58760700c0668fc7", "Input QA"), &[1_786_000_000]),
            None
        );
        let row = st.sensors.get("58760700c0668fc7").unwrap();
        assert_eq!(row.fields.get("temperature").copied(), Some(26.8));
        assert_ne!(row.alarm_state, LoRaWANAlarmState::Disconnected);
    }
}
