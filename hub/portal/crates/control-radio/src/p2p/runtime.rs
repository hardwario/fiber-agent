//! The P2P central runtime — the always-on tasks decision #12 hands to `proximos-control`.
//!
//! `plan/control/radio/p2p_control_runtime.md`: this is the loop that turns the pure `p2p`
//! modules into the running central. It opens no ports itself — it drives a [`Modem`] (real
//! [`crate::p2p::modem_io::SerialModem`] in production, [`crate::p2p::fake::FakeModem`] under
//! test) and publishes through a [`NorthboundPublisher`]. Everything it decides is exact per
//! §4 of that plan and was validated end-to-end on real hardware (2026-08-27).
//!
//! The engine is factored so a single event is handled by [`Central::on_event`], which the tests
//! call directly with a scripted modem; [`run_central`] is only the loop that feeds it.
//!
//! The modem is a gateway (`plan/control/radio/p2p_tower_gateway.md`): it opens, dedups and
//! ACKs the Nodes' frames and sends their queued downlinks itself. The central verifies joins
//! and answers them, installs the session keys (`TWR_NODE_ADD`), issues the gateway's TX
//! counter blocks, decodes the plaintext it is handed (`EVT_TWR_UPLINK`), and pushes the
//! downlinks (`TWR_QUEUE_PUSH`), learning their fate from `EVT_TWR_TX`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::cache::{Cache, CacheSlot};
use crate::config;
use crate::decode::device_status::DeviceStatusDocument;
use crate::decode::reading::GatewayRx;
use crate::error::{Component, Error, Kind, Result, Transport};
use crate::lorawan::eui::Eui;
use crate::northbound::Area;
use crate::p2p::airtime;
use crate::p2p::db::{self as p2p_db, SessionsDb};
use crate::p2p::gateways::{self, GwPeer, LOCAL};
use crate::p2p::modem::{self, Event, RadioConfig, RxEvent, TxSchedule};
use crate::p2p::modem_io::Modem;
use crate::p2p::outbox::{self, Outbox};
use crate::p2p::publisher::NorthboundPublisher;
use crate::p2p::session::{
    DownlinkPhase, HistoryCollect, PagesSeen, QueuedDownlink, Session, REANNOUNCE_AFTER_UPLINKS,
};
use crate::p2p::tower::control::{
    self, Capabilities, ControlDownlink, DevStatusAns, Envelope, Hello, JoinAccept, JoinReq,
    LinkCheckAns, TimeAns,
};
use crate::p2p::tower::delivery::{Admit, Delivery, DeliveryStats};
use crate::p2p::tower::frame::{self, Header};
use crate::p2p::tower::link::{self, NodeListPage, TwrCmd, TwrCtrLow, TwrTx, TwrUplink, UplinkAck};
use crate::p2p::tower::MAX_COMMAND_BODY;
use crate::store::{Slot, Store};

/// JoinAccept assigns this RX1 delay; also how far past RxDone the answer is aimed.
const DEFAULT_RX1_DELAY_S: u64 = 1;

/// Below this a `now_unix_secs()` reading is an unset clock, not a real time: 2023-11-14, chosen
/// to be safely behind any date this product could ship on and safely ahead of the epoch a Hub
/// with no RTC and no NTP comes up at.
const PLAUSIBLE_CLOCK_FLOOR: u32 = 1_700_000_000;

/// `ttl_s` of a pushed command: an hour, so a Node on a long report interval still meets it.
const COMMAND_TTL_S: u16 = 3600;

/// `ttl_s` of a `0x91` answer (contract §3.3).
const ANSWER_TTL_S: u16 = 600;

/// How many pushed `0x91` answers are remembered, for the log line of their outcome.
const ANSWERS_TRACKED: usize = 32;

/// The most `TWR_NODE_LIST` pages one sweep reads: 24 peers each, well past the gateway's
/// capacity, so a gateway that keeps answering `next` cannot hold the resync forever.
const NODE_LIST_MAX_PAGES: usize = 32;

/// The fPorts an `0x81` uplink carries (contract §3.3): the field frames, and the command
/// engine's responses.
mod port {
    pub const TELEMETRY: u8 = 2;
    pub const ALARM: u8 = 3;
    pub const RESPONSE: u8 = 85;
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// The host clock in Unix milliseconds, for the modem clock anchor.
fn now_unix_ms() -> u64 {
    u64::try_from(now_ms()).unwrap_or(0)
}

/// The 8 bytes of a canonical 16-hex DevEUI, MSB first.
fn dev_eui_bytes(dev_eui: &str) -> Option<[u8; 8]> {
    if dev_eui.len() != 16 {
        return None;
    }
    u64::from_str_radix(dev_eui, 16).ok().map(u64::to_be_bytes)
}

/// The first item id of the `0x91` answer pushes; commands use `1..ANSWER_ITEM_BASE`.
const ANSWER_ITEM_BASE: u16 = 0x8000;

/// The item id a queue head is pushed under: the same head always gets the same id, so a
/// push repeated after a central restart replaces the one the still-running Northbridge
/// holds instead of queueing it twice (0.3.0 replaces by `(addr, item)`, link doc §5.1).
/// From the head's `Command.seq`, or from its bytes for an operator's `--hex` payload.
fn command_item(head: &QueuedDownlink) -> u16 {
    let key = match head.seq {
        Some(seq) => seq,
        // FNV-1a over the body: stable across restarts, unlike the std hasher.
        None => head.body.iter().fold(0x811c_9dc5u32, |h, b| {
            (h ^ u32::from(*b)).wrapping_mul(0x0100_0193)
        }),
    };
    let span = u32::from(ANSWER_ITEM_BASE - 1);
    // In 1..ANSWER_ITEM_BASE: never 0, never an answer's.
    (key % span + 1) as u16
}

/// The highest counter a running gateway may have spent, from its `GET_INFO`: the end of the
/// block it holds, or the one before its next (0 / 0 without a block).
fn gateway_ctr_floor(info: &crate::p2p::modem_io::ModemInfo) -> u32 {
    info.ctr_last.max(info.ctr_next.saturating_sub(1))
}

/// The `command` document for a queue head given up after
/// [`crate::p2p::session::UNDELIVERED_LIMIT`] unACKed pushes (#470 C3): the shape of
/// [`crate::coalesce::superseded_document`], with `status`/`kind` `undelivered`, the attempts
/// and the last outcome. Like that one it is the Hub's, not the Node's: no `received_at`.
/// The ring `seq` of an `EVT_TWR_UPLINK` / `EVT_TWR_TX`.
fn ring_seq(ev: &Event) -> Option<u8> {
    match ev {
        Event::TwrUplink { seq, .. } | Event::TwrTx { seq, .. } => Some(*seq),
        _ => None,
    }
}

fn ring_what(ev: &Event) -> &'static str {
    match ev {
        Event::TwrUplink { .. } => "EVT_TWR_UPLINK",
        Event::TwrTx { .. } => "EVT_TWR_TX",
        _ => "event",
    }
}

/// ` page I/N` or ` frame I/N` of an fPort-85 frame, for a log line; empty for a single frame.
fn frame_position(cd: &crate::tasks::uplink::CommandDecode) -> String {
    if let Some(f) = cd.history.as_ref() {
        return format!(" frame {}/{}", f.frame_index, f.frame_count);
    }
    cd.response
        .paging()
        .map(|p| format!(" page {}/{}", p.index, p.count))
        .unwrap_or_default()
}

/// A Node's `Response.error` as a log line (#128): `<code> <NAME>[ field …][ "detail"]`.
fn node_error_text(e: &serde_json::Map<String, serde_json::Value>) -> String {
    let code = e.get("code").and_then(|v| v.as_u64()).unwrap_or(0);
    let name = crate::decode::command::error_code_name(code);
    let detail = e
        .get("detail")
        .and_then(|v| v.as_str())
        .map(|d| format!(" \"{d}\""))
        .unwrap_or_default();
    let field = e
        .get("fault_key")
        .or_else(|| e.get("fault_field"))
        .map(|f| format!(" field {f}"))
        .unwrap_or_default();
    format!("{code} {name}{field}{detail}")
}

fn undelivered_document(
    dev_eui: &str,
    serial: Option<&str>,
    seq: u32,
    last: link::TxOutcome,
    at: &str,
) -> serde_json::Value {
    use crate::p2p::session::{UNDELIVERED, UNDELIVERED_LIMIT};
    let mut doc = serde_json::Map::new();
    doc.insert("dev_eui".into(), dev_eui.into());
    if let Some(serial) = serial {
        doc.insert("serial".into(), serial.into());
    }
    doc.insert("seq".into(), seq.into());
    doc.insert("kind".into(), UNDELIVERED.into());
    doc.insert("status".into(), UNDELIVERED.into());
    doc.insert("attempts".into(), UNDELIVERED_LIMIT.into());
    doc.insert("last_outcome".into(), format!("{last:?}").into());
    doc.insert("at".into(), at.into());
    serde_json::Value::Object(doc)
}

/// A running gateway with under a quarter of a `block`-sized block left, or none at all: the
/// same low mark at which 0.3.0 raises `EVT_TWR_CTR_LOW`, read from a `GET_INFO` instead.
fn gateway_counters_short(info: &crate::p2p::modem_io::ModemInfo, block: u32) -> bool {
    if info.state != crate::p2p::modem_io::gateway_state::GATEWAY {
        return false;
    }
    let left = if info.ctr_last == 0 || info.ctr_next > info.ctr_last {
        0
    } else {
        u64::from(info.ctr_last - info.ctr_next) + 1
    };
    left * 4 < u64::from(block)
}

/// One diagnostic line to stderr — the bench witness, mirroring `control-radio-harness`'s own
/// `eprintln!` use. Not the northbound path (that is MQTT); just an operator trace.
macro_rules! diag {
    ($($arg:tt)*) => { eprintln!("[p2p {}] {}", now_ms(), format_args!($($arg)*)) };
}

fn hex16(s: &str) -> Result<[u8; 16]> {
    let bytes = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2).unwrap_or("zz"), 16).ok())
        .collect::<Option<Vec<u8>>>()
        .filter(|v| v.len() == 16)
        .ok_or_else(|| {
            Error::new(
                Kind::StorageError,
                Transport::P2p,
                Component::SessionDb,
                "a stored key is not 16 hex-encoded bytes",
            )
        })?;
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Why a session is parked, for its diag line: the carrier the registry holds the Node on
/// (node_registry.md §4, amended 2026-09-28). Read only when a parked session is hit, so the
/// live path pays nothing for it.
fn parked_reason(store: &Store, dev_eui: &str) -> String {
    match crate::registry::current(store) {
        Ok(reg) => match reg.get(dev_eui) {
            // `SessionsDb::load` leaves a P2P Node without an `app_key` off the view.
            Some(n) if n.radio == Transport::P2p => {
                "the Node is registered on p2p without an app_key".to_string()
            }
            Some(n) => format!("the Node is registered on {}", n.radio.as_str()),
            None => "the Node is not in the registry".to_string(),
        },
        Err(e) => format!("the registry is unreadable ({})", e.message()),
    }
}

fn to_hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        use std::fmt::Write;
        let _ = write!(s, "{x:02x}");
    }
    s
}

/// How far behind the newest timestamp the modem has reported an `EVT_RX` may be and still be
/// worth answering, on top of the RX1 aim itself.
///
/// An answer is aimed at about `rx_t_ms + rx1_delay_s*1000 + RX1_ACK_START_MS` (less the modem's
/// learned TX lead, [`modem::schedule_at`]). Once the modem's own
/// clock is past that, the node's window has closed and the modem refuses the frame `Late`, so
/// scheduling it achieves nothing but noise. The extra allowance absorbs ordinary jitter between
/// the RxDone interrupt and the event reaching us (D3).
const STALE_RX_SLACK_MS: u64 = 500;

/// How often [`run_central`] re-reads the `radio-set` override when no modem event has prompted
/// it. Only the quiet-link case depends on it — every event already checks.
const RADIO_OVERRIDE_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// How long the modem may stay silent before [`run_central`] asks it for a `GET_INFO`.
///
/// A quiet network produces no events, and a quiet network is not a dead modem
/// (`plan/portal/p2p_integration.md` §4.1): the central probes before anyone may call it dead.
/// One health interval, so the host's liveness check (three intervals) always sees a fresh
/// answer from a modem that is alive. Every 30 s, so the factory test sees the counters of a
/// quiet link move within half a minute (`plan/system/factory_test.md`, amendment 2026-10-13).
const IDLE_PROBE_AFTER_S: u64 = 30;

/// The longest the Northbridge's `TWR_TIME_SYNC` anchor goes unrefreshed
/// (`p2p_tower_gateway.md` §13.5). Every anchored `GET_INFO` also sends one, so this only
/// matters while the probe is held off.
const TIME_SYNC_EVERY: std::time::Duration = std::time::Duration::from_secs(600);
/// The soonest a `TWR_TIME_SYNC` goes again after a `TimeReq` the gateway's ACK could not
/// answer for want of an anchor (§13.5): a Node asks at most every uplink, the gateway is
/// told once a minute at most.
const TIME_SYNC_RETRY: std::time::Duration = std::time::Duration::from_secs(60);

/// The `0x91` answers of one uplink waiting for the Node's home: DevEUI, the uplink's counter,
/// the `(cmd, value)` answers.
type DueAnswers = (String, u32, Vec<(u8, Vec<u8>)>);

/// The `LinkCheckReq` / `TimeReq` of an uplink, by name, for the log.
fn req_names(reqs: &[u8]) -> String {
    reqs.iter()
        .map(|c| {
            if *c == control::cmd::TIME {
                "TimeReq"
            } else {
                "LinkCheckReq"
            }
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

/// How far a remote gateway's Hub clock may sit from this one's before its `GET_INFO` stamp is
/// not used for the clock anchor (`p2p_cluster_gateway.md` §6).
const ANCHOR_STAMP_MAX_SKEW: std::time::Duration = std::time::Duration::from_secs(2);

/// When the modem last proved it is alive — any event, or a `GET_INFO` answer — as Unix seconds.
///
/// Shared with the host (PI1, `plan/portal/p2p_integration.md` §4.1): the central is moved into
/// [`run_central`], so the host takes a clone with [`Central::liveness`] first and its health
/// loop reads it. The central marks it; nothing else writes it.
#[derive(Clone, Debug, Default)]
pub struct Liveness(std::sync::Arc<std::sync::atomic::AtomicU64>);

impl Liveness {
    /// Record a sign of life at `now_unix`.
    pub fn mark(&self, now_unix: u64) {
        self.0.store(now_unix, std::sync::atomic::Ordering::Relaxed);
    }

    /// The last sign of life, or `None` before the modem has answered at all.
    pub fn last(&self) -> Option<u64> {
        match self.0.load(std::sync::atomic::Ordering::Relaxed) {
            0 => None,
            t => Some(t),
        }
    }

    /// Whether the last sign of life is at most `max_age_s` old at `now_unix`. A timestamp from
    /// the future (clock step) counts as fresh.
    pub fn is_fresh(&self, now_unix: u64, max_age_s: u64) -> bool {
        self.last()
            .is_some_and(|t| now_unix.saturating_sub(t) <= max_age_s)
    }
}

/// Unix seconds now, for [`Liveness`].
fn now_unix_s() -> u64 {
    u64::try_from(crate::lorawan::gateway::now_unix()).unwrap_or(0)
}

/// Unix seconds now, signed, for the status document's timestamps.
fn now_unix_i() -> i64 {
    crate::lorawan::gateway::now_unix()
}

/// The running central. Holds only runtime state; the registry, sessions and cache live in
/// `sessions.db` and are (re)loaded per event so a `node-add` from the separate CLI process is
/// visible to the next join (the single-writer control socket that removes this cross-process
/// window is `plan.md` §5 / monolith §11.4 work).
pub struct Central<P: NorthboundPublisher> {
    store: Store,
    net_id: u32,
    radio: RadioConfig,
    /// The TX power in dBm handed to every Node in its JoinAccept `tx_power`, or 0 to assign
    /// nothing. From `radio.p2p_radio.node_tx_power_dbm`. One value for the whole network: P2P
    /// has no per-Node power control (#470, 2026-09-28).
    node_tx_power_dbm: u8,
    /// How long a history replay may go without a frame before it is retired. From
    /// `radio.p2p_radio.history_stall_timeout_s`.
    history_stall_timeout_s: u64,
    rx1_delay_s: u64,
    publisher: P,
    tx_id: u8,
    /// The newest timestamp the modem has reported, from either an `EVT_RX` or an
    /// `EVT_TX_DONE`. The modem's clock, not ours — it is the only reference for judging
    /// whether a buffered `EVT_RX` is still answerable (D3). `None` until the first event.
    newest_modem_t_ms: Option<u64>,
    /// Whether the last `EVT_RX` was dropped as stale, so a drained backlog says so once
    /// instead of once per frame.
    in_stale_burst: bool,
    /// `radio.device_profiles.p2p`, by profile name — §4's decoder binding. A node's registry
    /// entry names a profile, the profile names a compiled-in decoder, and there is no implicit
    /// fallback: a frame whose profile this table does not hold is ACKed and logged, never
    /// guessed at. Held on the central rather than re-read per frame because the config is
    /// process-lifetime state, unlike the registry (which a separate `node-add` process writes).
    ///
    /// It binds the **field** ports (`0x02`/`0x03`) only. A `0x55` RESPONSE deliberately
    /// bypasses this table: §9.7's command engine is one protobuf vocabulary for every Node,
    /// not a per-profile payload format, so a device whose profile is unknown still gets its
    /// `Info`, `ConfigDump` and `Error` decoded rather than reduced to hex.
    p2p_profiles: BTreeMap<String, config::P2pProfile>,
    /// `radio.p2p_radio.spreading_factor` as the rendered config declares it — the value a
    /// `radio-set --reset` falls back to. Kept beside [`Self::radio`] because `radio.sf` is the
    /// *effective* one and the two are only equal while no override is in force.
    config_sf: u8,
    /// The `radio-set` generation this central has pushed to the modem. The override is applied
    /// when the generation in `sessions.db` differs — differs, not exceeds: `--reset` removes the
    /// record and takes it back to 0, which is a change like any other.
    applied_generation: u64,
    /// The `tx_id` of a scheduled frame that has not reported `EVT_TX_DONE` yet, if any.
    ///
    /// A spreading-factor switch retunes the radio, so pushing one while the modem is holding a
    /// frame for a node's RX1 window would send that frame on the wrong SF — or not at all. The
    /// switch waits; the modem has a single TX slot, so at most one frame is ever in the way.
    tx_in_flight: Option<u8>,
    /// The frame in [`Self::tx_in_flight`]: its aimed `t_ms` and time on air, so its
    /// `EVT_TX_DONE` tells when it really started ([`airtime::TxLead`]).
    tx_aim: Option<(u8, u64, f64)>,
    /// The modem's TX start after the aimed `t_ms`, learned from `EVT_TX_DONE`.
    tx_lead: airtime::TxLead,
    /// The TX lead last saved to [`Slot::P2pTxLead`].
    tx_lead_saved: f64,
    /// §9.7 page reassembly for the paged `0x55` `ConfigDump`, one reassembler per serial.
    ///
    /// In memory and on the central, deliberately (decision D5): a dump half-collected when the
    /// central restarts is not worth persisting — `node-get config` re-requests it — and a
    /// partial dump on disk would have to be aged out to stop it merging with the next one.
    dumps: crate::tasks::uplink::State,
    /// The modem's last sign of life — see [`Liveness`].
    liveness: Liveness,
    /// The northbridge status document this central feeds
    /// (`plan/control/radio/northbridge_diagnostics.md` §3).
    nb: crate::p2p::northbridge::NorthbridgeStatus,
    /// When the last `GET_INFO` answer arrived, Unix seconds — the status document refreshes its
    /// counters once per health interval even while frames keep the link busy.
    last_info_at: Option<u64>,
    /// The modem's clock against the host's at the last `GET_INFO`: `(now_ms, host Unix ms)`.
    /// It maps a frame's `t_ms` to wall time for a `TimeAns` (contract §3.3).
    clock_anchor: Option<(u64, u64)>,
    /// Whether the gateway answers `LinkCheckReq` / `TimeReq` in its ACK: its last `GET_INFO`
    /// was 180 B (`p2p_tower_gateway.md` §3, §10.4). Then the central queues neither answer
    /// and keeps the gateway's time anchor; otherwise it answers them itself (§17).
    answers_in_ack: bool,
    /// When the last `TWR_TIME_SYNC` was tried, whatever its outcome (§13.5).
    time_sync_at: Option<Instant>,
    /// An ACK without a `TimeAns` for a `TimeReq` showed the gateway holds no anchor: the next
    /// `TWR_TIME_SYNC` goes after [`TIME_SYNC_RETRY`], not the 10 min floor.
    time_sync_wanted: bool,
    /// The peers the gateway holds, as this central put them there: `addr →` the Node, its
    /// key and whether this gateway is its home. Emptied at every (re)init and refilled by its
    /// resync; the tick keeps it equal to the joined sessions on the registry's P2P view
    /// (contract §3.4), with `HOME` on the home only (`p2p_cluster_gateway.md` §4).
    gw_table: BTreeMap<u32, GwPeer>,
    /// The queue head of each Node that is pushed to the gateway and has no outcome yet, by
    /// `dev_eui`: the `Announced` heads this central pushed. In memory, since a restart
    /// re-adds every peer, and that drops the gateway's queues.
    pushed: BTreeMap<String, Pushed>,
    /// The pending-info item behind a pushed command head, by `dev_eui`: `(addr, item)`
    /// (p2p_control_commands.md §4). Kept past the head's outcome, since it is meant to reach
    /// the Node after the head, so its `EVT_TWR_TX` is recognised.
    pending_info_items: BTreeMap<String, (u32, u16)>,
    /// The `0x91` answers pushed, oldest first: `(item, addr, what)`.
    answers: VecDeque<(u16, u32, String)>,
    /// The last `TWR_QUEUE_PUSH` item id.
    item: u16,
    /// The size of the TX counter blocks this central issues: [`p2p_db::COUNTER_BLOCK`], or the
    /// HIL test override ([`p2p_db::COUNTER_BLOCK_ENV`]).
    ctr_block: u32,
    /// The receiving side of the gateway's event ring: which ring events to process and when
    /// to ack them (link doc §4.1).
    delivery: Delivery,
    /// The receiver's counters as last added to the status document.
    delivery_reported: DeliveryStats,
    /// Whether the outbox was full at the last ring event, so the log says so once.
    outbox_full: bool,
    /// The ring event being handled, until its effects are stored ([`Self::ring_stored`]).
    ring_pending: Option<u8>,
    /// Ring events held, unprocessed, while the receiver learns where the gateway's ring
    /// starts; processed in `seq` order once it knows ([`Self::settle_ring`]).
    held: BTreeMap<u8, Event>,
    /// Whether the last `GET_INFO` carried the ring's head (Northbridge ≥ 0.3.1 final): then a
    /// gap relearn asks for it instead of learning.
    head_reported: bool,
    /// The last `GET_INFO` round trip, ms, for the gateways document (§8).
    link_rtt_ms: Option<u64>,
    /// Where a ring event's northbound messages are stored before it is acked, for the
    /// drainer to publish (`p2p_tower_gateway.md` §8.6). Opened on first use unless the host
    /// handed one in, so a host's central never opens (and sweeps) a second one on its dir.
    outbox_cell: std::sync::OnceLock<std::sync::Arc<Outbox>>,
    /// The `central_nonce` of the next JoinAccept instead of a random one: the seam the KAT
    /// test pins the JoinAccept bytes with.
    #[cfg(test)]
    central_nonce: Option<u32>,
    /// The gateway whose state the per-gateway fields above hold: [`LOCAL`] or a Client Hub's
    /// hostname (`p2p_cluster_gateway.md` §4). [`Self::activate`] swaps another one in.
    active: String,
    /// The other online gateways' state, while another one is active.
    parked_gws: BTreeMap<String, GwState>,
    /// How well each gateway hears each Node, for its home (§5.1).
    signals: gateways::Signals,
    /// The Nodes whose previous counter closed since their home was last reviewed.
    home_review: BTreeSet<u32>,
    /// `0x91` answers to an uplink a gateway other than the Node's home delivered first, by
    /// `addr`: `(dev_eui, counter, answers)`, pushed by the home in the convergence pass.
    answers_due: BTreeMap<u32, DueAnswers>,
    /// The last counter of each Node, by `addr`, whose ACK outcome this central has taken: from
    /// the copy of the gateway that ACKs it — its home, whose copy often comes after another
    /// gateway's (`p2p_cluster_gateway.md` §1), so once per counter.
    ack_taken: BTreeMap<u32, u32>,
    /// A join, a home move or a stashed answer that the other gateways must follow before the
    /// next tick.
    converge_due: bool,
    /// The host's view of the gateways (§8), when it publishes one.
    board: Option<crate::p2p::gw_board::GatewayBoard>,
    /// How long the ring event being handled has taken so far, step by step, until it is acked
    /// ([`RingTrace`], #130).
    ring_trace: Option<RingTrace>,
}

/// The time line of one ring event from the moment the central takes it to its ack (#130).
/// The gateway resends what it has no ack for after 300 ms (link doc §4.1), so an ack that
/// comes later than that is logged with the step that took the time.
#[derive(Debug)]
struct RingTrace {
    seq: u8,
    taken: Instant,
    /// When the gateway heard the frame, Unix ms through the clock anchor: `EVT_TWR_UPLINK`
    /// only.
    heard_ms: Option<u64>,
    steps: Vec<(&'static str, Instant)>,
}

/// A ring event acked later than this after the central took it, or after the gateway heard
/// it, is logged with its steps (#130). Under the gateway's 300 ms resend timeout, so the log
/// shows what comes close as well as what missed it.
const RING_ACK_SLOW: std::time::Duration = std::time::Duration::from_millis(200);

impl RingTrace {
    fn new(seq: u8, heard_ms: Option<u64>) -> Self {
        RingTrace {
            seq,
            taken: Instant::now(),
            heard_ms,
            steps: Vec::new(),
        }
    }

    /// The line to log once the ack is written at `now` (`now_ms` Unix), or `None` when it
    /// was quick.
    fn slow_line(&self, now: Instant, now_ms: u64) -> Option<String> {
        let handling = now.saturating_duration_since(self.taken);
        let since_heard = self.heard_ms.map(|h| now_ms.saturating_sub(h));
        let slow = handling >= RING_ACK_SLOW
            || since_heard.is_some_and(|ms| ms >= RING_ACK_SLOW.as_millis() as u64);
        if !slow {
            return None;
        }
        let mut from = self.taken;
        let mut steps = String::new();
        for (what, at) in &self.steps {
            let ms = at.saturating_duration_since(from).as_millis();
            steps.push_str(&format!(" {what} {ms}"));
            from = *at;
        }
        let rest = now.saturating_duration_since(from).as_millis();
        Some(format!(
            "ring event seq={} acked {} ms after it was taken{} — steps (ms):{steps} ack {rest}",
            self.seq,
            handling.as_millis(),
            match since_heard {
                Some(ms) => format!(", {ms} ms after the gateway heard it"),
                None => String::new(),
            },
        ))
    }
}

/// One gateway's runtime state (`p2p_cluster_gateway.md` §4, "per gateway"): the fields of
/// [`Central`] of the same names while it is the active one, parked here while another is.
struct GwState {
    radio: RadioConfig,
    applied_generation: u64,
    tx_id: u8,
    newest_modem_t_ms: Option<u64>,
    in_stale_burst: bool,
    tx_in_flight: Option<u8>,
    tx_aim: Option<(u8, u64, f64)>,
    tx_lead: airtime::TxLead,
    tx_lead_saved: f64,
    liveness: Liveness,
    nb: crate::p2p::northbridge::NorthbridgeStatus,
    last_info_at: Option<u64>,
    clock_anchor: Option<(u64, u64)>,
    answers_in_ack: bool,
    time_sync_at: Option<Instant>,
    time_sync_wanted: bool,
    gw_table: BTreeMap<u32, GwPeer>,
    pushed: BTreeMap<String, Pushed>,
    answers: VecDeque<(u16, u32, String)>,
    item: u16,
    delivery: Delivery,
    delivery_reported: DeliveryStats,
    outbox_full: bool,
    ring_pending: Option<u8>,
    held: BTreeMap<u8, Event>,
    head_reported: bool,
    link_rtt_ms: Option<u64>,
}

impl GwState {
    /// A gateway the central has not initialised yet: its `init_modem` fills the rest.
    fn fresh(radio: RadioConfig, tx_lead_ms: f64) -> Self {
        GwState {
            radio,
            applied_generation: 0,
            tx_id: 0,
            newest_modem_t_ms: None,
            in_stale_burst: false,
            tx_in_flight: None,
            tx_aim: None,
            tx_lead: airtime::TxLead::seeded(tx_lead_ms),
            tx_lead_saved: tx_lead_ms,
            liveness: Liveness::default(),
            nb: crate::p2p::northbridge::NorthbridgeStatus::default(),
            last_info_at: None,
            clock_anchor: None,
            answers_in_ack: false,
            time_sync_at: None,
            time_sync_wanted: false,
            gw_table: BTreeMap::new(),
            pushed: BTreeMap::new(),
            answers: VecDeque::new(),
            item: 0,
            delivery: Delivery::new(),
            delivery_reported: DeliveryStats::default(),
            outbox_full: false,
            ring_pending: None,
            held: BTreeMap::new(),
            head_reported: false,
            link_rtt_ms: None,
        }
    }
}

/// A queue head handed to the gateway with `TWR_QUEUE_PUSH` (contract §3.4).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pushed {
    addr: u32,
    item: u16,
    /// The command body, which tells whether the head is still the one pushed.
    body: Vec<u8>,
    /// The `PendingInfo.more` it was pushed with (0: none). A queue grown past it re-pushes
    /// the head.
    more: usize,
}

/// One queue head [`Central::heads_to_push`] hands to the push pass.
#[derive(Debug, Clone)]
struct DueHead {
    dev_eui: String,
    addr: u32,
    item: u16,
    body: Vec<u8>,
    /// A `0x91` control head (p2p_control_commands.md §2).
    control: bool,
    /// The `PendingInfo.more` to announce with it; 0 sends none.
    more: usize,
    /// The head is announced already and goes again for a larger `more`.
    repush: bool,
}

/// The plaintext a queue head goes out as: `0x81 ‖ 86 ‖ body`, or `0x91 ‖ body` for a control
/// head, which carries `PendingInfo` in the same frame when `more` > 0 and the frame has room
/// for it (#486 U-1: 74 B). A control head's TLVs are at most 7 B, so in practice it always
/// has; without room the Node still fetches the rest at its next confirmed uplink.
fn head_plaintext(body: &[u8], control: bool, more: usize) -> Vec<u8> {
    if !control {
        return control::command_payload(body);
    }
    let mut out = Vec::with_capacity(1 + body.len() + 3);
    out.push(control::ENVELOPE_CONTROL);
    out.extend_from_slice(body);
    if more > 0 {
        let pending = ControlDownlink::pending_info(more).tlv();
        if out.len() + pending.len() <= frame::MAX_PAYLOAD {
            out.extend(pending);
        }
    }
    out
}

/// The item of a Node's pending-info item: stable per DevEUI, in the command range, never the
/// item of the head it follows.
fn pending_info_item(dev_eui: &str, head_item: u16) -> u16 {
    let h = dev_eui
        .bytes()
        .chain(*b"pending-info")
        .fold(0x811c_9dc5u32, |h, b| {
            (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
        });
    let span = u32::from(ANSWER_ITEM_BASE - 1);
    let item = (h % span + 1) as u16;
    if item == head_item {
        // 1..ANSWER_ITEM_BASE, the next one round.
        item % (ANSWER_ITEM_BASE - 1) + 1
    } else {
        item
    }
}

impl<P: NorthboundPublisher> Central<P> {
    /// Build from the rendered `radio:` config: the `net_id` from `create` (`Slot::P2pNetId`, an
    /// 8-hex string) and the Hub-wide `p2p_radio` parameters mapped onto the modem encoding.
    pub fn from_config(store: Store, cfg: &config::RadioConfig, publisher: P) -> Result<Self> {
        let net_id_hex = store.require(Slot::P2pNetId)?;
        let net_id = u32::from_str_radix(net_id_hex.trim(), 16).map_err(|e| {
            Error::new(
                Kind::StorageError,
                Transport::P2p,
                Component::NodeRegistry,
                format!("p2p/net_id is not an 8-hex value: {e}"),
            )
        })?;
        let p2p_radio = cfg.p2p_radio.as_ref().ok_or_else(|| {
            Error::new(
                Kind::InvalidArgument,
                Transport::P2p,
                Component::RadioMode,
                "radio.p2p_radio is absent — cannot run the P2P central without radio parameters",
            )
        })?;
        let radio = RadioConfig::from_hub_config(p2p_radio)?;
        // `deliver_time` and `rekey_counter_threshold` still parse but steer nothing: the time
        // is a `TimeReq` answer and a session rekeys by joining again (p2p_tower_gateway.md §3).
        let mut central = Central::new(store, net_id, radio, publisher)
            .with_node_tx_power(p2p_radio.node_tx_power_dbm)
            .with_history_stall_timeout(p2p_radio.history_stall_timeout_s)
            .with_p2p_profiles(cfg.device_profiles.p2p.clone());
        // A central restarted after a `radio-set` must come up on the SF the network moved to,
        // not on the config key — the file is never rewritten (decision #4), so the key alone
        // would put the Hub on a spreading factor no node is using any more.
        central.adopt_effective_sf();
        Ok(central)
    }

    /// Assemble the runtime from already-resolved parameters. [`Central::from_config`] is the
    /// production entry point (reads `net_id` and the radio params from the store/config); this
    /// is the seam tests build against with a scripted modem.
    pub fn new(store: Store, net_id: u32, radio: RadioConfig, publisher: P) -> Self {
        // The lead learned before the last restart, so the first ACK is aimed right (bench c55).
        let saved_tx_lead = store
            .read(Slot::P2pTxLead)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or(airtime::TX_LEAD_SEED_MS);
        Central {
            store,
            net_id,
            radio,
            config_sf: radio.sf,
            applied_generation: 0,
            tx_in_flight: None,
            tx_aim: None,
            tx_lead: airtime::TxLead::seeded(saved_tx_lead),
            tx_lead_saved: saved_tx_lead,
            node_tx_power_dbm: 0,
            history_stall_timeout_s: config::default_history_stall_timeout_s(),
            rx1_delay_s: DEFAULT_RX1_DELAY_S,
            publisher,
            tx_id: 0,
            newest_modem_t_ms: None,
            in_stale_burst: false,
            // The binding the shipped `radio-section.yaml` declares, so a test seam that names
            // no config still decodes the one profile every Node is registered under.
            p2p_profiles: BTreeMap::from([(
                "default".to_string(),
                config::P2pProfile {
                    name: "default".to_string(),
                    description: String::new(),
                    decoder: "node".to_string(),
                },
            )]),
            dumps: crate::tasks::uplink::State::new(),
            liveness: Liveness::default(),
            nb: crate::p2p::northbridge::NorthbridgeStatus::default(),
            last_info_at: None,
            clock_anchor: None,
            answers_in_ack: false,
            time_sync_at: None,
            time_sync_wanted: false,
            gw_table: BTreeMap::new(),
            pushed: BTreeMap::new(),
            pending_info_items: BTreeMap::new(),
            answers: VecDeque::new(),
            item: 0,
            ctr_block: p2p_db::counter_block(),
            delivery: Delivery::new(),
            delivery_reported: DeliveryStats::default(),
            outbox_full: false,
            ring_pending: None,
            held: BTreeMap::new(),
            head_reported: false,
            link_rtt_ms: None,
            outbox_cell: std::sync::OnceLock::new(),
            #[cfg(test)]
            central_nonce: None,
            active: LOCAL.to_string(),
            parked_gws: BTreeMap::new(),
            signals: gateways::Signals::default(),
            home_review: BTreeSet::new(),
            answers_due: BTreeMap::new(),
            ack_taken: BTreeMap::new(),
            converge_due: false,
            board: None,
            ring_trace: None,
        }
    }

    /// Exchange the per-gateway fields with `g`.
    fn swap_gateway(&mut self, g: &mut GwState) {
        use std::mem::swap;
        swap(&mut self.radio, &mut g.radio);
        swap(&mut self.applied_generation, &mut g.applied_generation);
        swap(&mut self.tx_id, &mut g.tx_id);
        swap(&mut self.newest_modem_t_ms, &mut g.newest_modem_t_ms);
        swap(&mut self.in_stale_burst, &mut g.in_stale_burst);
        swap(&mut self.tx_in_flight, &mut g.tx_in_flight);
        swap(&mut self.tx_aim, &mut g.tx_aim);
        swap(&mut self.tx_lead, &mut g.tx_lead);
        swap(&mut self.tx_lead_saved, &mut g.tx_lead_saved);
        swap(&mut self.liveness, &mut g.liveness);
        swap(&mut self.nb, &mut g.nb);
        swap(&mut self.last_info_at, &mut g.last_info_at);
        swap(&mut self.clock_anchor, &mut g.clock_anchor);
        swap(&mut self.answers_in_ack, &mut g.answers_in_ack);
        swap(&mut self.time_sync_at, &mut g.time_sync_at);
        swap(&mut self.time_sync_wanted, &mut g.time_sync_wanted);
        swap(&mut self.gw_table, &mut g.gw_table);
        swap(&mut self.pushed, &mut g.pushed);
        swap(&mut self.answers, &mut g.answers);
        swap(&mut self.item, &mut g.item);
        swap(&mut self.delivery, &mut g.delivery);
        swap(&mut self.delivery_reported, &mut g.delivery_reported);
        swap(&mut self.outbox_full, &mut g.outbox_full);
        swap(&mut self.ring_pending, &mut g.ring_pending);
        swap(&mut self.held, &mut g.held);
        swap(&mut self.head_reported, &mut g.head_reported);
        swap(&mut self.link_rtt_ms, &mut g.link_rtt_ms);
    }

    /// The gateway the per-gateway state belongs to now.
    pub fn active_gateway(&self) -> &str {
        &self.active
    }

    /// Every online gateway, [`LOCAL`] included.
    pub fn gateway_names(&self) -> BTreeSet<String> {
        let mut names: BTreeSet<String> = self.parked_gws.keys().cloned().collect();
        names.insert(self.active.clone());
        names
    }

    pub fn has_gateway(&self, gw: &str) -> bool {
        self.active == gw || self.parked_gws.contains_key(gw)
    }

    /// Whether more than one gateway is online — what the join window and the fresh counter
    /// block of a join wait for (§5.3, §7).
    fn multi_gateway(&self) -> bool {
        !self.parked_gws.is_empty()
    }

    /// A gateway that came online on the plane: known from now on, initialised when it is
    /// activated and its `init_modem` runs. The radio it starts from is the effective one.
    pub fn add_gateway(&mut self, gw: &str) {
        if self.has_gateway(gw) {
            return;
        }
        let state = GwState::fresh(self.radio, self.tx_lead.estimate_ms());
        if let Some(board) = &self.board {
            board.online(gw, state.nb.clone(), state.liveness.clone());
        }
        self.parked_gws.insert(gw.to_string(), state);
        diag!("gateway {gw} online");
    }

    /// Make `gw`'s state the active one. `false` for a gateway the central does not know.
    pub fn activate(&mut self, gw: &str) -> bool {
        if self.active == gw {
            return true;
        }
        let Some(mut next) = self.parked_gws.remove(gw) else {
            return false;
        };
        self.swap_gateway(&mut next);
        let was = std::mem::replace(&mut self.active, gw.to_string());
        self.parked_gws.insert(was, next);
        true
    }

    /// A gateway gone offline or dead: its state is dropped (its signal statistics stay). The
    /// local gateway is never removed.
    pub fn remove_gateway(&mut self, gw: &str) {
        if gw == LOCAL || !self.has_gateway(gw) {
            return;
        }
        if self.active == gw {
            self.activate(LOCAL);
        }
        self.parked_gws.remove(gw);
        if let Some(board) = &self.board {
            board.offline(gw, now_unix_i(), "gateway offline");
        }
        diag!("gateway {gw} offline");
    }

    /// The name a reception is reported under: this Hub's hostname for its own Northbridge.
    fn gw_label(&self) -> String {
        if self.active == LOCAL {
            crate::northbound::hostname()
        } else {
            self.active.clone()
        }
    }

    /// The soonest ring ack any gateway owes.
    pub fn ack_deadline_any(&self) -> Option<Instant> {
        self.parked_gws
            .values()
            .filter_map(|g| g.delivery.deadline())
            .chain(self.delivery.deadline())
            .min()
    }

    /// Issue counter blocks of `size`: the test seam of [`p2p_db::COUNTER_BLOCK_ENV`], which a
    /// process reads once and a test cannot set per case.
    #[cfg(test)]
    fn with_counter_block(mut self, size: u32) -> Self {
        self.ctr_block = size;
        self
    }

    /// Store northbound messages in `outbox` — the host's, opened once and drained by the one
    /// [`Outbox::run_drainer`] it started, so a restart of the central abandons no publish.
    /// Without it the central opens its own and, having no drainer, publishes in place after
    /// each event (tests, a harness).
    pub fn with_outbox(mut self, outbox: std::sync::Arc<Outbox>) -> Self {
        self.outbox_cell = std::sync::OnceLock::from(outbox);
        self
    }

    /// The outbox this central stores northbound messages in.
    pub fn outbox(&self) -> std::sync::Arc<Outbox> {
        self.outbox_ref().clone()
    }

    fn outbox_ref(&self) -> &std::sync::Arc<Outbox> {
        self.outbox_cell
            .get_or_init(|| std::sync::Arc::new(Outbox::open(&self.store)))
    }

    /// A handle on the modem's last sign of life, for the host's health loop. Take it before
    /// the central is moved into [`run_central`].
    pub fn liveness(&self) -> Liveness {
        self.liveness.clone()
    }

    /// Report the modem's signs of life into `liveness` — a handle the host created once and
    /// keeps across the centrals its supervisor builds, so a restart does not orphan the one
    /// its health loop reads.
    pub fn with_liveness(mut self, liveness: Liveness) -> Self {
        self.liveness = liveness;
        self
    }

    /// Feed `status` — the host's handle, kept across the centrals its supervisor builds.
    pub fn with_status(mut self, status: crate::p2p::northbridge::NorthbridgeStatus) -> Self {
        self.nb = status;
        self
    }

    /// Keep `board` — the host's handle, kept across the centrals its supervisor builds — up to
    /// date with the gateways (§8).
    pub fn with_board(mut self, board: crate::p2p::gw_board::GatewayBoard) -> Self {
        self.board = Some(board);
        self
    }

    /// Refresh the gateways' rows on the board: the local one's handles, every online one's
    /// homes and link round trip.
    pub fn update_board(&self) {
        let Some(board) = &self.board else {
            return;
        };
        let live = match self.joined_on_view() {
            Ok(live) => live,
            Err(e) => {
                diag!(
                    "could not read sessions.db for the gateways document: {}",
                    e.message()
                );
                return;
            }
        };
        let mut homes: BTreeMap<&str, usize> = BTreeMap::new();
        for s in live.values() {
            *homes.entry(gateways::home_of(s)).or_default() += 1;
        }
        let rows = std::iter::once((
            self.active.as_str(),
            self.link_rtt_ms,
            &self.nb,
            &self.liveness,
        ))
        .chain(
            self.parked_gws
                .iter()
                .map(|(gw, g)| (gw.as_str(), g.link_rtt_ms, &g.nb, &g.liveness)),
        );
        for (gw, rtt, nb, liveness) in rows {
            if !board.has(gw) {
                board.online(gw, nb.clone(), liveness.clone());
            }
            board.set_figures(gw, homes.get(gw).copied().unwrap_or(0), rtt);
        }
    }

    /// A handle on the northbridge status document this central feeds.
    pub fn status(&self) -> crate::p2p::northbridge::NorthbridgeStatus {
        self.nb.clone()
    }

    /// Assign a TX power to every node at join time (F5). 0 assigns nothing. Builder form for
    /// tests; production reads it from `radio.p2p_radio.node_tx_power_dbm`.
    /// Above 14 dBm the JoinAccept carries 14 (#486 W8, [`control::assigned_node_tx_power`]).
    pub fn with_node_tx_power(mut self, dbm: u8) -> Self {
        if dbm > control::NODE_TX_POWER_MAX_DBM {
            diag!(
                "radio.p2p_radio.node_tx_power_dbm = {dbm}: a Node takes at most {} dBm from its \
                 JoinAccept and ignores more, so {} is assigned",
                control::NODE_TX_POWER_MAX_DBM,
                control::NODE_TX_POWER_MAX_DBM
            );
        }
        self.node_tx_power_dbm = dbm;
        self
    }

    /// Retire a history replay after `seconds` without a frame. Builder form for tests and the
    /// bench; production reads `radio.p2p_radio.history_stall_timeout_s`.
    pub fn with_history_stall_timeout(mut self, seconds: u64) -> Self {
        self.history_stall_timeout_s = seconds;
        self
    }

    /// Bind the decoder table (§4). [`Central::new`] seeds the single `default → node` entry
    /// the shipped config declares; [`Central::from_config`] replaces it with the rendered one.
    pub fn with_p2p_profiles(mut self, profiles: BTreeMap<String, config::P2pProfile>) -> Self {
        self.p2p_profiles = profiles;
        self
    }

    fn next_tx_id(&mut self) -> u8 {
        self.tx_id = self.tx_id.wrapping_add(1);
        self.tx_id
    }

    /// The next item id of a `0x91` answer push: `0x8000..=0xFFFF`, wrapping. The lower half
    /// belongs to [`command_item`], so an answer never replaces a queued command.
    fn next_item(&mut self) -> u16 {
        self.item = self.item.wrapping_add(1).max(ANSWER_ITEM_BASE);
        self.item
    }

    /// Take the modem's clock against the host's, from a `GET_INFO` answer. A remote gateway's
    /// answer carries the instant its Hub read it (`p2p_cluster_gateway.md` §6), which leaves
    /// the plane's latency out of the anchor; a stamp more than [`ANCHOR_STAMP_MAX_SKEW`] off
    /// this Hub's clock is a Client clock not to trust, and the local instant stands.
    fn anchor_clock(&mut self, modem_now_ms: u64, rsp_host_ms: Option<u64>) {
        let now = now_unix_ms();
        let host_ms = rsp_host_ms
            .filter(|t| t.abs_diff(now) <= ANCHOR_STAMP_MAX_SKEW.as_millis() as u64)
            .unwrap_or(now);
        self.clock_anchor = Some((modem_now_ms, host_ms));
    }

    /// `TWR_TIME_SYNC`'s `(now_ms, unix_ms)` at host Unix ms `host_ms`: the Northbridge's clock
    /// at that instant through the anchor, and the host clock (`p2p_tower_gateway.md` §13.5).
    /// The gateway then maps a `t_ms` to the wall time the central's [`Self::wall_ms_at`]
    /// gives. `None` before the first anchor and while the host clock is unset.
    fn time_sync_values(&self, host_ms: u64) -> Option<(u64, u64)> {
        let (modem_ms, anchor_host_ms) = self.clock_anchor?;
        if host_ms / 1000 < u64::from(PLAUSIBLE_CLOCK_FLOOR) {
            return None;
        }
        // A host clock stepped back past the anchor pairs the anchor's own instant with it:
        // the gateway's clock never runs backwards.
        let now_ms = modem_ms.checked_add(host_ms.saturating_sub(anchor_host_ms))?;
        Some((now_ms, host_ms))
    }

    /// Hand the gateway the central's clock anchor (`TWR_TIME_SYNC`, §13.5), so the `TimeAns`
    /// it builds into its ACK matches the central's time. Only to a gateway that answers in
    /// its ACK; a failure is counted and logged, never fatal: the next probe or the 10 min
    /// tick tries again.
    async fn time_sync<M: Modem>(&mut self, modem: &mut M) {
        if !self.answers_in_ack {
            return;
        }
        let Some((now_ms, unix_ms)) = self.time_sync_values(now_unix_ms()) else {
            diag!("TWR_TIME_SYNC not sent: no clock anchor or the Hub's clock is not set");
            return;
        };
        self.time_sync_at = Some(Instant::now());
        self.time_sync_wanted = false;
        match modem.twr(&TwrCmd::TimeSync { now_ms, unix_ms }).await {
            Ok(_) => {
                self.nb.on_time_sync(true);
                diag!("TWR_TIME_SYNC now_ms={now_ms} unix_ms={unix_ms}");
            }
            Err(e) => {
                self.nb.on_time_sync(false);
                diag!(
                    "TWR_TIME_SYNC refused ({}) — the gateway's TimeAns waits for the next one",
                    e.message()
                );
            }
        }
    }

    /// The 10 min floor of `TWR_TIME_SYNC` (§13.5), from the tick: due when the last try is
    /// [`TIME_SYNC_EVERY`] old, or there was none — or [`TIME_SYNC_RETRY`] old once an ACK
    /// showed the gateway without an anchor. Sent through a fresh `GET_INFO` (the reconcile
    /// probe, which re-anchors and syncs), never from an old anchor: the gateway refuses a
    /// `now_ms` ahead of its clock, and an anchor minutes old has drifted. A gateway busy
    /// with a `TX_SCHEDULE` is not probed; the next tick tries again.
    pub async fn time_sync_if_due<M: Modem>(&mut self, modem: &mut M, now: Instant) -> Result<()> {
        if !self.answers_in_ack {
            return Ok(());
        }
        let due = self.time_sync_at.is_none_or(|t| {
            let age = now.saturating_duration_since(t);
            age >= TIME_SYNC_EVERY || (self.time_sync_wanted && age >= TIME_SYNC_RETRY)
        });
        if !due {
            return Ok(());
        }
        self.last_info_at = None;
        self.probe_if_idle(modem).await
    }

    /// The wall time, Unix ms, of the modem's `t_ms` through the last clock anchor; `None`
    /// before the first `GET_INFO`, or while the host clock is unset ([`PLAUSIBLE_CLOCK_FLOOR`]).
    fn wall_ms_at(&self, t_ms: u64) -> Option<u64> {
        let (modem_ms, host_ms) = self.clock_anchor?;
        let wall = if t_ms >= modem_ms {
            host_ms.checked_add(t_ms - modem_ms)?
        } else {
            host_ms.checked_sub(modem_ms - t_ms)?
        };
        (wall / 1000 >= u64::from(PLAUSIBLE_CLOCK_FLOOR)).then_some(wall)
    }

    /// When the gateway received a frame it stamped `t_ms`, Unix seconds: the `TimeAns` mapping
    /// ([`Self::wall_ms_at`]) through the last `GET_INFO` anchor. An event the gateway held in
    /// its ring while the central was away keeps the time it was heard, not the time the
    /// central got round to it (T1-F1).
    ///
    /// `now` when the mapping has nothing to say: no anchor yet, an unset host clock, or a time
    /// ahead of `now` — an anchor a reboot invalidated before a `GET_INFO` caught it (an
    /// `EVT_BOOT` and the missed-reboot probe both re-anchor). Each such fallback is counted
    /// (`rx_time_fallback`).
    fn received_at_unix(&self, t_ms: u64, now: i64) -> i64 {
        /// How far ahead of the host clock a mapped time may be and still be believed: the
        /// mapping's own jitter (UART latency, ms rounding).
        const AHEAD_SLACK_S: i64 = 2;
        match self
            .wall_ms_at(t_ms)
            .and_then(|ms| i64::try_from(ms / 1000).ok())
            .filter(|t| *t <= now.saturating_add(AHEAD_SLACK_S))
        {
            Some(t) => t.min(now),
            None => {
                self.nb.on_rx_time_fallback();
                now
            }
        }
    }

    /// The effective spreading factor and its generation, read from `sessions.db`.
    ///
    /// Read **without** the session-DB lock, deliberately: [`Store::write`] replaces the slot by
    /// `rename`, so a reader always sees one complete document — at worst the previous one, which
    /// the next event or the tick picks up. Taking the lock here would put a `flock` on the path
    /// every single event walks, to gain nothing.
    fn stored_effective_sf(&self) -> (u8, u64) {
        match SessionsDb::load(&self.store) {
            Ok(db) => match db.radio {
                Some(r) => (r.spreading_factor, r.generation),
                // No override: the config key, at generation 0. `--reset` lands here too.
                None => (self.config_sf, 0),
            },
            Err(e) => {
                diag!(
                    "sessions.db unreadable while looking for a radio-set override ({}) — \
                     staying on sf{}",
                    e.message(),
                    self.radio.sf
                );
                (self.radio.sf, self.applied_generation)
            }
        }
    }

    /// Adopt the stored override into [`Self::radio`] without touching the modem — what a
    /// startup or an `EVT_BOOT` re-init needs, since both push the whole config anyway.
    fn adopt_effective_sf(&mut self) {
        let (sf, generation) = self.stored_effective_sf();
        match self.radio.with_spreading_factor(sf) {
            Ok(next) => self.radio = next,
            // Only a hand-edited `sessions.db` reaches this — `radio-set` validates the range.
            // The generation is marked applied regardless, so it is said once rather than on
            // every event until someone fixes the file.
            Err(e) => diag!("ignoring the recorded spreading factor: {}", e.message()),
        }
        self.applied_generation = generation;
    }

    /// Push a pending `radio-set` change to the modem: `RX_STOP` → `SET_RADIO_CONFIG` →
    /// `TWR_START`, which enters gateway receive again (p2p_tower_gateway.md §2.1).
    ///
    /// Stopping first is not cosmetic: the modem holds its radio config and its RX-active flag
    /// separately, and retuning a receiver that is mid-listen is what `INVALID_STATE` is for.
    ///
    /// Nothing here is fatal. A modem that refuses any of the three leaves the generation
    /// unapplied, so the next event — or the tick — tries again; a switch that aborted the loop
    /// would take the whole central down over an operator's typo.
    async fn apply_radio_override_if_changed<M: Modem>(&mut self, modem: &mut M) {
        let (sf, generation) = self.stored_effective_sf();
        if generation == self.applied_generation {
            return;
        }
        if let Some(tx_id) = self.tx_in_flight {
            diag!("radio-set gen {generation} (sf{sf}) deferred: tx_id={tx_id} is still in flight");
            return;
        }
        let next = match self.radio.with_spreading_factor(sf) {
            Ok(next) => next,
            Err(e) => {
                diag!("ignoring the recorded spreading factor: {}", e.message());
                self.applied_generation = generation;
                return;
            }
        };
        if let Err(e) = modem.rx_stop().await {
            diag!(
                "RX_STOP refused ({}) — sf{sf} not applied, retrying",
                e.message()
            );
            return;
        }
        if let Err(e) = modem.set_radio_config(&next).await {
            diag!(
                "SET_RADIO_CONFIG refused ({}) — staying on sf{}, retrying",
                e.message(),
                self.radio.sf
            );
            self.nb.on_modem_error(
                now_unix_i(),
                format!("SET_RADIO_CONFIG refused ({})", e.message()),
                true,
            );
            // Re-arm at the config the modem still holds: a central that gave up here would be
            // deaf until the next reboot.
            if let Err(e) = self.start_gateway(modem).await {
                diag!(
                    "TWR_START refused ({}) after a refused config — the central is not listening",
                    e.message()
                );
            }
            return;
        }
        self.radio = next;
        self.applied_generation = generation;
        self.nb.on_radio_config(self.radio);
        if let Err(e) = self.start_gateway(modem).await {
            diag!(
                "TWR_START refused ({}) — the central is not listening",
                e.message()
            );
            return;
        }
        diag!(
            "radio armed: {} Hz sf{} (config sf{}, radio-set gen {generation})",
            self.radio.freq_hz,
            self.radio.sf,
            self.config_sf
        );
    }

    /// `TWR_START`: the gateway's network, its ACK turnaround and downlink gap, and ACKs on. It
    /// enters gateway receive, so it closes every sequence that stopped RX (contract §2.1).
    async fn start_gateway<M: Modem>(&mut self, modem: &mut M) -> Result<()> {
        let start = TwrCmd::Start {
            net_id: self.net_id,
            turnaround_ms: link::TURNAROUND_MS,
            dl_gap_ms: link::DL_GAP_MS,
            flags: link::START_ACK_ENABLED,
        };
        if let Err(e) = modem.twr(&start).await {
            self.nb.on_modem_error(
                now_unix_i(),
                format!("TWR_START refused ({})", e.message()),
                true,
            );
            return Err(e);
        }
        Ok(())
    }

    /// Ask the modem for `GET_INFO` once per [`IDLE_PROBE_AFTER_S`]: on a silent link it is the
    /// probe that tells a quiet network from a dead modem ([`Liveness`]), on a busy one it
    /// refreshes the status document's counters (§3). Skipped while a frame waits in the modem's
    /// TX slot: that frame is aimed at a node's RX1 window and nothing should queue ahead of it.
    /// An unanswered probe is the dead link itself and ends the run — the host's supervisor
    /// reopens the UART with backoff.
    async fn probe_if_idle<M: Modem>(&mut self, modem: &mut M) -> Result<()> {
        let now = now_unix_s();
        let info_fresh = self
            .last_info_at
            .is_some_and(|t| now.saturating_sub(t) < IDLE_PROBE_AFTER_S);
        if self.tx_in_flight.is_some() || info_fresh {
            return Ok(());
        }
        let asked = Instant::now();
        match modem.get_info().await {
            Ok(info) => {
                self.link_rtt_ms = u64::try_from(asked.elapsed().as_millis()).ok();
                self.liveness.mark(now);
                self.last_info_at = Some(now);
                // A clock that went back is a gateway that rebooted with its `EVT_BOOT` lost
                // (link doc §6.3): it holds nothing, so it is restored as on a boot (§6.2).
                let rebooted = self
                    .clock_anchor
                    .is_some_and(|(modem_ms, _)| info.now_ms < modem_ms);
                if rebooted {
                    self.nb.on_info(info, now_unix_i());
                    diag!(
                        "the gateway's clock went back to {} ms — it rebooted unseen; restoring it",
                        info.now_ms
                    );
                    self.delivery.restart();
                    self.held.clear();
                    return self.init_modem(modem).await;
                }
                self.anchor_clock(info.now_ms, modem.last_rsp_host_unix_ms());
                self.answers_in_ack = info.answers_in_ack();
                self.delivery.set_ring(info.evt_ring_max > 0);
                self.apply_head(info.evt_head_seq);
                let short = gateway_counters_short(&info, self.ctr_block);
                self.nb.on_info(info, now_unix_i());
                // The reconcile probe re-anchors: the gateway's anchor follows (§13.5).
                self.time_sync(modem).await;
                if short {
                    // EVT_TWR_CTR_LOW is best-effort (link doc §5.1): a lost one is caught here.
                    self.issue_counter_block_or_log(modem, "a short block in GET_INFO")
                        .await;
                }
                Ok(())
            }
            Err(e) => {
                diag!(
                    "modem did not answer the GET_INFO probe: {} ({:?}) — stopping",
                    e.message(),
                    e.kind()
                );
                self.nb.unreachable(
                    now_unix_i(),
                    format!("GET_INFO probe unanswered: {}", e.message()),
                );
                Err(e)
            }
        }
    }

    /// The restore of link doc §6.2 / §6.3 (contract §3.4): `GET_INFO` → `SET_RADIO_CONFIG` →
    /// the `TWR_NODE_LIST` sweep → `TWR_CTR_BLOCK` → one `TWR_NODE_ADD` (`HOME | KEEP_NEWER`,
    /// the flag only to a gateway with a ring, §8.7) per joined Node on the P2P view →
    /// `TWR_START` → the downlinks pushed again. Run at startup and on every `EVT_BOOT`.
    ///
    /// - A rebooted gateway (`BOOT`, §6.2) holds no config, no peers, no queue and no
    ///   counters: no sweep, there is nothing to sweep. Its ring numbers from 1, so it is not
    ///   learned (§8.7).
    /// - One that kept running across a restart of the central (`GATEWAY` with this radio
    ///   config and network, §6.3) is not reconfigured: no `SET_RADIO_CONFIG`, no `TWR_START`.
    ///   It gets a new block, the peers with `KEEP_NEWER` so its newer `last_seen` stands, and
    ///   the pending downlinks again (idempotent by item); then it resends its ring.
    pub async fn init_modem<M: Modem>(&mut self, modem: &mut M) -> Result<()> {
        // The operator's override first, so the config this pushes is the effective one. On an
        // `EVT_BOOT` this is also what re-applies a switch the rebooted modem just forgot.
        self.adopt_effective_sf();
        // A rebooted modem holds nothing in its single TX slot, and the frame that was there will
        // never report `EVT_TX_DONE` — so nothing waiting behind it will be re-issued either.
        self.tx_in_flight = None;
        self.tx_aim = None;
        let asked = Instant::now();
        let info = modem.get_info().await?;
        self.link_rtt_ms = u64::try_from(asked.elapsed().as_millis()).ok();
        self.liveness.mark(now_unix_s());
        self.last_info_at = Some(now_unix_s());
        self.anchor_clock(info.now_ms, modem.last_rsp_host_unix_ms());
        self.answers_in_ack = info.answers_in_ack();
        self.delivery.set_ring(info.evt_ring_max > 0);
        let head = info.evt_head_seq;
        self.nb.on_info(info, now_unix_i());
        // Right after the `GET_INFO` it is taken from (§7.2, §7.3, §13.1): the anchor is
        // freshest here, and a rebooted gateway holds none until it gets this.
        self.time_sync(modem).await;
        if !self.answers_in_ack {
            diag!(
                "the gateway answers no LinkCheckReq / TimeReq in its ACK (GET_INFO < 180 B): \
                 the central answers them"
            );
        }
        if let Some(n) = p2p_db::counter_block_override().filter(|n| *n == self.ctr_block) {
            diag!(
                "WARNING: TEST/BENCH counter blocks of {n} ({}), not {} — remove the override \
                 outside a HIL run",
                p2p_db::COUNTER_BLOCK_ENV,
                p2p_db::COUNTER_BLOCK
            );
        }
        if !self.delivery.ring() {
            diag!("the gateway keeps no event ring (evt_ring_max 0): ring events are not acked");
        }
        diag!(
            "modem GET_INFO ok: fw={}.{}.{} state={} rx={} tx={} crc_err={}",
            info.fw[0],
            info.fw[1],
            info.fw[2],
            crate::p2p::modem_io::gateway_state::name(info.state),
            info.counter("rx_total"),
            info.counter("tx_total"),
            info.counter("radio_crc_err")
        );
        diag!(
            "RX1 aim: rx1_delay {} s, answer starts {:+} ms after the nominal RX1, modem TX lead \
             {:.1} ms ({} measured)",
            self.rx1_delay_s,
            modem::rx1_ack_start_ms(),
            self.tx_lead.estimate_ms(),
            self.tx_lead.samples()
        );
        // The modem's clock is its own uptime, and a reboot restarts it at zero. Without this the
        // pre-reboot high-water makes every frame from the restarted modem look stale — and since
        // `observe_modem_clock` only runs on frames that are *not* judged stale, the high-water
        // can never fall back on its own and the central stays deaf for as long as the modem's
        // previous uptime. `GET_INFO` is the one place that reports where the clock actually is.
        //
        // An older modem whose reply is too short to carry `now_ms` yields 0 here, which is the
        // same as having no reference at all — the pre-#0 behaviour, and safe.
        self.newest_modem_t_ms = Some(info.now_ms);
        self.in_stale_burst = false;
        // A Northbridge that kept running across a central restart is still in `GATEWAY`,
        // where `SET_RADIO_CONFIG` is `INVALID_STATE` (link doc §6.1). Holding the config
        // already, it is left receiving (§6.3); holding another, it is stopped first.
        let running = info.state == crate::p2p::modem_io::gateway_state::GATEWAY;
        let booted = info.state == crate::p2p::modem_io::gateway_state::BOOT;
        let keeps_config = running && info.radio == self.radio;
        // A gateway still in `boot` has not started its radio since it booted, so its ring is
        // empty and numbers from 1: known, nothing to learn — whether or not this central saw
        // the `EVT_BOOT` (a central restarted across the gateway's reboot, an `EVT_BOOT` lost,
        // a missed reboot). Learning is for a gateway found running (link doc §4.1, §8.7).
        if booted {
            self.delivery.booted();
            self.held.clear();
        }
        // A gateway that reports its ring's head needs no learning at all (§8.11). What was
        // held is processed with the next ring event, once the restore is done.
        self.apply_head(head);
        // §6.3: a gateway left running on this config and this network needs no `TWR_START`
        // either; one on another network, or with other timings, is started again.
        let keeps_running = keeps_config
            && info.net_id == self.net_id
            && info.turnaround_ms == link::TURNAROUND_MS
            && info.dl_gap_ms == link::DL_GAP_MS;
        if keeps_config {
            diag!("the gateway kept running with this radio config — not reconfigured");
        } else {
            if running {
                if let Err(e) = modem.rx_stop().await {
                    diag!("RX_STOP before the radio config refused ({})", e.message());
                    return Err(e);
                }
            }
            if let Err(e) = modem.set_radio_config(&self.radio).await {
                self.nb.on_modem_error(
                    now_unix_i(),
                    format!("SET_RADIO_CONFIG refused ({})", e.message()),
                    true,
                );
                return Err(e);
            }
        }
        self.nb.on_radio_config(self.radio);

        // What the gateway held before is unknown here, and a `TWR_NODE_ADD` replaces a peer's
        // lane and drops its queue: nothing pushed earlier is pending any more.
        self.gw_table.clear();
        self.pushed.clear();
        self.answers.clear();
        let joined = self.joined_on_view()?;
        if !booted {
            self.sweep_gateway_peers(modem, &joined).await;
        }
        if let Err(e) = self.issue_counter_block(modem).await {
            self.nb.on_modem_error(
                now_unix_i(),
                format!("TWR_CTR_BLOCK failed ({})", e.message()),
                true,
            );
            return Err(e);
        }
        for s in joined.values() {
            self.add_peer(modem, s, true).await;
        }
        if !keeps_running {
            self.start_gateway(modem).await?;
        }
        // Only the heads this gateway sends: another gateway's stay where they were pushed.
        if self.multi_gateway() {
            let homed: Vec<String> = joined
                .values()
                .filter(|s| gateways::home_of(s) == self.active)
                .map(|s| s.dev_eui.clone())
                .collect();
            self.requeue_announced(Some(&homed));
        } else {
            self.requeue_announced(None);
        }
        self.push_due_answers(modem).await;
        self.push_idle_heads(modem).await;
        diag!(
            "radio armed: {} Hz sf{} bw={} cr={} preamble={} net_id={:#010x} (config sf{}, \
             radio-set gen {})",
            self.radio.freq_hz,
            self.radio.sf,
            self.radio.bw,
            self.radio.cr,
            self.radio.preamble,
            self.net_id,
            self.config_sf,
            self.applied_generation
        );
        diag!(
            "gateway resynced: {} peer(s), {} downlink(s) pushed",
            self.gw_table.len(),
            self.pushed.len()
        );
        Ok(())
    }

    /// Handle one modem event. Per-frame problems (an unknown serial, a bad tag, a decrypt
    /// failure, a `BUSY`/`LATE` TX) are logged and swallowed here — they must never stop the
    /// loop. Only a dead link (surfaced by [`Modem::next_event`]) ends the run.
    pub async fn on_event<M: Modem>(&mut self, modem: &mut M, ev: Event) -> Result<()> {
        // Any event is the modem talking, whatever it says.
        self.liveness.mark(now_unix_s());
        self.nb.on_event(now_unix_i());
        match ev {
            Event::Fault(f) => {
                diag!(
                    "EVT_LOG {} code={} ({}): {}",
                    if f.level == crate::p2p::modem::FAULT_LEVEL_ERROR {
                        "ERROR"
                    } else {
                        "WARNING"
                    },
                    f.code,
                    crate::p2p::northbridge::fault_code_name(f.code),
                    f.text.trim_end()
                );
                self.nb.on_fault(&f, now_unix_i());
            }
            Event::Boot(b) => {
                self.nb.on_boot(&b, now_unix_i());
                diag!(
                    "EVT_BOOT fw={}.{}.{} reset_cause={} — re-initialising",
                    b.fw_ver[0],
                    b.fw_ver[1],
                    b.fw_ver[2],
                    b.reset_cause
                );
                // A rebooted gateway numbers its ring afresh from 1, and what it held is gone.
                self.delivery.booted();
                self.held.clear();
                self.init_modem(modem).await?;
            }
            // Ring events (link doc §4.1): processed once, in order, and acked only once what
            // they did is stored. One whose handling failed is not acked: the gateway resends.
            ev @ (Event::TwrUplink { .. } | Event::TwrTx { .. }) => {
                self.on_ring_event(modem, ev).await;
                self.ack_if_due(modem, Instant::now()).await;
                self.apply_radio_override_if_changed(modem).await;
            }
            Event::TwrCtrLow(low) => self.on_ctr_low(modem, low).await,
            Event::Log(s) => {
                diag!("EVT_LOG: {}", s.trim_end());
                // A link with nothing on it still gets its `EVT_LOG`s; the 2 s tick in
                // [`run_central`] covers a link that is silent even of those.
                self.apply_radio_override_if_changed(modem).await;
            }
            Event::TxDone(d) => {
                self.observe_modem_clock(d.t_actual_ms);
                diag!(
                    "EVT_TX_DONE tx_id={} status={:?} t_actual_ms={} (t_actual includes time-on-air)",
                    d.tx_id,
                    d.status,
                    d.t_actual_ms
                );
                // The slot is free again, so a deferred spreading-factor switch can go now.
                // Matched on `tx_id`: a late `EVT_TX_DONE` for an older frame must not clear the
                // one that is actually in flight.
                if self.tx_in_flight == Some(d.tx_id) {
                    self.tx_in_flight = None;
                }
                if d.status == crate::p2p::modem::TxStatus::RadioErr {
                    self.nb.on_modem_error(
                        now_unix_i(),
                        format!("TX radio error (tx_id={})", d.tx_id),
                        true,
                    );
                }
                self.learn_tx_lead(&d);
                self.apply_radio_override_if_changed(modem).await;
            }
            Event::Rx(rx) => {
                if !self.is_stale_rx(&rx) {
                    self.observe_modem_clock(rx.t_ms);
                    if let Err(e) = self.on_rx(modem, rx).await {
                        diag!("RX handling error: {} ({:?})", e.message(), e.kind());
                    }
                }
                // Only now, with the frame answered: the node that just transmitted is holding
                // its RX1 window open at the SF it sent on, so retuning first would send the ACK
                // where nobody is listening. Handling the Rx first also puts that ACK in the TX
                // slot, which defers the switch to the `EVT_TX_DONE` that frees it.
                self.apply_radio_override_if_changed(modem).await;
            }
        }
        Ok(())
    }

    /// One ring event as it arrives: processed, dropped, or — while the start of the ring is
    /// being learned — held, and the held ones processed in `seq` order once it is known.
    async fn on_ring_event<M: Modem>(&mut self, modem: &mut M, ev: Event) {
        let Some(seq) = ring_seq(&ev) else {
            return;
        };
        self.settle_ring(modem, Instant::now()).await;
        match self.delivery.admit(seq, Instant::now()) {
            Admit::Hold => {
                // The first copy is kept: a resend carries the same event.
                self.held.entry(seq).or_insert(ev);
                if self.head_reported {
                    // A gap relearn on a gateway that reports its head: ask, never learn.
                    match modem.get_info().await {
                        Ok(info) => self.apply_head(info.evt_head_seq),
                        Err(e) => diag!(
                            "GET_INFO for the ring's head failed: {} — learning it",
                            e.message()
                        ),
                    }
                    self.drain_held(modem).await;
                }
                self.settle_ring(modem, Instant::now()).await;
            }
            Admit::Process => self.process_ring(modem, seq, ev).await,
            dropped => self.log_dropped(seq, ring_what(&ev), dropped),
        }
    }

    /// End learning when it is due ([`Delivery::settle`]: a `seq` arrived twice, or
    /// [`LEARN_FOR`] passed) and process what was held, in `seq` order from the start it
    /// found — never in arrival order: a central that opened the link mid resend pass got the
    /// newer half of the pass first, and the `(addr, counter)` replay rule would then drop the
    /// older half as stale (`p2p_tower_gateway.md` §8.10, T3 on c66).
    async fn settle_ring<M: Modem>(&mut self, modem: &mut M, now: Instant) {
        if !self.held.is_empty() && !self.delivery.learning() {
            // Known by other means meanwhile: the gateway's head.
            self.drain_held(modem).await;
            return;
        }
        let Some(start) = self.delivery.settle(now) else {
            return;
        };
        diag!(
            "event ring: learned the start seq={start} from {} held event(s)",
            self.held.len()
        );
        self.drain_held(modem).await;
    }

    /// Process the held ring events in `seq` order from the next expected one, now that the
    /// receiver knows it.
    async fn drain_held<M: Modem>(&mut self, modem: &mut M) {
        let Some(last) = self.delivery.last() else {
            return;
        };
        let start = last.wrapping_add(1);
        let mut held = std::mem::take(&mut self.held);
        let order: Vec<u8> = held
            .keys()
            .map(|s| s.wrapping_sub(start))
            .collect::<std::collections::BTreeSet<u8>>()
            .into_iter()
            .map(|d| d.wrapping_add(start))
            .collect();
        for seq in order {
            let Some(ev) = held.remove(&seq) else {
                continue;
            };
            match self.delivery.admit(seq, Instant::now()) {
                Admit::Process => self.process_ring(modem, seq, ev).await,
                dropped => self.log_dropped(seq, ring_what(&ev), dropped),
            }
        }
    }

    /// Take the ring's head from a `GET_INFO` (Northbridge ≥ 0.3.1 final, link doc §4.1): the
    /// next expected `seq`, no learning (`p2p_tower_gateway.md` §8.11). A 171 B reply has none,
    /// and the receiver learns as before.
    fn apply_head(&mut self, head: Option<u8>) {
        self.head_reported = head.is_some();
        if let Some(head) = head {
            let was = self.delivery.last();
            self.delivery.expect(head);
            if self.delivery.last() != was {
                diag!("event ring: the gateway's head is seq={head} — expected next");
            }
        }
    }

    /// Process an admitted ring event and, once its effects are stored, ack it.
    async fn process_ring<M: Modem>(&mut self, modem: &mut M, seq: u8, ev: Event) {
        if !self.outbox_has_room(seq) {
            return;
        }
        self.ring_pending = Some(seq);
        let heard_ms = match &ev {
            Event::TwrUplink { up, .. } => self.wall_ms_at(up.t_ms),
            _ => None,
        };
        self.ring_trace = Some(RingTrace::new(seq, heard_ms));
        let what = ring_what(&ev);
        let result = match ev {
            Event::TwrUplink { up, .. } => self.on_twr_uplink(modem, up).await,
            Event::TwrTx { tx, .. } => self.on_twr_tx(modem, tx).await,
            _ => Ok(()),
        };
        match result {
            // Mostly marked already, right after its save: a path that stores nothing is done
            // here.
            Ok(()) => self.ring_stored(modem).await,
            Err(e) => {
                self.ring_pending = None;
                self.ring_trace = None;
                diag!(
                    "{what} seq={seq} handling error: {} ({:?}) — not acked, the gateway sends \
                     it again",
                    e.message(),
                    e.kind()
                );
            }
        }
    }

    /// Log a ring event the receiver dropped (link doc §4.1).
    fn log_dropped(&self, seq: u8, what: &str, admit: Admit) {
        match admit {
            Admit::Duplicate => {
                diag!("{what} seq={seq}: a resend of one already processed — dropped");
            }
            Admit::Gap => diag!(
                "{what} seq={seq}: ahead of the next expected (after {:?}) — dropped, the \
                 gateway resends from there",
                self.delivery.last()
            ),
            // Only while learning, and those are held, not dropped.
            Admit::Process | Admit::Hold => {}
        }
    }

    /// Whether the outbox can take what the ring event `seq` may publish. When it cannot, the
    /// event is not processed at all — nothing of it is stored, so nothing of it is acked, and
    /// the gateway holds it and sends it again (backpressure, `p2p_tower_gateway.md` §8.6).
    fn outbox_has_room(&mut self, seq: u8) -> bool {
        if self.outbox_ref().has_room(outbox::EVENT_MAX) {
            if self.outbox_full {
                diag!("the northbound outbox has room again — processing ring events");
                self.outbox_full = false;
            }
            return true;
        }
        if !self.outbox_full {
            diag!(
                "ring event seq={seq}: the northbound outbox is full ({} of {} messages wait \
                 for the broker) — not processed, not acked; the gateway holds it",
                self.outbox_ref().pending(),
                self.outbox_ref().capacity()
            );
            self.outbox_full = true;
        }
        false
    }

    /// The ring event being handled has its effects stored: mark it done and send the ack it
    /// is owed at once, before anything else the handler still waits for (queue pushes, the
    /// cache, the publish) — a 100 ms pacing deadline cannot fire while the handler holds the
    /// loop, and the gateway resends what it has no ack for within a few hundred ms
    /// (`p2p_tower_gateway.md` §8.8). `TWR_EVT_ACK` has no response, so it costs one write.
    async fn ring_stored<M: Modem>(&mut self, modem: &mut M) {
        let Some(seq) = self.ring_pending.take() else {
            return;
        };
        let now = Instant::now();
        self.delivery.done(seq, now);
        if let Some(ack) = self.delivery.owed(now) {
            self.send_ack(modem, ack).await;
        }
        if let Some(trace) = self.ring_trace.take() {
            if let Some(line) = trace.slow_line(Instant::now(), now_unix_ms()) {
                diag!("{line}");
            }
        }
    }

    /// Mark the end of one step of the ring event being handled ([`RingTrace`]).
    fn ring_step(&mut self, what: &'static str) {
        if let Some(t) = self.ring_trace.as_mut() {
            t.steps.push((what, Instant::now()));
        }
    }

    /// Bring the status document's delivery counters and outbox gauge up to date.
    fn report_counters(&mut self) {
        let now = self.delivery.stats();
        let was = self.delivery_reported;
        self.nb.add_delivery(
            now.evt_relearn.saturating_sub(was.evt_relearn),
            now.evt_seq_gap.saturating_sub(was.evt_seq_gap),
            now.evt_seq_gap_seen.saturating_sub(was.evt_seq_gap_seen),
        );
        self.delivery_reported = now;
        self.nb.set_outbox(
            self.outbox_ref().pending() as u64,
            self.outbox_ref().overflow(),
        );
    }

    /// Send the `TWR_EVT_ACK` the pacing says is due at `now` (link doc §4.1): eight ring
    /// events owed one, or 100 ms since the first of them. [`run_central`] calls it on the
    /// deadline [`Self::ack_deadline`] gives.
    pub async fn ack_if_due<M: Modem>(&mut self, modem: &mut M, now: Instant) {
        self.settle_ring(modem, now).await;
        if let Some(seq) = self.delivery.due(now) {
            self.send_ack(modem, seq).await;
        }
        self.report_counters();
    }

    /// When an owed ack falls due, or learning where the gateway's ring starts ends.
    pub fn ack_deadline(&self) -> Option<Instant> {
        self.delivery.deadline()
    }

    /// Send any owed ack now, paced or not: a central that is stopping leaves its successor
    /// fewer resends.
    pub async fn ack_owed<M: Modem>(&mut self, modem: &mut M) {
        if let Some(seq) = self.delivery.owed(Instant::now()) {
            self.send_ack(modem, seq).await;
        }
    }

    async fn send_ack<M: Modem>(&mut self, modem: &mut M, seq: u8) {
        match modem.twr(&TwrCmd::EvtAck { seq }).await {
            Ok(_) => self.delivery.sent(),
            // The gateway resends what it has no ack for, and the next ack covers it.
            Err(e) => diag!("TWR_EVT_ACK seq={seq} not sent: {}", e.message()),
        }
    }

    /// Advance the high-water mark of the modem's clock. Monotone: an out-of-order event never
    /// moves it backwards, or one stale frame would make the rest of the backlog look fresh.
    fn observe_modem_clock(&mut self, t_ms: u64) {
        if self.newest_modem_t_ms.is_none_or(|newest| t_ms > newest) {
            self.newest_modem_t_ms = Some(t_ms);
        }
        self.in_stale_burst = false;
    }

    /// Whether this `EVT_RX` is too far behind the modem's clock to be worth answering (D3).
    ///
    /// JoinRequests are judged by the same rule as data frames: a JoinAccept aimed into a closed
    /// window is as useless as a late ACK, and the node re-sends with a fresh nonce anyway.
    fn is_stale_rx(&mut self, rx: &RxEvent) -> bool {
        let Some(newest) = self.newest_modem_t_ms else {
            return false;
        };
        let budget = (self.rx1_delay_s * 1000).saturating_add_signed(modem::rx1_ack_start_ms())
            + STALE_RX_SLACK_MS;
        if newest.saturating_sub(rx.t_ms) <= budget {
            return false;
        }
        // One line per burst, not per frame: a reconnect can drain a whole buffer of these.
        if !self.in_stale_burst {
            self.in_stale_burst = true;
            diag!(
                "EVT_RX t_ms={} is {} ms behind the modem clock ({newest}) — its RX1 window has \
                 closed; dropping this and any that follow in the same backlog",
                rx.t_ms,
                newest - rx.t_ms
            );
        }
        true
    }

    /// The northbound messages of one uplink, in the order they are published.
    ///
    /// The LoRaWAN arm's order, now shared: the decoded uplink first, then whatever that uplink
    /// turned out to also be. A `command` document reports the frame — its index, its count,
    /// its completeness — so a consumer watching both areas is told what is coming before the
    /// records or the reassembled dump arrive. The outbox keeps this order: one event's
    /// messages are one entry, published head first.
    ///
    /// - One message per history frame, never coalesced, and only the first copy of a given
    ///   index — an ingest that credits a backfill job per frame would otherwise over-count a
    ///   retransmit and call the replay done early.
    /// - A `ConfigDump` publishes only when the page that completed it arrives — never a
    ///   half-collected one, which read back as the device's configuration would be worse
    ///   than no answer.
    fn uplink_messages(
        dev_eui: &str,
        to_publish: Option<(Area, serde_json::Value)>,
        history_publish: Option<serde_json::Value>,
        config_publish: Option<serde_json::Value>,
        device_info_publish: Option<serde_json::Value>,
        device_status_publish: Option<serde_json::Value>,
    ) -> Vec<outbox::Message> {
        let mut out = Vec::new();
        if let Some((area, payload)) = to_publish {
            out.push(outbox::Message::new(dev_eui, area, &payload));
        }
        if let Some(msg) = history_publish {
            out.push(outbox::Message::new(dev_eui, Area::History, &msg));
        }
        if let Some(dump) = config_publish {
            out.push(outbox::Message::new(dev_eui, Area::Config, &dump));
        }
        if let Some(info) = device_info_publish {
            out.push(outbox::Message::new(dev_eui, Area::DeviceInfo, &info));
        }
        if let Some(status) = device_status_publish {
            out.push(outbox::Message::new(dev_eui, Area::DeviceStatus, &status));
        }
        out
    }

    /// Have what the outbox holds published: the drainer's job when the host started one (it
    /// is woken), otherwise done here, in place, up to the first failure.
    ///
    /// Last in an event's handling, after the gateway has been told everything: the broker is
    /// never on the path of an answer or an ack. With a drainer it is not on the path at all.
    async fn flush_outbox(&mut self) {
        if self.outbox_ref().has_drainer() {
            self.outbox_ref().kick();
        } else {
            self.outbox_ref().drain_now(&self.publisher).await;
        }
    }

    /// The `node-get` cache — the same per-transport document the LoRaWAN arm fills, keyed by
    /// dev_eui under `p2p/cache/`. Written once the uplink's answer is scheduled and before the
    /// outbox is flushed, so the cache and the retained topic still agree (p2p_control_runtime.md,
    /// amended 2026-09-28, S-1). With the host's drainer running, the broker may get a document
    /// a moment before the cache does (p2p_tower_gateway.md §8.6).
    ///
    /// After the schedule, not before it: the cache is the `node-get` / retained view, not
    /// needed to answer the Node, and its fsync was one of the two that sat between a decoded
    /// uplink and an ACK that missed the Node's window on c59. `sessions.db` stays before the
    /// schedule, since it holds the anti-replay counter and the delivery marks.
    ///
    /// Under the session lock, taken again here: `node-send` writes the same document under
    /// `Slot::P2pSessions` (`command_seq`, `sent_commands`), so the two never interleave a
    /// read-modify-write. It may have written in the gap since this uplink released the lock,
    /// and [`Cache::merge_write`] keeps that: it reads the document as it is on disk and
    /// replaces only the slot it writes. Best-effort: a cache miss costs a `node-get` its
    /// answer, an event that is not acked costs the gateway's ring a slot. A crash
    /// before it loses this uplink's update, which the next uplink or dump brings back.
    fn flush_cache(
        &self,
        dev_eui: &str,
        received_at: &str,
        writes: &[(CacheSlot, serde_json::Value)],
    ) {
        if writes.is_empty() {
            return;
        }
        let _held = match self.store.lock(Slot::P2pSessions) {
            Ok(lock) => lock,
            Err(e) => {
                diag!(
                    "cache write for {dev_eui} skipped, the session lock failed (continuing): {}",
                    e.message()
                );
                return;
            }
        };
        let cache = Cache::new(&self.store, Transport::P2p);
        for (slot, value) in writes {
            if let Err(e) = cache.merge_write(dev_eui, *slot, received_at, value.clone()) {
                diag!("cache write failed (continuing): {}", e.message());
            }
        }
    }

    /// The joined sessions the central serves, by address: those whose Node is on the
    /// registry's P2P view, alone on its address. A parked session is left out, and so is an
    /// address two Nodes on the view share (contract §3.1): the central serves neither.
    ///
    /// A detaching session (p2p_control_commands.md §3) is served too while no registered
    /// Node uses its address: it has a Detach to deliver.
    fn live_sessions(db: &SessionsDb) -> BTreeMap<u32, Session> {
        db.sessions
            .values()
            .filter(|s| {
                if s.detach.is_some() {
                    db.node(&s.dev_eui).is_none() && db.nodes_on_addr(s.addr).is_empty()
                } else {
                    db.node(&s.dev_eui).is_some() && db.nodes_on_addr(s.addr).len() == 1
                }
            })
            .map(|s| (s.addr, s.clone()))
            .collect()
    }

    /// [`Self::live_sessions`] of the database as it is now. Read without the lock, as
    /// [`Self::stored_effective_sf`] is: a `rename` never shows half a document.
    fn joined_on_view(&self) -> Result<BTreeMap<u32, Session>> {
        Ok(Self::live_sessions(&SessionsDb::load(&self.store)?))
    }

    /// Hand one session to the gateway (`TWR_NODE_ADD`, home peer). A peer the gateway already
    /// holds is replaced, its queue with it, so nothing pushed for it earlier is pending any
    /// more. Returns whether the gateway took it; a refusal is logged and the tick tries again.
    ///
    /// `restore` sets `KEEP_NEWER` (link doc §4.2): a session the central already had goes
    /// back with its stored `last_seen`, which may be older than the gateway's if ring events
    /// were not processed yet, and the gateway keeps the newer of the two for the same key. A
    /// new join (D2) is not a restore: `last_seen` 0, a new key, no flag.
    ///
    /// Only to a gateway with an event ring (`evt_ring_max` > 0, Northbridge ≥ 0.3.1): 0.3.0
    /// refuses the flag (`BAD_PARAM`), and with no ring there are no unprocessed events whose
    /// counters it could hold newer (§8.7).
    async fn add_peer<M: Modem>(&mut self, modem: &mut M, s: &Session, restore: bool) -> bool {
        let session_key = match hex16(&s.session_key) {
            Ok(k) => k,
            Err(e) => {
                diag!(
                    "the session of {} holds no usable key ({}) — not handing it to the gateway",
                    s.dev_eui,
                    e.message()
                );
                return false;
            }
        };
        let keep_newer = restore && self.delivery.ring();
        // `HOME` on the Node's home only: every other gateway forwards its frames unACKed
        // (`p2p_cluster_gateway.md` §1).
        let home = gateways::home_of(s) == self.active;
        let mut flags = if home { link::NODE_HOME } else { 0 };
        if keep_newer {
            flags |= link::NODE_KEEP_NEWER;
        }
        let cmd = TwrCmd::NodeAdd {
            addr: s.addr,
            session_key,
            last_seen: s.last_seen,
            flags,
        };
        match modem.twr(&cmd).await {
            Ok(_) => {
                self.gw_table.insert(
                    s.addr,
                    GwPeer {
                        dev_eui: s.dev_eui.clone(),
                        key: s.session_key.clone(),
                        home,
                    },
                );
                self.pushed.remove(&s.dev_eui);
                diag!(
                    "TWR_NODE_ADD {} addr={:08x} last_seen={}{}{}{}",
                    s.dev_eui,
                    s.addr,
                    s.last_seen,
                    if keep_newer { " KEEP_NEWER" } else { "" },
                    if home { "" } else { " (not home)" },
                    if self.active == LOCAL {
                        String::new()
                    } else {
                        format!(" on {}", self.active)
                    }
                );
                true
            }
            Err(e) => {
                diag!(
                    "TWR_NODE_ADD {} addr={:08x} refused ({}) — tried again on the next pass",
                    s.dev_eui,
                    s.addr,
                    e.message()
                );
                false
            }
        }
    }

    /// Take a peer off the gateway (`TWR_NODE_REMOVE`). One the gateway does not hold is gone
    /// already; any other refusal keeps it in [`Self::gw_table`], so the tick tries again.
    async fn remove_peer<M: Modem>(&mut self, modem: &mut M, addr: u32) {
        match modem.twr(&TwrCmd::NodeRemove { addr }).await {
            Ok(_) => diag!("TWR_NODE_REMOVE addr={addr:08x}"),
            Err(e) if e.kind() == Kind::NotFound => {}
            Err(e) => {
                diag!(
                    "TWR_NODE_REMOVE addr={addr:08x} refused ({}) — tried again on the next pass",
                    e.message()
                );
                return;
            }
        }
        if let Some(p) = self.gw_table.remove(&addr) {
            self.pushed.remove(&p.dev_eui);
        }
    }

    /// Issue the gateway its next TX counter block. The block's end is saved **before** the
    /// block is sent (contract §3.4): a crash in between wastes a block, it never repeats one.
    ///
    /// The block starts above the gateway's own counter as its last `GET_INFO` gave it
    /// (0.3.0: a block ending below `ctr_next` is `BAD_PARAM`). A `BAD_PARAM` all the same
    /// means that reading was stale: the gateway is asked again and one more block, above
    /// what it now reports, is issued.
    async fn issue_counter_block<M: Modem>(&mut self, modem: &mut M) -> Result<()> {
        let floor = self.nb.snapshot().info.map_or(0, |i| gateway_ctr_floor(&i));
        match self.send_counter_block(modem, floor).await {
            Err(e) if e.kind() == Kind::InvalidArgument => {
                let info = modem.get_info().await?;
                self.nb.on_info(info, now_unix_i());
                diag!(
                    "TWR_CTR_BLOCK refused ({}) — the gateway is at ctr_next={} ctr_last={}, \
                     issuing above it",
                    e.message(),
                    info.ctr_next,
                    info.ctr_last
                );
                self.send_counter_block(modem, gateway_ctr_floor(&info))
                    .await
            }
            other => other,
        }
    }

    async fn send_counter_block<M: Modem>(&mut self, modem: &mut M, floor: u32) -> Result<()> {
        let (first, last) = {
            let _db_lock = self.store.lock(Slot::P2pSessions)?;
            let mut db = SessionsDb::load(&self.store)?;
            let block = db.reserve_counter_block_of(floor, self.ctr_block)?;
            db.save(&self.store)?;
            block
        };
        modem.twr(&TwrCmd::CtrBlock { first, last }).await?;
        diag!("TWR_CTR_BLOCK {first}..={last}");
        Ok(())
    }

    /// [`Self::issue_counter_block`] where a failure only needs saying: the gateway asks again
    /// (`EVT_TWR_CTR_LOW`, `NO_COUNTER`) while it is short.
    async fn issue_counter_block_or_log<M: Modem>(&mut self, modem: &mut M, why: &str) {
        if let Err(e) = self.issue_counter_block(modem).await {
            diag!("TWR_CTR_BLOCK after {why} failed: {}", e.message());
            self.nb.on_modem_error(
                now_unix_i(),
                format!("TWR_CTR_BLOCK failed ({})", e.message()),
                true,
            );
        }
    }

    /// Take off the gateway every peer it holds that is not a joined session on the P2P view
    /// (`TWR_NODE_LIST`, contract §3.4). A gateway that did not reboot across a central
    /// restart still holds the peers of Nodes removed or moved in between; the joined ones are
    /// replaced by the `TWR_NODE_ADD`s that follow. A gateway that cannot list is not swept.
    async fn sweep_gateway_peers<M: Modem>(
        &mut self,
        modem: &mut M,
        joined: &BTreeMap<u32, Session>,
    ) {
        let mut strays = Vec::new();
        let mut start = 0u16;
        for _ in 0..NODE_LIST_MAX_PAGES {
            let page = match modem.twr(&TwrCmd::NodeList { start }).await {
                Ok(b) => match NodeListPage::parse(&b) {
                    Some(page) => page,
                    None => {
                        diag!(
                            "TWR_NODE_LIST answered {} B, not a page — gateway not swept",
                            b.len()
                        );
                        return;
                    }
                },
                Err(e) => {
                    diag!(
                        "TWR_NODE_LIST refused ({}) — gateway not swept",
                        e.message()
                    );
                    return;
                }
            };
            strays.extend(
                page.entries
                    .iter()
                    .map(|e| e.addr)
                    .filter(|addr| !joined.contains_key(addr)),
            );
            match page.next {
                Some(next) if next > start => start = next,
                _ => break,
            }
        }
        for addr in strays {
            diag!("the gateway holds addr={addr:08x}, which no joined Node on P2P has");
            self.remove_peer(modem, addr).await;
        }
    }

    /// Put `Announced` heads back to `Idle`, so they are pushed again: every one (`None`), or
    /// those of `only`. What a peer the gateway just took again needs, since the gateway holds
    /// no queue for it any more.
    fn requeue_announced(&self, only: Option<&[String]>) {
        let run = || -> Result<()> {
            let _db_lock = self.store.lock(Slot::P2pSessions)?;
            let mut db = SessionsDb::load(&self.store)?;
            let mut changed = false;
            for s in db.sessions.values_mut() {
                if s.downlink_phase == DownlinkPhase::Announced
                    && only.is_none_or(|o| o.contains(&s.dev_eui))
                {
                    s.downlink_phase = DownlinkPhase::Idle;
                    changed = true;
                }
            }
            if changed {
                db.save(&self.store)?;
            }
            Ok(())
        };
        if let Err(e) = run() {
            diag!("could not re-queue the pushed downlinks: {}", e.message());
        }
    }

    /// Hand the gateway the queue heads it does not hold yet (`TWR_QUEUE_PUSH`, contract §3.4).
    ///
    /// Decided under the session lock, pushed without it (a `node-send` must not wait on the
    /// link), committed under it again: a head that changed during the round trip is dropped
    /// from the gateway rather than marked. Before anything is pushed, a pushed item whose head
    /// is no longer the one in the queue — discarded, retired by a replay timeout, withdrawn —
    /// is dropped from the gateway, so it cannot reach the Node.
    async fn push_idle_heads<M: Modem>(&mut self, modem: &mut M) {
        let (stale, due) = match self.heads_to_push() {
            Ok(v) => v,
            Err(e) => {
                diag!("could not read the downlink queues: {}", e.message());
                return;
            }
        };
        for dev_eui in stale {
            if let Some(p) = self.pushed.remove(&dev_eui) {
                self.drop_item(modem, p.addr, p.item, &dev_eui).await;
            }
        }
        for head in due {
            let DueHead {
                dev_eui,
                addr,
                item,
                body,
                control: is_control,
                more,
                repush,
            } = head;
            if !repush {
                if let Some(p) = self.pushed.remove(&dev_eui) {
                    if p.item != item || p.addr != addr {
                        self.drop_item(modem, p.addr, p.item, &dev_eui).await;
                    }
                }
            }
            let cmd = TwrCmd::QueuePush {
                addr,
                item,
                ttl_s: COMMAND_TTL_S,
                flags: link::PUSH_CONFIRMED,
                plaintext: head_plaintext(&body, is_control, more),
            };
            match modem.twr(&cmd).await {
                Ok(_) => {}
                // 0.3.0: the same item is on air or awaiting the Node's ACK. It is this head
                // (the item is the head's), so it is pushed; its `EVT_TWR_TX` follows.
                Err(e) if crate::p2p::modem::is_busy(&e) => {
                    diag!("TWR_QUEUE_PUSH item={item} for {dev_eui}: already on air");
                }
                Err(e) => {
                    if e.kind() == Kind::NotFound {
                        // The gateway lost the peer: the tick hands it over again.
                        self.gw_table.remove(&addr);
                    }
                    diag!(
                        "TWR_QUEUE_PUSH for {dev_eui} refused ({}) — pushed again on a later pass",
                        e.message()
                    );
                    continue;
                }
            }
            match self.mark_announced(&dev_eui, &body, repush) {
                Ok(true) => {
                    diag!(
                        "{} for {dev_eui} {}: item={item} ({} B body{})",
                        if is_control {
                            "control downlink"
                        } else {
                            "downlink"
                        },
                        if repush { "pushed again" } else { "pushed" },
                        body.len(),
                        if more > 0 {
                            format!(", PendingInfo more={}", more.min(255))
                        } else {
                            String::new()
                        }
                    );
                    self.pushed.insert(
                        dev_eui.clone(),
                        Pushed {
                            addr,
                            item,
                            body,
                            more,
                        },
                    );
                    if !is_control && more > 0 {
                        self.push_pending_info(modem, &dev_eui, addr, item, more)
                            .await;
                    }
                }
                Ok(false) => {
                    diag!("the downlink queue of {dev_eui} changed during the push — dropping item {item}");
                    self.pushed.remove(&dev_eui);
                    self.drop_item(modem, addr, item, &dev_eui).await;
                }
                Err(e) => {
                    diag!(
                        "could not mark the pushed downlink of {dev_eui} ({}) — dropping item {item}",
                        e.message()
                    );
                    self.pushed.remove(&dev_eui);
                    self.drop_item(modem, addr, item, &dev_eui).await;
                }
            }
        }
    }

    /// H-4, central variant (p2p_control_commands.md §4): the unconfirmed `0x91 22 01 <more>`
    /// item behind a command head, so the Node fetches the next downlinks with drain frames.
    async fn push_pending_info<M: Modem>(
        &mut self,
        modem: &mut M,
        dev_eui: &str,
        addr: u32,
        head_item: u16,
        more: usize,
    ) {
        let item = pending_info_item(dev_eui, head_item);
        let cmd = TwrCmd::QueuePush {
            addr,
            item,
            ttl_s: COMMAND_TTL_S,
            flags: 0,
            plaintext: head_plaintext(&ControlDownlink::pending_info(more).tlv(), true, 0),
        };
        match modem.twr(&cmd).await {
            Ok(_) => {
                diag!(
                    "PendingInfo more={} for {dev_eui} pushed behind item={head_item}: item={item}",
                    more.min(255)
                );
                self.pending_info_items
                    .insert(dev_eui.to_string(), (addr, item));
            }
            Err(e) if crate::p2p::modem::is_busy(&e) => {
                diag!("PendingInfo item={item} for {dev_eui}: already on air");
            }
            Err(e) => diag!(
                "PendingInfo for {dev_eui} refused ({}) — the Node fetches the next downlink \
                 at its next confirmed uplink",
                e.message()
            ),
        }
    }

    /// Under the lock: the pushed items that are stale, and the heads due a push: every `Idle`
    /// one, and an `Announced` one whose queue grew past the `PendingInfo` it was pushed with
    /// (p2p_control_commands.md §4). A head past [`MAX_COMMAND_BODY`] cannot go out in one
    /// frame and is dropped from the queue (`node-send` refuses them; this catches a queue
    /// written before that check).
    fn heads_to_push(&self) -> Result<(Vec<String>, Vec<DueHead>)> {
        let _db_lock = self.store.lock(Slot::P2pSessions)?;
        let mut db = SessionsDb::load(&self.store)?;
        let live = Self::live_sessions(&db);
        let stale: Vec<String> = self
            .pushed
            .iter()
            .filter(|(dev_eui, p)| {
                db.sessions.get(*dev_eui).is_none_or(|s| {
                    s.downlink_phase != DownlinkPhase::Announced
                        || s.downlink_queue.front().is_none_or(|h| h.body != p.body)
                })
            })
            .map(|(dev_eui, _)| dev_eui.clone())
            .collect();
        let mut due = Vec::new();
        let mut changed = false;
        for (addr, s) in &live {
            // Only the home sends a Node's downlinks (`p2p_cluster_gateway.md` §1).
            if !self.gw_table.get(addr).is_some_and(|p| p.home) {
                continue;
            }
            // H-7: PendingInfo only to a Node that understands it.
            let pending_ok =
                control::gate(s.capabilities.as_ref(), control::cmd::PENDING_INFO).is_ok();
            let more = if pending_ok {
                s.downlink_queue.len().saturating_sub(1)
            } else {
                0
            };
            if s.downlink_phase == DownlinkPhase::Announced {
                let grown = self.pushed.get(&s.dev_eui).is_some_and(|p| {
                    p.addr == *addr
                        && more > p.more
                        && s.downlink_queue.front().is_some_and(|h| h.body == p.body)
                });
                if let (true, Some(head)) = (grown, s.downlink_queue.front()) {
                    due.push(DueHead {
                        dev_eui: s.dev_eui.clone(),
                        addr: *addr,
                        item: command_item(head),
                        body: head.body.clone(),
                        control: head.control,
                        more,
                        repush: true,
                    });
                }
                continue;
            }
            if s.downlink_phase != DownlinkPhase::Idle {
                continue;
            }
            let Some(sess) = db.sessions.get_mut(&s.dev_eui) else {
                continue;
            };
            while let Some(head) = sess.downlink_queue.front() {
                // The whole plaintext against the frame (#486 U-1: 74 B): a command body of
                // up to MAX_COMMAND_BODY behind `0x81 86`, a control head behind `0x91`.
                if head_plaintext(&head.body, head.control, 0).len() <= frame::MAX_PAYLOAD {
                    break;
                }
                diag!(
                    "downlink seq={:?} for {} is {} B, past the {MAX_COMMAND_BODY} B a single \
                     P2P downlink carries — dropped",
                    head.seq,
                    s.dev_eui,
                    head.body.len()
                );
                sess.downlink_queue.pop_front();
                changed = true;
            }
            if let Some(head) = sess.downlink_queue.front() {
                let more = if pending_ok {
                    sess.downlink_queue.len() - 1
                } else {
                    0
                };
                due.push(DueHead {
                    dev_eui: s.dev_eui.clone(),
                    addr: *addr,
                    item: command_item(head),
                    body: head.body.clone(),
                    control: head.control,
                    more,
                    repush: false,
                });
            }
        }
        if changed {
            db.save(&self.store)?;
        }
        Ok((stale, due))
    }

    /// Commit a push: the head is still `body`, still `Idle` (or, for a re-push, still
    /// `Announced`), on a session still served.
    fn mark_announced(&self, dev_eui: &str, body: &[u8], repush: bool) -> Result<bool> {
        let _db_lock = self.store.lock(Slot::P2pSessions)?;
        let mut db = SessionsDb::load(&self.store)?;
        let Some(s) = db.sessions.get(dev_eui) else {
            return Ok(false);
        };
        let served = db.node(dev_eui).is_some() || s.detach.is_some();
        let phase = if repush {
            DownlinkPhase::Announced
        } else {
            DownlinkPhase::Idle
        };
        let same = served
            && s.downlink_phase == phase
            && s.downlink_queue.front().is_some_and(|h| h.body == body);
        if !same {
            return Ok(false);
        }
        if repush {
            return Ok(true);
        }
        let Some(s) = db.sessions.get_mut(dev_eui) else {
            return Ok(false);
        };
        s.downlink_phase = DownlinkPhase::Announced;
        if let Some(head) = s.downlink_queue.front_mut() {
            head.uplinks_since_sent = 0;
            // A pushed command is answered from page 0 again.
            head.pages = None;
        }
        db.save(&self.store)?;
        Ok(true)
    }

    /// `TWR_QUEUE_DROP` of one item; the gateway having sent or expired it already is fine.
    async fn drop_item<M: Modem>(&mut self, modem: &mut M, addr: u32, item: u16, dev_eui: &str) {
        match modem.twr(&TwrCmd::QueueDrop { addr, item }).await {
            Ok(_) => diag!("TWR_QUEUE_DROP item={item} for {dev_eui}"),
            Err(e) if e.kind() == Kind::NotFound => {}
            Err(e) => diag!(
                "TWR_QUEUE_DROP item={item} for {dev_eui} refused ({})",
                e.message()
            ),
        }
    }

    /// Push the `0x91` answers to one uplink's control commands, as one payload, unconfirmed
    /// (contract §3.3): the Node asks again if it misses one.
    ///
    /// At most one answer waits on the gateway per Node: an older one not sent yet is dropped
    /// (`TWR_QUEUE_DROP`) for the newer, so a replayed batch of requests yields one answer, the
    /// newest, not one per following uplink (`p2p_tower_gateway.md` §8.12, T3 on c66).
    async fn push_answers<M: Modem>(
        &mut self,
        modem: &mut M,
        addr: u32,
        dev_eui: &str,
        counter: u32,
        answers: &[(u8, Vec<u8>)],
    ) {
        if answers.is_empty() {
            return;
        }
        let older: Vec<u16> = self
            .answers
            .iter()
            .filter(|(_, a, _)| *a == addr)
            .map(|(item, ..)| *item)
            .collect();
        if !older.is_empty() {
            self.answers.retain(|(_, a, _)| *a != addr);
            for item in older {
                diag!("0x91 answer item={item} for {dev_eui} superseded — dropping it");
                self.drop_item(modem, addr, item, dev_eui).await;
            }
        }
        let items: Vec<(u8, &[u8])> = answers.iter().map(|(c, v)| (*c, v.as_slice())).collect();
        let plaintext = match control::encode_control(&items) {
            Ok(p) => p,
            Err(e) => {
                diag!("the answers to {dev_eui} do not encode: {}", e.message());
                return;
            }
        };
        let what = answers
            .iter()
            .map(|(c, _)| format!("{c:#04x}"))
            .collect::<Vec<_>>()
            .join(",");
        let item = self.next_item();
        let cmd = TwrCmd::QueuePush {
            addr,
            item,
            ttl_s: ANSWER_TTL_S,
            flags: 0,
            plaintext,
        };
        match modem.twr(&cmd).await {
            Ok(_) => {
                diag!("0x91 answer [{what}] for {dev_eui} counter={counter} pushed: item={item}");
                self.answers.push_back((item, addr, what));
                while self.answers.len() > ANSWERS_TRACKED {
                    self.answers.pop_front();
                }
            }
            Err(e) => {
                if e.kind() == Kind::NotFound {
                    self.gw_table.remove(&addr);
                }
                diag!(
                    "0x91 answer [{what}] for {dev_eui} counter={counter} refused ({}) — not sent",
                    e.message()
                );
            }
        }
    }

    /// Keep the gateway equal to the joined sessions on the P2P view: stale peers off, missing
    /// ones on (their `Announced` heads pushed again), then the heads it does not hold. The
    /// tick runs it, so a `node-remove`, `node-move`, `clear` or `node-send` from another
    /// process reaches the gateway within one tick.
    async fn reconcile_gateway<M: Modem>(&mut self, modem: &mut M) {
        self.reconcile_peers(modem, false).await;
        self.reconcile_peers(modem, true).await;
    }

    /// One phase of keeping the active gateway equal to the joined sessions on the P2P view
    /// (`p2p_cluster_gateway.md` §4, §5.2).
    ///
    /// - `promote` false: stale peers off, and every Node this gateway is **not** home of
    ///   brought to its current key without `HOME`. Run on every gateway before any promotes,
    ///   so a home move takes `HOME` off the old home before the new one gets it.
    /// - `promote` true: the Nodes this gateway is home of, with `HOME` — after a fresh counter
    ///   block when one moved here (§5.3) — their `Announced` heads put back, then the answers
    ///   and heads it does not hold.
    async fn reconcile_peers<M: Modem>(&mut self, modem: &mut M, promote: bool) {
        let live = match self.joined_on_view() {
            Ok(live) => live,
            Err(e) => {
                diag!(
                    "could not read sessions.db for the gateway pass: {}",
                    e.message()
                );
                return;
            }
        };
        if !promote {
            let stale: Vec<u32> = self
                .gw_table
                .iter()
                .filter(|(addr, p)| live.get(*addr).is_none_or(|s| s.dev_eui != p.dev_eui))
                .map(|(addr, _)| *addr)
                .collect();
            for addr in stale {
                self.remove_peer(modem, addr).await;
            }
            let others: Vec<Session> = live
                .values()
                .filter(|s| gateways::home_of(s) != self.active)
                .filter(|s| {
                    !self
                        .gw_table
                        .get(&s.addr)
                        .is_some_and(|p| p.matches(s, false))
                })
                .cloned()
                .collect();
            for s in &others {
                self.add_peer(modem, s, true).await;
            }
            return;
        }
        let homed: Vec<Session> = live
            .values()
            .filter(|s| gateways::home_of(s) == self.active)
            .filter(|s| {
                !self
                    .gw_table
                    .get(&s.addr)
                    .is_some_and(|p| p.matches(s, true))
            })
            .cloned()
            .collect();
        // Everything this gateway sends a Node it was not home of must be above whatever
        // another gateway sent it: the Node keeps one replay lane for the network (§5.3). A
        // gateway that only ever served alone needs no block for a peer it lost.
        let moved_here = homed.iter().any(|s| {
            self.gw_table.get(&s.addr).is_some_and(|p| !p.home)
                || (self.multi_gateway() && !self.gw_table.contains_key(&s.addr))
        });
        if moved_here {
            self.issue_counter_block_or_log(modem, "a home moved here")
                .await;
        }
        let mut added = Vec::new();
        for s in &homed {
            if self.add_peer(modem, s, true).await {
                added.push(s.dev_eui.clone());
            }
        }
        if !added.is_empty() {
            self.requeue_announced(Some(&added));
        }
        self.push_due_answers(modem).await;
        self.push_idle_heads(modem).await;
    }

    /// Push the `0x91` answers another gateway's delivery left for the Nodes this one is home
    /// of.
    async fn push_due_answers<M: Modem>(&mut self, modem: &mut M) {
        let due: Vec<u32> = self
            .answers_due
            .keys()
            .filter(|addr| self.gw_table.get(*addr).is_some_and(|p| p.home))
            .copied()
            .collect();
        for addr in due {
            if let Some((dev_eui, counter, answers)) = self.answers_due.remove(&addr) {
                self.push_answers(modem, addr, &dev_eui, counter, &answers)
                    .await;
            }
        }
    }

    /// Bring every gateway in `modems` to the desired state (`p2p_cluster_gateway.md` §4): the
    /// demoting phase on all of them, then the promoting one. The active gateway is whichever
    /// ran last; callers activate the one they need.
    pub async fn converge<M: Modem>(&mut self, modems: &mut BTreeMap<String, M>) {
        self.converge_due = false;
        for promote in [false, true] {
            for (gw, modem) in modems.iter_mut() {
                if self.activate(gw) {
                    self.reconcile_peers(modem, promote).await;
                }
            }
        }
    }

    /// Whether a join, a home move or a stashed answer waits for [`Self::converge`].
    pub fn converge_due(&self) -> bool {
        self.converge_due
    }

    /// Review the homes of the Nodes whose previous counter closed (§5.1), and move those that
    /// earned another gateway.
    pub fn review_homes(&mut self) {
        if self.home_review.is_empty() {
            return;
        }
        // One gateway has nowhere to move a home to.
        if !self.multi_gateway() {
            self.home_review.clear();
            return;
        }
        let addrs = std::mem::take(&mut self.home_review);
        let live = match self.joined_on_view() {
            Ok(live) => live,
            Err(e) => {
                diag!(
                    "could not read sessions.db for the home review: {}",
                    e.message()
                );
                return;
            }
        };
        let online = self.gateway_names();
        let mut moves = Vec::new();
        for addr in addrs {
            let Some(s) = live.get(&addr) else {
                continue;
            };
            let home = gateways::home_of(s).to_string();
            if let Some((to, why)) = self.signals.review(addr, &home, &online) {
                moves.push((s.dev_eui.clone(), home, to, why));
            }
        }
        self.move_homes(moves);
    }

    /// The Nodes whose home `gone` went offline move to the best online gateway that heard them
    /// within the hour; the others keep it (§5.1).
    pub fn rehome_from(&mut self, gone: &str) {
        let live = match self.joined_on_view() {
            Ok(live) => live,
            Err(e) => {
                diag!(
                    "could not read sessions.db to move homes off {gone}: {}",
                    e.message()
                );
                return;
            }
        };
        let online = self.gateway_names();
        let now = Instant::now();
        let mut moves = Vec::new();
        for s in live.values().filter(|s| gateways::home_of(s) == gone) {
            match self.signals.take_over(s.addr, gone, &online, now) {
                Some(to) => moves.push((
                    s.dev_eui.clone(),
                    gone.to_string(),
                    to,
                    gateways::MoveReason::HomeOffline,
                )),
                None => diag!(
                    "{}: home {gone} offline and no other gateway heard it lately — keeping it",
                    s.dev_eui
                ),
            }
        }
        self.move_homes(moves);
    }

    /// Record new homes in `sessions.db`; the convergence pass applies them (§5.2).
    fn move_homes(&mut self, moves: Vec<(String, String, String, gateways::MoveReason)>) {
        if moves.is_empty() {
            return;
        }
        let run = || -> Result<Vec<(String, String, String, gateways::MoveReason)>> {
            let _db_lock = self.store.lock(Slot::P2pSessions)?;
            let mut db = SessionsDb::load(&self.store)?;
            let mut done = Vec::new();
            for (dev_eui, from, to, why) in moves {
                let Some(s) = db.sessions.get_mut(&dev_eui) else {
                    continue;
                };
                // Moved meanwhile (a join): the newer decision stands.
                if gateways::home_of(s) != from {
                    continue;
                }
                s.home = gateways::home_tag(&to);
                done.push((dev_eui, from, to, why));
            }
            if !done.is_empty() {
                db.save(&self.store)?;
            }
            Ok(done)
        };
        match run() {
            Ok(done) => {
                for (dev_eui, from, to, why) in &done {
                    diag!("home of {dev_eui}: {from} -> {to} ({})", why.as_str());
                }
                if !done.is_empty() {
                    self.converge_due = true;
                }
            }
            Err(e) => diag!("could not record the new homes: {}", e.message()),
        }
    }

    /// The address of a JoinReq (`dest` 0) in an `EVT_RX`, for the join window (§7).
    pub fn join_addr(rx: &RxEvent) -> Option<u32> {
        Header::peek(&rx.frame)
            .ok()
            .filter(|h| h.dest == 0)
            .map(|h| h.src)
    }

    /// The `central_nonce` of a JoinAccept: 32 bits of OS randomness.
    fn draw_central_nonce(&self) -> Result<u32> {
        #[cfg(test)]
        if let Some(n) = self.central_nonce {
            return Ok(n);
        }
        let mut b = [0u8; 4];
        getrandom::fill(&mut b).map_err(|e| {
            Error::new(
                Kind::StorageError,
                Transport::P2p,
                Component::SessionDb,
                format!("randomness: {e}"),
            )
        })?;
        Ok(u32::from_be_bytes(b))
    }

    /// An `EVT_RX`: a frame the gateway did not take itself. A JoinReq (`dest = 0`) is the
    /// central's; a data frame from a peer the gateway should hold means it lost it.
    async fn on_rx<M: Modem>(&mut self, modem: &mut M, rx: RxEvent) -> Result<()> {
        let header = match Header::peek(&rx.frame) {
            Ok(h) => h,
            Err(e) => {
                diag!(
                    "EVT_RX len={}: not a P2P frame ({}) — ignoring",
                    rx.frame.len(),
                    e.message()
                );
                return Ok(());
            }
        };
        if header.dest == 0 {
            return self.on_join(modem, &rx, header).await;
        }
        if header.dest != self.net_id {
            diag!(
                "EVT_RX frame for net_id {:08x}, not ours ({:08x}) — ignoring",
                header.dest,
                self.net_id
            );
            return Ok(());
        }
        if self.gw_table.remove(&header.src).is_some() {
            diag!(
                "EVT_RX data frame from addr={:08x} counter={}, a peer the gateway should hold — \
                 handing it over again",
                header.src,
                header.counter
            );
        } else {
            diag!(
                "EVT_RX data frame from addr={:08x} counter={}: no session the gateway holds — \
                 ignoring",
                header.src,
                header.counter
            );
        }
        Ok(())
    }

    /// A JoinReq (contract §3.2): authenticate it under the Node's join key, answer with a
    /// JoinAccept in its RX window, and only once the modem took that frame, commit the session
    /// and hand it to the gateway.
    async fn on_join<M: Modem>(
        &mut self,
        modem: &mut M,
        rx: &RxEvent,
        header: Header,
    ) -> Result<()> {
        let addr = header.src;
        let dev_nonce = header.counter;

        // Who holds the address, under the lock; the reasons are said after it is released.
        enum Gate {
            Node(Box<crate::p2p::session::RegisteredNode>, Option<u32>),
            Parked(String),
            Nobody,
            Shared(Vec<String>),
        }
        let gate = {
            let _db_lock = self.store.lock(Slot::P2pSessions)?;
            let db = SessionsDb::load(&self.store)?;
            match db.nodes_on_addr(addr).as_slice() {
                [] => match db.session_by_addr(addr) {
                    Some(s) => Gate::Parked(s.dev_eui.clone()),
                    None => Gate::Nobody,
                },
                [one] => Gate::Node(
                    Box::new((*one).clone()),
                    db.sessions.get(&one.dev_eui).map(|s| s.dev_nonce),
                ),
                many => Gate::Shared(many.iter().map(|n| n.dev_eui.clone()).collect()),
            }
        };
        let (node, prior_nonce) = match gate {
            Gate::Node(node, prior) => (*node, prior),
            Gate::Parked(dev_eui) => {
                diag!(
                    "JoinReq from {dev_eui} refused — parked: {}",
                    parked_reason(&self.store, &dev_eui)
                );
                return Ok(());
            }
            Gate::Nobody => {
                diag!(
                    "JoinReq from addr={addr:08x}: no Node on P2P has that address — ignoring \
                     (register it with node-add --radio p2p)"
                );
                return Ok(());
            }
            Gate::Shared(dev_euis) => {
                diag!(
                    "JoinReq from addr={addr:08x} refused: {} all have that P2P address — keep \
                     one of them on LoRaWAN",
                    dev_euis.join(", ")
                );
                return Ok(());
            }
        };
        let dev_eui = node.dev_eui.clone();
        let app_key = hex16(&node.app_key)?;
        let Some(eui) = dev_eui_bytes(&dev_eui) else {
            diag!("registry DevEUI {dev_eui:?} is not 16 hex digits — ignoring its JoinReq");
            return Ok(());
        };
        let join_key = frame::join_key(&app_key, &eui);

        let plaintext = match frame::open(&join_key, &rx.frame) {
            Ok((_, p)) => p,
            Err(_) => {
                diag!(
                    "JoinReq for {dev_eui} failed authentication — dropping (forged, corrupt, \
                     or wrong app_key)"
                );
                return Ok(());
            }
        };
        let join_req = match Envelope::parse(&plaintext) {
            Ok(Envelope::Control(tlvs)) => tlvs
                .iter()
                .find(|t| t.cmd == control::cmd::JOIN_REQ)
                .and_then(|t| JoinReq::decode(t.value)),
            _ => None,
        };
        let Some(join_req) = join_req else {
            diag!("JoinReq frame from {dev_eui} authenticated but carries no JoinReq — dropping");
            return Ok(());
        };
        if join_req.dev_eui != eui {
            diag!(
                "JoinReq under the key of {dev_eui} names DevEUI {} — dropping",
                Eui::from_bytes(join_req.dev_eui).as_str()
            );
            return Ok(());
        }
        // The Node's `APP_PROTO_VERSION`, the app/protobuf payload version (SC 2026-09-28), not
        // the TOWER wire version (the header's, which the Northbridge checks). Any value joins:
        // no version is known to be undecodable yet, so a mismatch is only said.
        if join_req.proto_version != crate::decode::APP_PROTO_VERSION {
            diag!(
                "JoinReq from {dev_eui} reports app protocol {}, this central decodes {} — \
                 joining anyway",
                join_req.proto_version,
                crate::decode::APP_PROTO_VERSION
            );
        }
        if let Some(prior) = prior_nonce.filter(|p| dev_nonce <= *p) {
            diag!(
                "JoinReq dev_nonce {dev_nonce} from {dev_eui} is not above {prior} — a replay, \
                 dropping"
            );
            return Ok(());
        }

        let central_nonce = self.draw_central_nonce()?;
        let session_key = frame::session_key(&app_key, dev_nonce, central_nonce, &eui);
        let accept = JoinAccept {
            net_id: self.net_id,
            central_nonce,
            rx_delay: u8::try_from(self.rx1_delay_s).unwrap_or(u8::MAX),
            tx_power: control::assigned_node_tx_power(self.node_tx_power_dbm),
        };
        let payload = control::encode_control(&[(control::cmd::JOIN_ACCEPT, &accept.encode())])?;
        let accept_frame = frame::seal(
            &join_key,
            &Header {
                frame_type: frame::frame_type::DATA,
                flags: 0,
                src: self.net_id,
                dest: addr,
                counter: dev_nonce,
            },
            &payload,
        )?;
        diag!(
            "JoinAccept -> {dev_eui} addr={addr:08x} net_id={:08x} central_nonce={central_nonce:08x} \
             rx_delay={}s tx_power={}",
            self.net_id,
            accept.rx_delay,
            accept.tx_power
        );
        let sent = self
            .schedule(
                modem,
                rx.t_ms,
                accept_frame,
                &format!("JoinAccept addr={addr:08x}"),
            )
            .await?;
        // A JoinAccept that never goes out leaves the Node on the session it had: committing
        // would give the gateway a key the Node does not hold.
        if sent != TxOutcome::Accepted {
            diag!("JoinAccept for {dev_eui} was not sent — the session stays as it was");
            return Ok(());
        }

        let committed = {
            let _db_lock = self.store.lock(Slot::P2pSessions)?;
            let mut db = SessionsDb::load(&self.store)?;
            // A `node-remove` or a re-registration during the round trip is the newer intent.
            let still = db.node(&dev_eui).is_some_and(|n| n.app_key == node.app_key)
                && db.nodes_on_addr(addr).len() == 1;
            if !still {
                diag!(
                    "JoinAccept for {dev_eui} was sent, but the Node was removed or registered \
                     again during the round trip — not committing the session"
                );
                return Ok(());
            }
            // A new session keeps the operator's queue — command bodies, not sealed frames —
            // but nothing in flight: the gateway's lane goes with the old key.
            let queue = db
                .sessions
                .get(&dev_eui)
                .map(|s| s.downlink_queue.clone())
                .unwrap_or_default();
            let session = Session {
                dev_eui: dev_eui.clone(),
                addr,
                session_key: to_hex(&session_key),
                dev_nonce,
                last_seen: 0,
                joined_at: Some(crate::lorawan::gateway::format_rfc3339(
                    crate::lorawan::gateway::now_unix(),
                )),
                proto_version: join_req.proto_version,
                capabilities: None,
                hello: None,
                downlink_queue: queue,
                downlink_phase: DownlinkPhase::Idle,
                history: None,
                detach: None,
                // The gateway that answered the join is the Node's home (§5.1).
                home: gateways::home_tag(&self.active),
            };
            db.sessions.insert(dev_eui.clone(), session.clone());
            db.save(&self.store)?;
            session
        };
        self.pushed.remove(&dev_eui);
        // Another gateway may have sent the Node more under its old session: this one's block
        // goes above it before the Node is its (§5.3). The others take the new key on the next
        // convergence pass, without `HOME`.
        if self.multi_gateway() {
            self.issue_counter_block_or_log(modem, "a Node joined through it")
                .await;
            self.converge_due = true;
        }
        self.signals.forget(addr);
        self.add_peer(modem, &committed, false).await;

        // The retained inventory document, built as the broker-connect replay builds it.
        let info = serde_json::Value::Object(crate::p2p::replay::joined_info(
            &node,
            &committed,
            Some(self.net_id),
        ));
        match serde_json::to_vec(&info) {
            Ok(bytes) => {
                if let Err(e) = self.publisher.publish(&dev_eui, Area::Info, &bytes).await {
                    diag!("northbound info publish for {dev_eui} failed: {e}");
                }
            }
            Err(e) => diag!("northbound info for {dev_eui} could not be serialized: {e}"),
        }
        self.push_idle_heads(modem).await;
        Ok(())
    }

    /// An `EVT_TWR_UPLINK`: a frame the gateway verified and ACKed, as plaintext (contract
    /// §3.3). Decoded, correlated with the queue head, answered, and its counter kept.
    async fn on_twr_uplink<M: Modem>(&mut self, modem: &mut M, up: TwrUplink) -> Result<()> {
        if up.ack == UplinkAck::SuppressedRingFull {
            // `ack` 5 is reserved: a full ring drops the frame before any event exists, so no
            // Northbridge sends it. One that arrives is acked and never published.
            diag!(
                "WARNING: uplink addr={:08x} counter={} carries the reserved ack 5 — acked and \
                 dropped, nothing published",
                up.addr,
                up.counter
            );
            self.nb.on_reserved_ack();
            return Ok(());
        }
        self.observe_modem_clock(up.t_ms);
        let addr = up.addr;
        let counter = up.counter;

        let db_lock = self.store.lock(Slot::P2pSessions)?;
        self.ring_step("lock");
        let mut db = SessionsDb::load(&self.store)?;
        self.ring_step("load");
        let Some(session) = Self::live_sessions(&db).remove(&addr) else {
            let who = db.session_by_addr(addr).map(|s| s.dev_eui.clone());
            drop(db);
            drop(db_lock);
            match who {
                Some(dev_eui) => diag!(
                    "uplink from {dev_eui} (addr={addr:08x} counter={counter}) not served here: {} \
                     — taking it off the gateway",
                    parked_reason(&self.store, &dev_eui)
                ),
                None => diag!(
                    "uplink from addr={addr:08x} counter={counter}: no session here — taking it \
                     off the gateway"
                ),
            }
            self.remove_peer(modem, addr).await;
            return Ok(());
        };
        let dev_eui = session.dev_eui.clone();
        // Every gateway's copy counts for where the Node is heard best, the duplicates the
        // counter check drops below included (`p2p_cluster_gateway.md` §5.1).
        let q = gateways::signal_q(up.rssi_dbm, up.snr_q_db);
        if self
            .signals
            .observe(addr, counter, &self.active, q, Instant::now())
        {
            self.home_review.insert(addr);
        }
        if counter <= session.last_seen {
            diag!(
                "uplink from {dev_eui} counter={counter} is not above the last one seen ({}) — \
                 dropping{}",
                session.last_seen,
                if self.multi_gateway() {
                    format!(
                        " (copy from {}, rssi {} dBm, snr {} dB)",
                        self.gw_label(),
                        up.rssi_dbm,
                        up.snr_db()
                    )
                } else {
                    String::new()
                }
            );
            // The home's copy of the uplink just taken: what its ACK did is told here.
            if self.multi_gateway() && counter == session.last_seen {
                drop(db);
                drop(db_lock);
                self.on_later_copy(modem, &dev_eui, &up).await;
            }
            return Ok(());
        }
        if self.multi_gateway() {
            diag!(
                "uplink from {dev_eui} counter={counter} first heard through {} (rssi {} dBm, \
                 snr {} dB)",
                self.gw_label(),
                up.rssi_dbm,
                up.snr_db()
            );
        }
        // A removed Node whose Detach is still on its way (p2p_control_commands.md §3): the
        // counter is kept, nothing is decoded or published, the Detach is pushed.
        if let Some(d) = &session.detach {
            diag!(
                "uplink from {dev_eui} counter={counter}: removed at {}, Detach pending — \
                 nothing decoded",
                d.since
            );
            if let Some(s) = db.sessions.get_mut(&dev_eui) {
                s.last_seen = counter;
            }
            db.save(&self.store)?;
            drop(db_lock);
            self.ring_stored(modem).await;
            self.push_idle_heads(modem).await;
            return Ok(());
        }
        let Some(registered) = db.node(&dev_eui).cloned() else {
            return Ok(());
        };
        let decoder: Option<String> = self
            .p2p_profiles
            .get(&registered.profile)
            .map(|p| p.decoder.clone());

        let mut queue = session.downlink_queue.clone();
        let mut phase = session.downlink_phase;
        let mut history = session.history.clone();
        let mut capabilities = session.capabilities;
        let mut hello = session.hello;
        let mut answers: Vec<(u8, Vec<u8>)> = Vec::new();
        // Only the home ACKs (§1): the outcome of its ACK is read off its own copy, and a copy
        // through another gateway answers the requests only when the home's ACK does not.
        let acks_here = !self.multi_gateway() || self.gw_table.get(&addr).is_some_and(|p| p.home);
        let answered_in_ack = if acks_here {
            self.answers_in_ack
        } else {
            self.home_answers_in_ack(addr)
        };
        // The `LinkCheckReq` / `TimeReq` the gateway answered in its ACK.
        let mut ack_reqs: Vec<u8> = Vec::new();
        let mut to_publish: Option<(Area, serde_json::Value)> = None;
        let mut history_publish: Option<serde_json::Value> = None;
        let mut cache_write: Vec<(CacheSlot, serde_json::Value)> = Vec::new();
        let mut config_publish: Option<serde_json::Value> = None;
        let mut device_info_publish: Option<serde_json::Value> = None;
        let mut device_status_publish: Option<serde_json::Value> = None;
        // Every document and record of this uplink carries the time the gateway heard it.
        let heard_at = self.received_at_unix(up.t_ms, crate::lorawan::gateway::now_unix());
        let received_at = crate::lorawan::gateway::format_rfc3339(heard_at);
        let received_at_unix = u64::try_from(heard_at).unwrap_or(0);

        let reception = crate::tasks::uplink::Reception {
            transport: Transport::P2p,
            id: &dev_eui,
            dev_eui: &dev_eui,
            serial: Some(registered.serial.as_str()),
            received_at: &received_at,
            gateways: vec![GatewayRx {
                gateway_id: self.gw_label(),
                rssi: Some(i32::from(up.rssi_dbm)),
                snr: Some(up.snr_db()),
            }],
            counter: ("counter", u64::from(counter)),
            dr: None,
            deduplication_id: None,
        };

        match Envelope::parse(&up.plaintext) {
            Err(e) => diag!(
                "uplink from {dev_eui} counter={counter}: {} — nothing decoded",
                e.message()
            ),
            Ok(Envelope::App { port, body }) if port == port::RESPONSE => {
                let cd =
                    crate::tasks::uplink::decode_command_frame(body, &reception, &mut self.dumps);
                if let Some(w) = cd.document.get("warning").and_then(|v| v.as_str()) {
                    diag!("fPort-85 response from {dev_eui}: {w}");
                }
                to_publish = Some((Area::Command, cd.document.clone()));
                config_publish = cd.config.clone();
                device_info_publish = cd.device_info.clone();
                cache_write.extend(cd.cache.iter().cloned());
                // Only a completed dump reads the cache (node_reported_state.md §2.2, Q-N3).
                if let Some(dump) = &config_publish {
                    let prior = Cache::new(&self.store, Transport::P2p)
                        .read(&dev_eui)
                        .unwrap_or_else(|e| {
                            diag!(
                                "cannot read the node-get cache of {dev_eui}: {}",
                                e.message()
                            );
                            None
                        });
                    if let Some(doc) = prior
                        .as_ref()
                        .and_then(|p| crate::tasks::uplink::config_after_dump(p, dump))
                    {
                        let value = serde_json::to_value(&doc).unwrap_or(serde_json::Value::Null);
                        for (slot, v) in cache_write.iter_mut() {
                            if *slot == CacheSlot::Config {
                                *v = value.clone();
                            }
                        }
                        config_publish = Some(value);
                    }
                }
                if matches!(phase, DownlinkPhase::Sent | DownlinkPhase::Announced) {
                    self.correlate_response(
                        &dev_eui,
                        &cd,
                        &mut queue,
                        &mut phase,
                        &mut history,
                        &mut history_publish,
                        &received_at,
                        received_at_unix,
                    );
                }
            }
            Ok(Envelope::App { port, body }) if port == port::TELEMETRY || port == port::ALARM => {
                diag!(
                    "fPort {port} uplink from {dev_eui} counter={counter} body={}",
                    to_hex(body)
                );
                match decoder.as_deref().and_then(|d| {
                    crate::tasks::uplink::decode_field_frame(d, port, body, &reception)
                }) {
                    Some((area, slot, payload)) => {
                        to_publish = Some((area, payload.clone()));
                        cache_write.push((slot, payload));
                    }
                    // §4 allows no implicit decoder, so nothing is published.
                    None => diag!(
                        "no decoder for {dev_eui} (registry profile {:?}, config declares {:?}) \
                         — publishing nothing",
                        registered.profile.as_str(),
                        self.p2p_profiles.keys().collect::<Vec<_>>(),
                    ),
                }
            }
            Ok(Envelope::App { port, body }) => diag!(
                "fPort {port} uplink from {dev_eui} ({} B) carries nothing this central decodes",
                body.len()
            ),
            Ok(Envelope::Control(tlvs)) => {
                // H-3: the envelope byte alone is a Node's drain frame after a PendingInfo
                // (#487). The gateway ACKed it; the pass below pushes the next head.
                if tlvs.is_empty() {
                    diag!(
                        "empty 0x91 uplink from {dev_eui} counter={counter} (drain frame, ack \
                         {:?}) — {} downlink(s) queued",
                        up.ack,
                        queue.len()
                    );
                }
                for tlv in tlvs {
                    match tlv.cmd {
                        control::cmd::CAPABILITIES => match Capabilities::decode(tlv.value) {
                            Some(c) => {
                                diag!("Capabilities from {dev_eui}: {c:?}");
                                capabilities = Some(c);
                                answers.push((
                                    control::cmd::CAPABILITIES,
                                    Capabilities::central().encode().to_vec(),
                                ));
                            }
                            None => diag!(
                                "Capabilities from {dev_eui} of {} B — skipped",
                                tlv.value.len()
                            ),
                        },
                        control::cmd::HELLO => match Hello::decode(tlv.value) {
                            Some(h) => {
                                diag!("Hello from {dev_eui}: {h:?}");
                                hello = Some(h);
                                // A Node that restarted holds no command awaiting its answer:
                                // a delivered head goes out again.
                                if phase == DownlinkPhase::Sent && history.is_none() {
                                    phase = DownlinkPhase::Idle;
                                }
                            }
                            None => {
                                diag!("Hello from {dev_eui} of {} B — skipped", tlv.value.len())
                            }
                        },
                        // A gateway that answers in its ACK has done so already (§10.4).
                        control::cmd::LINK_CHECK | control::cmd::TIME if answered_in_ack => {
                            ack_reqs.push(tlv.cmd)
                        }
                        control::cmd::LINK_CHECK => answers.push((
                            control::cmd::LINK_CHECK,
                            LinkCheckAns::from_reception(
                                up.rssi_dbm,
                                up.snr_q_db,
                                self.radio.sf,
                                1,
                            )
                            .encode()
                            .to_vec(),
                        )),
                        control::cmd::TIME => match self.wall_ms_at(up.t_ms) {
                            Some(ms) => answers.push((
                                control::cmd::TIME,
                                TimeAns::from_unix_ms(ms, counter).encode().to_vec(),
                            )),
                            None => diag!(
                                "TimeReq from {dev_eui} not answered: the Hub's clock is not set"
                            ),
                        },
                        control::cmd::DEV_STATUS => match DevStatusAns::decode(tlv.value) {
                            Some(a) => {
                                diag!("DevStatusAns from {dev_eui} counter={counter}: {a:?}");
                                device_status_publish = Some(
                                    DeviceStatusDocument::from_p2p(
                                        &dev_eui,
                                        Some(registered.serial.as_str()),
                                        &received_at,
                                        &a,
                                    )
                                    .to_value(),
                                );
                            }
                            None => diag!(
                                "DevStatusAns from {dev_eui} counter={counter} of {} B — skipped",
                                tlv.value.len()
                            ),
                        },
                        other => diag!(
                            "control command {other:#04x} ({} B) from {dev_eui} is not one this \
                             central handles — skipped",
                            tlv.value.len()
                        ),
                    }
                }
            }
        }

        if acks_here {
            self.ack_taken.insert(addr, counter);
            self.on_ack_answers(&dev_eui, &up, &ack_reqs);
        } else if !ack_reqs.is_empty() {
            diag!(
                "{} from {dev_eui} counter={counter}: answered in the home's ACK — its copy \
                 tells how",
                req_names(&ack_reqs)
            );
        }

        // F3: new uplinks while the head is `Sent` mean its answer, or the command itself, was
        // lost. After three it is pushed again — except while a history replay streams in,
        // where a second request would restart the Node's ring (p2p_history_replay.md §4.4).
        if history.is_none() && phase == DownlinkPhase::Sent {
            let left = queue.len().saturating_sub(1);
            if let Some(head) = queue.front_mut() {
                head.uplinks_since_sent = head.uplinks_since_sent.saturating_add(1);
                if head.uplinks_since_sent >= REANNOUNCE_AFTER_UPLINKS {
                    head.uplinks_since_sent = 0;
                    phase = DownlinkPhase::Idle;
                    if let Some(pages) = head.pages.as_ref() {
                        // #128: the page stream has stopped. Asking again would restart it
                        // from page 0; what arrived stays, and a missing page can be
                        // re-requested on its own (`node-send get-config --page N --seq S`).
                        diag!(
                            "paged answer to seq={:?} for {dev_eui} stopped after \
                             {REANNOUNCE_AFTER_UPLINKS} uplinks with page(s) {:?} of {} missing \
                             — retiring the request ({} left in queue)",
                            head.seq,
                            pages.missing(),
                            pages.count,
                            left
                        );
                        queue.pop_front();
                    } else {
                        diag!(
                            "pushing downlink seq={:?} for {dev_eui} again after \
                             {REANNOUNCE_AFTER_UPLINKS} uplinks without its answer",
                            head.seq
                        );
                    }
                }
            }
        }

        if let Some(s) = db.sessions.get_mut(&dev_eui) {
            s.last_seen = counter;
            s.downlink_queue = queue;
            s.downlink_phase = phase;
            s.history = history;
            s.capabilities = capabilities;
            s.hello = hello;
        }
        self.ring_step("decode");
        // The northbound messages are stored before the counter (§8.6): when they cannot be,
        // nothing of the event is saved and the gateway's resend brings it again. The entry is
        // keyed by DevEUI and frame counter: a crash, or a failed save, between the two stores
        // the resend as the same entry, published once while it is still pending (§8.9).
        //
        // #130: both files are written first and flushed back to back, so a journaling
        // filesystem serves the two flushes with one commit; the outbox entry is still in place
        // (renamed, its directory synced) before `sessions.db` is.
        let entry = self.outbox_ref().stage(
            &outbox::uplink_key(&dev_eui, counter),
            Self::uplink_messages(
                &dev_eui,
                to_publish,
                history_publish,
                config_publish,
                device_info_publish,
                device_status_publish,
            ),
        )?;
        // A clone of the store handle (a path) so the staged write does not borrow `self`.
        let store = self.store.clone();
        let sessions = db.stage_save(&store)?;
        self.ring_step("stage");
        if let Some(entry) = &entry {
            entry.sync()?;
        }
        sessions.sync()?;
        self.ring_step("sync");
        if let Some(entry) = entry {
            self.outbox_ref().commit(entry)?;
        }
        self.ring_step("outbox");
        sessions.commit()?;
        self.ring_step("save");
        drop(db_lock);
        self.ring_stored(modem).await;

        // Only the home sends the Node anything (§1): a copy that came first through another
        // gateway leaves its answers for the home's next convergence pass.
        if !acks_here && !answers.is_empty() {
            self.answers_due
                .insert(addr, (dev_eui.clone(), counter, answers));
            self.converge_due = true;
        } else {
            self.push_answers(modem, addr, &dev_eui, counter, &answers)
                .await;
        }
        self.push_idle_heads(modem).await;
        if up.ack == UplinkAck::NoCounter {
            self.issue_counter_block_or_log(modem, "an ACK without a counter")
                .await;
        }
        self.flush_cache(&dev_eui, &received_at, &cache_write);
        self.flush_outbox().await;
        Ok(())
    }

    /// A later copy of the uplink just taken, through another gateway. The home sends its
    /// event once it has ACKed, so its copy often comes after one through another gateway: what
    /// its ACK did is taken from it, once per counter. A gateway out of TX counters says so in
    /// any copy.
    async fn on_later_copy<M: Modem>(&mut self, modem: &mut M, dev_eui: &str, up: &TwrUplink) {
        let home_here = self.gw_table.get(&up.addr).is_some_and(|p| p.home);
        if home_here && self.ack_taken.get(&up.addr) != Some(&up.counter) {
            self.ack_taken.insert(up.addr, up.counter);
            if self.answers_in_ack {
                let reqs = match Envelope::parse(&up.plaintext) {
                    Ok(Envelope::Control(tlvs)) => tlvs
                        .iter()
                        .map(|t| t.cmd)
                        .filter(|c| matches!(*c, control::cmd::LINK_CHECK | control::cmd::TIME))
                        .collect(),
                    _ => Vec::new(),
                };
                self.on_ack_answers(dev_eui, up, &reqs);
            }
        }
        if up.ack == UplinkAck::NoCounter {
            self.issue_counter_block_or_log(modem, "an ACK without a counter")
                .await;
        }
    }

    /// What the gateway's ACK did with the uplink's `LinkCheckReq` / `TimeReq` (§10.4),
    /// counted once per uplink. A `TimeReq` ACKed without a `TimeAns` shows the gateway holds
    /// no anchor (§13.5): re-synced soon, rate-limited.
    fn on_ack_answers(&mut self, dev_eui: &str, up: &TwrUplink, reqs: &[u8]) {
        if reqs.is_empty() {
            return;
        }
        let counter = up.counter;
        let through = if self.multi_gateway() {
            format!(" {}", self.gw_label())
        } else {
            String::new()
        };
        diag!(
            "{} from {dev_eui} counter={counter}: answered in the gateway{through}'s ACK (ack \
             {:?}, tail {:?})",
            req_names(reqs),
            up.ack,
            up.ack_tail
        );
        self.nb.on_ack_ctrl_req();
        if reqs.contains(&control::cmd::TIME)
            && matches!(up.ack, UplinkAck::Sent | UplinkAck::SentPending)
            && !up.ack_tail.time
        {
            diag!(
                "TimeReq from {dev_eui} counter={counter} without a TimeAns in the ACK — the \
                 gateway has no time anchor; re-syncing"
            );
            self.nb.on_ack_time_missed();
            self.time_sync_wanted = true;
        }
    }

    /// Whether the gateway that holds `addr` as its home answers in its ACK; this one's own
    /// setting when none does yet.
    fn home_answers_in_ack(&self, addr: u32) -> bool {
        if self.gw_table.get(&addr).is_some_and(|p| p.home) {
            return self.answers_in_ack;
        }
        self.parked_gws
            .values()
            .find(|g| g.gw_table.get(&addr).is_some_and(|p| p.home))
            .map_or(self.answers_in_ack, |g| g.answers_in_ack)
    }

    /// S2 / F3: a fPort-85 response retires the queue head when it is *that* head's answer —
    /// the Node echoes `Command.seq`; a history stream advances it frame by frame.
    #[allow(clippy::too_many_arguments)]
    fn correlate_response(
        &self,
        dev_eui: &str,
        cd: &crate::tasks::uplink::CommandDecode,
        queue: &mut VecDeque<crate::p2p::session::QueuedDownlink>,
        phase: &mut DownlinkPhase,
        history: &mut Option<HistoryCollect>,
        history_publish: &mut Option<serde_json::Value>,
        received_at: &str,
        received_at_unix: u64,
    ) {
        let got = cd.response.seq;
        let Some(head_seq) = queue.front().map(|q| q.seq) else {
            // #128: a late page or an extra history frame, its request retired already.
            diag!(
                "fPort-85 {} seq={got}{} from {dev_eui} with no downlink in flight — not \
                 correlated",
                cd.response.body.kind(),
                frame_position(cd),
            );
            return;
        };
        // The `command` document carries the error northbound already; this is the operator's
        // trace of which command the Node refused and why.
        if let crate::decode::command::ResponseBody::Error(e) = &cd.response.body {
            diag!(
                "{dev_eui} answered seq={got} with error {} (in flight: seq={head_seq:?})",
                node_error_text(e)
            );
        }
        match head_seq {
            Some(want) if got != want => {
                diag!(
                    "fPort-85 {} seq={got}{} does not match the in-flight seq={want} for \
                     {dev_eui} — keeping the head",
                    cd.response.body.kind(),
                    frame_position(cd),
                );
            }
            // One HistoryFrame of the stream answering the head (§4.3).
            Some(_) if cd.history.is_some() => {
                let Some(f) = cd.history.as_ref() else {
                    return;
                };
                let coll = history
                    .get_or_insert_with(|| HistoryCollect::new(got, received_at, received_at_unix));
                let fresh =
                    coll.accept(f.frame_index, f.frame_count, received_at, received_at_unix);
                if fresh {
                    *history_publish = Some(f.document.clone());
                }
                diag!(
                    "history frame {}/{} for {dev_eui} (seq {got}, {} records{}) — {} of {} seen",
                    f.frame_index,
                    f.frame_count,
                    f.records,
                    if fresh { "" } else { ", duplicate ignored" },
                    coll.seen.len(),
                    coll.frame_count
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "?".into()),
                );
                if coll.complete() {
                    diag!(
                        "history replay complete for {dev_eui} (seq {got}, {} frames) — retiring \
                         the request",
                        coll.seen.len()
                    );
                    *history = None;
                    queue.pop_front();
                    *phase = DownlinkPhase::Idle;
                } else {
                    if let Some(h) = queue.front_mut() {
                        h.uplinks_since_sent = 0;
                    }
                    // A frame of the stream proves the request landed.
                    *phase = DownlinkPhase::Sent;
                }
            }
            // #128: one page of a paged answer to the head. The head stays in flight until the
            // last page, so the next downlink does not cut into the Node's page stream.
            Some(_) if cd.response.paging().is_some() => {
                let Some(p) = cd.response.paging() else {
                    return;
                };
                let Some(head) = queue.front_mut() else {
                    return;
                };
                let pages = head.pages.get_or_insert_with(|| PagesSeen {
                    count: p.count,
                    seen: Default::default(),
                });
                let fresh = pages.accept(p.index, p.count);
                if pages.complete() {
                    queue.pop_front();
                    *phase = DownlinkPhase::Idle;
                    diag!(
                        "fPort-85 {} page {}/{} for {dev_eui} (seq {got}) completed the answer — \
                         {} left in queue",
                        cd.response.body.kind(),
                        p.index,
                        p.count,
                        queue.len()
                    );
                } else {
                    head.uplinks_since_sent = 0;
                    // A page proves the command landed.
                    *phase = DownlinkPhase::Sent;
                    diag!(
                        "fPort-85 {} page {}/{} for {dev_eui} (seq {got}{}) — {} of {} seen, \
                         holding the queue",
                        cd.response.body.kind(),
                        p.index,
                        p.count,
                        if fresh { "" } else { ", duplicate" },
                        pages.seen.len(),
                        pages.count
                    );
                }
            }
            // `None` is an operator's raw --hex payload: the first answer retires it.
            _ => {
                let head_was_history = history.is_some()
                    || queue
                        .front()
                        .is_some_and(|h| crate::decode::history::is_req_history(&h.body));
                if head_was_history {
                    let miss = history
                        .as_ref()
                        .map(|c| c.missing_indices())
                        .unwrap_or_default();
                    if !miss.is_empty() {
                        diag!(
                            "history replay for {dev_eui} ended with {} frame(s) missing: \
                             {miss:?} — accepting what arrived",
                            miss.len()
                        );
                    }
                    if history.as_ref().is_none_or(|c| c.seen.is_empty()) {
                        // The agreed "done, no data" sentinel.
                        *history_publish = Some(serde_json::json!({
                            "dev_eui": dev_eui,
                            "frame_index": 0,
                            "frame_count": 0,
                            "records": [],
                        }));
                    }
                    *history = None;
                }
                queue.pop_front();
                *phase = DownlinkPhase::Idle;
                diag!(
                    "fPort-85 response cleared the in-flight downlink for {dev_eui} — {} left in \
                     queue",
                    queue.len()
                );
            }
        }
    }

    /// An `EVT_TWR_TX`: the fate of one pushed item (contract §3.4).
    async fn on_twr_tx<M: Modem>(&mut self, modem: &mut M, tx: TwrTx) -> Result<()> {
        if tx.outcome == link::TxOutcome::NoCounter {
            self.issue_counter_block_or_log(modem, "NO_COUNTER").await;
        }
        if let Some(i) = self
            .answers
            .iter()
            .position(|(item, addr, _)| *item == tx.item && *addr == tx.addr)
        {
            if let Some((_, _, what)) = self.answers.remove(i) {
                diag!(
                    "0x91 answer [{what}] to addr={:08x}: {:?}",
                    tx.addr,
                    tx.outcome
                );
            }
            return Ok(());
        }
        if let Some(dev_eui) = self
            .pending_info_items
            .iter()
            .find(|(_, (addr, item))| *item == tx.item && *addr == tx.addr)
            .map(|(d, _)| d.clone())
        {
            self.pending_info_items.remove(&dev_eui);
            diag!(
                "PendingInfo item={} to {dev_eui}: {:?}",
                tx.item,
                tx.outcome
            );
            self.ring_stored(modem).await;
            return Ok(());
        }
        let Some(dev_eui) = self
            .pushed
            .iter()
            .find(|(_, p)| p.item == tx.item && p.addr == tx.addr)
            .map(|(d, _)| d.clone())
        else {
            diag!(
                "EVT_TWR_TX item={} addr={:08x} {:?}: not an item this central is waiting on",
                tx.item,
                tx.addr,
                tx.outcome
            );
            return Ok(());
        };
        let Some(pushed) = self.pushed.remove(&dev_eui) else {
            return Ok(());
        };
        let mut given_up: Option<(Option<u32>, Option<String>)> = None;
        let mut detached = false;
        {
            let _db_lock = self.store.lock(Slot::P2pSessions)?;
            let mut db = SessionsDb::load(&self.store)?;
            if let Some(s) = db.sessions.get_mut(&dev_eui).filter(|s| s.addr == tx.addr) {
                // The Node's ACK frame spends an uplink counter the gateway accepted.
                s.last_seen = s.last_seen.max(tx.node_ack_counter);
                let head_matches = s.downlink_phase == DownlinkPhase::Announced
                    && s.downlink_queue
                        .front()
                        .is_some_and(|h| h.body == pushed.body);
                if head_matches {
                    match tx.outcome {
                        link::TxOutcome::Delivered | link::TxOutcome::Sent => {
                            if let Some(h) = s.downlink_queue.front_mut() {
                                h.undelivered = 0;
                            }
                            // PN-4: a command the Node answers with a measurement, not a
                            // fPort-85 response, is done once delivered.
                            // A control head has no answer either (p2p_control_commands.md §2).
                            let no_answer = s.downlink_queue.front().is_some_and(|h| {
                                h.control
                                    || crate::decode::command::command_tag_of(&h.body)
                                        .is_some_and(|t| !t.answers_on_the_command_port())
                            });
                            if no_answer {
                                let head = s.downlink_queue.pop_front();
                                s.downlink_phase = DownlinkPhase::Idle;
                                diag!(
                                    "downlink seq={:?} delivered to {dev_eui}; it has no answer \
                                     to wait for — retired ({} left in queue)",
                                    head.and_then(|h| h.seq),
                                    s.downlink_queue.len()
                                );
                            } else {
                                s.downlink_phase = DownlinkPhase::Sent;
                                if let Some(h) = s.downlink_queue.front_mut() {
                                    h.uplinks_since_sent = 0;
                                }
                                diag!(
                                    "downlink seq={:?} delivered to {dev_eui} ({:?}) — awaiting \
                                     its answer",
                                    s.downlink_queue.front().and_then(|h| h.seq),
                                    tx.outcome
                                );
                            }
                        }
                        // Never on air: no chance spent, pushed again.
                        other @ (link::TxOutcome::Expired | link::TxOutcome::NoCounter) => {
                            s.downlink_phase = DownlinkPhase::Idle;
                            diag!(
                                "downlink seq={:?} for {dev_eui} not sent ({other:?}) — pushed \
                                 again",
                                s.downlink_queue.front().and_then(|h| h.seq)
                            );
                        }
                        // On air, no ACK from the Node: one of its chances spent (#470 C3).
                        other => {
                            s.downlink_phase = DownlinkPhase::Idle;
                            let tries = s.downlink_queue.front_mut().map_or(0, |h| {
                                h.undelivered = h.undelivered.saturating_add(1);
                                h.undelivered
                            });
                            if tries >= crate::p2p::session::UNDELIVERED_LIMIT {
                                let head = s.downlink_queue.pop_front();
                                diag!(
                                    "downlink seq={:?} for {dev_eui} not delivered ({other:?}) \
                                     {tries} times — given up ({} left in queue)",
                                    head.as_ref().and_then(|h| h.seq),
                                    s.downlink_queue.len()
                                );
                                given_up = Some((
                                    head.and_then(|h| h.seq),
                                    db.nodes.get(&dev_eui).map(|n| n.serial.clone()),
                                ));
                            } else {
                                diag!(
                                    "downlink seq={:?} for {dev_eui} not delivered ({other:?}, \
                                     {tries}/{}) — pushed again",
                                    s.downlink_queue.front().and_then(|h| h.seq),
                                    crate::p2p::session::UNDELIVERED_LIMIT
                                );
                            }
                        }
                    }
                }
                // A detaching session whose Detach is delivered or given up is done (§3).
                detached = s.detach.is_some() && s.downlink_queue.is_empty();
                // Into the outbox with the decision, before it is saved (§8.6). A head without
                // a seq (an operator's `--hex`) has nothing for a waiter to close.
                if let Some((Some(seq), serial)) = &given_up {
                    let at = crate::lorawan::gateway::format_rfc3339(
                        crate::lorawan::gateway::now_unix(),
                    );
                    let doc =
                        undelivered_document(&dev_eui, serial.as_deref(), *seq, tx.outcome, &at);
                    self.outbox_ref().append(
                        &outbox::undelivered_key(&dev_eui, *seq),
                        vec![outbox::Message::new(&dev_eui, Area::Command, &doc)],
                    )?;
                }
                if detached {
                    db.sessions.remove(&dev_eui);
                    diag!(
                        "the Detach for removed {dev_eui} is done ({:?}) — session dropped",
                        tx.outcome
                    );
                }
                db.save(&self.store)?;
            }
        }
        self.ring_stored(modem).await;
        if detached {
            self.remove_peer(modem, tx.addr).await;
        }
        self.flush_outbox().await;
        self.push_idle_heads(modem).await;
        Ok(())
    }

    /// `EVT_TWR_CTR_LOW`: the gateway is running out of TX counters.
    async fn on_ctr_low<M: Modem>(&mut self, modem: &mut M, low: TwrCtrLow) {
        diag!(
            "EVT_TWR_CTR_LOW next={} last={} — issuing a block",
            low.next,
            low.last
        );
        self.issue_counter_block_or_log(modem, "EVT_TWR_CTR_LOW")
            .await;
    }

    /// Retire a history replay that has stopped producing frames (`p2p_history_replay.md`
    /// §4.3 option 1 and §5.1).
    ///
    /// The head is popped and the collector dropped, but **nothing is published**: a
    /// `frame_count: 0` sentinel would mark the portal's backfill job DONE, and the reconciler
    /// has to stay free to re-request the window. The gaps go to the diagnostic log instead,
    /// which is what an operator needs to decide whether to re-issue the request.
    ///
    /// Only a collector whose request is still the queue head is retired. Anything else means
    /// the queue has moved on and the collector is stale bookkeeping rather than a stall.
    pub fn retire_stalled_replays(&self, now_unix: u64, timeout_s: u64) -> Result<()> {
        let db_lock = self.store.lock(Slot::P2pSessions)?;
        let mut db = SessionsDb::load(&self.store)?;
        let mut retired = false;

        for (dev_eui, s) in db.sessions.iter_mut() {
            let stalled = match s.history.as_ref() {
                Some(h) => {
                    now_unix.saturating_sub(h.last_frame_at_unix) > timeout_s
                        && s.downlink_queue.front().and_then(|q| q.seq) == Some(h.seq)
                }
                None => false,
            };
            if !stalled {
                continue;
            }

            if let Some(h) = s.history.as_ref() {
                let missing = h.missing_indices();
                // The list is bounded by MAX_FRAME_COUNT, which is still 65 535 — enough to
                // make one log line unreadable, so it is truncated rather than dumped.
                let shown: Vec<u32> = missing.iter().copied().take(16).collect();
                diag!(
                    "history replay for {dev_eui} stalled for {}s — retiring seq {} with {} of {} frames missing: {:?}{}",
                    now_unix.saturating_sub(h.last_frame_at_unix),
                    h.seq,
                    missing.len(),
                    h.frame_count
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "?".into()),
                    shown,
                    if missing.len() > shown.len() { " …" } else { "" }
                );
            }

            s.history = None;
            s.downlink_queue.pop_front();
            s.downlink_phase = DownlinkPhase::Idle;
            retired = true;
        }

        if retired {
            db.save(&self.store)?;
        }
        drop(db_lock);
        Ok(())
    }

    /// Drop the detaching sessions whose Detach has waited past
    /// [`crate::p2p::session::DETACH_MAX_S`]: the Node is gone (p2p_control_commands.md §3).
    /// The next gateway pass takes them off the gateway.
    pub fn retire_expired_detaches(&self, now_unix: u64) -> Result<()> {
        let _db_lock = self.store.lock(Slot::P2pSessions)?;
        let mut db = SessionsDb::load(&self.store)?;
        let expired: Vec<String> = db
            .sessions
            .values()
            .filter(|s| {
                s.detach.as_ref().is_some_and(|d| {
                    now_unix.saturating_sub(d.since_unix) > crate::p2p::session::DETACH_MAX_S
                })
            })
            .map(|s| s.dev_eui.clone())
            .collect();
        for dev_eui in &expired {
            diag!(
                "the Detach for removed {dev_eui} was not delivered within {} s — session \
                 dropped without it",
                crate::p2p::session::DETACH_MAX_S
            );
            db.sessions.remove(dev_eui);
        }
        if !expired.is_empty() {
            db.save(&self.store)?;
        }
        Ok(())
    }

    /// Take a completed transmission into the modem's TX lead: only the frame this central
    /// aimed and only when it went out (`Ok`), since a refused or cancelled one never started.
    fn learn_tx_lead(&mut self, done: &modem::TxDone) {
        let Some((tx_id, aim, airtime_ms)) = self.tx_aim else {
            return;
        };
        if tx_id != done.tx_id {
            return;
        }
        self.tx_aim = None;
        if done.status != modem::TxStatus::Ok {
            return;
        }
        let before = self.tx_lead.estimate_ms();
        let first = self.tx_lead.samples() == 0;
        match self.tx_lead.observe(aim, done.t_actual_ms, airtime_ms) {
            Some(measured) if first || (measured - before).abs() > airtime::TX_LEAD_NOTICE_MS => {
                diag!(
                    "modem TX lead {measured:.1} ms (estimate {before:.1} -> {:.1} ms)",
                    self.tx_lead.estimate_ms()
                );
            }
            Some(_) => {}
            None => diag!(
                "EVT_TX_DONE tx_id={} does not fit its aim ({aim}, {airtime_ms:.0} ms on air) — \
                 not taken into the TX lead",
                done.tx_id
            ),
        }
        let now = self.tx_lead.estimate_ms();
        // The saved lead is this Hub's own Northbridge's; a remote one learns its own each run.
        if self.active == LOCAL && (now - self.tx_lead_saved).abs() > airtime::TX_LEAD_SAVE_DELTA_MS
        {
            match self.store.write(Slot::P2pTxLead, &format!("{now:.1}\n")) {
                Ok(()) => self.tx_lead_saved = now,
                Err(e) => diag!("cannot save the TX lead: {}", e.message()),
            }
        }
    }

    /// Schedule one radio frame for the node's RX1 window, logging a `BUSY`/`LATE` refusal
    /// without aborting the loop (the central simply learns the answer was not sent).
    ///
    /// Returns what the modem did with the frame. A refusal is not a runtime error — the loop
    /// carries on — but it is not success either, and a caller that is about to commit state
    /// the frame was supposed to deliver has to be able to tell the difference (D2).
    async fn schedule<M: Modem>(
        &mut self,
        modem: &mut M,
        rx_t_ms: u64,
        radio_frame: Vec<u8>,
        what: &str,
    ) -> Result<TxOutcome> {
        let at = modem::schedule_at(rx_t_ms, self.rx1_delay_s, self.tx_lead.estimate_ms());
        let airtime_ms = airtime::lora_airtime_ms(radio_frame.len(), &self.radio);
        let tx = TxSchedule {
            tx_id: self.next_tx_id(),
            t_ms: at,
            frame: radio_frame,
        };
        match modem.tx_schedule(&tx).await {
            Ok(()) => {
                // The modem's single TX slot is now occupied; a spreading-factor switch waits for
                // the `EVT_TX_DONE` that frees it.
                self.tx_in_flight = Some(tx.tx_id);
                self.tx_aim = Some((tx.tx_id, at, airtime_ms));
                diag!(
                    "TX_SCHEDULE ok: {what} tx_id={} aim_t_ms={at} (rx_t_ms={rx_t_ms})",
                    tx.tx_id
                );
                Ok(TxOutcome::Accepted)
            }
            Err(e) => {
                diag!(
                    "TX_SCHEDULE refused ({}): {what} — answer NOT sent",
                    e.message()
                );
                self.nb.on_tx_refused();
                Ok(TxOutcome::Refused)
            }
        }
    }
}

/// What [`Central::schedule`] did with a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxOutcome {
    /// The modem took it into its TX slot.
    Accepted,
    /// Refused: it will not go out.
    Refused,
}

/// [`run_central`] until `stop` resolves — the form the host's supervisor needs
/// (`plan/portal/p2p_integration.md` §4.1). `stop` resolving flips the runtime's own shutdown
/// watch, so the loop ends between events rather than being cancelled in the middle of one.
pub async fn run_central_until<M: Modem, P: NorthboundPublisher>(
    central: Central<P>,
    modem: M,
    stop: impl std::future::Future<Output = ()> + Send,
) -> Result<()> {
    let (tx, rx) = tokio::sync::watch::channel(false);
    let run = run_central(central, modem, rx);
    tokio::pin!(run);
    tokio::select! {
        result = &mut run => result,
        () = stop => {
            let _ = tx.send(true);
            run.await
        }
    }
}

/// The loop: initialise the modem, then feed each event to [`Central::on_event`] until the
/// `shutdown` watch flips or the link dies.
pub async fn run_central<M: Modem, P: NorthboundPublisher>(
    mut central: Central<P>,
    mut modem: M,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    central.init_modem(&mut modem).await?;
    diag!("P2P central running");
    // A quiet network produces no events at all, and an operator's `radio-set` should not have to
    // wait for one. Two seconds is well inside the patience of someone watching a bench Hub and
    // far cheaper than polling the session DB per event.
    let mut tick = tokio::time::interval(RADIO_OVERRIDE_POLL);
    loop {
        // The ring's ack pacing (link doc §4.1): nothing to wait for while nothing is owed.
        let ack_at = central.ack_deadline().map(tokio::time::Instant::from_std);
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    diag!("stop requested (shutdown or a northbridge update) — stopping");
                    central.ack_owed(&mut modem).await;
                    break;
                }
            }
            () = async {
                match ack_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                central.ack_if_due(&mut modem, Instant::now()).await;
            }
            _ = tick.tick() => {
                central.probe_if_idle(&mut modem).await?;
                central.time_sync_if_due(&mut modem, Instant::now()).await?;
                central.reconcile_gateway(&mut modem).await;
                central.apply_radio_override_if_changed(&mut modem).await;
                central.report_counters();
                // A quiet network is exactly when a stalled replay needs retiring: the node that
                // stopped sending frames is the one producing no events either.
                let now = u64::try_from(crate::lorawan::gateway::now_unix()).unwrap_or(0);
                if let Err(e) = central.retire_stalled_replays(now, central.history_stall_timeout_s) {
                    diag!("could not retire stalled history replays: {}", e.message());
                }
                if let Err(e) = central.retire_expired_detaches(now) {
                    diag!("could not retire expired detaches: {}", e.message());
                }
            }
            ev = modem.next_event() => {
                match ev {
                    Ok(ev) => central.on_event(&mut modem, ev).await?,
                    Err(e) => {
                        diag!("modem link error: {} ({:?}) — stopping", e.message(), e.kind());
                        return Err(e);
                    }
                }
            }
        }
    }
    Ok(())
}

/// A change in the remote gateways [`run_central_multi`] serves (`p2p_cluster_gateway.md` §4).
pub enum GatewayChange {
    /// A Client Hub's gateway came online: the modem that reaches it over the plane.
    Online(String, Box<dyn Modem>),
    /// It went offline: its `state` document said so, or its LWT did.
    Offline(String),
}

/// [`run_central_multi`] until `stop` completes; the modems' owed ring acks go out first.
pub async fn run_central_multi_until<P: NorthboundPublisher>(
    central: Central<P>,
    local: Box<dyn Modem>,
    changes: tokio::sync::mpsc::Receiver<GatewayChange>,
    stop: impl std::future::Future<Output = ()> + Send,
) -> Result<()> {
    let (tx, rx) = tokio::sync::watch::channel(false);
    let run = run_central_multi(central, local, changes, rx);
    tokio::pin!(run);
    tokio::select! {
        result = &mut run => result,
        () = stop => {
            let _ = tx.send(true);
            run.await
        }
    }
}

/// The next event of any gateway, with its name. Each `next_event` is cancel-safe (an event
/// is taken off the link only when it is returned), so the futures not polled to completion
/// lose nothing.
async fn next_event_of(modems: &mut BTreeMap<String, Box<dyn Modem>>) -> (String, Result<Event>) {
    use std::future::Future;
    let mut pending: Vec<_> = modems
        .iter_mut()
        .map(|(gw, m)| Box::pin(async move { (gw.clone(), m.next_event().await) }))
        .collect();
    std::future::poll_fn(|cx| {
        for f in pending.iter_mut() {
            if let std::task::Poll::Ready(out) = f.as_mut().poll(cx) {
                return std::task::Poll::Ready(out);
            }
        }
        std::task::Poll::Pending
    })
    .await
}

/// The central over its own Northbridge and the Client Hubs' (`p2p_cluster_gateway.md` §4,
/// §5): the loop of [`run_central`] once per gateway, with a Node's home deciding which one
/// ACKs it and sends its downlinks.
///
/// A remote gateway that dies or goes offline is dropped and the Nodes it was home of move to
/// the best other gateway that heard them; only the local Northbridge's death ends the run, as
/// in [`run_central`]. A JoinReq heard by more than one gateway is answered through the one
/// that heard it best within [`gateways::JOIN_WINDOW`] (§7).
pub async fn run_central_multi<P: NorthboundPublisher>(
    mut central: Central<P>,
    mut local: Box<dyn Modem>,
    mut changes: tokio::sync::mpsc::Receiver<GatewayChange>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    central.activate(LOCAL);
    central.init_modem(&mut local).await?;
    let mut modems: BTreeMap<String, Box<dyn Modem>> = BTreeMap::new();
    modems.insert(LOCAL.to_string(), local);
    central.update_board();
    diag!("P2P central running (gateways on the plane are served as they come online)");
    let mut tick = tokio::time::interval(RADIO_OVERRIDE_POLL);
    let mut join: Option<gateways::JoinCopies<Event>> = None;
    let mut changes_open = true;
    loop {
        let ack_at = central
            .ack_deadline_any()
            .map(tokio::time::Instant::from_std);
        let join_at = join
            .as_ref()
            .map(|j| tokio::time::Instant::from_std(j.deadline));
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    diag!("stop requested (shutdown or a northbridge update) — stopping");
                    for (gw, modem) in modems.iter_mut() {
                        if central.activate(gw) {
                            central.ack_owed(modem).await;
                        }
                    }
                    central.activate(LOCAL);
                    break;
                }
            }
            () = async {
                match ack_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                let now = Instant::now();
                for (gw, modem) in modems.iter_mut() {
                    if central.activate(gw) {
                        central.ack_if_due(modem, now).await;
                    }
                }
            }
            () = async {
                match join_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some((gw, ev)) = join.take().and_then(gateways::JoinCopies::best) {
                    if let Some(modem) = modems.get_mut(&gw) {
                        if central.activate(&gw) {
                            central.on_event(modem, ev).await?;
                        }
                    }
                }
            }
            change = changes.recv(), if changes_open => match change {
                Some(GatewayChange::Online(gw, mut modem)) => {
                    if gw == LOCAL || central.has_gateway(&gw) {
                        continue;
                    }
                    central.add_gateway(&gw);
                    central.activate(&gw);
                    match central.init_modem(&mut modem).await {
                        Ok(()) => {
                            modems.insert(gw, modem);
                            // The others take the peers it is home of without `HOME`, and it
                            // the others' (§4).
                            central.converge_due = true;
                        }
                        Err(e) => {
                            diag!("gateway {gw} did not come up: {} — dropped", e.message());
                            central.remove_gateway(&gw);
                        }
                    }
                }
                Some(GatewayChange::Offline(gw)) => drop_gateway(&mut central, &mut modems, &gw),
                None => changes_open = false,
            },
            _ = tick.tick() => {
                let names: Vec<String> = modems.keys().cloned().collect();
                for gw in names {
                    let Some(modem) = modems.get_mut(&gw) else {
                        continue;
                    };
                    if !central.activate(&gw) {
                        continue;
                    }
                    let alive = match central.probe_if_idle(modem).await {
                        Ok(()) => central.time_sync_if_due(modem, Instant::now()).await,
                        Err(e) => Err(e),
                    };
                    if let Err(e) = alive {
                        if gw == LOCAL {
                            return Err(e);
                        }
                        diag!("gateway {gw} stopped answering ({}) — dropped", e.message());
                        drop_gateway(&mut central, &mut modems, &gw);
                        continue;
                    }
                    central.apply_radio_override_if_changed(modem).await;
                    central.report_counters();
                }
                central.review_homes();
                central.converge(&mut modems).await;
                central.activate(LOCAL);
                central.update_board();
                let now = u64::try_from(crate::lorawan::gateway::now_unix()).unwrap_or(0);
                if let Err(e) = central.retire_stalled_replays(now, central.history_stall_timeout_s) {
                    diag!("could not retire stalled history replays: {}", e.message());
                }
                if let Err(e) = central.retire_expired_detaches(now) {
                    diag!("could not retire expired detaches: {}", e.message());
                }
            }
            (gw, ev) = next_event_of(&mut modems) => match ev {
                Ok(Event::Rx(rx)) if central.multi_gateway() && Central::<P>::join_addr(&rx).is_some() => {
                    let q = gateways::signal_q(rx.rssi_dbm, rx.snr_q_db);
                    match join.as_mut() {
                        Some(j) => j.add(gw, q, Event::Rx(rx)),
                        None => {
                            join = Some(gateways::JoinCopies::open(Instant::now(), gw, q, Event::Rx(rx)));
                        }
                    }
                }
                Ok(ev) => {
                    if let Some(modem) = modems.get_mut(&gw) {
                        if central.activate(&gw) {
                            central.on_event(modem, ev).await?;
                        }
                    }
                }
                Err(e) if gw == LOCAL => {
                    diag!("modem link error: {} ({:?}) — stopping", e.message(), e.kind());
                    return Err(e);
                }
                Err(e) => {
                    diag!("gateway {gw} link error: {} ({:?}) — dropped", e.message(), e.kind());
                    drop_gateway(&mut central, &mut modems, &gw);
                }
            },
        }
        // A join, a home move or a stashed answer: every gateway catches up now, not at the
        // next tick — a downlink waits on it.
        central.review_homes();
        if central.converge_due() {
            central.converge(&mut modems).await;
            central.activate(LOCAL);
        }
    }
    Ok(())
}

/// Forget a remote gateway and move the Nodes it was home of (§5.1).
fn drop_gateway<P: NorthboundPublisher>(
    central: &mut Central<P>,
    modems: &mut BTreeMap<String, Box<dyn Modem>>,
    gw: &str,
) {
    if gw == LOCAL || modems.remove(gw).is_none() {
        return;
    }
    central.remove_gateway(gw);
    central.rehome_from(gw);
    central.converge_due = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::p2p::fake::{FakeModem, RecordingPublisher, SentCmd};
    use crate::p2p::modem::{BootEvent, Status};

    const NET_ID: u32 = 0x5d0e_3be3;

    fn radio() -> RadioConfig {
        RadioConfig {
            freq_hz: 868_100_000,
            sf: 10,
            bw: 0,
            cr: 1,
            preamble: 8,
            tx_power_dbm: 14,
            flags: 0,
        }
    }

    struct TmpStore(std::path::PathBuf);
    impl Drop for TmpStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn temp_store(tag: &str) -> (Store, TmpStore) {
        let d = std::env::temp_dir().join(format!("p2p-rt-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        (Store::new(&d), TmpStore(d))
    }

    /// The `TWR_START` every (re)arm ends with.
    fn start() -> SentCmd {
        SentCmd::Twr(TwrCmd::Start {
            net_id: NET_ID,
            turnaround_ms: link::TURNAROUND_MS,
            dl_gap_ms: link::DL_GAP_MS,
            flags: link::START_ACK_ENABLED,
        })
    }

    fn central(store: Store) -> Central<RecordingPublisher> {
        Central::new(store, NET_ID, radio(), RecordingPublisher::new())
    }

    #[tokio::test]
    async fn init_marks_the_modem_alive() {
        let (store, _t) = temp_store("live-init");
        let mut c = central(store);
        let live = c.liveness();
        assert_eq!(
            live.last(),
            None,
            "no sign of life before the modem has answered"
        );
        c.init_modem(&mut FakeModem::new()).await.expect("init");
        assert!(
            live.is_fresh(now_unix_s(), 5),
            "a GET_INFO answer is a sign of life"
        );
    }

    #[tokio::test]
    async fn a_silent_modem_is_probed_and_counts_as_alive_again() {
        let (store, _t) = temp_store("live-probe");
        let mut c = central(store);
        c.liveness().mark(1);
        let mut modem = FakeModem::new();
        c.probe_if_idle(&mut modem).await.expect("probe");
        assert_eq!(modem.sent, vec![SentCmd::GetInfo]);
        assert!(c.liveness().is_fresh(now_unix_s(), 5));
    }

    /// `EVT_TWR_CTR_LOW` is best-effort: a periodic `GET_INFO` that shows a short block
    /// issues the next one, a comfortable block does not.
    #[tokio::test]
    async fn a_probe_that_sees_a_short_block_issues_the_next() {
        let (store, _t) = temp_store("live-ctr");
        let mut c = central(store.clone());
        c.liveness().mark(1);
        let mut modem = FakeModem::new();
        modem.info.state = crate::p2p::modem_io::gateway_state::GATEWAY;
        modem.info.ctr_next = 5_000;
        modem.info.ctr_last = 8_192;
        c.probe_if_idle(&mut modem).await.expect("probe");
        assert_eq!(count_twr(&modem, link::cmd::TWR_CTR_BLOCK), 0);

        c.last_info_at = None;
        modem.info.ctr_next = 7_500;
        c.probe_if_idle(&mut modem).await.expect("probe");
        assert!(modem.twr_sent().contains(&&TwrCmd::CtrBlock {
            first: 8_193,
            last: 12_288
        }));
        assert_eq!(
            SessionsDb::load(&store).expect("load").ctr_block_end,
            12_288
        );
    }

    #[tokio::test]
    async fn a_modem_heard_from_recently_is_not_probed() {
        let (store, _t) = temp_store("live-fresh");
        let mut c = central(store);
        c.liveness().mark(now_unix_s());
        c.last_info_at = Some(now_unix_s());
        let mut modem = FakeModem::new();
        c.probe_if_idle(&mut modem).await.expect("probe");
        assert!(modem.sent.is_empty(), "a quiet minute has not passed yet");
    }

    #[tokio::test]
    async fn no_probe_queues_ahead_of_a_frame_waiting_for_its_window() {
        let (store, _t) = temp_store("live-tx");
        let mut c = central(store);
        c.liveness().mark(1);
        c.tx_in_flight = Some(7);
        let mut modem = FakeModem::new();
        c.probe_if_idle(&mut modem).await.expect("probe");
        assert!(modem.sent.is_empty());
    }

    #[tokio::test]
    async fn an_unanswered_probe_is_a_dead_link() {
        let (store, _t) = temp_store("live-dead");
        let mut c = central(store);
        c.liveness().mark(1);
        let mut modem = FakeModem {
            fail_get_info: true,
            ..FakeModem::new()
        };
        let err = c.probe_if_idle(&mut modem).await.expect_err("dead link");
        assert_eq!(err.kind(), Kind::Timeout);
        let doc = c.status().document(now_unix_i(), false);
        assert_eq!(doc["state"], "unreachable");
        assert_eq!(doc["last_error"]["source"], "link");
    }

    #[tokio::test]
    async fn init_fills_the_status_document() {
        let (store, _t) = temp_store("nb-init");
        let mut c = central(store);
        let mut modem = FakeModem::new();
        modem.info.fw = [0, 2, 0];
        c.init_modem(&mut modem).await.expect("init");
        let doc = c.status().document(now_unix_i(), true);
        assert_eq!(doc["state"], "alive");
        assert_eq!(doc["fw_version"], "0.2.0");
        assert_eq!(doc["radio"]["sf"], 10);
    }

    #[tokio::test]
    async fn a_refused_radio_config_is_an_error_the_document_names() {
        let (store, _t) = temp_store("nb-refused");
        let mut c = central(store);
        let mut modem = FakeModem {
            refuse_next_config: Some(Status::InvalidState),
            ..FakeModem::new()
        };
        c.init_modem(&mut modem).await.expect_err("config refused");
        let doc = c.status().document(now_unix_i(), true);
        assert_eq!(doc["state"], "error");
        assert!(doc["last_error"]["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("SET_RADIO_CONFIG refused")));
    }

    #[tokio::test]
    async fn an_evt_log_fault_reaches_the_status_document() {
        let (store, _t) = temp_store("nb-fault");
        let mut c = central(store);
        let mut modem = FakeModem::new();
        c.init_modem(&mut modem).await.expect("init");
        c.on_event(
            &mut modem,
            Event::Fault(crate::p2p::modem::FaultEvent {
                level: crate::p2p::modem::FAULT_LEVEL_ERROR,
                code: 2,
                text: String::new(),
            }),
        )
        .await
        .expect("event");
        let doc = c.status().document(now_unix_i(), true);
        assert_eq!(doc["state"], "error");
        assert_eq!(doc["last_error"]["code"], 2);
        assert_eq!(doc["last_error"]["message"], "RX re-arm failed");
    }

    #[tokio::test]
    async fn run_central_until_initialises_then_stops_when_told() {
        let (store, _t) = temp_store("until");
        let c = central(store);
        let live = c.liveness();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let run = tokio::spawn(run_central_until(c, FakeModem::new(), async move {
            let _ = rx.await;
        }));
        // Let the loop initialise the modem before asking it to stop.
        for _ in 0..50 {
            if live.last().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(live.last().is_some(), "the modem was initialised");
        let _ = tx.send(());
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("stops promptly")
            .expect("task");
        assert!(
            result.is_ok(),
            "a requested stop is not an error: {result:?}"
        );
    }

    // ---- C8: the live spreading-factor switch (`radio-set`) ----

    /// Write the operator's override exactly as the `radio-set` dispatch arm does: under the
    /// session-DB lock, in the separate process the CLI really is.
    fn set_effective_sf(store: &Store, sf: u8, generation: u64) {
        let _g = store.lock(Slot::P2pSessions).expect("lock");
        let mut db = SessionsDb::load(store).expect("load");
        db.radio = Some(crate::p2p::db::RadioOverride {
            spreading_factor: sf,
            generation,
            set_at: "2026-09-15T10:00:00Z".into(),
        });
        db.save(store).expect("save");
    }

    /// Every `SET_RADIO_CONFIG` the runtime pushed, in order.
    fn pushed_configs(modem: &FakeModem) -> Vec<RadioConfig> {
        modem
            .sent
            .iter()
            .filter_map(|c| match c {
                SentCmd::SetRadioConfig(cfg) => Some(*cfg),
                _ => None,
            })
            .collect()
    }

    /// The operator's recorded spreading factor outranks the config key at startup, not only
    /// live: a central restarted after a `radio-set` must come up on the SF the network moved to,
    /// or every node is talking to a deaf Hub.
    #[test]
    fn from_config_prefers_the_stored_spreading_factor_over_the_config_key() {
        let (store, _t) = temp_store("from-config-sf");
        store
            .write(Slot::P2pNetId, &format!("{NET_ID:08x}"))
            .expect("net_id");
        let cfg = crate::config::RadioConfig::from_device_config_yaml(include_str!(
            "../../tests/fixtures/radio-section.yaml"
        ))
        .expect("the plan example parses");
        assert_eq!(
            cfg.p2p_radio.as_ref().expect("p2p_radio").spreading_factor,
            10,
            "the fixture's config key"
        );

        // No override: the config key stands and nothing is marked applied.
        let c = Central::from_config(store.clone(), &cfg, RecordingPublisher::default())
            .expect("from_config");
        assert_eq!(c.radio.sf, 10);
        assert_eq!(c.config_sf, 10);
        assert_eq!(c.applied_generation, 0);

        set_effective_sf(&store, 9, 1);
        let c = Central::from_config(store.clone(), &cfg, RecordingPublisher::default())
            .expect("from_config");
        assert_eq!(c.radio.sf, 9, "the override is in force");
        assert_eq!(c.config_sf, 10, "and the config key is still remembered");
        assert_eq!(c.applied_generation, 1);
    }

    /// The live switch: RX_STOP, SET_RADIO_CONFIG, TWR_START, in that order. Stopping first is
    /// not cosmetic — the modem holds its config and its receive state separately, and retuning a
    /// receiver that is mid-listen is exactly the state `INVALID_STATE` exists for.
    #[tokio::test]
    async fn a_spreading_factor_change_is_pushed_as_rx_stop_set_radio_config_twr_start() {
        let (store, _t) = temp_store("sf-switch");
        let mut c = central(store.clone());
        let mut modem = FakeModem::new();
        c.init_modem(&mut modem).await.expect("init");
        let before = modem.sent.len();

        // The CLI process writes the override while the central is running.
        set_effective_sf(&store, 9, 1);
        c.on_event(&mut modem, Event::Log("idle".into()))
            .await
            .expect("log");

        assert_eq!(
            &modem.sent[before..],
            &[
                SentCmd::RxStop,
                SentCmd::SetRadioConfig(RadioConfig { sf: 9, ..radio() }),
                start()
            ],
            "the switch is a stop/config/start triple and nothing else"
        );
        assert_eq!(c.radio.sf, 9);

        // Applied once: a second event with the same generation pushes nothing.
        let after = modem.sent.len();
        c.on_event(&mut modem, Event::Log("idle".into()))
            .await
            .expect("log");
        assert_eq!(modem.sent.len(), after, "the generation is already applied");
    }

    /// The pushed config is the one in force with one field moved. A `radio-set` that also
    /// retuned the channel, the bandwidth or the TX power would sever the link it was meant to
    /// adjust.
    #[tokio::test]
    async fn only_the_spreading_factor_changes_in_the_pushed_radio_config() {
        let (store, _t) = temp_store("sf-only");
        let mut c = central(store.clone());
        let mut modem = FakeModem::new();
        c.init_modem(&mut modem).await.expect("init");
        set_effective_sf(&store, 12, 4);
        c.on_event(&mut modem, Event::Log("idle".into()))
            .await
            .expect("log");

        let pushed = pushed_configs(&modem);
        assert_eq!(pushed.len(), 2, "the init push, then the switch");
        assert_eq!(pushed[0], radio());
        assert_eq!(pushed[1].sf, 12);
        assert_eq!(
            RadioConfig {
                sf: radio().sf,
                ..pushed[1]
            },
            radio(),
            "freq, bw, cr, preamble, tx_power and flags are untouched"
        );
    }

    /// A rebooted modem comes back with no config at all, so the re-init must push the SF the
    /// operator moved the network to — not the config key it would read from the file.
    #[tokio::test]
    async fn evt_boot_re_init_uses_the_effective_spreading_factor() {
        let (store, _t) = temp_store("sf-boot");
        let mut c = central(store.clone());
        let mut modem = FakeModem::new();
        c.init_modem(&mut modem).await.expect("init");
        set_effective_sf(&store, 11, 1);

        c.on_event(
            &mut modem,
            Event::Boot(BootEvent {
                fw_ver: [0, 1, 0],
                git_sha: [0; 4],
                reset_cause: 1,
            }),
        )
        .await
        .expect("boot");

        let pushed = pushed_configs(&modem);
        assert_eq!(pushed.last().map(|c| c.sf), Some(11));
        assert_eq!(c.radio.sf, 11);
        assert!(
            modem.sent.last() == Some(&start()),
            "RX is re-armed after the re-init"
        );
    }

    /// A `SET_RADIO_CONFIG` the modem refuses leaves the generation unapplied, so the switch is
    /// retried on the next event rather than being lost. RX is re-armed either way — a central
    /// that gave up mid-switch would be deaf until the next reboot.
    #[tokio::test]
    async fn a_refused_set_radio_config_is_retried_on_the_next_event() {
        let (store, _t) = temp_store("sf-refused");
        let mut c = central(store.clone());
        let mut modem = FakeModem::new();
        c.init_modem(&mut modem).await.expect("init");
        set_effective_sf(&store, 9, 1);

        modem.refuse_next_config = Some(Status::InvalidState);
        c.on_event(&mut modem, Event::Log("idle".into()))
            .await
            .expect("a refused config must not stop the loop");
        assert_eq!(c.radio.sf, 10, "the refused SF is not adopted");
        assert_eq!(
            modem.sent.last(),
            Some(&start()),
            "RX is re-armed at the config the modem still holds"
        );

        let before = modem.sent.len();
        c.on_event(&mut modem, Event::Log("idle".into()))
            .await
            .expect("retry");
        assert_eq!(
            &modem.sent[before..],
            &[
                SentCmd::RxStop,
                SentCmd::SetRadioConfig(RadioConfig { sf: 9, ..radio() }),
                start()
            ],
            "the unapplied generation is retried"
        );
        assert_eq!(c.radio.sf, 9);
    }

    // ---- The gateway adapter ------------------------------------------------------------

    const APP_KEY: [u8; 16] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f,
    ];
    /// The Node of `tower_join_kat.json`.
    const DEV_EUI: &str = "5876070000000413";
    const ADDR: u32 = 0x0413;
    const SERIAL: &str = "80e007b2";
    const JOIN_KAT: &str = include_str!("../../tests/fixtures/tower_join_kat.json");

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    fn register_as(store: &Store, dev_eui: &str, profile: &str) {
        let mut db = SessionsDb::load(store).expect("load");
        db.upsert_node(crate::p2p::session::RegisteredNode {
            serial: SERIAL.into(),
            dev_eui: dev_eui.into(),
            profile: profile.into(),
            app_key: to_hex(&APP_KEY),
            name: None,
        });
        db.save(store).expect("save");
    }

    fn register(store: &Store) {
        register_as(store, DEV_EUI, "default");
    }

    /// A JoinReq frame from `dev_eui` naming `named` inside, sealed under `dev_eui`'s key.
    fn join_req_naming(dev_eui: &str, named: &str, dev_nonce: u32) -> Vec<u8> {
        let eui = dev_eui_bytes(dev_eui).expect("eui");
        let body = JoinReq {
            product_type: 1,
            proto_version: 1,
            dev_eui: dev_eui_bytes(named).expect("eui"),
            fw_version: 0x0001_0500,
        }
        .encode();
        let payload = control::encode_control(&[(control::cmd::JOIN_REQ, &body)]).expect("tlv");
        frame::seal(
            &frame::join_key(&APP_KEY, &eui),
            &Header {
                frame_type: frame::frame_type::DATA,
                flags: 0,
                src: crate::p2p::tower::addr_of(dev_eui).expect("addr"),
                dest: 0,
                counter: dev_nonce,
            },
            &payload,
        )
        .expect("seal")
    }

    fn join_req(dev_nonce: u32) -> Vec<u8> {
        join_req_naming(DEV_EUI, DEV_EUI, dev_nonce)
    }

    fn rx(frame: Vec<u8>, t_ms: u64) -> Event {
        Event::Rx(RxEvent {
            t_ms,
            rssi_dbm: -58,
            snr_q_db: 36,
            frame,
        })
    }

    /// An uplink at ring seq 0: the fake gateway keeps no ring unless a test gives it one
    /// (`evt_ring_max`), so the seq is not looked at. [`ring_uplink`] sets it.
    fn uplink(counter: u32, plaintext: Vec<u8>) -> Event {
        ring_uplink(0, counter, plaintext)
    }

    fn ring_uplink(seq: u8, counter: u32, plaintext: Vec<u8>) -> Event {
        Event::TwrUplink {
            seq,
            up: TwrUplink {
                t_ms: 10_000 + u64::from(counter) * 1000,
                rssi_dbm: -58,
                snr_q_db: 36,
                frame_flags: frame::flags::CONFIRMED,
                addr: ADDR,
                counter,
                ack: UplinkAck::Sent,
                ack_tail: Default::default(),
                plaintext,
            },
        }
    }

    fn app(port: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![control::ENVELOPE_APP, port];
        v.extend_from_slice(body);
        v
    }

    fn tx_event(item: u16, outcome: link::TxOutcome) -> Event {
        ring_tx(0, item, outcome)
    }

    fn ring_tx(seq: u8, item: u16, outcome: link::TxOutcome) -> Event {
        Event::TwrTx {
            seq,
            tx: TwrTx {
                addr: ADDR,
                item,
                outcome,
                gw_counter: 7,
                node_ack_counter: 0,
                ack_rssi: None,
            },
        }
    }

    fn session_now(store: &Store) -> Session {
        SessionsDb::load(store)
            .expect("load")
            .sessions
            .get(DEV_EUI)
            .expect("session")
            .clone()
    }

    fn enqueue_seq(store: &Store, seq: u32, body: Vec<u8>) {
        let mut db = SessionsDb::load(store).expect("load");
        db.enqueue_downlink(
            DEV_EUI,
            crate::p2p::session::QueuedDownlink::with_seq(seq, body),
        )
        .expect("live session");
        db.save(store).expect("save");
    }

    /// The `TWR_QUEUE_PUSH`es sent, as `(item, ttl_s, flags, plaintext)`.
    fn pushes(modem: &FakeModem) -> Vec<(u16, u16, u8, Vec<u8>)> {
        modem
            .twr_sent()
            .into_iter()
            .filter_map(|c| match c {
                TwrCmd::QueuePush {
                    item,
                    ttl_s,
                    flags,
                    plaintext,
                    ..
                } => Some((*item, *ttl_s, *flags, plaintext.clone())),
                _ => None,
            })
            .collect()
    }

    fn count_twr(modem: &FakeModem, cmd: u8) -> usize {
        modem
            .twr_sent()
            .into_iter()
            .filter(|c| c.cmd() == cmd)
            .count()
    }

    fn areas(pubh: &RecordingPublisher, area: Area) -> Vec<serde_json::Value> {
        pubh.published()
            .into_iter()
            .filter(|p| p.area == area)
            .map(|p| serde_json::from_slice(&p.payload).expect("json"))
            .collect()
    }

    /// A minimal fPort-85 `Response{seq}` as it goes on the air: version byte first.
    fn response_body(seq: u32) -> Vec<u8> {
        let mut v = vec![crate::decode::APP_PROTO_VERSION, 0x08];
        let mut n = seq;
        loop {
            let b = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                v.push(b);
                break;
            }
            v.push(b | 0x80);
        }
        v
    }

    /// Page `index` of a `count`-page `Info` answer to `seq` (envelope fields 12/13, #425).
    fn info_page(seq: u32, index: u8, count: u8) -> Vec<u8> {
        let mut v = response_body(seq);
        v.extend_from_slice(&[0x1a, 0x00, 0x60, index, 0x68, count]);
        v
    }

    /// A `Response.error` with `code` answering `seq`.
    fn error_response(seq: u32, code: u8) -> Vec<u8> {
        let mut v = response_body(seq);
        v.extend_from_slice(&[0x32, 0x02, 0x08, code]);
        v
    }

    fn history_response(seq: u32, frame_index: u32, frame_count: u32) -> Vec<u8> {
        fn varint(mut n: u32, out: &mut Vec<u8>) {
            loop {
                let b = (n & 0x7f) as u8;
                n >>= 7;
                if n == 0 {
                    out.push(b);
                    return;
                }
                out.push(b | 0x80);
            }
        }
        // One mb-temperature column (v2.0.0 `channels`), numbered in the envelope (#425).
        let mut hf = vec![0x18];
        varint(1_000_000, &mut hf);
        hf.push(0x22);
        varint(2, &mut hf);
        hf.extend_from_slice(&[0x80, 0x09]);
        hf.push(0x30);
        varint(60, &mut hf);
        hf.extend_from_slice(&[0x52, 0x01, 0x00]);
        let mut resp = vec![crate::decode::APP_PROTO_VERSION, 0x08];
        varint(seq, &mut resp);
        resp.push(0x2a);
        varint(hf.len() as u32, &mut resp);
        resp.extend_from_slice(&hf);
        resp.push(0x60);
        varint(frame_index, &mut resp);
        resp.push(0x68);
        varint(frame_count, &mut resp);
        resp
    }

    /// Registered, initialised and joined (dev_nonce 5): the gateway holds the peer.
    async fn joined(tag: &str) -> (Store, TmpStore, Central<RecordingPublisher>, FakeModem) {
        let (store, t) = temp_store(tag);
        register(&store);
        let mut c = central(store.clone());
        let mut modem = FakeModem::new();
        c.init_modem(&mut modem).await.expect("init");
        c.on_event(&mut modem, rx(join_req(5), 1000))
            .await
            .expect("join");
        assert!(c.gw_table.contains_key(&ADDR), "the gateway holds the peer");
        (store, t, c, modem)
    }

    /// Queue `body` under `seq`, push it on a tick, and have the gateway report it delivered.
    async fn delivered(
        store: &Store,
        c: &mut Central<RecordingPublisher>,
        modem: &mut FakeModem,
        seq: u32,
        body: Vec<u8>,
    ) -> u16 {
        enqueue_seq(store, seq, body);
        c.reconcile_gateway(modem).await;
        let (item, ..) = pushes(modem).last().cloned().expect("pushed");
        assert_eq!(session_now(store).downlink_phase, DownlinkPhase::Announced);
        c.on_event(modem, tx_event(item, link::TxOutcome::Delivered))
            .await
            .expect("tx");
        item
    }

    /// Contract §3.4: the resync order, with a stray peer swept and a pushed head re-pushed;
    /// then link doc §6.2: a rebooted gateway is restored with no sweep, peers `KEEP_NEWER`.
    #[tokio::test]
    async fn init_resyncs_the_gateway_in_the_contract_order() {
        let (store, _t) = temp_store("resync");
        register(&store);
        let key = [0x5a; 16];
        {
            let mut db = SessionsDb::load(&store).expect("load");
            let mut queue = VecDeque::new();
            queue.push_back(crate::p2p::session::QueuedDownlink::with_seq(
                9,
                vec![0x08, 0x09],
            ));
            db.sessions.insert(
                DEV_EUI.into(),
                Session {
                    dev_eui: DEV_EUI.into(),
                    addr: ADDR,
                    session_key: to_hex(&key),
                    dev_nonce: 4,
                    last_seen: 41,
                    joined_at: None,
                    proto_version: 1,
                    capabilities: None,
                    hello: None,
                    downlink_queue: queue,
                    downlink_phase: DownlinkPhase::Announced,
                    history: None,
                    detach: None,
                    home: None,
                },
            );
            db.save(&store).expect("save");
        }
        let mut c = central(store.clone());
        let mut modem = FakeModem::new();
        // Configured but not started, still holding our Node and a stray one.
        modem.info.state = crate::p2p::modem_io::gateway_state::CONFIGURED;
        // A gateway with a ring (≥ 0.3.1) takes `KEEP_NEWER`.
        modem.info.evt_ring_max = 16;
        let mut page = vec![2, 0, 0xff, 0xff, 2];
        for (addr, seen) in [(ADDR, 40u32), (0x0badu32, 3)] {
            page.extend_from_slice(&addr.to_le_bytes());
            page.extend_from_slice(&seen.to_le_bytes());
            page.push(link::NODE_HOME);
        }
        modem.twr_answers.push((link::cmd::TWR_NODE_LIST, page));
        c.init_modem(&mut modem).await.expect("init");
        assert_eq!(
            modem.sent,
            vec![
                SentCmd::GetInfo,
                SentCmd::SetRadioConfig(radio()),
                SentCmd::Twr(TwrCmd::NodeList { start: 0 }),
                SentCmd::Twr(TwrCmd::NodeRemove { addr: 0x0bad }),
                SentCmd::Twr(TwrCmd::CtrBlock {
                    first: 1,
                    last: 4096
                }),
                SentCmd::Twr(TwrCmd::NodeAdd {
                    addr: ADDR,
                    session_key: key,
                    last_seen: 41,
                    flags: link::NODE_HOME | link::NODE_KEEP_NEWER,
                }),
                start(),
                SentCmd::Twr(TwrCmd::QueuePush {
                    addr: ADDR,
                    item: command_item(&QueuedDownlink::with_seq(9, vec![0x08, 0x09])),
                    ttl_s: COMMAND_TTL_S,
                    flags: link::PUSH_CONFIRMED,
                    plaintext: vec![0x81, 86, 0x08, 0x09],
                }),
            ]
        );
        assert_eq!(session_now(&store).downlink_phase, DownlinkPhase::Announced);

        // An `EVT_BOOT` runs the whole restore again, on a fresh counter block, in the order
        // of §6.2: the rebooted gateway holds nothing, so there is no sweep.
        modem.sent.clear();
        modem.info.state = crate::p2p::modem_io::gateway_state::BOOT;
        c.on_event(
            &mut modem,
            Event::Boot(BootEvent {
                fw_ver: [0, 3, 0],
                git_sha: [0; 4],
                reset_cause: 1,
            }),
        )
        .await
        .expect("boot");
        assert_eq!(
            modem.sent,
            vec![
                SentCmd::GetInfo,
                SentCmd::SetRadioConfig(radio()),
                SentCmd::Twr(TwrCmd::CtrBlock {
                    first: 4097,
                    last: 8192
                }),
                SentCmd::Twr(TwrCmd::NodeAdd {
                    addr: ADDR,
                    session_key: key,
                    last_seen: 41,
                    flags: link::NODE_HOME | link::NODE_KEEP_NEWER,
                }),
                start(),
                SentCmd::Twr(TwrCmd::QueuePush {
                    addr: ADDR,
                    item: command_item(&QueuedDownlink::with_seq(9, vec![0x08, 0x09])),
                    ttl_s: COMMAND_TTL_S,
                    flags: link::PUSH_CONFIRMED,
                    plaintext: vec![0x81, 86, 0x08, 0x09],
                }),
            ]
        );
        assert_eq!(SessionsDb::load(&store).expect("load").ctr_block_end, 8192);
    }

    /// The counter floor is on disk before the block reaches the gateway, so neither a refused
    /// block nor a restart hands a counter out twice.
    #[tokio::test]
    async fn counter_blocks_persist_across_a_restart_and_never_repeat() {
        let (store, _t) = temp_store("ctr");
        let mut modem = FakeModem::new();
        central(store.clone())
            .init_modem(&mut modem)
            .await
            .expect("init");
        assert_eq!(SessionsDb::load(&store).expect("load").ctr_block_end, 4096);

        // The gateway fails the next one: that block is spent anyway.
        let mut refusing = FakeModem::new();
        refusing
            .refuse_twr
            .push((link::cmd::TWR_CTR_BLOCK, Status::Internal));
        central(store.clone())
            .init_modem(&mut refusing)
            .await
            .expect_err("a refused block fails the init");
        assert_eq!(SessionsDb::load(&store).expect("load").ctr_block_end, 8192);

        let mut c = central(store.clone());
        let mut modem = FakeModem::new();
        c.init_modem(&mut modem).await.expect("init");
        assert!(modem.twr_sent().contains(&&TwrCmd::CtrBlock {
            first: 8193,
            last: 12288
        }));

        // Asked for more: `EVT_TWR_CTR_LOW`, a `NO_COUNTER` outcome, an uplink ACK code 4 (no counter).
        c.on_event(
            &mut modem,
            Event::TwrCtrLow(TwrCtrLow {
                next: 12200,
                last: 12288,
            }),
        )
        .await
        .expect("ctr low");
        c.on_event(&mut modem, tx_event(99, link::TxOutcome::NoCounter))
            .await
            .expect("no counter");
        assert_eq!(count_twr(&modem, link::cmd::TWR_CTR_BLOCK), 3);
        assert_eq!(SessionsDb::load(&store).expect("load").ctr_block_end, 20480);
    }

    /// A central restarted under a running Northbridge (link doc §6.3): the same config is
    /// not pushed again, another one is pushed after `RX_STOP`; the restore runs either way.
    #[tokio::test]
    async fn a_running_gateway_is_restored_without_a_refused_config() {
        let (store, _t) = temp_store("running");
        let mut modem = FakeModem::new();
        modem.info.state = crate::p2p::modem_io::gateway_state::GATEWAY;
        modem.info.radio = radio();
        central(store.clone())
            .init_modem(&mut modem)
            .await
            .expect("init");
        assert!(!modem
            .sent
            .iter()
            .any(|c| matches!(c, SentCmd::SetRadioConfig(_) | SentCmd::RxStop)));
        assert!(modem
            .twr_sent()
            .iter()
            .any(|c| matches!(c, TwrCmd::Start { .. })));

        let mut other = FakeModem::new();
        other.info.state = crate::p2p::modem_io::gateway_state::GATEWAY;
        other.info.radio = RadioConfig { sf: 12, ..radio() };
        central(store.clone())
            .init_modem(&mut other)
            .await
            .expect("init");
        assert_eq!(
            &other.sent[1..3],
            &[SentCmd::RxStop, SentCmd::SetRadioConfig(radio())]
        );
    }

    /// 0.3.0 never goes below its next counter: a block starts above what a still-running
    /// gateway reports, and a `BAD_PARAM` (a stale reading) is answered with a fresh
    /// `GET_INFO` and one block above it.
    #[tokio::test]
    async fn a_counter_block_starts_above_the_running_gateway() {
        let (store, _t) = temp_store("ctr-floor");
        let mut modem = FakeModem::new();
        modem.info.ctr_next = 60_000;
        modem.info.ctr_last = 61_000;
        let mut c = central(store.clone());
        c.init_modem(&mut modem).await.expect("init");
        assert!(modem.twr_sent().contains(&&TwrCmd::CtrBlock {
            first: 61_001,
            last: 65_096
        }));

        modem.info.ctr_next = 70_000;
        modem.info.ctr_last = 70_500;
        modem
            .refuse_twr
            .push((link::cmd::TWR_CTR_BLOCK, Status::BadParam));
        c.on_event(
            &mut modem,
            Event::TwrCtrLow(TwrCtrLow {
                next: 64_000,
                last: 65_096,
            }),
        )
        .await
        .expect("ctr low");
        assert_eq!(
            modem.twr_sent().last(),
            Some(&&TwrCmd::CtrBlock {
                first: 70_501,
                last: 74_596
            })
        );
        assert_eq!(
            SessionsDb::load(&store).expect("load").ctr_block_end,
            74_596
        );
    }

    /// A head keeps its item across a restart of the central, so the push repeated to a
    /// Northbridge that kept running replaces its entry (0.3.0), and `BUSY` (the item on air)
    /// still counts as pushed.
    #[test]
    fn a_head_is_pushed_under_a_stable_item_outside_the_answers() {
        let a = QueuedDownlink::with_seq(9, vec![1]);
        assert_eq!(command_item(&a), command_item(&a.clone()));
        assert_ne!(
            command_item(&a),
            command_item(&QueuedDownlink::with_seq(10, vec![1]))
        );
        for seq in [0, 0x7ffe, 0x7fff, u32::MAX] {
            let i = command_item(&QueuedDownlink::with_seq(seq, vec![]));
            assert!((1..ANSWER_ITEM_BASE).contains(&i), "{seq}: {i}");
        }
        let mut c = central(temp_store("items").0);
        c.item = u16::MAX;
        assert_eq!(c.next_item(), ANSWER_ITEM_BASE);
        assert_eq!(c.next_item(), ANSWER_ITEM_BASE + 1);
    }

    #[tokio::test]
    async fn a_busy_push_is_the_head_already_on_air() {
        let (store, _t, mut c, mut modem) = joined("dl-busy").await;
        modem
            .refuse_twr
            .push((link::cmd::TWR_QUEUE_PUSH, Status::Busy));
        enqueue_seq(&store, 7, vec![0x08, 0x07]);
        c.reconcile_gateway(&mut modem).await;
        assert_eq!(session_now(&store).downlink_phase, DownlinkPhase::Announced);
    }

    /// Contract §3.2 against `tower_join_kat.json`: the JoinAccept byte for byte, and the
    /// session key the gateway is handed.
    #[tokio::test]
    async fn the_join_accept_matches_the_kat() {
        let kat: serde_json::Value = serde_json::from_str(JOIN_KAT).expect("kat");
        let (store, _t) = temp_store("kat");
        register(&store);
        let net_id = kat["net_id"].as_u64().expect("net_id") as u32;
        let mut c = Central::new(store.clone(), net_id, radio(), RecordingPublisher::new())
            .with_node_tx_power(14);
        c.central_nonce = Some(kat["central_nonce"].as_u64().expect("nonce") as u32);
        let mut modem = FakeModem::new();
        c.on_event(
            &mut modem,
            rx(
                unhex(kat["join_request"]["frame"].as_str().expect("frame")),
                1000,
            ),
        )
        .await
        .expect("join");
        let scheduled = modem.scheduled();
        assert_eq!(scheduled.len(), 1, "one JoinAccept");
        assert_eq!(
            to_hex(&scheduled[0].frame),
            kat["join_accept"]["frame"].as_str().expect("frame")
        );
        let s = session_now(&store);
        assert_eq!(s.session_key, kat["session_key"].as_str().expect("key"));
        assert_eq!((s.addr, s.dev_nonce, s.last_seen), (1043, 17, 0));
        assert_eq!(s.proto_version, 1);
        let key: [u8; 16] = unhex(kat["session_key"].as_str().expect("key"))
            .try_into()
            .expect("16 B");
        assert!(modem.twr_sent().contains(&&TwrCmd::NodeAdd {
            addr: 1043,
            session_key: key,
            last_seen: 0,
            flags: link::NODE_HOME,
        }));
        let info = areas(&c.publisher, Area::Info);
        assert_eq!(info.len(), 1, "the retained info of the joined Node");
        assert_eq!(info[0]["transport"], "p2p");
        assert_eq!(info[0]["dev_addr"], "00000413");

        // The KAT's first uplink, as the gateway hands it over.
        c.on_event(
            &mut modem,
            uplink(
                kat["first_uplink"]["counter"].as_u64().expect("counter") as u32,
                unhex(kat["first_uplink"]["plaintext"].as_str().expect("pt")),
            ),
        )
        .await
        .expect("uplink");
        // Learned at once: no `GET_INFO` here, so the ring's start is learned.
        c.ack_if_due(&mut modem, Instant::now() + LEARN_FOR).await;
        assert_eq!(session_now(&store).last_seen, 1);
    }

    /// #486 W8: a configured power above 14 dBm goes out as 14 — the KAT's JoinAccept (14
    /// dBm) byte for byte — and 0 still assigns nothing.
    #[tokio::test]
    async fn the_join_accept_never_assigns_more_than_14_dbm() {
        let kat: serde_json::Value = serde_json::from_str(JOIN_KAT).expect("kat");
        let net_id = kat["net_id"].as_u64().expect("net_id") as u32;
        for (configured, sent) in [(22u8, 14u8), (15, 14), (14, 14), (0, 0)] {
            let (store, _t) = temp_store("kat-power");
            register(&store);
            let mut c = Central::new(store.clone(), net_id, radio(), RecordingPublisher::new())
                .with_node_tx_power(configured);
            c.central_nonce = Some(kat["central_nonce"].as_u64().expect("nonce") as u32);
            let mut modem = FakeModem::new();
            c.on_event(
                &mut modem,
                rx(
                    unhex(kat["join_request"]["frame"].as_str().expect("frame")),
                    1000,
                ),
            )
            .await
            .expect("join");
            let scheduled = modem.scheduled();
            assert_eq!(scheduled.len(), 1, "one JoinAccept");
            let join_key: [u8; 16] = unhex(kat["join_key"].as_str().expect("join_key"))
                .try_into()
                .expect("16 B");
            let (_, pt) = frame::open(&join_key, &scheduled[0].frame).expect("open");
            let Ok(Envelope::Control(tlvs)) = Envelope::parse(&pt) else {
                panic!("a 0x91 JoinAccept");
            };
            let ja = JoinAccept::decode(tlvs[0].value).expect("JoinAccept");
            assert_eq!(ja.tx_power, sent, "configured {configured} dBm");
            if configured >= 14 {
                assert_eq!(
                    to_hex(&scheduled[0].frame),
                    kat["join_accept"]["frame"].as_str().expect("frame")
                );
            }
        }
    }

    /// #486 U-1: a 72 B command body is the longest one frame carries (`0x81 86` + 72 = 74 B
    /// plaintext, a 96 B frame); one byte more is dropped, not pushed.
    #[tokio::test]
    async fn a_72_byte_body_is_pushed_and_73_is_not() {
        assert_eq!(MAX_COMMAND_BODY, 72);
        let (store, _t, mut c, mut modem) = joined("dl-72").await;
        enqueue_seq(&store, 7, vec![0x08; 73]);
        enqueue_seq(&store, 8, vec![0x08; 72]);
        c.reconcile_gateway(&mut modem).await;
        let p = pushes(&modem);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].3.len(), frame::MAX_PAYLOAD);
        assert_eq!(p[0].3.len() + frame::HEADER_LEN + frame::TAG_LEN, 96);
        assert_eq!(session_now(&store).downlink_queue[0].seq, Some(8));
    }

    /// Every refusal of contract §3.2 answers nothing and commits nothing.
    #[tokio::test]
    async fn a_join_is_refused_for_each_reason_of_the_contract() {
        async fn no_answer(store: &Store, frame: Vec<u8>) -> FakeModem {
            let mut c = central(store.clone());
            let mut modem = FakeModem::new();
            c.on_event(&mut modem, rx(frame, 1000))
                .await
                .expect("event");
            assert!(modem.scheduled().is_empty(), "nothing answered");
            modem
        }

        // Unregistered.
        let (store, _t) = temp_store("refuse-unreg");
        no_answer(&store, join_req(5)).await;
        assert!(SessionsDb::load(&store).expect("load").sessions.is_empty());

        // A bad tag.
        let (store, _t2) = temp_store("refuse-tag");
        register(&store);
        let mut forged = join_req(5);
        let last = forged.len() - 1;
        forged[last] ^= 0x01;
        no_answer(&store, forged).await;

        // Sealed under the Node's key, naming another DevEUI.
        no_answer(&store, join_req_naming(DEV_EUI, "5876070000000999", 5)).await;
        assert!(SessionsDb::load(&store).expect("load").sessions.is_empty());

        // A second Node on the same address.
        let (store, _t3) = temp_store("refuse-shared");
        register(&store);
        register_as(&store, "70b3d57e00000413", "default");
        no_answer(&store, join_req(5)).await;
        assert!(SessionsDb::load(&store).expect("load").sessions.is_empty());

        // A replayed or older dev_nonce.
        let (store, _t4, mut c, mut modem) = joined("refuse-replay").await;
        for nonce in [5, 4] {
            c.on_event(&mut modem, rx(join_req(nonce), 3000))
                .await
                .expect("event");
        }
        assert_eq!(modem.scheduled().len(), 1, "only the first join answered");
        assert_eq!(session_now(&store).dev_nonce, 5);

        // Parked: the session stays, the Node is not on the P2P view.
        let (store, _t5, ..) = joined("refuse-parked").await;
        {
            let mut db = SessionsDb::load(&store).expect("load");
            db.nodes.clear();
            db.save(&store).expect("save");
        }
        let before = session_now(&store);
        no_answer(&store, join_req(9)).await;
        assert_eq!(session_now(&store), before);
    }

    /// A JoinAccept the modem refuses leaves the session the Node already had.
    #[tokio::test]
    async fn a_refused_join_accept_commits_nothing() {
        let (store, _t, mut c, mut modem) = joined("refused-accept").await;
        let before = session_now(&store);
        modem.refuse_next = Some(Status::Busy);
        c.on_event(&mut modem, rx(join_req(6), 3000))
            .await
            .expect("event");
        assert_eq!(modem.scheduled().len(), 2, "tried");
        assert_eq!(session_now(&store), before);
    }

    /// A `node-remove` landing while the JoinAccept is on its way wins.
    #[tokio::test]
    async fn a_removal_during_the_join_round_trip_is_not_overwritten() {
        let (store, _t) = temp_store("join-removed");
        register(&store);
        let mut c = central(store.clone());
        let mut modem = FakeModem::new();
        let s2 = store.clone();
        modem.on_tx_schedule = Some(std::sync::Arc::new(move || {
            let mut db = SessionsDb::load(&s2).expect("load");
            db.remove_node(DEV_EUI);
            db.save(&s2).expect("save");
        }));
        c.on_event(&mut modem, rx(join_req(5), 1000))
            .await
            .expect("join");
        assert_eq!(modem.scheduled().len(), 1);
        assert!(SessionsDb::load(&store).expect("load").sessions.is_empty());
        assert_eq!(count_twr(&modem, link::cmd::TWR_NODE_ADD), 0);
    }

    /// A downlink queued during the join round trip survives the commit, and is pushed.
    #[tokio::test]
    async fn a_downlink_queued_before_the_join_survives_it_and_is_pushed() {
        let (store, _t, mut c, mut modem) = joined("join-queue").await;
        enqueue_seq(&store, 3, vec![0x08, 0x03]);
        c.on_event(&mut modem, rx(join_req(6), 3000))
            .await
            .expect("rejoin");
        let s = session_now(&store);
        assert_eq!(s.dev_nonce, 6);
        assert_eq!(s.downlink_queue.len(), 1);
        assert_eq!(s.downlink_phase, DownlinkPhase::Announced);
        assert_eq!(
            pushes(&modem).last().expect("push").3,
            vec![0x81, 86, 0x08, 0x03]
        );
    }

    /// The gateway dedups, the central still refuses a counter it has seen.
    #[tokio::test]
    async fn an_uplink_counter_not_above_the_last_is_dropped() {
        let (store, _t, mut c, mut modem) = joined("dedup").await;
        let body = app(port::TELEMETRY, &unhex(GOLDEN_TELEMETRY));
        for counter in [3, 3, 2] {
            c.on_event(&mut modem, uplink(counter, body.clone()))
                .await
                .expect("uplink");
        }
        assert_eq!(areas(&c.publisher, Area::Reading).len(), 1);
        assert_eq!(session_now(&store).last_seen, 3);
    }

    /// The Node's ACK of a downlink spends one of its uplink counters (SC 2026-09-28): an
    /// uplink carrying that counter again is a replay, the next one is fresh.
    #[tokio::test]
    async fn the_node_ack_counter_is_spent() {
        let (store, _t, mut c, mut modem) = joined("node-ack").await;
        let body = app(port::TELEMETRY, &unhex(GOLDEN_TELEMETRY));
        c.on_event(&mut modem, uplink(3, body.clone()))
            .await
            .expect("uplink");
        enqueue_seq(&store, 7, vec![0x08, 0x07]);
        c.reconcile_gateway(&mut modem).await;
        let (item, ..) = pushes(&modem).last().cloned().expect("pushed");
        c.on_event(
            &mut modem,
            Event::TwrTx {
                seq: 0,
                tx: TwrTx {
                    node_ack_counter: 4,
                    ..match tx_event(item, link::TxOutcome::Delivered) {
                        Event::TwrTx { tx, .. } => tx,
                        _ => unreachable!(),
                    }
                },
            },
        )
        .await
        .expect("tx");
        assert_eq!(session_now(&store).last_seen, 4);
        for counter in [4, 5] {
            c.on_event(&mut modem, uplink(counter, body.clone()))
                .await
                .expect("uplink");
        }
        assert_eq!(
            areas(&c.publisher, Area::Reading).len(),
            2,
            "4 dropped, 5 taken"
        );
        assert_eq!(session_now(&store).last_seen, 5);
    }

    /// `last_seen` is kept per uplink and handed back to the gateway at the next resync.
    #[tokio::test]
    async fn last_seen_is_persisted_and_given_back_to_the_gateway() {
        let (store, _t, mut c, mut modem) = joined("last-seen").await;
        for counter in 1..=4 {
            c.on_event(&mut modem, uplink(counter, app(port::TELEMETRY, &[0x01])))
                .await
                .expect("uplink");
        }
        assert_eq!(session_now(&store).last_seen, 4);
        let mut restarted = central(store.clone());
        let mut modem = FakeModem::new();
        restarted.init_modem(&mut modem).await.expect("init");
        assert!(modem.twr_sent().iter().any(|c| matches!(
            c,
            TwrCmd::NodeAdd {
                addr: ADDR,
                last_seen: 4,
                ..
            }
        )));
    }

    const GOLDEN_TELEMETRY: &str = "01088901100018b226206b40024809d00125";

    /// An fPort-2 body is decoded as the LoRaWAN arm decodes it, and cached.
    #[tokio::test]
    async fn telemetry_publishes_the_node_reading_and_fills_the_cache() {
        let (store, _t, mut c, mut modem) = joined("telemetry").await;
        c.on_event(
            &mut modem,
            uplink(1, app(port::TELEMETRY, &unhex(GOLDEN_TELEMETRY))),
        )
        .await
        .expect("uplink");
        let r = areas(&c.publisher, Area::Reading);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0]["dev_eui"], DEV_EUI);
        assert_eq!(r[0]["serial"], SERIAL);
        assert_eq!(r[0]["fport"], 2);
        assert_eq!(r[0]["fields"]["temperature"], 24.57);
        assert_eq!(r[0]["counters"]["counter"], 1);
        assert_eq!(r[0]["gateways"][0]["rssi"], -58);
        assert_eq!(r[0]["gateways"][0]["snr"], 9.0);
        let doc = crate::cache::Cache::new(&store, Transport::P2p)
            .read(DEV_EUI)
            .expect("read")
            .expect("cached");
        assert!(doc.slot(CacheSlot::Message).is_some());
    }

    /// T1-F1: an uplink carries the time the gateway heard it (its `t_ms` through the
    /// `GET_INFO` anchor), not the time the central handled it — five readings a 45 s outage
    /// held in the ring stay five points. No anchor, or a mapped time ahead of the host clock,
    /// falls back to now.
    #[tokio::test]
    async fn an_uplink_is_stamped_with_the_time_the_gateway_heard_it() {
        let (store, _t, mut c, mut modem) = joined("heard-at").await;
        let fmt = crate::lorawan::gateway::format_rfc3339;
        let now = crate::lorawan::gateway::now_unix();
        // Counter n is heard at modem t_ms 10 000 + n·1000; the anchor puts t_ms 60 000 at
        // host now, so counter 1 was heard 49 s ago and counter 5 45 s ago.
        let now_ms = u64::try_from(now).expect("clock") * 1000;
        c.clock_anchor = Some((60_000, now_ms));
        for counter in 1..=5 {
            c.on_event(
                &mut modem,
                uplink(counter, app(port::TELEMETRY, &unhex(GOLDEN_TELEMETRY))),
            )
            .await
            .expect("uplink");
        }
        let stamps: Vec<_> = areas(&c.publisher, Area::Reading)
            .iter()
            .map(|r| r["received_at"].as_str().expect("received_at").to_owned())
            .collect();
        let heard: Vec<_> = (1..=5).map(|n| fmt(now - 50 + n)).collect();
        assert_eq!(stamps, heard);
        let cached = crate::cache::Cache::new(&store, Transport::P2p)
            .read(DEV_EUI)
            .expect("read")
            .expect("cached");
        assert_eq!(
            cached.slot(CacheSlot::Message).expect("message").at,
            fmt(now - 45)
        );
        let fallbacks = |c: &Central<RecordingPublisher>| {
            c.status().document(now_unix_i(), true)["counters"]["rx_time_fallback"].clone()
        };
        assert_eq!(fallbacks(&c), 0, "all five mapped");

        // An anchor from before a reboot nobody saw maps ahead of now: now it is.
        c.clock_anchor = Some((0, now_ms));
        let before = crate::lorawan::gateway::now_unix();
        c.on_event(
            &mut modem,
            uplink(6, app(port::TELEMETRY, &unhex(GOLDEN_TELEMETRY))),
        )
        .await
        .expect("uplink");
        let last = areas(&c.publisher, Area::Reading).pop().expect("reading");
        let after = crate::lorawan::gateway::now_unix();
        let got = last["received_at"].as_str().expect("received_at");
        assert!((before..=after).any(|t| fmt(t) == got), "{got}");
        assert_eq!(fallbacks(&c), 1, "counted");
        // No anchor at all: now, counted.
        c.clock_anchor = None;
        c.on_event(
            &mut modem,
            uplink(7, app(port::TELEMETRY, &unhex(GOLDEN_TELEMETRY))),
        )
        .await
        .expect("uplink");
        assert_eq!(fallbacks(&c), 2);
    }

    /// §4: a profile the config does not declare decodes nothing, the counter still advances.
    #[tokio::test]
    async fn an_unknown_profile_publishes_nothing() {
        let (store, _t) = temp_store("no-profile");
        register_as(&store, DEV_EUI, "cold-chain");
        let mut c = central(store.clone());
        let mut modem = FakeModem::new();
        c.init_modem(&mut modem).await.expect("init");
        c.on_event(&mut modem, rx(join_req(5), 1000))
            .await
            .expect("join");
        c.on_event(
            &mut modem,
            uplink(1, app(port::TELEMETRY, &unhex(GOLDEN_TELEMETRY))),
        )
        .await
        .expect("uplink");
        assert!(areas(&c.publisher, Area::Reading).is_empty());
        assert_eq!(session_now(&store).last_seen, 1);
    }

    /// Contract §3.3: Capabilities, LinkCheckReq and TimeReq are answered in one unconfirmed
    /// `0x91` push; the Node's Capabilities are kept.
    #[tokio::test]
    async fn control_requests_are_answered_in_one_0x91_push() {
        let (store, _t, mut c, mut modem) = joined("answers").await;
        let node_caps = Capabilities {
            proto_version: 1,
            mtu: 96,
            profiles: control::PROFILE_LORA,
            cmd_bitmap: [1u64, 2, 3, 4, 7, 8, 0x10, 0x20]
                .iter()
                .fold(0, |m, b| m | (1 << b)),
            power_class: 1,
        };
        // The central's own, byte for byte: proto 1, MTU 96 (#486 U-1), lora, its commands,
        // mains.
        let mut central = vec![1, 96, control::PROFILE_LORA];
        central.extend_from_slice(&0x0001_0005_0021_019Eu64.to_le_bytes());
        central.push(0);
        assert_eq!(Capabilities::central().encode().to_vec(), central);
        let up = uplink(
            2,
            control::encode_control(&[
                (control::cmd::CAPABILITIES, &node_caps.encode()),
                (control::cmd::LINK_CHECK, &[]),
                (control::cmd::TIME, &[]),
            ])
            .expect("tlv"),
        );
        // Modem t_ms 12 000 is host 1 800 000 000 250 ms.
        c.clock_anchor = Some((11_000, 1_799_999_999_250));
        c.on_event(&mut modem, up).await.expect("uplink");
        let expected = control::encode_control(&[
            (
                control::cmd::CAPABILITIES,
                &Capabilities::central().encode(),
            ),
            (
                control::cmd::LINK_CHECK,
                &LinkCheckAns::from_reception(-58, 36, 10, 1).encode(),
            ),
            (
                control::cmd::TIME,
                &TimeAns::from_unix_ms(1_800_000_000_250, 2).encode(),
            ),
        ])
        .expect("tlv");
        let (item, ttl, flags, plaintext) = pushes(&modem).last().cloned().expect("pushed");
        assert_eq!((ttl, flags), (ANSWER_TTL_S, 0));
        assert_eq!(plaintext, expected);
        assert_eq!(session_now(&store).capabilities, Some(node_caps));
        assert_eq!(session_now(&store).last_seen, 2);

        // Its outcome is only logged.
        c.on_event(&mut modem, tx_event(item, link::TxOutcome::Sent))
            .await
            .expect("tx");
        assert!(c.answers.is_empty());
    }

    /// F (T3 on c66): a newer `0x91` request supersedes an older answer still waiting on the
    /// gateway — it is dropped, and only the newest is queued.
    #[tokio::test]
    async fn a_newer_request_drops_the_unsent_older_answer() {
        let (_store, _t, mut c, mut modem) = joined("answers-one").await;
        let req = || control::encode_control(&[(control::cmd::LINK_CHECK, &[])]).expect("tlv");
        for counter in 1..=4 {
            c.on_event(&mut modem, uplink(counter, req()))
                .await
                .expect("uplink");
        }
        let pushed: Vec<u16> = pushes(&modem).iter().map(|p| p.0).collect();
        assert_eq!(pushed.len(), 4);
        let dropped: Vec<u16> = modem
            .twr_sent()
            .iter()
            .filter_map(|c| match c {
                TwrCmd::QueueDrop { addr, item } if *addr == ADDR => Some(*item),
                _ => None,
            })
            .collect();
        assert_eq!(dropped, pushed[..3], "each older one dropped");
        assert_eq!(c.answers.len(), 1);
        assert_eq!(c.answers[0].0, pushed[3], "the newest waits");
    }

    /// No `TimeAns` from a clock that is not set: better none than a wrong one.
    #[tokio::test]
    async fn no_time_is_answered_from_an_unset_clock() {
        let (_store, _t, mut c, mut modem) = joined("no-time").await;
        c.clock_anchor = Some((0, 5_000));
        let before = pushes(&modem).len();
        c.on_event(
            &mut modem,
            uplink(
                1,
                control::encode_control(&[(control::cmd::TIME, &[])]).expect("tlv"),
            ),
        )
        .await
        .expect("uplink");
        assert_eq!(pushes(&modem).len(), before, "nothing to answer");
    }

    /// An unknown control command is skipped, the rest of the list is still served.
    #[tokio::test]
    async fn an_unknown_control_command_is_skipped() {
        let (store, _t, mut c, mut modem) = joined("unknown-tlv").await;
        c.on_event(
            &mut modem,
            uplink(
                1,
                control::encode_control(&[(0x3f, &[1, 2, 3]), (control::cmd::LINK_CHECK, &[])])
                    .expect("tlv"),
            ),
        )
        .await
        .expect("uplink");
        let (.., plaintext) = pushes(&modem).last().cloned().expect("pushed");
        let answered = match Envelope::parse(&plaintext).expect("parse") {
            Envelope::Control(tlvs) => tlvs.iter().map(|t| t.cmd).collect::<Vec<_>>(),
            other => panic!("not a control payload: {other:?}"),
        };
        assert_eq!(answered, vec![control::cmd::LINK_CHECK]);

        // A list cut short decodes nothing; its counter is still the gateway's.
        c.on_event(&mut modem, uplink(2, vec![0x91, 0x10, 0x04, 0x01]))
            .await
            .expect("uplink");
        assert_eq!(session_now(&store).last_seen, 2);
    }

    /// S2 / F3 over the gateway: pushed → delivered (`Sent`) → answered (retired).
    #[tokio::test]
    async fn a_downlink_is_pushed_delivered_and_retired_by_its_answer() {
        let (store, _t, mut c, mut modem) = joined("dl-flow").await;
        delivered(&store, &mut c, &mut modem, 7, vec![0x08, 0x07, 0x3a, 0x00]).await;
        assert_eq!(session_now(&store).downlink_phase, DownlinkPhase::Sent);

        // An answer to another command keeps the head.
        c.on_event(
            &mut modem,
            uplink(1, app(port::RESPONSE, &response_body(6))),
        )
        .await
        .expect("uplink");
        assert_eq!(session_now(&store).downlink_queue.len(), 1);

        c.on_event(
            &mut modem,
            uplink(2, app(port::RESPONSE, &response_body(7))),
        )
        .await
        .expect("uplink");
        let s = session_now(&store);
        assert!(s.downlink_queue.is_empty());
        assert_eq!(s.downlink_phase, DownlinkPhase::Idle);
        assert_eq!(areas(&c.publisher, Area::Command).len(), 2);
    }

    /// #128: a paged answer holds the queue until its last page; only then does the next
    /// downlink go out, so it cannot cut into the Node's page stream.
    #[tokio::test]
    async fn a_paged_answer_holds_the_queue_until_its_last_page() {
        let (store, _t, mut c, mut modem) = joined("dl-paged").await;
        delivered(&store, &mut c, &mut modem, 7, vec![0x08, 0x07, 0x3a, 0x00]).await;
        enqueue_seq(&store, 8, vec![0x08, 0x08, 0x3a, 0x00]);
        let before = pushes(&modem).len();

        for (counter, index) in [(1, 0), (2, 1), (3, 1)] {
            c.on_event(
                &mut modem,
                uplink(counter, app(port::RESPONSE, &info_page(7, index, 3))),
            )
            .await
            .expect("page");
            let s = session_now(&store);
            assert_eq!(s.downlink_queue.len(), 2, "page {index} holds the head");
            assert_eq!(s.downlink_phase, DownlinkPhase::Sent);
            assert_eq!(pushes(&modem).len(), before, "nothing pushed mid-stream");
        }
        let seen = session_now(&store).downlink_queue[0]
            .pages
            .clone()
            .expect("pages");
        assert_eq!(seen.missing(), vec![2]);

        c.on_event(
            &mut modem,
            uplink(4, app(port::RESPONSE, &info_page(7, 2, 3))),
        )
        .await
        .expect("last page");
        let s = session_now(&store);
        assert_eq!(s.downlink_queue.len(), 1);
        assert_eq!(s.downlink_queue[0].seq, Some(8));
        assert_eq!(s.downlink_phase, DownlinkPhase::Announced);
        assert_eq!(pushes(&modem).len(), before + 1, "the next head goes out");
    }

    /// #128: a page stream that stops retires its request after three uplinks without a page,
    /// rather than asking again from page 0; the next downlink goes out.
    #[tokio::test]
    async fn a_stopped_page_stream_retires_its_request() {
        let (store, _t, mut c, mut modem) = joined("dl-paged-stall").await;
        delivered(&store, &mut c, &mut modem, 7, vec![0x08, 0x07, 0x3a, 0x00]).await;
        let next = vec![0x08, 0x08, 0x3a, 0x00];
        enqueue_seq(&store, 8, next.clone());
        c.on_event(
            &mut modem,
            uplink(1, app(port::RESPONSE, &info_page(7, 0, 3))),
        )
        .await
        .expect("page");
        for counter in 2..=4 {
            c.on_event(&mut modem, uplink(counter, app(port::TELEMETRY, &[0x01])))
                .await
                .expect("uplink");
        }
        let s = session_now(&store);
        assert_eq!(s.downlink_queue.len(), 1);
        assert_eq!(s.downlink_queue[0].seq, Some(8));
        assert_eq!(s.downlink_phase, DownlinkPhase::Announced);
        assert_eq!(
            pushes(&modem).last().expect("pushed").3,
            control::command_payload(&next)
        );
    }

    /// #128: a Node's error answer retires its command and publishes the code; a late page of
    /// an answer already retired changes nothing.
    #[tokio::test]
    async fn an_error_answer_retires_and_publishes_its_code() {
        let (store, _t, mut c, mut modem) = joined("dl-error").await;
        delivered(&store, &mut c, &mut modem, 7, vec![0x08, 0x07, 0x3a, 0x00]).await;
        c.on_event(
            &mut modem,
            uplink(1, app(port::RESPONSE, &error_response(7, 3))),
        )
        .await
        .expect("error");
        let s = session_now(&store);
        assert!(s.downlink_queue.is_empty());
        assert_eq!(s.downlink_phase, DownlinkPhase::Idle);
        let docs = areas(&c.publisher, Area::Command);
        assert_eq!(docs.last().expect("doc")["error"]["code"], 3);

        c.on_event(
            &mut modem,
            uplink(2, app(port::RESPONSE, &info_page(7, 1, 3))),
        )
        .await
        .expect("late page");
        let s = session_now(&store);
        assert!(s.downlink_queue.is_empty());
        assert_eq!(s.downlink_phase, DownlinkPhase::Idle);
    }

    /// A push the gateway could not deliver goes back to `Idle` and out again.
    #[tokio::test]
    async fn an_undelivered_downlink_is_pushed_again() {
        let (store, _t, mut c, mut modem) = joined("dl-undelivered").await;
        enqueue_seq(&store, 7, vec![0x08, 0x07]);
        c.reconcile_gateway(&mut modem).await;
        let before = pushes(&modem).len();
        let (first, ..) = pushes(&modem).last().cloned().expect("pushed");
        c.on_event(&mut modem, tx_event(first, link::TxOutcome::NotDelivered))
            .await
            .expect("tx");
        assert_eq!(pushes(&modem).len(), before + 1, "pushed again");
        let (second, ..) = pushes(&modem).last().cloned().expect("pushed again");
        // The head keeps its item: 0.3.0 replaces by `(addr, item)`, so no push can double it.
        assert_eq!(first, second);
        assert_eq!(session_now(&store).downlink_phase, DownlinkPhase::Announced);
    }

    /// PN-4: a `force_send` has no fPort-85 answer; delivered, it retires and the next goes.
    #[tokio::test]
    async fn a_delivered_force_send_retires_and_the_queue_moves_on() {
        use crate::decode::command_encode::Downlink;
        let (store, _t, mut c, mut modem) = joined("dl-pn4").await;
        let force = Downlink::ForceSend
            .encode(15, Transport::P2p)
            .expect("force_send");
        let reboot = Downlink::Reboot.encode(16, Transport::P2p).expect("reboot");
        enqueue_seq(&store, 16, reboot.clone());
        {
            let mut db = SessionsDb::load(&store).expect("load");
            let s = db.sessions.get_mut(DEV_EUI).expect("session");
            s.downlink_queue
                .push_front(crate::p2p::session::QueuedDownlink::with_seq(15, force));
            db.save(&store).expect("save");
        }
        c.reconcile_gateway(&mut modem).await;
        let (item, ..) = pushes(&modem).last().cloned().expect("pushed");
        c.on_event(&mut modem, tx_event(item, link::TxOutcome::Delivered))
            .await
            .expect("tx");
        let s = session_now(&store);
        assert_eq!(s.downlink_queue.len(), 1);
        assert_eq!(s.downlink_queue[0].seq, Some(16));
        assert_eq!(s.downlink_phase, DownlinkPhase::Announced);
        assert_eq!(
            pushes(&modem).last().expect("next").3,
            control::command_payload(&reboot)
        );
    }

    /// F3: three uplinks without the answer, and the delivered head is pushed again.
    #[tokio::test]
    async fn a_delivered_head_is_pushed_again_after_three_uplinks_without_its_answer() {
        let (store, _t, mut c, mut modem) = joined("dl-reannounce").await;
        delivered(&store, &mut c, &mut modem, 7, vec![0x08, 0x07, 0x3a, 0x00]).await;
        let before = pushes(&modem).len();
        for counter in 1..=2 {
            c.on_event(&mut modem, uplink(counter, app(port::TELEMETRY, &[0x01])))
                .await
                .expect("uplink");
        }
        assert_eq!(session_now(&store).downlink_phase, DownlinkPhase::Sent);
        c.on_event(&mut modem, uplink(3, app(port::TELEMETRY, &[0x01])))
            .await
            .expect("uplink");
        assert_eq!(pushes(&modem).len(), before + 1);
        assert_eq!(session_now(&store).downlink_phase, DownlinkPhase::Announced);
    }

    /// A Hello says the Node restarted: a delivered head goes out again at once.
    #[tokio::test]
    async fn a_hello_re_pushes_a_delivered_head() {
        let (store, _t, mut c, mut modem) = joined("dl-hello").await;
        delivered(&store, &mut c, &mut modem, 7, vec![0x08, 0x07, 0x3a, 0x00]).await;
        let mut hello = vec![0u8; Hello::LEN];
        hello[4] = 1;
        c.on_event(
            &mut modem,
            uplink(
                1,
                control::encode_control(&[(control::cmd::HELLO, &hello)]).expect("tlv"),
            ),
        )
        .await
        .expect("uplink");
        let s = session_now(&store);
        assert_eq!(s.downlink_phase, DownlinkPhase::Announced);
        assert_eq!(s.hello.map(|h| h.reset_reason), Some(1));
    }

    /// §4.3: a history stream retires its request only once every frame has arrived, and a
    /// duplicate frame publishes once.
    #[tokio::test]
    async fn a_history_stream_retires_its_request_when_complete() {
        use crate::decode::command_encode::Downlink;
        let (store, _t, mut c, mut modem) = joined("history").await;
        let body = Downlink::ReqHistory {
            from_unix: None,
            to_unix: None,
        }
        .encode(21, Transport::P2p)
        .expect("ReqHistory");
        delivered(&store, &mut c, &mut modem, 21, body).await;
        for (counter, index) in [(1, 1), (2, 1), (3, 0)] {
            c.on_event(
                &mut modem,
                uplink(
                    counter,
                    app(port::RESPONSE, &history_response(21, index, 3)),
                ),
            )
            .await
            .expect("frame");
            assert_eq!(
                session_now(&store).downlink_queue.len(),
                1,
                "still collecting"
            );
        }
        // Three uplinks went by, but a stream in progress is never pushed again.
        assert_eq!(session_now(&store).downlink_phase, DownlinkPhase::Sent);
        c.on_event(
            &mut modem,
            uplink(4, app(port::RESPONSE, &history_response(21, 2, 3))),
        )
        .await
        .expect("frame");
        let s = session_now(&store);
        assert!(s.downlink_queue.is_empty());
        assert!(s.history.is_none());
        assert_eq!(
            areas(&c.publisher, Area::History).len(),
            3,
            "one per fresh frame"
        );
    }

    /// A pushed head that leaves the queue (a Portal discard) is dropped from the gateway.
    #[tokio::test]
    async fn a_discarded_head_is_dropped_from_the_gateway() {
        let (store, _t, mut c, mut modem) = joined("dl-discard").await;
        enqueue_seq(&store, 7, vec![0x08, 0x07]);
        c.reconcile_gateway(&mut modem).await;
        let (item, ..) = pushes(&modem).last().cloned().expect("pushed");
        {
            let mut db = SessionsDb::load(&store).expect("load");
            assert_eq!(db.drop_queued_seqs(DEV_EUI, &[7]), 1);
            db.save(&store).expect("save");
        }
        c.reconcile_gateway(&mut modem).await;
        assert!(modem
            .twr_sent()
            .contains(&&TwrCmd::QueueDrop { addr: ADDR, item }));
        assert!(c.pushed.is_empty());
    }

    /// A head too long for one frame is dropped rather than pushed.
    #[tokio::test]
    async fn an_oversize_head_is_dropped_not_pushed() {
        let (store, _t, mut c, mut modem) = joined("dl-oversize").await;
        enqueue_seq(&store, 7, vec![0x08; MAX_COMMAND_BODY + 1]);
        enqueue_seq(&store, 8, vec![0x08, 0x08]);
        c.reconcile_gateway(&mut modem).await;
        assert_eq!(pushes(&modem).len(), 1);
        assert_eq!(session_now(&store).downlink_queue[0].seq, Some(8));
    }

    /// The tick takes a removed or moved Node off the gateway and gives a moved-back one to
    /// it with its counter.
    #[tokio::test]
    async fn the_tick_keeps_the_gateway_equal_to_the_p2p_view() {
        let (store, _t, mut c, mut modem) = joined("reconcile").await;
        c.on_event(&mut modem, uplink(3, app(port::TELEMETRY, &[0x01])))
            .await
            .expect("uplink");
        let registered = SessionsDb::load(&store)
            .expect("load")
            .node(DEV_EUI)
            .cloned()
            .expect("node");
        // Moved off P2P: the session is parked.
        {
            let mut db = SessionsDb::load(&store).expect("load");
            db.nodes.remove(DEV_EUI);
            db.park_session(DEV_EUI);
            db.save(&store).expect("save");
        }
        c.reconcile_gateway(&mut modem).await;
        assert_eq!(
            modem.twr_sent().last(),
            Some(&&TwrCmd::NodeRemove { addr: ADDR })
        );
        assert!(c.gw_table.is_empty());
        // An uplink under a parked session is not served, and takes it off again.
        c.on_event(&mut modem, uplink(4, app(port::TELEMETRY, &[0x01])))
            .await
            .expect("uplink");
        assert_eq!(session_now(&store).last_seen, 3);

        // Moved back.
        let mut db = SessionsDb::load(&store).expect("load");
        db.upsert_node(registered);
        db.save(&store).expect("save");
        c.reconcile_gateway(&mut modem).await;
        assert!(matches!(
            modem.twr_sent().last(),
            Some(TwrCmd::NodeAdd {
                addr: ADDR,
                last_seen: 3,
                ..
            })
        ));
    }

    /// A data frame the gateway passed up means it lost the peer: the tick hands it over again.
    #[tokio::test]
    async fn a_data_frame_on_evt_rx_re_adds_the_peer() {
        let (_store, _t, mut c, mut modem) = joined("rx-data").await;
        let kat: serde_json::Value = serde_json::from_str(JOIN_KAT).expect("kat");
        let mut frame = unhex(kat["first_uplink"]["frame"].as_str().expect("frame"));
        // Aimed at our network.
        frame[6..10].copy_from_slice(&NET_ID.to_le_bytes());
        c.on_event(&mut modem, rx(frame, 5000)).await.expect("rx");
        assert!(c.gw_table.is_empty());
        let adds = count_twr(&modem, link::cmd::TWR_NODE_ADD);
        c.reconcile_gateway(&mut modem).await;
        assert_eq!(count_twr(&modem, link::cmd::TWR_NODE_ADD), adds + 1);
    }

    /// An uplink from an address with no session here takes the stray peer off the gateway.
    #[tokio::test]
    async fn an_uplink_from_an_unknown_address_removes_the_peer() {
        let (_store, _t, mut c, mut modem) = joined("unknown-addr").await;
        c.on_event(
            &mut modem,
            Event::TwrUplink {
                seq: 0,
                up: TwrUplink {
                    addr: 0x0bad,
                    ..match uplink(1, app(port::TELEMETRY, &[0x01])) {
                        Event::TwrUplink { up, .. } => up,
                        _ => unreachable!(),
                    }
                },
            },
        )
        .await
        .expect("uplink");
        assert_eq!(
            modem.twr_sent().last(),
            Some(&&TwrCmd::NodeRemove { addr: 0x0bad })
        );
    }

    /// A JoinReq long past its RX window is not answered.
    #[tokio::test]
    async fn a_stale_join_req_is_not_answered() {
        let (store, _t) = temp_store("stale");
        register(&store);
        let mut c = central(store.clone());
        let mut modem = FakeModem::new();
        c.observe_modem_clock(60_000);
        c.on_event(&mut modem, rx(join_req(5), 1000))
            .await
            .expect("rx");
        assert!(modem.scheduled().is_empty());
    }

    /// The JoinAccept teaches the TX lead, and an SF change waits for it to be sent.
    #[tokio::test]
    async fn an_sf_change_waits_for_the_join_accept_in_flight() {
        let (store, _t) = temp_store("sf-accept");
        register(&store);
        let mut c = central(store.clone());
        let mut modem = FakeModem::new();
        c.init_modem(&mut modem).await.expect("init");
        c.on_event(&mut modem, rx(join_req(5), 1000))
            .await
            .expect("join");
        let accept = (*modem.scheduled().last().expect("JoinAccept")).clone();
        assert_eq!(
            accept.t_ms,
            modem::schedule_at(1000, DEFAULT_RX1_DELAY_S, airtime::TX_LEAD_SEED_MS)
        );
        set_effective_sf(&store, 9, 1);
        let before = modem.sent.len();
        c.on_event(&mut modem, Event::Log("idle".into()))
            .await
            .expect("log");
        assert_eq!(
            modem.sent.len(),
            before,
            "the JoinAccept is still in flight"
        );

        let on_air = airtime::lora_airtime_ms(accept.frame.len(), &radio());
        c.on_event(
            &mut modem,
            Event::TxDone(crate::p2p::modem::TxDone {
                tx_id: accept.tx_id,
                status: crate::p2p::modem::TxStatus::Ok,
                t_actual_ms: accept.t_ms + 10 + on_air.round() as u64,
            }),
        )
        .await
        .expect("tx done");
        assert_eq!(c.tx_lead.estimate_ms().round(), 10.0);
        assert_eq!(
            &modem.sent[before..],
            &[
                SentCmd::RxStop,
                SentCmd::SetRadioConfig(RadioConfig { sf: 9, ..radio() }),
                start(),
            ]
        );
    }

    // ---- #470 C3: the event ring, the restore of link doc §6.2 / §6.3, bounded retries ----

    use crate::p2p::tower::delivery::{ACK_WITHIN, LEARN_FOR};

    /// [`joined`] against a gateway that keeps an event ring (C1 firmware).
    async fn joined_ring(tag: &str) -> (Store, TmpStore, Central<RecordingPublisher>, FakeModem) {
        let (store, t) = temp_store(tag);
        register(&store);
        let mut c = central(store.clone());
        let mut modem = FakeModem::new();
        modem.info.evt_ring_max = 16;
        c.init_modem(&mut modem).await.expect("init");
        c.on_event(&mut modem, rx(join_req(5), 1000))
            .await
            .expect("join");
        // A central that found the gateway running: it learns where the ring starts.
        c.delivery.restart();
        (store, t, c, modem)
    }

    fn telemetry(seq: u8, counter: u32) -> Event {
        ring_uplink(seq, counter, app(port::TELEMETRY, &[0x01]))
    }

    fn readings(c: &Central<RecordingPublisher>) -> usize {
        areas(&c.publisher, Area::Reading).len()
    }

    /// End the learning of a fresh central at once: what it saw so far is the start.
    async fn settle(c: &mut Central<RecordingPublisher>, modem: &mut FakeModem) {
        c.ack_if_due(modem, Instant::now() + LEARN_FOR).await;
    }

    /// Make every directory of the store read-only (`true`) or writable again: a save fails.
    fn read_only(dir: &std::path::Path, ro: bool) {
        use std::os::unix::fs::PermissionsExt;
        for e in std::fs::read_dir(dir).expect("dir").flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                read_only(&e.path(), ro);
            }
        }
        let mode = if ro { 0o500 } else { 0o700 };
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }

    /// Link doc §4.1: a ring event is acked only once what it did is stored. One whose
    /// handling failed is not acked, nor is anything after it; its resend is processed.
    #[tokio::test]
    async fn a_ring_event_is_acked_only_after_it_is_stored() {
        let (_store, t, mut c, mut modem) = joined_ring("ring-persist").await;
        c.on_event(&mut modem, telemetry(10, 1)).await.expect("10");
        assert!(modem.acks().is_empty(), "nothing acked while learning");
        settle(&mut c, &mut modem).await;
        assert_eq!(modem.acks(), vec![10]);
        read_only(&t.0, true);
        c.on_event(&mut modem, telemetry(11, 2)).await.expect("11");
        read_only(&t.0, false);
        assert_eq!(readings(&c), 1, "11 was not stored");
        c.ack_if_due(&mut modem, Instant::now() + ACK_WITHIN).await;
        assert_eq!(modem.acks(), vec![10], "not past the failed 11");
        c.on_event(&mut modem, telemetry(11, 2))
            .await
            .expect("resend");
        assert_eq!(readings(&c), 2, "the resend is processed");
        assert_eq!(modem.acks(), vec![10, 11]);
    }

    /// #130: the ack path stages the outbox entry and `sessions.db` and flushes them together;
    /// once the event is acked both are in place and no temp file is left anywhere.
    #[tokio::test]
    async fn a_ring_event_leaves_both_files_in_place_and_no_temp_file() {
        fn temps(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).expect("dir").flatten() {
                let p = e.path();
                if p.is_dir() {
                    temps(&p, out);
                } else if p.extension().is_some_and(|x| x == "tmp") {
                    out.push(p);
                }
            }
        }
        let (store, t, mut c, mut modem) = joined_ring("ring-staged").await;
        c.on_event(&mut modem, telemetry(1, 1)).await.expect("1");
        settle(&mut c, &mut modem).await;
        c.on_event(&mut modem, telemetry(2, 2)).await.expect("2");
        assert_eq!(modem.acks(), vec![1, 2]);
        assert_eq!(session_now(&store).last_seen, 2);
        assert!(
            c.outbox_ref().pending() > 0 || readings(&c) == 2,
            "stored northbound"
        );
        let mut left = Vec::new();
        temps(&t.0, &mut left);
        assert!(left.is_empty(), "temp files left: {left:?}");
    }

    /// Fix C (§8.8): a processed ring event is acked as soon as it is stored. Resends and
    /// events dropped ahead of a gap keep the link doc §4.1 pacing: an ack once eight are
    /// owed one, or 100 ms after the first of them.
    #[tokio::test]
    async fn a_stored_event_is_acked_at_once_and_resends_on_the_pacing() {
        let (_store, _t, mut c, mut modem) = joined_ring("ring-pacing").await;
        c.on_event(&mut modem, telemetry(1, 1)).await.expect("1");
        settle(&mut c, &mut modem).await;
        assert_eq!(modem.acks(), vec![1]);
        for seq in 2..=3u8 {
            c.on_event(&mut modem, telemetry(seq, u32::from(seq)))
                .await
                .expect("uplink");
        }
        assert_eq!(modem.acks(), vec![1, 2, 3], "each at once");
        assert_eq!(c.ack_deadline(), None, "nothing owed");
        for _ in 0..7 {
            c.on_event(&mut modem, telemetry(3, 3))
                .await
                .expect("resend");
        }
        assert_eq!(
            modem.acks(),
            vec![1, 2, 3],
            "seven resends owed, inside 100 ms"
        );
        c.on_event(&mut modem, telemetry(3, 3))
            .await
            .expect("resend");
        assert_eq!(modem.acks(), vec![1, 2, 3, 3], "the eighth sends it");
        c.on_event(&mut modem, telemetry(2, 2))
            .await
            .expect("resend");
        c.ack_if_due(&mut modem, Instant::now()).await;
        assert_eq!(modem.acks(), vec![1, 2, 3, 3], "paced");
        assert!(c.ack_deadline().is_some());
        c.ack_if_due(&mut modem, Instant::now() + ACK_WITHIN).await;
        assert_eq!(modem.acks(), vec![1, 2, 3, 3, 3], "100 ms on");
        assert_eq!(c.ack_deadline(), None, "nothing owed");
        // A stopping central sends what it owes, paced or not.
        c.on_event(&mut modem, telemetry(1, 1))
            .await
            .expect("resend");
        c.ack_owed(&mut modem).await;
        assert_eq!(modem.acks(), vec![1, 2, 3, 3, 3, 3]);
    }

    /// `EVT_TWR_UPLINK` `ack` 5 is reserved (planner, 2026-10-06): the event is acked like any
    /// other, nothing is published or stored, and it is counted.
    #[tokio::test]
    async fn an_uplink_with_the_reserved_ack_is_acked_dropped_and_counted() {
        let (_store, _t, mut c, mut modem) = joined_ring("ring-reserved-ack").await;
        c.on_event(&mut modem, telemetry(1, 1)).await.expect("1");
        settle(&mut c, &mut modem).await;
        let Event::TwrUplink { seq, mut up } = telemetry(2, 2) else {
            unreachable!()
        };
        up.ack = UplinkAck::SuppressedRingFull;
        c.on_event(&mut modem, Event::TwrUplink { seq, up })
            .await
            .expect("2");
        c.ack_if_due(&mut modem, Instant::now() + ACK_WITHIN).await;
        assert_eq!(modem.acks(), vec![1, 2], "acked");
        assert_eq!(readings(&c), 1, "not published");
        let counters = c.status().document(now_unix_i(), true)["counters"].clone();
        assert_eq!(counters["evt_reserved_ack"], 1);
        // Its counter was not taken: the next uplink with it is fresh.
        c.on_event(&mut modem, telemetry(3, 2)).await.expect("3");
        assert_eq!(readings(&c), 2);
    }

    /// Fix C (HIL c66): the ack leaves right after the save, ahead of the UART work the
    /// handler still has to do — here the queue push of a downlink that waits for the Node.
    /// The 100 ms pacing deadline cannot fire while the handler holds the loop.
    #[tokio::test]
    async fn the_ack_goes_out_before_the_rest_of_the_handler_waits() {
        let (store, _t, mut c, mut modem) = joined_ring("ring-ack-first").await;
        c.on_event(&mut modem, telemetry(1, 1)).await.expect("1");
        settle(&mut c, &mut modem).await;
        enqueue_seq(&store, 7, vec![0x08, 0x07]);
        let before = modem.sent.len();
        c.on_event(&mut modem, telemetry(2, 2)).await.expect("2");
        let sent = &modem.sent[before..];
        let ack = sent
            .iter()
            .position(|c| matches!(c, SentCmd::EvtAck(2)))
            .expect("acked");
        let push = sent
            .iter()
            .position(|c| matches!(c, SentCmd::Twr(TwrCmd::QueuePush { .. })))
            .expect("the downlink is pushed");
        assert!(ack < push, "the ack goes first: {sent:?}");
    }

    /// #130: an ack that leaves later than [`RING_ACK_SLOW`] after the event was taken, or
    /// after the gateway heard it, is logged with the time of each step; a quick one is not.
    #[test]
    fn a_slow_ring_ack_is_logged_with_its_steps() {
        use std::time::Duration;
        let mut t = RingTrace::new(9, Some(1_000_000));
        let t0 = t.taken;
        t.steps.push(("lock", t0 + Duration::from_millis(5)));
        t.steps.push(("save", t0 + Duration::from_millis(255)));
        let line = t
            .slow_line(t0 + Duration::from_millis(260), 1_000_400)
            .expect("slow");
        assert!(line.contains("seq=9 acked 260 ms"), "{line}");
        assert!(line.contains("400 ms after the gateway heard it"), "{line}");
        assert!(line.contains("lock 5 save 250 ack 5"), "{line}");
        let quick = RingTrace::new(10, Some(1_000_000));
        let q0 = quick.taken;
        assert_eq!(
            quick.slow_line(q0 + Duration::from_millis(30), 1_000_090),
            None
        );
        // Quick to handle, but heard long ago: it sat in the UART behind other work.
        let queued = RingTrace::new(11, Some(1_000_000));
        let line = queued
            .slow_line(queued.taken + Duration::from_millis(20), 1_000_350)
            .expect("late since heard");
        assert!(line.contains("350 ms after the gateway heard it"), "{line}");
    }

    /// E (T3 on c66): a central (re)started mid resend pass of a full ring of 32 gets seqs
    /// 12..=31 (counters 1998..=2017) before the pass wraps to 0..=11 (1986..=1997). It holds
    /// them, learns the start once 12 comes again, and processes all 32 in seq order: none is
    /// dropped by the `(addr, counter)` replay rule.
    #[tokio::test]
    async fn a_central_started_mid_pass_processes_a_full_ring_in_seq_order() {
        let (store, _t, mut c, mut modem) = joined_ring("ring-mid-pass").await;
        let counter = |seq: u8| 1986 + u32::from(seq);
        for seq in (12..=31u8).chain(0..=11) {
            c.on_event(&mut modem, telemetry(seq, counter(seq)))
                .await
                .expect("held");
        }
        assert_eq!(readings(&c), 0, "nothing processed while learning");
        // The next pass: 12 again proves a whole pass was seen.
        c.on_event(&mut modem, telemetry(12, counter(12)))
            .await
            .expect("pass");
        let got: Vec<u64> = areas(&c.publisher, Area::Reading)
            .iter()
            .map(|r| r["counters"]["counter"].as_u64().expect("counter"))
            .collect();
        assert_eq!(got, (1986..=2017).collect::<Vec<u64>>(), "all 32, in order");
        assert_eq!(session_now(&store).last_seen, 2017);
        assert_eq!(modem.acks().last(), Some(&31));
        // The rest of that pass is resends.
        for seq in 13..=31u8 {
            c.on_event(&mut modem, telemetry(seq, counter(seq)))
                .await
                .expect("resend");
        }
        assert_eq!(readings(&c), 32);
    }

    /// Northbridge 0.3.1 final reports its ring's head in `GET_INFO` (172 B): a fresh central
    /// expects that seq at once — no learning, and each event acked once stored.
    #[tokio::test]
    async fn a_gateway_that_reports_its_head_is_not_learned() {
        let (store, _t) = temp_store("ring-head");
        register(&store);
        let mut c = central(store.clone());
        let mut modem = FakeModem::new();
        modem.info.evt_ring_max = 32;
        modem.info.evt_head_seq = Some(12);
        c.init_modem(&mut modem).await.expect("init");
        assert_eq!(c.delivery.last(), Some(11));
        c.on_event(&mut modem, rx(join_req(5), 1000))
            .await
            .expect("join");
        // Arriving out of order: 13 waits for 12.
        c.on_event(&mut modem, telemetry(13, 2)).await.expect("13");
        assert_eq!(readings(&c), 0);
        c.on_event(&mut modem, telemetry(12, 1)).await.expect("12");
        c.on_event(&mut modem, telemetry(13, 2)).await.expect("13");
        assert_eq!(readings(&c), 2);
        assert_eq!(modem.acks(), vec![12, 13]);
    }

    /// A gap relearn on a gateway that reports its head asks `GET_INFO` and continues from the
    /// head, counting what the gateway dropped; it does not learn.
    #[tokio::test]
    async fn a_gap_relearn_asks_the_gateway_for_its_head() {
        let (store, _t) = temp_store("ring-head-gap");
        register(&store);
        let mut c = central(store.clone());
        let mut modem = FakeModem::new();
        modem.info.evt_ring_max = 32;
        modem.info.evt_head_seq = Some(1);
        c.init_modem(&mut modem).await.expect("init");
        c.on_event(&mut modem, rx(join_req(5), 1000))
            .await
            .expect("join");
        c.on_event(&mut modem, telemetry(1, 1)).await.expect("1");
        // 2..=4 are gone from the gateway; it now delivers from 5.
        modem.info.evt_head_seq = Some(5);
        // 5 has been arriving ahead of the expected 2 for as long as a relearn waits.
        let gap_began = Instant::now() - crate::p2p::tower::delivery::GAP_RELEARN_AFTER;
        assert_eq!(c.delivery.admit(5, gap_began), Admit::Gap);
        let infos = |m: &FakeModem| m.sent.iter().filter(|c| **c == SentCmd::GetInfo).count();
        let before = infos(&modem);
        c.on_event(&mut modem, telemetry(5, 5)).await.expect("5");
        assert_eq!(infos(&modem), before + 1, "asked for the head");
        assert_eq!(c.delivery.last(), Some(5), "no learning");
        assert_eq!(readings(&c), 2, "5 processed at once");
        assert_eq!(modem.acks(), vec![1, 5]);
        let doc = c.status().document(now_unix_i(), true);
        assert_eq!(doc["counters"]["evt_seq_gap"], 3);
    }

    /// A resent ring event goes northbound once: dropped by its seq even where the
    /// `(addr, counter)` dedup would not catch it, and re-acked.
    #[tokio::test]
    async fn a_resent_ring_event_is_not_published_twice() {
        let (store, _t, mut c, mut modem) = joined_ring("ring-dedup").await;
        for (seq, counter) in [(5, 1), (6, 2)] {
            c.on_event(&mut modem, telemetry(seq, counter))
                .await
                .expect("uplink");
        }
        settle(&mut c, &mut modem).await;
        assert_eq!(modem.acks(), vec![5, 6], "each once stored, in seq order");
        // The gateway missed the ack and resends both; one carries a counter this central
        // would take, so only the seq tells it is a resend.
        for (seq, counter) in [(5, 1), (6, 3)] {
            c.on_event(&mut modem, telemetry(seq, counter))
                .await
                .expect("resend");
        }
        assert_eq!(readings(&c), 2);
        assert_eq!(session_now(&store).last_seen, 2);
        c.ack_if_due(&mut modem, Instant::now() + ACK_WITHIN).await;
        assert_eq!(modem.acks(), vec![5, 6, 6], "the resends are acked again");

        // An `EVT_TWR_TX` resent does not move a delivered head twice.
        enqueue_seq(&store, 7, vec![0x08, 0x07]);
        c.reconcile_gateway(&mut modem).await;
        let (item, ..) = pushes(&modem).last().cloned().expect("pushed");
        let pushed = pushes(&modem).len();
        for _ in 0..2 {
            c.on_event(&mut modem, ring_tx(7, item, link::TxOutcome::NotDelivered))
                .await
                .expect("tx");
        }
        assert_eq!(pushes(&modem).len(), pushed + 1, "pushed again once");
        assert_eq!(session_now(&store).downlink_queue[0].undelivered, 1);
    }

    /// The ring seq wraps 255 → 0 inside the window: 0 is the next one, 255 a resend.
    #[tokio::test]
    async fn the_ring_seq_wraps_from_255_to_0() {
        let (_store, _t, mut c, mut modem) = joined_ring("ring-wrap").await;
        c.on_event(&mut modem, telemetry(254, 1))
            .await
            .expect("254");
        settle(&mut c, &mut modem).await;
        for (seq, counter) in [(255, 2), (0, 3), (1, 4), (255, 2), (130, 9)] {
            c.on_event(&mut modem, telemetry(seq, counter))
                .await
                .expect("uplink");
        }
        assert_eq!(readings(&c), 4, "255 resent and 130 (out of order) dropped");
        c.ack_if_due(&mut modem, Instant::now() + ACK_WITHIN).await;
        assert_eq!(modem.acks(), vec![254, 255, 0, 1, 1], "the drops re-ack 1");
    }

    /// After `EVT_BOOT` the first ring event is seq 1 (link doc §4.1): taken and acked
    /// without learning, a stale higher one is not.
    #[tokio::test]
    async fn after_evt_boot_the_ring_starts_at_seq_1() {
        let (_store, _t, mut c, mut modem) = joined_ring("ring-boot").await;
        c.on_event(&mut modem, telemetry(200, 1))
            .await
            .expect("200");
        settle(&mut c, &mut modem).await;
        c.on_event(
            &mut modem,
            Event::Boot(BootEvent {
                fw_ver: [0, 4, 0],
                git_sha: [0; 4],
                reset_cause: 1,
            }),
        )
        .await
        .expect("boot");
        c.on_event(&mut modem, telemetry(201, 2))
            .await
            .expect("201");
        c.on_event(&mut modem, telemetry(1, 3)).await.expect("1");
        assert_eq!(readings(&c), 2, "201 is not the next");
        c.ack_if_due(&mut modem, Instant::now() + ACK_WITHIN).await;
        assert_eq!(modem.acks(), vec![200, 1]);
    }

    /// A 0.3.0 gateway (`evt_ring_max` 0) numbers every event: its seq is not looked at and
    /// nothing is acked — 0x18 would be an unknown command to it.
    #[tokio::test]
    async fn a_gateway_without_a_ring_gets_no_acks() {
        let (_store, _t, mut c, mut modem) = joined("ring-none").await;
        for counter in 1..=10 {
            c.on_event(&mut modem, telemetry(3, counter))
                .await
                .expect("uplink");
        }
        c.ack_owed(&mut modem).await;
        assert_eq!(readings(&c), 10);
        assert!(modem.acks().is_empty());
        assert_eq!(c.ack_deadline(), None);
    }

    /// Link doc §4.2: a new join hands the peer over with `last_seen` 0 and no `KEEP_NEWER`;
    /// every restore sets it (the resync test covers §6.2, the next one §6.3).
    #[tokio::test]
    async fn a_join_adds_the_peer_without_keep_newer() {
        let (_store, _t, _c, modem) = joined("join-flags").await;
        let adds: Vec<_> = modem
            .twr_sent()
            .into_iter()
            .filter_map(|c| match c {
                TwrCmd::NodeAdd {
                    last_seen, flags, ..
                } => Some((*last_seen, *flags)),
                _ => None,
            })
            .collect();
        assert_eq!(adds, vec![(0, link::NODE_HOME)]);
    }

    /// Link doc §6.3: a central restarted under a gateway still running this config on this
    /// network sends a new block, the peers with `KEEP_NEWER` and the pending downlinks under
    /// the same items — no `SET_RADIO_CONFIG`, no `TWR_START`.
    #[tokio::test]
    async fn a_running_gateway_is_restored_in_the_6_3_order() {
        let (store, _t, mut c, mut modem) = joined_ring("restore-6-3").await;
        c.on_event(&mut modem, telemetry(1, 3))
            .await
            .expect("uplink");
        settle(&mut c, &mut modem).await;
        enqueue_seq(&store, 7, vec![0x08, 0x07]);
        c.reconcile_gateway(&mut modem).await;
        let (item, ..) = pushes(&modem).last().cloned().expect("pushed");

        let mut c = central(store.clone());
        let mut modem = FakeModem::new();
        modem.info.state = crate::p2p::modem_io::gateway_state::GATEWAY;
        modem.info.radio = radio();
        modem.info.net_id = NET_ID;
        modem.info.turnaround_ms = link::TURNAROUND_MS;
        modem.info.dl_gap_ms = link::DL_GAP_MS;
        modem.info.evt_ring_max = 16;
        modem.info.ctr_next = 100;
        modem.info.ctr_last = 4096;
        c.init_modem(&mut modem).await.expect("restore");
        let cmds: Vec<u8> = modem.twr_sent().iter().map(|c| c.cmd()).collect();
        assert_eq!(
            cmds,
            vec![
                link::cmd::TWR_NODE_LIST,
                link::cmd::TWR_CTR_BLOCK,
                link::cmd::TWR_NODE_ADD,
                link::cmd::TWR_QUEUE_PUSH,
            ]
        );
        assert_eq!(modem.sent[0], SentCmd::GetInfo);
        assert!(!modem
            .sent
            .iter()
            .any(|c| matches!(c, SentCmd::SetRadioConfig(_) | SentCmd::RxStop)));
        assert!(modem.twr_sent().iter().any(|c| matches!(
            c,
            TwrCmd::NodeAdd { last_seen: 3, flags, .. }
                if *flags == link::NODE_HOME | link::NODE_KEEP_NEWER
        )));
        assert_eq!(
            pushes(&modem)[0].0,
            item,
            "the same item: the gateway replaces, never doubles"
        );

        // On another network it is started again.
        let mut other = FakeModem::new();
        other.info = modem.info;
        other.info.net_id = NET_ID ^ 1;
        central(store.clone())
            .init_modem(&mut other)
            .await
            .expect("restore");
        assert!(other
            .twr_sent()
            .iter()
            .any(|c| matches!(c, TwrCmd::Start { .. })));
    }

    /// A gateway that rebooted with its `EVT_BOOT` lost shows a clock that went back: it is
    /// restored as on a boot (§6.2), and its ring is learned afresh.
    #[tokio::test]
    async fn a_missed_reboot_is_seen_by_the_clock_and_restored() {
        let (store, _t, mut c, mut modem) = joined_ring("missed-boot").await;
        c.on_event(&mut modem, telemetry(1, 3))
            .await
            .expect("uplink");
        let block = SessionsDb::load(&store).expect("load").ctr_block_end;
        c.clock_anchor = Some((3_600_000, now_unix_ms()));
        c.last_info_at = None;
        c.tx_in_flight = None;
        modem.sent.clear();
        modem.info.now_ms = 2_000;
        c.probe_if_idle(&mut modem).await.expect("probe");
        assert!(modem.twr_sent().iter().any(|c| matches!(
            c,
            TwrCmd::NodeAdd { flags, .. } if *flags == link::NODE_HOME | link::NODE_KEEP_NEWER
        )));
        assert!(modem.twr_sent().contains(&&TwrCmd::CtrBlock {
            first: block + 1,
            last: block + 4096
        }));
        // The fake answers in `boot`: a rebooted gateway numbers from 1, nothing to learn.
        assert_eq!(c.delivery.last(), Some(0), "known from seq 1");
    }

    /// The HIL knob (T5): blocks of the override size, renewed on `EVT_TWR_CTR_LOW`, on a
    /// `NO_COUNTER` outcome and on a probe that sees the block short.
    #[tokio::test]
    async fn a_small_counter_block_is_renewed_on_every_ask() {
        let (store, _t) = temp_store("ctr-small");
        let mut c = central(store.clone()).with_counter_block(16);
        let mut modem = FakeModem::new();
        c.init_modem(&mut modem).await.expect("init");
        c.on_event(
            &mut modem,
            Event::TwrCtrLow(TwrCtrLow { next: 13, last: 16 }),
        )
        .await
        .expect("ctr low");
        c.on_event(&mut modem, tx_event(99, link::TxOutcome::NoCounter))
            .await
            .expect("no counter");
        modem.info.state = crate::p2p::modem_io::gateway_state::GATEWAY;
        modem.info.ctr_next = 46;
        modem.info.ctr_last = 48;
        c.last_info_at = None;
        c.probe_if_idle(&mut modem).await.expect("probe");
        let blocks: Vec<_> = modem
            .twr_sent()
            .into_iter()
            .filter_map(|c| match c {
                TwrCmd::CtrBlock { first, last } => Some((*first, *last)),
                _ => None,
            })
            .collect();
        assert_eq!(blocks, vec![(1, 16), (17, 32), (33, 48), (49, 64)]);
        assert_eq!(SessionsDb::load(&store).expect("load").ctr_block_end, 64);
    }

    /// `NO_COUNTER` is fail-closed: nothing went on air unsealed, so a new block goes out
    /// first, then the same item again, and the head spends none of its chances.
    #[tokio::test]
    async fn a_no_counter_outcome_issues_a_block_and_pushes_the_head_again() {
        let (store, _t, mut c, mut modem) = joined("dl-no-counter").await;
        enqueue_seq(&store, 7, vec![0x08, 0x07]);
        c.reconcile_gateway(&mut modem).await;
        let (item, ..) = pushes(&modem).last().cloned().expect("pushed");
        modem.sent.clear();
        c.on_event(&mut modem, tx_event(item, link::TxOutcome::NoCounter))
            .await
            .expect("tx");
        let cmds: Vec<u8> = modem.twr_sent().iter().map(|c| c.cmd()).collect();
        assert_eq!(
            cmds,
            vec![link::cmd::TWR_CTR_BLOCK, link::cmd::TWR_QUEUE_PUSH]
        );
        assert_eq!(pushes(&modem)[0].0, item);
        assert_eq!(session_now(&store).downlink_queue[0].undelivered, 0);
    }

    /// The gateway attempts a confirmed command once per push; the central pushes it again
    /// up to [`UNDELIVERED_LIMIT`] unACKed attempts, then gives it up with an `undelivered`
    /// `command` document and moves on. `EXPIRED` (never on air) spends no chance.
    #[tokio::test]
    async fn an_undelivered_command_is_retried_a_bounded_number_of_times() {
        use crate::p2p::session::{UNDELIVERED, UNDELIVERED_LIMIT};
        let (store, _t, mut c, mut modem) = joined("dl-bounded").await;
        enqueue_seq(&store, 7, vec![0x08, 0x07]);
        enqueue_seq(&store, 8, vec![0x08, 0x08]);
        c.reconcile_gateway(&mut modem).await;
        let (item, _, flags, _) = pushes(&modem).last().cloned().expect("pushed");
        assert_eq!(flags, link::PUSH_CONFIRMED);
        c.on_event(&mut modem, tx_event(item, link::TxOutcome::Expired))
            .await
            .expect("expired");
        for n in 1..UNDELIVERED_LIMIT {
            c.on_event(&mut modem, tx_event(item, link::TxOutcome::NotDelivered))
                .await
                .expect("tx");
            assert_eq!(session_now(&store).downlink_queue[0].undelivered, n);
            assert_eq!(
                pushes(&modem).last().expect("pushed").0,
                item,
                "the same item"
            );
        }
        assert!(areas(&c.publisher, Area::Command).is_empty());
        c.on_event(&mut modem, tx_event(item, link::TxOutcome::NotDelivered))
            .await
            .expect("tx");
        let s = session_now(&store);
        assert_eq!(s.downlink_queue.len(), 1, "given up");
        assert_eq!(s.downlink_queue[0].seq, Some(8));
        assert_eq!(
            s.downlink_phase,
            DownlinkPhase::Announced,
            "the next one is out"
        );
        assert_ne!(pushes(&modem).last().expect("pushed").0, item);
        let docs = areas(&c.publisher, Area::Command);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["status"], UNDELIVERED);
        assert_eq!(docs[0]["kind"], UNDELIVERED);
        assert_eq!(docs[0]["seq"], 7);
        assert_eq!(docs[0]["attempts"], UNDELIVERED_LIMIT);
        assert_eq!(docs[0]["dev_eui"], DEV_EUI);
    }

    /// A `0x91` answer goes out unconfirmed and is never pushed again: the Node asks again.
    #[tokio::test]
    async fn an_undelivered_0x91_answer_is_not_pushed_again() {
        let (_store, _t, mut c, mut modem) = joined("answer-once").await;
        c.on_event(
            &mut modem,
            uplink(
                1,
                control::encode_control(&[(control::cmd::LINK_CHECK, &[])]).expect("tlv"),
            ),
        )
        .await
        .expect("uplink");
        let (item, _, flags, _) = pushes(&modem).last().cloned().expect("answered");
        assert_eq!(flags, 0, "unconfirmed");
        let before = pushes(&modem).len();
        c.on_event(&mut modem, tx_event(item, link::TxOutcome::NotDelivered))
            .await
            .expect("tx");
        assert_eq!(pushes(&modem).len(), before);
        assert!(c.answers.is_empty());
    }

    // ---- The northbound outbox (p2p_tower_gateway.md §8.6) ----

    /// A broker for the outbox tests. While it is down a publish either fails at once
    /// (`hang: false`: the client refused it) or waits for it to come back (`hang: true`:
    /// rumqttc holding the publish across the reconnect). It records what it acked.
    #[derive(Clone)]
    struct Broker {
        up: std::sync::Arc<tokio::sync::watch::Sender<bool>>,
        hang: bool,
        acked: std::sync::Arc<std::sync::Mutex<Vec<(Area, serde_json::Value)>>>,
    }

    impl Broker {
        fn new(up: bool, hang: bool) -> Self {
            Broker {
                up: std::sync::Arc::new(tokio::sync::watch::channel(up).0),
                hang,
                acked: Default::default(),
            }
        }
        fn set_up(&self, up: bool) {
            self.up.send_replace(up);
        }
        /// The counters of the readings acked, in order.
        fn readings(&self) -> Vec<u64> {
            self.acked
                .lock()
                .expect("lock")
                .iter()
                .filter(|(a, _)| *a == Area::Reading)
                .map(|(_, v)| v["counters"]["counter"].as_u64().expect("counter"))
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl NorthboundPublisher for Broker {
        async fn publish(&self, dev_eui: &str, area: Area, payload: &[u8]) -> Result<()> {
            self.publish_confirmed(dev_eui, area, payload).await
        }
        async fn publish_confirmed(&self, _dev: &str, area: Area, payload: &[u8]) -> Result<()> {
            if !*self.up.borrow() {
                if !self.hang {
                    return Err(Error::new(
                        Kind::ServiceDown,
                        Transport::P2p,
                        Component::MqttBroker,
                        "the broker is down",
                    ));
                }
                let mut up = self.up.subscribe();
                let _ = up.wait_for(|u| *u).await;
            }
            let doc = serde_json::from_slice(payload).expect("json");
            self.acked.lock().expect("lock").push((area, doc));
            Ok(())
        }
        async fn tombstone(&self, _dev_eui: &str) -> Result<()> {
            Ok(())
        }
    }

    /// [`joined_ring`] publishing to `broker`, which is up for the join.
    async fn joined_ring_to(
        tag: &str,
        broker: &Broker,
    ) -> (Store, TmpStore, Central<Broker>, FakeModem) {
        let (store, t) = temp_store(tag);
        register(&store);
        let mut c = Central::new(store.clone(), NET_ID, radio(), broker.clone());
        let mut modem = FakeModem::new();
        modem.info.evt_ring_max = 16;
        c.init_modem(&mut modem).await.expect("init");
        c.on_event(&mut modem, rx(join_req(5), 1000))
            .await
            .expect("join");
        (store, t, c, modem)
    }

    async fn settle_to<P: NorthboundPublisher>(c: &mut Central<P>, modem: &mut FakeModem) {
        c.ack_if_due(modem, Instant::now() + LEARN_FOR).await;
    }

    /// Wait (up to 5 s) for the outbox to empty.
    async fn drained(outbox: &Outbox) {
        for _ in 0..500 {
            if outbox.pending() == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the outbox kept {} message(s)", outbox.pending());
    }

    /// A publish that fails loses nothing: the events are acked once their messages are in
    /// the outbox, and the messages go out, in order, once the broker is back.
    #[tokio::test]
    async fn a_failed_publish_loses_nothing_and_goes_out_when_the_broker_is_back() {
        let broker = Broker::new(true, false);
        let (_store, _t, mut c, mut modem) = joined_ring_to("ob-fail", &broker).await;
        broker.set_up(false);
        for seq in 1..=3u8 {
            c.on_event(&mut modem, telemetry(seq, u32::from(seq)))
                .await
                .expect("uplink");
        }
        settle_to(&mut c, &mut modem).await;
        assert_eq!(modem.acks(), vec![1, 2, 3], "stored, so acked");
        assert!(broker.readings().is_empty());
        assert_eq!(c.outbox().pending(), 3);
        assert_eq!(c.status().snapshot().outbox_pending, 3);
        broker.set_up(true);
        c.on_event(&mut modem, telemetry(4, 4)).await.expect("4");
        assert_eq!(broker.readings(), vec![1, 2, 3, 4], "in order, once each");
        assert_eq!(c.outbox().pending(), 0);
    }

    /// An uplink whose `sessions.db` save failed is not acked; the gateway's resend is
    /// processed afresh: one entry, one publish (§8.9). Since #130 both files are staged
    /// before either lands, so a `sessions.db` that cannot even be staged keeps the outbox
    /// entry out too. One that fails later, at its rename, leaves the entry in place and the
    /// resend appends it again under the same key (DevEUI and frame counter) — covered by
    /// `outbox::tests::the_same_event_appended_twice_is_published_once`.
    #[tokio::test]
    async fn an_event_stored_twice_after_a_failed_save_is_published_once() {
        use std::os::unix::fs::PermissionsExt;
        let broker = Broker::new(false, false);
        let (store, _t, mut c, mut modem) = joined_ring_to("ob-idem", &broker).await;
        let p2p = store.root().join("p2p");
        std::fs::create_dir_all(store.root().join(outbox::DIR)).expect("outbox dir");
        // `sessions.db` cannot be replaced; the outbox beside it still takes files.
        std::fs::set_permissions(&p2p, std::fs::Permissions::from_mode(0o500)).expect("chmod");
        c.on_event(&mut modem, telemetry(1, 1)).await.expect("1");
        std::fs::set_permissions(&p2p, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        assert!(modem.acks().is_empty(), "not saved, not acked");
        assert_eq!(c.outbox().pending(), 0, "nor in the outbox");
        c.on_event(&mut modem, telemetry(1, 1))
            .await
            .expect("resend");
        assert_eq!(modem.acks(), vec![1]);
        assert_eq!(c.outbox().pending(), 1, "the same entry");
        broker.set_up(true);
        c.outbox().drain_now(&broker).await;
        assert_eq!(broker.readings(), vec![1], "published once");
    }

    /// What the outbox held when the central stopped is published by the next one.
    #[tokio::test]
    async fn a_restart_publishes_what_the_outbox_held() {
        let broker = Broker::new(true, false);
        let (store, _t, mut c, mut modem) = joined_ring_to("ob-restart", &broker).await;
        broker.set_up(false);
        for seq in 1..=2u8 {
            c.on_event(&mut modem, telemetry(seq, u32::from(seq)))
                .await
                .expect("uplink");
        }
        settle_to(&mut c, &mut modem).await;
        assert_eq!(modem.acks(), vec![1, 2]);
        drop(c);
        broker.set_up(true);
        let c = Central::new(store, NET_ID, radio(), broker.clone());
        assert_eq!(c.outbox().pending(), 2, "reloaded from p2p/outbox/");
        c.outbox().drain_now(&broker).await;
        assert_eq!(broker.readings(), vec![1, 2]);
        assert_eq!(c.outbox().pending(), 0);
    }

    /// T8 (c67): the broker is away for a while during a stream of uplinks. Each event is
    /// acked as soon as its messages are in the outbox, not when the broker has them; the
    /// outbox drains once it is back; every message is published exactly once, in order.
    /// Both ways a broker can be away: the publish refused at once, or held until it is back.
    #[tokio::test]
    async fn a_broker_away_for_a_while_gets_every_message_exactly_once() {
        for hang in [false, true] {
            let broker = Broker::new(true, hang);
            let tag = format!("ob-t8-{hang}");
            let (store, _t, c, mut modem) = joined_ring_to(&tag, &broker).await;
            let outbox = std::sync::Arc::new(Outbox::open(&store).with_backoff(
                std::time::Duration::from_millis(5),
                std::time::Duration::from_millis(40),
            ));
            let mut c = c.with_outbox(outbox.clone());
            let drainer = tokio::spawn(outbox.clone().run_drainer(broker.clone()));
            tokio::task::yield_now().await;
            for seq in 1..=30u8 {
                if seq == 6 {
                    broker.set_up(false);
                }
                if seq == 21 {
                    broker.set_up(true);
                }
                c.on_event(&mut modem, telemetry(seq, u32::from(seq)))
                    .await
                    .expect("uplink");
                if seq == 1 {
                    settle_to(&mut c, &mut modem).await;
                }
                // The ack does not wait for the broker: it is due on the pacing alone.
                c.ack_if_due(&mut modem, Instant::now() + ACK_WITHIN).await;
                assert_eq!(modem.acks().last(), Some(&seq), "hang={hang}: {seq} acked");
                if (6..21).contains(&seq) {
                    assert!(
                        outbox.pending() > 0,
                        "hang={hang}: {seq} waits in the outbox"
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            drained(&outbox).await;
            assert_eq!(
                broker.readings(),
                (1..=30).collect::<Vec<u64>>(),
                "hang={hang}: each once, in order"
            );
            drainer.abort();
        }
    }

    /// A full outbox stops the acks: the event that does not fit is not processed, the
    /// gateway keeps it, and its resend goes through once the broker has made room.
    #[tokio::test]
    async fn a_full_outbox_stops_the_acks_and_drops_nothing() {
        let broker = Broker::new(true, false);
        let (store, _t, c, mut modem) = joined_ring_to("ob-full", &broker).await;
        // Room for one event of four messages after the first two readings: none.
        let outbox = std::sync::Arc::new(Outbox::open_dir(store.root().join(outbox::DIR), 5));
        let mut c = c.with_outbox(outbox.clone());
        broker.set_up(false);
        for seq in 1..=3u8 {
            c.on_event(&mut modem, telemetry(seq, u32::from(seq)))
                .await
                .expect("uplink");
        }
        settle_to(&mut c, &mut modem).await;
        assert_eq!(modem.acks(), vec![1, 2], "3 did not fit: not acked");
        assert_eq!((outbox.pending(), outbox.overflow()), (2, 1));
        let st = c.status().snapshot();
        assert_eq!((st.outbox_pending, st.outbox_overflow), (2, 1));
        c.on_event(&mut modem, telemetry(3, 3))
            .await
            .expect("resend");
        assert_eq!(outbox.overflow(), 2, "still full");
        broker.set_up(true);
        outbox.drain_now(&broker).await;
        c.on_event(&mut modem, telemetry(3, 3))
            .await
            .expect("resend");
        assert_eq!(modem.acks(), vec![1, 2, 3]);
        assert_eq!(broker.readings(), vec![1, 2, 3]);
    }

    /// A gap relearn's skipped seqs reach the northbridge document.
    #[tokio::test]
    async fn the_seqs_a_relearn_skips_are_in_the_status_document() {
        let (_store, _t, mut c, mut modem) = joined_ring("ob-gap").await;
        c.on_event(&mut modem, telemetry(1, 1)).await.expect("1");
        settle(&mut c, &mut modem).await;
        // 2..=4 are gone from the gateway; 5 keeps coming for longer than the gap allows.
        let t = Instant::now();
        assert_eq!(c.delivery.admit(5, t), Admit::Gap);
        c.on_event(&mut modem, telemetry(5, 5)).await.expect("5");
        assert!(c.delivery.last().is_some(), "not 2 s yet");
        c.delivery
            .admit(5, t + crate::p2p::tower::delivery::GAP_RELEARN_AFTER);
        c.delivery
            .done(5, t + crate::p2p::tower::delivery::GAP_RELEARN_AFTER);
        c.ack_if_due(
            &mut modem,
            t + crate::p2p::tower::delivery::GAP_RELEARN_AFTER + LEARN_FOR,
        )
        .await;
        let doc = c.status().document(now_unix_i(), true);
        assert_eq!(doc["counters"]["evt_relearn"], 1);
        assert_eq!(doc["counters"]["evt_seq_gap"], 3);
        assert_eq!(doc["counters"]["evt_seq_gap_seen"], 0);
    }

    // ---- HIL c66 + Northbridge 0.3.1 (§8.7) ----

    fn boot_event() -> Event {
        Event::Boot(BootEvent {
            fw_ver: [0, 3, 1],
            git_sha: [0; 4],
            reset_cause: 1,
        })
    }

    /// A: after `EVT_BOOT` the ring is known to start at seq 1. Seq 1 is acked once stored
    /// (C), not after a 1 s learning window, and each resend of it is acked again as
    /// promptly — the gateway stops resending.
    #[tokio::test]
    async fn after_evt_boot_seq_1_and_its_resends_are_acked_within_100_ms() {
        let (_store, _t, mut c, mut modem) = joined_ring("hil-boot").await;
        assert_eq!(
            c.delivery.last(),
            None,
            "learning: the gateway was found running"
        );
        c.on_event(&mut modem, boot_event()).await.expect("boot");
        c.on_event(&mut modem, telemetry(1, 1)).await.expect("1");
        assert_eq!(modem.acks(), vec![1], "acked once stored, no learning");
        for resend in 1..=3u32 {
            c.on_event(&mut modem, telemetry(1, 1))
                .await
                .expect("resend");
            assert!(
                c.ack_deadline().is_some(),
                "{resend}: the resend is owed an ack"
            );
            c.ack_if_due(&mut modem, Instant::now() + ACK_WITHIN).await;
            assert_eq!(
                modem.acks().len(),
                1 + resend as usize,
                "{resend}: re-acked"
            );
        }
        assert_eq!(readings(&c), 1, "processed once");
    }

    /// A: a central that starts with the gateway in `boot` — its `EVT_BOOT` lost, or seen by
    /// the central before a restart — needs no learning either.
    #[tokio::test]
    async fn a_central_that_finds_the_gateway_in_boot_does_not_learn() {
        let (store, _t) = temp_store("hil-fresh-boot");
        register(&store);
        let mut c = central(store.clone());
        let mut modem = FakeModem::new();
        modem.info.evt_ring_max = 16;
        modem.info.state = crate::p2p::modem_io::gateway_state::BOOT;
        c.init_modem(&mut modem).await.expect("init");
        assert_eq!(c.delivery.last(), Some(0));
        c.on_event(&mut modem, rx(join_req(5), 1000))
            .await
            .expect("join");
        c.on_event(&mut modem, telemetry(1, 1)).await.expect("1");
        c.ack_if_due(&mut modem, Instant::now() + ACK_WITHIN).await;
        assert_eq!(modem.acks(), vec![1]);
    }

    /// B: Northbridge 0.3.0 (`evt_ring_max` 0) refuses `KEEP_NEWER`; a restore to it sends
    /// `HOME` alone. One with a ring gets `HOME | KEEP_NEWER`.
    #[tokio::test]
    async fn keep_newer_goes_only_to_a_gateway_with_a_ring() {
        for (ring, flags) in [
            (0u8, link::NODE_HOME),
            (16, link::NODE_HOME | link::NODE_KEEP_NEWER),
        ] {
            let (store, _t) = temp_store(&format!("hil-keep-newer-{ring}"));
            register(&store);
            let mut c = central(store.clone());
            let mut modem = FakeModem::new();
            modem.info.evt_ring_max = ring;
            c.init_modem(&mut modem).await.expect("init");
            c.on_event(&mut modem, rx(join_req(5), 1000))
                .await
                .expect("join");
            modem.sent.clear();
            c.on_event(&mut modem, boot_event()).await.expect("boot");
            let adds: Vec<u8> = modem
                .twr_sent()
                .iter()
                .filter_map(|c| match c {
                    TwrCmd::NodeAdd { flags, .. } => Some(*flags),
                    _ => None,
                })
                .collect();
            assert_eq!(adds, vec![flags], "evt_ring_max {ring}");
        }
    }

    // --- #470 §13.5: answers in the gateway's ACK, TWR_TIME_SYNC (p2p_tower_gateway.md
    // §10.4, §13.5) ---

    /// A gateway on the current image: `GET_INFO` 180 B, a ring and its head.
    fn answering_gateway(modem: &mut FakeModem) {
        modem.info.evt_ring_max = 16;
        modem.info.evt_head_seq = Some(1);
        modem.info.ack_ctrl = Some(crate::p2p::modem_io::AckCtrlCounters {
            sent: 4,
            time_no_anchor: 1,
        });
    }

    /// Every `TWR_TIME_SYNC` sent, in order.
    fn time_syncs(modem: &FakeModem) -> Vec<(u64, u64)> {
        modem
            .twr_sent()
            .iter()
            .filter_map(|c| match c {
                TwrCmd::TimeSync { now_ms, unix_ms } => Some((*now_ms, *unix_ms)),
                _ => None,
            })
            .collect()
    }

    /// The command right after each `GET_INFO`.
    fn after_get_info(modem: &FakeModem) -> Vec<Option<SentCmd>> {
        modem
            .sent
            .iter()
            .enumerate()
            .filter(|(_, c)| **c == SentCmd::GetInfo)
            .map(|(i, _)| modem.sent.get(i + 1).cloned())
            .collect()
    }

    fn is_time_sync(c: &Option<SentCmd>) -> bool {
        matches!(c, Some(SentCmd::Twr(TwrCmd::TimeSync { .. })))
    }

    async fn joined_answering(
        tag: &str,
    ) -> (Store, TmpStore, Central<RecordingPublisher>, FakeModem) {
        let (store, t) = temp_store(tag);
        register(&store);
        let mut c = central(store.clone());
        let mut modem = FakeModem::new();
        answering_gateway(&mut modem);
        // Without the event ring, so the plain `uplink` helper's events are taken as they come
        // (the ring's ordering is not what this is about).
        modem.info.evt_ring_max = 0;
        modem.info.evt_head_seq = None;
        c.init_modem(&mut modem).await.expect("init");
        c.on_event(&mut modem, rx(join_req(5), 1000))
            .await
            .expect("join");
        (store, t, c, modem)
    }

    /// `now_ms` is the gateway's clock at the host's `unix_ms` through the central's anchor;
    /// nothing without an anchor or with an unset host clock.
    #[test]
    fn time_sync_values_come_from_the_clock_anchor() {
        let (store, _t) = temp_store("ts-values");
        let mut c = central(store);
        assert_eq!(c.time_sync_values(1_800_000_000_000), None, "no anchor");
        c.clock_anchor = Some((5_000, 1_800_000_000_000));
        assert_eq!(
            c.time_sync_values(1_800_000_000_250),
            Some((5_250, 1_800_000_000_250))
        );
        assert_eq!(
            c.time_sync_values(1_800_000_000_000),
            Some((5_000, 1_800_000_000_000)),
            "at the anchor itself"
        );
        // A host clock stepped back: the gateway's clock does not run backwards.
        assert_eq!(
            c.time_sync_values(1_799_999_999_000),
            Some((5_000, 1_799_999_999_000))
        );
        // An unset host clock (before 2023-11): none, better than a wrong one.
        c.clock_anchor = Some((5_000, 60_000));
        assert_eq!(c.time_sync_values(61_000), None);
    }

    /// §7.2 / §7.3 / §13.1: with a 180 B gateway every restore sends `TWR_TIME_SYNC` right after
    /// its `GET_INFO` — startup, `EVT_BOOT`, a gateway left running, a missed reboot — and so
    /// does the reconcile probe. The values are the anchor just taken.
    #[tokio::test]
    async fn time_sync_follows_every_get_info_of_the_restore_sequences() {
        let (store, _t) = temp_store("ts-restore");
        register(&store);
        let mut c = central(store.clone());
        let mut modem = FakeModem::new();
        answering_gateway(&mut modem);
        modem.info.now_ms = 77_000;
        let before = now_unix_ms();
        c.init_modem(&mut modem).await.expect("init");
        let after = now_unix_ms();
        // Startup, gateway in boot: GET_INFO → TWR_TIME_SYNC → SET_RADIO_CONFIG.
        assert_eq!(modem.sent[0], SentCmd::GetInfo);
        assert!(
            is_time_sync(&modem.sent.get(1).cloned()),
            "{:?}",
            modem.sent
        );
        assert_eq!(modem.sent[2], SentCmd::SetRadioConfig(radio()));
        let (now_ms, unix_ms) = time_syncs(&modem)[0];
        assert!(
            (before..=after).contains(&unix_ms),
            "unix_ms is the host clock"
        );
        assert!(
            (77_000..=77_000 + (after - before)).contains(&now_ms),
            "now_ms {now_ms} is the gateway's clock through the anchor"
        );
        let (anchor_nb, anchor_host) = c.clock_anchor.expect("anchored");
        assert_eq!(
            now_ms - anchor_nb,
            unix_ms - anchor_host,
            "the anchor's mapping"
        );

        // EVT_BOOT: the same order.
        modem.sent.clear();
        modem.info.now_ms = 900;
        c.on_event(&mut modem, boot_event()).await.expect("boot");
        assert_eq!(modem.sent[0], SentCmd::GetInfo);
        assert!(
            is_time_sync(&modem.sent.get(1).cloned()),
            "{:?}",
            modem.sent
        );
        assert_eq!(modem.sent[2], SentCmd::SetRadioConfig(radio()));

        // A central restart finding the gateway running: GET_INFO → TWR_TIME_SYNC → the sweep.
        modem.sent.clear();
        modem.info.state = crate::p2p::modem_io::gateway_state::GATEWAY;
        modem.info.radio = radio();
        modem.info.net_id = NET_ID;
        modem.info.turnaround_ms = link::TURNAROUND_MS;
        modem.info.dl_gap_ms = link::DL_GAP_MS;
        modem.info.now_ms = 5_000;
        let mut fresh = central(store.clone());
        fresh.init_modem(&mut modem).await.expect("restart");
        assert_eq!(modem.sent[0], SentCmd::GetInfo);
        assert!(
            is_time_sync(&modem.sent.get(1).cloned()),
            "{:?}",
            modem.sent
        );
        assert_eq!(modem.sent[2], SentCmd::Twr(TwrCmd::NodeList { start: 0 }));

        // The reconcile probe re-anchors, and the gateway's anchor follows.
        modem.sent.clear();
        fresh.last_info_at = None;
        modem.info.now_ms = 65_000;
        fresh.probe_if_idle(&mut modem).await.expect("probe");
        assert_eq!(after_get_info(&modem).len(), 1);
        assert!(
            after_get_info(&modem).iter().all(is_time_sync),
            "{:?}",
            modem.sent
        );

        // A missed reboot (the clock went back): restored, TWR_TIME_SYNC after each GET_INFO.
        modem.sent.clear();
        fresh.last_info_at = None;
        modem.info.now_ms = 1_200;
        modem.info.state = crate::p2p::modem_io::gateway_state::BOOT;
        fresh.probe_if_idle(&mut modem).await.expect("probe");
        let next = after_get_info(&modem);
        assert_eq!(next.len(), 2, "the probe and the restore: {:?}", modem.sent);
        assert!(
            is_time_sync(&next[1]),
            "the restore's GET_INFO: {:?}",
            modem.sent
        );
        assert_eq!(time_syncs(&modem).last().map(|t| t.0 >= 1_200), Some(true));
        let st = fresh.nb.snapshot();
        assert_eq!((st.time_sync_sent, st.time_sync_failed), (3, 0));
    }

    /// The 10 min floor (§13.5): due 10 min after the last try, not before, and sent through a
    /// fresh `GET_INFO` — an old anchor has drifted, and the gateway refuses a `now_ms` ahead
    /// of its clock.
    #[tokio::test]
    async fn time_sync_is_sent_at_least_every_ten_minutes() {
        let (store, _t) = temp_store("ts-timer");
        let mut c = central(store);
        let mut modem = FakeModem::new();
        answering_gateway(&mut modem);
        c.init_modem(&mut modem).await.expect("init");
        let t0 = c.time_sync_at.expect("sent at init");
        modem.sent.clear();
        c.time_sync_if_due(&mut modem, t0 + std::time::Duration::from_secs(599))
            .await
            .expect("tick");
        assert!(modem.sent.is_empty(), "not due yet: {:?}", modem.sent);
        modem.info.now_ms += 600_000;
        c.time_sync_if_due(&mut modem, t0 + TIME_SYNC_EVERY)
            .await
            .expect("tick");
        assert_eq!(modem.sent[0], SentCmd::GetInfo, "re-anchored first");
        assert!(
            is_time_sync(&modem.sent.get(1).cloned()),
            "{:?}",
            modem.sent
        );
        assert_eq!(time_syncs(&modem)[0].0 / 1000, modem.info.now_ms / 1000);
        let t1 = c.time_sync_at.expect("restarted");
        assert!(t1 > t0);
        c.time_sync_if_due(&mut modem, t1 + std::time::Duration::from_secs(1))
            .await
            .expect("tick");
        assert_eq!(time_syncs(&modem).len(), 1, "the timer restarted");
    }

    /// Mark an uplink event's ACK tail (`ack` bits 6 and 7).
    fn with_tail(ev: Event, ack: UplinkAck, sent: bool, time: bool) -> Event {
        match ev {
            Event::TwrUplink { seq, mut up } => {
                up.ack = ack;
                up.ack_tail = link::AckTail { sent, time };
                Event::TwrUplink { seq, up }
            }
            _ => unreachable!(),
        }
    }

    /// §13.5: a `TimeReq` whose ACK went out without a `TimeAns` (bit 6 clear) shows the
    /// gateway holds no anchor — counted, and re-synced after a minute rather than at the
    /// 10 min floor. A `TimeAns` in the tail, or no ACK at all, says nothing of the anchor.
    #[tokio::test]
    async fn an_ack_without_a_time_answer_brings_the_time_sync_forward() {
        let (_store, _t, mut c, mut modem) = joined_answering("ack-no-anchor").await;
        c.tx_in_flight = None;
        let time_req = || control::encode_control(&[(control::cmd::TIME, &[])]).expect("tlv");
        // Answered: the tail holds a TimeAns.
        c.on_event(
            &mut modem,
            with_tail(uplink(2, time_req()), UplinkAck::Sent, true, true),
        )
        .await
        .expect("uplink");
        // No ACK went out in the Node's window: nothing known about the anchor.
        c.on_event(
            &mut modem,
            with_tail(uplink(3, time_req()), UplinkAck::Late, false, false),
        )
        .await
        .expect("uplink");
        assert_eq!(c.nb.snapshot().ack_time_missed, 0);
        assert!(!c.time_sync_wanted);

        let t0 = c.time_sync_at.expect("synced at init");
        // An ACK without a TimeAns: the gateway lost its anchor.
        c.on_event(
            &mut modem,
            with_tail(uplink(4, time_req()), UplinkAck::SentPending, true, false),
        )
        .await
        .expect("uplink");
        assert_eq!(c.nb.snapshot().ack_time_missed, 1);
        assert!(c.time_sync_wanted);
        assert!(
            pushes(&modem).is_empty(),
            "still no 0x91 TimeAns from the central"
        );
        modem.sent.clear();
        c.time_sync_if_due(&mut modem, t0 + std::time::Duration::from_secs(59))
            .await
            .expect("tick");
        assert!(modem.sent.is_empty(), "rate-limited: {:?}", modem.sent);
        c.time_sync_if_due(&mut modem, t0 + TIME_SYNC_RETRY)
            .await
            .expect("tick");
        assert_eq!(modem.sent[0], SentCmd::GetInfo);
        assert_eq!(time_syncs(&modem).len(), 1, "{:?}", modem.sent);
        assert!(!c.time_sync_wanted);
        let t1 = c.time_sync_at.expect("synced");
        c.time_sync_if_due(&mut modem, t1 + TIME_SYNC_RETRY)
            .await
            .expect("tick");
        assert_eq!(time_syncs(&modem).len(), 1, "back to the 10 min floor");
    }

    /// A 172 B (or 171 B) gateway does not know `0x19`: it is never sent one, neither in a
    /// restore, nor by the probe, nor by the timer.
    #[tokio::test]
    async fn a_172_byte_gateway_gets_no_time_sync() {
        let (store, _t) = temp_store("ts-172");
        let mut c = central(store);
        let mut modem = FakeModem::new();
        modem.info.evt_ring_max = 16;
        modem.info.evt_head_seq = Some(1);
        c.init_modem(&mut modem).await.expect("init");
        c.on_event(&mut modem, boot_event()).await.expect("boot");
        c.last_info_at = None;
        c.probe_if_idle(&mut modem).await.expect("probe");
        let n = modem.sent.len();
        c.time_sync_if_due(&mut modem, Instant::now())
            .await
            .expect("tick");
        assert_eq!(
            modem.sent.len(),
            n,
            "the timer does not probe a 172 B gateway"
        );
        assert!(time_syncs(&modem).is_empty(), "{:?}", modem.sent);
        assert!(!c.answers_in_ack);
    }

    /// A refused `TWR_TIME_SYNC` is counted and logged; the restore goes on.
    #[tokio::test]
    async fn a_refused_time_sync_does_not_stop_the_restore() {
        let (store, _t) = temp_store("ts-refused");
        let mut c = central(store);
        let mut modem = FakeModem::new();
        answering_gateway(&mut modem);
        modem
            .refuse_twr
            .push((link::cmd::TWR_TIME_SYNC, Status::Unsupported));
        c.init_modem(&mut modem).await.expect("init goes on");
        assert!(modem.sent.contains(&start()), "{:?}", modem.sent);
        let st = c.nb.snapshot();
        assert_eq!((st.time_sync_sent, st.time_sync_failed), (0, 1));
        let doc = c.nb.document(now_unix_i(), true);
        assert_eq!(doc["counters"]["ack_ctrl_sent"], 4);
        assert_eq!(doc["counters"]["ack_time_no_anchor"], 1);
    }

    /// §10.4: a 180 B gateway answers `LinkCheckReq` / `TimeReq` in its ACK, so the central
    /// queues neither — the `Capabilities` answer still goes out as a `0x91` push. A gateway
    /// whose `GET_INFO` turns 172 B gets the P2 answers again.
    #[tokio::test]
    async fn a_180_byte_gateway_gets_no_queued_link_check_or_time_answer() {
        let (_store, _t, mut c, mut modem) = joined_answering("ack-answers").await;
        assert!(c.answers_in_ack);
        c.clock_anchor = Some((11_000, 1_799_999_999_250));
        let before = pushes(&modem).len();
        c.on_event(
            &mut modem,
            with_tail(
                uplink(
                    2,
                    control::encode_control(&[
                        (control::cmd::LINK_CHECK, &[]),
                        (control::cmd::TIME, &[]),
                    ])
                    .expect("tlv"),
                ),
                UplinkAck::Sent,
                true,
                true,
            ),
        )
        .await
        .expect("uplink");
        assert_eq!(pushes(&modem).len(), before, "nothing queued");
        assert_eq!(c.nb.snapshot().ack_time_missed, 0);
        assert!(c.answers.is_empty());
        assert_eq!(c.nb.snapshot().ack_ctrl_req, 1, "one per uplink");

        // Capabilities beside a LinkCheckReq: only the Capabilities answer is pushed.
        let node_caps = Capabilities {
            proto_version: 1,
            mtu: 96,
            profiles: control::PROFILE_LORA,
            cmd_bitmap: 1 << 0x10,
            power_class: 1,
        };
        c.on_event(
            &mut modem,
            uplink(
                3,
                control::encode_control(&[
                    (control::cmd::CAPABILITIES, &node_caps.encode()),
                    (control::cmd::LINK_CHECK, &[]),
                ])
                .expect("tlv"),
            ),
        )
        .await
        .expect("uplink");
        let (.., plaintext) = pushes(&modem).last().cloned().expect("pushed");
        assert_eq!(
            plaintext,
            control::encode_control(&[(
                control::cmd::CAPABILITIES,
                &Capabilities::central().encode()
            )])
            .expect("tlv")
        );
        assert_eq!(c.nb.snapshot().ack_ctrl_req, 2);
        // The join accept's `TX_SCHEDULE` long done: the gateway is idle and probed.
        c.tx_in_flight = None;

        // The gateway now reports 172 B (an older image): the central answers again.
        modem.info.ack_ctrl = None;
        c.last_info_at = None;
        c.probe_if_idle(&mut modem).await.expect("probe");
        assert!(!c.answers_in_ack);
        c.clock_anchor = Some((11_000, 1_799_999_999_250));
        c.on_event(
            &mut modem,
            uplink(
                4,
                control::encode_control(&[
                    (control::cmd::LINK_CHECK, &[]),
                    (control::cmd::TIME, &[]),
                ])
                .expect("tlv"),
            ),
        )
        .await
        .expect("uplink");
        let (.., plaintext) = pushes(&modem).last().cloned().expect("pushed");
        assert_eq!(
            plaintext,
            control::encode_control(&[
                (
                    control::cmd::LINK_CHECK,
                    &LinkCheckAns::from_reception(-58, 36, 10, 1).encode(),
                ),
                (
                    control::cmd::TIME,
                    &TimeAns::from_unix_ms(1_800_000_002_250, 4).encode(),
                ),
            ])
            .expect("tlv")
        );
        assert_eq!(
            c.nb.snapshot().ack_ctrl_req,
            2,
            "not counted in the fallback"
        );
    }

    // ---- P2P control commands (p2p_control_commands.md, sticker-firmware #487) ----

    /// A Node that names every #487 command in its Capabilities.
    fn caps_487() -> Capabilities {
        Capabilities {
            proto_version: 1,
            mtu: 96,
            profiles: control::PROFILE_LORA,
            cmd_bitmap: [
                control::cmd::CAPABILITIES,
                control::cmd::DETACH,
                control::cmd::REJOIN_REQ,
                control::cmd::BACKOFF,
                control::cmd::TIME,
                control::cmd::PENDING_INFO,
                control::cmd::DEV_STATUS,
            ]
            .iter()
            .fold(0u64, |m, b| m | (1u64 << b)),
            power_class: 1,
        }
    }

    fn set_caps(store: &Store, caps: Option<Capabilities>) {
        let mut db = SessionsDb::load(store).expect("load");
        db.sessions.get_mut(DEV_EUI).expect("session").capabilities = caps;
        db.save(store).expect("save");
    }

    fn enqueue_control(store: &Store, cmd: ControlDownlink) {
        let mut db = SessionsDb::load(store).expect("load");
        db.enqueue_downlink(DEV_EUI, crate::p2p::session::QueuedDownlink::control(cmd))
            .expect("live session");
        db.save(store).expect("save");
    }

    fn control_frame(cmds: &[ControlDownlink]) -> Vec<u8> {
        let mut v = vec![control::ENVELOPE_CONTROL];
        for c in cmds {
            v.extend(c.tlv());
        }
        v
    }

    /// §2: a control head goes out as `0x91 ‖ TLV`, confirmed, and retires on DELIVERED.
    #[tokio::test]
    async fn a_control_head_goes_out_as_0x91_and_retires_on_delivered() {
        let (store, _t, mut c, mut modem) = joined("ctl-head").await;
        set_caps(&store, Some(caps_487()));
        enqueue_control(&store, ControlDownlink::DevStatusReq);
        c.reconcile_gateway(&mut modem).await;
        let (item, ttl, flags, plaintext) = pushes(&modem).last().cloned().expect("pushed");
        assert_eq!(plaintext, vec![0x91, 0x30, 0x00]);
        assert_eq!(ttl, COMMAND_TTL_S);
        assert_eq!(flags, frame::flags::CONFIRMED);
        assert_eq!(session_now(&store).downlink_phase, DownlinkPhase::Announced);
        c.on_event(&mut modem, tx_event(item, link::TxOutcome::Delivered))
            .await
            .expect("tx");
        let s = session_now(&store);
        assert!(s.downlink_queue.is_empty(), "retired without an answer");
        assert_eq!(s.downlink_phase, DownlinkPhase::Idle);
    }

    /// H-3: an empty `0x91` uplink (the drain frame) is accepted and the queued head pushed.
    #[tokio::test]
    async fn an_empty_0x91_uplink_is_accepted_and_the_head_pushed() {
        let (store, _t, mut c, mut modem) = joined("ctl-empty").await;
        set_caps(&store, Some(caps_487()));
        enqueue_control(&store, ControlDownlink::Backoff { seconds: 120 });
        let before = pushes(&modem).len();
        let published = c.publisher.published().len();
        c.on_event(&mut modem, uplink(1, vec![control::ENVELOPE_CONTROL]))
            .await
            .expect("uplink");
        assert_eq!(session_now(&store).last_seen, 1, "counter stored");
        assert_eq!(
            c.publisher.published().len(),
            published,
            "nothing published"
        );
        let after = pushes(&modem);
        assert_eq!(after.len(), before + 1);
        assert_eq!(
            after.last().expect("push").3,
            vec![0x91, 0x15, 0x02, 120, 0]
        );
    }

    /// H-4 (§4): three control heads queued at once announce `more` 2, then 1, then none.
    #[tokio::test]
    async fn pending_info_counts_down_in_a_control_head() {
        let (store, _t, mut c, mut modem) = joined("ctl-pending").await;
        set_caps(&store, Some(caps_487()));
        let cmds = [
            ControlDownlink::Backoff { seconds: 120 },
            ControlDownlink::DevStatusReq,
            ControlDownlink::RejoinReq {
                kind: control::RejoinKind::Rekey,
            },
        ];
        for cmd in &cmds {
            enqueue_control(&store, *cmd);
        }
        c.reconcile_gateway(&mut modem).await;
        for (i, cmd) in cmds.iter().enumerate() {
            let (item, _, _, plaintext) = pushes(&modem).last().cloned().expect("pushed");
            let more = cmds.len() - 1 - i;
            let mut expected = vec![*cmd];
            if more > 0 {
                expected.push(ControlDownlink::pending_info(more));
            }
            assert_eq!(plaintext, control_frame(&expected), "head {i}");
            c.on_event(&mut modem, tx_event(item, link::TxOutcome::Delivered))
                .await
                .expect("tx");
        }
        assert!(session_now(&store).downlink_queue.is_empty());
    }

    /// §4: commands queued one after another re-push the announced head with the new count.
    #[tokio::test]
    async fn a_growing_queue_re_pushes_the_announced_head() {
        let (store, _t, mut c, mut modem) = joined("ctl-grow").await;
        set_caps(&store, Some(caps_487()));
        enqueue_control(&store, ControlDownlink::DevStatusReq);
        c.reconcile_gateway(&mut modem).await;
        let (item, _, _, first) = pushes(&modem).last().cloned().expect("pushed");
        assert_eq!(first, vec![0x91, 0x30, 0x00]);
        for (more, cmd) in [
            (1, ControlDownlink::Backoff { seconds: 0 }),
            (
                2,
                ControlDownlink::RejoinReq {
                    kind: control::RejoinKind::Rediscover,
                },
            ),
        ] {
            enqueue_control(&store, cmd);
            c.reconcile_gateway(&mut modem).await;
            let (again, _, _, plaintext) = pushes(&modem).last().cloned().expect("pushed");
            assert_eq!(again, item, "same item replaces it");
            assert_eq!(
                plaintext,
                control_frame(&[
                    ControlDownlink::DevStatusReq,
                    ControlDownlink::pending_info(more)
                ])
            );
        }
        // Nothing grew: no third push.
        let n = pushes(&modem).len();
        c.reconcile_gateway(&mut modem).await;
        assert_eq!(pushes(&modem).len(), n);
        assert_eq!(session_now(&store).downlink_phase, DownlinkPhase::Announced);
    }

    /// §4: behind a command head the count goes in a separate unconfirmed pending-info item.
    #[tokio::test]
    async fn pending_info_rides_its_own_item_behind_a_command_head() {
        let (store, _t, mut c, mut modem) = joined("ctl-pi-item").await;
        set_caps(&store, Some(caps_487()));
        enqueue_seq(&store, 11, vec![0x08, 0x0b]);
        enqueue_seq(&store, 12, vec![0x08, 0x0c]);
        enqueue_control(&store, ControlDownlink::DevStatusReq);
        c.reconcile_gateway(&mut modem).await;
        let p = pushes(&modem);
        let (head, _, head_flags, head_pt) = p[p.len() - 2].clone();
        let (pi, pi_ttl, pi_flags, pi_pt) = p[p.len() - 1].clone();
        assert_eq!(head_pt[0], control::ENVELOPE_APP);
        assert_eq!(head_flags, frame::flags::CONFIRMED);
        assert_eq!(pi, pending_info_item(DEV_EUI, head));
        assert_ne!(pi, head);
        assert_eq!((pi_ttl, pi_flags), (COMMAND_TTL_S, 0));
        assert_eq!(pi_pt, vec![0x91, 0x22, 0x01, 0x02]);
        // Its outcome touches nothing.
        c.on_event(&mut modem, tx_event(pi, link::TxOutcome::Delivered))
            .await
            .expect("tx");
        assert!(c.pending_info_items.is_empty());
        let s = session_now(&store);
        assert_eq!(s.downlink_queue.len(), 3);
        assert_eq!(s.downlink_phase, DownlinkPhase::Announced);
    }

    /// H-7: no PendingInfo for a Node that did not name `0x22`.
    #[tokio::test]
    async fn no_pending_info_without_the_capability() {
        let (store, _t, mut c, mut modem) = joined("ctl-pi-nocap").await;
        let mut caps = caps_487();
        caps.cmd_bitmap &= !(1u64 << control::cmd::PENDING_INFO);
        set_caps(&store, Some(caps));
        enqueue_control(&store, ControlDownlink::DevStatusReq);
        enqueue_control(&store, ControlDownlink::DevStatusReq);
        c.reconcile_gateway(&mut modem).await;
        assert_eq!(
            pushes(&modem).last().expect("pushed").3,
            vec![0x91, 0x30, 0x00]
        );
    }

    /// H-2 (§5): a DevStatusAns is published as the retained `device_status` document.
    #[tokio::test]
    async fn a_dev_status_answer_publishes_device_status() {
        let (store, _t, mut c, mut modem) = joined("ctl-devstatus").await;
        set_caps(&store, Some(caps_487()));
        let ans = DevStatusAns {
            battery_mv: 3012,
            dl_rssi: -71,
            dl_snr: 9,
            tx_power: 14,
            uptime_s: 86_400,
        };
        let up =
            control::encode_control(&[(control::cmd::DEV_STATUS, &ans.encode())]).expect("tlv");
        c.on_event(&mut modem, uplink(1, up)).await.expect("uplink");
        let docs = areas(&c.publisher, Area::DeviceStatus);
        assert_eq!(docs.len(), 1);
        let d = &docs[0];
        assert_eq!(d["dev_eui"], DEV_EUI);
        assert_eq!(d["transport"], "p2p");
        assert_eq!(d["battery_mv"], 3012);
        assert_eq!(d["battery_pct"], 51);
        assert_eq!(d["dl_rssi_dbm"], -71);
        assert_eq!(d["dl_snr_db"], 9);
        assert_eq!(d["tx_power_dbm"], 14);
        assert_eq!(d["uptime_s"], 86_400);
        assert!(Area::DeviceStatus.retained());
    }

    /// What `node-remove` leaves for a Node with the Detach bit (dispatch.rs, §3).
    fn remove_detaching(store: &Store, since_unix: u64) {
        let mut db = SessionsDb::load(store).expect("load");
        let _ = db.remove_node(DEV_EUI);
        let s = db.sessions.get_mut(DEV_EUI).expect("session kept");
        s.downlink_queue.clear();
        s.downlink_phase = DownlinkPhase::Idle;
        s.downlink_queue
            .push_back(crate::p2p::session::QueuedDownlink::control(
                ControlDownlink::Detach {
                    reason: control::DETACH_REASON_REMOVED,
                },
            ));
        s.detach = Some(crate::p2p::session::Detaching {
            since: "2026-10-12T00:00:00Z".into(),
            since_unix,
        });
        db.save(store).expect("save");
    }

    /// H-1 (§3): a detaching session stays on the gateway, pushes its Detach, publishes
    /// nothing, and leaves the gateway once the Detach is delivered.
    #[tokio::test]
    async fn a_detaching_session_pushes_detach_and_leaves_on_delivered() {
        let (store, _t, mut c, mut modem) = joined("ctl-detach").await;
        set_caps(&store, Some(caps_487()));
        remove_detaching(&store, 1);
        c.reconcile_gateway(&mut modem).await;
        assert!(c.gw_table.contains_key(&ADDR), "still on the gateway");
        assert_eq!(count_twr(&modem, link::cmd::TWR_NODE_REMOVE), 0);
        let (item, _, flags, plaintext) = pushes(&modem).last().cloned().expect("pushed");
        assert_eq!(plaintext, vec![0x91, 0x03, 0x01, 0x01]);
        assert_eq!(flags, frame::flags::CONFIRMED);

        let published = c.publisher.published().len();
        c.on_event(
            &mut modem,
            uplink(1, app(port::TELEMETRY, &unhex(GOLDEN_TELEMETRY))),
        )
        .await
        .expect("uplink");
        assert_eq!(
            c.publisher.published().len(),
            published,
            "a removed Node publishes nothing"
        );
        assert_eq!(session_now(&store).last_seen, 1);

        c.on_event(&mut modem, tx_event(item, link::TxOutcome::Delivered))
            .await
            .expect("tx");
        assert!(SessionsDb::load(&store).expect("load").sessions.is_empty());
        assert_eq!(count_twr(&modem, link::cmd::TWR_NODE_REMOVE), 1);
        c.reconcile_gateway(&mut modem).await;
        assert!(!c.gw_table.contains_key(&ADDR));
    }

    /// §3: a Detach never delivered gives up after `DETACH_MAX_S`.
    #[tokio::test]
    async fn an_undelivered_detach_expires() {
        let (store, _t, c, _modem) = joined("ctl-detach-exp").await;
        set_caps(&store, Some(caps_487()));
        remove_detaching(&store, 1000);
        let max = crate::p2p::session::DETACH_MAX_S;
        c.retire_expired_detaches(1000 + max).expect("retire");
        assert!(!SessionsDb::load(&store).expect("load").sessions.is_empty());
        c.retire_expired_detaches(1001 + max).expect("retire");
        assert!(SessionsDb::load(&store).expect("load").sessions.is_empty());
    }

    // --- several gateways (`p2p_cluster_gateway.md` §4, §5) ---

    const GW2: &str = "proximos-2200ad33";

    fn uplink_heard(counter: u32, rssi_dbm: i16, plaintext: Vec<u8>) -> Event {
        let mut ev = uplink(counter, plaintext);
        if let Event::TwrUplink { up, .. } = &mut ev {
            up.rssi_dbm = rssi_dbm;
        }
        ev
    }

    fn node_adds(modem: &FakeModem) -> Vec<u8> {
        modem
            .twr_sent()
            .into_iter()
            .filter_map(|c| match c {
                TwrCmd::NodeAdd { addr, flags, .. } if *addr == ADDR => Some(*flags),
                _ => None,
            })
            .collect()
    }

    /// Joined through the local gateway, then a second one online and initialised.
    async fn two_gateways(
        tag: &str,
    ) -> (
        Store,
        TmpStore,
        Central<RecordingPublisher>,
        BTreeMap<String, FakeModem>,
    ) {
        let (store, t, mut c, local) = joined(tag).await;
        let mut gw2 = FakeModem::new();
        c.add_gateway(GW2);
        assert!(c.activate(GW2));
        c.init_modem(&mut gw2).await.expect("init gw2");
        let modems = BTreeMap::from([(LOCAL.to_string(), local), (GW2.to_string(), gw2)]);
        (store, t, c, modems)
    }

    /// Every gateway holds every peer; only the home has it with `HOME` (§1).
    #[tokio::test]
    async fn a_second_gateway_holds_the_peer_without_home() {
        let (store, _t, c, modems) = two_gateways("gw-second").await;
        assert_eq!(
            session_now(&store).home,
            None,
            "joined through the local one"
        );
        assert_eq!(node_adds(&modems[LOCAL]), vec![link::NODE_HOME]);
        assert_eq!(node_adds(&modems[GW2]), vec![0]);
        assert_eq!(
            count_twr(&modems[GW2], link::cmd::TWR_CTR_BLOCK),
            1,
            "its own block"
        );
        assert_eq!(
            c.gateway_names(),
            BTreeSet::from([LOCAL.to_string(), GW2.to_string()])
        );
    }

    /// A downlink goes out through the home only.
    #[tokio::test]
    async fn heads_are_pushed_through_the_home_only() {
        let (store, _t, mut c, mut modems) = two_gateways("gw-heads").await;
        enqueue_seq(&store, 7, vec![0x08, 0x07]);
        c.converge(&mut modems).await;
        assert_eq!(pushes(&modems[LOCAL]).len(), 1);
        assert!(pushes(&modems[GW2]).is_empty());
    }

    /// The answers of an uplink that came first through another gateway wait for the home.
    #[tokio::test]
    async fn answers_of_a_copy_through_another_gateway_go_out_through_the_home() {
        let (_store, _t, mut c, mut modems) = two_gateways("gw-answers").await;
        let req = control::encode_control(&[(control::cmd::LINK_CHECK, &[])]).expect("tlv");
        assert!(c.activate(GW2));
        let gw2 = modems.get_mut(GW2).expect("gw2");
        c.on_event(gw2, uplink(1, req)).await.expect("uplink");
        assert!(pushes(&modems[GW2]).is_empty(), "not the home");
        assert!(c.converge_due());
        c.converge(&mut modems).await;
        assert_eq!(
            pushes(&modems[LOCAL]).len(),
            1,
            "the answer, through the home"
        );
        assert!(pushes(&modems[GW2]).is_empty());
        assert!(c.answers_due.is_empty());
    }

    /// Two gateways that answer in their ACK, joined through the local one (its home).
    async fn two_answering_gateways(
        tag: &str,
    ) -> (
        Store,
        TmpStore,
        Central<RecordingPublisher>,
        BTreeMap<String, FakeModem>,
    ) {
        let (store, t, mut c, local) = joined_answering(tag).await;
        let mut gw2 = FakeModem::new();
        answering_gateway(&mut gw2);
        gw2.info.evt_ring_max = 0;
        gw2.info.evt_head_seq = None;
        c.add_gateway(GW2);
        assert!(c.activate(GW2));
        c.init_modem(&mut gw2).await.expect("init gw2");
        let modems = BTreeMap::from([(LOCAL.to_string(), local), (GW2.to_string(), gw2)]);
        (store, t, c, modems)
    }

    /// The home's copy that comes after another gateway's tells what the home's ACK did
    /// (HC 2026-10-12): the first copy, through a gateway that does not ACK, says nothing of
    /// it, and the home's later copy is taken for it once.
    #[tokio::test]
    async fn the_homes_later_copy_tells_what_its_ack_did() {
        let (_store, _t, mut c, mut modems) = two_answering_gateways("gw-ack-home").await;
        let time_req = || control::encode_control(&[(control::cmd::TIME, &[])]).expect("tlv");
        assert!(c.activate(GW2));
        let gw2 = modems.get_mut(GW2).expect("gw2");
        c.on_event(
            gw2,
            with_tail(uplink(2, time_req()), UplinkAck::NotConfirmed, false, false),
        )
        .await
        .expect("first copy");
        assert_eq!(c.nb.snapshot().ack_ctrl_req, 0, "not the gateway that ACKs");
        assert!(
            !c.converge_due(),
            "the home answers in its ACK: nothing left to push"
        );
        assert!(c.answers_due.is_empty());

        assert!(c.activate(LOCAL));
        let local = modems.get_mut(LOCAL).expect("local");
        c.on_event(
            local,
            with_tail(uplink(2, time_req()), UplinkAck::Sent, true, false),
        )
        .await
        .expect("home copy");
        assert_eq!(c.nb.snapshot().ack_ctrl_req, 1);
        assert_eq!(
            c.nb.snapshot().ack_time_missed,
            1,
            "ACKed without a TimeAns"
        );
        assert!(c.time_sync_wanted);

        // Once per counter.
        c.on_event(
            local,
            with_tail(uplink(2, time_req()), UplinkAck::Sent, true, false),
        )
        .await
        .expect("again");
        assert_eq!(c.nb.snapshot().ack_ctrl_req, 1);
        assert!(pushes(&modems[LOCAL]).is_empty() && pushes(&modems[GW2]).is_empty());
        assert!(c.activate(GW2));
        assert_eq!(c.nb.snapshot().ack_ctrl_req, 0);
        assert_eq!(c.nb.snapshot().ack_time_missed, 0);
    }

    /// A gateway out of TX counters gets a block whichever copy says so, a later one included.
    #[tokio::test]
    async fn a_later_copy_without_a_counter_gets_its_gateway_a_block() {
        let (_store, _t, mut c, mut modems) = two_gateways("gw-ack-noctr").await;
        assert!(c.activate(GW2));
        let gw2 = modems.get_mut(GW2).expect("gw2");
        c.on_event(gw2, uplink(1, app(port::TELEMETRY, &[1])))
            .await
            .expect("first copy");
        let blocks = count_twr(&modems[LOCAL], link::cmd::TWR_CTR_BLOCK);
        assert!(c.activate(LOCAL));
        let local = modems.get_mut(LOCAL).expect("local");
        c.on_event(
            local,
            with_tail(
                uplink(1, app(port::TELEMETRY, &[1])),
                UplinkAck::NoCounter,
                false,
                false,
            ),
        )
        .await
        .expect("home copy");
        assert_eq!(
            count_twr(&modems[LOCAL], link::cmd::TWR_CTR_BLOCK),
            blocks + 1
        );
    }

    /// Heard 6 dB better on three counters, another gateway becomes the home: `HOME` comes off
    /// the old one before the new one gets it, after a fresh counter block (§5.2, §5.3).
    #[tokio::test]
    async fn a_home_moves_to_the_gateway_that_hears_the_node_better() {
        let (store, _t, mut c, mut modems) = two_gateways("gw-move").await;
        for counter in 1..=4 {
            for (gw, rssi) in [(GW2, -50), (LOCAL, -80)] {
                assert!(c.activate(gw));
                let modem = modems.get_mut(gw).expect("modem");
                c.on_event(
                    modem,
                    uplink_heard(counter, rssi, app(port::TELEMETRY, &[1])),
                )
                .await
                .expect("uplink");
            }
            c.review_homes();
        }
        assert_eq!(session_now(&store).home.as_deref(), Some(GW2));
        assert!(c.converge_due());
        let blocks_before = count_twr(&modems[GW2], link::cmd::TWR_CTR_BLOCK);
        c.converge(&mut modems).await;

        let local_adds = node_adds(&modems[LOCAL]);
        assert_eq!(local_adds.last(), Some(&0), "demoted");
        let gw2_sent = modems[GW2].twr_sent();
        let block_at = gw2_sent
            .iter()
            .rposition(|t| t.cmd() == link::cmd::TWR_CTR_BLOCK)
            .expect("block");
        let home_at = gw2_sent
            .iter()
            .rposition(
                |t| matches!(t, TwrCmd::NodeAdd { flags, .. } if flags & link::NODE_HOME != 0),
            )
            .expect("promoted");
        assert!(block_at < home_at, "a fresh block before HOME");
        assert_eq!(
            count_twr(&modems[GW2], link::cmd::TWR_CTR_BLOCK),
            blocks_before + 1
        );

        // Converged: a second pass changes nothing.
        let (n1, n2) = (modems[LOCAL].sent.len(), modems[GW2].sent.len());
        c.converge(&mut modems).await;
        assert_eq!((modems[LOCAL].sent.len(), modems[GW2].sent.len()), (n1, n2));
    }

    /// A home that is merely as good keeps its Nodes.
    #[tokio::test]
    async fn a_home_heard_about_as_well_stays() {
        let (store, _t, mut c, mut modems) = two_gateways("gw-stay").await;
        for counter in 1..=6 {
            for (gw, rssi) in [(GW2, -60), (LOCAL, -63)] {
                assert!(c.activate(gw));
                let modem = modems.get_mut(gw).expect("modem");
                c.on_event(
                    modem,
                    uplink_heard(counter, rssi, app(port::TELEMETRY, &[1])),
                )
                .await
                .expect("uplink");
            }
            c.review_homes();
        }
        assert_eq!(session_now(&store).home, None);
        assert!(!c.converge_due());
    }

    /// A home gone offline hands its Nodes to the gateway that heard them (§5.1).
    #[tokio::test]
    async fn an_offline_home_hands_its_nodes_to_a_gateway_that_heard_them() {
        let (store, _t, mut c, mut modems) = two_gateways("gw-offline").await;
        {
            let mut db = SessionsDb::load(&store).expect("load");
            db.sessions.get_mut(DEV_EUI).expect("session").home = Some(GW2.into());
            db.save(&store).expect("save");
        }
        c.converge(&mut modems).await;
        assert!(c.activate(LOCAL));
        let local = modems.get_mut(LOCAL).expect("local");
        c.on_event(local, uplink_heard(1, -90, app(port::TELEMETRY, &[1])))
            .await
            .expect("uplink");

        modems.remove(GW2);
        c.remove_gateway(GW2);
        c.rehome_from(GW2);
        assert_eq!(session_now(&store).home, None, "back home to the local one");
        c.converge(&mut modems).await;
        assert_eq!(node_adds(&modems[LOCAL]).last(), Some(&link::NODE_HOME));
    }

    /// One Northbridge alone never waits for a join window and needs no extra block.
    #[tokio::test]
    async fn a_single_gateway_central_is_unchanged() {
        let (_store, _t, c, modem) = joined("gw-single").await;
        assert_eq!(c.gateway_names(), BTreeSet::from([LOCAL.to_string()]));
        assert_eq!(count_twr(&modem, link::cmd::TWR_CTR_BLOCK), 1);
        assert!(!c.converge_due());
    }

    /// A fake shared with the test while [`run_central_multi`] drives it.
    #[derive(Clone)]
    struct SharedModem(
        std::sync::Arc<tokio::sync::Mutex<FakeModem>>,
        std::sync::Arc<tokio::sync::Notify>,
    );

    impl SharedModem {
        fn new() -> Self {
            SharedModem(std::sync::Arc::default(), std::sync::Arc::default())
        }
        async fn push(&self, ev: Event) {
            self.0.lock().await.events.push_back(ev);
            self.1.notify_one();
        }
        async fn sent(&self) -> Vec<SentCmd> {
            self.0.lock().await.sent.clone()
        }
    }

    #[async_trait::async_trait]
    impl Modem for SharedModem {
        async fn get_info(&mut self) -> Result<crate::p2p::modem_io::ModemInfo> {
            self.0.lock().await.get_info().await
        }
        async fn set_radio_config(&mut self, cfg: &RadioConfig) -> Result<()> {
            self.0.lock().await.set_radio_config(cfg).await
        }
        async fn rx_stop(&mut self) -> Result<()> {
            self.0.lock().await.rx_stop().await
        }
        async fn tx_schedule(&mut self, tx: &TxSchedule) -> Result<()> {
            self.0.lock().await.tx_schedule(tx).await
        }
        async fn tx_cancel(&mut self, tx_id: u8) -> Result<()> {
            self.0.lock().await.tx_cancel(tx_id).await
        }
        async fn twr(&mut self, cmd: &TwrCmd) -> Result<Vec<u8>> {
            self.0.lock().await.twr(cmd).await
        }
        async fn next_event(&mut self) -> Result<Event> {
            loop {
                if let Some(ev) = self.0.lock().await.events.pop_front() {
                    return Ok(ev);
                }
                self.1.notified().await;
            }
        }
    }

    async fn wait_for(what: &str, mut done: impl AsyncFnMut() -> bool) {
        for _ in 0..200 {
            if done().await {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    fn rx_heard(frame: Vec<u8>, rssi_dbm: i16) -> Event {
        Event::Rx(RxEvent {
            t_ms: 1000,
            rssi_dbm,
            snr_q_db: 36,
            frame,
        })
    }

    /// A JoinReq heard by two gateways is answered through the one that heard it best, which
    /// becomes the Node's home; the other takes the peer without `HOME` (§5.1, §7).
    #[tokio::test]
    async fn a_join_heard_twice_is_answered_through_the_better_gateway() {
        let (store, _t) = temp_store("gw-join");
        register(&store);
        let c = central(store.clone());
        let local = SharedModem::new();
        let gw2 = SharedModem::new();
        let (changes, rx) = tokio::sync::mpsc::channel(4);
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let run = tokio::spawn(run_central_multi_until(
            c,
            Box::new(local.clone()),
            rx,
            async move {
                let _ = stop_rx.await;
            },
        ));
        changes
            .send(GatewayChange::Online(GW2.into(), Box::new(gw2.clone())))
            .await
            .expect("send");
        wait_for("the second gateway started", async || {
            gw2.sent().await.contains(&start())
        })
        .await;

        local.push(rx_heard(join_req(5), -95)).await;
        gw2.push(rx_heard(join_req(5), -60)).await;
        let txs = |sent: Vec<SentCmd>| {
            sent.iter()
                .filter(|c| matches!(c, SentCmd::TxSchedule(_)))
                .count()
        };
        wait_for("the JoinAccept", async || txs(gw2.sent().await) == 1).await;
        let _ = stop_tx.send(());
        run.await.expect("join").expect("run");

        assert_eq!(
            txs(local.sent().await),
            0,
            "the weaker copy is not answered"
        );
        assert_eq!(session_now(&store).home.as_deref(), Some(GW2));
        let flags = |sent: Vec<SentCmd>| -> Vec<u8> {
            sent.iter()
                .filter_map(|c| match c {
                    SentCmd::Twr(TwrCmd::NodeAdd { flags, .. }) => Some(*flags),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(flags(gw2.sent().await), vec![link::NODE_HOME]);
        assert_eq!(flags(local.sent().await), vec![0]);
    }
}
