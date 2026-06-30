//! Parsed configuration types for the export module.
//!
//! The `mqtt.export` block in `fiber.config.yaml` is deserialised into
//! these types. They drive how the save-and-feed export pipeline ships
//! `sticker_readings` / `sensor_readings` / `alarm_events` to one or more
//! downstream MQTT destinations.

use serde::{Deserialize, Serialize};

/// Top-level export config: which streams to export, batching/QoS knobs,
/// and the list of destinations to publish to.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct ExportConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_streams")]
    pub streams: Vec<String>,
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    #[serde(default = "default_drain_interval_ms")]
    pub drain_interval_ms: u64,
    #[serde(default = "default_publish_qos")]
    pub publish_qos: u8,
    #[serde(default)]
    pub destinations: Vec<DestinationConfig>,
}

/// One destination broker. The `local` destination is generally always-on
/// once enabled; `remote` is opt-in for store-and-forward to e.g. cloud.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct DestinationConfig {
    pub broker_id: String,
    #[serde(default)]
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub tls: TlsConfig,
}

/// TLS knobs per destination (separate from the `mqtt.tls` config of the
/// device's primary MQTT client).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct TlsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub ca_cert_path: String,
    #[serde(default)]
    pub insecure_skip_verify: bool,
}

fn default_streams() -> Vec<String> {
    vec![
        "sticker".into(),
        "probe".into(),
        "probe_1m".into(),
        "alarm".into(),
        "eye".into(),
    ]
}
fn default_batch_size() -> usize {
    200
}
fn default_drain_interval_ms() -> u64 {
    500
}
fn default_publish_qos() -> u8 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_config_parses_minimum_yaml() {
        let yaml = r#"
enabled: true
destinations:
  - broker_id: local
    enabled: true
    host: localhost
    port: 1883
"#;
        let cfg: ExportConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.destinations.len(), 1);
        assert_eq!(cfg.destinations[0].broker_id, "local");
        assert_eq!(cfg.batch_size, 200);
        assert_eq!(cfg.drain_interval_ms, 500);
        assert_eq!(cfg.publish_qos, 1);
        assert_eq!(cfg.streams, vec!["sticker", "probe", "probe_1m", "alarm", "eye"]);
    }

    #[test]
    fn export_config_defaults_when_empty() {
        let cfg: ExportConfig = serde_yaml::from_str("{}").unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.destinations.len(), 0);
        assert_eq!(cfg.batch_size, 200);
        assert_eq!(cfg.streams, vec!["sticker", "probe", "probe_1m", "alarm", "eye"]);
    }
}
