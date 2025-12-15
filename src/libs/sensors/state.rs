//! Sensor state management for shared temperature and connectivity data
//!
//! Provides a thread-safe way to share sensor readings between monitors.
//! Used by SensorMonitor to publish readings and DisplayMonitor to read them.

use std::sync::{Arc, RwLock};

use crate::libs::alarms::AlarmState;

/// Single sensor reading with temperature and connection status
#[derive(Clone, Debug)]
pub struct SensorReading {
    /// Current temperature in Celsius
    pub temperature: f32,
    /// Whether sensor is currently connected
    pub is_connected: bool,
    /// Connection history state (NeverConnected, Disconnected, Normal, etc.)
    pub alarm_state: AlarmState,
}

impl SensorReading {
    pub fn new(temperature: f32, is_connected: bool, alarm_state: AlarmState) -> Self {
        Self {
            temperature,
            is_connected,
            alarm_state,
        }
    }
}

/// Shared sensor state for all 8 sensors
///
/// This structure holds the current readings for all sensors and is
/// updated by SensorMonitor and read by DisplayMonitor.
#[derive(Debug)]
pub struct SharedSensorState {
    /// Array of sensor readings (8 sensors)
    pub readings: [Option<SensorReading>; 8],
    /// Sensor names from config (hot-reloaded)
    pub names: [String; 8],
}

impl SharedSensorState {
    /// Create a new sensor state with all sensors initially disconnected
    pub fn new() -> Self {
        Self {
            readings: [None, None, None, None, None, None, None, None],
            names: [
                "Sensor 1".to_string(),
                "Sensor 2".to_string(),
                "Sensor 3".to_string(),
                "Sensor 4".to_string(),
                "Sensor 5".to_string(),
                "Sensor 6".to_string(),
                "Sensor 7".to_string(),
                "Sensor 8".to_string(),
            ],
        }
    }

    /// Update sensor names from config
    pub fn set_names(&mut self, names: [String; 8]) {
        self.names = names;
    }

    /// Get sensor name (with fallback to default)
    pub fn get_name(&self, sensor_idx: u8) -> &str {
        if (sensor_idx as usize) < 8 {
            &self.names[sensor_idx as usize]
        } else {
            "Unknown"
        }
    }

    /// Update a sensor reading
    pub fn set_reading(&mut self, sensor_idx: u8, reading: SensorReading) {
        if (sensor_idx as usize) < 8 {
            self.readings[sensor_idx as usize] = Some(reading);
        }
    }

    /// Get a sensor reading
    pub fn get_reading(&self, sensor_idx: u8) -> Option<SensorReading> {
        if (sensor_idx as usize) < 8 {
            self.readings[sensor_idx as usize].clone()
        } else {
            None
        }
    }
}

impl Default for SharedSensorState {
    fn default() -> Self {
        Self::new()
    }
}

/// Type alias for shared sensor state handle
///
/// Uses RwLock for multiple concurrent readers (DisplayMonitor, etc.)
/// and exclusive access for writers (SensorMonitor).
pub type SharedSensorStateHandle = Arc<RwLock<SharedSensorState>>;

/// Create a new shared sensor state handle
pub fn create_shared_sensor_state() -> SharedSensorStateHandle {
    Arc::new(RwLock::new(SharedSensorState::new()))
}
