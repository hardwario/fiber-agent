//! The TOWER payload envelopes and the `0x91` control TLV list — #470 §7.5, §8.
//!
//! Every TOWER Data payload starts with an envelope byte:
//!
//! ```text
//! 0x81 ‖ port(1) ‖ protobuf                  application data: fPort 2/3/85 up, 86 down
//! 0x91 ‖ { cmd(1) ‖ len(1) ‖ value(len) }×n  control, little-endian values like TOWER
//! ```
//!
//! The TLV list carries an explicit length so a receiver skips an unknown `cmd` and processes
//! the rest (forward compatible, unlike LoRaWAN MAC commands). Stage 1 is end to end, Node ↔
//! central: the Northbridge forwards `0x91` uplinks as plaintext and pushes `0x91` answers
//! like any downlink.

use crate::error::{Component, Error, Kind, Result, Transport};

/// Application data envelope.
pub const ENVELOPE_APP: u8 = 0x81;
/// Control TLV envelope.
pub const ENVELOPE_CONTROL: u8 = 0x91;

/// The fPort a downlink command rides in the `0x81` envelope.
pub const COMMAND_PORT: u8 = 86;

/// Stage-1 control command IDs (#470 §8.2; final, they carry over to the native form).
pub mod cmd {
    pub const CAPABILITIES: u8 = 0x01;
    pub const HELLO: u8 = 0x02;
    pub const DETACH: u8 = 0x03;
    pub const REJOIN_REQ: u8 = 0x04;
    pub const JOIN_REQ: u8 = 0x07;
    pub const JOIN_ACCEPT: u8 = 0x08;
    pub const HOME_GATEWAY: u8 = 0x06;
    pub const LINK_CHECK: u8 = 0x10;
    pub const BACKOFF: u8 = 0x15;
    pub const TIME: u8 = 0x20;
    pub const PENDING_INFO: u8 = 0x22;
    pub const DEV_STATUS: u8 = 0x30;
    // Dropped by #487 C-3, never sent: 0x05 RekeyReq, 0x11 RadioParamReq, 0x21 Poll.
    // Reserved by #487 C-4, not implemented: 0x12-0x14, 0x23, 0x31, 0x32.
}

/// The longest `Backoff` the Node honours (#487 `APP_RADIO_BACKOFF_MAX_S`).
pub const BACKOFF_MAX_S: u16 = 3600;

/// The `Detach` reason the central sends at `node-remove` (p2p_control_commands.md §3).
pub const DETACH_REASON_REMOVED: u8 = 1;

/// `RejoinReq.kind` (#470 §8.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejoinKind {
    Rediscover = 0,
    Rekey = 1,
}

/// A control command the central sends down (p2p_control_commands.md §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlDownlink {
    Detach { reason: u8 },
    RejoinReq { kind: RejoinKind },
    HomeGateway { gw_addr: u32, flags: u8 },
    Backoff { seconds: u16 },
    PendingInfo { more: u8 },
    DevStatusReq,
}

impl ControlDownlink {
    /// `PendingInfo` for `more` queued downlinks, capped at 255 (#487 H-4).
    pub fn pending_info(more: usize) -> Self {
        ControlDownlink::PendingInfo {
            more: u8::try_from(more).unwrap_or(u8::MAX),
        }
    }

    pub fn cmd(&self) -> u8 {
        match self {
            ControlDownlink::Detach { .. } => cmd::DETACH,
            ControlDownlink::RejoinReq { .. } => cmd::REJOIN_REQ,
            ControlDownlink::HomeGateway { .. } => cmd::HOME_GATEWAY,
            ControlDownlink::Backoff { .. } => cmd::BACKOFF,
            ControlDownlink::PendingInfo { .. } => cmd::PENDING_INFO,
            ControlDownlink::DevStatusReq => cmd::DEV_STATUS,
        }
    }

