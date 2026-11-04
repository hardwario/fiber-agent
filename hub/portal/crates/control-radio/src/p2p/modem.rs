//! The host <-> northbridge protocol — `plan/system/lora/design.md` §3, §4.
//!
//! The modem is **dumb, keyless and stateless**: it holds the current radio config, an
//! RX-active flag and one pending scheduled-TX slot, all volatile and all re-pushed by the
//! host after a reboot. No keys, no addressing, no `net_id` filtering — it reports every frame
//! that passes the radio CRC, and filtering, dedup and crypto all live here in the central.
//!
//! **There is no clock synchronization.** Every timestamp is the modem's own monotonic
//! millisecond clock, and the central computes `tx_at = rx_timestamp + rx1_delay - lead`
//! entirely in that clock domain, from values the modem itself reported. UART and LAN latency
//! only have to be smaller than the lead; they are never measured or compensated. That was
//! proven on the bench, not just designed.

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::p2p::tower::link::{self as twr_link, evt as twr};

/// Command types, host -> modem. Each is answered by an `RSP` of type `0x40 | cmd`.
pub mod cmd {
    pub const GET_INFO: u8 = 0x01;
    pub const SET_RADIO_CONFIG: u8 = 0x02;
    // 0x03 was RX_START; `TWR_START` enters gateway receive (p2p_tower_gateway.md §2.1).
    pub const RX_STOP: u8 = 0x04;
    pub const TX_SCHEDULE: u8 = 0x05;
    pub const TX_CANCEL: u8 = 0x06;
    /// Reboot into the ROM bootloader through the boot stub; the body is exactly "NBBL"
    /// (`northbridge_update.md` §10). Driven only by [`crate::p2p::nb_flash`].
    pub const ENTER_BOOTLOADER: u8 = 0x07;
}

/// A response carries the command type with this bit set.
pub const RSP_FLAG: u8 = 0x40;

/// Event types, modem -> host, unsolicited.
pub mod evt {
    pub const BOOT: u8 = 0x80;
    pub const RX: u8 = 0x81;
    pub const TX_DONE: u8 = 0x82;
    pub const LOG: u8 = 0x83;
}

/// `RSP` status byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    BadParam,
    /// The single TX slot is occupied. Never queued or reordered.
    Busy,
    InvalidState,
    /// `t_ms` is in the past, or closer than `TX_MIN_LEAD`.
    Late,
    Internal,
    /// The gateway's peer table or downlink queue is full.
    NoSpace,
    /// The gateway has no such peer, or no such queued item.
    NotFound,
    /// The command is not one this image implements (`0x03`, `0x16`, `0x18` on 0.3.0).
    Unsupported,
    Unknown(u8),
}

impl Status {
    pub fn from_byte(b: u8) -> Self {
        match b {
            0 => Status::Ok,
            1 => Status::BadParam,
            2 => Status::Busy,
            3 => Status::InvalidState,
            4 => Status::Late,
            5 => Status::Internal,
            6 => Status::NoSpace,
            7 => Status::NotFound,
            8 => Status::Unsupported,
            other => Status::Unknown(other),
        }
    }

    pub fn into_result(self, doing: &str) -> Result<()> {
        let kind = match self {
            Status::Ok => return Ok(()),
            Status::BadParam => Kind::InvalidArgument,
            // BUSY and INVALID_STATE are transient: the slot frees, the state changes.
            Status::Busy | Status::InvalidState => Kind::AlreadyExists,
            // A missed radio deadline is a Timeout, not a transport fault — the command was
            // accepted and the radio operation simply did not make its window.
            Status::Late => Kind::Timeout,
            Status::NotFound => Kind::NotFound,
            // A full table is the gateway's capacity, not a caller error; nothing the central
            // sends differently would change it.
            // An unsupported command means the images do not match: nothing to retry.
            Status::Internal | Status::NoSpace | Status::Unsupported | Status::Unknown(_) => {
                Kind::Transport
            }
        };
        Err(Error::new(
            kind,
            Transport::P2p,
            Component::NorthbridgeFirmware,
            format!("{doing}: modem answered {self:?}"),
        ))
    }
}

