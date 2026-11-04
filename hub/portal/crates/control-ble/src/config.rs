//! Configuration — `plan/control/ble/plan.md` §4.
//!
//! **Two files, two directions.** `ble:` is *renderer output*, read from
//! `/run/proximos/control/config.yaml` and never written by this component. `eye:` is *fleet state*,
//! read **and written** through `host::ConfigStore`. That is why the two schemas differ in strictness:
//!
//! - every `ble:` struct is `deny_unknown_fields`, so a deployed `ble.pin` or `ble.enable_terminal`
//!   is refused at parse time rather than silently ignored — the v1 `@main` defect §9.2 describes
//!   cannot come back through configuration, and the BLE terminal must not appear to still have a
//!   switch;
//! - `eye:` is **not** `deny_unknown_fields`, because `control-core` owns that schema. The legacy
//!   transport keys v1 carries (`publish_interval_s`, `tag_timeout_s`, `auto_discover_max`, …) are
//!   accepted and collected into `legacy` so the caller can warn once per key at start-up.

use crate::error::{Component, Error, Kind, Result};
use crate::mode::Mode;
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------------------------
// `ble:` — the rendered device-config section
// ---------------------------------------------------------------------------------------------

/// Wrapper for the whole rendered document: `ble:` is one section among `power`, `sensors`,
/// `mqtt`, `eye`. Deliberately not `deny_unknown_fields` — the other sections are none of our
/// business, and failing on them would make this component the whole config's validator.
#[derive(Debug, serde::Deserialize)]
struct DeviceConfig {
    #[serde(default)]
    ble: Option<BleConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BleConfig {
    /// Factory choice, set once by the provisioning tool at flash time. **No serde default**: a
    /// `ble:` block without `mode` is an error, not a silent `off`.
    pub mode: Mode,
    /// e.g. `"hci1"`; `None` = the default adapter. One adapter serves BOTH roles (§0.1) — this is
    /// not a per-role key and deliberately cannot become one.
    #[serde(default)]
    pub adapter: Option<String>,
    #[serde(default)]
    pub central: CentralConfig,
    #[serde(default)]
    pub peripheral: PeripheralConfig,
    /// Reserved for genuinely BlueZ-specific settings; empty today, and a non-empty value is a
    /// parse error rather than a second undocumented configuration surface.
    #[serde(default)]
    pub bluez: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CentralConfig {
    #[serde(default = "d_scan_interval_s")]
    pub scan_interval_s: u64,
    #[serde(default = "d_beacon_timeout_s")]
    pub beacon_timeout_s: u64,
    /// HARD CAP on tracked strangers. Not a tuning knob: one oversized publish tore down v1's
    /// entire MQTT client (central.md §4).
    #[serde(default = "d_discovered_max")]
    pub discovered_max: u32,
    #[serde(default)]
    pub beacon_profile: BeaconProfile,
    #[serde(default)]
    pub dial_budget: DialBudget,
    #[serde(default)]
    pub recovery: RecoveryConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeaconProfile {
    /// Config-layer unit is **seconds**, for a human editing YAML; the BTSMP1 characteristic is
    /// little-endian u16 **milliseconds** (beacon-btsmp1.md §2) — multiplied by 1000 at the write.
    #[serde(default = "d_advertising_interval_s")]
    pub advertising_interval_s: u16,
    /// A deliberate narrowing from v1's shipped default (all four) to what this fleet needs.
    #[serde(default = "d_active_sensors")]
    pub active_sensors: Vec<Sensor>,
    #[serde(default = "d_tx_power_dbm")]
    pub tx_power_dbm: i8,
}

/// The active-sensor bitmask of beacon-btsmp1.md §2 — the vendor app's "Packet settings" screen.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Sensor {
    Temperature,
    Humidity,
    Magnet,
    Movement,
}

impl Sensor {
    pub const ALL: [Sensor; 4] = [
        Sensor::Temperature,
        Sensor::Humidity,
        Sensor::Magnet,
        Sensor::Movement,
    ];
    pub const fn bit(self) -> u8 {
        match self {
            Sensor::Temperature => 0x01,
            Sensor::Humidity => 0x02,
            Sensor::Magnet => 0x04,
            Sensor::Movement => 0x08,
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Sensor::Temperature => "temperature",
            Sensor::Humidity => "humidity",
            Sensor::Magnet => "magnet",
            Sensor::Movement => "movement",
        }
    }
    /// OR the bits of a set — what gets written to `e61c0021…`.
    pub fn mask(sensors: &[Sensor]) -> u8 {
        sensors.iter().fold(0u8, |m, s| m | s.bit())
    }

    /// The inverse of [`Sensor::mask`], for reporting what a beacon read back. Bits the vendor's
    /// bitmask has no name for are dropped here; the raw mask is what verification compares.
    pub fn from_mask(mask: u8) -> Vec<Sensor> {
        Sensor::ALL
            .into_iter()
            .filter(|s| mask & s.bit() != 0)
            .collect()
    }
}

impl std::fmt::Display for Sensor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialBudget {
    /// Unlock/provision attempts before a beacon is marked and left alone. **Persisted** — a budget
    /// that resets on reboot is not a budget.
    #[serde(default = "d_per_beacon_attempts")]
    pub per_beacon_attempts: u32,
    /// One outbound dial at a time. The adapter is shared (§0.1); a second dial buys only contention.
    #[serde(default = "d_concurrent_dials")]
    pub concurrent_dials: u32,
    #[serde(default = "d_session_timeout_s")]
    pub session_timeout_s: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryConfig {
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default = "d_stall_secs")]
    pub stall_secs: u64,
    /// Minimum between recovery firings, enforced on EVERY rung — v1 took this as a parameter on
    /// one path and ignored it on the other, so a wedged controller was power-cycled every ~30 s.
    #[serde(default = "d_cooldown_secs")]
    pub cooldown_secs: u64,
    /// Consecutive failed discovery starts (e.g. a D-Bus timeout from a hung bluetoothd) that
    /// count as a stall on their own, with or without tracked beacons.
    #[serde(default = "d_start_failures")]
    pub start_failures: u32,
    /// Whether the last rung may restart `bluetooth.service` over systemd's D-Bus API.
    #[serde(default = "d_true")]
    pub restart_bluetoothd: bool,
    /// Minimum between two bluetoothd restarts. Inside it the ladder stays on rung 1.
    #[serde(default = "d_bluetoothd_restart_min_secs")]
    pub bluetoothd_restart_min_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeripheralConfig {
    /// FB01 session idle timeout; every operation bumps it.
    #[serde(default = "d_session_idle_timeout_s")]
    pub session_idle_timeout_s: u64,
    /// The paging budget ceiling (peripheral.md §4). Never relies on long reads.
    #[serde(default = "d_max_page_bytes")]
    pub max_page_bytes: usize,
    /// `None` = derived from the hostname.
    #[serde(default)]
    pub advertise_name: Option<String>,
}

fn d_scan_interval_s() -> u64 {
    30
}
fn d_beacon_timeout_s() -> u64 {
    600
}
fn d_discovered_max() -> u32 {
    64
}
fn d_advertising_interval_s() -> u16 {
    10
}
fn d_active_sensors() -> Vec<Sensor> {
    vec![Sensor::Temperature, Sensor::Humidity]
}
fn d_tx_power_dbm() -> i8 {
    8
}
fn d_per_beacon_attempts() -> u32 {
    5
}
fn d_concurrent_dials() -> u32 {
    1
}
fn d_session_timeout_s() -> u64 {
    30
}
fn d_stall_secs() -> u64 {
    180
}
fn d_cooldown_secs() -> u64 {
    120
}
fn d_start_failures() -> u32 {
    3
}
fn d_bluetoothd_restart_min_secs() -> u64 {
    1800
}
fn d_session_idle_timeout_s() -> u64 {
    300
}
fn d_max_page_bytes() -> usize {
    180
}
fn d_true() -> bool {
    true
}
fn d_alarm_after() -> u32 {
    3
}
fn d_aggregation_interval_s() -> u64 {
    600
}

/// The allowed `eye.aggregation_interval_s`, seconds (`plan/control/storage_intervals.md` §3).
pub const AGGREGATION_INTERVAL_S: std::ops::RangeInclusive<u64> = 60..=3600;

/// `eye.aggregation_interval_s` within [`AGGREGATION_INTERVAL_S`], or an error naming the key.
pub fn validate_aggregation_interval_s(v: u64) -> Result<()> {
    if AGGREGATION_INTERVAL_S.contains(&v) {
        return Ok(());
    }
    Err(bad(format!(
        "eye.aggregation_interval_s is {v}: allowed {}..={} seconds",
        AGGREGATION_INTERVAL_S.start(),
        AGGREGATION_INTERVAL_S.end()
    )))
}

/// `eye.recording_interval_min` when absent (beacon_history.md §3).
pub const DEFAULT_RECORDING_INTERVAL_MIN: u16 = 15;

fn d_recording_interval_min() -> u16 {
    DEFAULT_RECORDING_INTERVAL_MIN
}

/// Refused at load, never snapped: the BTSMP1 archive accepts only 1, 5 or 15 minutes, and a fleet
/// value it cannot hold would leave every provisioned Beacon `differs` with nobody told why.
fn recording_interval_min<'de, D>(d: D) -> std::result::Result<u16, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    let v = u16::deserialize(d)?;
    if crate::central::en12830::protocol::LOGGING_INTERVALS_MIN.contains(&v) {
        Ok(v)
    } else {
        Err(serde::de::Error::custom(format!(
            "eye.recording_interval_min is {v}: allowed {:?} minutes",
            crate::central::en12830::protocol::LOGGING_INTERVALS_MIN
        )))
    }
}

impl Default for CentralConfig {
    fn default() -> Self {
        Self {
            scan_interval_s: d_scan_interval_s(),
            beacon_timeout_s: d_beacon_timeout_s(),
            discovered_max: d_discovered_max(),
            beacon_profile: BeaconProfile::default(),
            dial_budget: DialBudget::default(),
            recovery: RecoveryConfig::default(),
        }
    }
}
impl Default for BeaconProfile {
    fn default() -> Self {
        Self {
            advertising_interval_s: d_advertising_interval_s(),
            active_sensors: d_active_sensors(),
            tx_power_dbm: d_tx_power_dbm(),
        }
    }
}
impl Default for DialBudget {
    fn default() -> Self {
        Self {
            per_beacon_attempts: d_per_beacon_attempts(),
            concurrent_dials: d_concurrent_dials(),
            session_timeout_s: d_session_timeout_s(),
        }
    }
}
impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            stall_secs: d_stall_secs(),
            cooldown_secs: d_cooldown_secs(),
            start_failures: d_start_failures(),
            restart_bluetoothd: true,
            bluetoothd_restart_min_secs: d_bluetoothd_restart_min_secs(),
        }
    }
}
impl Default for PeripheralConfig {
    fn default() -> Self {
        Self {
            session_idle_timeout_s: d_session_idle_timeout_s(),
            max_page_bytes: d_max_page_bytes(),
            advertise_name: None,
        }
    }
}

