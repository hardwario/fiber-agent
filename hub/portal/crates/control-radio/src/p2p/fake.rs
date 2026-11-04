//! In-memory doubles for the P2P runtime — a scripted [`FakeModem`] and a recording
//! [`RecordingPublisher`], the P2P counterparts of [`crate::lorawan::fake`].
//!
//! Kept as a real module (not `#[cfg(test)]`) for the same reason the LoRaWAN fake is: both the
//! crate's own unit tests and any integration harness drive the session engine
//! ([`crate::p2p::runtime::Central`]) through these, with no serial port and no broker.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::error::Result;
use crate::northbound::Area;
use crate::p2p::modem::{Event, RadioConfig, Status, TxSchedule};
use crate::p2p::modem_io::{Modem, ModemInfo};
use crate::p2p::publisher::NorthboundPublisher;
use crate::p2p::tower::link::TwrCmd;

/// Lock, recovering the guard from a poisoned mutex rather than panicking — the crate denies
/// `unwrap`/`expect` outside tests, and a recording double has nothing worth poisoning over.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One command the runtime issued, recorded in order so a test can assert the exact bytes and
/// the sequence (e.g. `SetRadioConfig` then `TWR_START` on boot, or a `TxSchedule` carrying a
/// byte-exact JoinAccept).
#[derive(Debug, Clone, PartialEq)]
pub enum SentCmd {
    GetInfo,
    SetRadioConfig(RadioConfig),
    RxStop,
    TxSchedule(TxSchedule),
    TxCancel(u8),
    /// A TOWER gateway command (`p2p_tower_gateway.md` §2.1), other than the ack.
    Twr(TwrCmd),
    /// `TWR_EVT_ACK {seq}` (link doc §4.1). Recorded apart from [`SentCmd::Twr`]: it has no
    /// `RSP`, and keeping it out of [`FakeModem::twr_sent`] lets a test assert a restore's
    /// command order without the acks that happen to interleave with it.
    EvtAck(u8),
}

/// A modem whose events are scripted and whose commands are recorded.
#[derive(Default)]
pub struct FakeModem {
    /// Events [`Modem::next_event`] hands out, in order.
    pub events: VecDeque<Event>,
    /// Commands the runtime issued, in order.
    pub sent: Vec<SentCmd>,
    /// What `get_info` returns.
    pub info: ModemInfo,
    /// Refuse the next `tx_schedule` with this status, then clear it — the modem's single TX
    /// slot really does refuse (`BUSY`, `LATE`), and the runtime's handling of that is a
    /// behaviour worth testing. Without this the real modem is the only thing that can produce
    /// a refusal, which is why D2 sat untested until a bench run happened to trip it.
    pub refuse_next: Option<Status>,
    /// Refuse the next `set_radio_config` with this status, then clear it. Separate from
    /// [`Self::refuse_next`] so a test can refuse one without arming the other: a live
    /// spreading-factor switch pushes a config in the middle of a stream of scheduled TXs, and
    /// the two refusals have entirely different consequences.
    pub refuse_next_config: Option<Status>,
    /// Run this on every `tx_schedule`, before the command is answered.
    ///
    /// The modem round trip is the one window in which the runtime holds no session-DB lock, so
    /// it is where a concurrent `node-send` lands. Nothing else can stand in for that: a test
    /// that writes before or after the call is testing a different ordering than the one that
    /// loses data.
    #[allow(clippy::type_complexity)]
    pub on_tx_schedule: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Make every `get_info` fail with a `Timeout`, the way a northbridge that stopped
    /// answering does — what the central's idle probe exists to notice.
    pub fail_get_info: bool,
    /// Refuse the next `TWR_*` command of this type with this status. Each entry is used once,
    /// the first match first, so a test can refuse one `TWR_NODE_ADD` and let the next through.
    pub refuse_twr: Vec<(u8, Status)>,
    /// What the next `TWR_*` command of this type answers after the status byte (`NODE_LIST`).
    /// Used once each; an unscripted command answers an empty payload.
    pub twr_answers: Vec<(u8, Vec<u8>)>,
}

impl FakeModem {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue an event for the runtime to receive.
    pub fn push_event(&mut self, ev: Event) {
        self.events.push_back(ev);
    }

    /// Every `TX_SCHEDULE` the runtime issued, in order — the radio frames it answered with.
    pub fn scheduled(&self) -> Vec<&TxSchedule> {
        self.sent
            .iter()
            .filter_map(|c| match c {
                SentCmd::TxSchedule(tx) => Some(tx),
                _ => None,
            })
            .collect()
    }

    /// Every `TWR_EVT_ACK` seq the runtime sent, in order.
    pub fn acks(&self) -> Vec<u8> {
        self.sent
            .iter()
            .filter_map(|c| match c {
                SentCmd::EvtAck(seq) => Some(*seq),
                _ => None,
            })
            .collect()
    }

