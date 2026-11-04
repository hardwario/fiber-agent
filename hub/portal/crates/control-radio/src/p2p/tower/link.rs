//! The `TWR_*` link messages a TOWER gateway image adds — contract §2.
//!
//! Framing, CRC, `seq` and the `RSP = 0x40 | cmd` status byte are the existing link's
//! ([`crate::p2p::modem`]); this module only encodes the command bodies and decodes the event
//! bodies. Every multi-byte field is little-endian.

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::p2p::tower::frame::MAX_PAYLOAD;

/// Command types, host → gateway (§2.1).
pub mod cmd {
    pub const TWR_START: u8 = 0x10;
    pub const TWR_NODE_ADD: u8 = 0x11;
    pub const TWR_NODE_REMOVE: u8 = 0x12;
    pub const TWR_CTR_BLOCK: u8 = 0x13;
    pub const TWR_QUEUE_PUSH: u8 = 0x14;
    pub const TWR_QUEUE_DROP: u8 = 0x15;
    pub const TWR_NODE_LIST: u8 = 0x17;
    /// Cumulative delivery ack of the gateway's event ring (link doc §4.1). No `RSP`.
    pub const TWR_EVT_ACK: u8 = 0x18;
    /// The Northbridge's time anchor for a `TimeAns` in its ACK (`p2p_tower_gateway.md` §4,
    /// §13.5; #470 §13.5).
    pub const TWR_TIME_SYNC: u8 = 0x19;
}

/// Event types, gateway → host (§2.2).
pub mod evt {
    pub const TWR_UPLINK: u8 = 0x84;
    pub const TWR_TX: u8 = 0x85;
    pub const TWR_CTR_LOW: u8 = 0x86;
}

/// `TWR_START` flags.
pub const START_ACK_ENABLED: u8 = 0x01;
/// `TWR_NODE_ADD` flags: this gateway ACKs the peer. P2 always sets it.
pub const NODE_HOME: u8 = 0x01;
/// `TWR_NODE_ADD` flags: keep the gateway's own `last_seen` when it is newer and the key is the
/// same (link doc §4.2). Set on every restore, never on a new join.
pub const NODE_KEEP_NEWER: u8 = 0x02;
/// `TWR_QUEUE_PUSH` flags: the downlink wants the Node's ACK.
pub const PUSH_CONFIRMED: u8 = 0x01;

/// The turnaround and downlink gap the central asks for (P0's measured 20 ms, §3.4).
pub const TURNAROUND_MS: u8 = 20;
pub const DL_GAP_MS: u8 = 20;

/// Most entries one `TWR_NODE_LIST` response carries.
pub const NODE_LIST_PAGE: usize = 24;

/// One `TWR_*` command. `Debug` never prints a session key.
#[derive(Clone, PartialEq, Eq)]
pub enum TwrCmd {
    Start {
        net_id: u32,
        turnaround_ms: u8,
        dl_gap_ms: u8,
        flags: u8,
    },
    NodeAdd {
        addr: u32,
        session_key: [u8; 16],
        last_seen: u32,
        flags: u8,
    },
    NodeRemove {
        addr: u32,
    },
    CtrBlock {
        first: u32,
        last: u32,
    },
    QueuePush {
        addr: u32,
        item: u16,
        ttl_s: u16,
        flags: u8,
        plaintext: Vec<u8>,
    },
    QueueDrop {
        addr: u32,
        item: u16,
    },
    NodeList {
        start: u16,
    },
    /// "Every ring event up to and including `seq` is processed" (link doc §4.1). The one
    /// command the gateway does not answer.
    EvtAck {
        seq: u8,
    },
    /// The time anchor (`p2p_tower_gateway.md` §13.5): at the Northbridge's `now_ms` (its
    /// `GET_INFO` clock) the wall time was `unix_ms`.
    TimeSync {
        now_ms: u64,
        unix_ms: u64,
    },
}

