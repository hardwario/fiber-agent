// Configuration management for FIBER Medical Thermometer
// Loads and provides access to configuration from fiber.config.yaml

use crate::libs::alarms::AlarmThreshold;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

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
    Slow, // 4 cycles
    Fast, // 1 cycle
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
    Disconnected, // Configurable timing
    #[serde(rename = "critical")]
    Critical, // Configurable timing
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

    /// Deep-standby behaviour: how the device waits for PoE while powered down.
    #[serde(default)]
    pub standby: StandbyConfig,
}

/// Deep standby — the state a powered-down device waits for PoE in.
///
/// `#[serde(default)]` throughout so an existing on-device
/// `/data/fiber/config/fiber.config.yaml` keeps parsing without a migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StandbyConfig {
    /// How often to read VIN while in standby.
    ///
    /// Deliberately separate from `update_interval_ms`, which ships as 60000:
    /// reusing it would make a device take up to a minute to notice PoE. The
    /// shorter interval is close to free — the SoC is awake either way and each
    /// read is one UART round trip — so this is a wake-latency knob, not a
    /// power one.
    #[serde(default = "default_standby_poll_interval_ms")]
    pub poll_interval_ms: u64,

    /// Whether PoE arriving should bring the device back up by itself.
    ///
    /// Turning this off leaves a device that must be woken by power-cycling it,
    /// which is the pre-standby behaviour.
    #[serde(default = "default_resume_on_dc")]
    pub resume_on_dc: bool,

    /// Consecutive above-threshold VIN readings required before resuming, so a
    /// cable being wiggled cannot thrash the device in and out of standby.
    #[serde(default = "default_standby_confirm_polls")]
    pub confirm_polls: u32,

    /// CPU governor to select on the way into standby, restored on resume.
    /// Empty leaves the governor alone.
    #[serde(default = "default_standby_cpu_governor")]
    pub cpu_governor: String,
}

fn default_standby_poll_interval_ms() -> u64 {
    // 1 s, not 5 s. This interval is the window in which a PoE interruption can
    // be sampled directly, and at 5 s a quick unplug/replug fell entirely between
    // two polls — the device never saw the absence and so never woke. The carrier
    // counter is the backstop for anything shorter, but sampling the dip itself is
    // the stronger evidence, so make it the common case.
    1000
}

fn default_resume_on_dc() -> bool {
    true
}

fn default_standby_confirm_polls() -> u32 {
    2
}

fn default_standby_cpu_governor() -> String {
    "powersave".to_string()
}

impl Default for StandbyConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: default_standby_poll_interval_ms(),
            resume_on_dc: default_resume_on_dc(),
            confirm_polls: default_standby_confirm_polls(),
            cpu_governor: default_standby_cpu_governor(),
        }
    }
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
    /// Superseded by `dc_connect_mv`. Kept so existing on-device configs keep
    /// parsing; no longer read.
    ///
    /// It shipped as 12000, which the southbridge's VIN maths makes unusable: two
    /// integer truncations mean a perfect 12.000 V input reports 11998 mV and
    /// 12000 is not reachable at all. Devices whose supply reported just under it
    /// were classified as running on battery while on mains.
    pub detection_threshold_mv: u16,

    /// Superseded by `dc_disconnect_mv`. Was never read even before that.
    pub battery_mode_threshold_mv: u16,

    /// VIN at or above which DC power counts as present (mV).
    ///
    /// Defaulted rather than required, so deployed
    /// `/data/fiber/config/fiber.config.yaml` files — which carry the old 12000
    /// under the legacy key — pick up the corrected value with no migration.
    #[serde(default = "default_dc_connect_mv")]
    pub dc_connect_mv: u16,

    /// VIN below which DC power counts as gone (mV). Below `dc_connect_mv`, so a
    /// supply resting near the boundary cannot chatter.
    #[serde(default = "default_dc_disconnect_mv")]
    pub dc_disconnect_mv: u16,
}

fn default_dc_connect_mv() -> u16 {
    crate::libs::power::status::DEFAULT_DC_CONNECT_MV
}

fn default_dc_disconnect_mv() -> u16 {
    crate::libs::power::status::DEFAULT_DC_DISCONNECT_MV
}

impl AcPowerConfig {
    /// The hysteresis pair this config describes, validated.
    pub fn dc_thresholds(&self) -> crate::libs::power::status::DcThresholds {
        crate::libs::power::status::DcThresholds::new(self.dc_connect_mv, self.dc_disconnect_mv)
    }
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

    /// Hysteresis override in °C (None = use common, #110)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hysteresis_celsius: Option<f32>,

    /// The band's "Mez zapnuta" switch (#167); None = on. Not the line's own `enabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits_enabled: Option<bool>,

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

    /// Default per-field thresholds for LoRaWAN nodes, keyed by field name
    /// (matches `registry::REGISTRY`). Used as a fallback for every node that
    /// has no explicit override, on a per-bound basis. Empty map disables
    /// auto-alarming for nodes.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub common_lorawan_field_thresholds: HashMap<String, FieldThresholdBounds>,

    /// Per-line sensor configurations
    pub lines: Vec<SensorLineConfig>,
}

impl SensorFileConfig {
    /// Load sensor configuration from a YAML file.
    ///
    /// A file still holding the default bands shipped before 2026-09-28 is moved to the new
    /// ones first (`config_migrations::migrate_sensor_defaults_and_persist`,
    /// plan/portal/threshold_defaults.md §4). A migration that fails is logged and the file
    /// is read as it is: old bands are still bands.
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let content = match crate::libs::config_migrations::migrate_sensor_defaults_and_persist(
            path.as_ref(),
        ) {
            Ok(content) => content,
            Err(e) => {
                // Logged once per message: the sensor monitor reloads this file on a
                // timer, and a stable failure would otherwise repeat forever (the
                // same reasoning as `Config::from_file`).
                let msg = e.to_string();
                static LAST: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
                if let Ok(mut last) = LAST.lock() {
                    if last.as_deref() != Some(msg.as_str()) {
                        eprintln!("[config] sensor default bands not migrated: {}", msg);
                        *last = Some(msg);
                    }
                }
                fs::read_to_string(path)?
            }
        };
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
                hysteresis_celsius: cfg
                    .hysteresis_celsius
                    .unwrap_or(self.common_alarms.hysteresis_celsius),
                enabled: cfg.limits_enabled.unwrap_or(self.common_alarms.enabled),
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

    /// Get default sensor configuration for testing or when file is missing.
    ///
    /// `common_alarms` is the shipped file's band (−10/0/40/50 °C since 2026-09-28,
    /// plan/portal/threshold_defaults.md), so a Hub that cannot read its file does not
    /// fall back to a different set of defaults.
    pub fn default_config() -> Self {
        Self {
            alarm_patterns: Some(Self::default_alarm_patterns()),
            common_alarms: AlarmThreshold {
                critical_low_celsius: -10.0,
                low_alarm_celsius: 0.0, // disabled - defaults
                warning_low_celsius: 0.0,
                warning_high_celsius: 40.0,
                high_alarm_celsius: 100.0, // disabled - defaults
                critical_high_celsius: 50.0,
                hysteresis_celsius: crate::libs::alarms::threshold::DEFAULT_HYSTERESIS_CELSIUS,
                enabled: true,
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
                    hysteresis_celsius: None,
                    limits_enabled: None,
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
                    hysteresis_celsius: None,
                    limits_enabled: None,
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
                    hysteresis_celsius: None,
                    limits_enabled: None,
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
                    hysteresis_celsius: None,
                    limits_enabled: None,
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
                    hysteresis_celsius: None,
                    limits_enabled: None,
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
                    hysteresis_celsius: None,
                    limits_enabled: None,
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
                    hysteresis_celsius: None,
                    limits_enabled: None,
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
                    hysteresis_celsius: None,
                    limits_enabled: None,
                    report_interval_ms: None,
                    location: None,
                },
            ],
        }
    }
}

/// Temperature sensor configuration
///
/// Intervals are in seconds (plan/control/storage_intervals.md §3). A file
/// written before 2026-10-12 carries `sample_interval_ms` /
/// `aggregation_interval_ms` / `report_interval_ms`; those are accepted,
/// converted and warned about, so a Hub keeps booting on its old YAML.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "RawSensorConfig")]
pub struct SensorConfig {
    /// Number of W1 sensor lines (0-7 for 8 lines)
    pub num_lines: u8,

    /// How often the lines are read; every sample is published live on MQTT.
    pub sample_interval_s: u64,

    /// Storage window: the samples of one window are aggregated into one
    /// database row per line and one `sensors/aggregated` message.
    pub aggregation_interval_s: u64,

    /// Number of consecutive failed reads before marking sensor as offline
    pub failure_threshold: u8,

    /// Number of consecutive successful reads before a sensor exits NeverConnected state.
    /// Prevents false alarms from lucky single reads during OneWire bus stabilization at boot.
    pub warmup_threshold: u8,
}

pub const DEFAULT_SAMPLE_INTERVAL_S: u64 = 2;
pub const DEFAULT_AGGREGATION_INTERVAL_S: u64 = 600;

impl SensorConfig {
    pub fn sample_interval_ms(&self) -> u64 {
        self.sample_interval_s.saturating_mul(1000)
    }

    pub fn aggregation_interval_ms(&self) -> u64 {
        self.aggregation_interval_s.saturating_mul(1000)
    }
}

/// The on-disk shape of `sensors:`, both spellings. `_s` wins over `_ms`.
#[derive(Deserialize)]
struct RawSensorConfig {
    num_lines: u8,
    #[serde(default)]
    sample_interval_s: Option<u64>,
    #[serde(default)]
    aggregation_interval_s: Option<u64>,
    #[serde(default)]
    sample_interval_ms: Option<u64>,
    #[serde(default)]
    aggregation_interval_ms: Option<u64>,
    #[serde(default)]
    report_interval_ms: Option<u64>,
    failure_threshold: u8,
    #[serde(default = "default_warmup_threshold")]
    warmup_threshold: u8,
}

/// Milliseconds to whole seconds, rounding up so a sub-second legacy value
/// never becomes 0.
fn ms_to_s(ms: u64) -> u64 {
    ms.div_ceil(1000)
}

impl From<RawSensorConfig> for SensorConfig {
    fn from(raw: RawSensorConfig) -> Self {
        let mut legacy = Vec::new();
        let sample_interval_s = match (raw.sample_interval_s, raw.sample_interval_ms) {
            (Some(s), _) => s,
            (None, Some(ms)) => {
                legacy.push("sample_interval_ms");
                ms_to_s(ms)
            }
            (None, None) => DEFAULT_SAMPLE_INTERVAL_S,
        };
        let aggregation_interval_s = match (raw.aggregation_interval_s, raw.aggregation_interval_ms)
        {
            (Some(s), _) => s,
            (None, Some(ms)) => {
                legacy.push("aggregation_interval_ms");
                ms_to_s(ms)
            }
            (None, None) => DEFAULT_AGGREGATION_INTERVAL_S,
        };
        if raw.report_interval_ms.is_some() {
            legacy.push("report_interval_ms");
        }
        if !legacy.is_empty() {
            warn_legacy_sensor_keys(&legacy);
        }
        SensorConfig {
            num_lines: raw.num_lines,
            sample_interval_s,
            aggregation_interval_s,
            failure_threshold: raw.failure_threshold,
            warmup_threshold: raw.warmup_threshold,
        }
    }
}

/// Once per process: the config is re-read every 10 s by the hot-reload loop.
fn warn_legacy_sensor_keys(keys: &[&str]) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "[config] sensors: {} are deprecated; use sample_interval_s / \
             aggregation_interval_s (seconds). report_interval_ms is ignored.",
            keys.join(", ")
        );
    });
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

    /// Idle timeout in seconds before the display backlight turns off to save
    /// power / reduce burn-in. Any button press wakes it; an active alarm keeps
    /// it lit. Default 3600 (1 hour). 0 disables the timeout (always on).
    #[serde(default = "default_screen_timeout_secs")]
    pub screen_timeout_secs: u32,
}

