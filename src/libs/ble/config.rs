//! BLE configuration loaded from the `[ble]` section of fiber.config.yaml.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BleConfig {
    /// Whether the in-app BLE GATT server is enabled.
    /// Phase 1 default: false (Yocto ble-fiber owns BLE).
    /// Phase 3 default: true (Yocto recipe deleted).
    #[serde(default)]
    pub enabled: bool,

    /// Path to the editable PIN file. If absent, created with `default_pin`.
    #[serde(default = "default_pin_file")]
    pub pin_file: String,

    /// Default PIN value used when `pin_file` does not exist.
    #[serde(default = "default_pin_value")]
    pub default_pin: String,

    /// Whether the Terminal-over-BLE characteristics (FB05/FB06) are exposed.
    /// Operators may set this to false in stricter deployments.
    #[serde(default = "default_enable_terminal")]
    pub enable_terminal: bool,

    /// BLE local name advertised. None → use device hostname.
    #[serde(default)]
    pub advertising_name: Option<String>,
}

fn default_pin_file() -> String { "/data/ble/pin.txt".to_string() }
fn default_pin_value() -> String { "123456".to_string() }
fn default_enable_terminal() -> bool { true }

impl Default for BleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            pin_file: default_pin_file(),
            default_pin: default_pin_value(),
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
pin_file: "/etc/ble/pin"
default_pin: "999999"
enable_terminal: false
advertising_name: "MY-DEVICE"
"#;
        let cfg: BleConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.pin_file, "/etc/ble/pin");
        assert_eq!(cfg.default_pin, "999999");
        assert!(!cfg.enable_terminal);
        assert_eq!(cfg.advertising_name, Some("MY-DEVICE".to_string()));
    }

    #[test]
    fn default_disables_ble() {
        let cfg = BleConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.pin_file, "/data/ble/pin.txt");
        assert_eq!(cfg.default_pin, "123456");
        assert!(cfg.enable_terminal);
    }
}
