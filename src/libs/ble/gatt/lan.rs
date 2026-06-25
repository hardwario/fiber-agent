//! LAN/Ethernet GATT characteristics (FB09 config, FB0C status).
//!
//! Mirrors the WiFi module (`wifi.rs`): pure functions for nmcli-arg
//! construction, validation and parsing are split from the `Command`-calling
//! action functions so the former can be unit-tested without a NetworkManager.
//!
//! `apply_lan_config` deliberately does NOT write to `fiber.config.yaml` — it
//! only detects the interface at runtime and drives nmcli — so it never
//! contends with the ConfigApplier write path (issue #81).

use std::net::Ipv4Addr;
use std::process::Command;

use serde::{Deserialize, Serialize};

use super::net_error::{categorize, NetworkErrorCategory};

/// Candidate wired-interface names, in priority order. Mirrors the list used
/// by `crate::libs::network::status`.
const ETH_INTERFACES: &[&str] = &["eth0", "enp0s3", "enp4s0", "enp0s31f6", "end0"];

// --- Wire types --------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LanMode {
    Dhcp,
    Static,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ipv4Config {
    pub address: String,
    pub prefix: u8,
    pub gateway: String,
    #[serde(default)]
    pub dns: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanConfigRequest {
    pub mode: LanMode,
    /// Present (and required) only when `mode == Static`.
    #[serde(default)]
    pub ipv4: Option<Ipv4Config>,
}

/// FB0C payload. `link` (carrier/operstate) is distinct from `connected`
/// (has an IP): `link=false` → cable unplugged, `link=true,connected=false`
/// → configured but not up yet / failed, both true → OK.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanStatusResponse {
    pub connected: bool,
    pub link: bool,
    pub ip_address: String,
    pub mac: String,
    pub mode: String,
    pub error: String,
}

// --- Pure functions (unit-tested) --------------------------------------------

/// Build the `nmcli connection modify` argument vector for a request.
///
/// DHCP clears the static keys so a previously-static profile reverts cleanly.
/// Static joins DNS entries into one space-separated argument (nmcli's
/// convention) and composes `address/prefix`. An empty DNS list yields an
/// empty `ipv4.dns ""` (valid — DNS is optional).
pub fn build_nmcli_modify_args(conn: &str, req: &LanConfigRequest) -> Vec<String> {
    let mut args = vec!["connection".to_string(), "modify".to_string(), conn.to_string()];
    match req.mode {
        LanMode::Dhcp => {
            args.extend([
                "ipv4.method".to_string(),
                "auto".to_string(),
                "ipv4.addresses".to_string(),
                String::new(),
                "ipv4.gateway".to_string(),
                String::new(),
                "ipv4.dns".to_string(),
                String::new(),
            ]);
        }
        LanMode::Static => {
            // Caller guarantees ipv4 is present and validated before this.
            if let Some(cfg) = &req.ipv4 {
                args.extend([
                    "ipv4.method".to_string(),
                    "manual".to_string(),
                    "ipv4.addresses".to_string(),
                    format!("{}/{}", cfg.address, cfg.prefix),
                    "ipv4.gateway".to_string(),
                    cfg.gateway.clone(),
                    "ipv4.dns".to_string(),
                    cfg.dns.join(" "),
                ]);
            }
        }
    }
    args
}

/// Validate a static IPv4 configuration. DNS is optional, but any provided
/// entry must parse. The gateway must sit in the same subnet as the address
/// (checked for prefixes ≤ 30; /31 and /32 skip the gateway-subnet check).
pub fn validate_static_config(cfg: &Ipv4Config) -> Result<(), NetworkErrorCategory> {
    let addr: Ipv4Addr = cfg
        .address
        .parse()
        .map_err(|_| NetworkErrorCategory::InvalidIp)?;
    if !(1..=32).contains(&cfg.prefix) {
        return Err(NetworkErrorCategory::InvalidIp);
    }
    let gw: Ipv4Addr = cfg
        .gateway
        .parse()
        .map_err(|_| NetworkErrorCategory::InvalidIp)?;
    for d in &cfg.dns {
        let _: Ipv4Addr = d.parse().map_err(|_| NetworkErrorCategory::InvalidIp)?;
    }
    if cfg.prefix <= 30 {
        // prefix is 1..=30 here, so the shift is well-defined.
        let mask: u32 = u32::MAX << (32 - cfg.prefix);
        if (u32::from(addr) & mask) != (u32::from(gw) & mask) {
            return Err(NetworkErrorCategory::GatewayUnreachable);
        }
    }
    Ok(())
}

/// Map an `ipv4.method` value (`auto`/`manual`) to the wire `mode`.
fn method_to_mode(method: &str) -> String {
    match method.trim() {
        "manual" => "static".to_string(),
        "auto" => "dhcp".to_string(),
        other => other.to_string(),
    }
}

// --- Interface / connection detection ----------------------------------------

