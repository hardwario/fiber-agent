// Configuration management for FIBER Medical Thermometer
// Loads and provides access to configuration from fiber.config.yaml

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use serde::{Deserialize, Serialize};
use crate::libs::alarms::AlarmThreshold;

/// LED color configuration for alarm patterns
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AlarmLedColor {
    Green,
    Red,
    Yellow,
    Off,
}

/// LED blink pattern configuration for alarm patterns
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AlarmLedBlink {
    Steady,
    Slow,   // 4 cycles
    Fast,   // 1 cycle
}

/// Buzzer timing configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuzzerTiming {
    /// Duration buzzer is ON (milliseconds)
    pub on_ms: u64,
    /// Duration buzzer is OFF (milliseconds)
    pub off_ms: u64,
}

impl Default for BuzzerTiming {
    fn default() -> Self {
        Self {
            on_ms: 100,
            off_ms: 100,
        }
    }
}

impl BuzzerTiming {
    /// Validate buzzer timing values
    pub fn validate(&self) -> Result<(), String> {
        if self.on_ms == 0 {
            return Err("buzzer on_ms must be > 0".to_string());
        }
        if self.off_ms == 0 {
            return Err("buzzer off_ms must be > 0".to_string());
        }
        let cycle_duration = self.on_ms + self.off_ms;
        if cycle_duration > 60000 {
            return Err(format!(
                "buzzer cycle duration {}ms exceeds maximum 60000ms",
                cycle_duration
            ));
        }
        Ok(())
    }
}

/// Buzzer pattern configuration for alarm patterns
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AlarmBuzzerPattern {
    None,
    #[serde(rename = "disconnected")]
    Disconnected,  // Configurable timing
    #[serde(rename = "critical")]
    Critical,      // Configurable timing
}

/// Alarm state behavior configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlarmStatePattern {
    pub led_color: AlarmLedColor,
    pub led_blink: AlarmLedBlink,
    pub buzzer_enabled: bool,
    pub buzzer_pattern: AlarmBuzzerPattern,
}

/// Complete alarm patterns configuration for all states
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlarmPatternsConfig {
    pub warning: AlarmStatePattern,
    #[serde(default = "default_alarm_pattern")]
    pub alarm: AlarmStatePattern,
    pub critical: AlarmStatePattern,
    pub disconnected: AlarmStatePattern,

    /// Buzzer timing for critical pattern (on_ms, off_ms)
    #[serde(default)]
    pub buzzer_critical_timing: BuzzerTiming,

    /// Buzzer timing for disconnected pattern (on_ms, off_ms)
    #[serde(default)]
    pub buzzer_disconnected_timing: BuzzerTiming,
}

/// Power management configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerConfig {
    /// Power status update interval in milliseconds
    pub update_interval_ms: u64,

    /// Battery configuration
    pub battery: BatteryConfig,

    /// AC power detection configuration
    pub ac_power: AcPowerConfig,

    /// LED blinking configuration
    pub led_blink: LedBlinkConfig,
}

/// Battery voltage and state configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatteryConfig {
    /// Minimum voltage for 0% battery (mV)
    pub min_voltage_mv: u16,

    /// Maximum voltage for 100% battery (mV)
    pub max_voltage_mv: u16,

    /// Low battery threshold (percentage)
    pub low_threshold_percent: u8,

    /// Critical battery threshold (percentage)
    pub critical_threshold_percent: u8,
}

/// AC power detection configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcPowerConfig {
    /// Voltage above which system considers AC power connected (mV)
    pub detection_threshold_mv: u16,

    /// Voltage below which system is in battery mode (mV)
    pub battery_mode_threshold_mv: u16,
}

/// LED blinking configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedBlinkConfig {
    /// Toggle blink every N update cycles
    pub toggle_count: u32,
}

/// Temperature alarm thresholds configuration (alias for AlarmThreshold from alarms library)
pub type SensorAlarmConfig = AlarmThreshold;

