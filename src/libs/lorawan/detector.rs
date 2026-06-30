//! RAK5146 LoRaWAN gateway hardware detection
//!
//! Checks whether a LoRaWAN concentrator (RAK5146) is present by checking
//! if the concentratord service is actually running (it fails without hardware).

use std::path::Path;

/// Result of gateway hardware detection
#[derive(Debug, Clone)]
pub struct GatewayDetection {
    /// Whether chirpstack-concentratord is actively running (requires hardware)
    pub concentratord_running: bool,
    /// Whether chirpstack service is actively running
    pub chirpstack_running: bool,
}

impl GatewayDetection {
    /// Returns true if LoRaWAN is usable: either a local concentrator (RAK5146)
    /// is running, OR ChirpStack is running — which covers devices that have no
    /// local concentrator but receive/transmit via an EXTERNAL gateway registered
    /// in the on-device ChirpStack (Semtech UDP). Without this, the monitor never
    /// starts on external-gateway devices, so fPort-85 sticker config/history
    /// downlinks can't be sent and uplinks aren't re-published.
    pub fn is_present(&self) -> bool {
        self.concentratord_running || self.chirpstack_running
    }
}

/// Detect if LoRaWAN gateway hardware is present
pub fn detect_gateway() -> GatewayDetection {
    let concentratord_running = is_service_running("chirpstack-concentratord");
    let chirpstack_running = is_service_running("chirpstack");

    let detection = GatewayDetection {
        concentratord_running,
        chirpstack_running,
    };

    if detection.is_present() {
        eprintln!(
            "[LoRaWAN] Gateway detected: concentratord={}, chirpstack={}",
            concentratord_running, chirpstack_running
        );
    } else {
        eprintln!(
            "[LoRaWAN] No gateway running: concentratord={}, chirpstack={}",
            concentratord_running, chirpstack_running
        );
    }

    detection
}

/// Returns true if at least one enabled external LoRaWAN gateway is configured.
///
/// Used so the LoRaWAN monitor starts even when the built-in concentrator is
/// absent but an external gateway forwards uplinks over Semtech UDP :1700.
pub fn has_external_gateway() -> bool {
    crate::libs::config::Config::load_default()
        .ok()
        .and_then(|c| c.lorawan)
        .map(|l| l.gateways.iter().any(|g| g.enabled))
        .unwrap_or(false)
}

/// Check if a systemd service is currently active (running)
pub fn is_service_running(service_name: &str) -> bool {
    std::process::Command::new("systemctl")
        .args(["is-active", "--quiet", service_name])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gateway_detection_logic() {
        let detection = GatewayDetection {
            concentratord_running: false,
            chirpstack_running: false,
        };
        assert!(!detection.is_present());

        let detection = GatewayDetection {
            concentratord_running: true,
            chirpstack_running: false,
        };
        assert!(detection.is_present());
    }
}