/// Physical display (ST7920 LCD) configuration.
///
/// When `custom_lines` is empty the overview screen renders the built-in
/// layout (active-first list of the 8 DS18B20 probes then the LoRaWAN
/// nodes) — i.e. an absent `display:` section means "behave as before".
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DisplayConfig {
    /// User-authored overview lines, rendered top-to-bottom in this order,
    /// four per page.
    ///
    /// Deserialized leniently: an entry that fails to parse is dropped with a
    /// warning rather than failing the whole `Config`. This section shares the
    /// file that `Config::load_default()` parses during boot, so a newer
    /// Viewer emitting an unknown `source` must not be able to stop the device
    /// from starting over a display-cosmetics field.
    #[serde(
        default,
        deserialize_with = "de_display_lines",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub custom_lines: Vec<DisplayLine>,
}

/// Where a configured display line reads its value from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayLineSource {
    /// A DS18B20 1-Wire probe, addressed by its line index.
    Ds18b20,
    /// A ProXimos Node / LoRaWAN sensor, addressed by DevEUI.
    #[serde(rename = "sticker")]
    Node,
    /// An EYE BLE tag, addressed by MAC.
    Ble,
}

/// One configured row of the sensor overview screen.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DisplayLine {
    /// Which subsystem provides the value.
    pub source: DisplayLineSource,

    /// DS18B20 line index 0..=7. Required when `source` is `ds18b20`, and
    /// must be absent otherwise. Named `line` to match the existing
    /// `SetSensorName { line }` / `SensorLineConfig::line` vocabulary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u8>,

    /// Node DevEUI (16 hex chars, lowercased on load). Required when
    /// `source` is `sticker`, and must be absent otherwise.
    ///
    /// Deliberately not an ordinal ("Node 2"): node ordering is derived
    /// from a sort over the live sensor map, so adding or removing one node
    /// renumbers every later one — an ordinal line would silently re-point at
    /// a different physical probe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dev_eui: Option<String>,

    /// EYE BLE tag MAC (`AA:BB:CC:DD:EE:FF`, **uppercased** on load). Required
    /// when `source` is `ble`, and must be absent otherwise.
    ///
    /// Not an ordinal, for the same reason `dev_eui` isn't. Uppercase rather
    /// than lowercase because that is the EYE subsystem's canonical form —
    /// [`crate::libs::beacon::state::BeaconSensorState::tags`] is keyed by uppercase
    /// MAC and the `add_eye_tag` / `remove_eye_tag` commands uppercase theirs.
    /// Each subsystem keeps its own canonical case; normalizing both to the
    /// same one would break one of the two lookups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,

    /// Field selector. For `ds18b20`: `temperature` or `status`. For
    /// `sticker`: any name in [`crate::libs::lorawan::registry::REGISTRY`],
    /// plus the pseudo-fields `rssi`, `snr` and `status` which live on the
    /// sensor state rather than in its field map. For `ble`: one of
    /// [`crate::libs::config_applier::validation::BLE_FIELDS`].
    pub field: String,

    /// Row label. When absent, derived from the source sensor's configured
    /// name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    /// Value formatting. May be omitted entirely for the defaults.
    #[serde(default, skip_serializing_if = "DisplayLineFormat::is_default")]
    pub format: DisplayLineFormat,
}

/// Per-line value formatting options.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DisplayLineFormat {
    /// Append the field's unit (`°C`, `%`, `V`, `hPa`, `m`, `lx`, `dBm`, `dB`).
    #[serde(default = "default_true")]
    pub units: bool,

    /// Decimal places, 0..=3. `None` uses the per-field default (1 for
    /// temperatures, 0 for humidity/pressure/counters, 2 for battery voltage).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u8>,

    /// Draw the alarm status character (`N`/`W`/`C`/`E`/`-`/`?`) at the right
    /// edge of the row.
    #[serde(default = "default_true")]
    pub status_char: bool,
}

impl Default for DisplayLineFormat {
    fn default() -> Self {
        Self {
            units: true,
            decimals: None,
            status_char: true,
        }
    }
}

impl DisplayLineFormat {
    /// True when every option is at its default, so `format:` can be omitted
    /// from the serialized YAML instead of written out in full.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Deserialize `display.custom_lines`, skipping entries that fail to parse.
///
/// See [`DisplayConfig::custom_lines`] for why this must not be strict.
fn de_display_lines<'de, D>(deserializer: D) -> Result<Vec<DisplayLine>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<serde_yaml::Value>::deserialize(deserializer)?;
    let mut lines = Vec::with_capacity(raw.len());
    for (idx, value) in raw.into_iter().enumerate() {
        match serde_yaml::from_value::<DisplayLine>(value) {
            Ok(line) => lines.push(line),
            Err(e) => eprintln!("[config] display.custom_lines[{}] ignored: {}", idx, e),
        }
    }
    Ok(lines)
}

/// Medical data storage configuration (EU MDR 2017/745 compliance)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Path to SQLite database file
    pub db_path: String,

    /// Maximum database size in gigabytes (whole-GB granularity).
    pub max_size_gb: i32,

    /// Optional override of `max_size_gb` for sub-GB precision. When
    /// `Some(n)`, the retention cap is `n` megabytes; when `None` the
    /// `max_size_gb` field is used. Lets us pick e.g. 2500 (=2.5 GB)
    /// on a 4 GB partition shared with fiber-viewer without breaking
    /// the i32-typed `max_size_gb` field for existing YAML configs.
    #[serde(default)]
    pub max_size_mb: Option<i32>,

    /// Path to HMAC secret key file for sensor reading integrity (EU MDR)
    #[serde(default = "default_hmac_secret_path")]
    pub hmac_secret_path: String,

    /// When a deferred full `VACUUM` may start. Absent = no scheduled
    /// maintenance, and a rewrite happens only when the database crosses the
    /// emergency threshold. See plan/portal/sqlite-vacuum.md §5.
    #[serde(default)]
    pub maintenance_window: Option<MaintenanceWindow>,
}

/// When a scheduled full `VACUUM` may *start*.
///
/// **UTC**, and the key says so. This binary carries no timezone database, and
/// `system.timezone_offset_hours` is a display affordance rather than a
/// scheduling authority — reading it here would make the same YAML mean two
/// different times in the agent and the portal.
///
/// Deliberately shape-identical to the portal's `maintenance_window` block
/// (`portal/src/config.rs`), so decision #9's merged binary can read one.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaintenanceWindow {
    /// `"HH:MM"`, UTC.
    #[serde(default = "default_window_start_utc")]
    pub start_utc: String,
    /// How long the window stays open. Bounds when a rewrite may *start*, never
    /// when it may finish: SQLite cannot be paused, and killing a `VACUUM`
    /// rolls the whole thing back rather than saving any of the work.
    #[serde(default = "default_window_duration_minutes")]
    pub duration_minutes: u32,
}

fn default_window_start_utc() -> String {
    "02:30".to_string()
}

fn default_window_duration_minutes() -> u32 {
    90
}

/// Minutes in a day, for the wrap arithmetic below.
const MINUTES_PER_DAY: u32 = 24 * 60;

impl MaintenanceWindow {
    /// Minutes past midnight UTC, or `None` when `start_utc` is not a 24-hour
    /// `HH:MM` time.
    ///
    /// Returned rather than defaulted: a misparsed window must not silently
    /// mean "run maintenance at midnight", and the caller logs it loudly and
    /// treats the window as absent.
    pub fn start_minute_utc(&self) -> Option<u32> {
        let (h, m) = self.start_utc.split_once(':')?;
        let h: u32 = h.parse().ok()?;
        let m: u32 = m.parse().ok()?;
        (h < 24 && m < 60).then_some(h * 60 + m)
    }

    /// Whether `minute_of_day_utc` is inside the window, wrapping across
    /// midnight (23:30 + 90 minutes runs to 01:00).
    pub fn contains(&self, minute_of_day_utc: u32) -> bool {
        let Some(start) = self.start_minute_utc() else {
            return false;
        };
        if self.duration_minutes == 0 {
            return false;
        }
        (minute_of_day_utc + MINUTES_PER_DAY - start) % MINUTES_PER_DAY < self.duration_minutes
    }
}

impl StorageConfig {
    /// Effective storage cap in bytes. Prefers `max_size_mb` (MB
    /// granularity) when set, otherwise falls back to the integer
    /// `max_size_gb`. Both clamped to a positive minimum so a
    /// misconfigured 0/negative value can't disable retention.
    pub fn effective_max_bytes(&self) -> i64 {
        if let Some(mb) = self.max_size_mb {
            (mb.max(1) as i64) * 1024 * 1024
        } else {
            (self.max_size_gb.max(1) as i64) * 1024 * 1024 * 1024
        }
    }
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

/// Resolve the effective threshold for a single field on a node, merging the
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

/// Compute the full list of effective thresholds for a node, considering
/// both the per-sensor `field_thresholds` and the YAML defaults map. Every
/// *thresholdable* field that has at least one bound from either source is
/// included.
///
/// The registry is the authority on which fields may alarm at all. Without that
/// check this function was purely data-driven, so a field the product has
/// withdrawn from alarming — battery, whose low-battery condition the node
/// already reports natively on fPort 3 — kept alarming from a stale per-sensor
/// override or an older YAML that still listed it. Filtering here (rather than
/// only in the UI) means the withdrawal holds for config written before it.
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
        // An unknown field is kept: the registry and the firmware ship together, so
        // a name it does not recognise is more likely a newer config than a
        // withdrawn field, and dropping it would silently disarm a real alarm.
        .filter(|f| crate::libs::lorawan::registry::lookup(f).is_none_or(|d| d.thresholdable))
        // An explicitly switched-off alarm beats everything, including the YAML
        // default. This is the only way to silence a quantity: an omitted bound
        // inherits the default instead of clearing it, so without this a
        // default-armed field could never be turned off.
        .filter(|f| !sensor.is_some_and(|s| s.disarmed_fields.iter().any(|d| d == f)))
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

    /// Fields whose alarm the operator has explicitly switched OFF.
    ///
    /// Needed because clearing the bounds cannot express it: an omitted bound
    /// inherits the YAML default (see [`resolve_field_threshold`]), and the
    /// defaults arm temperature, humidity and the probe fields on every node
    /// the moment it is paired. So there was no way to silence a quantity —
    /// emptying the inputs and "reset to default" both landed back on the armed
    /// default.
    ///
    /// A separate list rather than a flag on `FieldThreshold` for two reasons: a
    /// field can be disarmed while having no override of its own at all (exactly
    /// the default-armed case this exists for), and switching the alarm off is
    /// then independent of resetting the bounds, so one cannot silently undo the
    /// other.
    ///
    /// Absent from the YAML unless something is actually off, so an untouched
    /// deployment reads and behaves exactly as before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disarmed_fields: Vec<String>,
}

/// External LoRaWAN gateway registered in the on-device ChirpStack.
/// Lets the FIBER accept uplinks from a gateway forwarding over Semtech UDP :1700.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalGatewayConfig {
    /// Gateway EUI (16 hex chars, lowercase) — the ChirpStack gateway_id
    pub gateway_eui: String,

    /// Human-friendly name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Enable this gateway
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Credentials for the on-device ChirpStack gRPC-web API.
///
/// The API URL is deliberately not configurable: ChirpStack always runs on the
/// same system as the agent, so the loopback address in
/// [`crate::libs::lorawan::provisioning`] is part of the deployment contract.
/// Only the credentials vary per deployment.
///
/// `password` ships empty on purpose — it is populated at deployment time and
/// must never be committed. An empty password fails LoRaWAN init loudly rather
/// than falling back to ChirpStack's well-known factory account.
#[derive(Clone, Serialize, Deserialize)]
pub struct ChirpStackApiConfig {
    /// ChirpStack API user. `admin` is the factory account name.
    #[serde(default = "default_chirpstack_api_username")]
    pub username: String,

    /// ChirpStack API password. Empty means "not provisioned" — see
    /// [`ChirpStackApiConfig::validate`].
    #[serde(default)]
    pub password: String,
}

impl Default for ChirpStackApiConfig {
    fn default() -> Self {
        Self {
            username: default_chirpstack_api_username(),
            password: String::new(),
        }
    }
}

