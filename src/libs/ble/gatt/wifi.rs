//! WiFi GATT characteristics (FB02 scan, FB03 connect, FB04 status, FB08 disconnect).
//! Parses nmcli output and shells out to nmcli for actions.

use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WiFiNetwork {
    pub ssid: String,
    pub signal: i32,
    pub security: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WiFiConnectRequest {
    pub ssid: String,
    pub password: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WiFiStatusResponse {
    pub connected: bool,
    pub ssid: String,
    pub ip_address: String,
    pub error: String,
}

/// Parse `nmcli -t -f SSID,SIGNAL,SECURITY dev wifi list` output.
/// Each line is `SSID:SIGNAL:SECURITY`. Empty SSIDs are skipped.
pub fn parse_nmcli_wifi_list(stdout: &str) -> Vec<WiFiNetwork> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() >= 2 && !parts[0].is_empty() {
            out.push(WiFiNetwork {
                ssid: parts[0].to_string(),
                signal: parts.get(1).copied().unwrap_or("0").parse().unwrap_or(0),
                security: parts.get(2).copied().unwrap_or("").to_string(),
            });
        }
    }
    out
}

/// Parse `nmcli -t -f DEVICE,STATE,CONNECTION dev status` output and
/// return the wlan0 row if connected.
pub fn parse_nmcli_dev_status(stdout: &str) -> Option<(String, String)> {
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("wlan0:") {
            let parts: Vec<&str> = rest.split(':').collect();
            if parts.len() >= 2 && parts[0] == "connected" {
                return Some(("wlan0".to_string(), parts[1].to_string()));
            }
        }
    }
    None
}

/// Parse `ip addr show wlan0` output and return the first IPv4 address.
pub fn parse_ip_addr_show(stdout: &str) -> String {
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("inet ") {
            if let Some(addr_with_mask) = trimmed.split_whitespace().nth(1) {
                return addr_with_mask.split('/').next().unwrap_or(addr_with_mask).to_string();
            }
        }
    }
    String::new()
}

use std::process::Command;
use regex::Regex;

/// Scan for available WiFi networks using nmcli, falling back to iwlist.
pub(crate) fn scan_wifi() -> Vec<WiFiNetwork> {
    let mut networks = Vec::new();

    // Try nmcli first
    if let Ok(output) = Command::new("nmcli")
        .args(["-t", "-f", "SSID,SIGNAL,SECURITY", "dev", "wifi", "list"])
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        networks = parse_nmcli_wifi_list(&stdout);
        return networks;
    }

    // Fallback to iwlist
    if let Ok(output) = Command::new("iwlist").args(["wlan0", "scan"]).output() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut current = WiFiNetwork::default();

        for line in stdout.lines() {
            let line = line.trim();
            if line.contains("ESSID:") {
                if let Some(ssid) = line.split("ESSID:").nth(1) {
                    let ssid = ssid.trim_matches('"');
                    if !ssid.is_empty() {
                        current.ssid = ssid.to_string();
                    }
                }
            } else if line.contains("Quality=") {
                let re = Regex::new(r"Quality=(\d+)/(\d+)").unwrap();
                if let Some(caps) = re.captures(line) {
                    let quality: f32 = caps[1].parse().unwrap_or(0.0);
                    let max: f32 = caps[2].parse().unwrap_or(1.0);
                    current.signal = ((quality / max) * 100.0) as i32;
                }
            } else if line.contains("Encryption key:") {
                current.security = if line.to_lowercase().contains("off") {
                    "Open".to_string()
                } else {
                    "Secured".to_string()
                };

                if !current.ssid.is_empty() {
                    networks.push(current.clone());
                    current = WiFiNetwork::default();
                }
            }
        }
    }

    networks
}

