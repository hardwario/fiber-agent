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

    /// Configured tags.
    #[serde(default)]
    pub tags: Vec<EyeTagConfig>,
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
fn default_true() -> bool {
    true
}