/// Per-line sensor configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorLineConfig {
    /// Sensor line number (0-7)
    pub line: u8,

    /// Enable this sensor line
    pub enabled: bool,

    /// Sensor name/label
    pub name: String,

    /// Sensor probe location (e.g., "Cold room A, shelf 3")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,

    /// Critical low temperature override (None = use common)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub critical_low_celsius: Option<f32>,

    /// Low temperature alarm override (None = use common)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low_alarm_celsius: Option<f32>,

    /// Warning low temperature override (None = use common)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning_low_celsius: Option<f32>,

    /// Warning high temperature override (None = use common)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning_high_celsius: Option<f32>,

    /// High temperature alarm override (None = use common)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high_alarm_celsius: Option<f32>,

    /// Critical high temperature override (None = use common)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub critical_high_celsius: Option<f32>,

    /// Per-sensor reporting interval override (None = use global)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_interval_ms: Option<u64>,
}

/// Sensor file configuration (from fiber.sensors.config.yaml)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorFileConfig {
    /// Alarm pattern configurations for each state
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alarm_patterns: Option<AlarmPatternsConfig>,

    /// Common (default) alarm thresholds for all lines
    pub common_alarms: SensorAlarmConfig,

    /// Default per-field thresholds for LoRaWAN stickers, keyed by field name
    /// (matches `registry::REGISTRY`). Used as a fallback for every sticker that
    /// has no explicit override, on a per-bound basis. Empty map disables
    /// auto-alarming for stickers.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub common_lorawan_field_thresholds: HashMap<String, FieldThresholdBounds>,

    /// Per-line sensor configurations
    pub lines: Vec<SensorLineConfig>,
}