    /// The name operators and logs see.
    pub fn name(&self) -> &'static str {
        match self {
            ControlDownlink::Detach { .. } => "detach",
            ControlDownlink::RejoinReq { .. } => "rejoin",
            ControlDownlink::HomeGateway { .. } => "home-gateway",
            ControlDownlink::Backoff { .. } => "backoff",
            ControlDownlink::PendingInfo { .. } => "pending-info",
            ControlDownlink::DevStatusReq => "dev-status",
        }
    }

    /// The command's value, little-endian.
    pub fn value(&self) -> Vec<u8> {
        match *self {
            ControlDownlink::Detach { reason } => vec![reason],
            ControlDownlink::RejoinReq { kind } => vec![kind as u8],
            ControlDownlink::HomeGateway { gw_addr, flags } => {
                let mut v = gw_addr.to_le_bytes().to_vec();
                v.push(flags);
                v
            }
            ControlDownlink::Backoff { seconds } => seconds.to_le_bytes().to_vec(),
            ControlDownlink::PendingInfo { more } => vec![more],
            ControlDownlink::DevStatusReq => Vec::new(),
        }
    }

    /// One TLV, `cmd ‖ len ‖ value`, without the `0x91` envelope.
    pub fn tlv(&self) -> Vec<u8> {
        let value = self.value();
        let mut out = Vec::with_capacity(2 + value.len());
        out.push(self.cmd());
        // Every value here is at most 5 B.
        out.push(value.len() as u8);
        out.extend_from_slice(&value);
        out
    }
}

/// H-7: whether a Node's last Capabilities name `cmd`. `Err` says why not, for the operator.
pub fn gate(caps: Option<&Capabilities>, cmd: u8) -> std::result::Result<(), String> {
    match caps {
        None => Err(format!(
            "the node has sent no Capabilities in this session, so it is not known to \
             understand control command {cmd:#04x}"
        )),
        Some(c) if !c.supports(cmd) => Err(format!(
            "the node's Capabilities bitmap ({:#018x}) has no bit for control command \
             {cmd:#04x}: its firmware does not implement it (sticker-firmware #487 or later \
             does)",
            c.cmd_bitmap
        )),
        Some(_) => Ok(()),
    }
}

fn codec_err(msg: String) -> Error {
    Error::new(
        Kind::Transport,
        Transport::P2p,
        Component::NorthbridgeFirmware,
        msg,
    )
}

/// One control command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tlv<'a> {
    pub cmd: u8,
    pub value: &'a [u8],
}

/// A decoded TOWER payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Envelope<'a> {
    /// `0x81 ‖ port ‖ body`.
    App { port: u8, body: &'a [u8] },
    /// `0x91 ‖ TLV…`, every command in order, unknown ones included (the caller skips them).
    Control(Vec<Tlv<'a>>),
}

impl<'a> Envelope<'a> {
    /// Split a plaintext into its envelope. An empty payload, an unknown envelope byte, an
    /// `0x81` without a port, and a TLV list whose last length runs past the end are errors.
    pub fn parse(plaintext: &'a [u8]) -> Result<Self> {
        match plaintext {
            [ENVELOPE_APP, port, body @ ..] => Ok(Envelope::App { port: *port, body }),
            [ENVELOPE_APP] => Err(codec_err("0x81 envelope without a port".into())),
            [ENVELOPE_CONTROL, rest @ ..] => parse_tlvs(rest).map(Envelope::Control),
            [other, ..] => Err(codec_err(format!("unknown envelope byte {other:#04x}"))),
            [] => Err(codec_err("empty P2P payload".into())),
        }
    }
}

/// Parse a TLV list (without the `0x91`). Truncation is an error: a length that runs past the
/// end means the list cannot be trusted past that point.
pub fn parse_tlvs(mut b: &[u8]) -> Result<Vec<Tlv<'_>>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let [cmd, len, rest @ ..] = b else {
            return Err(codec_err(format!(
                "control TLV cut short: {} B left, a command needs 2",
                b.len()
            )));
        };
        let len = usize::from(*len);
        if rest.len() < len {
            return Err(codec_err(format!(
                "control TLV {cmd:#04x} says {len} B, {} B left",
                rest.len()
            )));
        }
        out.push(Tlv {
            cmd: *cmd,
            value: &rest[..len],
        });
        b = &rest[len..];
    }
    Ok(out)
}