    /// Every `TWR_*` command the runtime issued, in order.
    pub fn twr_sent(&self) -> Vec<&TwrCmd> {
        self.sent
            .iter()
            .filter_map(|c| match c {
                SentCmd::Twr(t) => Some(t),
                _ => None,
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl Modem for FakeModem {
    async fn get_info(&mut self) -> Result<ModemInfo> {
        self.sent.push(SentCmd::GetInfo);
        if self.fail_get_info {
            return Err(crate::error::Error::new(
                crate::error::Kind::Timeout,
                crate::error::Transport::P2p,
                crate::error::Component::NorthbridgeFirmware,
                "GET_INFO: no RSP (fake)",
            ));
        }
        Ok(self.info)
    }
    async fn set_radio_config(&mut self, cfg: &RadioConfig) -> Result<()> {
        // Recorded before the refusal, like `tx_schedule`: the real modem also sees the command
        // it rejects.
        self.sent.push(SentCmd::SetRadioConfig(*cfg));
        match self.refuse_next_config.take() {
            Some(status) => status.into_result("SET_RADIO_CONFIG"),
            None => Ok(()),
        }
    }
    async fn rx_stop(&mut self) -> Result<()> {
        self.sent.push(SentCmd::RxStop);
        Ok(())
    }
    async fn tx_schedule(&mut self, tx: &TxSchedule) -> Result<()> {
        self.sent.push(SentCmd::TxSchedule(tx.clone()));
        if let Some(hook) = self.on_tx_schedule.clone() {
            hook();
        }
        // Recorded before the refusal: the real modem also sees the command it rejects.
        match self.refuse_next.take() {
            Some(status) => status.into_result("TX_SCHEDULE"),
            None => Ok(()),
        }
    }
    async fn tx_cancel(&mut self, tx_id: u8) -> Result<()> {
        self.sent.push(SentCmd::TxCancel(tx_id));
        Ok(())
    }
    async fn twr(&mut self, cmd: &TwrCmd) -> Result<Vec<u8>> {
        // The body is encoded first, as the real link does: a body the central itself refuses
        // never reaches the modem, so it is not recorded either.
        cmd.encode()?;
        if let TwrCmd::EvtAck { seq } = cmd {
            // No RSP, so nothing to refuse either: the gateway acts on it or never sees it.
            self.sent.push(SentCmd::EvtAck(*seq));
            return Ok(Vec::new());
        }
        self.sent.push(SentCmd::Twr(cmd.clone()));
        let t = cmd.cmd();
        if let Some(i) = self.refuse_twr.iter().position(|(c, _)| *c == t) {
            let (_, status) = self.refuse_twr.remove(i);
            status.into_result(cmd.name())?;
        }
        Ok(match self.twr_answers.iter().position(|(c, _)| *c == t) {
            Some(i) => self.twr_answers.remove(i).1,
            // An empty gateway lists no peers: one page, no rows, no next.
            None if t == crate::p2p::tower::link::cmd::TWR_NODE_LIST => {
                vec![0, 0, 0xff, 0xff, 0]
            }
            None => Vec::new(),
        })
    }
    async fn next_event(&mut self) -> Result<Event> {
        match self.events.pop_front() {
            Some(ev) => Ok(ev),
            // A quiet link: never resolves, so a `run_central` test's shutdown branch wins.
            None => std::future::pending().await,
        }
    }
}

/// A recorded northbound publish.
#[derive(Debug, Clone, PartialEq)]
pub struct Published {
    pub dev_eui: String,
    pub area: Area,
    pub payload: Vec<u8>,
}

/// A publisher that records instead of connecting to a broker. Cloneable over shared state, so a
/// test keeps one handle while [`crate::p2p::runtime::Central`] owns another and both see the
/// same recordings.
#[derive(Debug, Clone, Default)]
pub struct RecordingPublisher {
    published: Arc<Mutex<Vec<Published>>>,
    tombstoned: Arc<Mutex<Vec<String>>>,
}

impl RecordingPublisher {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn published(&self) -> Vec<Published> {
        lock(&self.published).clone()
    }
    pub fn tombstoned(&self) -> Vec<String> {
        lock(&self.tombstoned).clone()
    }
}

#[async_trait::async_trait]
impl NorthboundPublisher for RecordingPublisher {
    async fn publish(&self, dev_eui: &str, area: Area, payload: &[u8]) -> Result<()> {
        lock(&self.published).push(Published {
            dev_eui: dev_eui.to_string(),
            area,
            payload: payload.to_vec(),
        });
        Ok(())
    }
    async fn tombstone(&self, dev_eui: &str) -> Result<()> {
        lock(&self.tombstoned).push(dev_eui.to_string());
        Ok(())
    }
}