impl SensorFileConfig {
    /// Load sensor configuration from a YAML file
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let content = fs::read_to_string(path)?;
        let config: SensorFileConfig = serde_yaml::from_str(&content)?;
        Ok(config)
    }

    /// Load sensor configuration from default location (/data/fiber/config/fiber.sensors.config.yaml)
    pub fn load_default() -> Result<Self, Box<dyn std::error::Error>> {
        let config = Self::from_file("/data/fiber/config/fiber.sensors.config.yaml")?;

        // Validate buzzer timings
        if let Some(patterns) = &config.alarm_patterns {
            patterns.buzzer_critical_timing.validate()?;
            patterns.buzzer_disconnected_timing.validate()?;
        }

        Ok(config)
    }

    /// Get effective alarm thresholds for a specific sensor line
    /// Merges line-specific overrides with common defaults
    pub fn get_line_thresholds(&self, line: u8) -> SensorAlarmConfig {
        let line_cfg = self.lines.iter().find(|l| l.line == line);

        match line_cfg {
            Some(cfg) => AlarmThreshold {
                critical_low_celsius: cfg
                    .critical_low_celsius
                    .unwrap_or(self.common_alarms.critical_low_celsius),
                low_alarm_celsius: cfg
                    .low_alarm_celsius
                    .unwrap_or(self.common_alarms.low_alarm_celsius),
                warning_low_celsius: cfg
                    .warning_low_celsius
                    .unwrap_or(self.common_alarms.warning_low_celsius),
                warning_high_celsius: cfg
                    .warning_high_celsius
                    .unwrap_or(self.common_alarms.warning_high_celsius),
                high_alarm_celsius: cfg
                    .high_alarm_celsius
                    .unwrap_or(self.common_alarms.high_alarm_celsius),
                critical_high_celsius: cfg
                    .critical_high_celsius
                    .unwrap_or(self.common_alarms.critical_high_celsius),
            },
            None => self.common_alarms,
        }
    }

    /// Get default alarm patterns
    fn default_alarm_patterns() -> AlarmPatternsConfig {
        AlarmPatternsConfig {
            warning: AlarmStatePattern {
                led_color: AlarmLedColor::Yellow,
                led_blink: AlarmLedBlink::Slow,
                buzzer_enabled: false,
                buzzer_pattern: AlarmBuzzerPattern::None,
            },
            alarm: AlarmStatePattern {
                led_color: AlarmLedColor::Red,
                led_blink: AlarmLedBlink::Steady,
                buzzer_enabled: false,
                buzzer_pattern: AlarmBuzzerPattern::None,
            },
            critical: AlarmStatePattern {
                led_color: AlarmLedColor::Red,
                led_blink: AlarmLedBlink::Fast,
                buzzer_enabled: true,
                buzzer_pattern: AlarmBuzzerPattern::Critical,
            },
            disconnected: AlarmStatePattern {
                led_color: AlarmLedColor::Red,
                led_blink: AlarmLedBlink::Slow,
                buzzer_enabled: true,
                buzzer_pattern: AlarmBuzzerPattern::Disconnected,
            },
            // Default buzzer timings (200ms on, 100ms off for critical; 100ms on/off for disconnected)
            buzzer_critical_timing: BuzzerTiming {
                on_ms: 200,
                off_ms: 100,
            },
            buzzer_disconnected_timing: BuzzerTiming {
                on_ms: 100,
                off_ms: 100,
            },
        }
    }

    /// Get default sensor configuration for testing or when file is missing
    pub fn default_config() -> Self {
        Self {
            alarm_patterns: Some(Self::default_alarm_patterns()),
            common_alarms: AlarmThreshold {
                critical_low_celsius: 32.0,
                low_alarm_celsius: 0.0,    // disabled - defaults
                warning_low_celsius: 35.0,
                warning_high_celsius: 38.0,
                high_alarm_celsius: 100.0, // disabled - defaults
                critical_high_celsius: 40.0,
            },
            common_lorawan_field_thresholds: HashMap::new(),
            lines: vec![
                SensorLineConfig {
                    line: 0,
                    enabled: true,
                    name: "Sensor 1".to_string(),
                    critical_low_celsius: None,
                    low_alarm_celsius: None,
                    warning_low_celsius: None,
                    warning_high_celsius: None,
                    high_alarm_celsius: None,
                    critical_high_celsius: None,
                    report_interval_ms: None,
                    location: None,
                },
                SensorLineConfig {
                    line: 1,
                    enabled: true,
                    name: "Sensor 2".to_string(),
                    critical_low_celsius: None,
                    low_alarm_celsius: None,
                    warning_low_celsius: None,
                    warning_high_celsius: None,
                    high_alarm_celsius: None,
                    critical_high_celsius: None,
                    report_interval_ms: None,
                    location: None,
                },
                SensorLineConfig {
                    line: 2,
                    enabled: true,
                    name: "Sensor 3".to_string(),
                    critical_low_celsius: None,
                    low_alarm_celsius: None,
                    warning_low_celsius: None,
                    warning_high_celsius: None,
                    high_alarm_celsius: None,
                    critical_high_celsius: None,
                    report_interval_ms: None,
                    location: None,
                },
                SensorLineConfig {
                    line: 3,
                    enabled: true,
                    name: "Sensor 4".to_string(),
                    critical_low_celsius: None,
                    low_alarm_celsius: None,
                    warning_low_celsius: None,
                    warning_high_celsius: None,
                    high_alarm_celsius: None,
                    critical_high_celsius: None,
                    report_interval_ms: None,
                    location: None,
                },
                SensorLineConfig {
                    line: 4,
                    enabled: true,
                    name: "Sensor 5".to_string(),
                    critical_low_celsius: None,
                    low_alarm_celsius: None,
                    warning_low_celsius: None,
                    warning_high_celsius: None,
                    high_alarm_celsius: None,
                    critical_high_celsius: None,
                    report_interval_ms: None,
                    location: None,
                },
                SensorLineConfig {
                    line: 5,
                    enabled: true,
                    name: "Sensor 6".to_string(),
                    critical_low_celsius: None,
                    low_alarm_celsius: None,
                    warning_low_celsius: None,
                    warning_high_celsius: None,
                    high_alarm_celsius: None,
                    critical_high_celsius: None,
                    report_interval_ms: None,
                    location: None,
                },
                SensorLineConfig {
                    line: 6,
                    enabled: true,
                    name: "Sensor 7".to_string(),
                    critical_low_celsius: None,
                    low_alarm_celsius: None,
                    warning_low_celsius: None,
                    warning_high_celsius: None,
                    high_alarm_celsius: None,
                    critical_high_celsius: None,
                    report_interval_ms: None,
                    location: None,
                },
                SensorLineConfig {
                    line: 7,
                    enabled: true,
                    name: "Sensor 8".to_string(),
                    critical_low_celsius: None,
                    low_alarm_celsius: None,
                    warning_low_celsius: None,
                    warning_high_celsius: None,
                    high_alarm_celsius: None,
                    critical_high_celsius: None,
                    report_interval_ms: None,
                    location: None,
                },
            ],
        }
    }
}