/// Encode a `0x91` payload from `(cmd, value)` pairs.
pub fn encode_control(items: &[(u8, &[u8])]) -> Result<Vec<u8>> {
    let mut out = vec![ENVELOPE_CONTROL];
    for (cmd, value) in items {
        let len = u8::try_from(value.len())
            .map_err(|_| codec_err(format!("control TLV {cmd:#04x} value of {} B", value.len())))?;
        out.push(*cmd);
        out.push(len);
        out.extend_from_slice(value);
    }
    Ok(out)
}

/// The `0x81 ‖ 86 ‖ body` a downlink command goes out as.
pub fn command_payload(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + body.len());
    out.push(ENVELOPE_APP);
    out.push(COMMAND_PORT);
    out.extend_from_slice(body);
    out
}

/// `0x07 JoinReq` — the legacy P2P JoinRequest body, re-framed (#470 §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinReq {
    pub product_type: u8,
    /// The Node's `APP_PROTO_VERSION` (app/protobuf payload version), not the wire version.
    pub proto_version: u8,
    /// MSB-first, as the registry spells it.
    pub dev_eui: [u8; 8],
    /// Big-endian on the wire (carried over from the legacy body).
    pub fw_version: u32,
}

impl JoinReq {
    pub const LEN: usize = 14;

    pub fn decode(v: &[u8]) -> Option<Self> {
        if v.len() != Self::LEN {
            return None;
        }
        Some(JoinReq {
            product_type: v[0],
            proto_version: v[1],
            dev_eui: v[2..10].try_into().ok()?,
            fw_version: u32::from_be_bytes(v[10..14].try_into().ok()?),
        })
    }

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0] = self.product_type;
        b[1] = self.proto_version;
        b[2..10].copy_from_slice(&self.dev_eui);
        b[10..14].copy_from_slice(&self.fw_version.to_be_bytes());
        b
    }
}

/// The highest TX power a Node takes from its JoinAccept: 14 dBm, the RFO_LP limit
/// (sticker-firmware #486 W8). A Node ignores 15–22, so the central never assigns them.
pub const NODE_TX_POWER_MAX_DBM: u8 = 14;

/// The JoinAccept `tx_power` for a configured `radio.p2p_radio.node_tx_power_dbm`: `0` assigns
/// nothing and stays `0`; anything above [`NODE_TX_POWER_MAX_DBM`] is clamped to it.
pub fn assigned_node_tx_power(configured_dbm: u8) -> u8 {
    configured_dbm.min(NODE_TX_POWER_MAX_DBM)
}

/// `0x08 JoinAccept` value: `net_id(4 LE) ‖ central_nonce(4 LE) ‖ rx_delay(1) ‖ tx_power(1) ‖
/// reserved(3)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinAccept {
    pub net_id: u32,
    pub central_nonce: u32,
    /// Seconds.
    pub rx_delay: u8,
    /// dBm; `0` assigns nothing, at most [`NODE_TX_POWER_MAX_DBM`] (see
    /// [`assigned_node_tx_power`]).
    pub tx_power: u8,
}

impl JoinAccept {
    pub const LEN: usize = 13;

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..4].copy_from_slice(&self.net_id.to_le_bytes());
        b[4..8].copy_from_slice(&self.central_nonce.to_le_bytes());
        b[8] = self.rx_delay;
        b[9] = self.tx_power;
        b
    }

    pub fn decode(v: &[u8]) -> Option<Self> {
        if v.len() != Self::LEN {
            return None;
        }
        Some(JoinAccept {
            net_id: u32::from_le_bytes(v[0..4].try_into().ok()?),
            central_nonce: u32::from_le_bytes(v[4..8].try_into().ok()?),
            rx_delay: v[8],
            tx_power: v[9],
        })
    }
}

/// `Capabilities.profiles` bit 0: FSK.
pub const PROFILE_FSK: u8 = 1 << 0;
/// `Capabilities.profiles` bit 1: LoRa.
pub const PROFILE_LORA: u8 = 1 << 1;

/// `0x01 Capabilities`: `proto_version(1) mtu(1) profiles(1) cmd_bitmap(8) power_class(1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Capabilities {
    pub proto_version: u8,
    pub mtu: u8,
    pub profiles: u8,
    /// Bit `n` = the sender understands control command `n` (the `0x00–0x3F` core space).
    pub cmd_bitmap: u64,
    pub power_class: u8,
}

