//! The modem I/O abstraction — the seam between the P2P runtime and the northbridge.
//!
//! `plan/control/radio/p2p_serial_transport.md`: the runtime drives the modem through a small
//! trait so the session engine can be exercised against a scripted [`crate::p2p::fake::FakeModem`]
//! exactly as the LoRaWAN arm tests against [`crate::lorawan::fake`]. [`SerialModem`] is the real
//! implementation over [`crate::p2p::uart::Northbridge`]: one in-flight command at a time, its
//! `RSP` matched by the echoed `seq`, and any unsolicited `EVT_*` that arrives while awaiting a
//! response stashed on a queue so it is delivered by [`Modem::next_event`] rather than dropped.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::p2p::hdlc::Frame;
use crate::p2p::modem::{self, Event, RadioConfig, Status, TxSchedule};
use crate::p2p::tower::link::TwrCmd;
use crate::p2p::uart::Northbridge;

/// A command's response must arrive within this budget, or it is a [`Kind::Timeout`]. `GET_INFO`
/// / `SET_RADIO_CONFIG` / `TWR_*` are near-instant; `TX_SCHEDULE`'s *delivery* outcome is the
/// later `EVT_TX_DONE`, not this RSP.
const RSP_TIMEOUT: Duration = Duration::from_millis(800);

/// The gateway state `GET_INFO` reports (`p2p_tower_gateway.md` §2, amendment 9).
pub mod gateway_state {
    /// Booted, no radio config yet.
    pub const BOOT: u8 = 0;
    /// `SET_RADIO_CONFIG` taken, not receiving.
    pub const CONFIGURED: u8 = 1;
    /// `TWR_START` taken: receiving, ACKing and sending downlinks.
    pub const GATEWAY: u8 = 2;

    /// The status document's spelling.
    pub const fn name(state: u8) -> &'static str {
        match state {
            BOOT => "boot",
            CONFIGURED => "configured",
            GATEWAY => "gateway",
            _ => "unknown",
        }
    }
}

/// `GET_INFO` `build_flags`, by bit.
pub mod build_flags {
    /// A bench image that bridges its console over RTT.
    pub const BENCH_RTT_BRIDGE: u8 = 1 << 0;
    /// A debug build.
    pub const DEBUG: u8 = 1 << 1;
}

/// The `GET_INFO` counters, in the reply's order and with the names of the #470 link doc §3.1
/// (p2p_tower_gateway.md §7).
pub const COUNTER_NAMES: [&str; 25] = [
    "rx_total",
    "rx_fresh",
    "rx_dup",
    "rx_replay",
    "rx_mic_fail",
    "rx_unknown_src",
    "rx_join_fwd",
    "rx_not_for_us",
    "rx_bad_hdr",
    "radio_crc_err",
    "radio_hdr_err",
    "rx_overrun",
    "rx_rearm_fail",
    "tx_total",
    "tx_radio_err",
    "acks_sent",
    "acks_late",
    "acks_suppressed",
    "dl_sent",
    "dl_delivered",
    "dl_not_delivered",
    "dl_expired",
    "link_crc_err",
    "link_err",
    "evt_resent",
];

/// The two counters a Northbridge that answers `LinkCheckReq` / `TimeReq` in its ACK appends
/// to `GET_INFO` (`p2p_tower_gateway.md` §3, bytes 172 and 176; #470 §13.5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AckCtrlCounters {
    /// ACKs sent with a `CTRL` tail since boot.
    pub sent: u32,
    /// `TimeReq`s left without a `TimeAns` in the ACK: no `TWR_TIME_SYNC` anchor held.
    pub time_no_anchor: u32,
}