impl std::fmt::Debug for TwrCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TwrCmd::Start {
                net_id,
                turnaround_ms,
                dl_gap_ms,
                flags,
            } => f
                .debug_struct("Start")
                .field("net_id", &format_args!("{net_id:#010x}"))
                .field("turnaround_ms", turnaround_ms)
                .field("dl_gap_ms", dl_gap_ms)
                .field("flags", flags)
                .finish(),
            TwrCmd::NodeAdd {
                addr,
                last_seen,
                flags,
                ..
            } => f
                .debug_struct("NodeAdd")
                .field("addr", &format_args!("{addr:#010x}"))
                .field("session_key", &"<redacted>")
                .field("last_seen", last_seen)
                .field("flags", flags)
                .finish(),
            TwrCmd::NodeRemove { addr } => f
                .debug_struct("NodeRemove")
                .field("addr", &format_args!("{addr:#010x}"))
                .finish(),
            TwrCmd::CtrBlock { first, last } => f
                .debug_struct("CtrBlock")
                .field("first", first)
                .field("last", last)
                .finish(),
            TwrCmd::QueuePush {
                addr,
                item,
                ttl_s,
                flags,
                plaintext,
            } => f
                .debug_struct("QueuePush")
                .field("addr", &format_args!("{addr:#010x}"))
                .field("item", item)
                .field("ttl_s", ttl_s)
                .field("flags", flags)
                .field("plaintext", &hex(plaintext))
                .finish(),
            TwrCmd::QueueDrop { addr, item } => f
                .debug_struct("QueueDrop")
                .field("addr", &format_args!("{addr:#010x}"))
                .field("item", item)
                .finish(),
            TwrCmd::NodeList { start } => f.debug_struct("NodeList").field("start", start).finish(),
            TwrCmd::EvtAck { seq } => f.debug_struct("EvtAck").field("seq", seq).finish(),
            TwrCmd::TimeSync { now_ms, unix_ms } => f
                .debug_struct("TimeSync")
                .field("now_ms", now_ms)
                .field("unix_ms", unix_ms)
                .finish(),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A body the central refuses before it reaches the link — as [`crate::p2p::modem::TxSchedule`]
/// does, it names the firmware whose contract the body would break.
fn bad_param(what: impl Into<String>) -> Error {
    Error::new(
        Kind::InvalidArgument,
        Transport::P2p,
        Component::NorthbridgeFirmware,
        what,
    )
}

/// Refuse a reserved TOWER address (#470 §4): `0` and `0xFFFFFFFF` are neither a Node nor a
/// network. The gateway answers `BAD_PARAM` for either in every command that carries one.
fn reserved_addr(cmd: &str, what: &str, addr: u32) -> Result<()> {
    if addr == 0 || addr == u32::MAX {
        return Err(bad_param(format!("{cmd}: {what} {addr:#010x} is reserved")));
    }
    Ok(())
}

/// Refuse a flag byte with a bit outside `defined`.
fn undefined_flags(cmd: &str, flags: u8, defined: u8) -> Result<()> {
    if flags & !defined != 0 {
        return Err(bad_param(format!(
            "{cmd}: flag bits {:#04x} are not defined",
            flags & !defined
        )));
    }
    Ok(())
}

impl TwrCmd {
    /// The link command type.
    pub fn cmd(&self) -> u8 {
        match self {
            TwrCmd::Start { .. } => cmd::TWR_START,
            TwrCmd::NodeAdd { .. } => cmd::TWR_NODE_ADD,
            TwrCmd::NodeRemove { .. } => cmd::TWR_NODE_REMOVE,
            TwrCmd::CtrBlock { .. } => cmd::TWR_CTR_BLOCK,
            TwrCmd::QueuePush { .. } => cmd::TWR_QUEUE_PUSH,
            TwrCmd::QueueDrop { .. } => cmd::TWR_QUEUE_DROP,
            TwrCmd::NodeList { .. } => cmd::TWR_NODE_LIST,
            TwrCmd::EvtAck { .. } => cmd::TWR_EVT_ACK,
            TwrCmd::TimeSync { .. } => cmd::TWR_TIME_SYNC,
        }
    }