/// Redacted by hand so the password cannot leak through a `{:?}` on this struct
/// or on any struct that contains it — `LoRaWANConfig` derives `Debug` and is
/// logged during startup diagnostics.
impl std::fmt::Debug for ChirpStackApiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChirpStackApiConfig")
            .field("username", &self.username)
            .field(
                "password",
                &if self.password.is_empty() {
                    "<unset>"
                } else {
                    "***"
                },
            )
            .finish()
    }
}

impl ChirpStackApiConfig {
    /// `Err` with an operator-actionable message when either half is blank.
    ///
    /// Callers surface this message in logs, so it must never contain the
    /// password value itself.
    pub fn validate(&self) -> Result<(), String> {
        if self.username.trim().is_empty() {
            return Err(
                "lorawan.chirpstack.username is empty — set it in fiber.config.yaml".to_string(),
            );
        }
        if self.password.is_empty() {
            return Err(
                "lorawan.chirpstack.password is empty — provision it at deployment time \
                 (see README, \"Provisioning ChirpStack credentials\")"
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// LoRaWAN gateway configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoRaWANConfig {
    /// Enable LoRaWAN gateway integration.
    ///
    /// Defaults to **true** when the key is absent, because that is what every
    /// deployed unit with a `lorawan:` block already does: the monitor used to
    /// start purely on hardware detection and nothing read this flag, so a
    /// config that never mentioned it would otherwise go silent on upgrade.
    /// Only an explicit `enabled: false` stops the monitor — which is how a
    /// cluster follower stops reporting nodes the leader now owns.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// ChirpStack gRPC-web API credentials. Distinct from the
    /// `chirpstack_mqtt_*` fields below, which authenticate to the local MQTT
    /// broker — this pair authenticates to ChirpStack's own API.
    #[serde(default)]
    pub chirpstack: ChirpStackApiConfig,

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

    /// Pull a STICKER's buffered history automatically when it reappears after
    /// an outage.
    ///
    /// Defaults to `true`, which is the behaviour that shipped. Set it to
    /// `false` on deployments where the viewer owns backfill (PROXIMOS
    /// application#43): both sides trigger on the same reconnect, so leaving
    /// both enabled spends two replays of the same window — precisely the
    /// airtime waste the viewer's job queue exists to prevent.
    #[serde(default = "default_history_backfill_enabled")]
    pub history_backfill_enabled: bool,

    /// Per-sensor configurations
    #[serde(default)]
    pub sensors: Vec<LoRaWANSensorConfig>,

    /// External LoRaWAN gateways registered in ChirpStack
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gateways: Vec<ExternalGatewayConfig>,
}

impl Default for LoRaWANConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            chirpstack: ChirpStackApiConfig::default(),
            chirpstack_mqtt_host: "localhost".to_string(),
            chirpstack_mqtt_port: 1883,
            chirpstack_mqtt_username: None,
            chirpstack_mqtt_password: None,
            publish_interval_s: 30,
            sensor_timeout_s: 3600, // 1 hour
            history_backfill_enabled: default_history_backfill_enabled(),
            sensors: Vec::new(),
            gateways: Vec::new(),
        }
    }
}

/// ChirpStack's factory API account name. Only the username defaults — the
/// password intentionally has no default, so an unprovisioned device fails
/// rather than authenticating with a credential that is public knowledge.
fn default_chirpstack_api_username() -> String {
    "admin".to_string()
}

fn default_chirpstack_mqtt_host() -> String {
    "localhost".to_string()
}
fn default_chirpstack_mqtt_port() -> u16 {
    1883
}
fn default_lorawan_publish_interval() -> u64 {
    30
}
fn default_lorawan_sensor_timeout() -> u64 {
    3600
}

/// Keeps the shipped behaviour for anyone who has not opted the viewer in.
fn default_history_backfill_enabled() -> bool {
    true
}

/// MQTT broker configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerConfig {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub client_id: String, // Empty = use hostname
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// `true` only when this broker came from the rendered Client override
    /// ([`Config::apply_rendered_broker_override`]): the Server's broker on a Client Hub.
    /// It is what makes the agent keep the Server's IPv4 address in the runtime hosts file
    /// (`mqtt::server_hosts`). Never read from or written to a file, so a hand-edited
    /// `fiber.config.yaml` cannot opt a Server into rewriting its hosts.
    #[serde(skip)]
    pub from_cluster_override: bool,
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
    pub max_reconnect_attempts: u32, // 0 = infinite
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

    /// Save-and-feed: multi-destination store-and-forward export config.
    /// Defaults to disabled with no destinations — backwards-compatible
    /// with configs that don't carry an `export` block.
    #[serde(default)]
    pub export: crate::libs::mqtt_export::ExportConfig,
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
fn default_warmup_threshold() -> u8 {
    3
}

// Default value functions for system configuration
fn default_led_brightness() -> u8 {
    50
}
fn default_screen_brightness() -> u8 {
    100
}
fn default_buzzer_volume() -> u8 {
    100
}
fn default_screen_timeout_secs() -> u32 {
    3600
}

// Default value functions for MQTT configuration
fn default_true() -> bool {
    true
}
fn default_qos_1() -> u8 {
    1
}
fn default_qos_2() -> u8 {
    2
}
fn default_queue_size() -> usize {
    10000
}
fn default_sensor_interval() -> u64 {
    5
}
fn default_power_interval() -> u64 {
    10
}
fn default_network_interval() -> u64 {
    30
}
fn default_system_interval() -> u64 {
    60
}
fn default_rate_limit() -> u32 {
    10
}
fn default_keepalive() -> u64 {
    60
}
fn default_timeout() -> u64 {
    30
}
fn default_reconnect_delay() -> u64 {
    1
}
fn default_max_reconnect_delay() -> u64 {
    30
}
fn default_lwt_topic() -> String {
    "status".to_string()
}
fn default_lwt_payload() -> String {
    r#"{"status":"offline"}"#.to_string()
}

/// The role an operator chose for this Hub during setup.
///
/// Spelled `server` / `client` on the wire because that is what
/// [`decisions.md`](../../../../../plan/decisions.md) #5 and the phone app already
/// call them. There is no `Unknown` variant: this is the **write** type — the
/// closed set of roles a build can put into `setup:` — and keeping it closed is
/// what makes "only server or client is ever written" a property of the type
/// rather than of every call site. Leniency belongs on the read side, where a
/// file this build did not write arrives; that is [`SetupSection::role`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SetupRole {
    Server,
    Client,
}

impl SetupRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            SetupRole::Server => "server",
            SetupRole::Client => "client",
        }
    }

    /// The one place a `role:` string becomes this enum, so the read side and
    /// `ApplierSetupState::read` cannot drift apart about what `server` means.
    pub fn parse(role: &str) -> Option<SetupRole> {
        match role {
            "server" => Some(SetupRole::Server),
            "client" => Some(SetupRole::Client),
            _ => None,
        }
    }
}

/// The `setup:` section — whether an operator has completed setup on this Hub,
/// and the role they chose.
///
/// This is the state [`decisions.md`](../../../../../plan/decisions.md) #4 says
/// must live in the device's own config rather than in the portal's database,
/// and `plan/control/ble/peripheral.md` §6.3 serves over FB16/FB17. Three
/// properties it is required to have, none of which are accidents of the type:
///
/// - **It is what an operator did, not a health check.** "Named and networked"
///   would flip back to false whenever a Hub moves networks, which is not the
///   question the app is asking.
/// - **A factory reset clears it** — by construction, because the config file
///   lives under `/data` and the wipe takes the whole tree; a refurbished Hub
///   must not claim to be configured.
/// - **The portal reads it and keeps no copy.** v1's `app_mode` in the viewer
///   database is deliberately not carried forward: a client Hub shuts its
///   portal down, so on exactly the Hubs the app needs to ask about, the
///   component that held v1's answer is not running.
///
/// `deny_unknown_fields` for the same reason the `ble:` section has it: a
/// mistyped *key* here is a Hub that silently reports the wrong setup state.
/// An unknown *value* for `role:` is the opposite case and is tolerated — see
/// the field.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupSection {
    /// An operator completed setup on this Hub at least once.
    #[serde(default)]
    pub provisioned: bool,

    /// The role chosen at that moment, as the file spells it. `None` until
    /// setup completes.
    ///
    /// A **string**, not the closed [`SetupRole`], and that is the fix for a
    /// real divergence: a firmware downgrade (or a newer app writing a role
    /// this build has never heard of) left `Config::load_default()` failing
    /// outright, so `system/info` published `setup.provisioned: false` while
    /// BLE FB16 — which reads the raw YAML and tolerates the value — said
    /// `true` about the same file. The two surfaces answer the same question
    /// and must not disagree, so the typed section is now as lenient about the
    /// value as the BLE seam is: the role rides through verbatim and
    /// [`SetupSection::known_role`] is where it becomes an enum, or does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

impl SetupSection {
    /// The role as *this build* understands it — `None` both for "not set" and
    /// for "set to something this build does not know".
    ///
    /// Callers that must tell those apart (the role lock in
    /// `ConfigApplier::apply_setup_complete`, which treats a stale-but-set role
    /// as a role) look at [`SetupSection::role`] itself.
    pub fn known_role(&self) -> Option<SetupRole> {
        self.role.as_deref().and_then(SetupRole::parse)
    }
}

/// Complete application configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// The `setup:` section — see [`SetupSection`]. Defaulted so that every
    /// config file written before 2026-09-16 keeps parsing and reads as
    /// "nobody has set this Hub up", which is the honest answer for a file
    /// that predates the question.
    #[serde(default)]
    pub setup: SetupSection,

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

    /// Physical display (LCD overview screen) settings
    #[serde(default)]
    pub display: DisplayConfig,

    /// MQTT client settings
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mqtt: Option<MqttConfig>,

    /// LoRaWAN gateway settings
    #[serde(default)]
    pub lorawan: Option<LoRaWANConfig>,

    /// The `ble:` section, kept as raw YAML.
    ///
    /// EDIT (control-ble integration). It used to be `crate::libs::ble::BleConfig`, a struct this
    /// crate owned. It is now `control_ble::config::BleConfig`'s, and that type is
    /// `deny_unknown_fields` — which is the point: a leftover v1 key (`pin`, `enable_terminal`)
    /// must be *named* at start-up, not silently ignored, because ignoring `enable_terminal` would
    /// leave an operator believing the BLE terminal still has a switch when CP-1 removed the
    /// terminal itself.
    ///
    /// Held as `serde_yaml::Value` rather than parsed here so that a malformed `ble:` section is
    /// fatal to Bluetooth alone and not to the whole agent: a Hub with a bad BLE section still
    /// measures temperature (CP-2). [`Config::ble_section_yaml`] is where it becomes a `BleConfig`.
    #[serde(default)]
    pub ble: serde_yaml::Value,

    /// The `eye:` section, kept as raw YAML for the same reason.
    ///
    /// EDIT (control-ble integration): the crate reads this through `ConfigStore`, straight off the
    /// file, so nothing in this process needs it as a struct any more. Keeping the key here is what
    /// stops `serde` dropping it on the next whole-config rewrite.
    #[serde(default, rename = "eye")]
    pub beacon: serde_yaml::Value,
}

impl Config {
    /// The `ble:` section as a standalone YAML document, ready for
    /// `control_ble::config::BleConfig::from_device_config_yaml`.
    ///
    /// EDIT (control-ble integration). Round-tripping through YAML rather than handing over the
    /// `Value` keeps one parser in charge of the schema — the crate's — so this crate cannot
    /// accidentally become a second, laxer definition of what a valid `ble:` section is.
    ///
    /// An absent section yields `ble: {}`, which the crate then rejects for want of a `mode`. That
    /// is deliberate: `ble.mode` has no serde default, because a Hub that silently picked a role
    /// would be a Hub whose radio behaviour nobody chose.
    pub fn ble_section_yaml(&self) -> Result<String, Box<dyn std::error::Error>> {
        let mut doc = serde_yaml::Mapping::new();
        doc.insert(
            serde_yaml::Value::String("ble".to_string()),
            if self.ble.is_null() {
                serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
            } else {
                self.ble.clone()
            },
        );
        Ok(serde_yaml::to_string(&serde_yaml::Value::Mapping(doc))?)
    }