/// The parsed `GET_INFO` reply of the gateway image (`p2p_tower_gateway.md` §3): 180 B after
/// the status byte, in this order; 172 B and 171 B replies of older images are accepted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModemInfo {
    /// `fw_ver[3]`: major, minor, patch.
    pub fw: [u8; 3],
    /// The first 4 bytes of the image's commit, in display order.
    pub git_sha: [u8; 4],
    /// The current boot's `EVT_BOOT` `reset_cause`, kept by the Northbridge.
    pub reset_cause: u8,
    /// [`build_flags`].
    pub build_flags: u8,
    /// `k_uptime` in ms: the clock of every `t_ms` in the events and in `TX_SCHEDULE`.
    pub now_ms: u64,
    /// [`gateway_state`].
    pub state: u8,
    /// The `net_id` of the running `TWR_START`, `0` before one.
    pub net_id: u32,
    pub turnaround_ms: u8,
    pub dl_gap_ms: u8,
    /// `0` = LoRa.
    pub modulation: u8,
    /// The radio config the Northbridge holds, `SET_RADIO_CONFIG`'s body.
    pub radio: RadioConfig,
    /// Sticky since boot, see [`crate::p2p::northbridge::err_flags`].
    pub err_flags: u32,
    /// The §2.2 code of the most recent error, `0` for none.
    pub last_err_code: u8,
    pub last_err_age_s: u32,
    /// [`COUNTER_NAMES`], in that order.
    pub counters: [u32; 25],
    pub ack_turnaround_last_us: u16,
    pub ack_turnaround_max_us: u16,
    pub airtime_ms_last_hour: u32,
    pub peers: u16,
    pub peers_max: u16,
    pub queue_used: u16,
    pub queue_max: u16,
    pub evt_ring_used: u8,
    pub evt_ring_max: u8,
    /// The gateway's next and last TX counter in its current block.
    pub ctr_next: u32,
    pub ctr_last: u32,
    /// The `seq` the next ring event the gateway delivers will carry: the oldest unacked one,
    /// or with an empty ring the next it hands out (1 after a boot). Byte 171, from
    /// Northbridge 0.3.1 final (link doc §3.1, §4.1); `None` in a 171 B reply.
    pub evt_head_seq: Option<u8>,
    /// Bytes 172..180, from a Northbridge that answers `LinkCheckReq` / `TimeReq` in its ACK
    /// (#470 §13.5); `None` in a 172 B or 171 B reply. Its presence is the switch: with it the
    /// central queues no `LinkCheckAns` / `TimeAns` and keeps the Northbridge's time anchor
    /// (`TWR_TIME_SYNC`); without it, it answers them itself as in P2 (§10.4, §17).
    pub ack_ctrl: Option<AckCtrlCounters>,
}

impl ModemInfo {
    /// The shortest reply accepted, after the status byte: Northbridge 0.3.0 / early 0.3.1.
    pub const LEN: usize = 171;
    /// With `evt_head_seq` (Northbridge 0.3.1 final).
    pub const LEN_HEAD: usize = 172;
    /// With the ACK-tail counters: the current image (#470 §13.5).
    pub const LEN_ACK_CTRL: usize = 180;

    /// Whether this Northbridge answers `LinkCheckReq` / `TimeReq` in its ACK and takes a
    /// `TWR_TIME_SYNC`: it reported the 180 B status.
    pub fn answers_in_ack(&self) -> bool {
        self.ack_ctrl.is_some()
    }

    /// One counter by its [`COUNTER_NAMES`] name, `0` for a name not in the list.
    pub fn counter(&self, name: &str) -> u32 {
        COUNTER_NAMES
            .iter()
            .position(|n| *n == name)
            .map_or(0, |i| self.counters[i])
    }