    /// The contract's name, for errors and the diagnostic log.
    pub fn name(&self) -> &'static str {
        match self {
            TwrCmd::Start { .. } => "TWR_START",
            TwrCmd::NodeAdd { .. } => "TWR_NODE_ADD",
            TwrCmd::NodeRemove { .. } => "TWR_NODE_REMOVE",
            TwrCmd::CtrBlock { .. } => "TWR_CTR_BLOCK",
            TwrCmd::QueuePush { .. } => "TWR_QUEUE_PUSH",
            TwrCmd::QueueDrop { .. } => "TWR_QUEUE_DROP",
            TwrCmd::NodeList { .. } => "TWR_NODE_LIST",
            TwrCmd::EvtAck { .. } => "TWR_EVT_ACK",
            TwrCmd::TimeSync { .. } => "TWR_TIME_SYNC",
        }
    }

    /// Whether the gateway answers this command with an `RSP`: all but `TWR_EVT_ACK`, which
    /// saves the link the round trip (link doc §5.1).
    pub fn expects_rsp(&self) -> bool {
        !matches!(self, TwrCmd::EvtAck { .. })
    }

    /// The command body. A body 0.3.0 would refuse as `BAD_PARAM` for a reason the central
    /// controls (a reserved value, an undefined flag bit, a zero `item`/`ttl_s`/`len`) is
    /// refused here instead, so it never reaches the link.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut b = Vec::new();
        match self {
            TwrCmd::Start {
                net_id,
                turnaround_ms,
                dl_gap_ms,
                flags,
            } => {
                for (name, v) in [("turnaround_ms", turnaround_ms), ("dl_gap_ms", dl_gap_ms)] {
                    if !(20..=60).contains(v) {
                        return Err(bad_param(format!("TWR_START: {name} {v} not in 20..=60")));
                    }
                }
                // 0.3.0 refuses undefined flag bits (`BAD_PARAM`, link doc §2): never sent.
                undefined_flags("TWR_START", *flags, START_ACK_ENABLED)?;
                reserved_addr("TWR_START", "net_id", *net_id)?;
                b.extend_from_slice(&net_id.to_le_bytes());
                b.extend_from_slice(&[*turnaround_ms, *dl_gap_ms, *flags]);
            }
            TwrCmd::NodeAdd {
                addr,
                session_key,
                last_seen,
                flags,
            } => {
                reserved_addr("TWR_NODE_ADD", "address", *addr)?;
                undefined_flags("TWR_NODE_ADD", *flags, NODE_HOME | NODE_KEEP_NEWER)?;
                b.extend_from_slice(&addr.to_le_bytes());
                b.extend_from_slice(session_key);
                b.extend_from_slice(&last_seen.to_le_bytes());
                b.push(*flags);
            }
            TwrCmd::NodeRemove { addr } => {
                reserved_addr("TWR_NODE_REMOVE", "address", *addr)?;
                b.extend_from_slice(&addr.to_le_bytes());
            }
            TwrCmd::CtrBlock { first, last } => {
                if *first == 0 || first > last || *last == u32::MAX {
                    return Err(bad_param(format!(
                        "TWR_CTR_BLOCK: {first}..={last} is not a valid block"
                    )));
                }
                b.extend_from_slice(&first.to_le_bytes());
                b.extend_from_slice(&last.to_le_bytes());
            }
            TwrCmd::QueuePush {
                addr,
                item,
                ttl_s,
                flags,
                plaintext,
            } => {
                reserved_addr("TWR_QUEUE_PUSH", "address", *addr)?;
                if *item == 0 {
                    return Err(bad_param("TWR_QUEUE_PUSH: item 0 is reserved"));
                }
                if *ttl_s == 0 {
                    return Err(bad_param("TWR_QUEUE_PUSH: ttl_s 0 would expire at once"));
                }
                undefined_flags("TWR_QUEUE_PUSH", *flags, PUSH_CONFIRMED)?;
                if plaintext.is_empty() || plaintext.len() > MAX_PAYLOAD {
                    return Err(bad_param(format!(
                        "TWR_QUEUE_PUSH: {} B payload, the frame carries 1..={MAX_PAYLOAD}",
                        plaintext.len()
                    )));
                }
                b.extend_from_slice(&addr.to_le_bytes());
                b.extend_from_slice(&item.to_le_bytes());
                b.extend_from_slice(&ttl_s.to_le_bytes());
                b.push(*flags);
                // Bounded by MAX_PAYLOAD above.
                b.push(plaintext.len() as u8);
                b.extend_from_slice(plaintext);
            }
            TwrCmd::QueueDrop { addr, item } => {
                reserved_addr("TWR_QUEUE_DROP", "address", *addr)?;
                b.extend_from_slice(&addr.to_le_bytes());
                b.extend_from_slice(&item.to_le_bytes());
            }
            TwrCmd::NodeList { start } => b.extend_from_slice(&start.to_le_bytes()),
            TwrCmd::EvtAck { seq } => b.push(*seq),
            TwrCmd::TimeSync { now_ms, unix_ms } => {
                b.extend_from_slice(&now_ms.to_le_bytes());
                b.extend_from_slice(&unix_ms.to_le_bytes());
            }
        }
        Ok(b)
    }
}