    /// Load configuration from a YAML file.
    ///
    /// Runs forward migrations on the file first (see
    /// `crate::libs::config_migrations`) — older yaml shapes on persisted
    /// device installs are upgraded in place with a backup so subsequent
    /// boots read the new shape natively.
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let content = match crate::libs::config_migrations::migrate_and_persist(path.as_ref()) {
            Ok(s) => s,
            Err(e) => {
                // Migration failed (e.g. file unreadable or yaml newer than
                // firmware). Fall back to raw read so the typed parser can
                // produce a more user-friendly error if the file is just
                // malformed, or surface the migration error if it's fatal.
                //
                // Logged only when the message changes. `from_file` is on a
                // periodic path — the display loop reconciles every
                // `CONFIG_RECONCILE_MS` and the sensor monitor hot-reloads
                // intervals — so a *stable* failure (a yaml newer than the
                // firmware, which is the normal state on a device that ran a
                // later build) otherwise repeats forever: measured at 32 lines
                // in 4 minutes on FIBER-CE3D59F8, ~11k/day, burying everything
                // else in the journal. De-duplicating on the message keeps the
                // first occurrence and any genuinely new failure.
                let msg = e.to_string();
                static LAST_MIGRATION_WARNING: std::sync::Mutex<Option<String>> =
                    std::sync::Mutex::new(None);
                if let Ok(mut last) = LAST_MIGRATION_WARNING.lock() {
                    if last.as_deref() != Some(msg.as_str()) {
                        eprintln!(
                            "[config] migration failed: {} — falling back to direct read",
                            msg
                        );
                        *last = Some(msg);
                    }
                }
                fs::read_to_string(path.as_ref())?
            }
        };
        let mut config: Config = serde_yaml::from_str(&content)?;

        // Validate TLS config for production safety (EU MDR Annex I, 17.2)
        if let Some(ref mut mqtt) = config.mqtt {
            if let Some(ref mut tls) = mqtt.tls {
                tls.validate();
            }
        }

        // Canonicalize dev_eui to lowercase across all LoRaWAN sensor entries.
        // ChirpStack lowercases the dev_eui in every uplink (chirpstack.rs ::
        // parse_uplink), and SQLite's default BINARY collation is
        // case-sensitive — a YAML carrying "70B3D5…" would never match the
        // "70b3d5…" rows written by save-and-feed, masking provisioning epoch
        // bumps and de-provisioning markers. Normalizing on load means every
        // downstream comparison (config_applier search, lorawan state lookup,
        // storage writes) sees one canonical form.
        if let Some(ref mut lorawan) = config.lorawan {
            for sensor in lorawan.sensors.iter_mut() {
                sensor.dev_eui = sensor.dev_eui.to_lowercase();
            }
        }

        // Same canonicalization for display lines: an uppercase DevEUI here
        // would never match the lowercased dev_eui in LoRaWANSensorState, so
        // the row would render as "unknown sensor" forever with no hint why.
        //
        // BLE MACs go the other way, to uppercase: BeaconSensorState::tags is keyed
        // by uppercase MAC. Same failure mode, opposite direction — normalizing
        // both to one case would fix one lookup and break the other.
        for line in config.display.custom_lines.iter_mut() {
            if let Some(dev_eui) = line.dev_eui.as_mut() {
                *dev_eui = dev_eui.to_lowercase();
            }
            if let Some(mac) = line.mac.as_mut() {
                *mac = mac.to_uppercase();
            }
        }