impl Capabilities {
    pub const LEN: usize = 12;

    /// What this central answers with (planner 2026-09-28, #470 §8.2): control protocol 1, the
    /// 96 B TOWER frame MTU (#486 U-1), `profiles` bit 1 = `lora` (bit 0 is `fsk`, not before P5), the control
    /// commands it handles in the bitmap, power class 0 = mains (1 is battery, the Nodes').
    pub fn central() -> Self {
        let bits = [
            cmd::CAPABILITIES,
            cmd::HELLO,
            cmd::DETACH,
            cmd::REJOIN_REQ,
            cmd::JOIN_REQ,
            cmd::JOIN_ACCEPT,
            cmd::LINK_CHECK,
            cmd::BACKOFF,
            cmd::TIME,
            cmd::PENDING_INFO,
            cmd::DEV_STATUS,
        ];
        Capabilities {
            proto_version: 1,
            mtu: super::frame::MAX_FRAME as u8,
            profiles: PROFILE_LORA,
            cmd_bitmap: bits.iter().fold(0u64, |m, b| m | (1u64 << b)),
            power_class: 0,
        }
    }

    /// Whether bit `cmd` is set. IDs past the 64-bit core space are never named (#487 C-5).
    pub fn supports(&self, cmd: u8) -> bool {
        cmd < 64 && self.cmd_bitmap & (1u64 << cmd) != 0
    }

    /// A longer value is accepted and its tail ignored, so a later field does not break us.
    pub fn decode(v: &[u8]) -> Option<Self> {
        if v.len() < Self::LEN {
            return None;
        }
        Some(Capabilities {
            proto_version: v[0],
            mtu: v[1],
            profiles: v[2],
            cmd_bitmap: u64::from_le_bytes(v[3..11].try_into().ok()?),
            power_class: v[11],
        })
    }

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0] = self.proto_version;
        b[1] = self.mtu;
        b[2] = self.profiles;
        b[3..11].copy_from_slice(&self.cmd_bitmap.to_le_bytes());
        b[11] = self.power_class;
        b
    }
}

/// `0x02 Hello`: `session_id(4 LE) reset_reason(1) fw(4)` (planner 2026-09-28, #470 §8.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Hello {
    /// Random per Node boot: a new value is a reboot.
    pub session_id: u32,
    /// The low byte of the Node's hwinfo `RESET_*` mask.
    pub reset_reason: u8,
    /// `major, minor, patch, 0`.
    pub fw: [u8; 4],
}

impl Hello {
    pub const LEN: usize = 9;

    pub fn decode(v: &[u8]) -> Option<Self> {
        if v.len() < Self::LEN {
            return None;
        }
        Some(Hello {
            session_id: u32::from_le_bytes(v[0..4].try_into().ok()?),
            reset_reason: v[4],
            fw: v[5..9].try_into().ok()?,
        })
    }
}

/// `0x10 LinkCheckAns`: `rssi(i8) snr(i8) margin(i8) gw_count(1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkCheckAns {
    pub rssi_dbm: i8,
    pub snr_db: i8,
    pub margin_db: i8,
    pub gw_count: u8,
}

impl LinkCheckAns {
    /// From the uplink's reception: RSSI and SNR clamped to `i8`, and the margin over the
    /// demodulation floor of the spreading factor (−7.5 dB at SF7, 2.5 dB lower per step,
    /// the SX126x datasheet), rounded down.
    pub fn from_reception(rssi_dbm: i16, snr_q_db: i8, sf: u8, gw_count: u8) -> Self {
        let snr = f64::from(snr_q_db) / 4.0;
        let floor = -7.5 - 2.5 * f64::from(sf.saturating_sub(7));
        let clamp = |v: f64| v.floor().clamp(f64::from(i8::MIN), f64::from(i8::MAX)) as i8;
        LinkCheckAns {
            rssi_dbm: rssi_dbm.clamp(i16::from(i8::MIN), i16::from(i8::MAX)) as i8,
            snr_db: clamp(snr),
            margin_db: clamp(snr - floor),
            gw_count,
        }
    }

