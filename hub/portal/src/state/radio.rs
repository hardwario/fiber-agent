//! The portal's deserialization mirror of `radio/gateways` (devices.hub.md §12 A2).
//!
//! `proximos-control` publishes the report retained on the fixed topic
//! `control_radio::northbound::GATEWAYS_TOPIC` every 60 s. Its own types
//! (`control_radio::lorawan::gateway::{GatewaysReport, GatewayStatus, ApiStatus}`) are
//! `Serialize` only and carry `&'static str` fields, so they cannot be read back — hence
//! this mirror. It is pinned to the producer by the round-trip test at the bottom of this
//! file: the producer's own struct is serialized and parsed back into these types, field by
//! field, so a shape change fails the build rather than silently emptying the card.

use serde::Deserialize;

/// One `radio/gateways` row — a gateway the cluster's ChirpStack knows about.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct GatewayStatus {
    /// Lowercased by the store on ingest, so it joins against
    /// `DeviceState::lorawan.gateway_eui` (which the store lowercases too).
    pub gateway_eui: String,
    pub name: String,
    /// The Hub this gateway belongs to, when the producer knows it. The topic itself carries
    /// no hostname level, so this is the only per-row Hub attribution there is.
    pub hostname: Option<String>,
    pub ip: Option<String>,
    /// `"online"` / `"offline"` as the producer computed it at publish time. Stale between
    /// publishes, so the renderer recomputes with [`GatewayStatus::is_online`].
    pub status: String,
    pub last_seen: Option<String>,
    pub rssi: Option<i32>,
    pub snr: Option<f64>,
    /// `"lorawan"` / `"p2p"`.
    pub radio: String,
}

impl GatewayStatus {
    /// Online/offline on the producer's own 300 s window, recomputed at render time.
    ///
    /// Calls `control_radio`'s `is_online` rather than reimplementing the window: the report
    /// is published every 60 s, so `status` above ages between publishes, and two different
    /// windows in two crates would disagree on exactly the rows an operator is watching.
    pub fn is_online(&self, now_unix: i64) -> bool {
        control_radio::lorawan::gateway::is_online(self.last_seen.as_deref(), now_unix)
    }
}

/// Whether the management API itself answered, kept separate from the gateway rows: a
/// rejected login is not a site-wide radio outage, and the card says which one it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiStatus {
    Ok,
    AuthFailed,
    Unreachable,
}

/// The whole `radio/gateways` payload. Cluster-wide: one broker, one report, rows attributed
/// by their own `hostname`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct GatewaysReport {
    pub timestamp: String,
    pub api_status: ApiStatus,
    /// The producer omits an empty list (`skip_serializing_if`), so this defaults rather than
    /// failing the parse.
    #[serde(default)]
    pub gateways: Vec<GatewayStatus>,
}

/// What [`crate::state::PortalState`] holds: the last report plus the store-clock second it
/// arrived, so the card can say how old the radio health picture is.
#[derive(Debug, Clone, PartialEq)]
pub struct RadioGatewaysState {
    pub report: GatewaysReport,
    pub received_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use control_radio::lorawan::gateway as producer;

    /// The mirror is pinned to the producer's wire shape, not to a hand-written fixture.
    #[test]
    fn the_producers_report_round_trips_into_the_mirror() {
        let report = producer::GatewaysReport {
            timestamp: "2026-09-15T10:00:00Z".to_string(),
            api_status: producer::ApiStatus::Ok,
            gateways: vec![
                producer::GatewayStatus {
                    gateway_eui: "70b3d54900aa0001".to_string(),
                    name: "Local concentrator".to_string(),
                    hostname: Some("FIBER-001".to_string()),
                    ip: Some("192.168.1.50".to_string()),
                    status: "online",
                    last_seen: Some("2026-09-15T09:59:41Z".to_string()),
                    rssi: Some(-67),
                    snr: Some(9.5),
                    radio: "lorawan",
                },
                // The all-optional-absent row: every `skip_serializing_if` field missing.
                producer::GatewayStatus {
                    gateway_eui: "70b3d54900aa0002".to_string(),
                    name: "Client gateway".to_string(),
                    hostname: None,
                    ip: None,
                    status: "offline",
                    last_seen: None,
                    rssi: None,
                    snr: None,
                    radio: "p2p",
                },
            ],
        };

        let bytes = serde_json::to_vec(&report).expect("producer serializes");
        let mirror: GatewaysReport = serde_json::from_slice(&bytes).expect("mirror parses");

        assert_eq!(mirror.timestamp, report.timestamp);
        assert_eq!(mirror.api_status, ApiStatus::Ok);
        assert_eq!(mirror.gateways.len(), 2);

        let first = &mirror.gateways[0];
        assert_eq!(first.gateway_eui, "70b3d54900aa0001");
        assert_eq!(first.name, "Local concentrator");
        assert_eq!(first.hostname.as_deref(), Some("FIBER-001"));
        assert_eq!(first.ip.as_deref(), Some("192.168.1.50"));
        assert_eq!(first.status, "online");
        assert_eq!(first.last_seen.as_deref(), Some("2026-09-15T09:59:41Z"));
        assert_eq!(first.rssi, Some(-67));
        assert_eq!(first.snr, Some(9.5));
        assert_eq!(first.radio, "lorawan");

        let second = &mirror.gateways[1];
        assert_eq!(second.gateway_eui, "70b3d54900aa0002");
        assert_eq!(second.name, "Client gateway");
        assert_eq!(second.hostname, None);
        assert_eq!(second.ip, None);
        assert_eq!(second.status, "offline");
        assert_eq!(second.last_seen, None);
        assert_eq!(second.rssi, None);
        assert_eq!(second.snr, None);
        assert_eq!(second.radio, "p2p");
    }

    /// Each `api_status` variant, one by one: the three spellings are what the card's banner
    /// branches on, and a renamed one would otherwise fail the whole parse at runtime.
    #[test]
    fn every_api_status_variant_maps_one_to_one() {
        for (produced, expected) in [
            (producer::ApiStatus::Ok, ApiStatus::Ok),
            (producer::ApiStatus::AuthFailed, ApiStatus::AuthFailed),
            (producer::ApiStatus::Unreachable, ApiStatus::Unreachable),
        ] {
            let bytes = serde_json::to_vec(&produced).expect("producer serializes");
            let mirror: ApiStatus = serde_json::from_slice(&bytes).expect("mirror parses");
            assert_eq!(mirror, expected, "{produced:?}");
        }
    }

    /// The 300 s window comes from the producer, so this holds the delegation, not the maths.
    #[test]
    fn online_is_the_producers_300_s_window() {
        let now = 1_800_000_000i64;
        let row = |seen_at: i64| GatewayStatus {
            gateway_eui: "70b3d54900aa0001".into(),
            name: "Local".into(),
            hostname: None,
            ip: None,
            // Deliberately the opposite of what the window says: the renderer recomputes.
            status: "offline".into(),
            last_seen: Some(producer::format_rfc3339(seen_at)),
            rssi: None,
            snr: None,
            radio: "lorawan".into(),
        };

        assert!(row(now - 299).is_online(now), "inside the window");
        assert!(!row(now - 301).is_online(now), "outside the window");
        assert!(
            !GatewayStatus {
                last_seen: None,
                ..row(now)
            }
            .is_online(now),
            "never seen is offline"
        );
    }
}