        Ok(config)
    }

    /// Load configuration from default location (/data/fiber/config/fiber.config.yaml),
    /// then let the rendered control config override the MQTT topic root.
    pub fn load_default() -> Result<Self, Box<dyn std::error::Error>> {
        let mut config = Self::from_file("/data/fiber/config/fiber.config.yaml")?;
        let rendered = rendered_control_config_path();
        config.apply_rendered_topic_prefix(&rendered);
        config.apply_rendered_broker_override(&rendered);
        Ok(config)
    }

    /// Take `mqtt.topic_prefix` from `/run/proximos/control/config.yaml` when it is
    /// there (plan/portal/mqtt/design.md §9.2).
    ///
    /// **Why this precedence and not the other way round.** The two halves must publish
    /// and subscribe under one root, and a half-flip is silent: the portal subscribes
    /// only under its own root (`{prefix}/+/…`, no top-level wildcard), so the broker never
    /// delivers the other root's frames to it at all, and its subscribe succeeds against a
    /// prefix nothing publishes — no error is raised anywhere, not even an `unhandled:`
    /// counter moves, the readings simply stop. `fiber.config.yaml` is a per-device file an operator may
    /// have edited; the rendered file is written from the same `portal.topic_prefix` key
    /// the portal itself is rendered from, every boot. So on a rendered Hub the two are
    /// one value by construction, and this file cannot drift from it.
    ///
    /// Absent or unreadable is not an error: a bare checkout, a dev box, or a v1-style
    /// Hub whose renderer has not run keeps `fiber.config.yaml`'s value, whose default
    /// is the same string. A malformed one is ignored for the same reason — `radio_gate`
    /// treats an unreadable rendered config as "not rendered", and the topic root is not
    /// the field to invent a new failure mode for.
    fn apply_rendered_topic_prefix(&mut self, path: &str) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(&text) else {
            return;
        };
        if let Some(prefix) = doc
            .get("mqtt")
            .and_then(|m| m.get("topic_prefix"))
            .and_then(|p| p.as_str())
            .filter(|p| !p.is_empty())
        {
            if let Some(mqtt) = self.mqtt.as_mut() {
                mqtt.publish.topic_prefix = prefix.to_string();
            }
        }
    }

    /// Take the Client's `mqtt.broker` + `mqtt.tls` from `/run/proximos/control/config.yaml`
    /// when the renderer emitted them (plan/control/cluster.md §4.4).
    ///
    /// A client-role Hub's agent is a device on the Server's broker. The renderer writes
    /// that broker here, every boot, from `/data/proximos/cluster/`, so the seeded
    /// `fiber.config.yaml` is never edited — v1 rewrote it in place. Read the same way as
    /// [`Self::apply_rendered_topic_prefix`], and with the same precedence: the rendered
    /// file wins.
    ///
    /// **Both blocks or neither, and the TLS half must be on.** The override exists only
    /// to reach a remote broker with a password, so applying a broker without `tls.enabled:
    /// true` and a CA path would be the one outcome this must never produce: a peer
    /// password sent in the clear. A partial or malformed override is therefore refused
    /// whole and logged, and the agent keeps the seeded broker — the loopback one, which is
    /// where a Hub with a damaged override is safest. Absent is silent: that is every
    /// Server.
    ///
    /// **The password is not in that file.** It is 0640 root:proximos, so
    /// `proximos-control` could read it; the override names `password_file`
    /// (`/data/proximos/cluster/peer_password`, 0600 root) and this process, which runs as
    /// root, reads it here. An unreadable or invalid password file refuses the override
    /// whole, like any other half: no username without its password.
    ///
    /// The client id, keep-alive, LWT and the rest of `mqtt:` stay the seeded file's; only
    /// where to connect and as whom move. A missing CA *file* is not judged here — the
    /// MQTT monitor refuses a non-loopback broker without one, and says so, on every
    /// attempt (`mqtt::monitor::plan_transport`).
    fn apply_rendered_broker_override(&mut self, path: &str) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(&text) else {
            return;
        };
        match rendered_broker_override(&doc) {
            Ok(None) => {}
            Ok(Some((broker, tls))) => {
                let Some(mqtt) = self.mqtt.as_mut() else {
                    eprintln!(
                        "[config] {path} names broker {}:{} but this config has no mqtt: \
                         section to apply it to; MQTT stays off",
                        broker.host, broker.port
                    );
                    return;
                };
                eprintln!(
                    "[config] MQTT broker from {path}: {}:{} as {} (TLS, CA {})",
                    broker.host,
                    broker.port,
                    broker.username.as_deref().unwrap_or("-"),
                    tls.ca_cert_path
                );
                mqtt.broker.host = broker.host;
                mqtt.broker.port = broker.port;
                mqtt.broker.username = broker.username;
                mqtt.broker.password = broker.password;
                mqtt.broker.from_cluster_override = broker.from_cluster_override;
                mqtt.tls = Some(tls);
            }
            Err(why) => eprintln!(
                "[config] ERROR: ignoring the mqtt.broker override in {path}: {why}; \
                 the agent stays on its configured broker"
            ),
        }
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
                    critical_threshold_percent: 10,
                },
                ac_power: AcPowerConfig {
                    detection_threshold_mv: 12000,
                    battery_mode_threshold_mv: 11000,
                    dc_connect_mv: default_dc_connect_mv(),
                    dc_disconnect_mv: default_dc_disconnect_mv(),
                },
                led_blink: LedBlinkConfig { toggle_count: 8 },
                standby: StandbyConfig::default(),
            },
            sensors: SensorConfig {
                num_lines: 8,
                sample_interval_s: 5,
                aggregation_interval_s: 60,
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
                // 2.5 GB cap of live pages. An upper bound only: /data
                // is ~2.9 GB on a CM4 Hub and shared with portal.db, so
                // the free-space floor in storage/retention.rs is what
                // keeps the partition from filling (#124).
                max_size_gb: 3,
                max_size_mb: Some(2500),
                hmac_secret_path: default_hmac_secret_path(),
                // The built-in default schedules nothing. A deployment opts in
                // through its YAML, exactly as the portal half does.
                maintenance_window: None,
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
                screen_timeout_secs: 3600,
            },
            display: DisplayConfig::default(), // built-in overview layout
            mqtt: None,                        // MQTT disabled by default
            lorawan: None,                     // LoRaWAN disabled by default
            // Both sections are raw YAML passthrough now (see the field docs). Null rather than an
            // empty mapping, so `ble_section_yaml` can tell "absent" from "present and empty" —
            // and so a Config built in code does not claim to configure a radio.
            setup: SetupSection::default(), // nobody has set this Hub up
            ble: serde_yaml::Value::Null,
            beacon: serde_yaml::Value::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped `fiber.config.yaml`, parsed. `default_config()` has `mqtt: None`
    /// ("MQTT disabled by default"), so it cannot exercise the prefix at all — and the
    /// shipped file is the thing whose value the rendered config has to beat anyway.
    fn shipped_config() -> Config {
        serde_yaml::from_str(include_str!("../../fiber.config.yaml"))
            .expect("the shipped fiber.config.yaml parses as a Config")
    }

    /// One root for both halves (plan/portal/mqtt/design.md §9.2): the rendered control
    /// config wins over this file's own value, because it is written from the same
    /// `portal.topic_prefix` key the portal is rendered from and this file is one an
    /// operator may have edited.
    #[test]
    fn the_rendered_control_config_overrides_this_files_topic_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let rendered = dir.path().join("control.yaml");
        std::fs::write(
            &rendered,
            "radio:\n  mode: off\nmqtt:\n  topic_prefix: \"rendered-root\"\n",
        )
        .expect("write");

        let mut config = shipped_config();
        config
            .mqtt
            .as_mut()
            .expect("the shipped file carries mqtt")
            .publish
            .topic_prefix = "from-the-file".to_string();

        config.apply_rendered_topic_prefix(rendered.to_str().unwrap());
        assert_eq!(
            config.mqtt.as_ref().unwrap().publish.topic_prefix,
            "rendered-root"
        );
    }

    /// A bare checkout, a dev box, or a Hub whose renderer has not run keeps this file's
    /// value — absent is not an error, and neither is malformed. `radio_gate` reads an
    /// unreadable rendered config the same way, and the topic root is not the field to
    /// invent a new failure mode for.
    #[test]
    fn an_absent_or_malformed_rendered_config_leaves_the_prefix_alone() {
        let dir = tempfile::tempdir().expect("tempdir");

        for rendered in [
            dir.path().join("does-not-exist.yaml"),
            {
                let p = dir.path().join("garbage.yaml");
                std::fs::write(&p, "this: [is: not: yaml").expect("write");
                p
            },
            {
                let p = dir.path().join("no-mqtt-block.yaml");
                std::fs::write(&p, "radio:\n  mode: off\n").expect("write");
                p
            },
            {
                let p = dir.path().join("empty-prefix.yaml");
                std::fs::write(&p, "mqtt:\n  topic_prefix: \"\"\n").expect("write");
                p
            },
        ] {
            let mut config = shipped_config();
            config
                .mqtt
                .as_mut()
                .expect("the shipped file carries mqtt")
                .publish
                .topic_prefix = "from-the-file".to_string();

            config.apply_rendered_topic_prefix(rendered.to_str().unwrap());
            assert_eq!(
                config.mqtt.as_ref().unwrap().publish.topic_prefix,
                "from-the-file",
                "{rendered:?} must not change the prefix"
            );
        }
    }

    /// The peer password the client fixtures put in their password file.
    const PEER_PASSWORD: &str = "Zm9vYmFyYmF6cXV4cXV1eA";

    /// What the renderer writes on a client-role Hub (proximos-render's
    /// `tests/golden/client/control-config.yaml`, values and shape), with the password file
    /// it names placed in `dir` instead of `/data/proximos/cluster/peer_password`.
    fn client_rendered(dir: &tempfile::TempDir) -> String {
        let pw = dir.path().join("peer_password");
        std::fs::write(&pw, PEER_PASSWORD).expect("write password file");
        format!(
            "radio:\n  mode: lorawan\nmqtt:\n  topic_prefix: \"proximos\"\n\
             \x20 broker:\n    host: \"proximos-1a2b3c4d.local\"\n    port: 8883\n\
             \x20   username: \"peer-proximos-5e6f7a8b\"\n    password_file: \"{}\"\n\
             \x20 tls:\n    enabled: true\n    ca_cert_path: \"/data/proximos/cluster/ca.crt\"\n",
            pw.display()
        )
    }

    fn write_rendered(dir: &tempfile::TempDir, text: &str) -> String {
        let p = dir.path().join("control.yaml");
        std::fs::write(&p, text).expect("write");
        p.to_str().unwrap().to_string()
    }

    /// plan/control/cluster.md §4.4: the rendered override moves the agent's broker to the
    /// Server's TLS listener under the peer account, and nothing else in `mqtt:`. The
    /// password comes from the file the override names, not from the override.
    #[test]
    fn the_rendered_broker_override_retargets_the_agent_at_the_server() {
        let dir = tempfile::tempdir().expect("tempdir");
        let rendered = write_rendered(&dir, &client_rendered(&dir));

        let mut config = shipped_config();
        let before = config.mqtt.clone().expect("the shipped file carries mqtt");
        assert!(
            !before.tls.as_ref().is_some_and(|t| t.enabled),
            "the shipped file is plaintext loopback"
        );

        config.apply_rendered_topic_prefix(&rendered);
        config.apply_rendered_broker_override(&rendered);
        let mqtt = config.mqtt.as_ref().unwrap();

        assert_eq!(mqtt.broker.host, "proximos-1a2b3c4d.local");
        assert_eq!(mqtt.broker.port, 8883);
        assert_eq!(
            mqtt.broker.username.as_deref(),
            Some("peer-proximos-5e6f7a8b")
        );
        assert_eq!(mqtt.broker.password.as_deref(), Some(PEER_PASSWORD));
        assert!(
            mqtt.broker.from_cluster_override,
            "marked as the Server's broker, which is what drives the hosts entry"
        );
        assert!(
            !before.broker.from_cluster_override,
            "the seeded broker is not"
        );
        let tls = mqtt.tls.as_ref().expect("TLS comes with the broker");
        assert!(tls.enabled);
        assert_eq!(tls.ca_cert_path, "/data/proximos/cluster/ca.crt");
        assert!(!tls.insecure_skip_verify);
        assert!(tls.client_cert_path.is_none() && tls.client_key_path.is_none());

        // Untouched: who the agent is, and what it says when it goes away.
        assert_eq!(mqtt.broker.client_id, before.broker.client_id);
        assert_eq!(mqtt.last_will.topic, before.last_will.topic);
        assert_eq!(mqtt.publish.topic_prefix, "proximos");
        assert_eq!(mqtt.enabled, before.enabled);
    }

    /// The file's trailing newline (a hand-written one) is not part of the password.
    #[test]
    fn the_password_file_is_trimmed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let text = client_rendered(&dir);
        std::fs::write(
            dir.path().join("peer_password"),
            format!("{PEER_PASSWORD}\n"),
        )
        .unwrap();
        let rendered = write_rendered(&dir, &text);
        let mut config = shipped_config();
        config.apply_rendered_broker_override(&rendered);
        assert_eq!(
            config.mqtt.unwrap().broker.password.as_deref(),
            Some(PEER_PASSWORD)
        );
    }

    /// A Server's rendered file has no override, and the seeded broker stays exactly as it
    /// was — the case every Hub in the field is in today.
    #[test]
    fn no_rendered_override_leaves_the_seeded_broker_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        for text in [
            "radio:\n  mode: lorawan\nmqtt:\n  topic_prefix: \"proximos\"\n",
            "radio:\n  mode: off\n",
            "this: [is: not: yaml",
        ] {
            let rendered = write_rendered(&dir, text);
            let mut config = shipped_config();
            let before = config.mqtt.clone().unwrap();
            config.apply_rendered_broker_override(&rendered);
            let after = config.mqtt.as_ref().unwrap();
            assert_eq!(after.broker.host, before.broker.host, "{text:?}");
            assert_eq!(after.broker.port, before.broker.port, "{text:?}");
            assert_eq!(after.broker.username, before.broker.username, "{text:?}");
            assert!(!after.tls.as_ref().unwrap().enabled, "{text:?}");
        }
        let mut config = shipped_config();
        config.apply_rendered_broker_override(
            dir.path().join("does-not-exist.yaml").to_str().unwrap(),
        );
        assert!(!config.mqtt.as_ref().unwrap().tls.as_ref().unwrap().enabled);
    }

    /// Both blocks or neither, TLS on, and a username only with a readable, valid password
    /// file: anything short of that is refused whole, so a damaged override can never put
    /// a peer password on a plaintext remote connection, nor a username on one without its
    /// password. The agent keeps its seeded (loopback) broker instead.
    #[test]
    fn a_partial_or_plaintext_override_is_refused_whole() {
        let dir = tempfile::tempdir().expect("tempdir");
        let good_pw = dir.path().join("good");
        std::fs::write(&good_pw, PEER_PASSWORD).unwrap();
        let short_pw = dir.path().join("short");
        std::fs::write(&short_pw, "0123456789abcde").unwrap();
        let empty_pw = dir.path().join("empty");
        std::fs::write(&empty_pw, "\n").unwrap();
        let two_pw = dir.path().join("two");
        std::fs::write(&two_pw, "0123456789abcdef 0123456789abcdef").unwrap();
        let missing_pw = dir.path().join("missing");

        let tls_line = "\n  tls:\n    enabled: true\n    ca_cert_path: \"/c\"";
        let broker_with = |pw: &std::path::Path| {
            format!(
                "\n  broker:\n    host: \"server.local\"\n    port: 8883\n\
                 \x20   username: \"peer-x\"\n    password_file: \"{}\"",
                pw.display()
            )
        };
        let broker = broker_with(&good_pw);
        let cases = [
            ("broker without tls", format!("mqtt:{broker}\n")),
            ("tls without broker", format!("mqtt:{tls_line}\n")),
            (
                "tls disabled",
                format!("mqtt:{broker}\n  tls:\n    enabled: false\n    ca_cert_path: \"/c\"\n"),
            ),
            (
                "tls enabled missing",
                format!("mqtt:{broker}\n  tls:\n    ca_cert_path: \"/c\"\n"),
            ),
            (
                "empty CA path",
                format!("mqtt:{broker}\n  tls:\n    enabled: true\n    ca_cert_path: \"\"\n"),
            ),
            (
                "no host",
                format!("mqtt:\n  broker:\n    port: 8883\n    username: \"peer-x\"\n    password_file: \"{}\"{tls_line}\n", good_pw.display()),
            ),
            (
                "bad port",
                format!("mqtt:\n  broker:\n    host: \"h\"\n    port: 70000{tls_line}\n"),
            ),
            (
                "username without password_file",
                format!("mqtt:\n  broker:\n    host: \"h\"\n    port: 8883\n    username: \"peer-x\"{tls_line}\n"),
            ),
            (
                "password_file without username",
                format!("mqtt:\n  broker:\n    host: \"h\"\n    port: 8883\n    password_file: \"{}\"{tls_line}\n", good_pw.display()),
            ),
            (
                "an inline password, even beside a password_file",
                format!("mqtt:{broker}\n    password: \"{PEER_PASSWORD}\"{tls_line}\n"),
            ),
            (
                "an inline password instead of a password_file",
                format!("mqtt:\n  broker:\n    host: \"h\"\n    port: 8883\n    username: \"peer-x\"\n    password: \"{PEER_PASSWORD}\"{tls_line}\n"),
            ),
            ("a missing password file", format!("mqtt:{}{tls_line}\n", broker_with(&missing_pw))),
            ("an empty password file", format!("mqtt:{}{tls_line}\n", broker_with(&empty_pw))),
            ("a short password", format!("mqtt:{}{tls_line}\n", broker_with(&short_pw))),
            ("two tokens", format!("mqtt:{}{tls_line}\n", broker_with(&two_pw))),
            (
                "an IPv6-literal host",
                format!("mqtt:\n  broker:\n    host: \"fd00::1\"\n    port: 8883{tls_line}\n"),
            ),
            (
                "a bracketed IPv6-literal host",
                format!("mqtt:\n  broker:\n    host: \"[fd00::1]\"\n    port: 8883{tls_line}\n"),
            ),
        ];
        for (what, text) in cases {
            let doc: serde_yaml::Value =
                serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("{what}: {e}\n{text}"));
            let err = rendered_broker_override(&doc).expect_err(what);
            assert!(
                !err.contains(PEER_PASSWORD),
                "{what}: the refusal names no secret"
            );

            let rendered = write_rendered(&dir, &text);
            let mut config = shipped_config();
            let before = config.mqtt.clone().unwrap();
            config.apply_rendered_broker_override(&rendered);
            let after = config.mqtt.as_ref().unwrap();
            assert_eq!(after.broker.host, before.broker.host, "{what}");
            assert_eq!(after.broker.username, before.broker.username, "{what}");
            assert_eq!(after.broker.password, before.broker.password, "{what}");
            assert!(!after.tls.as_ref().unwrap().enabled, "{what}");
        }
    }

    /// An unreadable password file — the 0600 root file read by a process that is not
    /// root — is a refusal like a missing one. Skipped when the test runs as root, which
    /// can read any mode.
    #[test]
    fn an_unreadable_password_file_refuses_the_override() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let text = client_rendered(&dir);
        let pw = dir.path().join("peer_password");
        std::fs::set_permissions(&pw, std::fs::Permissions::from_mode(0o000)).unwrap();
        let doc: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
        let err = rendered_broker_override(&doc).expect_err("unreadable");
        assert!(err.contains("cannot be read"), "{err}");
    }

    /// `load_default`'s order: the prefix and the broker come from the same rendered file,
    /// and neither application undoes the other.
    #[test]
    fn the_override_and_the_prefix_apply_together_in_either_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let rendered = write_rendered(
            &dir,
            &client_rendered(&dir).replace("topic_prefix: \"proximos\"", "topic_prefix: \"r\""),
        );
        let mut a = shipped_config();
        a.apply_rendered_topic_prefix(&rendered);
        a.apply_rendered_broker_override(&rendered);
        let mut b = shipped_config();
        b.apply_rendered_broker_override(&rendered);
        b.apply_rendered_topic_prefix(&rendered);
        for c in [a, b] {
            let m = c.mqtt.unwrap();
            assert_eq!(m.publish.topic_prefix, "r");
            assert_eq!(m.broker.port, 8883);
            assert_eq!(m.broker.password.as_deref(), Some(PEER_PASSWORD));
            assert!(m.tls.unwrap().enabled);
        }
    }

    /// The shipped default has to equal the portal's, for the bare-checkout case the two
    /// tests above leave in place. The portal's half of this pin is
    /// `portal/src/mqtt/mod.rs::the_command_and_node_roots_are_the_same_string`.
    #[test]
    fn the_shipped_default_prefix_is_proximos() {
        let text = include_str!("../../fiber.config.yaml");
        assert!(
            text.contains("topic_prefix: proximos"),
            "fiber.config.yaml must ship the same root the portal defaults to"
        );
    }

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
        assert_eq!(config.power.battery.critical_threshold_percent, 10);
    }

    #[test]
    fn test_serial_config() {
        let config = Config::default_config();
        assert_eq!(config.serial.port, "/dev/ttyAMA4");
        assert_eq!(config.serial.baud_rate, 115200u32);
    }

    #[test]
    fn lorawan_is_enabled_unless_a_config_says_otherwise() {
        // Until the cluster needed it, nothing read `lorawan.enabled` — the
        // monitor started on hardware detection alone. Every deployed config
        // that omits the key must therefore keep reporting, or an upgrade would
        // silence units that never opted out of anything.
        let implied: LoRaWANConfig = serde_yaml::from_str("chirpstack_mqtt_port: 1883").unwrap();
        assert!(implied.enabled, "an absent key must not disable LoRaWAN");

        let off: LoRaWANConfig = serde_yaml::from_str("enabled: false").unwrap();
        assert!(
            !off.enabled,
            "an explicit false is how a cluster follower stops publishing"
        );

        let on: LoRaWANConfig = serde_yaml::from_str("enabled: true").unwrap();
        assert!(on.enabled);
    }

    #[test]
    fn shipped_sensors_yaml_has_no_battery_default() {
        // Replaces `shipped_sensors_yaml_has_voltage_low_only_defaults`, which
        // asserted the opposite. That test was quarantined for drifting from the
        // shipped values and was un-quarantined on dev by 7fea467 (application#19);
        // it is deleted here rather than repaired, because the value it pinned is
        // gone on purpose. Battery must NOT be auto-armed: the node raises its
        // own low-battery alarm on fPort 3, so a default here gave every node a
        // duplicate alarm nobody had configured.
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fiber.sensors.config.yaml");
        let cfg = SensorFileConfig::from_file(&path)
            .expect("shipped fiber.sensors.config.yaml must parse");
        assert!(
            cfg.common_lorawan_field_thresholds.get("voltage").is_none(),
            "voltage must have no global default — the node alarms on it natively"
        );
    }

    #[test]
    fn shipped_sensors_yaml_mirrors_temperature_for_external_probes() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fiber.sensors.config.yaml");
        let cfg = SensorFileConfig::from_file(&path).unwrap();
        let temp = cfg
            .common_lorawan_field_thresholds
            .get("temperature")
            .expect("temperature default present");
        for name in [
            "ext_temperature_1",
            "ext_temperature_2",
            "machine_probe_temperature_1",
            "machine_probe_temperature_2",
            "w1_1_temperature",
            "w1_2_temperature_aux",
            "w1_4_temperature",
        ] {
            let other = cfg
                .common_lorawan_field_thresholds
                .get(name)
                .unwrap_or_else(|| panic!("{} default missing", name));
            assert_eq!(other.critical_low, temp.critical_low, "{}", name);
            assert_eq!(other.warning_low, temp.warning_low, "{}", name);
            assert_eq!(other.warning_high, temp.warning_high, "{}", name);
            assert_eq!(other.critical_high, temp.critical_high, "{}", name);
        }
    }

    #[test]
    fn effective_thresholds_drop_a_field_the_registry_no_longer_alarms() {
        // The withdrawal has to hold for config written BEFORE it: a deployed YAML
        // default and a per-sensor override both used to keep battery alarming,
        // because this function was purely data-driven.
        let mut defaults = HashMap::new();
        defaults.insert(
            "voltage".to_string(),
            FieldThresholdBounds {
                critical_low: Some(2.4),
                warning_low: Some(2.5),
                warning_high: None,
                critical_high: None,
            },
        );
        defaults.insert(
            "temperature".to_string(),
            FieldThresholdBounds {
                critical_low: Some(0.0),
                warning_low: Some(5.0),
                warning_high: Some(30.0),
                critical_high: Some(40.0),
            },
        );

        let sensor = LoRaWANSensorConfig {
            dev_eui: "aabb".into(),
            name: None,
            serial_number: None,
            location: None,
            enabled: true,
            field_thresholds: vec![FieldThreshold {
                field: "voltage".into(),
                critical_low: Some(2.0),
                warning_low: Some(2.1),
                warning_high: None,
                critical_high: None,
            }],
            disarmed_fields: Vec::new(),
        };

        let out = effective_field_thresholds(Some(&sensor), &defaults);
        let fields: Vec<&str> = out.iter().map(|t| t.field.as_str()).collect();
        assert!(
            !fields.contains(&"voltage"),
            "battery must not alarm: {fields:?}"
        );
        assert!(
            fields.contains(&"temperature"),
            "other fields unaffected: {fields:?}"
        );
    }

    #[test]
    fn effective_thresholds_keep_a_field_the_registry_does_not_know() {
        // A name the registry has never heard of is more likely a newer config than
        // a withdrawn field, so it must NOT be silently disarmed.
        let mut defaults = HashMap::new();
        defaults.insert(
            "future_quantity".to_string(),
            FieldThresholdBounds {
                critical_low: Some(1.0),
                warning_low: Some(2.0),
                warning_high: Some(8.0),
                critical_high: Some(9.0),
            },
        );
        let out = effective_field_thresholds(None, &defaults);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].field, "future_quantity");
    }

    #[test]
    fn shipped_sensors_yaml_omits_defaults_for_unbounded_fields() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fiber.sensors.config.yaml");
        let cfg = SensorFileConfig::from_file(&path).unwrap();
        for name in ["illuminance", "pressure", "altitude"] {
            assert!(
                cfg.common_lorawan_field_thresholds.get(name).is_none(),
                "{} should NOT have a global default",
                name
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

    /// Switching an alarm off has to beat the shipped default, because nothing
    /// else can.
    ///
    /// The YAML defaults arm temperature, humidity and the probe fields on every
    /// node the moment it is paired — measured on a bench unit with no
    /// `field_thresholds` of its own at all, which still reported eight armed
    /// bands. Clearing the numbers does not help: an omitted bound inherits the
    /// default (see `resolve_field_threshold`), so before this there was no way
    /// to silence a quantity from anywhere in the product.
    #[test]
    fn a_disarmed_field_beats_the_shipped_default() {
        let mut defaults = HashMap::new();
        defaults.insert(
            "temperature".to_string(),
            FieldThresholdBounds {
                critical_low: Some(0.0),
                warning_low: Some(5.0),
                warning_high: Some(30.0),
                critical_high: Some(40.0),
            },
        );
        defaults.insert(
            "humidity".to_string(),
            FieldThresholdBounds {
                critical_low: Some(15.0),
                warning_low: Some(25.0),
                warning_high: Some(75.0),
                critical_high: Some(85.0),
            },
        );

        let armed = LoRaWANSensorConfig {
            dev_eui: "aabb".into(),
            name: None,
            serial_number: None,
            location: None,
            enabled: true,
            field_thresholds: Vec::new(),
            disarmed_fields: Vec::new(),
        };
        let before = effective_field_thresholds(Some(&armed), &defaults);
        assert_eq!(before.len(), 2, "both defaults arm the node to begin with");

        let mut off = armed.clone();
        off.disarmed_fields = vec!["temperature".to_string()];
        let after = effective_field_thresholds(Some(&off), &defaults);
        assert_eq!(after.len(), 1);
        assert_eq!(
            after[0].field, "humidity",
            "only temperature was switched off"
        );
    }

    /// Off must also survive the bounds being reset — the two controls are
    /// independent, so clearing the numbers cannot quietly re-arm the alarm.
    #[test]
    fn a_disarmed_field_stays_off_even_with_its_own_bounds_set() {
        let mut defaults = HashMap::new();
        defaults.insert(
            "temperature".to_string(),
            FieldThresholdBounds {
                critical_low: Some(0.0),
                warning_low: Some(5.0),
                warning_high: Some(30.0),
                critical_high: Some(40.0),
            },
        );
        let cfg = LoRaWANSensorConfig {
            dev_eui: "aabb".into(),
            name: None,
            serial_number: None,
            location: None,
            enabled: true,
            field_thresholds: vec![FieldThreshold {
                field: "temperature".into(),
                critical_low: Some(1.0),
                warning_low: Some(6.0),
                warning_high: Some(31.0),
                critical_high: Some(41.0),
            }],
            disarmed_fields: vec!["temperature".to_string()],
        };
        assert!(effective_field_thresholds(Some(&cfg), &defaults).is_empty());
    }

    /// Untouched deployments must serialise byte-identically, or every device
    /// would rewrite its config on the next save for no reason.
    #[test]
    fn an_empty_disarmed_list_is_absent_from_the_yaml() {
        let cfg = LoRaWANSensorConfig {
            dev_eui: "x".to_string(),
            name: None,
            serial_number: None,
            location: None,
            enabled: true,
            field_thresholds: Vec::new(),
            disarmed_fields: Vec::new(),
        };
        assert!(!serde_yaml::to_string(&cfg)
            .unwrap()
            .contains("disarmed_fields"));
    }

    /// And an old config without the key still loads.
    #[test]
    fn a_config_without_the_key_loads_as_fully_armed() {
        let cfg: LoRaWANSensorConfig =
            serde_yaml::from_str("dev_eui: aabb\nenabled: true\n").unwrap();
        assert!(cfg.disarmed_fields.is_empty());
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
            disarmed_fields: Vec::new(),
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

#[cfg(test)]
mod display_config_tests {
    use super::*;

    /// The four lines from the feature request, as a Viewer would author them.
    const EXAMPLE_YAML: &str = r#"
custom_lines:
  - source: sticker
    dev_eui: "70b3d57ed0051f2a"
    field: ext_temperature_1
    label: "Stkr1 ext"
  - source: ds18b20
    line: 0
    field: temperature
    label: "Probe 1"
  - source: sticker
    dev_eui: "70b3d57ed0051f31"
    field: humidity
    label: "Stkr2 RH"
    format:
      decimals: 0
  - source: sticker
    dev_eui: "70b3d57ed0051f2a"
    field: voltage
    label: "Stkr1 bat"
    format:
      decimals: 2
      status_char: false
"#;

    /// Build a loadable config file from `default_config()` plus an injected
    /// `display:` section, so the tests exercise the real `Config::from_file`
    /// path (migrations included) without hand-writing every required section.
    fn config_file_with_display(dir: &std::path::Path, display_yaml: &str) -> std::path::PathBuf {
        let base = serde_yaml::to_value(Config::default_config()).unwrap();
        let mut map = match base {
            serde_yaml::Value::Mapping(m) => m,
            other => panic!("expected mapping, got {:?}", other),
        };
        let display: serde_yaml::Value = serde_yaml::from_str(display_yaml).unwrap();
        map.insert(serde_yaml::Value::String("display".to_string()), display);
        let path = dir.join("fiber.config.yaml");
        fs::write(
            &path,
            serde_yaml::to_string(&serde_yaml::Value::Mapping(map)).unwrap(),
        )
        .unwrap();
        path
    }

    #[test]
    fn display_config_defaults_to_empty_when_section_absent() {
        // No `display:` key at all — the built-in layout must be selected.
        let cfg: DisplayConfig = serde_yaml::from_str("{}").unwrap();
        assert!(cfg.custom_lines.is_empty());
    }

    #[test]
    fn default_config_has_empty_display_section() {
        assert!(Config::default_config().display.custom_lines.is_empty());
    }

    #[test]
    fn parse_display_custom_lines_from_yaml() {
        let cfg: DisplayConfig = serde_yaml::from_str(EXAMPLE_YAML).unwrap();
        assert_eq!(cfg.custom_lines.len(), 4);

        let node_ext = &cfg.custom_lines[0];
        assert_eq!(node_ext.source, DisplayLineSource::Node);
        assert_eq!(node_ext.dev_eui.as_deref(), Some("70b3d57ed0051f2a"));
        assert_eq!(node_ext.field, "ext_temperature_1");
        assert_eq!(node_ext.label.as_deref(), Some("Stkr1 ext"));
        assert_eq!(node_ext.line, None);

        let probe = &cfg.custom_lines[1];
        assert_eq!(probe.source, DisplayLineSource::Ds18b20);
        assert_eq!(probe.line, Some(0));
        assert_eq!(probe.dev_eui, None);

        assert_eq!(cfg.custom_lines[2].format.decimals, Some(0));
        assert!(
            cfg.custom_lines[2].format.status_char,
            "unset options keep their default"
        );

        assert_eq!(cfg.custom_lines[3].format.decimals, Some(2));
        assert!(!cfg.custom_lines[3].format.status_char);
    }

    #[test]
    fn display_line_format_defaults_when_format_omitted() {
        let cfg: DisplayConfig = serde_yaml::from_str(
            "custom_lines:\n  - {source: ds18b20, line: 3, field: temperature}\n",
        )
        .unwrap();
        let fmt = &cfg.custom_lines[0].format;
        assert!(fmt.units);
        assert_eq!(fmt.decimals, None);
        assert!(fmt.status_char);
        assert!(fmt.is_default());
    }

    #[test]
    fn default_format_is_omitted_from_serialized_yaml() {
        // Keeps the applier's rewrite of the file free of noise.
        let cfg = DisplayConfig {
            custom_lines: vec![DisplayLine {
                source: DisplayLineSource::Ds18b20,
                line: Some(1),
                dev_eui: None,
                mac: None,
                field: "temperature".to_string(),
                label: None,
                format: DisplayLineFormat::default(),
            }],
        };
        let out = serde_yaml::to_string(&cfg).unwrap();
        assert!(!out.contains("format"), "got: {}", out);
        assert!(!out.contains("dev_eui"), "got: {}", out);
        assert!(!out.contains("label"), "got: {}", out);
    }

    #[test]
    fn unknown_source_line_is_dropped_not_fatal() {
        // A newer Viewer emitting `source: eye` must cost us one row, not boot.
        let yaml = r#"
custom_lines:
  - {source: ds18b20, line: 0, field: temperature}
  - {source: eye, mac: "aa:bb", field: temperature}
  - {source: sticker, dev_eui: "aabb", field: humidity}
"#;
        let cfg: DisplayConfig = serde_yaml::from_str(yaml).expect("must not fail the whole parse");
        assert_eq!(cfg.custom_lines.len(), 2);
        assert_eq!(cfg.custom_lines[0].field, "temperature");
        assert_eq!(cfg.custom_lines[1].field, "humidity");
    }

    #[test]
    fn malformed_line_is_dropped_not_fatal() {
        // Missing the required `field` key.
        let yaml = "custom_lines:\n  - {source: ds18b20, line: 0}\n";
        let cfg: DisplayConfig = serde_yaml::from_str(yaml).expect("must not fail the whole parse");
        assert!(cfg.custom_lines.is_empty());
    }

    #[test]
    fn display_line_dev_eui_lowercased_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_file_with_display(
            dir.path(),
            "custom_lines:\n  - {source: sticker, dev_eui: \"70B3D57ED0051F2A\", field: temperature}\n",
        );
        let cfg = Config::from_file(&path).expect("config must load");
        assert_eq!(
            cfg.display.custom_lines[0].dev_eui.as_deref(),
            Some("70b3d57ed0051f2a")
        );
    }

    #[test]
    fn display_section_round_trips_through_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_file_with_display(dir.path(), EXAMPLE_YAML);
        let cfg = Config::from_file(&path).expect("config must load");
        assert_eq!(cfg.display.custom_lines.len(), 4);
        assert_eq!(cfg.display.custom_lines[3].field, "voltage");
        assert_eq!(cfg.display.custom_lines[3].format.decimals, Some(2));
    }

    #[test]
    fn parse_ble_display_line_from_yaml() {
        let cfg: DisplayConfig = serde_yaml::from_str(
            "custom_lines:\n  - {source: ble, mac: \"7C:D9:F4:13:10:DE\", field: temperature}\n",
        )
        .unwrap();
        let line = &cfg.custom_lines[0];
        assert_eq!(line.source, DisplayLineSource::Ble);
        assert_eq!(line.mac.as_deref(), Some("7C:D9:F4:13:10:DE"));
        assert_eq!(line.field, "temperature");
        assert_eq!(line.line, None);
        assert_eq!(line.dev_eui, None);
    }

    /// Opposite direction to the DevEUI: `BeaconSensorState::tags` is keyed by
    /// uppercase MAC, so a lowercase one in the file would render as an unknown
    /// sensor forever.
    #[test]
    fn display_line_mac_uppercased_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_file_with_display(
            dir.path(),
            "custom_lines:\n  - {source: ble, mac: \"7c:d9:f4:13:10:de\", field: temperature}\n",
        );
        let cfg = Config::from_file(&path).expect("config must load");
        assert_eq!(
            cfg.display.custom_lines[0].mac.as_deref(),
            Some("7C:D9:F4:13:10:DE")
        );
    }

    /// The two normalizations are independent: one file may carry both sources,
    /// and each must end up in its own subsystem's canonical case.
    #[test]
    fn node_and_ble_lines_normalize_in_opposite_directions() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_file_with_display(
            dir.path(),
            "custom_lines:\n  \
             - {source: sticker, dev_eui: \"70B3D57ED0051F2A\", field: temperature}\n  \
             - {source: ble, mac: \"7c:d9:f4:13:10:de\", field: humidity}\n",
        );
        let cfg = Config::from_file(&path).expect("config must load");
        assert_eq!(
            cfg.display.custom_lines[0].dev_eui.as_deref(),
            Some("70b3d57ed0051f2a")
        );
        assert_eq!(
            cfg.display.custom_lines[1].mac.as_deref(),
            Some("7C:D9:F4:13:10:DE")
        );
    }

    /// `mac` is `skip_serializing_if = "Option::is_none"`, so a probe or node
    /// row must not gain an empty key when the applier rewrites the file.
    #[test]
    fn mac_is_omitted_from_serialized_non_ble_lines() {
        let cfg = DisplayConfig {
            custom_lines: vec![DisplayLine {
                source: DisplayLineSource::Ds18b20,
                line: Some(1),
                dev_eui: None,
                mac: None,
                field: "temperature".to_string(),
                label: None,
                format: DisplayLineFormat::default(),
            }],
        };
        let out = serde_yaml::to_string(&cfg).unwrap();
        assert!(!out.contains("mac"), "got: {}", out);
    }
}

