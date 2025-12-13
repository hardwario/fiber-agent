// Configuration management for FIBER Medical Thermometer
// Loads and provides access to configuration from fiber.config.yaml

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

    /// Get effective report interval for a sensor line
    pub fn get_line_report_interval(&self, line: u8, default_ms: u64) -> u64 {
        self.lines
            .iter()
            .find(|l| l.line == line)
            .and_then(|l| l.report_interval_ms)
            .unwrap_or(default_ms)
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
                low_alarm_celsius: 35.0,
                warning_low_celsius: 34.0,
                warning_high_celsius: 39.0,
                high_alarm_celsius: 38.0,
                critical_high_celsius: 40.0,
            },
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
}

/// Temperature threshold configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemperatureThresholds {
    /// High temperature alarm threshold (Celsius)
    pub high_alarm_celsius: f32,

    /// Low temperature alarm threshold (Celsius)
    pub low_alarm_celsius: f32,

    /// Critical high temperature alarm (Celsius)
    pub critical_high_celsius: f32,

    /// Critical low temperature alarm (Celsius)
    pub critical_low_celsius: f32,
}

/// Display configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayConfig {
    /// Display update interval in milliseconds
    pub update_interval_ms: u64,

    /// Display refresh rate (updates per second)
    pub refresh_rate_hz: u8,

    /// Backlight control enabled
    pub backlight_enabled: bool,

    /// Rotation in degrees (0, 90, 180, 270)
    pub rotation_degrees: u16,
}

/// User interface configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    /// Button debounce time in milliseconds
    pub button_debounce_ms: u64,

    /// Menu idle timeout in seconds (0 = no timeout)
    pub menu_idle_timeout_sec: u64,

    /// Buzzer configuration
    pub buzzer: BuzzerConfig,
}

/// Buzzer feedback configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuzzerConfig {
    /// Enable buzzer feedback
    pub enabled: bool,

    /// Beep duration in milliseconds
    pub beep_duration_ms: u64,

    /// Beep interval for alerts in milliseconds
    pub alert_interval_ms: u64,
}

/// Data logging configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Enable data logging
    pub enabled: bool,

    /// Log file path
    pub log_file: String,

    /// Log interval in milliseconds
    pub interval_ms: u64,

    /// Maximum log file size in MB
    pub max_file_size_mb: u64,

    /// Number of historical log files to keep
    pub max_backup_files: u32,

    /// Log verbosity level
    pub verbosity: String,
}

/// Serial communication configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerialConfig {
    /// STM32 serial port device path
    pub port: String,

    /// Baud rate
    pub baud_rate: u32,

    /// Serial port timeout in milliseconds
    pub timeout_ms: u64,

    /// Maximum retries for failed commands
    pub max_retries: u8,
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
}

/// Medical data storage configuration (EU MDR 2017/745 compliance)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Enable storage system
    pub enabled: bool,

    /// Path to SQLite database file
    pub db_path: String,

    /// Maximum database size in gigabytes
    pub max_size_gb: i32,

    /// Minimum data retention in days
    pub retention_days: i32,

    /// Cleanup threshold percentage (e.g., 90 = cleanup at 90% capacity)
    pub cleanup_threshold_percent: f32,

    /// Auto-flush interval in milliseconds
    pub flush_interval_ms: u64,

    /// Auto-flush threshold (number of messages)
    pub flush_threshold_messages: usize,

    /// Enable backup functionality
    pub backup_enabled: bool,

    /// Path for database backups
    pub backup_path: String,

    /// Enable audit trail logging
    pub audit_enabled: bool,
}

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
    pub enabled: bool,
    pub ca_cert_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_cert_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_key_path: Option<String>,
    #[serde(default)]
    pub insecure_skip_verify: bool,
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

    /// Display settings
    pub display: DisplayConfig,

    /// User interface settings
    pub ui: UiConfig,

    /// Data logging settings
    pub logging: LoggingConfig,

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
}

impl Config {
    /// Load configuration from a YAML file
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let content = fs::read_to_string(path)?;
        let config: Config = serde_yaml::from_str(&content)?;
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
            },
            display: DisplayConfig {
                update_interval_ms: 1000,
                refresh_rate_hz: 1,
                backlight_enabled: true,
                rotation_degrees: 180,
            },
            ui: UiConfig {
                button_debounce_ms: 50,
                menu_idle_timeout_sec: 30,
                buzzer: BuzzerConfig {
                    enabled: true,
                    beep_duration_ms: 100,
                    alert_interval_ms: 500,
                },
            },
            logging: LoggingConfig {
                enabled: true,
                log_file: "data/sensor_log.csv".to_string(),
                interval_ms: 60000,
                max_file_size_mb: 10,
                max_backup_files: 10,
                verbosity: "info".to_string(),
            },
            serial: SerialConfig {
                port: "/dev/ttyAMA4".to_string(),
                baud_rate: 115200,
                timeout_ms: 1000,
                max_retries: 3,
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
                enabled: true,
                db_path: "/data/fiber_medical.db".to_string(),
                max_size_gb: 5,
                retention_days: 1095, // 3 years
                cleanup_threshold_percent: 90.0,
                flush_interval_ms: 100,
                flush_threshold_messages: 1000,
                backup_enabled: true,
                backup_path: "/data/backups/".to_string(),
                audit_enabled: true,
            },
            system: SystemConfig {
                debug_mode: false,
                app_name: "FIBER Medical Thermometer".to_string(),
                app_version: "0.1.0".to_string(),
                timezone_offset_hours: 0,
            },
            mqtt: None,  // MQTT disabled by default
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
        assert_eq!(config.serial.baud_rate, 115200);
    }
}