fn u16le(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}

fn u32le(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

fn u64le(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

/// `EVT_TWR_UPLINK.ack` (§2.2): the code in bits 0..5; bits 6 and 7 are [`AckTail`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UplinkAck {
    NotConfirmed,
    Sent,
    SentPending,
    /// The ACK went on air past the Node's window, or not at all (a radio error). With
    /// `PENDING` a queued downlink waits for the Node's next uplink (0.3.0).
    Late,
    /// The gateway had no counter left to seal the ACK with.
    NoCounter,
    /// Not ACKed because the event ring was full (link doc §4.1, after the first E2E; 0.3.0
    /// never sends it).
    SuppressedRingFull,
    /// A code this central does not know (bits 0..5 only).
    Unknown(u8),
}

/// The code bits of `EVT_TWR_UPLINK.ack`.
pub const ACK_CODE_MASK: u8 = 0x3F;
/// `ack` bit 6: the ACK's CTRL tail holds a `TimeAns` (#470 §13.5).
pub const ACK_TAIL_TIME: u8 = 0x40;
/// `ack` bit 7: the ACK went out with a CTRL tail (#470 §13.5).
pub const ACK_TAIL_SENT: u8 = 0x80;

/// What the gateway answered in the ACK itself (`ack` bits 6 and 7, #470 §13.5): a
/// `LinkCheckAns` / `TimeAns` tail it built from the uplink's requests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AckTail {
    /// Bit 7: a CTRL tail went out with this ACK.
    pub sent: bool,
    /// Bit 6: the tail holds a `TimeAns` — clear on a `TimeReq` means the gateway had no
    /// `TWR_TIME_SYNC` anchor.
    pub time: bool,
}

impl AckTail {
    fn from_byte(b: u8) -> Self {
        AckTail {
            sent: b & ACK_TAIL_SENT != 0,
            time: b & ACK_TAIL_TIME != 0,
        }
    }

    fn bits(self) -> u8 {
        (if self.sent { ACK_TAIL_SENT } else { 0 }) | (if self.time { ACK_TAIL_TIME } else { 0 })
    }
}

impl UplinkAck {
    /// The code from the whole `ack` byte; bits 6 and 7 are ignored here.
    pub fn from_byte(b: u8) -> Self {
        match b & ACK_CODE_MASK {
            0 => UplinkAck::NotConfirmed,
            1 => UplinkAck::Sent,
            2 => UplinkAck::SentPending,
            3 => UplinkAck::Late,
            4 => UplinkAck::NoCounter,
            5 => UplinkAck::SuppressedRingFull,
            o => UplinkAck::Unknown(o),
        }
    }
}

/// `EVT_TWR_UPLINK`: a fresh, verified frame from a known peer, as plaintext.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwrUplink {
    /// The gateway's monotonic clock at RxDone, the same domain as `EVT_RX.t_ms`.
    pub t_ms: u64,
    pub rssi_dbm: i16,
    /// SNR in quarter-dB.
    pub snr_q_db: i8,
    /// The TOWER header's flags byte.
    pub frame_flags: u8,
    pub addr: u32,
    pub counter: u32,
    pub ack: UplinkAck,
    /// `ack` bits 6 and 7.
    pub ack_tail: AckTail,
    pub plaintext: Vec<u8>,
}

/// The fixed part of `EVT_TWR_UPLINK`, before the plaintext.
const UPLINK_FIXED: usize = 8 + 2 + 1 + 1 + 4 + 4 + 1 + 1;

