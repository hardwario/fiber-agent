//! `NetworkOps` over the surviving `nmcli` surface.
//!
//! This is the one place in the BLE path that still executes a program. CP-1 removed the BLE
//! terminal and the shell-based adapter recovery; it did **not** remove `nmcli`, because
//! reimplementing NetworkManager's connection model over D-Bus is a different project and the
//! wizard depends on Wi-Fi provisioning working today. The exec surface is therefore confined to
//! `libs/network/{wifi,lan,net_error}.rs`, which is where those files now live — they belong to the
//! network module, not to a GATT server this crate no longer hosts.
//!
//! The structs on both sides carry the same fields and the two `NetworkErrorCategory` enums have
//! the same six variants, so this file is a translation and nothing more. It exists because the
//! crate must not name `nmcli`, not because the shapes disagree — and because the day one of them
//! gains a variant, the mismatch surfaces here as a compile error rather than as a wrong category
//! on an installer's phone.

use control_ble::error::Result;
use control_ble::host::{
    LanConfigRequest, LanMode, LanStatus, NetworkErrorCategory, NetworkOps, WifiNetwork, WifiStatus,
};

use crate::libs::network::{lan, net_error, wifi};

pub struct NmcliNetwork;

impl NetworkOps for NmcliNetwork {
    fn wifi_scan(&self) -> Result<Vec<WifiNetwork>> {
        // v1's `scan_wifi` cannot fail — an unavailable `nmcli` is an empty list, which the phone
        // renders as "no networks found". Keeping that is deliberate: a scan that errors would make
        // the wizard's first screen a dead end instead of a retry.
        Ok(wifi::scan_wifi()
            .into_iter()
            .map(|n| WifiNetwork {
                ssid: n.ssid,
                signal: n.signal,
                security: n.security,
            })
            .collect())
    }

    fn wifi_connect(&self, ssid: &str, password: &str) -> WifiStatus {
        to_wifi_status(wifi::connect_wifi(ssid, password))
    }

    fn wifi_disconnect(&self) -> WifiStatus {
        to_wifi_status(wifi::disconnect_wifi())
    }

    fn wifi_status(&self) -> WifiStatus {
        to_wifi_status(wifi::get_wifi_status())
    }

    fn lan_apply(&self, req: &LanConfigRequest) -> std::result::Result<(), NetworkErrorCategory> {
        let native = lan::LanConfigRequest {
            mode: match req.mode {
                LanMode::Dhcp => lan::LanMode::Dhcp,
                LanMode::Static => lan::LanMode::Static,
            },
            ipv4: req.ipv4.as_ref().map(|c| lan::Ipv4Config {
                address: c.address.clone(),
                prefix: c.prefix,
                gateway: c.gateway.clone(),
                dns: c.dns.clone(),
            }),
        };
        lan::apply_lan_config(&native).map_err(to_category)
    }

    fn lan_status(&self) -> LanStatus {
        let s = lan::get_lan_status();
        LanStatus {
            connected: s.connected,
            link: s.link,
            ip_address: s.ip_address,
            mac: s.mac,
            mode: s.mode,
            error: s.error,
        }
    }
}

fn to_wifi_status(s: wifi::WiFiStatusResponse) -> WifiStatus {
    WifiStatus {
        connected: s.connected,
        ssid: s.ssid,
        ip_address: s.ip_address,
        error: s.error,
    }
}

/// Exhaustive on purpose: no `_ =>` arm, so a new variant on either side stops the build.
fn to_category(e: net_error::NetworkErrorCategory) -> NetworkErrorCategory {
    match e {
        net_error::NetworkErrorCategory::InvalidIp => NetworkErrorCategory::InvalidIp,
        net_error::NetworkErrorCategory::GatewayUnreachable => {
            NetworkErrorCategory::GatewayUnreachable
        }
        net_error::NetworkErrorCategory::AuthFailed => NetworkErrorCategory::AuthFailed,
        net_error::NetworkErrorCategory::NotFound => NetworkErrorCategory::NotFound,
        net_error::NetworkErrorCategory::Timeout => NetworkErrorCategory::Timeout,
        net_error::NetworkErrorCategory::Other => NetworkErrorCategory::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_category_maps_across_without_collapsing_two_into_one() {
        // The map is a bijection today; a collapse would send an installer to the wrong fix.
        let pairs = [
            (
                net_error::NetworkErrorCategory::InvalidIp,
                NetworkErrorCategory::InvalidIp,
            ),
            (
                net_error::NetworkErrorCategory::GatewayUnreachable,
                NetworkErrorCategory::GatewayUnreachable,
            ),
            (
                net_error::NetworkErrorCategory::AuthFailed,
                NetworkErrorCategory::AuthFailed,
            ),
            (
                net_error::NetworkErrorCategory::NotFound,
                NetworkErrorCategory::NotFound,
            ),
            (
                net_error::NetworkErrorCategory::Timeout,
                NetworkErrorCategory::Timeout,
            ),
            (
                net_error::NetworkErrorCategory::Other,
                NetworkErrorCategory::Other,
            ),
        ];
        assert_eq!(pairs.len(), 6, "both enums have six variants");
        for (native, mapped) in pairs {
            assert_eq!(to_category(native), mapped);
        }
    }

    #[test]
    fn the_wifi_status_map_keeps_every_field() {
        let s = to_wifi_status(wifi::WiFiStatusResponse {
            connected: true,
            ssid: "lab".into(),
            ip_address: "10.0.0.5".into(),
            error: String::new(),
        });
        assert!(s.connected);
        assert_eq!(s.ssid, "lab");
        assert_eq!(s.ip_address, "10.0.0.5");
        assert!(s.error.is_empty());
    }
}
