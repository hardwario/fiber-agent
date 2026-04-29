//! BLE configuration loaded from the `[ble]` section of fiber.config.yaml.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BleConfig {
    /// Whether the in-app BLE GATT server is enabled.
    /// Phase 1 default: false (Yocto ble-fiber owns BLE).
    /// Phase 3 default: true (Yocto recipe deleted).
    #[serde(default)]
    pub enabled: bool,

    /// BLE pairing PIN. Edit this in fiber.config.yaml to rotate.
    #[serde(default = "default_pin")]
    pub pin: String,

    /// Whether the Terminal-over-BLE characteristics (FB05/FB06) are exposed.
    /// Operators may set this to false in stricter deployments.
    #[serde(default = "default_enable_terminal")]
    pub enable_terminal: bool,

    /// BLE local name advertised. None → use device hostname.
    #[serde(default)]
    pub advertising_name: Option<String>,
}

fn default_pin() -> String { "123456".to_string() }
fn default_enable_terminal() -> bool { true }

impl Default for BleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            pin: default_pin(),
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
    fn full_yaml_round_trip() {
        let yaml = r#"
enabled: true
pin: "999999"
enable_terminal: false
advertising_name: "MY-DEVICE"
"#;
        let cfg: BleConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.pin, "999999");
        assert!(!cfg.enable_terminal);
        assert_eq!(cfg.advertising_name, Some("MY-DEVICE".to_string()));
    }

    #[test]
    fn default_disables_ble() {
        let cfg = BleConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.pin, "123456");
        assert!(cfg.enable_terminal);
    }
}