/// Temperature sensor configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorConfig {
    /// Number of W1 sensor lines (0-7 for 8 lines)
    pub num_lines: u8,

    /// Sensor sampling interval in milliseconds
    pub sample_interval_ms: u64,

    /// Sensor aggregation interval in milliseconds
    pub aggregation_interval_ms: u64,

    /// Sensor report interval in milliseconds
    pub report_interval_ms: u64,

    /// Number of consecutive failed reads before marking sensor as offline
    pub failure_threshold: u8,

    /// Number of consecutive successful reads before a sensor exits NeverConnected state.
    /// Prevents false alarms from lucky single reads during OneWire bus stabilization at boot.
    #[serde(default = "default_warmup_threshold")]
    pub warmup_threshold: u8,
}

/// Serial communication configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerialConfig {
    /// STM32 serial port device path
    pub port: String,

    /// Baud rate
    pub baud_rate: u32,
}

/// Accelerometer motion detection configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccelerometerConfig {
    /// Enable motion detection
    pub enabled: bool,

    /// I2C device path
    pub i2c_path: String,

    /// Update interval in milliseconds
    pub update_interval_ms: u64,

    /// Motion detection threshold in gravitational units (g)
    pub motion_threshold_g: f32,

    /// Number of consecutive samples above threshold to confirm motion
    pub debounce_samples: u8,

    /// Enable logging of motion events
    pub logging_enabled: bool,
}

/// System-wide configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfig {
    /// Enable debug logging
    pub debug_mode: bool,

    /// Application name
    pub app_name: String,

    /// Application version
    pub app_version: String,

    /// Timezone offset from UTC (hours)
    pub timezone_offset_hours: i8,

    /// Device label (user-friendly name, defaults to hostname)
    #[serde(default)]
    pub device_label: Option<String>,

    /// LED brightness percentage (0-100), persisted across reboots
    #[serde(default = "default_led_brightness")]
    pub led_brightness: u8,

    /// Screen brightness percentage (0-100), persisted across reboots
    #[serde(default = "default_screen_brightness")]
    pub screen_brightness: u8,

    /// Buzzer volume percentage (0 = muted, 1-100 = active). Default 100.
    #[serde(default = "default_buzzer_volume")]
    pub buzzer_volume: u8,
}

/// Medical data storage configuration (EU MDR 2017/745 compliance)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Path to SQLite database file
    pub db_path: String,

    /// Maximum database size in gigabytes
    pub max_size_gb: i32,

    /// Path to HMAC secret key file for sensor reading integrity (EU MDR)
    #[serde(default = "default_hmac_secret_path")]
    pub hmac_secret_path: String,
}

/// Per-field threshold (4-level, like DS18B20)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FieldThreshold {
    /// Field name from the LoRaWAN field registry (e.g., "temperature")
    pub field: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub critical_low: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning_low: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning_high: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub critical_high: Option<f64>,
}

/// Bounds-only form of a field threshold, used for the YAML defaults map
/// where the field name is the map key.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FieldThresholdBounds {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub critical_low: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning_low: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning_high: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub critical_high: Option<f64>,
}

impl FieldThresholdBounds {
    /// True if any bound is set.
    pub fn is_set(&self) -> bool {
        self.critical_low.is_some()
            || self.warning_low.is_some()
            || self.warning_high.is_some()
            || self.critical_high.is_some()
    }
}