    /// Parse the `GET_INFO` RSP payload (everything after the status byte).
    ///
    /// A reply shorter than [`Self::LEN`] is refused rather than read as zeros: the Northbridge
    /// image and the Hub software ship together (amendment 9, flag day), so a short one is a
    /// Northbridge on an image this central cannot drive. A reply between two known lengths
    /// is read as the shorter one; bytes past [`Self::LEN_ACK_CTRL`] are ignored.
    pub fn parse(payload: &[u8]) -> Result<Self> {
        if payload.len() < Self::LEN {
            return Err(Error::new(
                Kind::Transport,
                Transport::P2p,
                Component::NorthbridgeFirmware,
                format!(
                    "GET_INFO answered {} B, the gateway link's reply is {} B — is the \
                     Northbridge on the image this Hub software ships with?",
                    payload.len(),
                    Self::LEN
                ),
            ));
        }
        let b = payload;
        let u16le = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let u32le = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let mut now = [0u8; 8];
        now.copy_from_slice(&b[9..17]);
        let mut counters = [0u32; 25];
        for (i, c) in counters.iter_mut().enumerate() {
            *c = u32le(45 + 4 * i);
        }
        Ok(ModemInfo {
            fw: [b[0], b[1], b[2]],
            git_sha: [b[3], b[4], b[5], b[6]],
            reset_cause: b[7],
            build_flags: b[8],
            now_ms: u64::from_le_bytes(now),
            state: b[17],
            net_id: u32le(18),
            turnaround_ms: b[22],
            dl_gap_ms: b[23],
            modulation: b[24],
            radio: RadioConfig {
                freq_hz: u32le(25),
                sf: b[29],
                bw: b[30],
                cr: b[31],
                preamble: u16le(32),
                tx_power_dbm: b[34] as i8,
                flags: b[35],
            },
            err_flags: u32le(36),
            last_err_code: b[40],
            last_err_age_s: u32le(41),
            counters,
            ack_turnaround_last_us: u16le(145),
            ack_turnaround_max_us: u16le(147),
            airtime_ms_last_hour: u32le(149),
            peers: u16le(153),
            peers_max: u16le(155),
            queue_used: u16le(157),
            queue_max: u16le(159),
            evt_ring_used: b[161],
            evt_ring_max: b[162],
            ctr_next: u32le(163),
            ctr_last: u32le(167),
            evt_head_seq: b.get(Self::LEN).copied(),
            ack_ctrl: (b.len() >= Self::LEN_ACK_CTRL).then(|| AckCtrlCounters {
                sent: u32le(172),
                time_no_anchor: u32le(176),
            }),
        })
    }

    /// The reply's bytes — the inverse of [`Self::parse`], for the fakes and the tests.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(Self::LEN);
        b.extend_from_slice(&self.fw);
        b.extend_from_slice(&self.git_sha);
        b.push(self.reset_cause);
        b.push(self.build_flags);
        b.extend_from_slice(&self.now_ms.to_le_bytes());
        b.push(self.state);
        b.extend_from_slice(&self.net_id.to_le_bytes());
        b.push(self.turnaround_ms);
        b.push(self.dl_gap_ms);
        b.push(self.modulation);
        b.extend_from_slice(&self.radio.encode());
        b.extend_from_slice(&self.err_flags.to_le_bytes());
        b.push(self.last_err_code);
        b.extend_from_slice(&self.last_err_age_s.to_le_bytes());
        for c in self.counters {
            b.extend_from_slice(&c.to_le_bytes());
        }
        b.extend_from_slice(&self.ack_turnaround_last_us.to_le_bytes());
        b.extend_from_slice(&self.ack_turnaround_max_us.to_le_bytes());
        b.extend_from_slice(&self.airtime_ms_last_hour.to_le_bytes());
        for v in [self.peers, self.peers_max, self.queue_used, self.queue_max] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.push(self.evt_ring_used);
        b.push(self.evt_ring_max);
        b.extend_from_slice(&self.ctr_next.to_le_bytes());
        b.extend_from_slice(&self.ctr_last.to_le_bytes());
        b.extend(self.evt_head_seq);
        // The counters follow the head; without one there is no byte 172 to put them at.
        if let (Some(_), Some(a)) = (self.evt_head_seq, self.ack_ctrl) {
            b.extend_from_slice(&a.sent.to_le_bytes());
            b.extend_from_slice(&a.time_no_anchor.to_le_bytes());
        }
        b
    }
}