/// Detect the wired interface by probing `/sys/class/net/<if>`. Returns the
/// first candidate that exists (present even if the cable is unplugged, so a
/// disconnected interface can still be configured).
pub fn detect_lan_interface() -> Option<String> {
    for iface in ETH_INTERFACES {
        if std::path::Path::new(&format!("/sys/class/net/{}", iface)).exists() {
            return Some((*iface).to_string());
        }
    }
    None
}

/// Read the MAC address of an interface from sysfs.
fn read_mac(iface: &str) -> String {
    std::fs::read_to_string(format!("/sys/class/net/{}/address", iface))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Find the NetworkManager connection profile bound to `iface`, if any.
fn find_eth_connection_name(iface: &str) -> Option<String> {
    let output = Command::new("nmcli")
        .args(["-t", "-f", "NAME,DEVICE", "connection", "show"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() >= 2 && parts[1] == iface && !parts[0].is_empty() {
            return Some(parts[0].to_string());
        }
    }
    None
}

/// Read the configured `ipv4.method` of a connection profile → wire `mode`.
fn read_connection_mode(conn: &str) -> String {
    if let Ok(output) = Command::new("nmcli")
        .args(["-t", "-f", "ipv4.method", "connection", "show", conn])
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Output is like `ipv4.method:manual`.
        if let Some(rest) = stdout.lines().next().and_then(|l| l.split(':').nth(1)) {
            return method_to_mode(rest);
        }
    }
    String::new()
}

/// Ensure a connection profile exists for `iface`, creating one with an
/// add-fallback (`nmcli connection add type ethernet`) when none is bound.
fn ensure_eth_connection(iface: &str) -> Result<String, NetworkErrorCategory> {
    if let Some(conn) = find_eth_connection_name(iface) {
        return Ok(conn);
    }
    eprintln!("[LAN] No profile for {}; creating one", iface);
    let out = Command::new("nmcli")
        .args([
            "connection",
            "add",
            "type",
            "ethernet",
            "con-name",
            iface,
            "ifname",
            iface,
        ])
        .output()
        .map_err(|e| {
            eprintln!("[LAN] connection add spawn error: {}", e);
            NetworkErrorCategory::Other
        })?;
    if out.status.success() {
        Ok(iface.to_string())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        eprintln!("[LAN] connection add FAILED: {}", stderr.trim());
        Err(categorize(&stderr))
    }
}

/// Run an nmcli invocation, mapping a non-zero exit to a categorized error.
fn run_nmcli(args: &[String]) -> Result<(), NetworkErrorCategory> {
    let out = Command::new("nmcli").args(args).output().map_err(|e| {
        eprintln!("[LAN] nmcli spawn error: {}", e);
        NetworkErrorCategory::Other
    })?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        eprintln!("[LAN] nmcli {:?} FAILED: {}", args, stderr.trim());
        Err(categorize(&stderr))
    }
}

// --- Actions -----------------------------------------------------------------

/// Apply a LAN configuration via NetworkManager.
///
/// Steps: detect interface → (static) validate → find/create the connection
/// profile → `nmcli connection modify` → `nmcli connection up`.
pub fn apply_lan_config(req: &LanConfigRequest) -> Result<(), NetworkErrorCategory> {
    let iface = detect_lan_interface().ok_or(NetworkErrorCategory::NotFound)?;
    eprintln!("[LAN] Configure requested: iface={} mode={:?}", iface, req.mode);

    if let LanMode::Static = req.mode {
        let cfg = req.ipv4.as_ref().ok_or(NetworkErrorCategory::InvalidIp)?;
        validate_static_config(cfg)?;
    }

    let conn = ensure_eth_connection(&iface)?;
    run_nmcli(&build_nmcli_modify_args(&conn, req))?;
    run_nmcli(&[
        "connection".to_string(),
        "up".to_string(),
        conn.clone(),
    ])?;

    eprintln!("[LAN] Applied {:?} on {} (conn={})", req.mode, iface, conn);
    Ok(())
}