/// Resolve the effective threshold for a single field on a sticker, merging the
/// per-sensor override (if present) over the YAML defaults (if present) on a
/// per-bound basis. Returns `None` only when neither side provides any bound.
pub fn resolve_field_threshold(
    field: &str,
    sensor_override: Option<&FieldThreshold>,
    default: Option<&FieldThresholdBounds>,
) -> Option<FieldThreshold> {
    match (sensor_override, default) {
        (None, None) => None,
        (Some(o), None) => Some(o.clone()),
        (None, Some(d)) if d.is_set() => Some(FieldThreshold {
            field: field.to_string(),
            critical_low: d.critical_low,
            warning_low: d.warning_low,
            warning_high: d.warning_high,
            critical_high: d.critical_high,
        }),
        (None, Some(_)) => None,
        (Some(o), Some(d)) => {
            let merged = FieldThreshold {
                field: field.to_string(),
                critical_low: o.critical_low.or(d.critical_low),
                warning_low: o.warning_low.or(d.warning_low),
                warning_high: o.warning_high.or(d.warning_high),
                critical_high: o.critical_high.or(d.critical_high),
            };
            if merged.critical_low.is_none()
                && merged.warning_low.is_none()
                && merged.warning_high.is_none()
                && merged.critical_high.is_none()
            {
                None
            } else {
                Some(merged)
            }
        }
    }
}

/// Compute the full list of effective thresholds for a sticker, considering
/// both the per-sensor `field_thresholds` and the YAML defaults map. Every
/// field that has at least one bound from either source is included.
pub fn effective_field_thresholds(
    sensor: Option<&LoRaWANSensorConfig>,
    defaults: &HashMap<String, FieldThresholdBounds>,
) -> Vec<FieldThreshold> {
    let mut fields: std::collections::BTreeSet<String> = defaults.keys().cloned().collect();
    if let Some(s) = sensor {
        for t in &s.field_thresholds {
            fields.insert(t.field.clone());
        }
    }
    fields
        .into_iter()
        .filter_map(|f| {
            let override_ = sensor.and_then(|s| s.field_thresholds.iter().find(|t| t.field == f));
            let default = defaults.get(&f);
            resolve_field_threshold(&f, override_, default)
        })
        .collect()
}

/// Per-sensor LoRaWAN configuration (generic field-driven thresholds)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoRaWANSensorConfig {
    /// Device EUI (unique identifier)
    pub dev_eui: String,

    /// Sensor name/label override
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Serial number (user-assigned)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial_number: Option<String>,

    /// Sensor location (e.g., "Cold room A, shelf 3")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,

    /// Enable this sensor
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Per-field thresholds (replaces temp_*/humidity_* fixed columns)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub field_thresholds: Vec<FieldThreshold>,
}

/// LoRaWAN gateway configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoRaWANConfig {
    /// Enable LoRaWAN gateway integration
    #[serde(default)]
    pub enabled: bool,

    /// ChirpStack local MQTT broker host
    #[serde(default = "default_chirpstack_mqtt_host")]
    pub chirpstack_mqtt_host: String,

    /// ChirpStack local MQTT broker port
    #[serde(default = "default_chirpstack_mqtt_port")]
    pub chirpstack_mqtt_port: u16,

    /// ChirpStack local MQTT broker username (optional)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chirpstack_mqtt_username: Option<String>,

    /// ChirpStack local MQTT broker password (optional)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chirpstack_mqtt_password: Option<String>,

    /// Publish interval for LoRaWAN sensor data (seconds)
    #[serde(default = "default_lorawan_publish_interval")]
    pub publish_interval_s: u64,

    /// Sensor timeout in seconds (mark as disconnected after this)
    #[serde(default = "default_lorawan_sensor_timeout")]
    pub sensor_timeout_s: u64,

    /// Per-sensor configurations
    #[serde(default)]
    pub sensors: Vec<LoRaWANSensorConfig>,
}

impl Default for LoRaWANConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            chirpstack_mqtt_host: "localhost".to_string(),
            chirpstack_mqtt_port: 1883,
            chirpstack_mqtt_username: None,
            chirpstack_mqtt_password: None,
            publish_interval_s: 30,
            sensor_timeout_s: 3600, // 1 hour
            sensors: Vec::new(),
        }
    }
}

fn default_chirpstack_mqtt_host() -> String { "localhost".to_string() }
fn default_chirpstack_mqtt_port() -> u16 { 1883 }
fn default_lorawan_publish_interval() -> u64 { 30 }
fn default_lorawan_sensor_timeout() -> u64 { 3600 }

/// MQTT broker configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerConfig {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub client_id: String,  // Empty = use hostname
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

/// TLS/SSL configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub ca_cert_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_cert_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_key_path: Option<String>,
    #[serde(default)]
    pub insecure_skip_verify: bool,
}