    pub fn encode(&self) -> [u8; 4] {
        [
            self.rssi_dbm as u8,
            self.snr_db as u8,
            self.margin_db as u8,
            self.gw_count,
        ]
    }
}

/// `0x30 DevStatusAns`: `battery_mv(2) ‖ dl_rssi(i8) ‖ dl_snr(i8) ‖ tx_power(i8) ‖
/// uptime_s(4)`, little-endian (#487).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevStatusAns {
    /// The last battery sample; 0 = none yet.
    pub battery_mv: u16,
    /// The Node's RSSI of the downlink that carried the request.
    pub dl_rssi: i8,
    /// The Node's SNR of that downlink, dB.
    pub dl_snr: i8,
    /// The session TX power, dBm.
    pub tx_power: i8,
    pub uptime_s: u32,
}

impl DevStatusAns {
    pub const LEN: usize = 9;

    /// A longer value is accepted and its tail ignored, like [`Capabilities::decode`].
    pub fn decode(v: &[u8]) -> Option<Self> {
        if v.len() < Self::LEN {
            return None;
        }
        Some(DevStatusAns {
            battery_mv: u16::from_le_bytes([v[0], v[1]]),
            dl_rssi: v[2] as i8,
            dl_snr: v[3] as i8,
            tx_power: v[4] as i8,
            uptime_s: u32::from_le_bytes(v[5..9].try_into().ok()?),
        })
    }

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..2].copy_from_slice(&self.battery_mv.to_le_bytes());
        b[2] = self.dl_rssi as u8;
        b[3] = self.dl_snr as u8;
        b[4] = self.tx_power as u8;
        b[5..9].copy_from_slice(&self.uptime_s.to_le_bytes());
        b
    }
}

/// `0x20 TimeAns`: `unix(4 LE) ‖ frac(1, 1/256 s) ‖ req_counter(4 LE)` (amendment 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeAns {
    pub unix: u32,
    pub frac: u8,
    pub req_counter: u32,
}

impl TimeAns {
    /// From a wall-clock time in milliseconds: the fraction is the sub-second part in
    /// 1/256 s, rounded down.
    pub fn from_unix_ms(unix_ms: u64, req_counter: u32) -> Self {
        TimeAns {
            unix: (unix_ms / 1000) as u32,
            frac: ((unix_ms % 1000) * 256 / 1000) as u8,
            req_counter,
        }
    }

