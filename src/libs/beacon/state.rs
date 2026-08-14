//! Shared, live state for EYE BLE tags (latest readings + provisioning status).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock, RwLock};

use super::advertising::BeaconReading;
use super::config::{BeaconConfig, BeaconTagConfig};
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
pub struct BeaconTagState {
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
    /// Seen advertising but not in `eye.tags` — a discovery candidate rather than
    /// a tag this gateway owns. Published like any other so the viewer can offer
    /// it for adoption, but never alarm-evaluated (no thresholds exist for it)
    /// and dropped again once it goes quiet. Adoption clears the flag; deleting
    /// the tag lets it be re-discovered.
    pub discovered: bool,
}

impl BeaconTagState {
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
            discovered: false,
        }
    }

    /// Evaluate the current temperature/humidity against the tag's configured
    /// field thresholds, producing per-field + aggregate alarm state. Reuses the
    /// LoRaWAN node classifier so behaviour matches node field alarms.
    ///
    /// Only the measured quantities are threshold-driven (phase 1). `battery` and
    /// `movement` are deliberately absent from the match, so a stored threshold row
    /// for either is ignored rather than silently alarming:
    ///   * battery has the tag's own hardware low-battery assertion below, which is
    ///     the real signal — a numeric mV band on top of it is a duplicate;
    ///   * `movement` is `movement_count`, a monotonically increasing counter, so a
    ///     `[lo, hi]` band on it alarms once and then stays alarmed forever.
    pub fn evaluate_alarms(&mut self, cfg: &BeaconTagConfig) {
        self.field_alarm_states.clear();
        for t in &cfg.field_thresholds {
            let value: Option<f64> = match t.field.as_str() {
                "temperature" => self.temperature_c.map(|v| v as f64),
                "humidity" => self.humidity_pct.map(|v| v as f64),
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
    pub fn apply_reading(&mut self, r: &BeaconReading, rssi: Option<i16>, now_ts: i64) {
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
pub enum BeaconCommand {
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
pub struct BeaconSensorState {
    /// Whether a usable BLE adapter was found at startup.
    pub adapter_present: bool,
    /// Tags keyed by uppercase MAC `AA:BB:CC:DD:EE:FF`.
    pub tags: HashMap<String, BeaconTagState>,
    /// Pending external commands (from MQTT); drained by the monitor loop.
    pub command_queue: Vec<BeaconCommand>,
}

impl BeaconSensorState {
    /// Get-or-create the per-tag state for `mac`.
    pub fn entry(&mut self, mac: &str, name: Option<String>) -> &mut BeaconTagState {
        self.tags
            .entry(mac.to_string())
            .or_insert_with(|| BeaconTagState::new(mac.to_string(), name))
    }
}

pub type SharedBeaconState = Arc<RwLock<BeaconSensorState>>;

/// Process-wide handle to the running monitor's state, so the MQTT command
/// handler can enqueue EYE commands without threading the state through every
/// call site. Set once when the monitor starts.
static EYE_STATE: OnceLock<SharedBeaconState> = OnceLock::new();

/// Register the monitor's shared state (called once at monitor startup).
pub fn register_beacon_state(state: SharedBeaconState) {
    let _ = EYE_STATE.set(state);
}

/// MACs the *fleet* knows about, pushed by the server (system#6).
///
/// Deliberately separate from [`BeaconConfig::tags`] and never written to
/// `fiber.config.yaml`: a tag registered on another gateway must become audible
/// here without this gateway claiming ownership of it. Ownership is what decides
/// who runs the archive download and who evaluates the alarm thresholds — two
/// gateways doing either for one tag would race the tag's single GATT connection
/// and double every alarm.
pub type SharedKnownTags = Arc<RwLock<HashSet<String>>>;

static EYE_KNOWN_TAGS: OnceLock<SharedKnownTags> = OnceLock::new();

/// Handle to the fleet allowlist, creating it on first use.
pub fn beacon_known_tags() -> SharedKnownTags {
    EYE_KNOWN_TAGS
        .get_or_init(|| Arc::new(RwLock::new(HashSet::new())))
        .clone()
}

/// Replace the allowlist wholesale.
///
/// Wholesale, not merged: the server sends the full union every time, so a merge
/// could never forget a tag that was unregistered fleet-wide — it would stay
/// audible here until the gateway restarted.
pub fn set_beacon_known_tags<I: IntoIterator<Item = String>>(macs: I) -> usize {
    let handle = beacon_known_tags();
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
    beacon_known_tags()
        .read()
        .map(|g| g.clone())
        .unwrap_or_default()
}

pub type SharedBeaconConfig = Arc<RwLock<BeaconConfig>>;

/// Process-wide handle to the monitor's live config. The scan loop re-reads it
/// each poll cycle and the MQTT add/remove handlers mutate it, so tag changes
/// take effect without restarting the monitor. Set once at monitor startup.
static EYE_CONFIG: OnceLock<SharedBeaconConfig> = OnceLock::new();

/// Register the monitor's shared config (called once at monitor startup).
pub fn register_beacon_config(config: SharedBeaconConfig) {
    let _ = EYE_CONFIG.set(config);
}

/// Handle to the monitor's live config, if the monitor has started.
pub fn beacon_config_handle() -> Option<SharedBeaconConfig> {
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
pub fn beacon_state_handle() -> Option<SharedBeaconState> {
    EYE_STATE.get().cloned()
}

/// Copy of `tag` whose aggregate `alarm_state` is escalated to `Disconnected`
/// when the tag has not been heard from within `tag_timeout_s`.
///
/// [`BeaconTagState::alarm_state`] itself only ever holds the threshold verdict —
/// staleness is a function of the clock, so it is applied at read time. This is
/// the same escalation `beacon::monitor::publish_snapshot` applies before
/// publishing, factored out so the LCD overview and MQTT cannot disagree about
/// whether a tag is lost.
pub fn escalate_if_stale(tag: &BeaconTagState, now_ts: i64, tag_timeout_s: i64) -> BeaconTagState {
    let mut out = tag.clone();
    if tag.is_stale(now_ts, tag_timeout_s) {
        out.alarm_state = tag.alarm_state.worst(&LoRaWANAlarmState::Disconnected);
    }
    out
}

/// Tags for the configurable LCD overview: sorted by MAC for a stable row order,
/// with staleness already escalated (see [`escalate_if_stale`]).
///
/// Empty when the EYE monitor is not running — the display then renders the
/// configured BLE rows as "never seen" placeholders rather than dropping them,
/// which is the same thing an unprovisioned MAC produces.
pub fn display_snapshot(tag_timeout_s: i64) -> Vec<BeaconTagState> {
    let Some(state) = EYE_STATE.get() else {
        return Vec::new();
    };
    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let Ok(snapshot) = state.read() else {
        return Vec::new();
    };
    let mut tags: Vec<BeaconTagState> = snapshot
        .tags
        .values()
        .map(|t| escalate_if_stale(t, now_ts, tag_timeout_s))
        .collect();
    tags.sort_by(|a, b| a.mac.cmp(&b.mac));
    tags
}

/// Enqueue an external command for the monitor to run. Returns `false` if the
/// EYE monitor is not running (state never registered).
pub fn queue_beacon_command(cmd: BeaconCommand) -> bool {
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
pub fn create_shared_beacon_state(adapter_present: bool) -> SharedBeaconState {
    Arc::new(RwLock::new(BeaconSensorState {
        adapter_present,
        tags: HashMap::new(),
        command_queue: Vec::new(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::beacon::advertising::parse_manufacturer_value;

    #[test]
    fn apply_reading_updates_fields_and_last_seen() {
        let mut tag = BeaconTagState::new("7C:D9:F4:13:10:DE".into(), Some("Fridge".into()));
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
        let mut tag = BeaconTagState::new("AA:BB:CC:DD:EE:FF".into(), None);
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
        let cfg = BeaconTagConfig {
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
        let mut tag = BeaconTagState::new("AA:BB:CC:DD:EE:FF".into(), None);

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
        let empty = BeaconTagConfig {
            field_thresholds: vec![],
            ..cfg.clone()
        };
        tag.evaluate_alarms(&empty);
        assert!(tag.field_alarm_states.is_empty());
        assert_eq!(tag.alarm_state, LoRaWANAlarmState::Normal);
    }

    #[test]
    fn evaluate_alarms_ignores_a_stored_battery_threshold_but_keeps_the_hardware_flag() {
        use crate::libs::config::FieldThreshold;
        // Phase 1 withdrew the configurable battery band — a threshold row left over
        // from before must not alarm. The tag's own hardware low-battery assertion is
        // a different thing and must survive: it is the real signal, the analogue of
        // the node's native fPort-3 battery alarm.
        let cfg = BeaconTagConfig {
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
        let mut tag = BeaconTagState::new("AA:BB:CC:DD:EE:FF".into(), None);

        // Well under both stored bounds — would have been Critical before.
        tag.battery_mv = Some(2300);
        tag.evaluate_alarms(&cfg);
        assert!(
            tag.field_alarm_states.get("battery").is_none(),
            "a stored battery threshold must be ignored, got {:?}",
            tag.field_alarm_states.get("battery")
        );
        assert_eq!(tag.alarm_state, LoRaWANAlarmState::Normal);

        // The hardware flag still alarms, with or without a stored threshold.
        let no_thr = BeaconTagConfig {
            field_thresholds: vec![],
            ..cfg.clone()
        };
        let mut t2 = BeaconTagState::new("AA:BB:CC:DD:EE:FF".into(), None);
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
    fn evaluate_alarms_ignores_a_stored_movement_threshold() {
        use crate::libs::config::FieldThreshold;
        // `movement` is movement_count — monotonically increasing — so a [lo, hi]
        // band alarms once and then stays alarmed for the life of the tag. Phase 1
        // withdrew it; a leftover threshold row must be inert.
        let cfg = BeaconTagConfig {
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
        let mut tag = BeaconTagState::new("AA:BB:CC:DD:EE:FF".into(), None);
        for count in [5u16, 20, 80] {
            tag.movement_count = Some(count);
            tag.evaluate_alarms(&cfg);
            assert!(
                tag.field_alarm_states.get("movement").is_none(),
                "movement must not alarm at count {count}"
            );
            assert_eq!(tag.alarm_state, LoRaWANAlarmState::Normal);
        }
    }

    // ---- staleness escalation for the LCD overview ------------------------

    const NOW: i64 = 1_700_000_000;
    const TIMEOUT: i64 = 600;

    #[test]
    fn escalate_leaves_a_fresh_tag_alone() {
        let mut tag = BeaconTagState::new("AA:BB:CC:DD:EE:FF".into(), None);
        tag.last_seen_ts = Some(NOW - 10);
        tag.alarm_state = LoRaWANAlarmState::Warning;
        let out = escalate_if_stale(&tag, NOW, TIMEOUT);
        assert_eq!(out.alarm_state, LoRaWANAlarmState::Warning);
    }

    #[test]
    fn escalate_marks_a_tag_past_the_timeout_as_disconnected() {
        let mut tag = BeaconTagState::new("AA:BB:CC:DD:EE:FF".into(), None);
        tag.last_seen_ts = Some(NOW - TIMEOUT - 1);
        tag.alarm_state = LoRaWANAlarmState::Critical;
        let out = escalate_if_stale(&tag, NOW, TIMEOUT);
        assert_eq!(
            out.alarm_state,
            LoRaWANAlarmState::Disconnected,
            "Disconnected outranks Critical — a lost tag is not a hot tag"
        );
    }

    #[test]
    fn escalate_marks_a_never_seen_tag_as_disconnected() {
        let tag = BeaconTagState::new("AA:BB:CC:DD:EE:FF".into(), None);
        assert_eq!(tag.last_seen_ts, None);
        let out = escalate_if_stale(&tag, NOW, TIMEOUT);
        assert_eq!(out.alarm_state, LoRaWANAlarmState::Disconnected);
    }

    #[test]
    fn escalate_does_not_mutate_the_source_tag() {
        let mut tag = BeaconTagState::new("AA:BB:CC:DD:EE:FF".into(), None);
        tag.alarm_state = LoRaWANAlarmState::Normal;
        let _ = escalate_if_stale(&tag, NOW, TIMEOUT);
        assert_eq!(
            tag.alarm_state,
            LoRaWANAlarmState::Normal,
            "the live state must keep the threshold verdict; staleness is applied at read time"
        );
    }

    /// The monitor may not be running (EYE disabled, or not yet started). The
    /// display then renders configured `ble` rows as placeholders rather than
    /// dropping them, so an empty list is the correct answer, not a panic.
    #[test]
    fn display_snapshot_is_empty_when_the_monitor_never_registered() {
        if EYE_STATE.get().is_none() {
            assert!(display_snapshot(TIMEOUT).is_empty());
        }
    }
}
