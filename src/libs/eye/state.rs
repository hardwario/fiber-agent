//! Shared, live state for EYE BLE tags (latest readings + provisioning status).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock, RwLock};

use super::advertising::EyeReading;
use super::config::{EyeConfig, EyeTagConfig};
use crate::libs::lorawan::state::{evaluate_threshold, LoRaWANAlarmState};

/// Per-tag provisioning lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningStatus {
    /// Registered (MAC known) but not yet seen / configured.
    PendingProvisioning,
    /// A provisioning GATT session is in progress.
    Provisioning,
    /// Successfully provisioned; read-only from here on.
    Provisioned,
    /// Provisioning attempt failed (will be retried with backoff).
    Failed,
    /// Tag is registered and read; provisioning intentionally skipped.
    ReadOnly,
}

impl ProvisioningStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ProvisioningStatus::PendingProvisioning => "pending",
            ProvisioningStatus::Provisioning => "provisioning",
            ProvisioningStatus::Provisioned => "provisioned",
            ProvisioningStatus::Failed => "failed",
            ProvisioningStatus::ReadOnly => "read_only",
        }
    }
}

/// Live state of a single EYE tag.
#[derive(Debug, Clone)]
pub struct EyeTagState {
    pub mac: String,
    pub name: Option<String>,
    pub temperature_c: Option<f32>,
    pub humidity_pct: Option<u8>,
    pub battery_mv: Option<u16>,
    pub low_battery: bool,
    pub magnet_present: bool,
    pub magnet_detected: bool,
    pub moving: Option<bool>,
    pub movement_count: Option<u16>,
    pub pitch_deg: Option<i8>,
    pub roll_deg: Option<i16>,
    pub rssi: Option<i16>,
    /// Unix seconds of the last advertising frame seen, or `None` if never.
    pub last_seen_ts: Option<i64>,
    pub provisioning: ProvisioningStatus,
    /// Number of consecutive failed provisioning attempts (for backoff).
    pub provision_attempts: u32,
    /// EN12830 recorder present (white tag)? `None` until first probed.
    pub is_en12830: Option<bool>,
    /// Unix seconds of the newest archived (recording) sample stored so far.
    /// Pre-seeded from the DB at startup; the download resumes from here.
    pub last_archived_ts: Option<i64>,
    /// Unix seconds of the last archive download attempt (rate-limit + fallback).
    pub last_download_ts: Option<i64>,
    /// Per-field alarm state (fields `temperature`/`humidity`), evaluated from
    /// the tag's configured thresholds on each reading. Empty when unset.
    pub field_alarm_states: HashMap<String, LoRaWANAlarmState>,
    /// Aggregate (worst-of-fields) alarm state.
    pub alarm_state: LoRaWANAlarmState,
}

impl EyeTagState {
    pub fn new(mac: String, name: Option<String>) -> Self {
        Self {
            mac,
            name,
            temperature_c: None,
            humidity_pct: None,
            battery_mv: None,
            low_battery: false,
            magnet_present: false,
            magnet_detected: false,
            moving: None,
            movement_count: None,
            pitch_deg: None,
            roll_deg: None,
            rssi: None,
            last_seen_ts: None,
            provisioning: ProvisioningStatus::PendingProvisioning,
            provision_attempts: 0,
            is_en12830: None,
            last_archived_ts: None,
            last_download_ts: None,
            field_alarm_states: HashMap::new(),
            alarm_state: LoRaWANAlarmState::Normal,
        }
    }