impl TlsConfig {
    /// Validate TLS configuration for production safety.
    /// Blocks insecure_skip_verify in non-dev builds (EU MDR Annex I, 17.2).
    pub fn validate(&mut self) {
        #[cfg(not(feature = "dev-platform"))]
        if self.insecure_skip_verify {
            eprintln!("SECURITY: insecure_skip_verify is not allowed in production builds. Forcing to false.");
            self.insecure_skip_verify = false;
        }
    }
}

/// QoS overrides by message type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QosOverrides {
    #[serde(default)]
    pub sensor_readings: u8,
    #[serde(default = "default_qos_1")]
    pub power_status: u8,
    #[serde(default = "default_qos_2")]
    pub alarm_events: u8,
    #[serde(default = "default_qos_2")]
    pub power_events: u8,
    #[serde(default)]
    pub network_status: u8,
}

/// Publishing intervals configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishIntervals {
    #[serde(default = "default_sensor_interval")]
    pub sensors_sec: u64,
    #[serde(default = "default_power_interval")]
    pub power_sec: u64,
    #[serde(default = "default_network_interval")]
    pub network_sec: u64,
    #[serde(default = "default_system_interval")]
    pub system_info_sec: u64,
}

/// Publishing configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishConfig {
    pub topic_prefix: String,
    #[serde(default = "default_true")]
    pub include_hostname: bool,
    #[serde(default)]
    pub default_qos: u8,
    pub qos_overrides: QosOverrides,
    pub intervals: PublishIntervals,
    #[serde(default = "default_queue_size")]
    pub max_queue_size: usize,
}

/// Subscription configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_rate_limit")]
    pub max_commands_per_second: u32,
    #[serde(default = "default_true")]
    pub audit_enabled: bool,
}

/// Connection behavior configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionConfig {
    #[serde(default = "default_keepalive")]
    pub keep_alive_sec: u64,
    #[serde(default = "default_timeout")]
    pub connection_timeout_sec: u64,
    #[serde(default)]
    pub max_reconnect_attempts: u32,  // 0 = infinite
    #[serde(default = "default_reconnect_delay")]
    pub reconnect_delay_sec: u64,
    #[serde(default = "default_max_reconnect_delay")]
    pub max_reconnect_delay_sec: u64,
    #[serde(default = "default_true")]
    pub clean_session: bool,
}

/// Last Will and Testament configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastWillConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_lwt_topic")]
    pub topic: String,
    #[serde(default = "default_lwt_payload")]
    pub payload: String,
    #[serde(default = "default_qos_1")]
    pub qos: u8,
    #[serde(default = "default_true")]
    pub retain: bool,
}

/// MQTT client configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MqttConfig {
    /// Enable MQTT functionality
    pub enabled: bool,

    /// MQTT broker configuration
    pub broker: BrokerConfig,

    /// TLS/SSL configuration (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsConfig>,

    /// Publishing configuration
    pub publish: PublishConfig,

    /// Subscription configuration
    pub subscribe: SubscribeConfig,

    /// Connection behavior
    pub connection: ConnectionConfig,

    /// Last Will and Testament
    pub last_will: LastWillConfig,
}

// Default alarm pattern for backward compatibility (alarm level disabled)
fn default_alarm_pattern() -> AlarmStatePattern {
    AlarmStatePattern {
        led_color: AlarmLedColor::Red,
        led_blink: AlarmLedBlink::Steady,
        buzzer_enabled: false,
        buzzer_pattern: AlarmBuzzerPattern::None,
    }
}

// Default value functions for storage configuration
fn default_hmac_secret_path() -> String {
    "/data/fiber/config/hmac.key".to_string()
}

// Default value functions for sensor configuration
fn default_warmup_threshold() -> u8 { 3 }

// Default value functions for system configuration
fn default_led_brightness() -> u8 { 50 }
fn default_screen_brightness() -> u8 { 100 }
fn default_buzzer_volume() -> u8 { 100 }

