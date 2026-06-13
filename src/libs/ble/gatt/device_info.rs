//! Device Info characteristic (FB07) — read-only, no auth required.

use serde::{Deserialize, Serialize};
use std::process::Command;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceInfoResponse {
    pub hostname: String,
    pub version: String,
    pub uptime: String,
    pub mac_address: String,
}

/// Firmware version source-of-truth: prefer `FIBER_VERSION` (set by Yocto
/// from the upstream git tag at build time), fall back to `CARGO_PKG_VERSION`
/// for local cargo builds. Matches the logic in `src/main.rs` that fills
/// `config.system.app_version` — the same value published over MQTT — so
/// the BLE Device Info characteristic (FB07) reports the same string the
/// dashboard sees over MQTT.
fn firmware_version() -> &'static str {
    option_env!("FIBER_VERSION")
        .filter(|v| !v.is_empty())
        .unwrap_or(env!("CARGO_PKG_VERSION"))
}

pub fn build_response(hostname: &str, mac: &str) -> DeviceInfoResponse {
    let uptime = Command::new("uptime")
        .arg("-p")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    DeviceInfoResponse {
        hostname: hostname.to_string(),
        version: firmware_version().to_string(),
        uptime,
        mac_address: mac.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_response_carries_hostname_mac_and_version() {
        let r = build_response("FIBER-TEST", "AA:BB:CC:DD:EE:FF");
        assert_eq!(r.hostname, "FIBER-TEST");
        assert_eq!(r.mac_address, "AA:BB:CC:DD:EE:FF");
        assert!(!r.version.is_empty(), "version pulled from FIBER_VERSION or CARGO_PKG_VERSION");
    }
}