    pub fn encode(&self) -> [u8; 9] {
        let mut b = [0u8; 9];
        b[0..4].copy_from_slice(&self.unix.to_le_bytes());
        b[4] = self.frac;
        b[5..9].copy_from_slice(&self.req_counter.to_le_bytes());
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelopes_split_by_their_first_byte() {
        assert_eq!(
            Envelope::parse(&[0x81, 0x02, 0x08, 0x01]).unwrap(),
            Envelope::App {
                port: 2,
                body: &[0x08, 0x01]
            }
        );
        assert_eq!(
            Envelope::parse(&[0x91, 0x20, 0x00]).unwrap(),
            Envelope::Control(vec![Tlv {
                cmd: cmd::TIME,
                value: &[]
            }])
        );
        assert!(Envelope::parse(&[]).is_err());
        assert!(Envelope::parse(&[0x81]).is_err());
        assert!(Envelope::parse(&[0x42, 0x00]).is_err());
        // An empty control list is a valid, empty list.
        assert_eq!(Envelope::parse(&[0x91]).unwrap(), Envelope::Control(vec![]));
    }

    #[test]
    fn an_unknown_command_is_kept_by_length_and_the_rest_still_parses() {
        let list = [0x91, 0x7e, 0x03, 0xaa, 0xbb, 0xcc, 0x10, 0x00, 0x20, 0x00];
        let Envelope::Control(tlvs) = Envelope::parse(&list).unwrap() else {
            panic!("control")
        };
        assert_eq!(
            tlvs.iter().map(|t| t.cmd).collect::<Vec<_>>(),
            vec![0x7e, cmd::LINK_CHECK, cmd::TIME]
        );
        assert_eq!(tlvs[0].value, &[0xaa, 0xbb, 0xcc]);
    }

    #[test]
    fn a_length_past_the_end_is_refused() {
        assert!(parse_tlvs(&[0x20, 0x05, 0x00]).is_err());
        assert!(parse_tlvs(&[0x20]).is_err());
    }

    #[test]
    fn the_join_kat_payloads_decode() {
        // tower_join_kat.json: the JoinReq and JoinAccept plaintexts.
        let req = [
            0x91, 0x07, 0x0e, 0x01, 0x01, 0x58, 0x76, 0x07, 0x00, 0x00, 0x00, 0x04, 0x13, 0x01,
            0x05, 0x00, 0x00,
        ];
        let Envelope::Control(tlvs) = Envelope::parse(&req).unwrap() else {
            panic!("control")
        };
        assert_eq!(tlvs[0].cmd, cmd::JOIN_REQ);
        let jr = JoinReq::decode(tlvs[0].value).unwrap();
        assert_eq!(jr.dev_eui, [0x58, 0x76, 0x07, 0x00, 0x00, 0x00, 0x04, 0x13]);
        assert_eq!(jr.fw_version, 0x0105_0000);
        assert_eq!((jr.product_type, jr.proto_version), (1, 1));
        assert_eq!(
            encode_control(&[(cmd::JOIN_REQ, &jr.encode())]).unwrap(),
            req
        );

        let ja = JoinAccept {
            net_id: 0x6D24_9662,
            central_nonce: 0xA1B2_C3D4,
            rx_delay: 1,
            tx_power: 14,
        };
        let acc = encode_control(&[(cmd::JOIN_ACCEPT, &ja.encode())]).unwrap();
        assert_eq!(
            acc,
            [
                0x91, 0x08, 0x0d, 0x62, 0x96, 0x24, 0x6d, 0xd4, 0xc3, 0xb2, 0xa1, 0x01, 0x0e, 0x00,
                0x00, 0x00
            ]
        );
        assert_eq!(JoinAccept::decode(&acc[3..]), Some(ja));
    }

    #[test]
    fn time_ans_is_unix_fraction_and_the_request_counter() {
        let t = TimeAns::from_unix_ms(1_790_000_000_500, 3);
        assert_eq!((t.unix, t.frac, t.req_counter), (1_790_000_000, 128, 3));
        let b = t.encode();
        assert_eq!(&b[0..4], &1_790_000_000u32.to_le_bytes());
        assert_eq!(b[4], 128);
        assert_eq!(&b[5..9], &[3, 0, 0, 0]);
        assert_eq!(TimeAns::from_unix_ms(999, 0).frac, 255);
    }

    #[test]
    fn link_check_margin_is_over_the_sf_floor() {
        // SF7 floor -7.5 dB: SNR +5 dB is a 12 dB margin.
        let a = LinkCheckAns::from_reception(-80, 20, 7, 1);
        assert_eq!(
            (a.rssi_dbm, a.snr_db, a.margin_db, a.gw_count),
            (-80, 5, 12, 1)
        );
        // SF10 floor -15 dB: SNR -10.25 dB is a 4 dB margin (rounded down).
        let b = LinkCheckAns::from_reception(-140, -41, 10, 1);
        assert_eq!((b.rssi_dbm, b.snr_db, b.margin_db), (-128, -11, 4));
        assert_eq!(b.encode(), [0x80, 0xf5, 0x04, 0x01]);
    }

    #[test]
    fn capabilities_round_trip_and_a_longer_value_is_accepted() {
        let c = Capabilities::central();
        assert_eq!(Capabilities::decode(&c.encode()), Some(c));
        assert!(c.cmd_bitmap & (1 << cmd::TIME) != 0);
        let mut longer = c.encode().to_vec();
        longer.push(0xff);
        assert_eq!(Capabilities::decode(&longer), Some(c));
        assert_eq!(Capabilities::decode(&longer[..5]), None);
    }

    /// #487 "Wire": every central downlink's TLV, byte for byte.
    #[test]
    fn control_downlinks_encode_as_the_487_table() {
        let tlv = |d: ControlDownlink| d.tlv();
        assert_eq!(
            tlv(ControlDownlink::Detach {
                reason: DETACH_REASON_REMOVED
            }),
            [0x03, 0x01, 0x01]
        );
        assert_eq!(
            tlv(ControlDownlink::RejoinReq {
                kind: RejoinKind::Rediscover
            }),
            [0x04, 0x01, 0x00]
        );
        assert_eq!(
            tlv(ControlDownlink::RejoinReq {
                kind: RejoinKind::Rekey
            }),
            [0x04, 0x01, 0x01]
        );
        assert_eq!(
            tlv(ControlDownlink::HomeGateway {
                gw_addr: 0xa1b2_c3d4,
                flags: 0
            }),
            [0x06, 0x05, 0xd4, 0xc3, 0xb2, 0xa1, 0x00]
        );
        assert_eq!(
            tlv(ControlDownlink::Backoff { seconds: 120 }),
            [0x15, 0x02, 0x78, 0x00]
        );
        assert_eq!(
            tlv(ControlDownlink::Backoff { seconds: 3600 }),
            [0x15, 0x02, 0x10, 0x0e]
        );
        assert_eq!(tlv(ControlDownlink::DevStatusReq), [0x30, 0x00]);
        assert_eq!(tlv(ControlDownlink::pending_info(2)), [0x22, 0x01, 0x02]);
        // The list parses back to the same commands.
        let mut list = tlv(ControlDownlink::DevStatusReq);
        list.extend(tlv(ControlDownlink::pending_info(1)));
        let parsed = parse_tlvs(&list).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed[1],
            Tlv {
                cmd: 0x22,
                value: &[1]
            }
        );
    }

