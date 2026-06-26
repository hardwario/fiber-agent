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
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::net_error::{categorize, NetworkErrorCategory};

/// Candidate wired-interface names, in priority order. Mirrors the list used
/// by `crate::libs::network::status`.
const ETH_INTERFACES: &[&str] = &["eth0", "enp0s3", "enp4s0", "enp0s31f6", "end0"];

/// Last LAN configuration error, surfaced in FB0C `error` so a failed FB09
/// write is observable on a subsequent status read. Set on every
/// `apply_lan_config` (cleared on success, set to the category on failure).
static LAST_LAN_ERROR: Mutex<Option<String>> = Mutex::new(None);

fn set_last_error(err: Option<String>) {
    if let Ok(mut guard) = LAST_LAN_ERROR.lock() {
        *guard = err;
    }
}

fn last_error() -> String {
    LAST_LAN_ERROR
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default()
}

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

/// FB0C payload. `link` (physical carrier) is distinct from `connected`
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

/// Read the physical link (carrier) state of an interface from sysfs.
///
/// `/sys/class/net/<if>/carrier` is "1" when a cable is plugged in and "0"
/// when not. It is the truth source for `link` — unlike `operstate`, which on
/// wired NICs (e.g. the RPi CM4) frequently reports "unknown" even with a live
/// cable, so deriving `link` from operstate would falsely report "unplugged".
/// `carrier` is only readable while the interface is administratively up
/// (EINVAL otherwise); a down interface has no usable link, so an unreadable
/// value maps to `false`.
fn read_link(iface: &str) -> bool {
    std::fs::read_to_string(format!("/sys/class/net/{}/carrier", iface))
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

/// Read the MAC address of an interface from sysfs.
fn read_mac(iface: &str) -> String {
    std::fs::read_to_string(format!("/sys/class/net/{}/address", iface))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Read the static `connection.interface-name` property of a profile.
fn connection_interface_name(conn: &str) -> Option<String> {
    let output = Command::new("nmcli")
        .args(["-g", "connection.interface-name", "connection", "show", conn])
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// Find the NetworkManager connection profile bound to `iface`, returning its
/// UUID, if any.
///
/// Matches on the **static** `connection.interface-name` property rather than
/// the runtime `DEVICE` column: `DEVICE` is empty for an *inactive* profile
/// (e.g. an unplugged cable), which would make the caller miss an existing
/// profile and create a duplicate on the same interface (verified on hardware).
/// Profiles are identified by UUID — UUIDs never contain ':', so this also
/// sidesteps ':'-escaping in display names.
fn find_eth_connection_name(iface: &str) -> Option<String> {
    let output = Command::new("nmcli")
        .args(["-t", "-f", "UUID,TYPE", "connection", "show"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        // UUID is the first field and never contains ':', so split_once is safe.
        let Some((uuid, ctype)) = line.split_once(':') else {
            continue;
        };
        if ctype == "802-3-ethernet"
            && connection_interface_name(uuid).as_deref() == Some(iface)
        {
            return Some(uuid.to_string());
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
///
/// Returns the profile name and whether it was freshly created — the caller
/// deletes a just-created stub if the subsequent `modify` fails, while leaving
/// an existing profile intact.
fn ensure_eth_connection(iface: &str) -> Result<(String, bool), NetworkErrorCategory> {
    if let Some(conn) = find_eth_connection_name(iface) {
        return Ok((conn, false));
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
        Ok((iface.to_string(), true))
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        eprintln!("[LAN] connection add FAILED: {}", stderr.trim());
        Err(categorize(&stderr))
    }
}

/// Clean up after a failed `connection modify`. A freshly-created profile is
/// an empty stub with no usable config, so it is deleted. An existing profile
/// is left untouched: `nmcli modify` is transactional (a failed modify leaves
/// the profile unchanged), so we must not clobber the user's previous working
/// config. Best-effort — failures are logged but do not mask the original error.
///
/// Note: this is intentionally NOT called on a failed `up`. Once `modify`
/// succeeds the profile holds a validated config; an `up` failure is usually
/// just "no carrier yet" (the legitimate configure-first-plug-in-later flow),
/// so the config must stay written for the next boot / cable insertion.
fn cleanup_failed_modify(conn: &str, created: bool) {
    if created {
        eprintln!("[LAN] modify failed; deleting freshly-created profile {}", conn);
        let _ = Command::new("nmcli")
            .args(["connection", "delete", conn])
            .output();
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
/// Thin wrapper over `apply_lan_config_inner` that records the outcome in
/// `LAST_LAN_ERROR` so FB0C can surface the reason of a failed FB09 write.
pub fn apply_lan_config(req: &LanConfigRequest) -> Result<(), NetworkErrorCategory> {
    let result = apply_lan_config_inner(req);
    match &result {
        Ok(()) => set_last_error(None),
        Err(cat) => set_last_error(Some(cat.as_str().to_string())),
    }
    result
}

/// Apply a LAN configuration via NetworkManager.
///
/// Steps: detect interface → (static) validate → find/create the connection
/// profile → `nmcli connection modify` → `nmcli connection up`.
///
/// Rollback policy: a failed `modify` deletes a just-created stub profile (an
/// existing one is left intact — modify is transactional). A failed `up` does
/// NOT roll back: the config is already validated and written, and an `up`
/// failure is typically just a missing carrier, so the config stays put for
/// the next boot / cable insertion. The failure is still surfaced via FB0C.
fn apply_lan_config_inner(req: &LanConfigRequest) -> Result<(), NetworkErrorCategory> {
    let iface = detect_lan_interface().ok_or(NetworkErrorCategory::NotFound)?;
    eprintln!("[LAN] Configure requested: iface={} mode={:?}", iface, req.mode);

    if let LanMode::Static = req.mode {
        let cfg = req.ipv4.as_ref().ok_or(NetworkErrorCategory::InvalidIp)?;
        validate_static_config(cfg)?;
    }

    let (conn, created) = ensure_eth_connection(&iface)?;

    if let Err(e) = run_nmcli(&build_nmcli_modify_args(&conn, req)) {
        cleanup_failed_modify(&conn, created);
        return Err(e);
    }
    // `--wait` bounds how long `up` blocks: bringing the link up on a dead
    // cable (a legitimate "configure first, plug in later" flow) would
    // otherwise hang on nmcli's ~90 s default and tie up the worker thread.
    // We keep the (validated) config written even if `up` fails for exactly
    // that flow — see the rollback policy above.
    if let Err(e) = run_nmcli(&[
        "--wait".to_string(),
        "20".to_string(),
        "connection".to_string(),
        "up".to_string(),
        conn.clone(),
    ]) {
        eprintln!("[LAN] 'up' failed on {} (config kept for next link/boot)", conn);
        return Err(e);
    }

    eprintln!("[LAN] Applied {:?} on {} (conn={})", req.mode, iface, conn);
    Ok(())
}

/// Current LAN status for FB0C.
///
/// `link` is the physical carrier (`/sys/class/net/<if>/carrier`) and
/// `connected` is "has an IP" — read **independently** so the three states
/// stay distinguishable: unplugged (`link=false`), configured-but-not-up
/// (`link=true, connected=false`), OK (both true). `mode` comes from the bound
/// connection's `ipv4.method`; `error` carries the last `apply_lan_config`
/// failure (empty after a success).
pub fn get_lan_status() -> LanStatusResponse {
    match detect_lan_interface() {
        Some(iface) => {
            let link = read_link(&iface);
            let ip = crate::libs::network::status::get_interface_ip(&iface).unwrap_or_default();
            let connected = !ip.is_empty();
            let mode = find_eth_connection_name(&iface)
                .map(|c| read_connection_mode(&c))
                .unwrap_or_default();
            LanStatusResponse {
                connected,
                link,
                ip_address: ip,
                mac: read_mac(&iface),
                mode,
                error: last_error(),
            }
        }
        None => LanStatusResponse {
            error: last_error(),
            ..Default::default()
        },
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

    // Single test owns the LAST_LAN_ERROR global so it can't race other tests.
    #[test]
    fn last_error_surfaces_in_status_then_clears() {
        // A recorded failure is observable on a subsequent FB0C read…
        set_last_error(Some("gateway_unreachable".to_string()));
        assert_eq!(last_error(), "gateway_unreachable");
        assert_eq!(get_lan_status().error, "gateway_unreachable");
        // …and a success clears it (no stale error leaks to the next read).
        set_last_error(None);
        assert_eq!(last_error(), "");
        assert_eq!(get_lan_status().error, "");
    }
}