#[cfg(test)]
mod chirpstack_api_config_tests {
    use super::*;

    #[test]
    fn validate_rejects_an_unprovisioned_password() {
        let cfg = ChirpStackApiConfig::default();
        let err = cfg
            .validate()
            .expect_err("empty password must not validate");
        assert!(
            err.contains("lorawan.chirpstack.password"),
            "message must name the key an operator has to fix: {err}"
        );
    }

    #[test]
    fn validate_rejects_a_blank_username() {
        let cfg = ChirpStackApiConfig {
            username: "   ".to_string(),
            password: "s3cret".to_string(),
        };
        let err = cfg
            .validate()
            .expect_err("blank username must not validate");
        assert!(err.contains("lorawan.chirpstack.username"), "got: {err}");
    }

    #[test]
    fn validate_accepts_a_populated_pair() {
        let cfg = ChirpStackApiConfig {
            username: "admin".to_string(),
            password: "s3cret".to_string(),
        };
        assert!(cfg.validate().is_ok());
    }

    /// The password must not reach a log through `{:?}` on this struct or on
    /// `LoRaWANConfig`, which derives `Debug` and is printed during startup.
    #[test]
    fn debug_redacts_the_password() {
        let cfg = ChirpStackApiConfig {
            username: "admin".to_string(),
            password: "hunter2".to_string(),
        };
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("hunter2"), "leaked: {rendered}");
        assert!(rendered.contains("***"), "got: {rendered}");