impl TwrUplink {
    pub fn decode(b: &[u8]) -> Option<Self> {
        let len = *b.get(UPLINK_FIXED - 1)? as usize;
        let plaintext = b.get(UPLINK_FIXED..UPLINK_FIXED + len)?.to_vec();
        Some(TwrUplink {
            t_ms: u64le(b, 0)?,
            rssi_dbm: i16::from_le_bytes([b[8], b[9]]),
            snr_q_db: b[10] as i8,
            frame_flags: b[11],
            addr: u32le(b, 12)?,
            counter: u32le(b, 16)?,
            ack: UplinkAck::from_byte(b[20]),
            ack_tail: AckTail::from_byte(b[20]),
            plaintext,
        })
    }

    /// The SNR in dB.
    pub fn snr_db(&self) -> f64 {
        f64::from(self.snr_q_db) / 4.0
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(UPLINK_FIXED + self.plaintext.len());
        b.extend_from_slice(&self.t_ms.to_le_bytes());
        b.extend_from_slice(&self.rssi_dbm.to_le_bytes());
        b.push(self.snr_q_db as u8);
        b.push(self.frame_flags);
        b.extend_from_slice(&self.addr.to_le_bytes());
        b.extend_from_slice(&self.counter.to_le_bytes());
        let code = match self.ack {
            UplinkAck::NotConfirmed => 0,
            UplinkAck::Sent => 1,
            UplinkAck::SentPending => 2,
            UplinkAck::Late => 3,
            UplinkAck::NoCounter => 4,
            UplinkAck::SuppressedRingFull => 5,
            UplinkAck::Unknown(o) => o & ACK_CODE_MASK,
        };
        b.push(code | self.ack_tail.bits());
        b.push(self.plaintext.len().min(u8::MAX as usize) as u8);
        b.extend_from_slice(&self.plaintext);
        b
    }
}

/// `EVT_TWR_TX.outcome` (§2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxOutcome {
    /// Confirmed, and the Node's ACK was seen.
    Delivered,
    /// Unconfirmed, on air.
    Sent,
    NotDelivered,
    Expired,
    RadioErr,
    /// The gateway had no counter to seal it with.
    NoCounter,
    Unknown(u8),
}

impl TxOutcome {
    fn from_byte(b: u8) -> Self {
        match b {
            0 => TxOutcome::Delivered,
            1 => TxOutcome::Sent,
            2 => TxOutcome::NotDelivered,
            3 => TxOutcome::Expired,
            4 => TxOutcome::RadioErr,
            5 => TxOutcome::NoCounter,
            o => TxOutcome::Unknown(o),
        }
    }

    fn to_byte(self) -> u8 {
        match self {
            TxOutcome::Delivered => 0,
            TxOutcome::Sent => 1,
            TxOutcome::NotDelivered => 2,
            TxOutcome::Expired => 3,
            TxOutcome::RadioErr => 4,
            TxOutcome::NoCounter => 5,
            TxOutcome::Unknown(o) => o,
        }
    }
}

/// `EVT_TWR_TX`: what became of one queued downlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TwrTx {
    pub addr: u32,
    pub item: u16,
    pub outcome: TxOutcome,
    pub gw_counter: u32,
    /// The header counter of the Node's ACK frame, `0` if none. In the Node's own uplink
    /// counter space (SC 2026-09-28): its next uplink is one above, so it is spent.
    pub node_ack_counter: u32,
    /// `None` for the wire's `0x7F`.
    pub ack_rssi: Option<i8>,
}

impl TwrTx {
    pub const LEN: usize = 4 + 2 + 1 + 4 + 4 + 1;

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < Self::LEN {
            return None;
        }
        let rssi = b[15] as i8;
        Some(TwrTx {
            addr: u32le(b, 0)?,
            item: u16le(b, 4)?,
            outcome: TxOutcome::from_byte(b[6]),
            gw_counter: u32le(b, 7)?,
            node_ack_counter: u32le(b, 11)?,
            ack_rssi: (rssi != 0x7F).then_some(rssi),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(Self::LEN);
        b.extend_from_slice(&self.addr.to_le_bytes());
        b.extend_from_slice(&self.item.to_le_bytes());
        b.push(self.outcome.to_byte());
        b.extend_from_slice(&self.gw_counter.to_le_bytes());
        b.extend_from_slice(&self.node_ack_counter.to_le_bytes());
        b.push(self.ack_rssi.unwrap_or(0x7F) as u8);
        b
    }
}

/// `EVT_TWR_CTR_LOW`: fewer than a quarter of the block is left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TwrCtrLow {
    pub next: u32,
    pub last: u32,
}

