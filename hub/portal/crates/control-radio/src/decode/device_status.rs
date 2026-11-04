//! The `device_status` document: what a Node said about its battery and its radio link when
//! asked (p2p_control_commands.md §5).
//!
//! One shape for both carriers (decision #23). P2P fills it from a `0x91` `DevStatusAns`; a
//! LoRaWAN feed (ChirpStack's `status` event) is a follow-up and fills the same fields it has.

use serde::Serialize;

/// The battery range the percentage maps linearly, in mV (#487: "LoRaWAN maps 2.4–3.6 V").
pub const BATTERY_EMPTY_MV: u16 = 2400;
pub const BATTERY_FULL_MV: u16 = 3600;

/// `battery_mv` as a percentage of [`BATTERY_EMPTY_MV`]..=[`BATTERY_FULL_MV`], clamped and
/// rounded.
pub fn battery_pct(mv: u16) -> u8 {
    let span = f64::from(BATTERY_FULL_MV - BATTERY_EMPTY_MV);
    let pct = (f64::from(mv) - f64::from(BATTERY_EMPTY_MV)) / span * 100.0;
    pct.round().clamp(0.0, 100.0) as u8
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceStatusDocument {
    pub dev_eui: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial_number: Option<String>,
    pub transport: &'static str,
    pub received_at: String,
    /// `None` when the Node has no battery sample yet.
    pub battery_mv: Option<u16>,
    pub battery_pct: Option<u8>,
    /// The Node's reception of the downlink that carried the request.
    pub dl_rssi_dbm: Option<i16>,
    pub dl_snr_db: Option<i16>,
    pub tx_power_dbm: Option<i16>,
    pub uptime_s: Option<u32>,
}

impl DeviceStatusDocument {
    /// From a P2P `DevStatusAns`.
    pub fn from_p2p(
        dev_eui: &str,
        serial: Option<&str>,
        received_at: &str,
        ans: &crate::p2p::tower::control::DevStatusAns,
    ) -> Self {
        let battery_mv = (ans.battery_mv != 0).then_some(ans.battery_mv);
        DeviceStatusDocument {
            dev_eui: dev_eui.to_string(),
            serial_number: serial.filter(|s| !s.is_empty()).map(str::to_string),
            transport: crate::error::Transport::P2p.as_str(),
            received_at: received_at.to_string(),
            battery_mv,
            battery_pct: battery_mv.map(battery_pct),
            dl_rssi_dbm: Some(i16::from(ans.dl_rssi)),
            dl_snr_db: Some(i16::from(ans.dl_snr)),
            tx_power_dbm: Some(i16::from(ans.tx_power)),
            uptime_s: Some(ans.uptime_s),
        }
    }

    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::p2p::tower::control::DevStatusAns;

    #[test]
    fn the_battery_percentage_is_linear_over_2_4_to_3_6_v() {
        assert_eq!(battery_pct(2400), 0);
        assert_eq!(battery_pct(3000), 50);
        assert_eq!(battery_pct(3012), 51);
        assert_eq!(battery_pct(3600), 100);
        assert_eq!(battery_pct(2000), 0);
        assert_eq!(battery_pct(3700), 100);
    }

    #[test]
    fn a_dev_status_ans_becomes_the_document() {
        let ans = DevStatusAns {
            battery_mv: 3012,
            dl_rssi: -71,
            dl_snr: 9,
            tx_power: 14,
            uptime_s: 86_400,
        };
        let doc = DeviceStatusDocument::from_p2p(
            "5876070000000413",
            Some("80e007b2"),
            "2026-10-12T10:00:00Z",
            &ans,
        );
        assert_eq!(
            doc.to_value(),
            serde_json::json!({
                "dev_eui": "5876070000000413",
                "serial_number": "80e007b2",
                "transport": "p2p",
                "received_at": "2026-10-12T10:00:00Z",
                "battery_mv": 3012,
                "battery_pct": 51,
                "dl_rssi_dbm": -71,
                "dl_snr_db": 9,
                "tx_power_dbm": 14,
                "uptime_s": 86400
            })
        );
    }

    #[test]
    fn no_battery_sample_publishes_null() {
        let ans = DevStatusAns {
            battery_mv: 0,
            dl_rssi: -100,
            dl_snr: -5,
            tx_power: 14,
            uptime_s: 3,
        };
        let v = DeviceStatusDocument::from_p2p("5876070000000413", None, "t", &ans).to_value();
        assert!(v["battery_mv"].is_null());
        assert!(v["battery_pct"].is_null());
        assert!(v.get("serial_number").is_none());
    }
}