// Default value functions for MQTT configuration
fn default_true() -> bool { true }
fn default_qos_1() -> u8 { 1 }
fn default_qos_2() -> u8 { 2 }
fn default_queue_size() -> usize { 10000 }
fn default_sensor_interval() -> u64 { 5 }
fn default_power_interval() -> u64 { 10 }
fn default_network_interval() -> u64 { 30 }
fn default_system_interval() -> u64 { 60 }
fn default_rate_limit() -> u32 { 10 }
fn default_keepalive() -> u64 { 60 }
fn default_timeout() -> u64 { 30 }
fn default_reconnect_delay() -> u64 { 1 }
fn default_max_reconnect_delay() -> u64 { 30 }
fn default_lwt_topic() -> String { "status".to_string() }
fn default_lwt_payload() -> String { r#"{"status":"offline"}"#.to_string() }

/// Complete application configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Power management settings
    pub power: PowerConfig,

    /// Temperature sensor settings
    pub sensors: SensorConfig,

    /// Serial communication settings
    pub serial: SerialConfig,

    /// Accelerometer motion detection settings
    pub accelerometer: AccelerometerConfig,

    /// Medical data storage settings (EU MDR 2017/745)
    pub storage: StorageConfig,

    /// System-wide settings
    pub system: SystemConfig,

    /// MQTT client settings
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mqtt: Option<MqttConfig>,

    /// LoRaWAN gateway settings
    #[serde(default)]
    pub lorawan: Option<LoRaWANConfig>,

    /// BLE GATT server settings
    #[serde(default)]
    pub ble: crate::libs::ble::BleConfig,
}

impl Config {
    /// Load configuration from a YAML file
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let content = fs::read_to_string(path)?;
        let mut config: Config = serde_yaml::from_str(&content)?;

        // Validate TLS config for production safety (EU MDR Annex I, 17.2)
        if let Some(ref mut mqtt) = config.mqtt {
            if let Some(ref mut tls) = mqtt.tls {
                tls.validate();
            }
        }

