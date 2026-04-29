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

const FIRMWARE_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn build_response(hostname: &str, mac: &str) -> DeviceInfoResponse {
    let uptime = Command::new("uptime")
        .arg("-p")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    DeviceInfoResponse {
        hostname: hostname.to_string(),
        version: FIRMWARE_VERSION.to_string(),
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
        assert!(!r.version.is_empty(), "version pulled from CARGO_PKG_VERSION");
    }
}
