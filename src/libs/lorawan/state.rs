//! Shared LoRaWAN state management
//!
//! Thread-safe state for LoRaWAN gateway and sensor data.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::libs::config::LoRaWANSensorConfig;

use super::chirpstack::StickerReading;

/// Alarm state for LoRaWAN sensors (4-level, matches DS18B20)
#[derive(Debug, Clone, PartialEq)]
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
    /// Return the worse of two alarm states (for combining temp + humidity)
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

/// Evaluate a value against 4-level thresholds
fn evaluate_threshold(
    value: f32,
    critical_low: Option<f32>,
    warning_low: Option<f32>,
    warning_high: Option<f32>,
    critical_high: Option<f32>,
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

/// State for a single LoRaWAN sensor
#[derive(Debug, Clone)]
pub struct LoRaWANSensorState {
    pub dev_eui: String,
    pub name: String,
    pub serial_number: Option<String>,
    pub temperature: Option<f32>,
    pub humidity: Option<f32>,
    pub voltage: Option<f32>,
    pub ext_temperature_1: Option<f32>,
    pub ext_temperature_2: Option<f32>,
    pub illuminance: Option<u32>,
    pub motion_count: Option<u32>,
    pub orientation: Option<u8>,
    pub rssi: Option<i32>,
    pub snr: Option<f32>,
    pub last_seen: Option<String>,
    pub alarm_state: LoRaWANAlarmState,
    pub temp_alarm_state: LoRaWANAlarmState,
    pub humidity_alarm_state: LoRaWANAlarmState,
}

impl LoRaWANSensorState {
    /// Create a new sensor state from a reading
    pub fn from_reading(reading: &StickerReading) -> Self {
        Self {
            dev_eui: reading.dev_eui.clone(),
            name: reading.device_name.clone(),
            serial_number: None,
            temperature: reading.temperature,
            humidity: reading.humidity,
            voltage: reading.voltage,
            ext_temperature_1: reading.ext_temperature_1,
            ext_temperature_2: reading.ext_temperature_2,
            illuminance: reading.illuminance,
            motion_count: reading.motion_count,
            orientation: reading.orientation,
            rssi: reading.rssi,
            snr: reading.snr,
            last_seen: if reading.received_at.is_empty() {
                None
            } else {
                Some(reading.received_at.clone())
            },
            alarm_state: LoRaWANAlarmState::Normal,
            temp_alarm_state: LoRaWANAlarmState::Normal,
            humidity_alarm_state: LoRaWANAlarmState::Normal,
        }
    }

    /// Update from a new reading, preserving the name if the reading has an empty one
    pub fn update_from_reading(&mut self, reading: &StickerReading) {
        if !reading.device_name.is_empty() {
            self.name = reading.device_name.clone();
        }
        self.temperature = reading.temperature;
        self.humidity = reading.humidity;
        self.voltage = reading.voltage;
        self.ext_temperature_1 = reading.ext_temperature_1;
        self.ext_temperature_2 = reading.ext_temperature_2;
        self.illuminance = reading.illuminance;
        self.motion_count = reading.motion_count;
        self.orientation = reading.orientation;
        self.rssi = reading.rssi;
        self.snr = reading.snr;
        if !reading.received_at.is_empty() {
            self.last_seen = Some(reading.received_at.clone());
        }
        // Reset alarm states to Normal (will be re-evaluated by evaluate_alarms)
        self.temp_alarm_state = LoRaWANAlarmState::Normal;
        self.humidity_alarm_state = LoRaWANAlarmState::Normal;
        self.alarm_state = LoRaWANAlarmState::Normal;
    }

    /// Evaluate alarm thresholds for this sensor using its config
    pub fn evaluate_alarms(&mut self, config: Option<&LoRaWANSensorConfig>) {
        let config = match config {
            Some(c) => c,
            None => return, // No config = no thresholds = stay Normal
        };

        // Apply config overrides for name and serial_number
        if let Some(ref name) = config.name {
            self.name = name.clone();
        }
        self.serial_number = config.serial_number.clone();

        // Evaluate temperature thresholds
        if let Some(temp) = self.temperature {
            self.temp_alarm_state = evaluate_threshold(
                temp,
                config.temp_critical_low,
                config.temp_warning_low,
                config.temp_warning_high,
                config.temp_critical_high,
            );
        }

        // Evaluate humidity thresholds
        if let Some(hum) = self.humidity {
            self.humidity_alarm_state = evaluate_threshold(
                hum,
                config.humidity_critical_low,
                config.humidity_warning_low,
                config.humidity_warning_high,
                config.humidity_critical_high,
            );
        }

        // Overall alarm = worst of temp + humidity
        self.alarm_state = self.temp_alarm_state.worst(&self.humidity_alarm_state);
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

    /// Update or insert sensor state from a reading
    pub fn update_sensor(&mut self, reading: &StickerReading) {
        if let Some(sensor) = self.sensors.get_mut(&reading.dev_eui) {
            sensor.update_from_reading(reading);
        } else {
            self.sensors.insert(
                reading.dev_eui.clone(),
                LoRaWANSensorState::from_reading(reading),
            );
        }
    }

    /// Evaluate alarm thresholds for all sensors using config
    pub fn evaluate_alarms(&mut self, sensor_configs: &[LoRaWANSensorConfig]) {
        for sensor in self.sensors.values_mut() {
            let config = sensor_configs.iter().find(|c| c.dev_eui == sensor.dev_eui);
            sensor.evaluate_alarms(config);
        }
    }

    /// Mark sensors as disconnected if not seen within timeout
    pub fn check_timeouts(&mut self, timeout_secs: u64) {
        let now = chrono::Utc::now();
        for sensor in self.sensors.values_mut() {
            if let Some(ref last_seen) = sensor.last_seen {
                if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(last_seen) {
                    let elapsed = now.signed_duration_since(ts);
                    if elapsed.num_seconds() > timeout_secs as i64 {
                        sensor.alarm_state = LoRaWANAlarmState::Disconnected;
                        sensor.temp_alarm_state = LoRaWANAlarmState::Disconnected;
                        sensor.humidity_alarm_state = LoRaWANAlarmState::Disconnected;
                    }
                }
            }
        }
    }
}

/// Thread-safe shared LoRaWAN state
pub type SharedLoRaWANState = Arc<RwLock<LoRaWANState>>;

/// Create a new shared LoRaWAN state
pub fn create_shared_lorawan_state(gateway_present: bool) -> SharedLoRaWANState {
    Arc::new(RwLock::new(LoRaWANState::new(gateway_present)))
}

/// Thread-safe shared LoRaWAN sensor configs.
///
/// Acts as the live source of truth for LoRa sensor configuration (thresholds,
/// names, locations). Producers (MQTT config-applier) take a write lock to
/// mutate; consumers (LoRaWAN monitor for alarm evaluation, display for
/// rendering) take a read lock on each access.
pub type SharedLoRaWANSensorConfigs = Arc<RwLock<Vec<LoRaWANSensorConfig>>>;

/// Construct a shared configs handle, seeded with the given vec.
pub fn create_shared_lorawan_sensor_configs(
    seed: Vec<LoRaWANSensorConfig>,
) -> SharedLoRaWANSensorConfigs {
    Arc::new(RwLock::new(seed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_reading(dev_eui: &str, temp: f32) -> StickerReading {
        StickerReading {
            dev_eui: dev_eui.to_string(),
            device_name: "test-sensor".to_string(),
            temperature: Some(temp),
            humidity: Some(50.0),
            voltage: Some(3.1),
            ext_temperature_1: None,
            ext_temperature_2: None,
            illuminance: None,
            motion_count: None,
            orientation: None,
            boot: false,
            rssi: Some(-80),
            snr: Some(7.0),
            received_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    #[test]
    fn test_update_sensor() {
        let mut state = LoRaWANState::new(true);
        let reading = make_reading("aabb", 22.5);
        state.update_sensor(&reading);
        assert_eq!(state.sensors.len(), 1);
        assert_eq!(state.sensors["aabb"].temperature, Some(22.5));

        // Update existing
        let reading2 = make_reading("aabb", 23.0);
        state.update_sensor(&reading2);
        assert_eq!(state.sensors.len(), 1);
        assert_eq!(state.sensors["aabb"].temperature, Some(23.0));
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
    fn test_evaluate_alarms_with_thresholds() {
        let mut state = LoRaWANState::new(true);
        let reading = make_reading("aabb", 42.0); // High temp
        state.update_sensor(&reading);

        let configs = vec![LoRaWANSensorConfig {
            dev_eui: "aabb".to_string(),
            name: Some("Test Sensor".to_string()),
            serial_number: Some("SN-001".to_string()),
            location: None,
            enabled: true,
            temp_critical_low: Some(0.0),
            temp_warning_low: Some(10.0),
            temp_warning_high: Some(35.0),
            temp_critical_high: Some(40.0),
            humidity_critical_low: None,
            humidity_warning_low: None,
            humidity_warning_high: None,
            humidity_critical_high: None,
        }];

        state.evaluate_alarms(&configs);
        assert_eq!(state.sensors["aabb"].temp_alarm_state, LoRaWANAlarmState::Critical);
        assert_eq!(state.sensors["aabb"].alarm_state, LoRaWANAlarmState::Critical);
        assert_eq!(state.sensors["aabb"].name, "Test Sensor");
        assert_eq!(state.sensors["aabb"].serial_number, Some("SN-001".to_string()));
    }

    #[test]
    fn test_evaluate_alarms_no_config() {
        let mut state = LoRaWANState::new(true);
        let reading = make_reading("aabb", 42.0);
        state.update_sensor(&reading);

        // No configs = stays Normal
        state.evaluate_alarms(&[]);
        assert_eq!(state.sensors["aabb"].alarm_state, LoRaWANAlarmState::Normal);
    }

    #[test]
    fn test_evaluate_humidity_warning() {
        let mut state = LoRaWANState::new(true);
        let reading = make_reading("aabb", 25.0); // Normal temp
        state.update_sensor(&reading);

        let configs = vec![LoRaWANSensorConfig {
            dev_eui: "aabb".to_string(),
            name: None,
            serial_number: None,
            location: None,
            enabled: true,
            temp_critical_low: None,
            temp_warning_low: None,
            temp_warning_high: None,
            temp_critical_high: None,
            humidity_critical_low: Some(10.0),
            humidity_warning_low: Some(20.0),
            humidity_warning_high: Some(80.0),
            humidity_critical_high: Some(90.0),
        }];

        state.evaluate_alarms(&configs);
        // humidity is 50% from make_reading -> Normal
        assert_eq!(state.sensors["aabb"].humidity_alarm_state, LoRaWANAlarmState::Normal);
    }

    #[test]
    fn shared_lorawan_sensor_configs_round_trip() {
        let cfgs = create_shared_lorawan_sensor_configs(vec![
            LoRaWANSensorConfig {
                dev_eui: "aabb".to_string(),
                name: Some("A".to_string()),
                serial_number: None,
                location: None,
                enabled: true,
                temp_critical_low: Some(0.0),
                temp_warning_low: None,
                temp_warning_high: None,
                temp_critical_high: Some(40.0),
                humidity_critical_low: None,
                humidity_warning_low: None,
                humidity_warning_high: None,
                humidity_critical_high: None,
            },
        ]);
        {
            let read = cfgs.read().unwrap();
            assert_eq!(read.len(), 1);
            assert_eq!(read[0].dev_eui, "aabb");
        }
        {
            let mut write = cfgs.write().unwrap();
            write.clear();
        }
        assert_eq!(cfgs.read().unwrap().len(), 0);
    }
}
