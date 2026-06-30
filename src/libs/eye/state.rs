//! Shared, live state for EYE BLE tags (latest readings + provisioning status).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use super::advertising::EyeReading;

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
        }
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

/// Aggregate state for the EYE subsystem.
#[derive(Debug, Clone, Default)]
pub struct EyeSensorState {
    /// Whether a usable BLE adapter was found at startup.
    pub adapter_present: bool,
    /// Tags keyed by uppercase MAC `AA:BB:CC:DD:EE:FF`.
    pub tags: HashMap<String, EyeTagState>,
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

/// Build a fresh shared state.
pub fn create_shared_eye_state(adapter_present: bool) -> SharedEyeState {
    Arc::new(RwLock::new(EyeSensorState {
        adapter_present,
        tags: HashMap::new(),
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
}
