// src/config.rs
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "lowercase")]
pub enum SensorKind {
    /// Uses SimulatedTemperatureBackend
    Simulated,
    /// DS18B20 via DS2482S-800+ I2C-to-1Wire bridge or discovery-based auto-detection
    Ds18b20,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SensorConfig {
    /// Logical ID (must match SensorId.0)
    pub id: u64,

    pub label: Option<String>,

    /// "simulated" or "onewire"
    pub kind: SensorKind,

    /// Index 0..7 of the per-line LED
    pub led_index: u8,

    // --- Simulated-specific fields ---
    pub base_c: Option<f32>,
    pub amplitude_c: Option<f32>,
    pub period_s: Option<f32>,

    // --- OneWire-specific fields ---
    /// 8-byte ROM code, e.g. "28-00-00-00-ab-cd-ef-00"
    /// Used by both Onewire and Ds18b20 sensor kinds
    pub rom: Option<String>,
    /// Optional root path for OneWire sysfs; default: /sys/bus/w1/devices
    pub root: Option<String>,

    // --- DS18B20 via DS2482-specific fields ---
    /// IO pin on DS2482S-800+ bridge (0-7)
    pub io_pin: Option<u8>,
    /// Optional I2C device path; default: /dev/i2c-1
    /// Shared across all ds18b20 sensors
    pub i2c_path: Option<String>,
    /// Optional I2C address (7-bit); default: 0x18
    /// Shared across all ds18b20 sensors
    pub i2c_address: Option<u16>,

    /// Temperature calibration offset in Celsius
    pub calibration_offset: Option<f32>,

    // --- Alarm thresholds ---
    pub warning_low: Option<f32>,
    pub warning_high: Option<f32>,
    pub critical_low: Option<f32>,
    pub critical_high: Option<f32>,
    pub hysteresis: Option<f32>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AppConfig {
    pub sensors: Vec<SensorConfig>,
}

impl AppConfig {
    /// Load config from a YAML file. If the file does not exist, use
    /// a built-in default with 2 simulated sensors.
    pub fn load_from(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if path.exists() {
            let text = fs::read_to_string(path)?;
            let cfg: AppConfig = serde_yaml::from_str(&text)?;
            Ok(cfg)
        } else {
            Ok(Self::default_simulated_demo())
        }
    }

    /// Default configuration: 2 simulated sensors mapped to LED 0 and 1.
    fn default_simulated_demo() -> Self {
        AppConfig {
            sensors: vec![
                SensorConfig {
                    id: 1,
                    label: Some("Sensor 1".to_string()),
                    kind: SensorKind::Simulated,
                    led_index: 0,
                    base_c: Some(4.0),
                    amplitude_c: Some(2.0),
                    period_s: Some(300.0),
                    rom: None,
                    root: None,
                    io_pin: None,
                    i2c_path: None,
                    i2c_address: None,
                    calibration_offset: Some(0.0),
                    warning_low: Some(2.0),
                    warning_high: Some(8.0),
                    critical_low: Some(0.0),
                    critical_high: Some(10.0),
                    hysteresis: Some(0.5),
                },
                SensorConfig {
                    id: 2,
                    label: Some("Sensor 2".to_string()),
                    kind: SensorKind::Simulated,
                    led_index: 1,
                    base_c: Some(6.0),
                    amplitude_c: Some(3.0),
                    period_s: Some(240.0),
                    rom: None,
                    root: None,
                    io_pin: None,
                    i2c_path: None,
                    i2c_address: None,
                    calibration_offset: Some(0.0),
                    warning_low: Some(3.0),
                    warning_high: Some(9.0),
                    critical_low: Some(1.0),
                    critical_high: Some(11.0),
                    hysteresis: Some(0.5),
                },
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_simulated_demo_has_two_sensors() {
        let cfg = AppConfig::default_simulated_demo();
        assert_eq!(cfg.sensors.len(), 2);
        assert_eq!(cfg.sensors[0].id, 1);
        assert_eq!(cfg.sensors[1].led_index, 1);
    }

    #[test]
    fn parse_simple_yaml() {
        let yaml = r#"
sensors:
  - id: 10
    kind: simulated
    led_index: 3
    base_c: 5.0
    amplitude_c: 1.0
    period_s: 120.0
    warning_low: 3.0
    warning_high: 7.0
    critical_low: 1.0
    critical_high: 9.0
"#;
        let cfg: AppConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.sensors.len(), 1);
        let s0 = &cfg.sensors[0];
        assert_eq!(s0.id, 10);
        assert_eq!(s0.led_index, 3);
        assert!(matches!(s0.kind, SensorKind::Simulated));
    }

    #[test]
    fn parse_ds18b20_yaml() {
        let yaml = r#"
sensors:
  - id: 20
    label: "Freezer"
    kind: ds18b20
    led_index: 2
    rom: "28-00-00-00-ab-cd-ef-00"
    io_pin: 0
    i2c_path: /dev/i2c-1
    i2c_address: 0x18
    calibration_offset: 0.5
    warning_low: -25.0
    warning_high: -15.0
    critical_low: -30.0
    critical_high: -10.0
"#;
        let cfg: AppConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.sensors.len(), 1);
        let s0 = &cfg.sensors[0];
        assert_eq!(s0.id, 20);
        assert_eq!(s0.led_index, 2);
        assert!(matches!(s0.kind, SensorKind::Ds18b20));
        assert_eq!(s0.rom, Some("28-00-00-00-ab-cd-ef-00".to_string()));
        assert_eq!(s0.io_pin, Some(0));
        assert_eq!(s0.calibration_offset, Some(0.5));
    }
}
