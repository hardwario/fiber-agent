//! LoRa time on air, and the modem's TX start learned from it.
//!
//! The central aims every answer at the node's RX1 window from the northbridge's own `RxDone`
//! stamp. What reaches the air is later than the aimed `t_ms` by the modem's RX -> TX turnaround
//! (22-23 ms on the bench of 2026-09-26, at every spreading factor), and a different northbridge
//! firmware can change that. `EVT_TX_DONE` reports when the transmission *ended*, so with the
//! frame's time on air the central sees when it actually started, and [`TxLead`] keeps an
//! estimate that the next aim takes off (p2p_control_runtime.md, amendment 2026-09-26).

use super::modem::RadioConfig;

/// Time on air of one LoRa frame in milliseconds (Semtech AN1200.13): explicit header, CRC on,
/// low-data-rate optimisation when a symbol lasts 16 ms or more — what the Zephyr `sx12xx`
/// driver on both the northbridge and the STICKER transmits.
pub fn lora_airtime_ms(payload_len: usize, radio: &RadioConfig) -> f64 {
    let bw_hz: f64 = match radio.bw {
        1 => 250_000.0,
        2 => 500_000.0,
        _ => 125_000.0,
    };
    let sf = f64::from(radio.sf);
    let symbol_ms = 2f64.powf(sf) / bw_hz * 1000.0;
    let ldro = if symbol_ms >= 16.0 { 1.0 } else { 0.0 };
    let cr = f64::from(radio.cr.clamp(1, 4));
    let bits = 8.0 * payload_len as f64 - 4.0 * sf + 28.0 + 16.0;
    let payload_symbols = 8.0 + ((bits / (4.0 * (sf - 2.0 * ldro))).ceil() * (cr + 4.0)).max(0.0);
    (f64::from(radio.preamble) + 4.25 + payload_symbols) * symbol_ms
}

/// The modem's TX start after the aimed `t_ms` before any `EVT_TX_DONE` was seen, when no
/// learned value was saved: the bench value of northbridge 0.2.1 (2026-09-27, measured -2.7 ms).
/// It was 22 ms on the firmware of 2026-09-26; with that seed the first ACK after every restart
/// went out ~25 ms early and was lost (bench c55, 0413 c1024).
pub const TX_LEAD_SEED_MS: f64 = -3.0;

/// How far the estimate may drift from the saved one before it is saved again: a flash write
/// per ACK would be pointless, and one millisecond is well inside the RX1 margin.
pub const TX_LEAD_SAVE_DELTA_MS: f64 = 1.0;

/// Weight of one new measurement. A quarter follows a changed firmware within a few frames and
/// does not chase the ±1 ms of jitter.
const TX_LEAD_WEIGHT: f64 = 0.25;

/// Measurements outside this range are not a turnaround: a late `EVT_TX_DONE` of another
/// frame, a clock step, or an airtime this module computed for the wrong parameters.
const TX_LEAD_PLAUSIBLE_MS: std::ops::RangeInclusive<f64> = -20.0..=120.0;

/// A measurement this far from the estimate is logged, so a firmware whose timing moved is
/// visible in the journal rather than only in a shifted ACK.
pub const TX_LEAD_NOTICE_MS: f64 = 5.0;

/// The learned TX start of the modem, relative to the aimed `t_ms`.
#[derive(Debug, Clone, PartialEq)]
pub struct TxLead {
    estimate_ms: f64,
    samples: u64,
}

impl Default for TxLead {
    fn default() -> Self {
        Self {
            estimate_ms: TX_LEAD_SEED_MS,
            samples: 0,
        }
    }
}

impl TxLead {
    /// Start from a value learned before a restart. It counts as no sample, so the first
    /// measurement still replaces it outright.
    pub fn seeded(estimate_ms: f64) -> Self {
        let estimate_ms = if TX_LEAD_PLAUSIBLE_MS.contains(&estimate_ms) {
            estimate_ms
        } else {
            TX_LEAD_SEED_MS
        };
        Self {
            estimate_ms,
            samples: 0,
        }
    }

    pub fn estimate_ms(&self) -> f64 {
        self.estimate_ms
    }

    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// One transmission reported done: aimed at `aim_t_ms`, ended at `t_actual_ms`, on air for
    /// `airtime_ms`. Returns the measured lead when it was plausible and taken in.
    pub fn observe(&mut self, aim_t_ms: u64, t_actual_ms: u64, airtime_ms: f64) -> Option<f64> {
        let measured = t_actual_ms as f64 - airtime_ms - aim_t_ms as f64;
        if !TX_LEAD_PLAUSIBLE_MS.contains(&measured) {
            return None;
        }
        self.estimate_ms = if self.samples == 0 {
            measured
        } else {
            self.estimate_ms + (measured - self.estimate_ms) * TX_LEAD_WEIGHT
        };
        self.samples += 1;
        Some(measured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn radio(sf: u8) -> RadioConfig {
        RadioConfig {
            freq_hz: 868_100_000,
            sf,
            bw: 0,
            cr: 1,
            preamble: 8,
            tx_power_dbm: 14,
            flags: 0,
        }
    }

    /// The ACK the central sends is 22 B on air. The bench saw `actual - aim` of 79/126/228/393
    /// ms at SF7/8/9/10, i.e. these airtimes plus the same 22-23 ms of turnaround.
    #[test]
    fn airtime_matches_the_bench() {
        let ack = |sf| lora_airtime_ms(22, &radio(sf)).round() as i64;
        assert_eq!(
            [ack(7), ack(8), ack(9), ack(10), ack(11), ack(12)],
            [57, 103, 206, 371, 741, 1483]
        );
        // The telemetry uplink (27 B) and the alarm (33 B) at SF10, as the STICKER logs them.
        assert_eq!(lora_airtime_ms(27, &radio(10)).round() as i64, 412);
        assert_eq!(lora_airtime_ms(33, &radio(10)).round() as i64, 453);
    }

    #[test]
    fn the_lead_is_learned_from_tx_done_and_implausible_ones_are_ignored() {
        let mut lead = TxLead::default();
        assert_eq!(lead.estimate_ms(), TX_LEAD_SEED_MS);

        // The first measurement replaces the seed, later ones move it by a quarter.
        let airtime = lora_airtime_ms(22, &radio(7));
        assert_eq!(
            lead.observe(1000, 1000 + 2 + airtime.round() as u64, airtime)
                .map(f64::round),
            Some(2.0)
        );
        assert!((lead.estimate_ms() - 2.0).abs() < 1.0);
        lead.observe(5000, 5000 + 10 + airtime.round() as u64, airtime);
        assert!(lead.estimate_ms() > 2.0 && lead.estimate_ms() < 10.0);

        let before = lead.clone();
        assert_eq!(
            lead.observe(9000, 9000 + 5000, airtime),
            None,
            "another frame's TX_DONE"
        );
        assert_eq!(lead.observe(9000, 8000, airtime), None, "a clock step");
        assert_eq!(lead, before);
    }
}