/// Did the modem answer `BUSY`? [`Status::into_result`] folds `BUSY` and `INVALID_STATE`
/// into one kind; the one caller that must tell them apart (`TWR_QUEUE_PUSH` of an item
/// already on air) reads the status the message names.
pub fn is_busy(e: &Error) -> bool {
    e.kind() == Kind::AlreadyExists && e.message().ends_with("modem answered Busy")
}

/// The TX powers the Northbridge's PA produces: RFO_LP with `rfo-lp-max-power = 15`. 0.3.0
/// refuses anything else with `BAD_PARAM` rather than clamping, so the central refuses first.
pub const NB_TX_POWER_DBM: std::ops::RangeInclusive<i8> = -16..=15;

/// The minimum lead a scheduled TX needs. Proposed 20 ms, to be HIL-tuned.
///
/// A `t_ms` closer than this is rejected `LATE` in the response, so the central always learns
/// an ACK was not sent — never a silent drop.
pub const TX_MIN_LEAD_MS: u64 = 20;

/// The spreading factors the modem accepts. Named rather than repeated as a literal because
/// three layers check it — this one, `radio-set`'s argument validation, and the config schema —
/// and a drift between them would let a nonsense register write reach the northbridge.
pub const SF_MIN: u8 = 7;
pub const SF_MAX: u8 = 12;

/// `RadioConfig::flags` bit 0: the public LoRa sync word (`0x34`) instead of the private one.
pub const SYNC_WORD_PUBLIC_FLAG: u8 = 0x01;

/// Radio parameters pushed to the modem.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RadioConfig {
    pub freq_hz: u32,
    pub sf: u8,
    /// 0 = 125 kHz, 1 = 250 kHz, 2 = 500 kHz.
    pub bw: u8,
    /// 1..=4, meaning 4/5..4/8.
    pub cr: u8,
    pub preamble: u16,
    pub tx_power_dbm: i8,
    /// b0 public syncword, b1 IQ inverted.
    pub flags: u8,
}

impl RadioConfig {
    /// Encode the `SET_RADIO_CONFIG` body. Little-endian, as the link protocol specifies.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(11);
        b.extend_from_slice(&self.freq_hz.to_le_bytes());
        b.push(self.sf);
        b.push(self.bw);
        b.push(self.cr);
        b.extend_from_slice(&self.preamble.to_le_bytes());
        b.push(self.tx_power_dbm as u8);
        b.push(self.flags);
        b
    }

    /// Map the Hub-wide `radio.p2p_radio` config onto the modem's encoding.
    pub fn from_hub_config(c: &crate::config::P2pRadio) -> Result<Self> {
        let bw = match c.bandwidth_khz {
            125 => 0,
            250 => 1,
            500 => 2,
            other => {
                return Err(Error::new(
                    Kind::InvalidArgument,
                    Transport::P2p,
                    Component::NorthbridgeFirmware,
                    format!("bandwidth {other} kHz is not one the modem encodes (125/250/500)"),
                ))
            }
        };
        let cr = match c.coding_rate.as_str() {
            "4/5" => 1,
            "4/6" => 2,
            "4/7" => 3,
            "4/8" => 4,
            other => {
                return Err(Error::new(
                    Kind::InvalidArgument,
                    Transport::P2p,
                    Component::NorthbridgeFirmware,
                    format!("coding rate {other:?} is not one of 4/5, 4/6, 4/7, 4/8"),
                ))
            }
        };
        // The config schema caps this too, but the modem layer must be safe standalone —
        // an out-of-range SF reaches the northbridge as a nonsense register write.
        check_sf(c.spreading_factor)?;
        let refuse = |why: String| {
            Error::new(
                Kind::InvalidArgument,
                Transport::P2p,
                Component::NorthbridgeFirmware,
                why,
            )
        };
        // p2p_tower_gateway.md §3.5 (amended 2026-09-28).
        if c.modulation != crate::config::P2pModulation::Lora {
            return Err(refuse(
                "modulation fsk is not supported until #470 P5; use lora".to_string(),
            ));
        }
        let flags = match c.sync_word {
            crate::config::SYNC_WORD_PRIVATE => 0,
            crate::config::SYNC_WORD_PUBLIC => SYNC_WORD_PUBLIC_FLAG,
            other => {
                return Err(refuse(format!(
                    "sync_word {other:#04x} is not one the modem encodes (0x12 private, 0x34 \
                     public)"
                )))
            }
        };
        if !NB_TX_POWER_DBM.contains(&c.tx_power) {
            return Err(refuse(format!(
                "tx_power {} dBm is outside the Northbridge's {}..={}",
                c.tx_power,
                NB_TX_POWER_DBM.start(),
                NB_TX_POWER_DBM.end()
            )));
        }
        Ok(RadioConfig {
            freq_hz: c.frequency_hz,
            sf: c.spreading_factor,
            bw,
            cr,
            preamble: c.preamble,
            tx_power_dbm: c.tx_power,
            // Private sync word by default: this is not a LoRaWAN network.
            flags,
        })
    }

    /// The same configuration with a different spreading factor — the one axis `radio-set` moves.
    ///
    /// Everything else is copied verbatim rather than re-derived, so the `SET_RADIO_CONFIG` the
    /// central pushes for a live SF change cannot quietly retune the channel, the bandwidth or
    /// the TX power along with it. The range is re-checked here because the value may have come
    /// from a hand-edited `sessions.db`, not from the validated CLI path.
    pub fn with_spreading_factor(&self, sf: u8) -> Result<Self> {
        check_sf(sf)?;
        Ok(RadioConfig { sf, ..*self })
    }
}

