//! Shared LoRaWAN state management
//!
//! Thread-safe state for LoRaWAN gateway and sensor data.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use super::chirpstack::StickerReading;

/// Alarm state for LoRaWAN sensors (simplified, based on timeout)
#[derive(Debug, Clone, PartialEq)]
pub enum LoRaWANAlarmState {
    Normal,
    Disconnected,
}

impl std::fmt::Display for LoRaWANAlarmState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoRaWANAlarmState::Normal => write!(f, "NORMAL"),
            LoRaWANAlarmState::Disconnected => write!(f, "DISCONNECTED"),
        }
    }
}

/// State for a single LoRaWAN sensor
#[derive(Debug, Clone)]
pub struct LoRaWANSensorState {
    pub dev_eui: String,
    pub name: String,
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
}

impl LoRaWANSensorState {
    /// Create a new sensor state from a reading
    pub fn from_reading(reading: &StickerReading) -> Self {
        Self {
            dev_eui: reading.dev_eui.clone(),
            name: reading.device_name.clone(),
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
        self.alarm_state = LoRaWANAlarmState::Normal;
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

    /// Mark sensors as disconnected if not seen within timeout
    pub fn check_timeouts(&mut self, timeout_secs: u64) {
        let now = chrono::Utc::now();
        for sensor in self.sensors.values_mut() {
            if let Some(ref last_seen) = sensor.last_seen {
                if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(last_seen) {
                    let elapsed = now.signed_duration_since(ts);
                    if elapsed.num_seconds() > timeout_secs as i64 {
                        sensor.alarm_state = LoRaWANAlarmState::Disconnected;
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
}
