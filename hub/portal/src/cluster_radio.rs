//! A Client Hub runs the Server's radio mode (decision #47, `plan/control/cluster.md`
//! amendments 2026-10-29, #174).
//!
//! The Server's agent publishes its mode to every Client, retained, and each Client's agent
//! applies it (decision #48, `control-core` `cluster_radio`). The portal only shows it: the
//! Cluster card flags a Client whose last report differs until it converges.

use crate::state::DeviceState;

/// Whether the Cluster card flags the Client: it reported a mode other than the Server's, online
/// or not (its last report).
pub fn differs(server: Option<&str>, device: Option<&DeviceState>) -> bool {
    let reported = device
        .and_then(|d| d.lorawan.as_ref())
        .and_then(|l| l.mode.as_deref());
    matches!((server, reported), (Some(s), Some(r)) if !r.trim().eq_ignore_ascii_case(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fiber_protocol::payloads::system_info::LorawanGatewayInfo;

    fn device(online: bool, mode: Option<&str>) -> DeviceState {
        let mut d = DeviceState::new("proximos-c23e465c".into());
        d.online = online;
        d.lorawan = Some(LorawanGatewayInfo {
            mode: mode.map(str::to_string),
            ..Default::default()
        });
        d
    }

    /// The card flags the last report, online or not.
    #[test]
    fn the_card_flags_a_reported_mode_that_differs() {
        assert!(differs(Some("lorawan"), Some(&device(false, Some("p2p")))));
        assert!(!differs(Some("dual"), Some(&device(true, Some("dual")))));
        assert!(!differs(Some("lorawan"), Some(&device(true, None))));
        assert!(!differs(None, Some(&device(true, Some("p2p")))));
    }
}