impl TwrCtrLow {
    pub fn decode(b: &[u8]) -> Option<Self> {
        Some(TwrCtrLow {
            next: u32le(b, 0)?,
            last: u32le(b, 4)?,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = self.next.to_le_bytes().to_vec();
        b.extend_from_slice(&self.last.to_le_bytes());
        b
    }
}

/// One row of a `TWR_NODE_LIST` response. Never carries a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeListEntry {
    pub addr: u32,
    pub last_seen: u32,
    pub flags: u8,
}

/// One `TWR_NODE_LIST` response page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeListPage {
    pub total: u16,
    /// Where the next page starts, `None` at the end (`0xFFFF`).
    pub next: Option<u16>,
    pub entries: Vec<NodeListEntry>,
}

impl NodeListPage {
    pub fn parse(b: &[u8]) -> Option<Self> {
        let total = u16le(b, 0)?;
        let next = u16le(b, 2)?;
        let n = *b.get(4)? as usize;
        let entries = (0..n)
            .map(|i| {
                let o = 5 + i * 9;
                Some(NodeListEntry {
                    addr: u32le(b, o)?,
                    last_seen: u32le(b, o + 4)?,
                    flags: *b.get(o + 8)?,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(NodeListPage {
            total,
            next: (next != u16::MAX).then_some(next),
            entries,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The command bodies byte for byte as §2.1 lays them out, little-endian throughout.
    #[test]
    fn commands_encode_as_the_contract_lays_them_out() {
        let start = TwrCmd::Start {
            net_id: 0x0403_0201,
            turnaround_ms: 20,
            dl_gap_ms: 25,
            flags: START_ACK_ENABLED,
        };
        assert_eq!(start.cmd(), 0x10);
        assert_eq!(start.encode().expect("start"), [1, 2, 3, 4, 20, 25, 1]);

        let add = TwrCmd::NodeAdd {
            addr: 0x0000_0413,
            session_key: [0xAA; 16],
            last_seen: 7,
            flags: NODE_HOME,
        };
        let body = add.encode().expect("add");
        assert_eq!(&body[..4], &[0x13, 0x04, 0, 0]);
        assert_eq!(&body[4..20], &[0xAA; 16]);
        assert_eq!(&body[20..], &[7, 0, 0, 0, 1]);

        assert_eq!(
            TwrCmd::CtrBlock {
                first: 1,
                last: 4096
            }
            .encode()
            .expect("ctr"),
            [1, 0, 0, 0, 0, 0x10, 0, 0]
        );
        assert_eq!(
            TwrCmd::QueuePush {
                addr: 1,
                item: 0x0102,
                ttl_s: 3600,
                flags: PUSH_CONFIRMED,
                plaintext: vec![0x81, 86, 1],
            }
            .encode()
            .expect("push"),
            [1, 0, 0, 0, 2, 1, 0x10, 0x0E, 1, 3, 0x81, 86, 1]
        );
        assert_eq!(
            TwrCmd::QueueDrop { addr: 1, item: 0 }
                .encode()
                .expect("drop"),
            [1, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            TwrCmd::NodeList { start: 24 }.encode().expect("list"),
            [24, 0]
        );
        assert_eq!(TwrCmd::NodeRemove { addr: 2 }.cmd(), 0x12);

        let restore = TwrCmd::NodeAdd {
            addr: 0x0000_0413,
            session_key: [0xAA; 16],
            last_seen: 7,
            flags: NODE_HOME | NODE_KEEP_NEWER,
        };
        assert_eq!(restore.encode().expect("restore")[24], 0x03);

        let ack = TwrCmd::EvtAck { seq: 0xFE };
        assert_eq!(ack.cmd(), 0x18);
        assert_eq!(ack.name(), "TWR_EVT_ACK");
        assert_eq!(ack.encode().expect("ack"), [0xFE]);
        assert!(!ack.expects_rsp(), "TWR_EVT_ACK has no RSP");
        assert!(start.expects_rsp());
    }

    /// What the gateway would answer `BAD_PARAM` for, the central refuses before sending.
    #[test]
    fn a_body_the_gateway_would_refuse_is_refused_here() {
        let refused = [
            TwrCmd::Start {
                net_id: 1,
                turnaround_ms: 10,
                dl_gap_ms: 20,
                flags: 0,
            },
            TwrCmd::NodeAdd {
                addr: 0,
                session_key: [0; 16],
                last_seen: 0,
                flags: 1,
            },
            TwrCmd::CtrBlock { first: 0, last: 5 },
            TwrCmd::CtrBlock { first: 9, last: 5 },
            TwrCmd::CtrBlock {
                first: 1,
                last: u32::MAX,
            },
            TwrCmd::QueuePush {
                addr: 1,
                item: 0,
                ttl_s: 1,
                flags: 0,
                plaintext: vec![1],
            },
            TwrCmd::QueuePush {
                addr: 1,
                item: 1,
                ttl_s: 1,
                flags: 0,
                plaintext: vec![0; MAX_PAYLOAD + 1],
            },
        ];
        for c in refused {
            let err = c.encode().expect_err("refused");
            assert_eq!(err.kind(), Kind::InvalidArgument, "{c:?}");
        }
        // The largest payload the frame carries still goes.
        assert!(TwrCmd::QueuePush {
            addr: 1,
            item: 1,
            ttl_s: 1,
            flags: 0,
            plaintext: vec![0; MAX_PAYLOAD],
        }
        .encode()
        .is_ok());
    }

    #[test]
    fn a_node_add_never_prints_its_key() {
        let add = TwrCmd::NodeAdd {
            addr: 1,
            session_key: [0x5A; 16],
            last_seen: 0,
            flags: 1,
        };
        let shown = format!("{add:?}");
        assert!(shown.contains("redacted"), "{shown}");
        assert!(!shown.to_lowercase().contains("5a5a"), "{shown}");
    }

    #[test]
    fn events_round_trip_and_a_truncated_one_is_none() {
        let up = TwrUplink {
            t_ms: 123_456,
            rssi_dbm: -97,
            snr_q_db: -30,
            frame_flags: 1,
            addr: 0x0413,
            counter: 42,
            ack: UplinkAck::SentPending,
            ack_tail: AckTail::default(),
            plaintext: vec![0x81, 2, 0xAB],
        };
        let b = up.encode();
        assert_eq!(b.len(), UPLINK_FIXED + 3);
        assert_eq!(TwrUplink::decode(&b), Some(up.clone()));
        assert_eq!(TwrUplink::decode(&b[..b.len() - 1]), None);
        assert!((up.snr_db() + 7.5).abs() < f64::EPSILON);

        let tx = TwrTx {
            addr: 9,
            item: 3,
            outcome: TxOutcome::Delivered,
            gw_counter: 11,
            node_ack_counter: 43,
            ack_rssi: Some(-80),
        };
        assert_eq!(TwrTx::decode(&tx.encode()), Some(tx));
        let mut none = tx.encode();
        none[15] = 0x7F;
        assert_eq!(TwrTx::decode(&none).expect("tx").ack_rssi, None);
        assert_eq!(TwrTx::decode(&none[..10]), None);

        let low = TwrCtrLow {
            next: 3073,
            last: 4096,
        };
        assert_eq!(TwrCtrLow::decode(&low.encode()), Some(low));
    }

    #[test]
    fn a_node_list_page_parses() {
        let mut page = vec![2, 0, 0xFF, 0xFF, 1];
        page.extend_from_slice(&0x0413u32.to_le_bytes());
        page.extend_from_slice(&42u32.to_le_bytes());
        page.push(1);
        let p = NodeListPage::parse(&page).expect("page");
        assert_eq!(p.total, 2);
        assert_eq!(p.next, None);
        assert_eq!(
            p.entries,
            [NodeListEntry {
                addr: 0x0413,
                last_seen: 42,
                flags: 1
            }]
        );
        assert_eq!(NodeListPage::parse(&page[..page.len() - 1]), None);
    }

    /// 0.3.0's strict `BAD_PARAM` (link doc §2): the central refuses before sending an
    /// undefined flag bit, a zero `item` or `ttl_s`, or an empty payload.
    #[test]
    fn what_the_gateway_would_refuse_is_never_encoded() {
        let push = |item: u16, ttl_s: u16, flags: u8, plaintext: Vec<u8>| TwrCmd::QueuePush {
            addr: 1,
            item,
            ttl_s,
            flags,
            plaintext,
        };
        assert!(push(1, 600, PUSH_CONFIRMED, vec![0x91]).encode().is_ok());
        for bad in [
            push(0, 600, 0, vec![0x91]),
            push(1, 0, 0, vec![0x91]),
            push(1, 600, 0x02, vec![0x91]),
            push(1, 600, 0, vec![]),
            TwrCmd::NodeAdd {
                addr: 1,
                session_key: [0; 16],
                last_seen: 0,
                flags: NODE_HOME | NODE_KEEP_NEWER | 0x04,
            },
            TwrCmd::Start {
                net_id: 7,
                turnaround_ms: 20,
                dl_gap_ms: 20,
                flags: START_ACK_ENABLED | 0x80,
            },
        ] {
            let err = bad.encode().expect_err("refused");
            assert_eq!(err.kind(), Kind::InvalidArgument, "{bad:?}");
        }
    }

    /// `TWR_TIME_SYNC` (§4): `now_ms u64 ‖ unix_ms u64`, little-endian, answered by an `RSP`.
    #[test]
    fn time_sync_encodes_as_the_contract_lays_it_out() {
        let sync = TwrCmd::TimeSync {
            now_ms: 0x0102_0304_0506_0708,
            unix_ms: 1_800_000_000_250,
        };
        assert_eq!(sync.cmd(), 0x19);
        assert_eq!(sync.name(), "TWR_TIME_SYNC");
        assert!(sync.expects_rsp(), "TWR_TIME_SYNC is answered");
        let body = sync.encode().expect("time sync");
        assert_eq!(body.len(), 16);
        assert_eq!(&body[..8], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(
            u64::from_le_bytes(body[8..].try_into().expect("8 B")),
            1_800_000_000_250
        );
        assert_eq!(u64le(&body, 0), Some(0x0102_0304_0506_0708));
        assert_eq!(u64le(&body, 8), Some(1_800_000_000_250));
        let shown = format!("{sync:?}");
        assert!(
            shown.contains("TimeSync") && shown.contains("1800000000250"),
            "{shown}"
        );
    }

    #[test]
    fn uplink_ack_codes_follow_the_link_doc() {
        for (b, want) in [
            (3, UplinkAck::Late),
            (4, UplinkAck::NoCounter),
            (5, UplinkAck::SuppressedRingFull),
        ] {
            assert_eq!(UplinkAck::from_byte(b), want);
        }
    }

    /// #470 §13.5: bits 6 and 7 are the ACK tail, the code is bits 0..5 — a tail never turns
    /// a known code into an unknown one, nor ack 4 into anything but NoCounter.
    #[test]
    fn uplink_ack_bits_6_and_7_are_the_tail() {
        for (b, code, sent, time) in [
            (0x01, UplinkAck::Sent, false, false),
            (0x81, UplinkAck::Sent, true, false),
            (0xC1, UplinkAck::Sent, true, true),
            (0xC2, UplinkAck::SentPending, true, true),
            (0x84, UplinkAck::NoCounter, true, false),
            (0x85, UplinkAck::SuppressedRingFull, true, false),
            (0xBF, UplinkAck::Unknown(0x3F), true, false),
        ] {
            assert_eq!(UplinkAck::from_byte(b), code, "{b:#04x}");
            assert_eq!(AckTail::from_byte(b), AckTail { sent, time }, "{b:#04x}");
            let up = TwrUplink {
                t_ms: 1,
                rssi_dbm: -1,
                snr_q_db: 0,
                frame_flags: 1,
                addr: 1,
                counter: 1,
                ack: code,
                ack_tail: AckTail { sent, time },
                plaintext: vec![],
            };
            let wire = up.encode();
            assert_eq!(wire[20], b, "re-encoded");
            assert_eq!(TwrUplink::decode(&wire), Some(up));
        }
    }
}