/// Connect to a WiFi network by explicitly building a NetworkManager profile.
///
/// `nmcli dev wifi connect` relies on scan-cache state to infer the security
/// type. When the cache is stale (most common right after a disconnect from
/// the same network), nmcli emits
/// `Error: 802-11-wireless-security.key-mgmt: property is missing` and
/// refuses to create the connection. To dodge that whole class of state bugs
/// we build the profile by hand:
///
/// 1. Delete any existing profile with the same SSID name (clean slate).
/// 2. Rescan so wlan0 has a fresh view of nearby networks.
/// 3. `nmcli connection add type wifi ...` creates the profile.
/// 4. `nmcli connection modify ... wifi-sec.key-mgmt wpa-psk wifi-sec.psk ...`
///    sets WPA-PSK security explicitly (skipped for open networks).
/// 5. `nmcli connection up <ssid>` activates it.
///
/// If any step after the `add` fails we delete the partial profile so the
/// next attempt starts clean.
pub(crate) fn connect_wifi(ssid: &str, password: &str) -> WiFiStatusResponse {
    eprintln!("[WiFi] Connect requested: ssid='{}'", ssid);

    // Step 1: cleanup stale profile, if any.
    if let Ok(output) = Command::new("nmcli")
        .args(["connection", "delete", ssid])
        .output()
    {
        if output.status.success() {
            eprintln!("[WiFi] Cleaned up stale profile '{}'", ssid);
        }
    }

    // Step 2: refresh scan cache. Best-effort — failures here aren't fatal
    // since the network may still be reachable.
    let _ = Command::new("nmcli").args(["dev", "wifi", "rescan"]).output();

    // Step 3: create the profile.
    let add_result = Command::new("nmcli")
        .args([
            "connection", "add",
            "type", "wifi",
            "con-name", ssid,
            "ifname", "wlan0",
            "ssid", ssid,
        ])
        .output();
    match add_result {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).to_string();
            eprintln!("[WiFi] connection add '{}' FAILED: {}", ssid, stderr.trim());
            return failure(ssid, stderr);
        }
        Err(e) => {
            eprintln!("[WiFi] connection add '{}' spawn error: {}", ssid, e);
            return failure(ssid, e.to_string());
        }
    }

    // Step 4: set WPA-PSK + password (skipped for open networks).
    if !password.is_empty() {
        let modify_result = Command::new("nmcli")
            .args([
                "connection", "modify", ssid,
                "wifi-sec.key-mgmt", "wpa-psk",
                "wifi-sec.psk", password,
            ])
            .output();
        match modify_result {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr).to_string();
                eprintln!("[WiFi] connection modify '{}' FAILED: {}", ssid, stderr.trim());
                let _ = Command::new("nmcli").args(["connection", "delete", ssid]).output();
                return failure(ssid, stderr);
            }
            Err(e) => {
                eprintln!("[WiFi] connection modify '{}' spawn error: {}", ssid, e);
                let _ = Command::new("nmcli").args(["connection", "delete", ssid]).output();
                return failure(ssid, e.to_string());
            }
        }
    }

    // Step 5: activate the profile.
    let up_result = Command::new("nmcli")
        .args(["connection", "up", ssid])
        .output();
    match up_result {
        Ok(o) if o.status.success() => {
            let ip = get_ip_address();
            eprintln!("[WiFi] Connected to '{}' (ip={})", ssid, ip);
            WiFiStatusResponse {
                connected: true,
                ssid: ssid.to_string(),
                ip_address: ip,
                error: String::new(),
            }
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).to_string();
            eprintln!("[WiFi] connection up '{}' FAILED: {}", ssid, stderr.trim());
            let _ = Command::new("nmcli").args(["connection", "delete", ssid]).output();
            failure(ssid, stderr)
        }
        Err(e) => {
            eprintln!("[WiFi] connection up '{}' spawn error: {}", ssid, e);
            let _ = Command::new("nmcli").args(["connection", "delete", ssid]).output();
            failure(ssid, e.to_string())
        }
    }
}

fn failure(ssid: &str, error: String) -> WiFiStatusResponse {
    WiFiStatusResponse {
        connected: false,
        ssid: ssid.to_string(),
        ip_address: String::new(),
        error,
    }
}

