//! Shared LoRaWAN state management
//!
//! Thread-safe state for LoRaWAN gateway and sensor data.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};

use crate::libs::config::{LoRaWANSensorConfig, FieldThreshold};

use super::chirpstack::{StickerReading, StickerEvent};

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

fn evaluate_threshold(
    value: f64,
    critical_low: Option<f64>,
    warning_low: Option<f64>,
    warning_high: Option<f64>,
    critical_high: Option<f64>,
) -> LoRaWANAlarmState {
    if let Some(cl) = critical_low { if value < cl { return LoRaWANAlarmState::Critical; } }
    if let Some(ch) = critical_high { if value > ch { return LoRaWANAlarmState::Critical; } }
    if let Some(wl) = warning_low { if value < wl { return LoRaWANAlarmState::Warning; } }
    if let Some(wh) = warning_high { if value > wh { return LoRaWANAlarmState::Warning; } }
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
    pub counters: HashMap<String, u64>,
    pub recent_events: VecDeque<StickerEvent>,
    pub rssi: Option<i32>,
    pub snr: Option<f32>,
    pub last_seen: Option<String>,
    pub alarm_state: LoRaWANAlarmState,
}

impl LoRaWANSensorState {
    pub fn from_reading(reading: &StickerReading) -> Self {
        let mut events: VecDeque<StickerEvent> = reading.events.iter().cloned().collect();
        while events.len() > MAX_RECENT_EVENTS { events.pop_front(); }
        Self {
            dev_eui: reading.dev_eui.clone(),
            name: reading.device_name.clone(),
            serial_number: None,
            location: None,
            fields: reading.fields.clone(),
            field_alarm_states: HashMap::new(),
            counters: reading.counters.clone(),
            recent_events: events,
            rssi: reading.rssi,
            snr: reading.snr,
            last_seen: if reading.received_at.is_empty() {
                None
            } else {
                Some(reading.received_at.clone())
            },
            alarm_state: LoRaWANAlarmState::Normal,
        }
    }

    pub fn update_from_reading(&mut self, reading: &StickerReading) {
        if !reading.device_name.is_empty() {
            self.name = reading.device_name.clone();
        }
        for (k, v) in &reading.fields { self.fields.insert(k.clone(), *v); }
        for (k, v) in &reading.counters { self.counters.insert(k.clone(), *v); }
        for ev in &reading.events {
            self.recent_events.push_back(ev.clone());
            if self.recent_events.len() > MAX_RECENT_EVENTS { self.recent_events.pop_front(); }
        }
        self.rssi = reading.rssi;
        self.snr = reading.snr;
        if !reading.received_at.is_empty() {
            self.last_seen = Some(reading.received_at.clone());
        }
        self.field_alarm_states.clear();
        self.alarm_state = LoRaWANAlarmState::Normal;
    }

    pub fn evaluate_alarms(&mut self, config: Option<&LoRaWANSensorConfig>) {
        self.field_alarm_states.clear();
        let Some(cfg) = config else {
            self.alarm_state = LoRaWANAlarmState::Normal;
            return;
        };
        if let Some(ref name) = cfg.name { self.name = name.clone(); }
        self.serial_number = cfg.serial_number.clone();
        self.location = cfg.location.clone();

        for t in &cfg.field_thresholds {
            if let Some(&v) = self.fields.get(&t.field) {
                let s = evaluate_threshold(
                    v,
                    t.critical_low, t.warning_low,
                    t.warning_high, t.critical_high,
                );
                self.field_alarm_states.insert(t.field.clone(), s);
            }
        }
        self.alarm_state = self.field_alarm_states.values().cloned()
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
}

impl LoRaWANState {
    pub fn new(gateway_present: bool) -> Self {
        Self {
            gateway_present,
            concentratord_running: false,
            chirpstack_running: false,
            sensors: HashMap::new(),
        }
    }

    pub fn update_sensor(&mut self, reading: &StickerReading) {
        if !self.sensors.contains_key(&reading.dev_eui) {
            self.sensors.insert(
                reading.dev_eui.clone(),
                LoRaWANSensorState::from_reading(reading),
            );
        } else {
            self.sensors.get_mut(&reading.dev_eui).unwrap().update_from_reading(reading);
        }
    }

    pub fn evaluate_alarms(&mut self, sensor_configs: &[LoRaWANSensorConfig]) {
        for sensor in self.sensors.values_mut() {
            let config = sensor_configs.iter().find(|c| c.dev_eui == sensor.dev_eui);
            sensor.evaluate_alarms(config);
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
            rssi: Some(-80),
            snr: Some(7.0),
            received_at: "2026-05-12T10:00:00Z".into(),
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
                    critical_low: Some(0.0), warning_low: Some(10.0),
                    warning_high: Some(35.0), critical_high: Some(40.0),
                },
                FieldThreshold {
                    field: "humidity".into(),
                    critical_low: None, warning_low: Some(20.0),
                    warning_high: Some(80.0), critical_high: Some(90.0),
                },
            ],
        };
        state.evaluate_alarms(&[cfg]);
        let s = &state.sensors["aabb"];
        assert_eq!(s.field_alarm_states["temperature"], LoRaWANAlarmState::Critical);
        assert_eq!(s.field_alarm_states["humidity"], LoRaWANAlarmState::Normal);
        assert_eq!(s.alarm_state, LoRaWANAlarmState::Critical);
    }

    #[test]
    fn test_no_threshold_means_no_alarm_entry() {
        let mut state = LoRaWANState::new(true);
        state.update_sensor(&reading_with_fields("aabb", 22.0, 50.0));
        let cfg = LoRaWANSensorConfig {
            dev_eui: "aabb".into(), name: None, serial_number: None,
            location: None, enabled: true, field_thresholds: vec![],
        };
        state.evaluate_alarms(&[cfg]);
        assert!(state.sensors["aabb"].field_alarm_states.is_empty());
        assert_eq!(state.sensors["aabb"].alarm_state, LoRaWANAlarmState::Normal);
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
        let cfgs = create_shared_lorawan_sensor_configs(vec![
            LoRaWANSensorConfig {
                dev_eui: "aabb".into(),
                name: Some("A".into()),
                serial_number: None,
                location: None,
                enabled: true,
                field_thresholds: vec![FieldThreshold {
                    field: "temperature".into(),
                    critical_low: Some(0.0), warning_low: None,
                    warning_high: None, critical_high: Some(40.0),
                }],
            },
        ]);
        assert_eq!(cfgs.read().unwrap().len(), 1);
    }
}
