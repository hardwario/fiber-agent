// Network connection status detection
// Reads from /sys/class/net/ to determine WiFi and Ethernet connectivity

use std::fs;

/// Network connection status
#[derive(Debug, Clone)]
pub struct NetworkStatus {
    /// WiFi connection state
    pub wifi_connected: bool,
    /// WiFi signal strength in dBm (-30 to -90 typical range)
    pub wifi_signal_strength: i32,
    /// WiFi IP address (if connected)
    pub wifi_ip: Option<String>,
    /// Ethernet connection state
    pub ethernet_connected: bool,
    /// Ethernet IP address (if connected)
    pub ethernet_ip: Option<String>,
}

impl NetworkStatus {
    /// Create a disconnected network status
    pub fn disconnected() -> Self {
        Self {
            wifi_connected: false,
            wifi_signal_strength: -90,
            wifi_ip: None,
            ethernet_connected: false,
            ethernet_ip: None,
        }
    }
}

/// Get current network connection status from system files
pub fn get_network_status() -> NetworkStatus {
    let mut status = NetworkStatus::disconnected();

    // Check Ethernet connection (try common names)
    let eth_interfaces = ["eth0", "enp0s3", "enp4s0", "enp0s31f6", "end0"];
    for iface in &eth_interfaces {
        if is_interface_up(iface) {
            status.ethernet_connected = true;
            status.ethernet_ip = get_interface_ip(iface);
            break;
        }
    }

    // Check WiFi connection (try common names)
    let wifi_interfaces = ["wlan0", "wlp3s0", "wlp4s0", "wlo1"];
    for iface in &wifi_interfaces {
        if is_interface_up(iface) {
            status.wifi_connected = true;
            status.wifi_signal_strength = read_wifi_signal_strength();
            status.wifi_ip = get_interface_ip(iface);
            break;
        }
    }

    status
}

/// Get IP address of a network interface using ip command
fn get_interface_ip(interface: &str) -> Option<String> {
    // Try using ip command to get IPv4 address
    if let Ok(output) = std::process::Command::new("ip")
        .args(["addr", "show", interface])
        .output()
    {
        if let Ok(output_str) = String::from_utf8(output.stdout) {
            // Look for inet line: "inet 192.168.1.100/24 brd ..."
            for line in output_str.lines() {
                let line = line.trim();
                if line.starts_with("inet ") && !line.starts_with("inet6") {
                    // Parse: "inet 192.168.1.100/24 ..."
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 2 {
                        // Remove subnet mask (e.g., /24)
                        let ip_with_mask = parts[1];
                        if let Some(ip) = ip_with_mask.split('/').next() {
                            return Some(ip.to_string());
                        }
                    }
                }
            }
        }
    }
    None
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