fn check_sf(sf: u8) -> Result<()> {
    if !(SF_MIN..=SF_MAX).contains(&sf) {
        return Err(Error::new(
            Kind::InvalidArgument,
            Transport::P2p,
            Component::NorthbridgeFirmware,
            format!("spreading factor {sf} is outside the {SF_MIN}..={SF_MAX} the modem accepts"),
        ));
    }
    Ok(())
}

/// A scheduled transmission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxSchedule {
    pub tx_id: u8,
    /// Absolute time in the modem's own clock. 0 means immediate.
    pub t_ms: u64,
    pub frame: Vec<u8>,
}

impl TxSchedule {
    /// Largest frame the one-byte length field can describe.
    pub const MAX_FRAME: usize = u8::MAX as usize;

    /// Encode for the wire.
    ///
    /// The length field is one byte while `hdlc` allows frames up to 280, so anything longer
    /// used to be truncated by `as u8` and handed to the northbridge as a shorter frame —
    /// silently, and with the tail read as the next message.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let len = u8::try_from(self.frame.len()).map_err(|_| {
            Error::new(
                Kind::InvalidArgument,
                Transport::P2p,
                Component::NorthbridgeFirmware,
                format!(
                    "a {} byte frame cannot be described by the schedule's one-byte length \
                     field (max {})",
                    self.frame.len(),
                    Self::MAX_FRAME
                ),
            )
        })?;
        let mut b = Vec::with_capacity(10 + self.frame.len());
        b.push(self.tx_id);
        b.extend_from_slice(&self.t_ms.to_le_bytes());
        b.push(len);
        b.extend_from_slice(&self.frame);
        Ok(b)
    }
}

/// A received frame, with the metadata the central needs to answer it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxEvent {
    /// Captured at the RxDone interrupt — the end of the frame on air, which is exactly the
    /// reference point the node's RX1 delay counts from.
    pub t_ms: u64,
    pub rssi_dbm: i16,
    /// Quarter-dB, sx126x native.
    pub snr_q_db: i8,
    pub frame: Vec<u8>,
}

impl RxEvent {
    pub fn decode(body: &[u8]) -> Option<Self> {
        // t_ms u64 | rssi i16 | snr i8 | reserved u8 | len u8 | frame[len]
        if body.len() < 13 {
            return None;
        }
        let t_ms = u64::from_le_bytes(body[0..8].try_into().ok()?);
        let rssi_dbm = i16::from_le_bytes(body[8..10].try_into().ok()?);
        let snr_q_db = body[10] as i8;
        let len = body[12] as usize;
        let frame = body.get(13..13 + len)?.to_vec();
        Some(RxEvent {
            t_ms,
            rssi_dbm,
            snr_q_db,
            frame,
        })
    }

    /// SNR in dB. The wire carries quarter-dB.
    pub fn snr_db(&self) -> f64 {
        self.snr_q_db as f64 / 4.0
    }
}

/// The outcome of a scheduled transmission. Every scheduled TX produces one — the modem never
/// silently drops one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxDone {
    pub tx_id: u8,
    pub status: TxStatus,
    pub t_actual_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxStatus {
    Ok,
    Late,
    RadioErr,
    Cancelled,
    Unknown(u8),
}