    #[test]
    fn pending_info_is_capped_at_255() {
        assert_eq!(
            ControlDownlink::pending_info(0),
            ControlDownlink::PendingInfo { more: 0 }
        );
        assert_eq!(
            ControlDownlink::pending_info(255),
            ControlDownlink::PendingInfo { more: 255 }
        );
        assert_eq!(
            ControlDownlink::pending_info(1000),
            ControlDownlink::PendingInfo { more: 255 }
        );
    }

    #[test]
    fn dev_status_ans_decodes_the_487_layout() {
        // 3012 mV, RSSI -71, SNR -3, 14 dBm, uptime 86400 s, as the Node writes it.
        let v = [0xc4, 0x0b, 0xb9, 0xfd, 0x0e, 0x80, 0x51, 0x01, 0x00];
        let a = DevStatusAns::decode(&v).unwrap();
        assert_eq!(
            a,
            DevStatusAns {
                battery_mv: 3012,
                dl_rssi: -71,
                dl_snr: -3,
                tx_power: 14,
                uptime_s: 86_400
            }
        );
        assert_eq!(a.encode(), v);
        // A longer value: the tail is a later field. A shorter one is not an answer.
        let mut longer = v.to_vec();
        longer.push(0xee);
        assert_eq!(DevStatusAns::decode(&longer), Some(a));
        assert_eq!(DevStatusAns::decode(&v[..8]), None);
    }

    /// #487: the Node's bitmap `de 01 21 00 05 00 01 00` names 0x01-0x04, 0x06-0x08, 0x10,
    /// 0x15, 0x20, 0x22, 0x30, and the gate follows it.
    #[test]
    fn the_capabilities_gate_follows_the_node_bitmap() {
        let node = Capabilities {
            proto_version: 1,
            mtu: 96,
            profiles: PROFILE_LORA,
            cmd_bitmap: u64::from_le_bytes([0xde, 0x01, 0x21, 0x00, 0x05, 0x00, 0x01, 0x00]),
            power_class: 1,
        };
        for c in [
            cmd::DETACH,
            cmd::REJOIN_REQ,
            cmd::HOME_GATEWAY,
            cmd::BACKOFF,
            cmd::PENDING_INFO,
            cmd::DEV_STATUS,
        ] {
            assert!(gate(Some(&node), c).is_ok(), "{c:#04x}");
        }
        // A pre-#487 Node (v1.4: no 0x06, 0x15, 0x22, 0x30).
        let old = Capabilities {
            cmd_bitmap: (1 << 1)
                | (1 << 2)
                | (1 << 3)
                | (1 << 4)
                | (1 << 7)
                | (1 << 8)
                | (1 << 0x10)
                | (1 << 0x20),
            ..node
        };
        assert!(gate(Some(&old), cmd::DETACH).is_ok());
        let err = gate(Some(&old), cmd::DEV_STATUS).unwrap_err();
        assert!(err.contains("0x30"), "{err}");
        assert!(gate(None, cmd::DEV_STATUS).is_err());
        assert!(!node.supports(0x40), "past the 64-bit core space");
    }