    /// Evaluate the current temperature/humidity against the tag's configured
    /// field thresholds, producing per-field + aggregate alarm state. Reuses the
    /// LoRaWAN sticker classifier so behaviour matches sticker field alarms.
    pub fn evaluate_alarms(&mut self, cfg: &EyeTagConfig) {
        self.field_alarm_states.clear();
        for t in &cfg.field_thresholds {
            let value: Option<f64> = match t.field.as_str() {
                "temperature" => self.temperature_c.map(|v| v as f64),
                "humidity" => self.humidity_pct.map(|v| v as f64),
                "battery" => self.battery_mv.map(|v| v as f64),
                "movement" => self.movement_count.map(|v| v as f64),
                _ => None,
            };
            if let Some(v) = value {
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
        // The tag's own low-battery flag is a hardware assertion independent of
        // any configured numeric threshold: surface it as at least a Warning on
        // the `battery` field so a depleting cell alarms even when the operator
        // set no battery threshold.
        if self.low_battery {
            let cur = self
                .field_alarm_states
                .get("battery")
                .cloned()
                .unwrap_or(LoRaWANAlarmState::Normal);
            self.field_alarm_states.insert(
                "battery".to_string(),
                cur.worst(&LoRaWANAlarmState::Warning),
            );
        }
        self.alarm_state = self
            .field_alarm_states
            .values()
            .cloned()
            .fold(LoRaWANAlarmState::Normal, |a, b| a.worst(&b));
    }

    /// Apply a freshly parsed advertising frame.
    pub fn apply_reading(&mut self, r: &EyeReading, rssi: Option<i16>, now_ts: i64) {
        if r.temperature_c.is_some() {
            self.temperature_c = r.temperature_c;
        }
        if r.humidity_pct.is_some() {
            self.humidity_pct = r.humidity_pct;
        }
        if r.battery_mv.is_some() {
            self.battery_mv = r.battery_mv;
        }
        self.low_battery = r.low_battery;
        self.magnet_present = r.magnet_present;
        self.magnet_detected = r.magnet_detected;
        if r.moving.is_some() {
            self.moving = r.moving;
        }
        if r.movement_count.is_some() {
            self.movement_count = r.movement_count;
        }
        if r.pitch_deg.is_some() {
            self.pitch_deg = r.pitch_deg;
        }
        if r.roll_deg.is_some() {
            self.roll_deg = r.roll_deg;
        }
        if rssi.is_some() {
            self.rssi = rssi;
        }
        self.last_seen_ts = Some(now_ts);
    }

    /// Whether the tag has not been seen within `timeout_secs`.
    pub fn is_stale(&self, now_ts: i64, timeout_secs: i64) -> bool {
        match self.last_seen_ts {
            Some(ts) => now_ts.saturating_sub(ts) > timeout_secs,
            None => true,
        }
    }
}

/// External command for the EYE monitor, queued by the MQTT command handler and
/// drained by the monitor loop (which runs it while the scan is paused).
#[derive(Debug, Clone)]
pub enum EyeCommand {
    /// Change the on-tag logging interval (minutes) and (re)start recording.
    SetRecording { mac: String, interval_min: u16 },
    /// Manually back-fill the archive for a tag now.
    DownloadHistory { mac: String },
    /// Connect over BLE and determine whether the tag is an EN12830 recorder,
    /// updating `is_en12830` in the shared state.
    Detect { mac: String },
}

/// Aggregate state for the EYE subsystem.
#[derive(Debug, Clone, Default)]
pub struct EyeSensorState {
    /// Whether a usable BLE adapter was found at startup.
    pub adapter_present: bool,
    /// Tags keyed by uppercase MAC `AA:BB:CC:DD:EE:FF`.
    pub tags: HashMap<String, EyeTagState>,
    /// Pending external commands (from MQTT); drained by the monitor loop.
    pub command_queue: Vec<EyeCommand>,
}

impl EyeSensorState {
    /// Get-or-create the per-tag state for `mac`.
    pub fn entry(&mut self, mac: &str, name: Option<String>) -> &mut EyeTagState {
        self.tags
            .entry(mac.to_string())
            .or_insert_with(|| EyeTagState::new(mac.to_string(), name))
    }
}

pub type SharedEyeState = Arc<RwLock<EyeSensorState>>;

/// Process-wide handle to the running monitor's state, so the MQTT command
/// handler can enqueue EYE commands without threading the state through every
/// call site. Set once when the monitor starts.
static EYE_STATE: OnceLock<SharedEyeState> = OnceLock::new();

/// Register the monitor's shared state (called once at monitor startup).
pub fn register_eye_state(state: SharedEyeState) {
    let _ = EYE_STATE.set(state);
}

/// MACs the *fleet* knows about, pushed by the server (system#6).
///
/// Deliberately separate from [`EyeConfig::tags`] and never written to
/// `fiber.config.yaml`: a tag registered on another gateway must become audible
/// here without this gateway claiming ownership of it. Ownership is what decides
/// who runs the archive download and who evaluates the alarm thresholds — two
/// gateways doing either for one tag would race the tag's single GATT connection
/// and double every alarm.
pub type SharedKnownTags = Arc<RwLock<HashSet<String>>>;

static EYE_KNOWN_TAGS: OnceLock<SharedKnownTags> = OnceLock::new();

/// Handle to the fleet allowlist, creating it on first use.
pub fn eye_known_tags() -> SharedKnownTags {
    EYE_KNOWN_TAGS
        .get_or_init(|| Arc::new(RwLock::new(HashSet::new())))
        .clone()
}

/// Replace the allowlist wholesale.
///
/// Wholesale, not merged: the server sends the full union every time, so a merge
/// could never forget a tag that was unregistered fleet-wide — it would stay
/// audible here until the gateway restarted.
pub fn set_eye_known_tags<I: IntoIterator<Item = String>>(macs: I) -> usize {
    let handle = eye_known_tags();
    let set: HashSet<String> = macs
        .into_iter()
        .map(|m| m.to_uppercase())
        .filter(|m| is_valid_mac(m))
        .collect();
    let n = set.len();
    if let Ok(mut guard) = handle.write() {
        *guard = set;
    }
    n
}

/// Snapshot of the allowlist, for the scan loop.
pub fn known_tags_snapshot() -> HashSet<String> {
    eye_known_tags()
        .read()
        .map(|g| g.clone())
        .unwrap_or_default()
}

pub type SharedEyeConfig = Arc<RwLock<EyeConfig>>;

/// Process-wide handle to the monitor's live config. The scan loop re-reads it
/// each poll cycle and the MQTT add/remove handlers mutate it, so tag changes
/// take effect without restarting the monitor. Set once at monitor startup.
static EYE_CONFIG: OnceLock<SharedEyeConfig> = OnceLock::new();

/// Register the monitor's shared config (called once at monitor startup).
pub fn register_eye_config(config: SharedEyeConfig) {
    let _ = EYE_CONFIG.set(config);
}

/// Handle to the monitor's live config, if the monitor has started.
pub fn eye_config_handle() -> Option<SharedEyeConfig> {
    EYE_CONFIG.get().cloned()
}

/// Strict MAC format check `AA:BB:CC:DD:EE:FF`. The recorder path builds a raw
/// `sockaddr` from this string via `parse_mac`, which silently coerces bad hex
/// to zero — a malformed MAC would then attempt to connect to `00:00:00:...`,
/// so validate at the choke point (MQTT command handlers) before enqueuing.
pub fn is_valid_mac(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 17 {
        return false;
    }
    for (i, &c) in bytes.iter().enumerate() {
        let in_sep = i % 3 == 2;
        if in_sep {
            if c != b':' {
                return false;
            }
        } else if !c.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

/// Handle to the running monitor's shared state, if the monitor has started.
/// Lets command handlers seed/drop in-memory tag entries after a config change.
pub fn eye_state_handle() -> Option<SharedEyeState> {
    EYE_STATE.get().cloned()
}

/// Enqueue an external command for the monitor to run. Returns `false` if the
/// EYE monitor is not running (state never registered).
pub fn queue_eye_command(cmd: EyeCommand) -> bool {
    match EYE_STATE.get() {
        Some(state) => {
            if let Ok(mut s) = state.write() {
                s.command_queue.push(cmd);
                true
            } else {
                false
            }
        }
        None => false,
    }
}

/// Build a fresh shared state.
pub fn create_shared_eye_state(adapter_present: bool) -> SharedEyeState {
    Arc::new(RwLock::new(EyeSensorState {
        adapter_present,
        tags: HashMap::new(),
        command_queue: Vec::new(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::eye::advertising::parse_manufacturer_value;

    #[test]
    fn apply_reading_updates_fields_and_last_seen() {
        let mut tag = EyeTagState::new("7C:D9:F4:13:10:DE".into(), Some("Fridge".into()));
        let r = parse_manufacturer_value(&[0x01, 0x83, 0x09, 0xab, 0x3f, 0x6a]).unwrap();
        tag.apply_reading(&r, Some(-60), 1_000);
        assert_eq!(tag.temperature_c, Some(24.75));
        assert_eq!(tag.humidity_pct, Some(63));
        assert_eq!(tag.battery_mv, Some(3060));
        assert_eq!(tag.rssi, Some(-60));
        assert_eq!(tag.last_seen_ts, Some(1_000));
        assert!(!tag.is_stale(1_030, 60));
        assert!(tag.is_stale(2_000, 60));
    }

    #[test]
    fn missing_fields_are_not_overwritten() {
        let mut tag = EyeTagState::new("AA:BB:CC:DD:EE:FF".into(), None);
        tag.temperature_c = Some(10.0);
        // a frame with only battery present must not wipe temperature
        let r = parse_manufacturer_value(&[0x01, 0x80, 0x6a]).unwrap();
        tag.apply_reading(&r, None, 5);
        assert_eq!(tag.temperature_c, Some(10.0));
        assert_eq!(tag.battery_mv, Some(3060));
    }

    #[test]
    fn evaluate_alarms_classifies_temperature_and_humidity() {
        use crate::libs::config::FieldThreshold;
        let cfg = EyeTagConfig {
            mac: "AA:BB:CC:DD:EE:FF".into(),
            name: None,
            enabled: true,
            logging_interval_min: None,
            recording: None,
            field_thresholds: vec![
                FieldThreshold {
                    field: "temperature".into(),
                    critical_low: Some(-20.0),
                    warning_low: Some(0.0),
                    warning_high: Some(8.0),
                    critical_high: Some(12.0),
                },
                FieldThreshold {
                    field: "humidity".into(),
                    critical_low: None,
                    warning_low: None,
                    warning_high: Some(70.0),
                    critical_high: Some(90.0),
                },
            ],
            provisioned: None,
        };
        let mut tag = EyeTagState::new("AA:BB:CC:DD:EE:FF".into(), None);

        // 5 °C + 50 % → both normal
        tag.temperature_c = Some(5.0);
        tag.humidity_pct = Some(50);
        tag.evaluate_alarms(&cfg);
        assert_eq!(tag.alarm_state, LoRaWANAlarmState::Normal);

        // 10 °C → temperature warning (>8) → aggregate Warning
        tag.temperature_c = Some(10.0);
        tag.evaluate_alarms(&cfg);
        assert_eq!(
            tag.field_alarm_states.get("temperature"),
            Some(&LoRaWANAlarmState::Warning)
        );
        assert_eq!(tag.alarm_state, LoRaWANAlarmState::Warning);

        // 15 °C + 95 % → both critical → aggregate Critical
        tag.temperature_c = Some(15.0);
        tag.humidity_pct = Some(95);
        tag.evaluate_alarms(&cfg);
        assert_eq!(tag.alarm_state, LoRaWANAlarmState::Critical);

        // no thresholds → cleared to Normal
        let empty = EyeTagConfig {
            field_thresholds: vec![],
            ..cfg.clone()
        };
        tag.evaluate_alarms(&empty);
        assert!(tag.field_alarm_states.is_empty());
        assert_eq!(tag.alarm_state, LoRaWANAlarmState::Normal);
    }

    #[test]
    fn evaluate_alarms_battery_threshold_and_low_battery_flag() {
        use crate::libs::config::FieldThreshold;
        let cfg = EyeTagConfig {
            mac: "AA:BB:CC:DD:EE:FF".into(),
            name: None,
            enabled: true,
            logging_interval_min: None,
            recording: None,
            field_thresholds: vec![FieldThreshold {
                field: "battery".into(),
                critical_low: Some(2400.0),
                warning_low: Some(2700.0),
                warning_high: None,
                critical_high: None,
            }],
            provisioned: None,
        };
        let mut tag = EyeTagState::new("AA:BB:CC:DD:EE:FF".into(), None);

        tag.battery_mv = Some(3000); // healthy
        tag.evaluate_alarms(&cfg);
        assert_eq!(tag.alarm_state, LoRaWANAlarmState::Normal);

        tag.battery_mv = Some(2600); // < warning_low
        tag.evaluate_alarms(&cfg);
        assert_eq!(
            tag.field_alarm_states.get("battery"),
            Some(&LoRaWANAlarmState::Warning)
        );

        tag.battery_mv = Some(2300); // < critical_low
        tag.evaluate_alarms(&cfg);
        assert_eq!(tag.alarm_state, LoRaWANAlarmState::Critical);

        // The hardware low_battery flag alarms even with NO battery threshold set.
        let no_thr = EyeTagConfig {
            field_thresholds: vec![],
            ..cfg.clone()
        };
        let mut t2 = EyeTagState::new("AA:BB:CC:DD:EE:FF".into(), None);
        t2.battery_mv = Some(3000);
        t2.low_battery = true;
        t2.evaluate_alarms(&no_thr);
        assert_eq!(
            t2.field_alarm_states.get("battery"),
            Some(&LoRaWANAlarmState::Warning)
        );
        assert_eq!(t2.alarm_state, LoRaWANAlarmState::Warning);
    }

    #[test]
    fn evaluate_alarms_movement_count_threshold() {
        use crate::libs::config::FieldThreshold;
        let cfg = EyeTagConfig {
            mac: "AA:BB:CC:DD:EE:FF".into(),
            name: None,
            enabled: true,
            logging_interval_min: None,
            recording: None,
            field_thresholds: vec![FieldThreshold {
                field: "movement".into(),
                critical_low: None,
                warning_low: None,
                warning_high: Some(10.0),
                critical_high: Some(50.0),
            }],
            provisioned: None,
        };
        let mut tag = EyeTagState::new("AA:BB:CC:DD:EE:FF".into(), None);
        tag.movement_count = Some(5);
        tag.evaluate_alarms(&cfg);
        assert_eq!(tag.alarm_state, LoRaWANAlarmState::Normal);
        tag.movement_count = Some(20); // > warning_high
        tag.evaluate_alarms(&cfg);
        assert_eq!(
            tag.field_alarm_states.get("movement"),
            Some(&LoRaWANAlarmState::Warning)
        );
        tag.movement_count = Some(80); // > critical_high
        tag.evaluate_alarms(&cfg);
        assert_eq!(tag.alarm_state, LoRaWANAlarmState::Critical);
    }
}