impl TxDone {
    pub fn decode(body: &[u8]) -> Option<Self> {
        if body.len() < 10 {
            return None;
        }
        Some(TxDone {
            tx_id: body[0],
            status: match body[1] {
                0 => TxStatus::Ok,
                1 => TxStatus::Late,
                2 => TxStatus::RadioErr,
                3 => TxStatus::Cancelled,
                o => TxStatus::Unknown(o),
            },
            t_actual_ms: u64::from_le_bytes(body[2..10].try_into().ok()?),
        })
    }
}

/// The modem rebooted and lost all state — radio config, peer table, keys, counter block,
/// queue; the host must resync (`p2p_tower_gateway.md` §3.4). 8 B, frozen by amendment 9.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootEvent {
    pub fw_ver: [u8; 3],
    /// The first 4 bytes of the image's commit, in display order.
    pub git_sha: [u8; 4],
    pub reset_cause: u8,
}

impl BootEvent {
    pub const LEN: usize = 8;

    pub fn decode(body: &[u8]) -> Option<Self> {
        if body.len() < Self::LEN {
            return None;
        }
        Some(BootEvent {
            fw_ver: [body[0], body[1], body[2]],
            git_sha: [body[3], body[4], body[5], body[6]],
            reset_cause: body[7],
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(Self::LEN);
        b.extend_from_slice(&self.fw_ver);
        b.extend_from_slice(&self.git_sha);
        b.push(self.reset_cause);
        b
    }
}

/// `EVT_LOG` level of an error (`plan/control/radio/northbridge_diagnostics.md` §2.2).
pub const FAULT_LEVEL_ERROR: u8 = 1;
/// `EVT_LOG` level of a warning.
pub const FAULT_LEVEL_WARNING: u8 = 2;
/// `EVT_LOG` level of a debug build's free-form line.
pub const FAULT_LEVEL_DEBUG: u8 = 3;

/// A structured `EVT_LOG` (§2.2): `level u8, code u8`, then ASCII detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultEvent {
    pub level: u8,
    pub code: u8,
    pub text: String,
}

/// Anything the modem sent unprompted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Boot(BootEvent),
    Rx(RxEvent),
    TxDone(TxDone),
    /// A free-form line: a debug build, or firmware older than §2.2.
    Log(String),
    /// An error or warning the modem reports (§2.2).
    Fault(FaultEvent),
    /// A TOWER gateway's fresh, verified uplink (`p2p_tower_gateway.md` §2.2). A ring event:
    /// `seq` is the frame's delivery sequence number (link doc §4.1), which the central dedups
    /// by and acknowledges with `TWR_EVT_ACK`.
    TwrUplink {
        seq: u8,
        up: crate::p2p::tower::link::TwrUplink,
    },
    /// A TOWER gateway's downlink outcome. A ring event, like [`Event::TwrUplink`].
    TwrTx {
        seq: u8,
        tx: crate::p2p::tower::link::TwrTx,
    },
    /// A TOWER gateway's counter block is running low.
    TwrCtrLow(crate::p2p::tower::link::TwrCtrLow),
}

/// Decode an `EVT_LOG` body. §2.2 prefixes `level` and `code`; a body whose first byte is
/// printable ASCII is the older free-form line and stays one, so neither firmware breaks the
/// other. A structured debug-level line is shown as the line it carries.
fn decode_log(body: &[u8]) -> Event {
    match body {
        [level @ (FAULT_LEVEL_ERROR | FAULT_LEVEL_WARNING), code, rest @ ..] => {
            Event::Fault(FaultEvent {
                level: *level,
                code: *code,
                text: String::from_utf8_lossy(rest).into_owned(),
            })
        }
        [FAULT_LEVEL_DEBUG, _code, rest @ ..] => {
            Event::Log(String::from_utf8_lossy(rest).into_owned())
        }
        _ => Event::Log(String::from_utf8_lossy(body).into_owned()),
    }
}

