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
