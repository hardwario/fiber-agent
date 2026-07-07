//! Configuration for the EYE BLE tag subsystem (loaded from `fiber.config.yaml`).

use serde::{Deserialize, Serialize};

/// Top-level EYE subsystem configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EyeConfig {
    /// Enable the EYE BLE tag monitor.
    #[serde(default)]
    pub enabled: bool,

    /// Active-scan window length per cycle, seconds.
    #[serde(default = "default_scan_window_s")]
    pub scan_window_s: u64,

    /// How often to publish the tag snapshot to MQTT, seconds.
    #[serde(default = "default_publish_interval_s")]
    pub publish_interval_s: u64,

    /// Mark a tag stale if not seen within this many seconds.
    #[serde(default = "default_tag_timeout_s")]
    pub tag_timeout_s: i64,

    /// Automatically provision a configured tag (apply the default profile) the
    /// first time it is seen advertising.
    #[serde(default)]
    pub auto_provision: bool,

    /// Master switch for the EN12830 temperature archive (white tags). When on,
    /// recording is auto-enabled at provisioning and gaps are back-filled from
    /// the tag's internal memory.
    #[serde(default = "default_true")]
    pub recording_enabled: bool,

    /// Default on-tag logging interval in minutes (tag supports 1 / 5 / 15).
    #[serde(default = "default_logging_interval_min")]
    pub default_logging_interval_min: u16,

    /// Fallback archive sync period in hours — download at least this often even
    /// without a detected gap.
    #[serde(default = "default_sync_fallback_hours")]
    pub sync_fallback_hours: u64,

    /// Configured tags.
    #[serde(default)]
    pub tags: Vec<EyeTagConfig>,
}

impl EyeConfig {
    /// Effective logging interval (minutes) for a tag: per-tag override, else
    /// the subsystem default. Clamped to the tag-supported set {1, 5, 15}.
    pub fn interval_min_for(&self, tag: &EyeTagConfig) -> u16 {
        let raw = tag
            .logging_interval_min
            .unwrap_or(self.default_logging_interval_min);
        match raw {
            1 => 1,
            15 => 15,
            _ => 5, // 5 is the default/kompromis; unknown values snap to it
        }
    }

    /// Whether the archive recording is active for a tag (per-tag override else
    /// the subsystem master switch).
    pub fn recording_on_for(&self, tag: &EyeTagConfig) -> bool {
        self.recording_enabled && tag.recording.unwrap_or(true)
    }

    /// Insert or update a tag by MAC (case-insensitive; stored uppercased).
    /// Overwrites the name only when `name` is `Some`. Mirrors the YAML upsert in
    /// `ConfigApplier::update_eye_tag_config` so the monitor's live view stays in
    /// sync with disk after an `add_eye_tag` command.
    pub fn upsert_tag(&mut self, mac: &str, name: Option<&str>) {
        let up = mac.to_uppercase();
        if let Some(t) = self.tags.iter_mut().find(|t| t.mac.to_uppercase() == up) {
            if let Some(n) = name {
                t.name = Some(n.to_string());
            }
        } else {
            self.tags.push(EyeTagConfig {
                mac: up,
                name: name.map(|s| s.to_string()),
                enabled: true,
                logging_interval_min: None,
                recording: None,
            });
        }
    }

    /// Remove a tag by MAC (case-insensitive). Returns whether one was removed.
    pub fn remove_tag(&mut self, mac: &str) -> bool {
        let up = mac.to_uppercase();
        let before = self.tags.len();
        self.tags.retain(|t| t.mac.to_uppercase() != up);
        self.tags.len() != before
    }
}

/// A single configured EYE tag (identified by MAC).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EyeTagConfig {
    /// MAC address `AA:BB:CC:DD:EE:FF` (case-insensitive).
    pub mac: String,

    /// Operator-facing name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Whether this tag is active.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Per-tag on-tag logging interval in minutes (1 / 5 / 15). `None` inherits
    /// [`EyeConfig::default_logging_interval_min`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging_interval_min: Option<u16>,

    /// Per-tag archive recording override. `None` inherits
    /// [`EyeConfig::recording_enabled`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<bool>,
}

fn default_scan_window_s() -> u64 {
    60
}
fn default_publish_interval_s() -> u64 {
    30
}
fn default_tag_timeout_s() -> i64 {
    600
}
fn default_logging_interval_min() -> u16 {
    5
}
fn default_sync_fallback_hours() -> u64 {
    6
}
fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_tag_inserts_uppercased_with_defaults() {
        let mut cfg = EyeConfig::default();
        cfg.upsert_tag("aa:bb:cc:dd:ee:ff", Some("Freezer"));
        assert_eq!(cfg.tags.len(), 1);
        assert_eq!(cfg.tags[0].mac, "AA:BB:CC:DD:EE:FF");
        assert_eq!(cfg.tags[0].name.as_deref(), Some("Freezer"));
        assert!(cfg.tags[0].enabled);
    }

    #[test]
    fn upsert_tag_updates_in_place_and_keeps_name_when_none() {
        let mut cfg = EyeConfig::default();
        cfg.upsert_tag("AA:BB:CC:DD:EE:FF", Some("Freezer"));
        // same MAC (lowercased) with a new name updates in place, no duplicate
        cfg.upsert_tag("aa:bb:cc:dd:ee:ff", Some("Fridge"));
        assert_eq!(cfg.tags.len(), 1, "must upsert, not duplicate");
        assert_eq!(cfg.tags[0].name.as_deref(), Some("Fridge"));
        // name None keeps the existing name
        cfg.upsert_tag("aa:bb:cc:dd:ee:ff", None);
        assert_eq!(cfg.tags[0].name.as_deref(), Some("Fridge"));
    }

    #[test]
    fn remove_tag_is_case_insensitive_and_reports() {
        let mut cfg = EyeConfig::default();
        cfg.upsert_tag("AA:BB:CC:DD:EE:FF", None);
        assert!(cfg.remove_tag("aa:bb:cc:dd:ee:ff"));
        assert!(cfg.tags.is_empty());
        assert!(!cfg.remove_tag("AA:BB:CC:DD:EE:FF"), "absent -> false");
    }
}