impl Event {
    /// Decode an unsolicited frame; an unknown type or a truncated body is `None`.
    pub fn from_frame(f: &crate::p2p::hdlc::Frame) -> Option<Self> {
        match f.frame_type {
            evt::BOOT => BootEvent::decode(&f.body).map(Event::Boot),
            evt::RX => RxEvent::decode(&f.body).map(Event::Rx),
            evt::TX_DONE => TxDone::decode(&f.body).map(Event::TxDone),
            evt::LOG => Some(decode_log(&f.body)),
            twr::TWR_UPLINK => {
                twr_link::TwrUplink::decode(&f.body).map(|up| Event::TwrUplink { seq: f.seq, up })
            }
            twr::TWR_TX => {
                twr_link::TwrTx::decode(&f.body).map(|tx| Event::TwrTx { seq: f.seq, tx })
            }
            twr::TWR_CTR_LOW => twr_link::TwrCtrLow::decode(&f.body).map(Event::TwrCtrLow),
            _ => None,
        }
    }
}

/// Where an answer should **start** on air: this many milliseconds after the nominal RX1, which
/// is `RxDone + rx1_delay` as the modem stamps `RxDone`.
///
/// The central used to add a fixed margin to the aimed `t_ms` (+30 ms). The bench of 2026-09-26
/// (p2p_control_runtime.md, M6 amendment) showed what that margin was really absorbing:
///
/// - The modem starts the TX 22-23 ms after `t_ms`. [`super::airtime::TxLead`] now measures
///   that and takes it off the aim.
/// - The modem stamps `RxDone` ~14-26 ms late, depending on the SF. The central cannot see that.
/// - The STICKER listens from ~+997 ms (its window opens at +975 plus ~22 ms of setup). It
///   closes after 12 symbols + the frame's airtime + 40 ms, so at SF7 by +1111 ms.
///
/// With +30 the ACK ended after that close at SF7. With a margin of −50 it started before the
/// Node listened. Starting 12 ms after the nominal RX1 puts it at +1026..+1083 at SF7 and
/// leaves ~100 ms at SF10. It also stays inside the window if the modem's `RxDone` stamp gets
/// exact, which moves the ACK ~14 ms earlier. A Node whose window keeps a wider trailing slack
/// (sticker F-P2P-2) gains the rest.
pub const RX1_ACK_START_MS: i64 = 12;
// The Node listens only after its ~22 ms of setup past the window's opening at -25 ms, so an
// answer that starts before the nominal RX1 loses its preamble at SF7.
const _: () = assert!(RX1_ACK_START_MS > 0);

/// Overrides [`RX1_ACK_START_MS`] for HIL sweeps: signed milliseconds, read once. Unset,
/// unparsable or outside ±500 ms keeps the constant.
pub const RX1_ACK_START_ENV: &str = "PROXIMOS_P2P_RX1_ACK_START_MS";

/// The start offset in force: [`RX1_ACK_START_ENV`] if set, else [`RX1_ACK_START_MS`].
pub fn rx1_ack_start_ms() -> i64 {
    static OFFSET: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *OFFSET.get_or_init(|| {
        std::env::var(RX1_ACK_START_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|m| (-500..=500).contains(m))
            .unwrap_or(RX1_ACK_START_MS)
    })
}

