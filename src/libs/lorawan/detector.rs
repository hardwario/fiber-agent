//! RAK5146 LoRaWAN gateway hardware detection
//!
//! Checks whether a LoRaWAN concentrator (RAK5146) is present by looking for
//! the SPI device and/or running ChirpStack services.

use std::path::Path;

/// Result of gateway hardware detection
#[derive(Debug, Clone)]
pub struct GatewayDetection {
    /// Whether SPI device for RAK5146 was found
    pub spi_device_present: bool,
    /// Whether chirpstack-concentratord service exists
    pub concentratord_service: bool,
    /// Whether chirpstack service exists
    pub chirpstack_service: bool,
}

impl GatewayDetection {
    /// Returns true if gateway hardware appears to be present
    pub fn is_present(&self) -> bool {
        self.spi_device_present || self.concentratord_service || self.chirpstack_service
    }
}

/// Detect if LoRaWAN gateway hardware is present
pub fn detect_gateway() -> GatewayDetection {
    let spi_device_present = Path::new("/dev/spidev0.0").exists();

    let concentratord_service = systemd_service_exists("chirpstack-concentratord");
    let chirpstack_service = systemd_service_exists("chirpstack");

    let detection = GatewayDetection {
        spi_device_present,
        concentratord_service,
        chirpstack_service,
    };

    if detection.is_present() {
        eprintln!(
            "[LoRaWAN] Gateway detected: SPI={}, concentratord={}, chirpstack={}",
            spi_device_present, concentratord_service, chirpstack_service
        );
    } else {
        eprintln!("[LoRaWAN] No gateway hardware detected");
    }

    detection
}

/// Check if a systemd service unit file exists
fn systemd_service_exists(service_name: &str) -> bool {
    let paths = [
        format!("/etc/systemd/system/{}.service", service_name),
        format!("/lib/systemd/system/{}.service", service_name),
        format!("/usr/lib/systemd/system/{}.service", service_name),
    ];
    paths.iter().any(|p| Path::new(p).exists())
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
            spi_device_present: false,
            concentratord_service: false,
            chirpstack_service: false,
        };
        assert!(!detection.is_present());

        let detection = GatewayDetection {
            spi_device_present: true,
            concentratord_service: false,
            chirpstack_service: false,
        };
        assert!(detection.is_present());
    }
}
