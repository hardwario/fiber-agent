//! BLE configuration loaded from the `[ble]` section of fiber.config.yaml.
//!
//! Note: the legacy static `pin` field was removed when BLE pairing switched
//! to ephemeral provisioning tokens. Yamls that still carry `pin: "..."` are
//! tolerated (the field is silently ignored) — the value no longer has any
//! effect.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BleConfig {
    /// Whether the in-app BLE GATT server is enabled.
    /// Phase 1 default: false (Yocto ble-fiber owns BLE).
    /// Phase 3 default: true (Yocto recipe deleted).
    #[serde(default)]
    pub enabled: bool,

    /// Whether the Terminal-over-BLE characteristics (FB05/FB06) are exposed.
    /// Operators may set this to false in stricter deployments.
    #[serde(default = "default_enable_terminal")]
    pub enable_terminal: bool,

    /// BLE local name advertised. None → use device hostname.
    #[serde(default)]
    pub advertising_name: Option<String>,
}

fn default_enable_terminal() -> bool { true }

impl Default for BleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            enable_terminal: default_enable_terminal(),
            advertising_name: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_yaml_yields_defaults() {
        let cfg: BleConfig = serde_yaml::from_str("{}").unwrap();
        assert_eq!(cfg, BleConfig::default());
    }

    #[test]
    fn enabled_can_be_toggled() {
        let cfg: BleConfig = serde_yaml::from_str("enabled: true").unwrap();
        assert!(cfg.enabled);
    }

    #[test]
    fn legacy_pin_field_is_silently_ignored() {
        // Backwards compat: old yamls still carry pin; we accept and discard.
        let yaml = r#"
enabled: true
pin: "999999"
enable_terminal: false
advertising_name: "MY-DEVICE"
"#;
        let cfg: BleConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.enabled);
        assert!(!cfg.enable_terminal);
        assert_eq!(cfg.advertising_name, Some("MY-DEVICE".to_string()));
    }

    #[test]
    fn default_disables_ble() {
        let cfg = BleConfig::default();
        assert!(!cfg.enabled);
        assert!(cfg.enable_terminal);
    }
}
