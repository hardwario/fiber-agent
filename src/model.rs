// src/model.rs
use chrono::{DateTime, Utc};
use std::hash::Hash;

/// Unique identifier for a sensor (e.g. DS18B20 64-bit ROM code,
/// or logical ID for VIN/VBAT, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SensorId(pub u64);

/// Quality of a sensor reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadingQuality {
    Ok,
    CrcError,
    Timeout,
    Disconnected,
    Other,
}

/// Generic time-series reading from a sensor.
///
/// For temperature sensors this is degrees Celsius,
/// but the engine itself doesn’t care – just a scalar value.
#[derive(Debug, Clone, Copy)]
pub struct SensorReading {
    pub sensor_id: SensorId,
    pub ts_utc: DateTime<Utc>,
    pub value: f32,
    pub quality: ReadingQuality,
}