/// Compute when to transmit an answer, in the modem's own clock domain: the aimed `t_ms` for a
/// start [`rx1_ack_start_ms`] after the nominal RX1, given that the modem starts `tx_lead_ms`
/// after `t_ms`.
pub fn schedule_at(rx_t_ms: u64, rx1_delay_s: u64, tx_lead_ms: f64) -> u64 {
    let offset = rx1_ack_start_ms() - tx_lead_ms.round() as i64;
    (rx_t_ms + rx1_delay_s * 1000).saturating_add_signed(offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::p2p::hdlc::Frame;

    /// `radio-set` changes one axis and one only: everything else in the pushed
    /// `SET_RADIO_CONFIG` must be byte-identical to what is already in force, or a spreading
    /// factor change would silently retune the channel too.
    #[test]
    fn with_spreading_factor_changes_only_the_spreading_factor() {
        let base = RadioConfig {
            freq_hz: 868_100_000,
            sf: 10,
            bw: 0,
            cr: 1,
            preamble: 8,
            tx_power_dbm: 14,
            flags: 0,
        };
        let next = base.with_spreading_factor(9).expect("9 is in range");
        assert_eq!(next.sf, 9);
        assert_eq!(
            RadioConfig {
                sf: base.sf,
                ..next
            },
            base,
            "nothing else moved"
        );

        // The modem layer stays safe standalone: a hand-edited sessions.db cannot push a
        // nonsense register write through it.
        for bad in [6u8, 13, 0, 255] {
            let err = base
                .with_spreading_factor(bad)
                .expect_err("outside 7..=12 is refused");
            assert_eq!(err.kind(), Kind::InvalidArgument);
        }
    }

    #[test]
    fn responses_carry_the_command_type_with_the_rsp_bit() {
        assert_eq!(RSP_FLAG | cmd::GET_INFO, 0x41);
        assert_eq!(RSP_FLAG | cmd::TX_SCHEDULE, 0x45);
    }

    #[test]
    fn radio_config_encodes_little_endian() {
        let c = RadioConfig {
            freq_hz: 868_100_000,
            sf: 10,
            bw: 0,
            cr: 1,
            preamble: 8,
            tx_power_dbm: 14,
            flags: 0,
        };
        let b = c.encode();
        assert_eq!(&b[0..4], &868_100_000u32.to_le_bytes());
        assert_eq!(b[4], 10);
        assert_eq!(&b[7..9], &8u16.to_le_bytes());
        assert_eq!(b[9], 14);
        assert_eq!(b.len(), 11);
    }

    #[test]
    fn the_hub_config_maps_onto_the_modem_encoding() {
        let hub = crate::config::P2pRadio {
            frequency_hz: 868_100_000,
            spreading_factor: 10,
            bandwidth_khz: 125,
            coding_rate: "4/5".into(),
            preamble: 8,
            node_tx_power_dbm: 0,
            deliver_time: false,
            rekey_counter_threshold: u32::MAX,
            history_stall_timeout_s: crate::config::default_history_stall_timeout_s(),
            modulation: crate::config::P2pModulation::Lora,
            sync_word: crate::config::SYNC_WORD_PRIVATE,
            tx_power: 14,
        };
        let c = RadioConfig::from_hub_config(&hub).expect("maps");
        assert_eq!(c.bw, 0, "125 kHz is encoding 0");
        assert_eq!(c.cr, 1, "4/5 is encoding 1");
        assert_eq!(
            c.flags & 1,
            0,
            "private syncword: this is not a LoRaWAN network"
        );

        // Unencodable values are refused rather than silently coerced.
        let bad = crate::config::P2pRadio {
            bandwidth_khz: 62,
            ..hub.clone()
        };
        assert!(RadioConfig::from_hub_config(&bad).is_err());
        let bad = crate::config::P2pRadio {
            coding_rate: "4/9".into(),
            ..hub
        };
        assert!(RadioConfig::from_hub_config(&bad).is_err());
    }

    #[test]
    fn an_rx_event_decodes_with_its_metadata() {
        let mut body = Vec::new();
        body.extend_from_slice(&123_456u64.to_le_bytes());
        body.extend_from_slice(&(-77i16).to_le_bytes());
        body.push(28u8); // 28 quarter-dB = 7.0 dB
        body.push(0); // reserved
        body.push(3);
        body.extend_from_slice(&[0xaa, 0xbb, 0xcc]);

        let e = RxEvent::decode(&body).expect("decodes");
        assert_eq!(e.t_ms, 123_456);
        assert_eq!(e.rssi_dbm, -77);
        assert_eq!(e.snr_db(), 7.0);
        assert_eq!(e.frame, vec![0xaa, 0xbb, 0xcc]);
    }

    #[test]
    fn a_truncated_event_decodes_to_none_rather_than_panicking() {
        assert!(RxEvent::decode(&[]).is_none());
        assert!(RxEvent::decode(&[0; 12]).is_none());
        // Claims 10 bytes of frame but carries none.
        let mut body = vec![0u8; 13];
        body[12] = 10;
        assert!(RxEvent::decode(&body).is_none());

        assert!(TxDone::decode(&[0; 9]).is_none());
        assert!(BootEvent::decode(&[0; 7]).is_none());
    }

    #[test]
    fn events_dispatch_by_frame_type() {
        let boot = Frame {
            frame_type: evt::BOOT,
            seq: 0,
            body: vec![1, 1, 4, 0xc0, 0x5b, 0x8d, 0x3e, 2],
        };
        assert!(matches!(Event::from_frame(&boot), Some(Event::Boot(_))));

        let log = Frame {
            frame_type: evt::LOG,
            seq: 1,
            body: b"hello".to_vec(),
        };
        assert_eq!(Event::from_frame(&log), Some(Event::Log("hello".into())));

        let low = Frame {
            frame_type: 0x86,
            seq: 2,
            body: vec![1, 0, 0, 0, 2, 0, 0, 0],
        };
        assert!(matches!(
            Event::from_frame(&low),
            Some(Event::TwrCtrLow(twr_link::TwrCtrLow { next: 1, last: 2 }))
        ));

        // A ring event keeps the frame's seq: it is the delivery sequence number.
        let tx = twr_link::TwrTx {
            addr: 9,
            item: 3,
            outcome: twr_link::TxOutcome::Sent,
            gw_counter: 11,
            node_ack_counter: 0,
            ack_rssi: None,
        };
        let ring = Frame {
            frame_type: 0x85,
            seq: 0xF3,
            body: tx.encode(),
        };
        assert_eq!(
            Event::from_frame(&ring),
            Some(Event::TwrTx { seq: 0xF3, tx })
        );
    }

    #[test]
    fn a_structured_evt_log_is_a_fault_and_a_legacy_line_stays_a_line() {
        assert_eq!(
            decode_log(&[1, 2, b'r', b'x']),
            Event::Fault(FaultEvent {
                level: FAULT_LEVEL_ERROR,
                code: 2,
                text: "rx".into()
            })
        );
        assert_eq!(
            decode_log(&[3, 0, b'd', b'b', b'g']),
            Event::Log("dbg".into())
        );
        assert_eq!(decode_log(b"legacy"), Event::Log("legacy".into()));

        let unknown = Frame {
            frame_type: 0x99,
            seq: 0,
            body: vec![],
        };
        assert_eq!(Event::from_frame(&unknown), None);
    }

    /// Every non-OK status maps onto the taxonomy, and a missed radio deadline is a Timeout
    /// rather than a transport fault: the command was accepted, the window was not made.
    #[test]
    fn modem_statuses_map_onto_the_taxonomy() {
        assert!(Status::Ok.into_result("x").is_ok());
        assert_eq!(
            Status::BadParam.into_result("x").expect_err("err").kind(),
            Kind::InvalidArgument
        );
        assert_eq!(
            Status::Late.into_result("x").expect_err("err").kind(),
            Kind::Timeout
        );
        assert_eq!(
            Status::Busy.into_result("x").expect_err("err").kind(),
            Kind::AlreadyExists
        );
        assert_eq!(
            Status::Internal.into_result("x").expect_err("err").kind(),
            Kind::Transport
        );
        // The TOWER gateway's two statuses (p2p_tower_gateway.md §2).
        assert_eq!(Status::from_byte(6), Status::NoSpace);
        assert_eq!(Status::from_byte(7), Status::NotFound);
        assert_eq!(Status::from_byte(8), Status::Unsupported);
        assert_eq!(
            Status::NotFound.into_result("x").expect_err("err").kind(),
            Kind::NotFound
        );
        assert_eq!(
            Status::NoSpace.into_result("x").expect_err("err").kind(),
            Kind::Transport
        );
        // Every one names the firmware, so an operator knows which side to look at.
        assert_eq!(
            Status::Late.into_result("x").expect_err("err").component(),
            Component::NorthbridgeFirmware
        );
    }

    /// The bench finding, encoded: the aim takes the modem's TX lead off, so the ACK itself
    /// starts the set offset after the nominal RX1, whatever that lead is.
    #[test]
    fn the_answer_starts_the_set_offset_after_the_nominal_rx1() {
        for lead in [0.0, 22.0, 40.0] {
            let at = schedule_at(1_000_000, 1, lead);
            assert_eq!(
                at as i64 + lead as i64,
                1_000_000 + 1000 + RX1_ACK_START_MS,
                "lead {lead} ms"
            );
        }
    }

    #[test]
    fn a_tx_schedule_encodes_its_frame_inline() {
        let s = TxSchedule {
            tx_id: 7,
            t_ms: 42,
            frame: vec![1, 2, 3],
        };
        let b = s.encode().expect("a 3 byte frame encodes");
        assert_eq!(b[0], 7);
        assert_eq!(&b[1..9], &42u64.to_le_bytes());
        assert_eq!(b[9], 3, "length precedes the frame");
        assert_eq!(&b[10..], &[1, 2, 3]);
    }

    fn hub_config() -> crate::config::P2pRadio {
        crate::config::P2pRadio {
            frequency_hz: 868_100_000,
            spreading_factor: 9,
            bandwidth_khz: 125,
            coding_rate: "4/5".into(),
            preamble: 8,
            node_tx_power_dbm: 0,
            deliver_time: false,
            rekey_counter_threshold: u32::MAX,
            history_stall_timeout_s: crate::config::default_history_stall_timeout_s(),
            modulation: crate::config::P2pModulation::Lora,
            sync_word: crate::config::SYNC_WORD_PRIVATE,
            tx_power: 14,
        }
    }

    /// F15: `hdlc` allows frames up to 280 bytes, the schedule's length field is one byte.
    /// `as u8` truncated silently, so the northbridge was handed a frame whose declared
    /// length was wrong and whose tail read as the next message.
    #[test]
    fn a_frame_too_long_for_the_length_field_is_refused_not_truncated() {
        let s = TxSchedule {
            tx_id: 1,
            t_ms: 0,
            frame: vec![0xAA; 256],
        };
        let err = s.encode().expect_err("256 does not fit in a u8");
        assert_eq!(err.kind(), Kind::InvalidArgument);
        assert_eq!(err.component(), Component::NorthbridgeFirmware);

        // The boundary itself still encodes.
        let ok = TxSchedule {
            tx_id: 1,
            t_ms: 0,
            frame: vec![0xAA; TxSchedule::MAX_FRAME],
        };
        assert_eq!(
            ok.encode().expect("255 fits")[9],
            255,
            "the largest describable frame is still accepted"
        );
    }

    /// F15: bandwidth and coding rate were range-checked, the spreading factor was not.
    #[test]
    fn an_out_of_range_spreading_factor_is_refused() {
        for sf in [6u8, 13] {
            let cfg = crate::config::P2pRadio {
                spreading_factor: sf,
                ..hub_config()
            };
            let err = RadioConfig::from_hub_config(&cfg)
                .expect_err("only 7..=12 are spreadable factors the modem takes");
            assert_eq!(err.kind(), Kind::InvalidArgument);
        }
        for sf in [7u8, 12] {
            let cfg = crate::config::P2pRadio {
                spreading_factor: sf,
                ..hub_config()
            };
            assert!(
                RadioConfig::from_hub_config(&cfg).is_ok(),
                "sf {sf} is valid"
            );
        }
    }

    /// The three keys of p2p_tower_gateway.md §3.5: the public sync word sets `flags` bit 0,
    /// `tx_power` is the Northbridge's own, and `fsk` is refused until #470 P5.
    #[test]
    fn modulation_sync_word_and_tx_power_map_or_are_refused() {
        let c = RadioConfig::from_hub_config(&crate::config::P2pRadio {
            sync_word: crate::config::SYNC_WORD_PUBLIC,
            tx_power: -16,
            ..hub_config()
        })
        .expect("maps");
        assert_eq!(c.flags & SYNC_WORD_PUBLIC_FLAG, SYNC_WORD_PUBLIC_FLAG);
        assert_eq!(c.tx_power_dbm, -16);
        assert_eq!(c.encode()[9], (-16i8) as u8);
        let top = RadioConfig::from_hub_config(&crate::config::P2pRadio {
            tx_power: 15,
            ..hub_config()
        })
        .expect("the RFO_LP top");
        assert_eq!(top.tx_power_dbm, 15);

        for bad in [
            crate::config::P2pRadio {
                modulation: crate::config::P2pModulation::Fsk,
                ..hub_config()
            },
            crate::config::P2pRadio {
                sync_word: 0x2B,
                ..hub_config()
            },
            crate::config::P2pRadio {
                tx_power: -17,
                ..hub_config()
            },
            crate::config::P2pRadio {
                tx_power: 16,
                ..hub_config()
            },
        ] {
            let err = RadioConfig::from_hub_config(&bad).expect_err("refused");
            assert_eq!(err.kind(), Kind::InvalidArgument, "{bad:?}");
        }
    }
}