        Ok(config)
    }

    /// Load configuration from default location (/data/fiber/config/fiber.config.yaml)
    pub fn load_default() -> Result<Self, Box<dyn std::error::Error>> {
        Self::from_file("/data/fiber/config/fiber.config.yaml")
    }

    /// Get a default configuration (for testing or when config file is missing)
    pub fn default_config() -> Self {
        Self {
            power: PowerConfig {
                update_interval_ms: 500,
                battery: BatteryConfig {
                    min_voltage_mv: 3100,
                    max_voltage_mv: 3400,
                    low_threshold_percent: 20,
                    critical_threshold_percent: 5,
                },
                ac_power: AcPowerConfig {
                    detection_threshold_mv: 12000,
                    battery_mode_threshold_mv: 11000,
                },
                led_blink: LedBlinkConfig {
                    toggle_count: 8,
                },
            },
            sensors: SensorConfig {
                num_lines: 8,
                sample_interval_ms: 5000,
                aggregation_interval_ms: 60000,
                report_interval_ms: 120000,
                failure_threshold: 3,
                warmup_threshold: 3,
            },
            serial: SerialConfig {
                port: "/dev/ttyAMA4".to_string(),
                baud_rate: 115200,
            },
            accelerometer: AccelerometerConfig {
                enabled: true,
                i2c_path: "/dev/i2c-1".to_string(),
                update_interval_ms: 100,
                motion_threshold_g: 0.3,
                debounce_samples: 5,
                logging_enabled: true,
            },
            storage: StorageConfig {
                db_path: "/data/fiber/fiber_medical.db".to_string(),
                max_size_gb: 5,
                hmac_secret_path: default_hmac_secret_path(),
            },
            system: SystemConfig {
                debug_mode: false,
                app_name: "FIBER Medical Thermometer".to_string(),
                app_version: "0.1.0".to_string(),
                timezone_offset_hours: 0,
                device_label: None, // Defaults to hostname at runtime
                led_brightness: 50,
                screen_brightness: 100,
                buzzer_volume: 100,
            },
            mqtt: None,  // MQTT disabled by default
            lorawan: None,  // LoRaWAN disabled by default
            ble: crate::libs::ble::BleConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default_config();
        assert_eq!(config.power.update_interval_ms, 500);
        assert_eq!(config.power.battery.min_voltage_mv, 3100);
        assert_eq!(config.power.battery.max_voltage_mv, 3400);
        assert_eq!(config.sensors.num_lines, 8);
        assert_eq!(config.system.app_version, "0.1.0");
    }

    #[test]
    fn test_battery_thresholds() {
        let config = Config::default_config();
        assert_eq!(config.power.battery.low_threshold_percent, 20);
        assert_eq!(config.power.battery.critical_threshold_percent, 5);
    }

    #[test]
    fn test_serial_config() {
        let config = Config::default_config();
        assert_eq!(config.serial.port, "/dev/ttyAMA4");
        assert_eq!(config.serial.baud_rate, 115200u32);
    }

    #[test]
    fn shipped_sensors_yaml_has_voltage_low_only_defaults() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fiber.sensors.config.yaml");
        let cfg = SensorFileConfig::from_file(&path)
            .expect("shipped fiber.sensors.config.yaml must parse");
        let v = cfg
            .common_lorawan_field_thresholds
            .get("voltage")
            .expect("voltage default present");
        assert_eq!(v.warning_low, Some(2.5));
        assert_eq!(v.critical_low, Some(2.2));
        assert!(v.warning_high.is_none(), "low_only field should not have warning_high");
        assert!(v.critical_high.is_none(), "low_only field should not have critical_high");
    }

    #[test]
    fn shipped_sensors_yaml_mirrors_temperature_for_external_probes() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fiber.sensors.config.yaml");
        let cfg = SensorFileConfig::from_file(&path).unwrap();
        let temp = cfg
            .common_lorawan_field_thresholds
            .get("temperature")
            .expect("temperature default present");
        for name in ["ext_temperature_1", "ext_temperature_2",
                     "machine_probe_temperature_1", "machine_probe_temperature_2"] {
            let other = cfg
                .common_lorawan_field_thresholds
                .get(name)
                .unwrap_or_else(|| panic!("{} default missing", name));
            assert_eq!(other.critical_low, temp.critical_low, "{}", name);
            assert_eq!(other.warning_low,  temp.warning_low, "{}", name);
            assert_eq!(other.warning_high, temp.warning_high, "{}", name);
            assert_eq!(other.critical_high, temp.critical_high, "{}", name);
        }
    }

    #[test]
    fn shipped_sensors_yaml_omits_defaults_for_unbounded_fields() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fiber.sensors.config.yaml");
        let cfg = SensorFileConfig::from_file(&path).unwrap();
        for name in ["illuminance", "pressure", "altitude"] {
            assert!(
                cfg.common_lorawan_field_thresholds.get(name).is_none(),
                "{} should NOT have a global default", name
            );
        }
    }
}

#[cfg(test)]
mod lorawan_sensor_config_tests {
    use super::*;

    #[test]
    fn lorawan_sensor_config_deserializes_with_location() {
        let yaml = r#"
dev_eui: "aabbccdd"
name: "Fridge"
location: "Cold room A, shelf 3"
"#;
        let cfg: LoRaWANSensorConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.location.as_deref(), Some("Cold room A, shelf 3"));
    }

    #[test]
    fn lorawan_sensor_config_deserializes_without_location() {
        let yaml = r#"
dev_eui: "aabbccdd"
name: "Fridge"
"#;
        let cfg: LoRaWANSensorConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.location, None);
    }

    #[test]
    fn lorawan_sensor_config_omits_location_when_none() {
        let cfg = LoRaWANSensorConfig {
            dev_eui: "x".to_string(),
            name: None,
            serial_number: None,
            location: None,
            enabled: true,
            field_thresholds: Vec::new(),
        };
        let out = serde_yaml::to_string(&cfg).unwrap();
        assert!(!out.contains("location"));
    }

    #[test]
    fn lorawan_sensor_config_deserializes_field_thresholds() {
        let yaml = r#"
dev_eui: "aabb"
name: "Sticker"
field_thresholds:
  - field: temperature
    critical_low: 0.0
    warning_low: 10.0
    warning_high: 35.0
    critical_high: 40.0
  - field: ext_temperature_1
    critical_high: 80.0
"#;
        let cfg: LoRaWANSensorConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.field_thresholds.len(), 2);
        assert_eq!(cfg.field_thresholds[0].field, "temperature");
        assert_eq!(cfg.field_thresholds[1].critical_high, Some(80.0));
    }
}