/// What the runtime asks of a modem. Async because the real one is serial I/O; `Send` so the
/// runtime task can move it across `.await` points.
#[async_trait::async_trait]
pub trait Modem: Send {
    /// `GET_INFO` — also the health probe (a round-trip proves the link is live).
    async fn get_info(&mut self) -> Result<ModemInfo>;
    async fn set_radio_config(&mut self, cfg: &RadioConfig) -> Result<()>;
    async fn rx_stop(&mut self) -> Result<()>;
    /// Schedule a frame. Returns on the `RSP` (`Ok` = accepted; `BUSY`/`LATE`/… map through
    /// [`Status::into_result`]); the *delivery* outcome arrives later as an `EVT_TX_DONE`.
    async fn tx_schedule(&mut self, tx: &TxSchedule) -> Result<()>;
    async fn tx_cancel(&mut self, tx_id: u8) -> Result<()>;
    /// One `TWR_*` command of the gateway link (`p2p_tower_gateway.md` §2.1). Returns the
    /// `RSP` payload after the status byte; a non-OK status maps through
    /// [`Status::into_result`]. A command without an `RSP` (`TWR_EVT_ACK`) returns once written,
    /// with an empty payload.
    async fn twr(&mut self, cmd: &TwrCmd) -> Result<Vec<u8>>;
    /// Block until the modem delivers an unsolicited event.
    async fn next_event(&mut self) -> Result<Event>;
    /// The far host's clock when the last command's `RSP` left its UART, Unix ms — a Client
    /// Hub's stamp on the gateway plane (`p2p_cluster_gateway.md` §6). `None` for a local
    /// Northbridge: the central's own clock is the one.
    fn last_rsp_host_unix_ms(&self) -> Option<u64> {
        None
    }
}

/// A boxed modem is a modem, so the central can drive a local and a remote one alike.
#[async_trait::async_trait]
impl<T: Modem + ?Sized> Modem for Box<T> {
    async fn get_info(&mut self) -> Result<ModemInfo> {
        (**self).get_info().await
    }
    async fn set_radio_config(&mut self, cfg: &RadioConfig) -> Result<()> {
        (**self).set_radio_config(cfg).await
    }
    async fn rx_stop(&mut self) -> Result<()> {
        (**self).rx_stop().await
    }
    async fn tx_schedule(&mut self, tx: &TxSchedule) -> Result<()> {
        (**self).tx_schedule(tx).await
    }
    async fn tx_cancel(&mut self, tx_id: u8) -> Result<()> {
        (**self).tx_cancel(tx_id).await
    }
    async fn twr(&mut self, cmd: &TwrCmd) -> Result<Vec<u8>> {
        (**self).twr(cmd).await
    }
    async fn next_event(&mut self) -> Result<Event> {
        (**self).next_event().await
    }
    fn last_rsp_host_unix_ms(&self) -> Option<u64> {
        (**self).last_rsp_host_unix_ms()
    }
}

/// The byte side of a link to a Northbridge: frames out, frames in. The `ttyAMA3` UART
/// ([`Northbridge`]) on a Server, the gateway plane ([`crate::p2p::remote::PlaneLink`]) for a
/// Client Hub's Northbridge (`p2p_cluster_gateway.md` §4).
#[async_trait::async_trait]
pub trait LinkIo: Send {
    async fn write_frame(&mut self, frame_type: u8, seq: u8, body: &[u8]) -> Result<()>;
    /// Block until at least one frame arrives. Cancel-safe: a frame is never half-taken.
    async fn read_frames(&mut self) -> Result<Vec<Frame>>;
    /// [`LinkIo::read_frames`] bounded by a deadline — `Kind::Timeout` on silence.
    async fn read_frames_within(&mut self, deadline: Duration) -> Result<Vec<Frame>>;
    /// The far host's clock when the last frame left its UART, Unix ms: the Client's stamp
    /// on the plane. `None` for a local UART, where this host's clock is the one.
    fn last_frame_host_unix_ms(&self) -> Option<u64> {
        None
    }
}

#[async_trait::async_trait]
impl LinkIo for Northbridge {
    async fn write_frame(&mut self, frame_type: u8, seq: u8, body: &[u8]) -> Result<()> {
        Northbridge::write_frame(self, frame_type, seq, body).await
    }