/// Find the active NetworkManager connection profile bound to wlan0, if any.
fn active_connection_on_wlan0() -> Option<String> {
    let output = Command::new("nmcli")
        .args(["-t", "-f", "NAME,DEVICE", "connection", "show", "--active"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() >= 2 && parts[1] == "wlan0" && !parts[0].is_empty() {
            return Some(parts[0].to_string());
        }
    }
    None
}

/// Disconnect from WiFi network.
///
/// Also deletes the active connection profile so NetworkManager does not
/// auto-reconnect on the next radio event. Without this, an explicit
/// disconnect followed by a connect to a different SSID often loses the
/// race against NM's autoconnect of the previous profile.
pub(crate) fn disconnect_wifi() -> WiFiStatusResponse {
    let active = active_connection_on_wlan0();
    eprintln!("[WiFi] Disconnect requested (active profile: {:?})", active);

    let disconnect_result = Command::new("nmcli")
        .args(["dev", "disconnect", "wlan0"])
        .output();

    // Delete the active profile so NM doesn't auto-reconnect.
    if let Some(ref name) = active {
        match Command::new("nmcli")
            .args(["connection", "delete", name])
            .output()
        {
            Ok(o) if o.status.success() => eprintln!("[WiFi] Deleted profile '{}'", name),
            Ok(o) => eprintln!(
                "[WiFi] Profile delete '{}' stderr: {}",
                name,
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => eprintln!("[WiFi] Profile delete '{}' spawn error: {}", name, e),
        }
    }

    match disconnect_result {
        Ok(output) if output.status.success() => {
            eprintln!("[WiFi] Disconnected successfully");
            WiFiStatusResponse {
                connected: false,
                ssid: String::new(),
                ip_address: String::new(),
                error: String::new(),
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            eprintln!("[WiFi] Disconnect FAILED: {}", stderr.trim());
            let current = get_wifi_status();
            WiFiStatusResponse {
                connected: current.connected,
                ssid: current.ssid,
                ip_address: String::new(),
                error: stderr,
            }
        }
        Err(e) => {
            eprintln!("[WiFi] Disconnect command spawn failed: {}", e);
            WiFiStatusResponse {
                connected: true,
                ssid: String::new(),
                ip_address: String::new(),
                error: e.to_string(),
            }
        }
    }
}

/// Get current WiFi status.
pub(crate) fn get_wifi_status() -> WiFiStatusResponse {
    if let Ok(output) = Command::new("nmcli")
        .args(["-t", "-f", "DEVICE,STATE,CONNECTION", "dev", "status"])
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some((_device, ssid)) = parse_nmcli_dev_status(&stdout) {
            return WiFiStatusResponse {
                connected: true,
                ssid,
                ip_address: get_ip_address(),
                error: String::new(),
            };
        }
    }

    WiFiStatusResponse {
        connected: false,
        ssid: String::new(),
        ip_address: String::new(),
        error: String::new(),
    }
}

/// Get IP address of wlan0.
pub(crate) fn get_ip_address() -> String {
    if let Ok(output) = Command::new("ip").args(["addr", "show", "wlan0"]).output() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        return parse_ip_addr_show(&stdout);
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_wifi_list_typical() {
        let stdout = "MyHome:85:WPA2\nGuestWiFi:60:--\n:0:WPA2\nFiberLab:42:WPA1 WPA2";
        let parsed = parse_nmcli_wifi_list(stdout);
        assert_eq!(parsed.len(), 3, "empty-SSID line must be skipped");
        assert_eq!(parsed[0], WiFiNetwork {
            ssid: "MyHome".into(), signal: 85, security: "WPA2".into() });
        assert_eq!(parsed[1].security, "--");
        assert_eq!(parsed[2].security, "WPA1 WPA2");
    }

    #[test]
    fn parse_wifi_list_handles_missing_columns() {
        let stdout = "OnlySSID";
        let parsed = parse_nmcli_wifi_list(stdout);
        // Only SSID, no signal column → < 2 parts, line skipped.
        assert_eq!(parsed.len(), 0);
    }

    #[test]
    fn parse_wifi_list_empty_input() {
        assert!(parse_nmcli_wifi_list("").is_empty());
    }

    #[test]
    fn parse_dev_status_connected() {
        let stdout = "lo:unmanaged:\nwlan0:connected:MyHome\neth0:disconnected:";
        assert_eq!(parse_nmcli_dev_status(stdout),
                   Some(("wlan0".to_string(), "MyHome".to_string())));
    }

    #[test]
    fn parse_dev_status_disconnected() {
        let stdout = "wlan0:disconnected:";
        assert_eq!(parse_nmcli_dev_status(stdout), None);
    }

    #[test]
    fn parse_ip_extracts_v4() {
        let stdout = r#"
3: wlan0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc fq_codel state UP group default qlen 1000
    link/ether dc:a6:32:11:22:33 brd ff:ff:ff:ff:ff:ff
    inet 192.168.1.42/24 brd 192.168.1.255 scope global dynamic noprefixroute wlan0
       valid_lft 86385sec preferred_lft 86385sec
"#;
        assert_eq!(parse_ip_addr_show(stdout), "192.168.1.42");
    }

    #[test]
    fn parse_ip_no_match_returns_empty() {
        assert_eq!(parse_ip_addr_show("no inet line here"), "");
    }
}