/// Current LAN status for FB0C.
///
/// Reuses `get_network_status()` (operstate → `link`, IP presence →
/// `connected`); `mode` comes from the bound connection's `ipv4.method`.
pub fn get_lan_status() -> LanStatusResponse {
    let iface = detect_lan_interface();
    let status = crate::libs::network::get_network_status();

    let link = status.ethernet_connected;
    let ip = status.ethernet_ip.unwrap_or_default();
    let connected = !ip.is_empty();

    let (mac, mode) = match iface.as_deref() {
        Some(i) => {
            let mode = find_eth_connection_name(i)
                .map(|c| read_connection_mode(&c))
                .unwrap_or_default();
            (read_mac(i), mode)
        }
        None => (String::new(), String::new()),
    };

    LanStatusResponse {
        connected,
        link,
        ip_address: ip,
        mac,
        mode,
        error: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn static_cfg() -> Ipv4Config {
        Ipv4Config {
            address: "192.168.1.50".to_string(),
            prefix: 24,
            gateway: "192.168.1.1".to_string(),
            dns: vec!["8.8.8.8".to_string(), "1.1.1.1".to_string()],
        }
    }

    #[test]
    fn modify_args_dhcp_clears_static_keys() {
        let req = LanConfigRequest { mode: LanMode::Dhcp, ipv4: None };
        let args = build_nmcli_modify_args("eth0", &req);
        assert_eq!(&args[0..3], &["connection", "modify", "eth0"]);
        assert!(args.windows(2).any(|w| w == ["ipv4.method", "auto"]));
        // static keys are present but emptied
        let dns_idx = args.iter().position(|a| a == "ipv4.dns").unwrap();
        assert_eq!(args[dns_idx + 1], "");
        let gw_idx = args.iter().position(|a| a == "ipv4.gateway").unwrap();
        assert_eq!(args[gw_idx + 1], "");
    }

    #[test]
    fn modify_args_static_composes_fields() {
        let req = LanConfigRequest { mode: LanMode::Static, ipv4: Some(static_cfg()) };
        let args = build_nmcli_modify_args("eth0", &req);
        assert!(args.windows(2).any(|w| w == ["ipv4.method", "manual"]));
        let addr_idx = args.iter().position(|a| a == "ipv4.addresses").unwrap();
        assert_eq!(args[addr_idx + 1], "192.168.1.50/24");
        let gw_idx = args.iter().position(|a| a == "ipv4.gateway").unwrap();
        assert_eq!(args[gw_idx + 1], "192.168.1.1");
        let dns_idx = args.iter().position(|a| a == "ipv4.dns").unwrap();
        assert_eq!(args[dns_idx + 1], "8.8.8.8 1.1.1.1");
    }

    #[test]
    fn modify_args_static_empty_dns_ok() {
        let mut cfg = static_cfg();
        cfg.dns.clear();
        let req = LanConfigRequest { mode: LanMode::Static, ipv4: Some(cfg) };
        let args = build_nmcli_modify_args("eth0", &req);
        let dns_idx = args.iter().position(|a| a == "ipv4.dns").unwrap();
        assert_eq!(args[dns_idx + 1], "");
    }

    #[test]
    fn validate_ok() {
        assert!(validate_static_config(&static_cfg()).is_ok());
    }

    #[test]
    fn validate_empty_dns_ok() {
        let mut cfg = static_cfg();
        cfg.dns.clear();
        assert!(validate_static_config(&cfg).is_ok());
    }

    #[test]
    fn validate_gateway_out_of_subnet() {
        let mut cfg = static_cfg();
        cfg.gateway = "10.0.0.1".to_string();
        assert_eq!(
            validate_static_config(&cfg),
            Err(NetworkErrorCategory::GatewayUnreachable)
        );
    }

    #[test]
    fn validate_bad_address() {
        let mut cfg = static_cfg();
        cfg.address = "999.1.1.1".to_string();
        assert_eq!(validate_static_config(&cfg), Err(NetworkErrorCategory::InvalidIp));
    }

    #[test]
    fn validate_bad_prefix() {
        let mut cfg = static_cfg();
        cfg.prefix = 33;
        assert_eq!(validate_static_config(&cfg), Err(NetworkErrorCategory::InvalidIp));
        cfg.prefix = 0;
        assert_eq!(validate_static_config(&cfg), Err(NetworkErrorCategory::InvalidIp));
    }

    #[test]
    fn validate_bad_dns() {
        let mut cfg = static_cfg();
        cfg.dns = vec!["not-an-ip".to_string()];
        assert_eq!(validate_static_config(&cfg), Err(NetworkErrorCategory::InvalidIp));
    }

    #[test]
    fn validate_slash31_skips_gateway_subnet_check() {
        let cfg = Ipv4Config {
            address: "192.168.1.50".to_string(),
            prefix: 31,
            gateway: "10.9.9.9".to_string(),
            dns: vec![],
        };
        assert!(validate_static_config(&cfg).is_ok());
    }

    #[test]
    fn deserialize_dhcp_request() {
        let json = r#"{"mode":"dhcp"}"#;
        let req: LanConfigRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.mode, LanMode::Dhcp);
        assert!(req.ipv4.is_none());
    }

    #[test]
    fn deserialize_static_request() {
        let json = r#"{"mode":"static","ipv4":{"address":"192.168.1.50","prefix":24,"gateway":"192.168.1.1","dns":["8.8.8.8","1.1.1.1"]}}"#;
        let req: LanConfigRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.mode, LanMode::Static);
        let cfg = req.ipv4.unwrap();
        assert_eq!(cfg.address, "192.168.1.50");
        assert_eq!(cfg.prefix, 24);
        assert_eq!(cfg.dns.len(), 2);
    }

    #[test]
    fn status_response_serializes_expected_fields() {
        let resp = LanStatusResponse {
            connected: true,
            link: true,
            ip_address: "192.168.1.50".to_string(),
            mac: "aa:bb:cc:dd:ee:ff".to_string(),
            mode: "static".to_string(),
            error: String::new(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        for key in ["connected", "link", "ip_address", "mac", "mode", "error"] {
            assert!(json.contains(key), "missing key {}", key);
        }
    }

    #[test]
    fn method_to_mode_maps() {
        assert_eq!(method_to_mode("manual"), "static");
        assert_eq!(method_to_mode("auto"), "dhcp");
    }
}