    #[test]
    fn the_central_names_the_commands_it_speaks() {
        let c = Capabilities::central();
        for id in [
            0x01, 0x02, 0x03, 0x04, 0x07, 0x08, 0x10, 0x15, 0x20, 0x22, 0x30,
        ] {
            assert!(c.supports(id), "{id:#04x}");
        }
        for id in [0x05, 0x06, 0x11, 0x21] {
            assert!(!c.supports(id), "{id:#04x}");
        }
    }

    /// #486 W8: the Node takes 2..=14 dBm from its JoinAccept and ignores 15–22, so the
    /// central clamps; 0 (assign nothing) stays 0.
    #[test]
    fn the_assigned_node_tx_power_is_clamped_to_14_dbm() {
        assert_eq!(NODE_TX_POWER_MAX_DBM, 14);
        assert_eq!(assigned_node_tx_power(0), 0, "no assignment");
        assert_eq!(assigned_node_tx_power(2), 2);
        assert_eq!(assigned_node_tx_power(14), 14);
        for dbm in 15..=22 {
            assert_eq!(assigned_node_tx_power(dbm), 14, "{dbm} dBm");
        }
        assert_eq!(assigned_node_tx_power(u8::MAX), 14);
    }

    /// #486 U-1: every `0x91` frame the central sends fits the 74 B payload — the longest
    /// control head with its in-frame PendingInfo, and every answer of one uplink together
    /// (Capabilities, LinkCheckAns, TimeAns), and the JoinAccept.
    #[test]
    fn every_control_frame_fits_the_tower_mtu() {
        use super::super::frame::MAX_PAYLOAD;
        let heads = [
            ControlDownlink::Detach { reason: 1 },
            ControlDownlink::RejoinReq {
                kind: RejoinKind::Rekey,
            },
            ControlDownlink::HomeGateway {
                gw_addr: u32::MAX,
                flags: 0xff,
            },
            ControlDownlink::Backoff { seconds: u16::MAX },
            ControlDownlink::PendingInfo { more: 255 },
            ControlDownlink::DevStatusReq,
        ];
        let longest = heads.iter().map(|h| h.tlv().len()).max().unwrap();
        let pending = ControlDownlink::pending_info(255).tlv().len();
        assert!(1 + longest + pending <= MAX_PAYLOAD, "{longest} B head");

        let caps = Capabilities::central().encode();
        let lc = LinkCheckAns::from_reception(-140, -80, 12, 255).encode();
        let time = TimeAns::from_unix_ms(u64::from(u32::MAX) * 1000, u32::MAX).encode();
        let all = encode_control(&[
            (cmd::CAPABILITIES, &caps[..]),
            (cmd::LINK_CHECK, &lc[..]),
            (cmd::TIME, &time[..]),
        ])
        .unwrap();
        assert!(all.len() <= MAX_PAYLOAD, "{} B of answers", all.len());

        let ja = JoinAccept {
            net_id: u32::MAX,
            central_nonce: u32::MAX,
            rx_delay: u8::MAX,
            tx_power: NODE_TX_POWER_MAX_DBM,
        };
        let accept = encode_control(&[(cmd::JOIN_ACCEPT, &ja.encode())]).unwrap();
        assert!(accept.len() <= MAX_PAYLOAD);
        assert_eq!(Capabilities::central().mtu, 96, "the MTU the Hub reports");
    }

    #[test]
    fn a_command_payload_is_the_0x81_port_86_envelope() {
        assert_eq!(command_payload(&[0x01, 0x02]), vec![0x81, 86, 0x01, 0x02]);
    }
}