    async fn read_frames(&mut self) -> Result<Vec<Frame>> {
        Northbridge::read_frames(self).await
    }

    async fn read_frames_within(&mut self, deadline: Duration) -> Result<Vec<Frame>> {
        Northbridge::read_frames_within(self, deadline).await
    }
}

/// A Northbridge driven over a [`LinkIo`]: one in-flight command at a time, its `RSP` matched by
/// the echoed `seq`, and any unsolicited `EVT_*` that arrives while awaiting a response stashed
/// for [`Modem::next_event`].
#[derive(Debug)]
pub struct FramedModem<L> {
    nb: L,
    seq: u8,
    /// `EVT_*` frames read while awaiting a command's `RSP`, replayed by `next_event`.
    pending: VecDeque<Frame>,
    /// How long a command waits for its `RSP`.
    rsp_timeout: Duration,
    /// The far host's stamp of the last `RSP` a command took, if the link carries one.
    last_rsp_host_unix_ms: Option<u64>,
}

/// The real northbridge, driven over `ttyAMA3`.
pub type SerialModem = FramedModem<Northbridge>;

impl SerialModem {
    pub fn new(nb: Northbridge) -> Self {
        FramedModem::with_timeout(nb, RSP_TIMEOUT)
    }

    /// Open the configured link (`PROXIMOS_P2P_UART`, default `/dev/ttyAMA3`).
    pub fn open_default() -> Result<Self> {
        Ok(SerialModem::new(Northbridge::open_default()?))
    }
}

impl<L: LinkIo> FramedModem<L> {
    pub fn with_timeout(nb: L, rsp_timeout: Duration) -> Self {
        FramedModem {
            nb,
            seq: 0,
            pending: VecDeque::new(),
            rsp_timeout,
            last_rsp_host_unix_ms: None,
        }
    }

    fn next_seq(&mut self) -> u8 {
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    /// Send one command and return its `RSP` payload (the bytes after the status byte), mapping a
    /// non-OK status through the taxonomy. Any `EVT_*` seen while waiting is queued for
    /// `next_event`, never dropped.
    async fn command(&mut self, cmd: u8, body: &[u8], doing: &str) -> Result<Vec<u8>> {
        let seq = self.next_seq();
        self.nb.write_frame(cmd, seq, body).await?;
        let want = modem::RSP_FLAG | cmd;
        let start = Instant::now();
        let budget = self.rsp_timeout;
        loop {
            let remaining = budget
                .checked_sub(start.elapsed())
                .filter(|d| !d.is_zero())
                .ok_or_else(|| {
                    Error::new(
                        Kind::Timeout,
                        Transport::P2p,
                        Component::NorthbridgeFirmware,
                        format!("{doing}: no RSP for command {cmd:#04x} within {budget:?}"),
                    )
                })?;
            let frames = self.nb.read_frames_within(remaining).await?;
            let stamp = self.nb.last_frame_host_unix_ms();
            for f in frames {
                if f.frame_type == want && f.seq == seq {
                    self.last_rsp_host_unix_ms = stamp;
                    let status = Status::from_byte(f.body.first().copied().unwrap_or(5));
                    status.into_result(doing)?;
                    return Ok(f.body.get(1..).unwrap_or(&[]).to_vec());
                }
                self.pending.push_back(f);
            }
        }
    }
}

#[async_trait::async_trait]
impl<L: LinkIo> Modem for FramedModem<L> {
    async fn get_info(&mut self) -> Result<ModemInfo> {
        let payload = self.command(modem::cmd::GET_INFO, &[], "GET_INFO").await?;
        ModemInfo::parse(&payload)
    }

    async fn set_radio_config(&mut self, cfg: &RadioConfig) -> Result<()> {
        self.command(
            modem::cmd::SET_RADIO_CONFIG,
            &cfg.encode(),
            "SET_RADIO_CONFIG",
        )
        .await
        .map(|_| ())
    }

