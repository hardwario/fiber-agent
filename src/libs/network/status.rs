// Network connection status detection
// Reads from /sys/class/net/ to determine WiFi and Ethernet connectivity

use std::fs;

/// Network connection status
#[derive(Debug, Clone, Copy)]
pub struct NetworkStatus {
    /// WiFi connection state
    pub wifi_connected: bool,
    /// WiFi signal strength in dBm (-30 to -90 typical range)
    pub wifi_signal_strength: i32,
    /// Ethernet connection state
    pub ethernet_connected: bool,
}

impl NetworkStatus {
    /// Create a disconnected network status
    pub fn disconnected() -> Self {
        Self {
            wifi_connected: false,
            wifi_signal_strength: -90,
            ethernet_connected: false,
        }
    }
}

/// Get current network connection status from system files
pub fn get_network_status() -> NetworkStatus {
    let mut status = NetworkStatus::disconnected();

    // Check Ethernet connection
    if is_interface_up("eth0") {
        status.ethernet_connected = true;
        return status; // Prioritize Ethernet
    }

    // Check WiFi connection
    if is_interface_up("wlan0") {
        status.wifi_connected = true;
        status.wifi_signal_strength = read_wifi_signal_strength();
    }

    status
}

/// Check if a network interface is up
fn is_interface_up(interface: &str) -> bool {
    let operstate_path = format!("/sys/class/net/{}/operstate", interface);
    match fs::read_to_string(&operstate_path) {
        Ok(state) => state.trim().eq_ignore_ascii_case("up"),
        Err(_) => false,
    }
}

/// Read WiFi signal strength (RSSI) in dBm
fn read_wifi_signal_strength() -> i32 {
    // Try multiple methods to read signal strength

    // Method 1: Try reading from iw command output if available
    if let Ok(output) = std::process::Command::new("iw")
        .args(&["dev", "wlan0", "link"])
        .output()
    {
        if let Ok(output_str) = String::from_utf8(output.stdout) {
            if let Some(rssi) = parse_iw_output(&output_str) {
                return rssi;
            }
        }
    }

    // Method 2: Try reading from /proc/net/wireless
    if let Some(rssi) = read_proc_wireless() {
        return rssi;
    }

    // Default to weak signal if we can't read
    -80
}

/// Parse output from `iw dev wlan0 link` command
fn parse_iw_output(output: &str) -> Option<i32> {
    for line in output.lines() {
        if line.contains("signal:") {
            // Line format: "signal: -45 dBm"
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(rssi) = parts[1].parse::<i32>() {
                    return Some(rssi);
                }
            }
        }
    }
    None
}

/// Read signal strength from /proc/net/wireless
fn read_proc_wireless() -> Option<i32> {
    match fs::read_to_string("/proc/net/wireless") {
        Ok(content) => {
            for line in content.lines().skip(2) {
                // Line format: "wlan0: 0000   XX.  -45.  -90."
                if line.contains("wlan0") {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 4 {
                        // Try to parse the signal strength (usually in parts[3])
                        if let Ok(rssi) = parts[3].parse::<i32>() {
                            return Some(rssi);
                        }
                    }
                }
            }
            None
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_iw_output() {
        let output = r#"Connected to aa:bb:cc:dd:ee:ff (on wlan0)
	SSID: MyNetwork
	freq: 2462
	signal: -45 dBm
	tx bitrate: 72.2 MBit/s"#;

        assert_eq!(parse_iw_output(output), Some(-45));
    }

    #[test]
    fn test_parse_iw_output_no_signal() {
        let output = "No connection";
        assert_eq!(parse_iw_output(output), None);
    }
}