        let lorawan = LoRaWANConfig {
            chirpstack: cfg,
            ..LoRaWANConfig::default()
        };
        let rendered = format!("{lorawan:?}");
        assert!(
            !rendered.contains("hunter2"),
            "leaked through LoRaWANConfig: {rendered}"
        );
    }

    #[test]
    fn debug_distinguishes_unset_from_set() {
        let unset = format!("{:?}", ChirpStackApiConfig::default());
        assert!(unset.contains("<unset>"), "got: {unset}");
    }

    /// A `lorawan:` block carrying only `chirpstack:` must deserialize — every
    /// other field is `#[serde(default)]`. Guards the shape shipped in
    /// `fiber.config.yaml`.
    #[test]
    fn deserializes_a_lorawan_block_with_only_chirpstack() {
        let yaml = r#"
chirpstack:
  username: admin
  password: ""
"#;
        let cfg: LoRaWANConfig = serde_yaml::from_str(yaml).expect("must deserialize");
        assert_eq!(cfg.chirpstack.username, "admin");
        assert_eq!(cfg.chirpstack.password, "");
        assert!(cfg.chirpstack.validate().is_err());
        // Untouched defaults.
        assert_eq!(cfg.chirpstack_mqtt_host, "localhost");
        assert!(cfg.sensors.is_empty());
    }

    /// An absent `chirpstack:` block yields the factory username and no
    /// password — i.e. a deployed config that predates this field fails the
    /// credential check rather than silently authenticating as admin/admin.
    #[test]
    fn absent_block_defaults_to_username_only() {
        let cfg: LoRaWANConfig = serde_yaml::from_str("enabled: true\n").expect("must deserialize");
        assert_eq!(cfg.chirpstack.username, "admin");
        assert_eq!(cfg.chirpstack.password, "");
        assert!(cfg.chirpstack.validate().is_err());
    }
}

#[cfg(test)]
mod maintenance_window_tests {
    use super::*;

    fn window(start: &str, minutes: u32) -> MaintenanceWindow {
        MaintenanceWindow {
            start_utc: start.to_string(),
            duration_minutes: minutes,
        }
    }

    #[test]
    fn a_window_opens_at_its_own_hour_and_closes_on_time() {
        let w = window("02:30", 90);
        assert!(!w.contains(149), "02:29");
        assert!(w.contains(150), "02:30");
        assert!(w.contains(239), "03:59");
        assert!(!w.contains(240), "04:00 — closed");
    }

    #[test]
    fn a_window_may_straddle_midnight() {
        let w = window("23:30", 90);
        assert!(w.contains(23 * 60 + 30));
        assert!(
            w.contains(0),
            "00:00 is inside a window that opened at 23:30"
        );
        assert!(w.contains(59));
        assert!(!w.contains(60), "01:00 — closed");
    }

    /// A misparsed window must never silently become midnight: that would put a
    /// full rewrite — which locks out every writer for its whole duration — at
    /// an hour nobody chose, on a device that monitors patients.
    #[test]
    fn a_malformed_start_is_never_inside_any_window() {
        for bad in ["25:00", "02:60", "0230", "half past two", ""] {
            let w = window(bad, 90);
            assert_eq!(w.start_minute_utc(), None, "{bad:?} must not parse");
            assert!(
                (0..24 * 60).all(|m| !w.contains(m)),
                "{bad:?} must not be inside any minute of the day"
            );
        }
    }

    #[test]
    fn a_zero_length_window_never_opens() {
        assert!((0..24 * 60).all(|m| !window("02:30", 0).contains(m)));
    }

    /// The two halves must agree byte-for-byte on this block: decision #9's
    /// merged binary reads one config, and a key spelled differently on each
    /// side would silently schedule maintenance in only one of them.
    #[test]
    fn the_block_is_shaped_exactly_like_the_portals() {
        let yaml = "start_utc: \"02:30\"\nduration_minutes: 90\n";
        let w: MaintenanceWindow = serde_yaml::from_str(yaml).expect("parses");
        assert_eq!(w.start_minute_utc(), Some(150));
        assert_eq!(w.duration_minutes, 90);
    }

    /// `deny_unknown_fields`: a typo'd key here would otherwise leave the
    /// window at its default with no complaint.
    #[test]
    fn a_typo_in_the_block_is_refused_rather_than_defaulted() {
        assert!(serde_yaml::from_str::<MaintenanceWindow>("start_time: \"02:30\"\n").is_err());
    }