    async fn rx_stop(&mut self) -> Result<()> {
        self.command(modem::cmd::RX_STOP, &[], "RX_STOP")
            .await
            .map(|_| ())
    }

    async fn tx_schedule(&mut self, tx: &TxSchedule) -> Result<()> {
        let body = tx.encode()?;
        self.command(modem::cmd::TX_SCHEDULE, &body, "TX_SCHEDULE")
            .await
            .map(|_| ())
    }

    async fn tx_cancel(&mut self, tx_id: u8) -> Result<()> {
        self.command(modem::cmd::TX_CANCEL, &[tx_id], "TX_CANCEL")
            .await
            .map(|_| ())
    }

    async fn twr(&mut self, cmd: &TwrCmd) -> Result<Vec<u8>> {
        let body = cmd.encode()?;
        if !cmd.expects_rsp() {
            // `TWR_EVT_ACK`: written, not answered (link doc §5.1). A lost one is repaired by
            // the gateway's resend and the next ack, so there is nothing to wait for.
            let seq = self.next_seq();
            self.nb.write_frame(cmd.cmd(), seq, &body).await?;
            return Ok(Vec::new());
        }
        self.command(cmd.cmd(), &body, cmd.name()).await
    }

    async fn next_event(&mut self) -> Result<Event> {
        loop {
            if let Some(f) = self.pending.pop_front() {
                if let Some(ev) = Event::from_frame(&f) {
                    return Ok(ev);
                }
                // A stray RSP with no in-flight command, or an undecodable frame: skip it.
                continue;
            }
            let frames = self.nb.read_frames().await?;
            self.pending.extend(frames);
        }
    }