/// The Tx Power values the BT SIG characteristic accepts on this hardware (beacon-btsmp1.md §2).
pub const TX_POWER_ALLOWED: [i8; 8] = [-14, -11, -8, -5, -2, 2, 4, 8];

fn bad(message: impl Into<String>) -> Error {
    Error::new(
        Kind::InvalidArgument,
        None,
        Component::DeviceConfig,
        message,
    )
}

impl BleConfig {
    /// Parse the `ble:` key out of a whole rendered device-config document, then validate it.
    ///
    /// A missing `ble:` is `NotBootstrapped`, not a default: this component cannot guess a factory
    /// mode. A malformed one is `InvalidArgument`. Both name `device-config` as the component.
    pub fn from_device_config_yaml(yaml: &str) -> Result<BleConfig> {
        let doc: DeviceConfig = serde_yaml::from_str(yaml).map_err(|e| {
            bad(format!(
                "the rendered device config could not be parsed: {e}"
            ))
        })?;
        let cfg = doc.ble.ok_or_else(|| {
            Error::new(
                Kind::NotBootstrapped,
                None,
                Component::DeviceConfig,
                "the rendered device config has no `ble:` section: this Hub has not been \
                 provisioned for Bluetooth (plan/control/ble/plan.md §4)",
            )
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Bounds every later task may then assume. Each message names the key and the allowed set.
    pub fn validate(&self) -> Result<()> {
        let p = &self.central.beacon_profile;
        if !(1..=10).contains(&p.advertising_interval_s) {
            return Err(bad(format!(
                "ble.central.beacon_profile.advertising_interval_s is {}: allowed 1..=10 seconds \
                 (the characteristic accepts 1000..=10000 ms)",
                p.advertising_interval_s
            )));
        }
        if !TX_POWER_ALLOWED.contains(&p.tx_power_dbm) {
            return Err(bad(format!(
                "ble.central.beacon_profile.tx_power_dbm is {}: allowed {:?} dBm",
                p.tx_power_dbm, TX_POWER_ALLOWED
            )));
        }
        if self.central.discovered_max < 1 {
            return Err(bad(
                "ble.central.discovered_max is 0: allowed 1 or more (an unbounded discovered set \
                 is what turns a dense RF environment into an oversized publish)",
            ));
        }
        if self.central.dial_budget.concurrent_dials != 1 {
            return Err(bad(format!(
                "ble.central.dial_budget.concurrent_dials is {}: phase 1 allows exactly 1 (the \
                 adapter is shared between both roles, §0.1)",
                self.central.dial_budget.concurrent_dials
            )));
        }
        if !(40..=512).contains(&self.peripheral.max_page_bytes) {
            return Err(bad(format!(
                "ble.peripheral.max_page_bytes is {}: allowed 40..=512",
                self.peripheral.max_page_bytes
            )));
        }
        if !self.bluez.is_empty() {
            let mut keys: Vec<&str> = self.bluez.keys().map(String::as_str).collect();
            keys.sort_unstable();
            return Err(bad(format!(
                "ble.bluez carries {}: the block is reserved and must be empty — a non-empty value \
                 is a parse error rather than a second undocumented configuration surface",
                keys.join(", ")
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// `eye:` — the fleet section, read and written through host::ConfigStore
// ---------------------------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct FleetConfig {
    #[serde(default)]
    eye: Option<EyeSection>,
}

/// The fleet keys this component reads. **Not** `deny_unknown_fields`: `control-core` owns this
/// schema, and the legacy transport keys land in `legacy` so the caller warns once per key.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EyeSection {
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// Configure a registered Beacon automatically: a **new** enrollment is followed by a session
    /// that writes the fleet profile and reads it back, instead of only a read. v1's key and v1's
    /// meaning — "applies the profile to a tag already in `eye.tags`, never registers one" — kept
    /// under the same name, so the signed `set_eye_config` toggles it (central.md §5, amended
    /// 2026-09-23). Off when absent.
    #[serde(default)]
    pub auto_provision: bool,
    /// Track and publish beacons in range that are **not registered** — the `discovered` set the
    /// portal's "Přidat beacon" page lists. Off, only enrolled and fleet-known beacons are tracked.
    /// Re-read every scan tick. v1's key, toggled by the same `set_eye_config`; **on** when absent,
    /// because v2 always listed them and a Hub that never set it must not lose the list.
    #[serde(default = "d_true")]
    pub auto_discover: bool,
    #[serde(default)]
    pub tags: Vec<EyeTag>,
    #[serde(default)]
    pub en12830: En12830Config,
    /// The storage window, in seconds (`plan/control/storage_intervals.md` §3, §5): every frame
    /// is a sample, and the samples of each UTC-aligned window are aggregated into the one
    /// `advertising` row and the one `eye/aggregated` entry per Beacon. Same name and meaning as
    /// `sensors.aggregation_interval_s`. Re-read every scan tick; see [`EyeSection::validate`].
    #[serde(default = "d_aggregation_interval_s")]
    pub aggregation_interval_s: u64,
    /// The archive recording cadence the Hub configures on every Beacon it provisions, in minutes
    /// (beacon_history.md §3): set after the read-back of `auto_provision` and of the
    /// reconfiguration request passes. One fleet value (Q1); 1, 5 or 15, refused otherwise.
    #[serde(
        default = "d_recording_interval_min",
        deserialize_with = "recording_interval_min"
    )]
    pub recording_interval_min: u16,
    /// Everything else the v1 schema carries — accepted and ignored, reported once at start-up.
    #[serde(flatten)]
    pub legacy: BTreeMap<String, serde_yaml::Value>,
    /// The Beacons this Hub is **home** for in a cluster (decision #42) — only these may be
    /// dialled. Never on disk: [`crate::central::cluster::ClusterConfigStore`] attaches it from
    /// the Server's pushed view on every load. `None` = no view, a standalone Hub, which dials
    /// what it has registered as before.
    #[serde(skip)]
    pub home: Option<std::collections::BTreeSet<crate::mac::MacAddr>>,
}

impl Default for EyeSection {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_provision: false,
            auto_discover: true,
            tags: Vec::new(),
            en12830: En12830Config::default(),
            aggregation_interval_s: d_aggregation_interval_s(),
            recording_interval_min: DEFAULT_RECORDING_INTERVAL_MIN,
            legacy: BTreeMap::new(),
            home: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EyeTag {
    pub mac: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging_interval_min: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub field_thresholds: Vec<serde_yaml::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provisioned: Option<bool>,
}

/// The capability flag central.md §6 owns. A fleet property, so it lives under `eye:`, not `ble:`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct En12830Config {
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default = "d_alarm_after")]
    pub protocol_failure_alarm_after: u32,
}

impl Default for En12830Config {
    fn default() -> Self {
        Self {
            enabled: true,
            protocol_failure_alarm_after: d_alarm_after(),
        }
    }
}

/// The legacy `eye:` transport keys `ble:` supersedes — accepted and ignored, warned about once.
pub const LEGACY_EYE_TRANSPORT_KEYS: [&str; 6] = [
    "publish_interval_s",
    "tag_timeout_s",
    "auto_discover_max",
    "scan_stall_recovery",
    "scan_stall_secs",
    "adapter",
];

impl EyeTag {
    /// The archive cadence this tag's own `beacon-recording` set, overriding the fleet's
    /// `eye.recording_interval_min`; `Some(0)` means an operator stopped it.
    pub fn recording_override_min(&self) -> Option<u16> {
        match (self.recording, self.logging_interval_min) {
            (Some(false), _) => Some(0),
            (_, Some(min)) => Some(min),
            _ => None,
        }
    }
}

impl EyeSection {
    /// The archive cadence `mac` is held to (beacon_history.md §3): its tag's override, else the
    /// fleet's. `0` means not recording.
    pub fn recording_target_min(&self, mac: &crate::mac::MacAddr) -> u16 {
        let key = mac.to_string();
        self.tags
            .iter()
            .find(|t| t.mac.eq_ignore_ascii_case(&key))
            .and_then(EyeTag::recording_override_min)
            .unwrap_or(self.recording_interval_min)
    }

    /// An absent `eye:` is the defaults — unlike `ble:`, a Hub with no beacons enrolled is normal.
    pub fn from_fleet_yaml(yaml: &str) -> Result<EyeSection> {
        let doc: FleetConfig = serde_yaml::from_str(yaml).map_err(|e| {
            Error::new(
                Kind::InvalidArgument,
                None,
                Component::DeviceConfig,
                format!("the fleet config could not be parsed: {e}"),
            )
        })?;
        let section = doc.eye.unwrap_or_default();
        section.validate()?;
        Ok(section)
    }

    /// Bounds on the keys this component types. Each message names the key and the allowed set.
    ///
    /// `ConfigStore::load` deliberately does not call this: the section also carries the enrolled
    /// Beacons, and a bad interval must not make the scan loop forget them. The loop validates on
    /// every tick instead and keeps the last good interval (`central::scan`).
    pub fn validate(&self) -> Result<()> {
        validate_aggregation_interval_s(self.aggregation_interval_s)
    }

    /// The ignored keys, for the one start-up warning. Sorted, so the log line is stable.
    pub fn ignored_transport_keys(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self
            .legacy
            .keys()
            .map(String::as_str)
            .filter(|k| LEGACY_EYE_TRANSPORT_KEYS.contains(k))
            .collect();
        v.sort_unstable();
        v
    }

    /// Case-insensitive lookup — the fleet file is hand-edited, and `mac` is a string there.
    pub fn tag(&self, mac: &str) -> Option<&EyeTag> {
        self.tags.iter().find(|t| t.mac.eq_ignore_ascii_case(mac))
    }

    /// Whether this Hub may open a GATT connection to `mac` (decision #42): always on a
    /// standalone Hub, and in a cluster only for a Beacon it is home for.
    pub fn allows_gatt(&self, mac: &crate::mac::MacAddr) -> bool {
        self.home.as_ref().is_none_or(|h| h.contains(mac))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_ble_section_is_not_bootstrapped_but_an_absent_eye_section_is_defaults() {
        assert_eq!(
            BleConfig::from_device_config_yaml("{}\n")
                .unwrap_err()
                .kind(),
            Kind::NotBootstrapped
        );
        let e = EyeSection::from_fleet_yaml("{}\n").unwrap();
        assert!(e.enabled);
        assert!(
            !e.auto_provision,
            "no writes to a beacon unless the fleet says so"
        );
        assert!(
            e.auto_discover,
            "a Hub that never set it keeps listing beacons in range"
        );
        assert!(e.en12830.enabled);
        assert_eq!(e.aggregation_interval_s, 600);
    }

    #[test]
    fn the_aggregation_interval_is_a_typed_key_bounded_to_an_hour() {
        let e = EyeSection::from_fleet_yaml("eye:\n  aggregation_interval_s: 300\n").unwrap();
        assert_eq!(e.aggregation_interval_s, 300);
        assert!(
            !e.legacy.contains_key("aggregation_interval_s"),
            "a typed key, not an ignored legacy one"
        );
        for ok in [60u64, 3600] {
            let y = format!("eye:\n  aggregation_interval_s: {ok}\n");
            assert!(EyeSection::from_fleet_yaml(&y).is_ok(), "{ok} is allowed");
        }
        for bad in [0u64, 59, 3601, 86_400] {
            let y = format!("eye:\n  aggregation_interval_s: {bad}\n");
            let err = EyeSection::from_fleet_yaml(&y).unwrap_err();
            assert_eq!(err.kind(), Kind::InvalidArgument);
            let msg = err.to_string();
            assert!(
                msg.contains("eye.aggregation_interval_s") && msg.contains("60..=3600"),
                "{msg}"
            );
        }
    }

    #[test]
    fn the_recording_interval_defaults_to_15_and_takes_only_what_the_archive_accepts() {
        assert_eq!(
            EyeSection::from_fleet_yaml("{}\n")
                .unwrap()
                .recording_interval_min,
            15
        );
        assert_eq!(EyeSection::default().recording_interval_min, 15);
        for min in [1u16, 5, 15] {
            let y = format!("eye:\n  recording_interval_min: {min}\n");
            assert_eq!(
                EyeSection::from_fleet_yaml(&y)
                    .unwrap()
                    .recording_interval_min,
                min
            );
        }
        for bad in ["0", "2", "10", "60", "-5", "\"fast\""] {
            let y = format!("eye:\n  recording_interval_min: {bad}\n");
            let err = EyeSection::from_fleet_yaml(&y).expect_err(bad);
            assert_eq!(err.kind(), Kind::InvalidArgument, "{bad}");
            assert_eq!(err.component(), Component::DeviceConfig);
        }
        let err = EyeSection::from_fleet_yaml("eye:\n  recording_interval_min: 10\n").unwrap_err();
        assert!(
            err.message().contains("recording_interval_min is 10"),
            "the message names the key and the value: {}",
            err.message()
        );
        // It is a typed key, not a legacy one.
        let e = EyeSection::from_fleet_yaml("eye:\n  recording_interval_min: 5\n").unwrap();
        assert!(!e.legacy.contains_key("recording_interval_min"));
    }

    #[test]
    fn a_ble_block_without_mode_is_refused() {
        // mode has no serde default on purpose: there is no safe guess for a factory choice.
        assert_eq!(
            BleConfig::from_device_config_yaml("ble:\n  adapter: hci0\n")
                .unwrap_err()
                .kind(),
            Kind::InvalidArgument
        );
    }

    #[test]
    fn the_legacy_transport_keys_are_recognised_by_name() {
        let y = "eye:\n  auto_discover: false\n  scan_stall_secs: 30\n  something_else: 1\n";
        let e = EyeSection::from_fleet_yaml(y).unwrap();
        assert_eq!(e.ignored_transport_keys(), vec!["scan_stall_secs"]);
        assert!(
            !e.auto_discover,
            "auto_discover is a live key now, not a superseded one"
        );
        assert!(
            e.legacy.contains_key("something_else"),
            "unknown keys are kept, only the documented ones are reported as superseded"
        );
    }

    #[test]
    fn tag_lookup_is_case_insensitive() {
        let y = "eye:\n  tags:\n    - mac: \"7c:d9:f4:13:10:de\"\n";
        let e = EyeSection::from_fleet_yaml(y).unwrap();
        assert!(e.tag("7C:D9:F4:13:10:DE").is_some());
        assert!(e.tag("AA:BB:CC:DD:EE:FF").is_none());
    }

    #[test]
    fn a_tag_defaults_to_enabled_with_no_optional_fields() {
        let y = "eye:\n  tags:\n    - mac: \"AA:BB:CC:DD:EE:FF\"\n";
        let t = &EyeSection::from_fleet_yaml(y).unwrap().tags[0];
        assert!(t.enabled);
        assert_eq!(t.name, None);
        assert_eq!(t.logging_interval_min, None);
        assert_eq!(t.recording, None);
        assert_eq!(t.provisioned, None);
        assert!(t.field_thresholds.is_empty());
    }

    #[test]
    fn every_allowed_tx_power_passes_validation_and_the_gaps_do_not() {
        for dbm in TX_POWER_ALLOWED {
            let y = format!(
                "ble:\n  mode: dual\n  central:\n    beacon_profile:\n      tx_power_dbm: {dbm}\n"
            );
            assert!(
                BleConfig::from_device_config_yaml(&y).is_ok(),
                "{dbm} is in the allowed set"
            );
        }
        for dbm in [-13i8, -1, 0, 1, 3, 5, 7, 9, 20] {
            let y = format!(
                "ble:\n  mode: dual\n  central:\n    beacon_profile:\n      tx_power_dbm: {dbm}\n"
            );
            assert!(
                BleConfig::from_device_config_yaml(&y).is_err(),
                "{dbm} is not in the allowed set"
            );
        }
    }
}