    #[test]
    fn the_shipped_config_schedules_maintenance() {
        let cfg: Config =
            serde_yaml::from_str(include_str!("../../fiber.config.yaml")).expect("shipped config");
        let w = cfg
            .storage
            .maintenance_window
            .expect("the shipped config opts in");
        assert_eq!(w.start_minute_utc(), Some(150));
        assert_eq!(w.duration_minutes, 90);
    }
}

/// The rendered `mqtt.broker` / `mqtt.tls` pair, validated as one unit.
///
/// `Ok(None)` when neither block is present (a Server). `Err` for anything that is not the
/// complete, TLS-enabled pair the renderer writes — see
/// [`Config::apply_rendered_broker_override`] for why a half is refused rather than used.
fn rendered_broker_override(
    doc: &serde_yaml::Value,
) -> Result<Option<(BrokerConfig, TlsConfig)>, String> {
    let mqtt = doc.get("mqtt");
    let broker = mqtt.and_then(|m| m.get("broker")).filter(|v| !v.is_null());
    let tls = mqtt.and_then(|m| m.get("tls")).filter(|v| !v.is_null());
    let (broker, tls) = match (broker, tls) {
        (None, None) => return Ok(None),
        (Some(_), None) => return Err("mqtt.broker without mqtt.tls".to_string()),
        (None, Some(_)) => return Err("mqtt.tls without mqtt.broker".to_string()),
        (Some(b), Some(t)) => (b, t),
    };

    let host = broker
        .get("host")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .ok_or("mqtt.broker.host is missing or empty")?;
    // The Server's 8883 listener is IPv4 only (plan/control/cluster.md §5): an IPv6 literal
    // names nothing that listens. The renderer refuses it too; this is the agent's own
    // check for an override it did not render.
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if bare.parse::<std::net::Ipv6Addr>().is_ok() {
        return Err(format!(
            "mqtt.broker.host {host} is an IPv6 literal, and the Server's 8883 listener is \
             IPv4 only"
        ));
    }
    let port = broker
        .get("port")
        .and_then(|v| v.as_u64())
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p != 0)
        .ok_or("mqtt.broker.port is missing or not a TCP port")?;
    let string = |v: &serde_yaml::Value, key: &str| -> Result<Option<String>, String> {
        match v.get(key) {
            None | Some(serde_yaml::Value::Null) => Ok(None),
            Some(serde_yaml::Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
            Some(_) => Err(format!("mqtt.broker.{key} is not a non-empty string")),
        }
    };
    // The secret itself never rides in the rendered file: that file is 0640
    // root:proximos, so proximos-control could read it. The override names the file the
    // password is in (root's, 0600) and this process — root — reads it here.
    if broker.get("password").is_some() {
        return Err(
            "mqtt.broker carries an inline password; the rendered override must name a \
             password_file instead"
                .into(),
        );
    }
    let username = string(broker, "username")?;
    let password_file = string(broker, "password_file")?;
    if username.is_some() != password_file.is_some() {
        return Err(
            "mqtt.broker carries a username without a password_file, or the reverse".into(),
        );
    }

    if tls.get("enabled").and_then(|v| v.as_bool()) != Some(true) {
        return Err("mqtt.tls.enabled is not true".to_string());
    }
    let ca_cert_path = tls
        .get("ca_cert_path")
        .and_then(|v| v.as_str())
        .filter(|p| !p.is_empty())
        .ok_or("mqtt.tls.ca_cert_path is missing or empty")?;

    // Last, once the shape is known to be the complete pair: a malformed override is
    // refused without touching the secret.
    let password = match &password_file {
        Some(path) => Some(read_peer_password(path)?),
        None => None,
    };

    Ok(Some((
        BrokerConfig {
            host: host.to_string(),
            port,
            // Not the override's to set: the seeded client id (or the hostname) stays.
            client_id: String::new(),
            username,
            password,
            from_cluster_override: true,
        },
        TlsConfig {
            enabled: true,
            ca_cert_path: ca_cert_path.to_string(),
            client_cert_path: None,
            client_key_path: None,
            insecure_skip_verify: false,
        },
    )))
}

/// The peer password a rendered override's `password_file` names
/// (`/data/proximos/cluster/peer_password`, 0600 root, plan/control/cluster.md §3).
///
/// The same rules the renderer applies before it names the file — one token, no
/// whitespace or control characters, at least 16 characters (`validate_arm`'s floor) —
/// so a file damaged after the render is refused here rather than sent to the Server.
/// Surrounding whitespace is stripped: the join writes no newline, a hand-written file
/// has one. The error names the file, never its content.
fn read_peer_password(path: &str) -> Result<String, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("mqtt.broker.password_file {path} cannot be read: {e}"))?;
    let password = text.trim();
    if password.is_empty() {
        return Err(format!("mqtt.broker.password_file {path} is empty"));
    }
    if password
        .chars()
        .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!(
            "mqtt.broker.password_file {path} holds more than one token"
        ));
    }
    if password.chars().count() < 16 {
        return Err(format!(
            "mqtt.broker.password_file {path} is shorter than 16 characters"
        ));
    }
    Ok(password.to_string())
}

/// `/run/proximos/control/config.yaml`, or `PROXIMOS_CONFIG` when set.
///
/// Same convention as `proximosctl` and `control_radio::dispatch::config_path`, so one
/// environment variable means one file from every end; duplicated rather than imported
/// because `control-core` does not depend on `control-radio` (and must not start to for
/// one string).
fn rendered_control_config_path() -> String {
    std::env::var("PROXIMOS_CONFIG")
        .unwrap_or_else(|_| "/run/proximos/control/config.yaml".to_string())
}

/// The `setup:` section (`plan/control/ble/peripheral.md` §6.3, GitLab #58) — and specifically the
/// rule that an unknown *value* must not cost the whole config, while an unknown *key* still does.
#[cfg(test)]
mod setup_section_tests {
    use super::*;

    /// The shipped file with `setup:` replaced, parsed the way the agent parses it at boot.
    fn config_with_setup(setup: &str) -> Result<Config, serde_yaml::Error> {
        let mut doc: serde_yaml::Value =
            serde_yaml::from_str(include_str!("../../fiber.config.yaml"))
                .expect("the shipped fiber.config.yaml is YAML");
        let section: serde_yaml::Value = serde_yaml::from_str(setup).expect("the fixture is YAML");
        doc.as_mapping_mut()
            .expect("a mapping")
            .insert(serde_yaml::Value::String("setup".to_string()), section);
        serde_yaml::from_value(doc)
    }

    /// The divergence this leniency exists for: a firmware downgrade (or a newer app) leaves a
    /// `role:` this build has never heard of, and the whole `Config::load_default()` used to fail
    /// on it — so `system/info` published `setup.provisioned: false` while BLE FB16, which reads
    /// the raw YAML, said `true` about the very same file.
    #[test]
    fn a_role_this_build_does_not_know_still_loads_and_keeps_provisioned() {
        let config = config_with_setup("provisioned: true\nrole: arbiter\n")
            .expect("an unknown role must not fail the load");
        assert!(config.setup.provisioned);
        assert_eq!(config.setup.role.as_deref(), Some("arbiter"));
        assert_eq!(
            config.setup.known_role(),
            None,
            "unknown to this build, and said so — not silently mapped onto a role it does know"
        );
    }

    #[test]
    fn the_two_roles_this_build_does_know_map_to_the_enum() {
        for (text, expected) in [("server", SetupRole::Server), ("client", SetupRole::Client)] {
            let config = config_with_setup(&format!("provisioned: true\nrole: {text}\n"))
                .expect("a known role loads");
            assert_eq!(config.setup.known_role(), Some(expected), "{text}");
        }
    }

    /// Leniency is about values only. A mistyped *key* is still a Hub that would silently report
    /// the wrong setup state, which is what `deny_unknown_fields` is there to name at start-up.
    #[test]
    fn a_mistyped_key_is_still_refused() {
        config_with_setup("provisioned: true\nprovisioned_at: 1\n")
            .expect_err("`deny_unknown_fields` stands");
    }

    #[test]
    fn an_absent_section_is_a_hub_nobody_has_set_up() {
        let mut doc: serde_yaml::Value =
            serde_yaml::from_str(include_str!("../../fiber.config.yaml")).expect("YAML");
        doc.as_mapping_mut()
            .expect("a mapping")
            .remove(serde_yaml::Value::String("setup".to_string()));
        let config: Config = serde_yaml::from_value(doc).expect("every pre-2026-09-16 file loads");
        assert_eq!(config.setup, SetupSection::default());
        assert!(!config.setup.provisioned);
        assert_eq!(config.setup.known_role(), None);
    }

    /// The section round-trips: what the applier writes is what the typed section reads back, and
    /// an absent role stays absent rather than becoming `role: null`.
    #[test]
    fn the_section_round_trips_through_yaml() {
        let section = SetupSection {
            provisioned: true,
            role: Some("server".to_string()),
        };
        let text = serde_yaml::to_string(&section).expect("serialize");
        assert!(text.contains("role: server"), "{text}");
        assert_eq!(
            serde_yaml::from_str::<SetupSection>(&text).expect("parse back"),
            section
        );

        let bare = serde_yaml::to_string(&SetupSection::default()).expect("serialize");
        assert!(
            !bare.contains("role"),
            "an absent role stays absent: {bare}"
        );
    }
}

#[cfg(test)]
mod sensor_interval_tests {
    use super::*;

    fn parse(yaml: &str) -> SensorConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn seconds_keys_are_read_as_written() {
        let c = parse("num_lines: 8\nsample_interval_s: 2\naggregation_interval_s: 600\nfailure_threshold: 3\n");
        assert_eq!((c.sample_interval_s, c.aggregation_interval_s), (2, 600));
        assert_eq!(c.sample_interval_ms(), 2000);
        assert_eq!(c.aggregation_interval_ms(), 600_000);
        assert_eq!(c.warmup_threshold, 3);
    }

    /// A Hub upgraded with its pre-2026-10-12 YAML keeps booting, on the
    /// values it had; the retired `report_interval_ms` is dropped.
    #[test]
    fn legacy_millisecond_keys_are_converted() {
        let c = parse(
            "num_lines: 8\nsample_interval_ms: 2000\naggregation_interval_ms: 15000\n\
             report_interval_ms: 60000\nfailure_threshold: 3\n",
        );
        assert_eq!((c.sample_interval_s, c.aggregation_interval_s), (2, 15));
        // Sub-second legacy values round up, never to zero.
        let c = parse("num_lines: 8\nsample_interval_ms: 500\nfailure_threshold: 3\n");
        assert_eq!(c.sample_interval_s, 1);
    }

    #[test]
    fn seconds_win_over_milliseconds_and_absent_keys_default() {
        let c = parse(
            "num_lines: 8\nsample_interval_s: 5\nsample_interval_ms: 2000\nfailure_threshold: 3\n",
        );
        assert_eq!(c.sample_interval_s, 5);
        assert_eq!(c.aggregation_interval_s, DEFAULT_AGGREGATION_INTERVAL_S);
        let c = parse("num_lines: 8\nfailure_threshold: 3\n");
        assert_eq!(c.sample_interval_s, DEFAULT_SAMPLE_INTERVAL_S);
    }

    /// The file is rewritten from this struct in places; what it writes must
    /// read back unchanged, under the new keys.
    #[test]
    fn serialises_to_the_seconds_keys() {
        let c = parse("num_lines: 8\nsample_interval_ms: 3000\nfailure_threshold: 3\n");
        let out = serde_yaml::to_string(&c).unwrap();
        assert!(out.contains("sample_interval_s: 3"), "{out}");
        assert!(!out.contains("_ms"), "{out}");
        assert_eq!(parse(&out), c);
    }

    #[test]
    fn the_shipped_default_config_uses_the_new_keys() {
        let yaml = include_str!("../../fiber.config.yaml");
        let doc: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let sensors: SensorConfig = serde_yaml::from_value(doc["sensors"].clone()).unwrap();
        assert_eq!(
            (sensors.sample_interval_s, sensors.aggregation_interval_s),
            (2, 600)
        );
        assert!(doc["sensors"].get("sample_interval_ms").is_none());
    }
}