    fn last_rsp_host_unix_ms(&self) -> Option<u64> {
        self.last_rsp_host_unix_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frozen layout, byte by byte: each field carries a value that tells its offset.
    #[test]
    fn get_info_parses_the_171_172_and_180_byte_layouts() {
        let mut b = vec![0u8; ModemInfo::LEN];
        b[..3].copy_from_slice(&[0, 3, 1]);
        b[3..7].copy_from_slice(&[0xc0, 0x5b, 0x8d, 0x3e]);
        b[7] = 0x10;
        b[8] = 0x01;
        b[9..17].copy_from_slice(&3_600_000u64.to_le_bytes());
        b[17] = gateway_state::GATEWAY;
        b[18..22].copy_from_slice(&0x6d24_8a62u32.to_le_bytes());
        b[22] = 20;
        b[23] = 21;
        b[24] = 0;
        b[25..29].copy_from_slice(&869_525_000u32.to_le_bytes());
        b[29] = 9;
        b[30] = 1;
        b[31] = 2;
        b[32..34].copy_from_slice(&12u16.to_le_bytes());
        b[34] = (-3i8) as u8;
        b[35] = 0x02;
        b[36..40].copy_from_slice(&0x8000_0001u32.to_le_bytes());
        b[40] = 3;
        b[41..45].copy_from_slice(&77u32.to_le_bytes());
        for i in 0..25u32 {
            let o = 45 + 4 * i as usize;
            b[o..o + 4].copy_from_slice(&(1000 + i).to_le_bytes());
        }
        b[145..147].copy_from_slice(&1500u16.to_le_bytes());
        b[147..149].copy_from_slice(&9000u16.to_le_bytes());
        b[149..153].copy_from_slice(&4321u32.to_le_bytes());
        b[153..155].copy_from_slice(&2u16.to_le_bytes());
        b[155..157].copy_from_slice(&64u16.to_le_bytes());
        b[157..159].copy_from_slice(&1u16.to_le_bytes());
        b[159..161].copy_from_slice(&32u16.to_le_bytes());
        b[161] = 3;
        b[162] = 16;
        b[163..167].copy_from_slice(&4097u32.to_le_bytes());
        b[167..171].copy_from_slice(&8192u32.to_le_bytes());

        let i = ModemInfo::parse(&b).expect("parses");
        assert_eq!((i.fw, i.git_sha), ([0, 3, 1], [0xc0, 0x5b, 0x8d, 0x3e]));
        assert_eq!((i.reset_cause, i.build_flags), (0x10, 0x01));
        assert_eq!(i.now_ms, 3_600_000);
        assert_eq!((i.state, i.net_id), (gateway_state::GATEWAY, 0x6d24_8a62));
        assert_eq!((i.turnaround_ms, i.dl_gap_ms, i.modulation), (20, 21, 0));
        assert_eq!(
            i.radio,
            RadioConfig {
                freq_hz: 869_525_000,
                sf: 9,
                bw: 1,
                cr: 2,
                preamble: 12,
                tx_power_dbm: -3,
                flags: 0x02,
            }
        );
        assert_eq!(
            (i.err_flags, i.last_err_code, i.last_err_age_s),
            (0x8000_0001, 3, 77)
        );
        assert_eq!(i.counter("rx_total"), 1000);
        assert_eq!(i.counter("radio_crc_err"), 1009);
        assert_eq!(i.counter("tx_total"), 1013);
        assert_eq!(i.counter("evt_resent"), 1024);
        assert_eq!(i.counter("no_such_counter"), 0);
        assert_eq!(
            (i.ack_turnaround_last_us, i.ack_turnaround_max_us),
            (1500, 9000)
        );
        assert_eq!(i.airtime_ms_last_hour, 4321);
        assert_eq!(
            (i.peers, i.peers_max, i.queue_used, i.queue_max),
            (2, 64, 1, 32)
        );
        assert_eq!((i.evt_ring_used, i.evt_ring_max), (3, 16));
        assert_eq!((i.ctr_next, i.ctr_last), (4097, 8192));
        assert_eq!(i.evt_head_seq, None, "a 171 B reply has no head");
        assert_eq!(i.encode(), b, "encode is the inverse");

        assert!(!i.answers_in_ack());

        // Northbridge 0.3.1 final: 172 B, byte 171 the ring's head.
        b.push(29);
        let i = ModemInfo::parse(&b).expect("parses");
        assert_eq!(i.evt_head_seq, Some(29));
        assert_eq!(i.ack_ctrl, None, "a 172 B reply has no ACK-tail counters");
        assert!(!i.answers_in_ack());
        assert_eq!(i.encode(), b);

        // A reply between two known lengths is the shorter one, not a half-read counter.
        let mut partial = b.clone();
        partial.extend_from_slice(&[0xAA; 7]);
        let i = ModemInfo::parse(&partial).expect("parses");
        assert_eq!((i.evt_head_seq, i.ack_ctrl), (Some(29), None));

        // The current image (#470 §13.5): 180 B, ack_ctrl_sent at 172, ack_time_no_anchor at 176.
        b.extend_from_slice(&0x0102_0304u32.to_le_bytes());
        b.extend_from_slice(&0x0A0B_0C0Du32.to_le_bytes());
        assert_eq!(b.len(), ModemInfo::LEN_ACK_CTRL);
        let i = ModemInfo::parse(&b).expect("parses");
        assert_eq!(i.evt_head_seq, Some(29));
        assert_eq!(
            i.ack_ctrl,
            Some(AckCtrlCounters {
                sent: 0x0102_0304,
                time_no_anchor: 0x0A0B_0C0D,
            })
        );
        assert!(i.answers_in_ack());
        assert_eq!(i.encode(), b, "encode is the inverse at 180 B");
        // Bytes past 180 are ignored.
        let mut longer = b.clone();
        longer.extend_from_slice(&[0xFF; 4]);
        assert_eq!(ModemInfo::parse(&longer).expect("parses"), i);
    }

    #[test]
    fn a_short_get_info_is_refused_rather_than_read_as_zeros() {
        let err = ModemInfo::parse(&[0u8; 21]).expect_err("the legacy 21 B reply");
        assert_eq!(err.kind(), Kind::Transport);
        assert!(err.message().contains("171"), "{}", err.message());
        assert!(
            ModemInfo::parse(&[0u8; ModemInfo::LEN_ACK_CTRL + 4]).is_ok(),
            "a longer one is read"
        );
    }
}
