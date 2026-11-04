//! The uplink pipeline — `plan/control/radio/plan.md` §7, §9.4, §9.7.
//!
//! One ChirpStack integration event in; the northbound publishes and cache writes it implies
//! out. **Nothing here touches a broker, a socket or the filesystem** — [`process`] is a
//! function of its arguments, and [`commit`] is the one place a cache write happens. That is
//! what makes the whole decode path testable against the shipped code with no broker running,
//! which decision #12's standalone guarantee requires and which the v1 lineage never had.
//!
//! # The route a frame takes
//!
//! 1. **Attribute it.** `deviceInfo.devEui`, normalized, joined against `nodes.json`. A device
//!    with no enrollment record is **skipped**, by name — see [`SkipReason::NotEnrolled`].
//! 2. **Resolve the decoder.** The record names a `device_profiles.{otaa|abp}.<name>` entry
//!    and that entry names a registry key (§4). Never a ChirpStack per-device-profile
//!    JavaScript codec — that rule is binding (§7, decision #8 §5) and `payload_codec_runtime`
//!    is not even a field in the config schema.
//! 3. **Dispatch on fPort.** 2 → telemetry, 3 → the alarm batch, 85 → the command engine
//!    (§9.7), anything else → log and skip. There is no legacy fPort-1/`object` fallback in
//!    v2: minimum supported Node firmware is v2.0.0.
//! 4. **Refresh the retained `info`.** Every uplink — including one on an fPort this build
//!    skips — proves the device was heard, so `pairing_status` becomes `joined` and
//!    `last_seen` moves. A frame that could not be decoded is still evidence of life, and an
//!    inventory that only advanced on decodable frames would show a device with a broken
//!    sensor as one that had stopped reporting.
//!
//! # Node-reported state
//!
//! An fPort-85 `Info` and a completed `ConfigDump` are what the Node says about itself, and
//! they go out on the retained `device_info` and `config` areas
//! (`plan/control/radio/node_reported_state.md` §2). The Hub's broker keeps no retained state
//! across a restart, so [`replay`] rebuilds the same documents from the `node-get` cache for
//! `proximos-control` to republish after every connect (§3). The uplink builds each document
//! through its one constructor ([`DeviceInfoDocument::new`], [`ConfigDocument::render`]) and
//! caches it; the replay rebuilds `device_info` through the same constructor and republishes
//! `config` as the cache holds it, so a replay cannot publish a different document than the
//! uplink did.
//!
//! # Joins
//!
//! ChirpStack's `event/join` is not an uplink the Node's firmware sent a body in, but it is
//! the one place the LoRaWAN session shows: [`process_join`] keeps its `devAddr` and `time` in
//! the Node's cache and republishes the retained `info` with `dev_addr` and `joined_at`, as the
//! P2P arm's does (node_registry.md §5, amended 2026-09-28, Q-N2). Every later `info` keeps
//! them: an uplink's refresh by [`fold_session`], the replay by reading the cache.
//!
//! # Reassembly state
//!
//! §9.7's paged `ConfigDump` cannot be reassembled by a stateless function, so [`process`]
//! takes a `&mut `[`State`] holding one [`PageReassembler`] per device. It is still fully
//! deterministic — same event plus same state in, same outcome out — and the state is an
//! explicit argument rather than a global precisely so a test can drive three pages through it
//! in order. §7 step 4's guard holds by construction: only fPort-85 dumps are ever reassembled,
//! so no reassembly can merge an alarm into a telemetry report and shift its `message_id`.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::cache::{Cache, CacheDocument, CacheSlot, LorawanSession};
use crate::config::RadioConfig;
use crate::decode::command::{self, PageReassembler, Response, ResponseBody};
use crate::decode::history;
use crate::decode::reading::{self, GatewayRx, NodeReading};
use crate::decode::snapshot::{self, ConfigDocument, DeviceInfoDocument};
use crate::error::{Result, Transport};
use crate::lorawan::enroll::{EnrollmentRecord, KeyDisclosure, NodesFile};
use crate::lorawan::event::{self, JoinEvent, LogEvent, UplinkEvent};
use crate::northbound::{Area, Topics};

/// One message the daemon owes the broker.
///
/// `retained` rather than a `QoS` field: §9.4 fixes the quality of service (QoS 1 throughout)
/// and leaves only the retain flag varying, so this is the one axis a caller can get wrong.
#[derive(Debug, Clone, PartialEq)]
pub struct Publish {
    pub topic: String,
    pub payload: Vec<u8>,
    pub retained: bool,
}

/// One slot of one node's cache, to be written.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheWrite {
    pub id: String,
    pub slot: CacheSlot,
    pub at: String,
    pub value: Value,
}

/// The session a join set up, to be kept in one node's cache. Not a [`CacheWrite`]: that is a
/// `node-get` slot, and this is not one ([`crate::cache::LORAWAN_SESSION_KEY`]).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionWrite {
    pub id: String,
    pub session: LorawanSession,
}

/// Why an event produced nothing.
///
/// Named rather than a bare empty outcome: "this Hub did not enrol that device" and "this
/// build does not decode fPort 10" are different operational facts, and a daemon that logged
/// both as "skipped" would make a mis-enrolled fleet look like a decoder gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// No `nodes.json` record. §4 forbids an implicit decoder, so a device this Hub did not
    /// enrol has no declared way to read its payload — and guessing one would put wrong
    /// numbers in the portal rather than no numbers.
    NotEnrolled(String),
    /// The record names a profile the current config no longer defines, or the profile names a
    /// decoder the registry does not have.
    NoDecoder { dev_eui: String, why: String },
    /// fPort 1 (retired), 10 (calibration, log-and-skip in phase 1), or anything unrecognised.
    UnhandledPort { dev_eui: String, f_port: u32 },
    /// The event carried no usable `deviceInfo.devEui`.
    Unattributable(String),
    /// A join whose `devAddr` is not an address. Nothing is kept, so the `info` goes on
    /// showing the session before it rather than half of a new one.
    MalformedJoin { dev_eui: String, why: String },
}

impl SkipReason {
    /// A log line. One sentence, saying which device and why.
    pub fn describe(&self) -> String {
        match self {
            SkipReason::NotEnrolled(eui) => format!(
                "{eui}: no enrollment record on this Hub, so no decoder is declared for it \
                 (plan.md §4); run node-add, or ignore if it belongs to another Hub"
            ),
            SkipReason::NoDecoder { dev_eui, why } => format!("{dev_eui}: {why}"),
            SkipReason::UnhandledPort { dev_eui, f_port } => format!(
                "{dev_eui}: fPort {f_port} is not decoded in v2 (fPort 1 is retired, 10 is \
                 calibration); minimum supported Node firmware is v2.0.0"
            ),
            SkipReason::Unattributable(why) => why.clone(),
            SkipReason::MalformedJoin { dev_eui, why } => format!("{dev_eui}: {why}"),
        }
    }
}

/// What one event implies.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    pub publishes: Vec<Publish>,
    pub cache_writes: Vec<CacheWrite>,
    /// A join's session, for [`commit`] to keep.
    pub session: Option<SessionWrite>,
    /// Present when the frame's *payload* produced nothing. The retained `info` refresh may
    /// still be in `publishes` — being heard is not the same as being understood.
    pub skipped: Option<SkipReason>,
}

impl Outcome {
    fn skip(reason: SkipReason) -> Self {
        Outcome {
            skipped: Some(reason),
            ..Default::default()
        }
    }

    /// The message published on one area, if any — the shape assertions read.
    pub fn published(&self, topics: &Topics, dev_eui: &str, area: Area) -> Option<&Publish> {
        let topic = topics.node(dev_eui, area);
        self.publishes.iter().find(|p| p.topic == topic)
    }
}

/// What the pipeline needs that is not the event itself.
pub struct Context<'a> {
    pub config: &'a RadioConfig,
    pub nodes: &'a NodesFile,
    pub topics: &'a Topics,
}

/// Per-device reassembly state — see the module note.
#[derive(Debug, Default)]
pub struct State {
    dumps: BTreeMap<String, InFlightDump>,
}

/// One device's dump in progress, tied to the exchange it answers.
///
/// **Keyed by `seq`, because pages of different exchanges must never meet.** A `get-config`
/// can be abandoned half-way (GitLab #91: the Node answered page 0 of 6 and nothing more), and
/// the reassembler on its own knows only page numbers. The Node's next boot dump (seq 0, one
/// page) was then rejected as a "page count change" and lost. A fresh `get-config` with the
/// same page count was worse: its page 0 was deduped as a retransmit of the abandoned one, and
/// the "completed" dump joined the stale page's bytes with the fresh ones — a wrong
/// configuration, not a missing one. A page whose `seq` is not the one in flight therefore
/// starts a new reassembly: the old exchange cannot finish any more, since the Node answers one
/// command at a time and a boot ends whatever it was sending.
///
/// **And by kind.** From Node firmware v2.0.0 (sticker-firmware #425) every response type can
/// page, not only `ConfigDump`: an `Info` page carrying the `seq` of a dump in flight is a
/// different answer, not the dump's next page (node_reported_state.md §2.2, amendment
/// 2026-09-23).
#[derive(Debug)]
struct InFlightDump {
    seq: u32,
    kind: &'static str,
    /// When the latest page arrived, unix seconds. `None` when its timestamp did not parse,
    /// which only switches the expiry off for this reassembly.
    last_page_at: Option<i64>,
    pages: PageReassembler,
}

/// How long a reassembly waits for its next page before it is given up (#96).
///
/// Long, because a stream can legitimately stall: the device sends its pages back to back
/// (about 5 s apart on EU868 DR0), but after a dozen DR0 frames it hits the 1 % duty cycle, and
/// LoRaMac 1.0.4 refills a band only when its one-hour observation window has passed. Seen on
/// the bench 2026-09-23: pages 0–3 of `GetConfig` seq 108 at 13:10Z, pages 4–5 due at 13:59Z.
/// Expiring earlier would throw away an answer whose rest is still on its way. Stale pages are
/// not what the expiry guards against — every exchange has its own `seq`, so a late page can
/// only ever meet its own answer; it bounds how long a half answer sits in memory.
const REASSEMBLY_EXPIRY_S: i64 = 2 * 3600;

impl InFlightDump {
    fn new(seq: u32, kind: &'static str, at: Option<i64>) -> Self {
        InFlightDump {
            seq,
            kind,
            last_page_at: at,
            pages: PageReassembler::new(Transport::Lorawan),
        }
    }

    fn expired(&self, now: Option<i64>) -> bool {
        matches!((self.last_page_at, now), (Some(last), Some(now)) if now - last > REASSEMBLY_EXPIRY_S)
    }
}

/// A paged answer the current frame completed.
struct Completed {
    page_count: u32,
    /// Every page's oneof submessage in page order, i.e. the whole answer as one message.
    joined: Vec<u8>,
    /// `joined` decoded, for the kinds that are published as state (`Info`).
    body: Option<ResponseBody>,
}

impl State {
    pub fn new() -> Self {
        State::default()
    }

    /// Forget a device's in-flight reassembly. `node-remove`'s and `clear`'s counterpart in
    /// memory, so a re-enrolled device does not inherit half a config dump.
    pub fn forget(&mut self, dev_eui: &str) {
        self.dumps.remove(dev_eui);
    }
}

/// Run one uplink event through §7.
pub fn process(ev: &UplinkEvent, ctx: &Context<'_>, state: &mut State) -> Outcome {
    let dev_eui = match event::uplink_dev_eui(ev) {
        Ok(e) => e,
        Err(e) => return Outcome::skip(SkipReason::Unattributable(e.message().to_string())),
    };
    let id = dev_eui.as_str().to_string();

    let Some(record) = ctx.nodes.by_dev_eui(&dev_eui) else {
        return Outcome::skip(SkipReason::NotEnrolled(id));
    };

    // A timestamp is not optional downstream: it is half the `message_id` derivation and the
    // whole of `last_seen`. An event without one is stamped with the local clock rather than
    // dropped — a reading a few milliseconds late beats no reading, and the substitution is
    // visible because ChirpStack always sets `time` when it is healthy.
    let received_at = event::uplink_received_at(ev).unwrap_or_else(|| {
        crate::lorawan::gateway::format_rfc3339(crate::lorawan::gateway::now_unix())
    });

    let mut out = Outcome::default();
    // Published first, so it is present whatever the payload turns out to be. Being heard is
    // not the same as being understood.
    out.publishes
        .push(info_refresh(ctx.topics, record, &received_at));

    let f_port = ev.f_port;
    match f_port {
        2 | 3 => decode_fields(ev, ctx, record, &id, &received_at, f_port, &mut out),
        85 => decode_command(ev, ctx, record, &id, &received_at, state, &mut out),
        other => {
            out.skipped = Some(SkipReason::UnhandledPort {
                dev_eui: id,
                f_port: other,
            })
        }
    }
    out
}

/// Run one `event/join` — node_registry.md §5, amended 2026-09-28 (Q-N2).
///
/// Attributed and gated like an uplink: a DevEUI this Hub did not enrol on LoRaWAN is skipped
/// by name. The outcome is the retained `info` with the new session's `dev_addr` and
/// `joined_at`, and the session itself for [`commit`] to keep, so the replay and every later
/// uplink's refresh ([`fold_session`]) carry them too. A JoinRequest is a frame the Node sent,
/// so the join also counts as hearing it: `pairing_status` becomes `joined` and `last_seen`
/// the join's time.
pub fn process_join(ev: &JoinEvent, ctx: &Context<'_>) -> Outcome {
    let dev_eui = match event::join_dev_eui(ev) {
        Ok(e) => e,
        Err(e) => return Outcome::skip(SkipReason::Unattributable(e.message().to_string())),
    };
    let id = dev_eui.as_str().to_string();
    let Some(record) = ctx.nodes.by_dev_eui(&dev_eui) else {
        return Outcome::skip(SkipReason::NotEnrolled(id));
    };
    let dev_addr = match event::join_dev_addr(ev) {
        Ok(a) => a,
        Err(e) => {
            return Outcome::skip(SkipReason::MalformedJoin {
                dev_eui: id,
                why: e.message().to_string(),
            })
        }
    };
    // Stamped from the local clock when ChirpStack sent no time, as an uplink is: the Portal
    // shows when the session began, and a few milliseconds late beats not at all.
    let joined_at = event::join_time(ev).unwrap_or_else(|| {
        crate::lorawan::gateway::format_rfc3339(crate::lorawan::gateway::now_unix())
    });
    let session = LorawanSession {
        dev_addr,
        joined_at,
    };
    Outcome {
        publishes: vec![info_document(
            ctx.topics,
            record,
            Some(&session.joined_at),
            Some(&session),
        )],
        session: Some(SessionWrite { id, session }),
        ..Default::default()
    }
}

/// What one ChirpStack `event/log` implies: a journal line, and for a discarded downlink the
/// `command` document that ends the wait for its answer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LogReport {
    /// One line for the journal, opening with its level (`warning:`, `error:`, `info:`).
    pub line: String,
    pub outcome: Outcome,
}

/// Consume ChirpStack's `event/log` (#129).
///
/// ChirpStack deletes a queue item larger than the current data rate's maximum payload at the
/// moment it would transmit it, and says so only here (`DOWNLINK_PAYLOAD_SIZE`, with the item
/// id and both sizes in `context`). A frame that fit when it was queued can still be refused:
/// ADR lowered the data rate, or a set-config was packed for a data rate the Node has since
/// left. Without this the operator saw `queued: true` and then nothing at all.
///
/// The discard is logged as a warning. When the item is one `node-send` recorded
/// ([`crate::cache::DOWNLINK_ID_KEY`]), its seq gets the `undelivered` `command` document the
/// P2P arm publishes for a downlink it gave up (`p2p_tower_gateway.md` §10.5), so the Portal
/// closes the request as not delivered and shows why; `attempts` is 0 because the frame was
/// never on the air. `prior` is the node's cache document. Every other code is only logged.
pub fn process_log(
    ev: &LogEvent,
    ctx: &Context<'_>,
    prior: impl FnOnce(&str) -> Option<CacheDocument>,
) -> LogReport {
    let id = ev.dev_eui.as_str();
    if ev.code != event::LOG_CODE_DOWNLINK_PAYLOAD_SIZE {
        let level = match ev.level.as_str() {
            "ERROR" => "error",
            "WARNING" => "warning",
            _ => "info",
        };
        let context: Vec<String> = ev.context.iter().map(|(k, v)| format!("{k}={v}")).collect();
        return LogReport {
            line: format!(
                "{level}: {id}: ChirpStack {}: {} {}",
                ev.code,
                ev.description,
                context.join(" ")
            )
            .trim_end()
            .to_string(),
            outcome: Outcome::default(),
        };
    }

    let item = ev.context.get("queue_item_id").cloned().unwrap_or_default();
    let size = |key: &str| {
        ev.context_u64(key)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".to_string())
    };
    let (item_size, max) = (size("item_size"), size("max_payload_size"));
    let reason = format!(
        "discarded by the network server: {item_size} B exceeds {max} B at the current data rate"
    );
    let seq = prior(id).and_then(|doc| doc.seq_for_downlink(&item));
    let line = format!(
        "warning: {id}: ChirpStack discarded downlink queue_item_id {item} ({}): item_size \
         {item_size} B > max_payload_size {max} B",
        match seq {
            Some(seq) => format!("seq {seq}, reported to the Portal as undelivered"),
            None => "no node-send seq recorded for it".to_string(),
        }
    );
    let Some(seq) = seq else {
        return LogReport {
            line,
            outcome: Outcome::default(),
        };
    };

    let mut doc = Map::new();
    doc.insert("dev_eui".into(), id.into());
    if let Some(record) = ctx.nodes.by_dev_eui(&ev.dev_eui) {
        doc.insert("serial".into(), record.serial_number.clone().into());
    }
    doc.insert("seq".into(), seq.into());
    doc.insert("kind".into(), crate::p2p::session::UNDELIVERED.into());
    doc.insert("status".into(), crate::p2p::session::UNDELIVERED.into());
    // Never on the air, so no attempt was spent; `last_outcome` is what the Portal shows.
    doc.insert("attempts".into(), 0.into());
    doc.insert("last_outcome".into(), reason.clone().into());
    doc.insert("reason".into(), reason.into());
    doc.insert("code".into(), ev.code.clone().into());
    doc.insert("queue_item_id".into(), item.into());
    if let Some(n) = ev.context_u64("item_size") {
        doc.insert("item_size".into(), n.into());
    }
    if let Some(n) = ev.context_u64("max_payload_size") {
        doc.insert("max_payload_size".into(), n.into());
    }
    let at = ev.time.clone().unwrap_or_else(|| {
        crate::lorawan::gateway::format_rfc3339(crate::lorawan::gateway::now_unix())
    });
    doc.insert("at".into(), at.into());
    LogReport {
        line,
        outcome: Outcome {
            publishes: vec![Publish {
                topic: ctx.topics.node(id, Area::Command),
                payload: serde_json::to_vec(&Value::Object(doc)).unwrap_or_default(),
                retained: Area::Command.retained(),
            }],
            ..Default::default()
        },
    }
}

/// Carry the Node's LoRaWAN session into an uplink's `info` refresh.
///
/// [`process`] sees one frame and no cache, so the `info` it refreshes has no `dev_addr` or
/// `joined_at`, and published as it stands it would drop them from the retained topic until
/// the next Hub restart replayed them. `prior` is the node's cache document before [`commit`],
/// as for [`fold_prior`]. An `info` that already carries a `joined_at` is left alone: that is a
/// join's own, and newer than anything the cache holds.
pub fn fold_session(outcome: &mut Outcome, prior: impl FnOnce(&str) -> Option<CacheDocument>) {
    let Some(p) = outcome
        .publishes
        .iter_mut()
        .find(|p| p.topic.ends_with("/info"))
    else {
        return;
    };
    let Ok(Value::Object(mut doc)) = serde_json::from_slice::<Value>(&p.payload) else {
        return;
    };
    if doc.contains_key("joined_at") {
        return;
    }
    let Some(id) = doc.get("dev_eui").and_then(Value::as_str) else {
        return;
    };
    let Some(session) = prior(id).and_then(|d| d.lorawan_session()) else {
        return;
    };
    insert_session(&mut doc, &session);
    p.payload = serde_json::to_vec(&Value::Object(doc)).unwrap_or_default();
}

/// Fold a completed dump into the configuration the Hub already holds —
/// `node_reported_state.md` §2.2, "The Hub accumulates".
///
/// [`process`] sees one frame and no cache, so the `config` document it builds is that dump
/// alone. Published and cached as it stands, an 8-key `GetParam` answer would replace a 38-key
/// `GetConfig` for every late subscriber and for `node-get config` (measured on the bench,
/// #409). This folds it into the cached one, in both the cache write and the retained
/// publish, so the two stay one document. `prior` is the node's cache document as it stands
/// before [`commit`]; a caller with no cache passes `|_| None` and gets the dump unchanged.
///
/// **Which fold is decided by what the Hub sent under the dump's `seq`** (the cache's
/// [`crate::cache::SENT_COMMANDS_KEY`]). A complete answer to a `get-config` is the Node's
/// whole configuration and *replaces*: an emptied alarm slot sends no field, so a merge kept
/// cleared rules alive (bench, 2026-09-23). Anything else *merges*: the boot settings-info
/// (seq 0), a partial read, or an answer to a command this Hub has no record of.
pub fn fold_prior(
    outcome: &mut Outcome,
    topics: &Topics,
    prior: impl FnOnce(&str) -> Option<CacheDocument>,
) {
    let Some(write) = outcome
        .cache_writes
        .iter_mut()
        .find(|w| w.slot == CacheSlot::Config)
    else {
        return;
    };
    let Some(prior) = prior(&write.id) else {
        return;
    };
    let Some(merged) = config_after_dump(&prior, &write.value) else {
        return;
    };
    write.value = serde_json::to_value(&merged).unwrap_or(Value::Null);
    let topic = topics.node(&merged.dev_eui, Area::Config);
    if let Some(p) = outcome.publishes.iter_mut().find(|p| p.topic == topic) {
        p.payload = serde_json::to_vec(&merged).unwrap_or_default();
    }
}

/// The Hub's config once a completed dump has arrived: `dump` (the rendered `config` document
/// [`decode_command_frame`] built) folded into the config `prior` holds, by [`fold_prior`]'s
/// rule — a complete answer to a recorded `get-config` replaces, anything else merges. `None`
/// when `prior` holds no config or `dump` is not a config document; the dump then stands as it
/// is. Both carriers use it: [`fold_prior`] on LoRaWAN, the P2P central on a `0x55` that
/// completes a dump (PH-2, bench 2026-09-27).
///
/// A dump is the only thing that changes the config. An `ack` of a `set-config` does not
/// (§2.2, the ack fold withdrawn 2026-09-28, Q-N3): the Hub's config holds only what the Node
/// reported, and an acked `--replace-alarms` had put 15 empty slots in it that the Node never
/// reports (P4-1).
pub fn config_after_dump(prior: &CacheDocument, dump: &Value) -> Option<ConfigDocument> {
    let held = prior
        .slot(CacheSlot::Config)
        .and_then(|e| serde_json::from_value::<ConfigDocument>(e.value).ok())?;
    let dump = serde_json::from_value::<ConfigDocument>(dump.clone()).ok()?;
    let answers_get_config =
        prior.sent_tag(dump.seq) == Some(command::CommandTag::GetConfig as u32);
    Some(if answers_get_config {
        dump.replacing(&held)
    } else {
        dump.over(&held)
    })
}

/// Apply an outcome's cache writes.
///
/// Separate from [`process`] so the decision and the side effect are testable apart, and so a
/// caller that only wants to publish (a replay, a dry run) can.
pub fn commit(cache: &Cache, outcome: &Outcome) -> Result<()> {
    for w in &outcome.cache_writes {
        cache.merge_write(&w.id, w.slot, &w.at, w.value.clone())?;
    }
    if let Some(w) = &outcome.session {
        cache.set_lorawan_session(&w.id, &w.session)?;
    }
    Ok(())
}

/// §9.4's retained per-node `info`, refreshed with what the uplink just proved.
fn info_refresh(topics: &Topics, record: &EnrollmentRecord, received_at: &str) -> Publish {
    info_document(topics, record, Some(received_at), None)
}

/// The retained `info` document.
///
/// The enrollment record's own rendering — the same one `node-info` prints and `node-add`
/// published — plus the two facts only a received frame can supply, when one has been. Building
/// it any other way would let the retained topic drift from `node-info`. `None` leaves the
/// record's own `pairing_status: null`: a device never heard from has not joined.
///
/// `session` is the Node's LoRaWAN session when the cache holds one (Q-N2); absent before the
/// Node's first join after this build, and after a move, as on P2P.
fn info_document(
    topics: &Topics,
    record: &EnrollmentRecord,
    last_seen: Option<&str>,
    session: Option<&LorawanSession>,
) -> Publish {
    let mut doc = record.to_json(KeyDisclosure::Omitted);
    if let (Some(map), Some(last_seen)) = (doc.as_object_mut(), last_seen) {
        // A LoRaWAN device that has been heard from has joined. There is no `pending` on this
        // arm — that value belongs to the P2P pairing flow.
        map.insert("pairing_status".into(), Value::from("joined"));
        map.insert("last_seen".into(), Value::from(last_seen));
    }
    if let (Some(map), Some(session)) = (doc.as_object_mut(), session) {
        insert_session(map, session);
    }
    Publish {
        topic: topics.node(record.dev_eui.as_str(), Area::Info),
        payload: serde_json::to_vec(&doc).unwrap_or_default(),
        retained: Area::Info.retained(),
    }
}

/// The session's two `info` fields, under the names the P2P `info` uses for its own, so the
/// Portal's diagnostics read one document shape on both carriers.
fn insert_session(info: &mut Map<String, Value>, session: &LorawanSession) {
    info.insert("dev_addr".into(), Value::from(session.dev_addr.as_str()));
    info.insert("joined_at".into(), Value::from(session.joined_at.as_str()));
}

/// The retained `device_info` publish — `node_reported_state.md` §2.1.
fn device_info_publish(topics: &Topics, doc: &DeviceInfoDocument) -> Publish {
    Publish {
        topic: topics.node(&doc.dev_eui, Area::DeviceInfo),
        payload: serde_json::to_vec(doc).unwrap_or_default(),
        retained: Area::DeviceInfo.retained(),
    }
}

/// The retained `config` publish — `node_reported_state.md` §2.2.
fn config_publish(topics: &Topics, doc: &ConfigDocument) -> Publish {
    Publish {
        topic: topics.node(&doc.dev_eui, Area::Config),
        payload: serde_json::to_vec(doc).unwrap_or_default(),
        retained: Area::Config.retained(),
    }
}

/// The retained documents one enrolled node owes a subscriber, rebuilt from its `node-get`
/// cache — `node_reported_state.md` §3.
///
/// `proximos-control` publishes these after every broker connect, because the Hub's mosquitto
/// keeps no retained state across a restart and a Node's boot snapshot may be months old. The
/// cache is the source: already on disk, already what `node-get` answers, and already dropped
/// by `node-remove`/`clear`/`wipe` — so a replay cannot resurrect a removed node.
///
/// - `info` always: a registered Node never heard from stays listed as waiting for its join.
///   `last_seen` is the newest cache slot, since any slot is proof of a frame, or the join
///   when that is newer, as the join's own `info` said. `dev_addr` and `joined_at` come from
///   the cached session (Q-N2).
/// - `device_info` and `config` only when their slot is filled. **Never an empty payload**:
///   that is the removal tombstone, and a replay must not remove.
/// - `config` is the cached document, the fold of every dump ([`fold_prior`]), so the keys
///   earlier dumps contributed survive. A dump cached before the renderer existed, bytes only,
///   comes out rendered after the upgrade.
pub fn replay(
    topics: &Topics,
    record: &EnrollmentRecord,
    cache: Option<&CacheDocument>,
) -> Vec<Publish> {
    let session = cache.and_then(CacheDocument::lorawan_session);
    let last_seen = [
        cache.and_then(newest_slot_at),
        session.as_ref().map(|s| s.joined_at.clone()),
    ]
    .into_iter()
    .flatten()
    .max_by_key(|at| crate::cache::at_order_key(at));
    let mut out = vec![info_document(
        topics,
        record,
        last_seen.as_deref(),
        session.as_ref(),
    )];
    out.extend(replay_reported(topics, record.dev_eui.as_str(), cache));
    out
}

/// The reported-state half of [`replay`]: `device_info` and `config` from a node's cache.
///
/// Carrier-neutral — the P2P arm replays its own `info` (`p2p::replay`) and shares this, so
/// both arms restore the same documents from the same cache shape after a broker connect.
pub fn replay_reported(topics: &Topics, id: &str, cache: Option<&CacheDocument>) -> Vec<Publish> {
    let mut out = Vec::new();
    let Some(doc) = cache else {
        return out;
    };
    if let Some(entry) = doc.slot(CacheSlot::DeviceInfo) {
        if let Value::Object(info) = entry.value {
            out.push(device_info_publish(
                topics,
                &DeviceInfoDocument::new(id, &entry.at, info),
            ));
        }
    }
    if let Some(entry) = doc.slot(CacheSlot::Config) {
        let cached = serde_json::from_value::<ConfigDocument>(entry.value).ok();
        if let Some((mut c, body)) =
            cached.and_then(|c| snapshot::unhex(&c.body_hex).map(|body| (c, body)))
        {
            // The cached stamp when it has one; an older cache stored the dump without it, and
            // the slot's own `at` is the same instant.
            if c.received_at.is_empty() {
                c.received_at = entry.at.clone();
            }
            c.dev_eui = id.to_string();
            // The cache is the truth (§3): what goes out is what `node-get config` answers, the
            // fold of every dump the Node sent. Re-rendering the last dump over it would put
            // that dump's values back over whatever the cache holds instead. Only a cache
            // written before the renderer existed, bytes and no keys, is rendered, so it comes
            // out with keys after the upgrade.
            if c.config.is_empty() {
                let rendered = snapshot::render_config_dump(&body);
                c.config = rendered.config;
                if c.w1_slot_type.is_none() {
                    c.w1_slot_type = rendered.w1_slot_type;
                }
            }
            out.push(config_publish(topics, &c));
        }
    }
    out
}

/// The newest `at` across a node's filled slots: [`CacheDocument::last_heard`], compared on
/// [`crate::cache::at_order_key`] so a whole-second stamp and a fractional one order right.
pub fn newest_slot_at(doc: &CacheDocument) -> Option<String> {
    doc.last_heard()
}

/// The `decoder:` registry key this device's profile names — §4.
fn decoder_name<'a>(config: &'a RadioConfig, record: &EnrollmentRecord) -> Option<&'a str> {
    match record.activation.as_str() {
        "otaa" => config
            .device_profiles
            .otaa
            .get(&record.profile)
            .map(|p| p.decoder.as_str()),
        "abp" => config
            .device_profiles
            .abp
            .get(&record.profile)
            .map(|p| p.decoder.as_str()),
        _ => None,
    }
}

/// One received frame's carrier facts, with no carrier in them.
///
/// The decode of a Node body is identical on both arms — a P2P `0x02` body *is* an fPort-2
/// body, a `0x55` body *is* an fPort-85 body — and everything that actually differs between
/// ChirpStack and the northbridge modem is in this struct. Passing it, rather than an
/// `UplinkEvent`, is what lets [`decode_field_frame`] and [`decode_command_frame`] serve both
/// carriers without either one learning about the other.
///
/// It deliberately carries **no topic and no publisher**: the core decides what a frame means,
/// the caller decides where the result goes. That split is why the P2P runtime can reuse this
/// while publishing through [`crate::p2p::publisher::NorthboundPublisher`] instead of building
/// a [`Publish`].
pub struct Reception<'a> {
    /// Which arm heard it — the reassembly key space and the error transport, not a topic.
    pub transport: Transport,
    /// The node's wire identity on the topic it is published under: DevEUI on LoRaWAN, serial
    /// on P2P. This is what `message_id` and [`history_message`] are derived from, so the two
    /// agree with the topic a consumer read them off.
    pub id: &'a str,
    /// The LoRaWAN identity, which P2P carries too (from the node registry) — always the
    /// `dev_eui` key of every document this core produces.
    pub dev_eui: &'a str,
    /// The operator-facing identifier, when this Hub knows it.
    pub serial: Option<&'a str>,
    /// RFC3339.
    pub received_at: &'a str,
    /// Every receiver that heard the frame: `rxInfo[]` on LoRaWAN, this Hub's one modem on P2P.
    pub gateways: Vec<GatewayRx>,
    /// The frame counter and what to call it — `("fCnt", n)` on LoRaWAN, `("counter", n)` on
    /// P2P. Named by the caller because the two are different numbers with different
    /// wrap rules, and a consumer that saw one spelling for both could not tell them apart.
    pub counter: (&'static str, u64),
    /// LoRaWAN data-rate index; `None` on P2P, which has no DR.
    pub dr: Option<u32>,
    /// The LNS's own deduplication identity, when there is one. See
    /// [`crate::decode::reading::message_id`].
    pub deduplication_id: Option<&'a str>,
}

/// Decode one field frame (fPort/type 2 or 3) into the [`NodeReading`] envelope, and say which
/// area publishes it and which cache slot holds it.
///
/// `data` is the raw body **including** the `APP_PROTO_VERSION` byte:
/// [`crate::decode::decode_uplink`] strips it itself and turns an unexpected version into a
/// `decode_warning` event rather than a dropped frame.
///
/// `None` means this path does not own the frame — no such decoder in the registry, or a port
/// the field decoders do not decode. §4 allows no implicit fallback, so the caller names the
/// skip rather than guessing a decoder.
pub fn decode_field_frame(
    decoder: &str,
    port: u8,
    data: &[u8],
    rx: &Reception<'_>,
) -> Option<(Area, CacheSlot, Value)> {
    let decoded = crate::decode::decode_uplink(decoder, port, data)?;

    let (fields, mut counters, events) = reading::from_decoded(&decoded, rx.received_at);
    let page = if port == 2 {
        reading::telemetry_page(&decoded)
    } else {
        None
    };
    counters.insert(rx.counter.0.to_string(), rx.counter.1);

    let best = reading::best_gateway(&rx.gateways);

    // With no LNS deduplication id the derivation falls back to identity + timestamp — and on
    // P2P that timestamp is stamped on arrival with one-second resolution, so a telemetry frame
    // and an alarm from one node in the same second would share an id. The frame counter is the
    // only per-frame identity either carrier offers when the LNS supplies none, so it is folded
    // into that case and only that case: a real `deduplicationId` still wins outright.
    let stamp = match rx.deduplication_id {
        Some(id) if !id.is_empty() => rx.received_at.to_string(),
        _ => format!("{}#{}", rx.received_at, rx.counter.1),
    };

    let node = NodeReading {
        message_id: reading::message_id(rx.id, &stamp, rx.deduplication_id),
        dev_eui: rx.dev_eui.to_string(),
        serial: rx.serial.map(str::to_string),
        fields,
        counters,
        events,
        rssi: best.and_then(|g| g.rssi),
        snr: best.and_then(|g| g.snr),
        gateways: rx.gateways.clone(),
        dr: rx.dr,
        received_at: rx.received_at.to_string(),
        fport: Some(port),
        page_index: page.map(|(i, _)| i),
        page_count: page.map(|(_, n)| n),
    };

    // §9.4: a port-2 decode is a `reading`; a port-3 batch is an `alarm`. Same envelope, two
    // areas — `portal` reads one shape from both.
    let (area, slot) = if port == 3 {
        (Area::Alarm, CacheSlot::Alarm)
    } else {
        (Area::Reading, CacheSlot::Message)
    };
    Some((
        area,
        slot,
        serde_json::to_value(&node).unwrap_or(Value::Null),
    ))
}

/// fPort 2 and 3 — the field decoders. The LoRaWAN wrapper around [`decode_field_frame`].
fn decode_fields(
    ev: &UplinkEvent,
    ctx: &Context<'_>,
    record: &EnrollmentRecord,
    id: &str,
    received_at: &str,
    f_port: u32,
    out: &mut Outcome,
) {
    let Some(decoder) = decoder_name(ctx.config, record) else {
        out.skipped = Some(SkipReason::NoDecoder {
            dev_eui: id.to_string(),
            why: format!(
                "the enrollment record names the {} profile {:?}, which radio.device_profiles \
                 no longer defines; re-run node-add against a profile the config declares",
                record.activation, record.profile
            ),
        });
        return;
    };

    // `f_port` is 2 or 3 here, so the cast cannot lose anything.
    let port = f_port as u8;
    let rx = Reception {
        transport: Transport::Lorawan,
        id,
        dev_eui: id,
        serial: Some(record.serial_number.as_str()),
        received_at,
        gateways: ev
            .rx_info
            .iter()
            .map(|r| GatewayRx {
                gateway_id: r.gateway_id.clone(),
                rssi: Some(r.rssi),
                snr: Some(f64::from(r.snr)),
            })
            .collect(),
        // The LoRaWAN frame counter, carried so a consumer can see a gap without joining
        // anything.
        counter: ("fCnt", u64::from(ev.f_cnt)),
        dr: Some(ev.dr),
        deduplication_id: Some(&ev.deduplication_id),
    };

    let Some((area, slot, payload)) = decode_field_frame(decoder, port, &ev.data, &rx) else {
        out.skipped = Some(SkipReason::NoDecoder {
            dev_eui: id.to_string(),
            why: format!(
                "the profile {:?} names decoder {decoder:?}, which is not in the compiled-in \
                 registry ({}); §4 allows no implicit fallback",
                record.profile,
                crate::decode::registry::registered().join(", ")
            ),
        });
        return;
    };

    out.publishes.push(Publish {
        topic: ctx.topics.node(id, area),
        payload: serde_json::to_vec(&payload).unwrap_or_default(),
        retained: area.retained(),
    });
    out.cache_writes.push(CacheWrite {
        id: id.to_string(),
        slot,
        at: received_at.to_string(),
        value: payload,
    });
}

/// fPort 85 — §9.7's command engine. The LoRaWAN wrapper around [`decode_command_frame`]: the
/// same decode as the P2P arm, published as `command`, then whatever the answer also is
/// (`history`, the retained `device_info` and `config`), and cached.
fn decode_command(
    ev: &UplinkEvent,
    ctx: &Context<'_>,
    record: &EnrollmentRecord,
    id: &str,
    received_at: &str,
    state: &mut State,
    out: &mut Outcome,
) {
    let rx = Reception {
        transport: Transport::Lorawan,
        id,
        dev_eui: id,
        // Both identities, as every other document on this arm carries: `dev_eui` is the wire
        // identity the topic is built from, the serial is what an operator typed into
        // `node-send` (tests/carrier_parity.rs).
        serial: Some(record.serial_number.as_str()),
        received_at,
        gateways: Vec::new(),
        counter: ("fCnt", u64::from(ev.f_cnt)),
        dr: Some(ev.dr),
        deduplication_id: Some(&ev.deduplication_id),
    };
    let cd = decode_command_frame(&ev.data, &rx, state);

    let publish = |area: Area, doc: &Value| Publish {
        topic: ctx.topics.node(id, area),
        payload: serde_json::to_vec(doc).unwrap_or_default(),
        retained: area.retained(),
    };
    out.publishes.push(publish(Area::Command, &cd.document));
    // A replay's records go out on `history`, one document per frame, for the portal to store
    // (p2p_history_replay.md §14). The frame stays on `command` too, for `node-send`'s waiter.
    if let Some(h) = &cd.history {
        out.publishes.push(publish(Area::History, &h.document));
    }
    // `Info` and a completed `ConfigDump` are the Node's own state, each on its retained area
    // (node_reported_state.md §2); neither before it is complete.
    if let Some(info) = &cd.device_info {
        out.publishes.push(publish(Area::DeviceInfo, info));
    }
    if let Some(config) = &cd.config {
        out.publishes.push(publish(Area::Config, config));
    }
    for (slot, value) in cd.cache {
        out.cache_writes.push(CacheWrite {
            id: id.to_string(),
            slot,
            at: received_at.to_string(),
            value,
        });
    }
}

/// Everything one fPort-85 / `0x55` body implies, for a caller that publishes it itself: the
/// P2P runtime. The LoRaWAN arm ([`decode_command`]) builds the same documents from the same
/// helpers (`fill_command_document`, [`history_decode`], `ConfigDocument::render`), so the two
/// carriers answer alike (plan.md §9.7: one command engine).
pub struct CommandDecode {
    /// The decoded response, so a caller correlating against an in-flight command does not
    /// decode the body a second time.
    pub response: Response,
    /// The `command` document, complete: what publishes on [`Area::Command`].
    pub document: Value,
    /// The Node's `Info`, unpaged or completed, as the retained `device_info` document.
    pub device_info: Option<Value>,
    /// A `ConfigDump` that *this* page completed, rendered for [`Area::Config`]. Never a
    /// partial one.
    pub config: Option<Value>,
    /// Slots to write, in the order they were produced.
    pub cache: Vec<(CacheSlot, Value)>,
    /// A `HistoryFrame`, decoded and numbered. Whether it publishes is the caller's call: only
    /// the caller knows whether the frame is a fresh one.
    pub history: Option<HistoryDecode>,
}

/// Decode one fPort-85 / `0x55` body for a caller that publishes the result itself.
///
/// `data` carries the `APP_PROTO_VERSION` byte; an unexpected version warns into the document
/// and the remainder is decoded anyway, as on the LoRaWAN arm.
pub fn decode_command_frame(data: &[u8], rx: &Reception<'_>, state: &mut State) -> CommandDecode {
    let (body, warning) = strip_version(data);
    let response = command::decode_response(&body);

    let mut doc = Map::new();
    doc.insert("dev_eui".into(), Value::from(rx.dev_eui));
    if let Some(serial) = rx.serial {
        doc.insert("serial".into(), Value::from(serial));
    }
    doc.insert("received_at".into(), Value::from(rx.received_at));
    doc.insert("seq".into(), Value::from(response.seq));
    doc.insert("kind".into(), Value::from(response.body.kind()));
    if let Some(w) = warning {
        doc.insert("warning".into(), Value::from(w));
    }

    let completed = fill_command_document(&response, rx.id, rx.received_at, state, &mut doc);

    let mut cache = Vec::new();
    let info = match (&response.body, &completed) {
        (ResponseBody::Info(info), None) if response.paging().is_none() => Some(info.clone()),
        (
            _,
            Some(Completed {
                body: Some(ResponseBody::Info(merged)),
                ..
            }),
        ) => Some(merged.clone()),
        _ => None,
    };
    let device_info = info.map(|info| {
        cache.push((CacheSlot::DeviceInfo, Value::Object(info.clone())));
        serde_json::to_value(DeviceInfoDocument::new(rx.dev_eui, rx.received_at, info))
            .unwrap_or(Value::Null)
    });

    let mut config = None;
    if let (ResponseBody::ConfigDump { .. }, Some(done)) = (&response.body, &completed) {
        let dump = ConfigDocument::render(
            rx.dev_eui,
            rx.received_at,
            response.seq,
            done.page_count,
            &done.joined,
        );
        let mut value = serde_json::to_value(&dump).unwrap_or(Value::Null);
        // Both identities, as every other document of this core carries: the serial is what
        // an operator typed into `node-send`.
        if let (Some(serial), Value::Object(map)) = (rx.serial, &mut value) {
            map.insert("serial".into(), Value::from(serial));
        }
        cache.push((CacheSlot::Config, value.clone()));
        config = Some(value);
    }

    let history = match &response.body {
        ResponseBody::HistoryFrame(frame) => {
            Some(history_decode(rx.dev_eui, rx.received_at, &response, frame))
        }
        _ => None,
    };

    CommandDecode {
        response,
        document: Value::Object(doc),
        device_info,
        config,
        cache,
        history,
    }
}

/// One `HistoryFrame` of a replay, numbered as the portal credits it: by the envelope (#425),
/// `0` of `1` when unpaged. Both carriers publish
/// [`HistoryDecode::document`] on `history` (p2p_history_replay.md §14).
pub struct HistoryDecode {
    pub frame_index: u32,
    pub frame_count: u32,
    /// How many records the frame expanded to, for a caller's log line.
    pub records: usize,
    /// The portal's `NodeHistoryMsg`: `{dev_eui, frame_index, frame_count, records}`, plus
    /// `seq` and the frame's header for a reader who wants them.
    pub document: Value,
}

/// Decodes one `HistoryFrame` body into its northbound document.
///
/// Numbered per frame and never coalesced: the portal credits a backfill job frame by frame and
/// closes it on the last one.
pub fn history_decode(
    dev_eui: &str,
    received_at: &str,
    response: &Response,
    body: &[u8],
) -> HistoryDecode {
    let frame = history::decode_frame(body);
    let (frame_index, frame_count) = match response.paging() {
        Some(p) => (p.index, p.count),
        None => (0, 1),
    };
    let records = history::expand(&frame);
    let count = records.len();
    let document = serde_json::json!({
        "dev_eui": dev_eui,
        "received_at": received_at,
        "seq": response.seq,
        "frame_index": frame_index,
        "frame_count": frame_count,
        "t0_unix": frame.t0_unix,
        "interval_s": frame.interval_s,
        "time_synced": frame.time_synced.unwrap_or(true),
        "records": records,
    });
    // v2.0.0 (#430): the frame's own column layout, as Hub field names, or a flag that the
    // registry could not resolve it (the frame's records are then not decoded).
    let mut document = document;
    match frame.columns() {
        Some(Some(cols)) => {
            document["channels"] =
                Value::from(cols.into_iter().map(|c| c.field).collect::<Vec<_>>());
        }
        Some(None) => document["undecodable"] = Value::Bool(true),
        None => {}
    }
    HistoryDecode {
        frame_index,
        frame_count,
        records: count,
        document,
    }
}

/// Add the body-specific keys to a `command` document, and return the whole answer when this
/// page completed a paged one.
///
/// Every page's own keys go into its document as before (`info`, `roms`, `groups`, …). A page
/// that completes an answer carries the whole answer under the same key, so the `complete:
/// true` document is the exchange's result.
fn fill_command_document(
    response: &Response,
    id: &str,
    received_at: &str,
    state: &mut State,
    doc: &mut Map<String, Value>,
) -> Option<Completed> {
    match &response.body {
        ResponseBody::Ack => {}
        ResponseBody::Info(info) => {
            doc.insert("info".into(), Value::Object(info.clone()));
        }
        ResponseBody::Error(e) => {
            doc.insert("error".into(), Value::Object(e.clone()));
        }
        ResponseBody::W1Scan(roms) => {
            doc.insert("roms".into(), roms_value(roms));
        }
        ResponseBody::RadioState(rs) => {
            doc.insert("radio_state".into(), Value::Object(rs.clone()));
        }
        ResponseBody::HistoryFrame(bytes) | ResponseBody::Sample(bytes) => {
            doc.insert("body_hex".into(), Value::from(command::hex(bytes)));
        }
        ResponseBody::Unknown(field) => {
            doc.insert("oneof_field".into(), Value::from(*field));
        }
        ResponseBody::ConfigDump { groups, .. } => {
            doc.insert(
                "groups".into(),
                Value::Array(groups.iter().map(|g| Value::from(*g)).collect()),
            );
        }
    }

    // A dump always goes through the reassembler, a single page as a one-page reassembly
    // (that is what completes it); any other kind only when it is paged.
    let (index, count) = match (response.paging(), &response.body) {
        (Some(p), _) => (p.index, p.count),
        (None, ResponseBody::ConfigDump { .. }) => (0, 1),
        (None, _) => return None,
    };
    doc.insert("page_index".into(), Value::from(index));
    doc.insert("page_count".into(), Value::from(count));

    let kind = response.body.kind();
    if !matches!(kind, "config_dump" | "info" | "w1_scan" | "radio_state") {
        // History pages stay verbatim with their numbering: the history surface is future
        // work, and nothing else pages on the wire.
        return None;
    }

    let now = crate::lorawan::gateway::parse_rfc3339_unix(received_at);
    let in_flight = state
        .dumps
        .entry(id.to_string())
        .or_insert_with(|| InFlightDump::new(response.seq, kind, now));
    if in_flight.seq != response.seq || in_flight.kind != kind {
        // A different exchange: the one in flight is abandoned (see `InFlightDump`). A
        // re-request of a missing page is not one: it goes out under the incomplete answer's own
        // `seq` (`node-send get-config --page N --seq S`), so its pages land in this reassembly
        // by the key alone.
        doc.insert("abandoned_seq".into(), Value::from(in_flight.seq));
        *in_flight = InFlightDump::new(response.seq, kind, now);
    } else if in_flight.expired(now) {
        doc.insert("expired_seq".into(), Value::from(in_flight.seq));
        *in_flight = InFlightDump::new(response.seq, kind, now);
    }
    in_flight.last_page_at = now.or(in_flight.last_page_at);

    let accepted = in_flight.pages.accept(index, count, &response.raw);
    let received = in_flight.pages.received();
    match accepted {
        Ok(Some(joined)) => {
            // Nothing is in flight any more; a retransmit after this starts afresh.
            state.dumps.remove(id);
            doc.insert("complete".into(), Value::Bool(true));
            doc.insert("pages_received".into(), Value::from(count));
            let body = command::decode_joined(kind, &joined);
            match &body {
                Some(ResponseBody::Info(merged)) => {
                    doc.insert("info".into(), Value::Object(merged.clone()));
                }
                Some(ResponseBody::W1Scan(all)) => {
                    doc.insert("roms".into(), roms_value(all));
                }
                Some(ResponseBody::RadioState(all)) => {
                    doc.insert("radio_state".into(), Value::Object(all.clone()));
                }
                Some(ResponseBody::ConfigDump { groups, .. }) if count > 1 => {
                    doc.insert(
                        "groups".into(),
                        Value::Array({
                            // The joined pages name a group once per page it spans; the
                            // whole answer names each once, in order.
                            let mut seen: Vec<&str> = Vec::new();
                            for g in groups {
                                if !seen.contains(g) {
                                    seen.push(g);
                                }
                            }
                            seen.into_iter().map(Value::from).collect()
                        }),
                    );
                }
                _ => {}
            }
            Some(Completed {
                page_count: count,
                joined,
                body: body.filter(|b| matches!(b, ResponseBody::Info(_))),
            })
        }
        Ok(None) => {
            doc.insert("complete".into(), Value::Bool(false));
            doc.insert("pages_received".into(), Value::from(received));
            None
        }
        Err(e) => {
            // A malformed page is a command-plane fault, not a reason to lose the device: the
            // reassembly is dropped so the next exchange starts clean.
            state.dumps.remove(id);
            doc.insert("complete".into(), Value::Bool(false));
            doc.insert("reassembly_error".into(), Value::from(e.message()));
            None
        }
    }
}

fn roms_value(roms: &[String]) -> Value {
    Value::Array(roms.iter().map(|r| Value::from(r.clone())).collect())
}

/// §7 step 1, for the one port `decode_uplink` does not own.
fn strip_version(bytes: &[u8]) -> (Vec<u8>, Option<String>) {
    match bytes.split_first() {
        None => (
            Vec::new(),
            Some("empty payload: nothing to decode".to_string()),
        ),
        Some((&v, rest)) if v == crate::decode::APP_PROTO_VERSION => (rest.to_vec(), None),
        Some((&v, rest)) => (
            rest.to_vec(),
            Some(format!("unknown payload version 0x{v:x}")),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lorawan::eui::Eui;
    use crate::store::Store;

    const RADIO_SECTION: &str = include_str!("../../tests/fixtures/radio-section.yaml");
    const UP_EVENT: &str = include_str!("../../tests/fixtures/chirpstack-up-event.json");

    struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn config() -> RadioConfig {
        RadioConfig::from_device_config_yaml(RADIO_SECTION).expect("the plan example")
    }

    fn nodes(dev_eui: &str) -> NodesFile {
        let mut n = NodesFile::default();
        n.by_serial.insert(
            "SN-1".into(),
            EnrollmentRecord {
                serial_number: "SN-1".into(),
                dev_eui: Eui::parse(dev_eui).expect("eui"),
                profile: "default".into(),
                activation: "otaa".into(),
                join_eui: Some("5876070000000000".into()),
                name: Some("Motion QA".into()),
                app_key: Some("000102030405060708090a0b0c0d0e0f".into()),
            },
        );
        n
    }

    fn topics() -> Topics {
        Topics::new("hub")
    }

    /// The fixture, with `fPort`/`data` swapped for the frame under test.
    fn event(f_port: u32, data_b64_bytes: &[u8]) -> UplinkEvent {
        let mut v: Value = serde_json::from_str(UP_EVENT).expect("fixture");
        v["fPort"] = Value::from(f_port);
        v["data"] = Value::from(base64(data_b64_bytes));
        event::parse_uplink(&serde_json::to_vec(&v).expect("ser")).expect("parses")
    }

    /// Minimal base64, so the fixture can be rewritten without a dependency the crate has
    /// deliberately not taken.
    fn base64(bytes: &[u8]) -> String {
        const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(A[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    fn hexbytes(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    fn reading_of(out: &Outcome, dev_eui: &str, area: Area) -> NodeReading {
        let p = out
            .published(&topics(), dev_eui, area)
            .unwrap_or_else(|| panic!("nothing published on {area:?}"));
        serde_json::from_slice(&p.payload).expect("a NodeReading")
    }

    const DEV: &str = "70b3d57ed80051b2";

    /// The golden: the live fPort-2 frame captured from a real Node, through the shipped
    /// pipeline, to the envelope `portal` will persist.
    #[test]
    fn a_live_uplink_becomes_the_documented_node_reading() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        // The fixture's own `data` — a live frame captured from a real Node.
        let ev = event::parse_uplink(UP_EVENT.as_bytes()).expect("fixture parses");
        let out = process(&ev, &ctx, &mut State::new());

        assert_eq!(out.skipped, None);
        let r = reading_of(&out, DEV, Area::Reading);

        assert_eq!(r.dev_eui, DEV);
        assert_eq!(r.serial.as_deref(), Some("SN-1"));
        assert_eq!(r.fport, Some(2));
        assert_eq!(r.dr, Some(5));
        assert_eq!(r.received_at, "2026-08-27T09:59:19.187803Z");
        assert_eq!(
            r.message_id, "ee59a0b7-6b0a-4b23-a4d0-3f1c15a4b1a2",
            "ChirpStack's own dedup id is the message identity when it has one"
        );

        // The decode itself, at the reference decoder's scalings.
        assert_eq!(r.fields.get("voltage"), Some(&2.74));
        assert_eq!(r.fields.get("temperature"), Some(&24.57));
        assert_eq!(r.fields.get("humidity"), Some(&53.5));
        assert_eq!(r.fields.get("boot"), Some(&0.0));
        assert_eq!(r.counters.get("motion_count"), Some(&9));
        assert_eq!(r.counters.get("accel_motion_count"), Some(&37));
        assert_eq!(r.counters.get("fCnt"), Some(&8));

        // Both receivers survive, and the scalar signal is the loudest of them.
        let ids: Vec<&str> = r.gateways.iter().map(|g| g.gateway_id.as_str()).collect();
        assert_eq!(ids, vec!["24e124fffefd3bda", "0016c001f153a04c"]);
        assert_eq!(r.rssi, Some(-41));
        assert_eq!(r.snr, Some(13.5));

        // …and the cache slot `node-get message` reads.
        assert_eq!(out.cache_writes.len(), 1);
        assert_eq!(out.cache_writes[0].slot, CacheSlot::Message);
        assert_eq!(out.cache_writes[0].id, DEV);
    }

    /// §9.4: every uplink refreshes the retained inventory document, and the refresh is the
    /// enrollment record's own rendering plus the two facts only a frame can supply.
    #[test]
    fn every_uplink_refreshes_the_retained_info_document() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let out = process(&event(2, &hexbytes("010864")), &ctx, &mut State::new());

        let info = out
            .published(&topics, DEV, Area::Info)
            .expect("the retained info document");
        assert!(info.retained, "the inventory topic is retained (§9.4)");
        assert_eq!(info.topic, "proximos/hub/nodes/70b3d57ed80051b2/info");

        let doc: Value = serde_json::from_slice(&info.payload).expect("json");
        assert_eq!(doc["pairing_status"], "joined");
        assert_eq!(doc["last_seen"], "2026-08-27T09:59:19.187803Z");
        assert_eq!(doc["serial_number"], "SN-1");
        assert_eq!(doc["dev_eui"], DEV);
        assert!(
            doc.get("app_key").is_none(),
            "a broker topic never carries key material: {doc}"
        );
    }

    /// A device this Hub did not enrol has no declared decoder (§4), and guessing one would
    /// put wrong numbers in the portal rather than no numbers.
    #[test]
    fn an_unenrolled_dev_eui_is_skipped_by_name_and_publishes_nothing() {
        let (config, topics) = (config(), topics());
        let nodes = nodes("0011223344556677");
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let out = process(&event(2, &hexbytes("010864")), &ctx, &mut State::new());

        assert!(out.publishes.is_empty(), "not even an info refresh");
        assert!(out.cache_writes.is_empty());
        match out.skipped {
            Some(SkipReason::NotEnrolled(ref e)) => assert_eq!(e, DEV),
            other => panic!("expected NotEnrolled, got {other:?}"),
        }
        assert!(out
            .skipped
            .expect("skipped")
            .describe()
            .contains("node-add"));
    }

    /// A frame this build cannot decode is still evidence of life: the inventory has to
    /// advance, or a device with a broken sensor reads as one that stopped reporting.
    #[test]
    fn an_unhandled_fport_still_refreshes_the_inventory() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        for port in [1u32, 10, 200] {
            let out = process(&event(port, &hexbytes("0164")), &ctx, &mut State::new());
            assert!(
                out.published(&topics, DEV, Area::Info).is_some(),
                "fPort {port} was heard"
            );
            assert!(out.published(&topics, DEV, Area::Reading).is_none());
            assert!(out.cache_writes.is_empty());
            assert_eq!(
                out.skipped,
                Some(SkipReason::UnhandledPort {
                    dev_eui: DEV.into(),
                    f_port: port
                })
            );
        }
    }

    /// §9.4: an fPort-3 batch publishes on `alarm`, not `reading` — one envelope, two areas.
    #[test]
    fn an_alarm_batch_publishes_on_the_alarm_area_and_caches_as_alarm() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        // version, base_time, total=1, one event.
        let frame = hexbytes("0108800110011a06080010013002");
        let out = process(&event(3, &frame), &ctx, &mut State::new());

        assert!(out.published(&topics, DEV, Area::Reading).is_none());
        let r = reading_of(&out, DEV, Area::Alarm);
        assert_eq!(r.fport, Some(3));
        assert!(
            r.events.iter().any(|e| e.event_type == "alarm_batch"),
            "the batch header travels: {:?}",
            r.events
        );
        assert_eq!(out.cache_writes[0].slot, CacheSlot::Alarm);
    }

    /// sticker-firmware #460 T2c: a Node sends its alarms unconfirmed unless `radio-alarm-ack`
    /// is set (p2p_link_check.md §3.2, amendment 2026-09-27). ChirpStack answers the MAC-layer
    /// ACK; the Hub decodes the alarm the same either way.
    #[test]
    fn an_alarm_is_raised_the_same_confirmed_or_not() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let frame = hexbytes("0108800110011a06080010013002");
        let alarm_of = |confirmed: bool| {
            let mut v: Value = serde_json::from_str(UP_EVENT).expect("fixture");
            v["fPort"] = Value::from(3);
            v["data"] = Value::from(base64(&frame));
            v["confirmed"] = Value::from(confirmed);
            let ev = event::parse_uplink(&serde_json::to_vec(&v).expect("ser")).expect("parses");
            let out = process(&ev, &ctx, &mut State::new());
            assert_eq!(out.cache_writes[0].slot, CacheSlot::Alarm, "{confirmed}");
            reading_of(&out, DEV, Area::Alarm)
        };
        assert_eq!(alarm_of(false), alarm_of(true));
    }

    /// §7 step 1: an unexpected version warns and the remainder decodes anyway. The warning
    /// has to reach a consumer, which for this envelope means an event.
    #[test]
    fn an_unknown_payload_version_warns_and_still_decodes() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let out = process(&event(2, &hexbytes("020864")), &ctx, &mut State::new());
        let r = reading_of(&out, DEV, Area::Reading);
        assert_eq!(
            r.fields.get("voltage"),
            Some(&2.0),
            "the rest still decoded"
        );
        assert!(r.events.iter().any(|e| e.event_type == "decode_warning"
            && e.extra["message"] == "unknown payload version 0x2"));
    }

    /// One fPort-85 page: a `command` publish and, once the dump completes, a `config` publish
    /// plus the cache slot `node-get config` reads.
    #[test]
    fn an_fport_85_config_dump_publishes_per_page_and_caches_once_complete() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let mut state = State::new();

        // seq 5, config_dump{lorawan{...}}, envelope page 0 of 2
        let page0 = hexbytes("01080522041a02080160006802");
        let out0 = process(&event(85, &page0), &ctx, &mut state);
        let cmd = out0
            .published(&topics, DEV, Area::Command)
            .expect("a command publish per page");
        assert!(!cmd.retained, "command results are not retained");
        let doc: Value = serde_json::from_slice(&cmd.payload).expect("json");
        assert_eq!(doc["kind"], "config_dump");
        assert_eq!(doc["seq"], 5);
        assert_eq!(doc["page_index"], 0);
        assert_eq!(doc["page_count"], 2);
        assert_eq!(doc["complete"], Value::Bool(false));
        assert_eq!(doc["groups"], serde_json::json!(["lorawan"]));
        assert!(
            out0.cache_writes.is_empty(),
            "a half-collected dump is never cached"
        );
        assert!(out0.published(&topics, DEV, Area::Config).is_none());

        // seq 5, page 1 of 2, application group.
        let page1 = hexbytes("01080522042202100a60016802");
        let out1 = process(&event(85, &page1), &ctx, &mut state);
        let doc: Value = serde_json::from_slice(
            &out1
                .published(&topics, DEV, Area::Command)
                .expect("cmd")
                .payload,
        )
        .expect("json");
        assert_eq!(doc["complete"], Value::Bool(true));

        let cfg = out1
            .published(&topics, DEV, Area::Config)
            .expect("the completed dump is published");
        assert!(cfg.retained, "config is Node-reported state and retained");
        let dump: Value = serde_json::from_slice(&cfg.payload).expect("json");
        assert_eq!(dump["page_count"], 2);
        assert_eq!(dump["seq"], 5);
        assert!(dump["body_hex"].as_str().is_some_and(|h| !h.is_empty()));
        assert!(dump["received_at"].as_str().is_some_and(|t| !t.is_empty()));
        // Both pages' groups, rendered under the names `set-config` writes.
        assert_eq!(dump["config"]["lorawan.region"], "us915");
        assert_eq!(dump["config"]["application.interval_sample"], 10);

        assert_eq!(out1.cache_writes.len(), 1);
        assert_eq!(out1.cache_writes[0].slot, CacheSlot::Config);
        assert_eq!(
            out1.cache_writes[0].value, dump,
            "node-get config answers with the published document"
        );
    }

    /// An `Info` response is what `node-get device-info` reads back, and §9.7's redaction
    /// guard has to hold on the published document as well as the cached one.
    #[test]
    fn an_fport_85_info_caches_device_info_with_no_claim_token() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        // seq 7, info{fw 1.4.0, serial 1234, claim_token deadbeef}
        let frame = hexbytes("0108071a0e08011004180028054a04deadbeef");
        let out = process(&event(85, &frame), &ctx, &mut State::new());

        let cmd = out.published(&topics, DEV, Area::Command).expect("command");
        let rendered = String::from_utf8_lossy(&cmd.payload);
        assert!(!rendered.contains("deadbeef"), "leaked: {rendered}");
        assert!(!rendered.contains("claim_token"), "leaked: {rendered}");

        let info = out
            .published(&topics, DEV, Area::DeviceInfo)
            .expect("the Info goes out on its own retained area");
        assert!(info.retained);
        let rendered = String::from_utf8_lossy(&info.payload);
        assert!(!rendered.contains("deadbeef"), "leaked: {rendered}");
        let doc: DeviceInfoDocument = serde_json::from_slice(&info.payload).expect("document");
        assert_eq!(doc.dev_eui, DEV);
        assert_eq!(doc.device_info["fw_version"], "1.4.0");

        assert_eq!(out.cache_writes.len(), 1);
        assert_eq!(out.cache_writes[0].slot, CacheSlot::DeviceInfo);
        assert_eq!(out.cache_writes[0].id, DEV);
        assert_eq!(out.cache_writes[0].at, doc.received_at);
        assert_eq!(
            out.cache_writes[0].value,
            Value::Object(doc.device_info),
            "the document carries exactly the node-get device-info slot"
        );
    }

    /// The boot pair a STICKER on sticker-firmware#413 sends after every join (GitLab #82
    /// hardware note, 2026-09-22): `Info` then a one-page `ConfigDump` with the 1-Wire slots.
    #[test]
    fn the_real_boot_pair_becomes_device_info_and_config() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let mut state = State::new();

        let info = process(
            &event(
                85,
                &hexbytes("011a15080110042002301238cfa4cbd506400150821b5803"),
            ),
            &ctx,
            &mut state,
        );
        let doc: DeviceInfoDocument = serde_json::from_slice(
            &info
                .published(&topics, DEV, Area::DeviceInfo)
                .expect("device_info")
                .payload,
        )
        .expect("document");
        assert_eq!(doc.device_info["fw_version"], "1.4.0");
        assert_eq!(doc.device_info["battery"], 3458);

        let dump = process(
            &event(
                85,
                &hexbytes(
                    "01222510012207103c18840720002a120800100018002000280030003800400148003a0400000000",
                ),
            ),
            &ctx,
            &mut state,
        );
        let doc: ConfigDocument = serde_json::from_slice(
            &dump
                .published(&topics, DEV, Area::Config)
                .expect("a one-page dump completes at once")
                .payload,
        )
        .expect("document");
        assert_eq!(doc.seq, 0, "the boot dump is unsolicited");
        assert_eq!(doc.page_count, 1);
        assert_eq!(doc.config["application.interval_report"], 900);
        assert_eq!(doc.config["sensors.cap_w1_sensors"], true);
        assert_eq!(doc.w1_slot_type, Some(vec!["empty".to_string(); 4]));
    }

    /// `commit` is the only thing that touches the disk, and what it writes is what
    /// `node-get` reads.
    #[test]
    fn committing_an_outcome_fills_the_cache_slot_node_get_reads() {
        let dir = std::env::temp_dir().join(format!("cr-uplink-commit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _guard = TempDir(dir.clone());
        let cache = Cache::new(&Store::new(&dir), Transport::Lorawan);

        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let out = process(&event(2, &hexbytes("010864")), &ctx, &mut State::new());
        commit(&cache, &out).expect("commits");

        let doc = cache.read(DEV).expect("reads").expect("present");
        let entry = doc.slot(CacheSlot::Message).expect("filled");
        assert_eq!(entry.at, "2026-08-27T09:59:19.187803Z");
        assert_eq!(entry.value["dev_eui"], DEV);
        assert_eq!(entry.value["fields"]["voltage"], 2.0);
    }

    /// #126: `node-get message` of a paged report is the whole report, not its last page —
    /// the cached message folds every later page of the same report in; page 0 or an unpaged
    /// frame starts over. The publishes stay one partial reading per page (§12 (B)).
    #[test]
    fn node_get_message_holds_every_page_of_a_paged_report() {
        let dir = std::env::temp_dir().join(format!("cr-uplink-pages-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _guard = TempDir(dir.clone());
        let cache = Cache::new(&Store::new(&dir), Transport::Lorawan);
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        // sticker-firmware #480's HIL captures: one DR0 report in two pages.
        let page0 = hexbytes(
            "01089e01102718fc26205628ffffffff0f30ffffffff0f4800900100980100a00100a80100d00100e80102",
        );
        let page1 = hexbytes("01da01160801100358fb03620da226a2019c047e005c9a0cfe08e00101e80102");
        let message = || {
            cache
                .read(DEV)
                .expect("reads")
                .and_then(|d| d.slot(CacheSlot::Message))
                .expect("filled")
                .value
        };
        let fields = |v: &Value| v["fields"].as_object().expect("fields").clone();

        let out0 = process(&event(2, &page0), &ctx, &mut State::new());
        commit(&cache, &out0).expect("commits page 0");
        let board = fields(&message());
        assert!(board.keys().all(|k| !k.starts_with("w1_")));

        let out1 = process(&event(2, &page1), &ctx, &mut State::new());
        let published: Value =
            serde_json::from_slice(&out1.published(&topics, DEV, Area::Reading).unwrap().payload)
                .unwrap();
        assert!(
            fields(&published).keys().all(|k| k.starts_with("w1_")),
            "the publish is still the page alone"
        );
        commit(&cache, &out1).expect("commits page 1");
        let whole = message();
        let merged = fields(&whole);
        assert!(board.keys().all(|k| merged.contains_key(k)), "page 0 kept");
        assert!(merged.contains_key("w1_1_temperature"), "page 1 added");
        assert_eq!(whole["page_index"], 1);
        assert_eq!(whole["page_count"], 2);

        // The next report's page 0 replaces the cached one.
        let out = process(&event(2, &page0), &ctx, &mut State::new());
        commit(&cache, &out).expect("commits the next page 0");
        assert!(!fields(&message()).contains_key("w1_1_temperature"));

        // So does an unpaged frame.
        commit(&cache, &process(&event(2, &page1), &ctx, &mut State::new())).unwrap();
        commit(
            &cache,
            &process(&event(2, &hexbytes("010864")), &ctx, &mut State::new()),
        )
        .unwrap();
        let single = message();
        assert!(!fields(&single).contains_key("w1_1_temperature"));
        assert!(single.get("page_count").is_none());
    }

    /// §7 step 4's guard, held by construction: reassembly only ever sees fPort-85 dumps, so
    /// nothing can merge an alarm into a telemetry report and shift its `message_id`.
    #[test]
    fn only_fport_85_dumps_are_ever_reassembled() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let mut state = State::new();
        process(&event(2, &hexbytes("010864")), &ctx, &mut state);
        process(&event(3, &hexbytes("0108001001")), &ctx, &mut state);
        assert!(
            state.dumps.is_empty(),
            "no field-port frame ever entered a reassembler"
        );
    }

    /// A re-enrolled device must not inherit half a config dump.
    #[test]
    fn forgetting_a_device_drops_its_in_flight_reassembly() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let mut state = State::new();
        process(
            &event(85, &hexbytes("01080522041a02080160006802")),
            &ctx,
            &mut state,
        );
        assert!(state.dumps.contains_key(DEV));
        state.forget(DEV);
        assert!(!state.dumps.contains_key(DEV));
    }

    /// An event without a `time` is stamped rather than dropped — a reading a few
    /// milliseconds late beats no reading, and `message_id` still has both halves.
    #[test]
    fn an_event_without_a_time_is_stamped_from_the_local_clock() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let mut v: Value = serde_json::from_str(UP_EVENT).expect("fixture");
        v.as_object_mut().expect("object").remove("time");
        v["deduplicationId"] = Value::from("");
        v["data"] = Value::from(base64(&hexbytes("010864")));
        let ev = event::parse_uplink(&serde_json::to_vec(&v).expect("ser")).expect("parses");

        let out = process(&ev, &ctx, &mut State::new());
        let r = reading_of(&out, DEV, Area::Reading);
        assert!(r.received_at.ends_with('Z'), "{}", r.received_at);
        assert_eq!(
            r.message_id,
            format!("{DEV}:{}#{}", r.received_at, ev.f_cnt),
            "with no dedup id the derivation is identity, stamp and frame counter"
        );
    }

    /// The record must name a profile the config still declares; §4 allows no implicit
    /// fallback, so a stale record is a named skip rather than a wrong decode.
    #[test]
    fn a_record_naming_a_profile_the_config_dropped_is_a_named_skip() {
        let (config, topics) = (config(), topics());
        let mut nodes = nodes(DEV);
        if let Some(r) = nodes.by_serial.get_mut("SN-1") {
            r.profile = "cold-chain".into();
        }
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let out = process(&event(2, &hexbytes("010864")), &ctx, &mut State::new());
        assert!(out.published(&topics, DEV, Area::Info).is_some());
        assert!(out.published(&topics, DEV, Area::Reading).is_none());
        match out.skipped {
            Some(SkipReason::NoDecoder { ref why, .. }) => {
                assert!(why.contains("cold-chain"), "{why}")
            }
            other => panic!("expected NoDecoder, got {other:?}"),
        }
    }

    fn record() -> EnrollmentRecord {
        nodes(DEV)
            .by_serial
            .remove("SN-1")
            .expect("the fixture record")
    }

    fn cache_doc(slots: Value) -> CacheDocument {
        let mut v = slots;
        v["id"] = Value::from(DEV);
        serde_json::from_value(v).expect("a cache document")
    }

    /// A registered Node never heard from still gets its inventory back after a Hub reboot —
    /// as `node-add` left it, not as joined.
    #[test]
    fn a_replay_with_no_cache_republishes_the_bare_inventory() {
        let out = replay(&topics(), &record(), None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].topic, topics().node(DEV, Area::Info));
        assert!(out[0].retained);
        let doc: Value = serde_json::from_slice(&out[0].payload).expect("json");
        assert_eq!(doc["pairing_status"], Value::Null);
        assert!(doc.get("last_seen").is_none());
        assert!(
            doc.get("app_key").is_none(),
            "key material stays out of info"
        );
    }

    #[test]
    fn a_replay_rebuilds_every_retained_area_from_the_cache() {
        let body_hex = "10012207103c18840720002a120800100018002000280030003800400148003a0400000000";
        let doc = cache_doc(serde_json::json!({
            "message": { "at": "2026-09-22T19:20:00Z", "value": { "dev_eui": DEV } },
            "device_info": { "at": "2026-09-22T19:09:04Z",
                             "value": { "fw_version": "1.4.0", "battery": 3458 } },
            // The shape a cache written before the renderer holds: bytes only, no stamp.
            "config": { "at": "2026-09-22T19:09:09Z",
                        "value": { "dev_eui": DEV, "seq": 0, "page_count": 1,
                                   "body_hex": body_hex } },
            "command_seq": 4,
        }));

        let out = replay(&topics(), &record(), Some(&doc));
        let topics_out: Vec<&str> = out.iter().map(|p| p.topic.as_str()).collect();
        assert_eq!(
            topics_out,
            vec![
                topics().node(DEV, Area::Info),
                topics().node(DEV, Area::DeviceInfo),
                topics().node(DEV, Area::Config),
            ]
        );
        assert!(out.iter().all(|p| p.retained), "every replay is retained");
        assert!(
            out.iter().all(|p| !p.payload.is_empty()),
            "a replay never publishes the removal tombstone"
        );

        let info: Value = serde_json::from_slice(&out[0].payload).expect("json");
        assert_eq!(info["pairing_status"], "joined");
        assert_eq!(info["last_seen"], "2026-09-22T19:20:00Z", "the newest slot");

        let di: DeviceInfoDocument = serde_json::from_slice(&out[1].payload).expect("doc");
        assert_eq!(di.received_at, "2026-09-22T19:09:04Z");
        assert_eq!(di.device_info["battery"], 3458);

        let cfg: ConfigDocument = serde_json::from_slice(&out[2].payload).expect("doc");
        assert_eq!(
            cfg.received_at, "2026-09-22T19:09:09Z",
            "the slot's own stamp"
        );
        assert_eq!(
            cfg.config["application.interval_report"], 900,
            "a bytes-only cache comes out rendered"
        );
        assert_eq!(cfg.w1_slot_type, Some(vec!["empty".to_string(); 4]));
        assert_eq!(cfg.body_hex, body_hex);
    }

    /// A slot that is not there is not published at all — an empty payload would remove.
    #[test]
    fn a_replay_skips_empty_and_unreadable_slots() {
        let doc = cache_doc(serde_json::json!({
            "alarm": { "at": "2026-09-22T19:00:00Z", "value": {} },
            "config": { "at": "2026-09-22T19:00:00Z",
                        "value": { "dev_eui": DEV, "body_hex": "not hex" } },
        }));
        let out = replay(&topics(), &record(), Some(&doc));
        assert_eq!(out.len(), 1, "only info: {out:?}");
        let info: Value = serde_json::from_slice(&out[0].payload).expect("json");
        assert_eq!(info["last_seen"], "2026-09-22T19:00:00Z");
    }

    #[test]
    fn the_newest_slot_wins_across_whole_and_fractional_seconds() {
        let doc = cache_doc(serde_json::json!({
            "message": { "at": "2026-09-22T19:00:05.100000Z", "value": {} },
            "alarm": { "at": "2026-09-22T19:00:05Z", "value": {} },
            "device_info": { "at": "2026-09-22T18:59:59.999999Z", "value": {} },
        }));
        assert_eq!(
            newest_slot_at(&doc).as_deref(),
            Some("2026-09-22T19:00:05.100000Z")
        );
    }

    const JOIN_EVENT: &str = include_str!("../../tests/fixtures/chirpstack-join-event.json");

    /// The join fixture, attributed to `DEV`.
    fn join_event() -> JoinEvent {
        let mut v: Value = serde_json::from_str(JOIN_EVENT).expect("fixture");
        v["deviceInfo"]["devEui"] = Value::from(DEV);
        event::parse_join(&serde_json::to_vec(&v).expect("ser")).expect("parses")
    }

    fn info_of(out: &Outcome) -> Value {
        let p = out
            .published(&topics(), DEV, Area::Info)
            .expect("the retained info document");
        serde_json::from_slice(&p.payload).expect("json")
    }

    /// Q-N2 (node_registry.md §5, amended 2026-09-28): a join republishes the retained `info`
    /// with the session's `dev_addr` and `joined_at`, and keeps the session in the cache.
    #[test]
    fn a_join_republishes_the_info_with_its_session_and_keeps_it() {
        let dir = std::env::temp_dir().join(format!("cr-uplink-join-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _guard = TempDir(dir.clone());
        let cache = Cache::new(&Store::new(&dir), Transport::Lorawan);
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };

        let out = process_join(&join_event(), &ctx);
        assert_eq!(out.skipped, None);
        assert!(out.cache_writes.is_empty(), "no node-get slot is filled");
        let p = out.published(&topics, DEV, Area::Info).expect("info");
        assert!(p.retained);
        let info = info_of(&out);
        assert_eq!(info["dev_addr"], "01ab23cd");
        assert_eq!(info["joined_at"], "2026-09-27T21:04:11.482161Z");
        assert_eq!(info["pairing_status"], "joined");
        assert_eq!(info["last_seen"], "2026-09-27T21:04:11.482161Z");
        assert_eq!(info["transport"], "lorawan");
        assert!(info.get("app_key").is_none(), "{info}");

        commit(&cache, &out).expect("commits");
        let doc = cache.read(DEV).expect("reads").expect("present");
        assert_eq!(
            doc.lorawan_session(),
            Some(LorawanSession {
                dev_addr: "01ab23cd".into(),
                joined_at: "2026-09-27T21:04:11.482161Z".into(),
            })
        );
        assert!(doc.filled().is_empty(), "bookkeeping, not a slot");
    }

    /// A join is gated like an uplink: a DevEUI this Hub did not enrol on LoRaWAN, or a join
    /// with no address, keeps nothing and publishes nothing.
    #[test]
    fn an_unenrolled_or_malformed_join_is_a_named_skip() {
        let (config, topics) = (config(), topics());
        let other = nodes("0011223344556677");
        let ctx = Context {
            config: &config,
            nodes: &other,
            topics: &topics,
        };
        let out = process_join(&join_event(), &ctx);
        assert_eq!(out.skipped, Some(SkipReason::NotEnrolled(DEV.into())));
        assert!(out.publishes.is_empty() && out.session.is_none());

        let nodes = nodes(DEV);
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let mut v: Value = serde_json::from_str(JOIN_EVENT).expect("fixture");
        v["deviceInfo"]["devEui"] = Value::from(DEV);
        v["devAddr"] = Value::from("");
        let ev = event::parse_join(&serde_json::to_vec(&v).expect("ser")).expect("parses");
        let out = process_join(&ev, &ctx);
        assert!(
            matches!(out.skipped, Some(SkipReason::MalformedJoin { .. })),
            "{:?}",
            out.skipped
        );
        assert!(out.publishes.is_empty() && out.session.is_none());
    }

    /// The uplink after a join refreshes `info` without dropping the session: `process` has no
    /// cache, and `fold_session` carries the cached one in.
    #[test]
    fn a_later_uplinks_info_refresh_keeps_the_session() {
        let dir = std::env::temp_dir().join(format!("cr-uplink-join-up-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _guard = TempDir(dir.clone());
        let cache = Cache::new(&Store::new(&dir), Transport::Lorawan);
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let read = |id: &str| cache.read(id).ok().flatten();

        // Before the first join there is nothing to carry.
        let mut out = process(&event(2, &hexbytes("010864")), &ctx, &mut State::new());
        fold_session(&mut out, read);
        assert!(info_of(&out).get("dev_addr").is_none());
        assert!(info_of(&out).get("joined_at").is_none());
        commit(&cache, &out).expect("commits");

        commit(&cache, &process_join(&join_event(), &ctx)).expect("commits the join");
        let mut out = process(&event(2, &hexbytes("010864")), &ctx, &mut State::new());
        fold_session(&mut out, read);
        let info = info_of(&out);
        assert_eq!(info["dev_addr"], "01ab23cd");
        assert_eq!(info["joined_at"], "2026-09-27T21:04:11.482161Z");
        assert_eq!(
            info["last_seen"], "2026-08-27T09:59:19.187803Z",
            "the uplink's own time"
        );

        // A join's own `info` is never overwritten by the session it replaces.
        let mut v: Value = serde_json::from_str(JOIN_EVENT).expect("fixture");
        v["deviceInfo"]["devEui"] = Value::from(DEV);
        v["devAddr"] = Value::from("02cd45ef");
        let ev = event::parse_join(&serde_json::to_vec(&v).expect("ser")).expect("parses");
        let mut rejoin = process_join(&ev, &ctx);
        fold_session(&mut rejoin, read);
        assert_eq!(info_of(&rejoin)["dev_addr"], "02cd45ef");
    }

    /// The broker keeps no retained state across a restart, so the replay rebuilds `info` with
    /// the cached session — and `last_seen` is the join when nothing newer was heard.
    #[test]
    fn a_replay_after_a_restart_carries_the_session() {
        let doc = cache_doc(serde_json::json!({
            "message": { "at": "2026-09-27T21:00:00Z", "value": { "dev_eui": DEV } },
            "lorawan_session": { "dev_addr": "01ab23cd",
                                 "joined_at": "2026-09-27T21:04:11.482161Z" },
        }));
        let out = replay(&topics(), &record(), Some(&doc));
        assert_eq!(out.len(), 1, "the session is no retained area of its own");
        let info: Value = serde_json::from_slice(&out[0].payload).expect("json");
        assert_eq!(info["dev_addr"], "01ab23cd");
        assert_eq!(info["joined_at"], "2026-09-27T21:04:11.482161Z");
        assert_eq!(info["pairing_status"], "joined");
        assert_eq!(info["last_seen"], "2026-09-27T21:04:11.482161Z", "the join");

        // A frame after the join is the newer evidence.
        let doc = cache_doc(serde_json::json!({
            "message": { "at": "2026-09-27T21:05:00Z", "value": { "dev_eui": DEV } },
            "lorawan_session": { "dev_addr": "01ab23cd",
                                 "joined_at": "2026-09-27T21:04:11.482161Z" },
        }));
        let out = replay(&topics(), &record(), Some(&doc));
        let info: Value = serde_json::from_slice(&out[0].payload).expect("json");
        assert_eq!(info["last_seen"], "2026-09-27T21:05:00Z");

        // Joined on a session only: the Node joined and sent nothing yet.
        let doc = cache_doc(serde_json::json!({
            "lorawan_session": { "dev_addr": "01ab23cd",
                                 "joined_at": "2026-09-27T21:04:11.482161Z" },
        }));
        let out = replay(&topics(), &record(), Some(&doc));
        let info: Value = serde_json::from_slice(&out[0].payload).expect("json");
        assert_eq!(info["pairing_status"], "joined");
        assert_eq!(info["dev_addr"], "01ab23cd");
    }

    /// Absent before the first join, as on P2P.
    #[test]
    fn a_replay_without_a_join_carries_no_session_fields() {
        let doc = cache_doc(serde_json::json!({
            "message": { "at": "2026-09-27T21:00:00Z", "value": { "dev_eui": DEV } },
        }));
        let out = replay(&topics(), &record(), Some(&doc));
        let info: Value = serde_json::from_slice(&out[0].payload).expect("json");
        assert!(info.get("dev_addr").is_none(), "{info}");
        assert!(info.get("joined_at").is_none(), "{info}");
    }

    /// One fPort-85 `ConfigDump` page carrying one `application` group. `seq` 0 is left off the
    /// wire, as the firmware's proto3 encoder does for the autonomous boot dump.
    fn dump_frame(seq: u8, page_index: u8, page_count: u8, application: &[u8]) -> Vec<u8> {
        let mut body = vec![0x22, application.len() as u8];
        body.extend_from_slice(application);
        let mut frame = vec![0x01];
        if seq != 0 {
            frame.extend([0x08, seq]);
        }
        frame.extend([0x22, body.len() as u8]);
        frame.extend(body);
        // v2.0.0 (#425): the envelope numbers the pages; a single page carries none.
        if page_count >= 2 {
            frame.extend([0x60, page_index, 0x68, page_count]);
        }
        frame
    }

    /// A #425 frame: version byte, `seq`, one oneof submessage, then the envelope's page numbers.
    fn paged_frame(seq: u8, oneof_tag: u8, sub: &[u8], index: u8, count: u8) -> Vec<u8> {
        let mut frame = vec![0x01, 0x08, seq, oneof_tag, sub.len() as u8];
        frame.extend_from_slice(sub);
        frame.extend([0x60, index, 0x68, count]);
        frame
    }

    fn event_at(f_port: u32, bytes: &[u8], time: &str) -> UplinkEvent {
        let mut v: Value = serde_json::from_str(UP_EVENT).expect("fixture");
        v["fPort"] = Value::from(f_port);
        v["data"] = Value::from(base64(bytes));
        v["time"] = Value::from(time);
        event::parse_uplink(&serde_json::to_vec(&v).expect("ser")).expect("parses")
    }

    fn command_doc(out: &Outcome, topics: &Topics) -> Value {
        let p = out
            .published(topics, DEV, Area::Command)
            .expect("every fPort-85 frame has its command document");
        serde_json::from_slice(&p.payload).expect("json")
    }

    /// The frame STICKER 0413 answered `history-replay` with (2026-09-25, seq 111), its
    /// `present` mask 0b11 (field 5) replaced by the v2.0.0 `channels` of the same two columns,
    /// mb temperature and humidity (field 10, `52 02 00 01`).
    const HISTORY_FRAME_0413: &str = "18f7bad8d5062221930a6a900a6a8c0a6a8b0a6a840a69810a69810a\
                                      697c0a68780a69790a69790a69303c380152020001";

    fn history_doc(out: &Outcome, topics: &Topics) -> Value {
        let p = out
            .published(topics, DEV, Area::History)
            .expect("a history frame goes out on `history`");
        assert!(!p.retained, "a replay is rows, not state");
        serde_json::from_slice(&p.payload).expect("json")
    }

    /// p2p_history_replay.md §14: a replay frame reaches the portal decoded, in its
    /// `NodeHistoryMsg` shape, and stays on `command` for `node-send`'s waiter.
    #[test]
    fn a_history_frame_is_published_decoded_on_history() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let body = hexbytes(&HISTORY_FRAME_0413.replace(' ', ""));
        let mut frame = vec![0x01, 0x08, 111, 0x2a, body.len() as u8];
        frame.extend(&body);

        let out = process(&event(85, &frame), &ctx, &mut State::new());
        let doc = history_doc(&out, &topics);
        assert_eq!(doc["dev_eui"], DEV);
        assert_eq!(doc["seq"], 111);
        assert_eq!(
            (doc["frame_index"].clone(), doc["frame_count"].clone()),
            (0.into(), 1.into())
        );
        let records = doc["records"].as_array().expect("records");
        assert_eq!(records.len(), 11);
        assert_eq!(records[0]["time"], 1_790_319_991);
        assert_eq!(records[0]["fields"]["temperature"], 27.07);
        assert_eq!(records[0]["fields"]["humidity"], 53.0);
        assert_eq!(records[1]["time"], 1_790_320_051);

        let cmd = command_doc(&out, &topics);
        assert_eq!(cmd["kind"], "history_frame");
        assert!(
            cmd["body_hex"].is_string(),
            "the verbatim frame stays on command"
        );
    }

    /// One document per frame, numbered as the envelope numbers it, so the portal can credit
    /// the job frame by frame and close it on the last one.
    #[test]
    fn a_paged_history_frame_keeps_its_numbering() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let body = hexbytes(&HISTORY_FRAME_0413.replace(' ', ""));
        let mut state = State::new();
        for index in 0..3u8 {
            let out = process(
                &event(85, &paged_frame(7, 0x2a, &body, index, 3)),
                &ctx,
                &mut state,
            );
            let doc = history_doc(&out, &topics);
            assert_eq!(doc["frame_index"], index);
            assert_eq!(doc["frame_count"], 3);
            assert_eq!(doc["records"].as_array().map(Vec::len), Some(11));
        }
        assert!(
            state.dumps.is_empty(),
            "history frames are never reassembled"
        );
    }

    /// #96: a paged `Info` is the device's state only once every page is in; the page before
    /// that is a record on `command` and nothing more.
    #[test]
    fn a_paged_info_publishes_device_info_once_whole() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let mut state = State::new();

        let first = process(
            &event(85, &paged_frame(4, 0x1a, &[0x08, 0x01, 0x10, 0x05], 0, 2)),
            &ctx,
            &mut state,
        );
        assert!(first.published(&topics, DEV, Area::DeviceInfo).is_none());
        assert!(first.cache_writes.is_empty(), "half an Info is not cached");
        let doc = command_doc(&first, &topics);
        assert_eq!(doc["complete"], false);
        assert_eq!(doc["pages_received"], 1);
        assert_eq!(doc["page_count"], 2);

        let mut page1 = vec![0x50u8, 0xb5, 0x19];
        page1.extend([0x7a, 0x06, 0x08, 0x01, 0x10, 0x00, 0x18, 0x02]);
        let second = process(
            &event(85, &paged_frame(4, 0x1a, &page1, 1, 2)),
            &ctx,
            &mut state,
        );
        let info = second
            .published(&topics, DEV, Area::DeviceInfo)
            .expect("the whole Info is published");
        let info: Value = serde_json::from_slice(&info.payload).expect("json");
        assert_eq!(info["device_info"]["fw_version"], "1.5.0");
        assert_eq!(info["device_info"]["battery"], 3253);
        assert_eq!(
            info["device_info"]["active_alarms"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
        let doc = command_doc(&second, &topics);
        assert_eq!(doc["complete"], true);
        assert_eq!(
            doc["info"]["fw_version"], "1.5.0",
            "the completing page carries the whole answer"
        );
    }

    /// #425 firmware no longer fills `ConfigDump` 1/2: the dump pages by the envelope alone.
    #[test]
    fn a_config_dump_paged_by_the_envelope_reassembles() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let mut state = State::new();

        // application{interval_sample 60}, then sensors{cap_w1_sensors true}.
        let p0 = process(
            &event(85, &paged_frame(9, 0x22, &[0x22, 0x02, 0x10, 0x3c], 0, 2)),
            &ctx,
            &mut state,
        );
        assert!(p0.published(&topics, DEV, Area::Config).is_none());
        let p1 = process(
            &event(85, &paged_frame(9, 0x22, &[0x2a, 0x02, 0x40, 0x01], 1, 2)),
            &ctx,
            &mut state,
        );
        let cfg = p1
            .published(&topics, DEV, Area::Config)
            .expect("the two pages make one dump");
        let doc: ConfigDocument = serde_json::from_slice(&cfg.payload).expect("document");
        assert_eq!(doc.seq, 9);
        assert_eq!(doc.page_count, 2);
        assert_eq!(doc.config["application.interval_sample"], 60);
        assert_eq!(doc.config["sensors.cap_w1_sensors"], true);
    }

    /// The bench capture of 2026-09-23 13:01:56–13:02:20Z: sticker-firmware #425 (`p425-dbg`
    /// @9ef98b0) answering `GetConfig` seq 101 at EU868 DR0 with six pages numbered only in the
    /// envelope. The Hub without #96 took every page for a whole one-page dump and the retained
    /// `config` ended up with the last page's five keys.
    #[test]
    fn a_firmware_425_get_config_captured_on_the_bench_reassembles_whole() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let mut state = State::new();
        let pages = [
            "01086522181a16080010021800200028003208587607000000041378016806",
            "010865221f1a173a0800000000000000005204000000006800700580010122040800103c60016806",
            "010865221c220c1884072000280330e01238012a0c08001000180020002800300060026806",
            "01086522172a1538004001480050005a08000000000000000098010060036806",
            "01086522202a1e620800000000000000006a0800000000000000007208000000000000000060046806",
            "01086522142a0b78008001008801009001003205080aa0010060056806",
        ];
        let mut last = None;
        for (i, hex) in pages.iter().enumerate() {
            let out = process(&event(85, &hexbytes(hex)), &ctx, &mut state);
            if i + 1 < pages.len() {
                assert!(
                    out.published(&topics, DEV, Area::Config).is_none(),
                    "page {i} alone"
                );
            }
            last = Some(out);
        }
        let out = last.expect("six pages");
        let cfg = out
            .published(&topics, DEV, Area::Config)
            .expect("the sixth page completes the dump");
        let doc: ConfigDocument = serde_json::from_slice(&cfg.payload).expect("document");
        assert_eq!(doc.seq, 101);
        assert_eq!(doc.page_count, 6);
        for group in ["lorawan", "application", "sensors", "alarms"] {
            assert!(
                doc.config
                    .keys()
                    .any(|k| k.starts_with(&format!("{group}."))),
                "{group} is in the whole dump"
            );
        }
        assert!(
            doc.config.len() > 30,
            "the whole config, not one page: {}",
            doc.config.len()
        );
    }

    /// STICKER 0413's P2P `GetConfig` seq 55 (0x37) on sticker-firmware 7be011e, six pages as
    /// decrypted off the air (bench PS-1, 2026-09-27): 40 keys, one alarm rule (`alarm_0`).
    const GET_CONFIG_7BE011E: [&str; 6] = [
        "01083722181a16080010021800200128003208587607000000041378026806",
        "010837221f1a173a0800000000000000005204000000006806700580010022040800103c60016806",
        "010837221b220b183c2001280330e01238012a0c08001000180020002800300060026806",
        "010837221c2a16380040004800500078008001008801009001009801003202080a60036806",
        "010837222232161a1103000000000000704100000c4200000000a00100420808a0cff89d03100760046806",
        "01083722044202180e60056806",
    ];

    /// PS-1, bench 2026-09-27: STICKER 0413 on sticker-firmware feat-p2p 7be011e answering a
    /// P2P `GetConfig` (seq 55) in six pages, as decrypted off the air. Pages 4 and 5 carry the
    /// `p2p` group (`ConfigDump` field 8), which the Hub on the 177a3dd snapshot dropped: its
    /// retained `config` had 37 keys and no radio tuning. The whole document, key for key.
    #[test]
    fn a_7be011e_get_config_keeps_the_p2p_group() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let mut state = State::new();
        let pages = GET_CONFIG_7BE011E;
        let mut last = None;
        for (i, hex) in pages.iter().enumerate() {
            let out = process(&event(85, &hexbytes(hex)), &ctx, &mut state);
            if i + 1 < pages.len() {
                assert!(
                    out.published(&topics, DEV, Area::Config).is_none(),
                    "page {i} alone"
                );
            }
            last = Some(out);
        }
        let out = last.expect("six pages");
        let cfg = out
            .published(&topics, DEV, Area::Config)
            .expect("the sixth page completes the dump");
        let doc: ConfigDocument = serde_json::from_slice(&cfg.payload).expect("document");
        assert_eq!((doc.seq, doc.page_count), (55, 6));
        let expected = serde_json::json!({
            "lorawan.region": "eu868",
            "lorawan.sub_band": 2,
            "lorawan.network": "public",
            "lorawan.adr": true,
            "lorawan.activation": "otaa",
            "lorawan.deveui": "5876070000000413",
            "lorawan.radio_mode": "p2p",
            "lorawan.joineui": "0000000000000000",
            "lorawan.devaddr": "00000000",
            "lorawan.radio_link_check_interval": 6,
            "lorawan.radio_link_check_fail_rejoin": 5,
            "lorawan.datarate": "auto",
            "application.calibration": false,
            "application.interval_sample": 60,
            "application.interval_report": 60,
            "application.history_enable": true,
            // `application.history_sensors` (field 5) is in the dump too; v2.0.0 (435273c6)
            // reserves the field, so the key table no longer names it and it is skipped.
            // So are `sensors` 3, 4, 7 and 19 (the GP bool caps, reserved since #498).
            "application.battery_level": 2400,
            "application.vendor_reset_allow": true,
            "sensors.cap_hall_left": false,
            "sensors.cap_hall_right": false,
            "sensors.cap_light_sensor": false,
            "sensors.cap_barometer": false,
            "sensors.cap_w1_sensors": false,
            "sensors.cap_accelerometer": false,
            "sensors.accel_motion_sensitivity": "off",
            "sensors.hall_left_counter": false,
            "sensors.hall_right_counter": false,
            "sensors.input_a_counter": false,
            "sensors.input_b_counter": false,
            "alarms.alarm_limit": 10,
            "alarms.alarm_0": "03000000000000704100000c4200000000",
            "alarms.alarm_buzzer_mode": "off",
            "p2p.frequency": 868_100_000,
            "p2p.spreading_factor": 7,
            "p2p.tx_power": 14,
        });
        assert_eq!(
            serde_json::Value::Object(doc.config.clone()),
            expected,
            "the whole 7be011e dump"
        );
        assert_eq!(
            doc.config.len(),
            35,
            "the bench's 37 keys and the three p2p ones, less history_sensors and the four GP caps"
        );
    }

    /// `get_radio_state` (sticker-firmware #446) is answered by `Response.radio_state`, field
    /// 14. The `command` document carries it decoded, as `info` and `roms` are, not as the
    /// `oneof_field` of an unknown arm; paged, the completing page carries the whole of it.
    #[test]
    fn a_radio_state_answer_is_a_decoded_command_document() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let mut state = State::new();
        // seq 21, radio_state{state 2, sf 7, dl_rssi -87}.
        let frame = hexbytes("01081572070802100728ad01");
        let out = process(&event(85, &frame), &ctx, &mut state);
        let cmd = out.published(&topics, DEV, Area::Command).expect("command");
        let doc: Value = serde_json::from_slice(&cmd.payload).expect("json");
        assert_eq!(doc["kind"], "radio_state");
        assert_eq!(doc["radio_state"]["state_name"], "healthy");
        assert_eq!(doc["radio_state"]["dl_rssi"], -87);
        assert!(doc.get("oneof_field").is_none(), "{doc}");

        // Two pages: state + sf, then the counters.
        let p0 = process(
            &event(85, &paged_frame(22, 0x72, &[0x08, 0x03, 0x10, 0x0a], 0, 2)),
            &ctx,
            &mut state,
        );
        let d0: Value = serde_json::from_slice(
            &p0.published(&topics, DEV, Area::Command)
                .expect("page 0")
                .payload,
        )
        .expect("json");
        assert_eq!(d0["complete"], false);
        let p1 = process(
            &event(85, &paged_frame(22, 0x72, &[0x98, 0x01, 0x90, 0x1c], 1, 2)),
            &ctx,
            &mut state,
        );
        let d1: Value = serde_json::from_slice(
            &p1.published(&topics, DEV, Area::Command)
                .expect("page 1")
                .payload,
        )
        .expect("json");
        assert_eq!(d1["complete"], true);
        assert_eq!(d1["radio_state"]["state_name"], "warning");
        assert_eq!(d1["radio_state"]["sf"], 10);
        assert_eq!(d1["radio_state"]["uptime_s"], 3600, "field 19 from page 1");
    }

    /// The bench frames of 2026-09-23 13:01–13:05Z: pages 0–2 of `GetConfig` seq 101, then pages
    /// 3–5 as a re-request would bring them — under the same seq, which is how `node-send
    /// get-config --page 3 --seq 101` asks. The key alone fills the gap and the dump completes.
    #[test]
    fn a_re_request_under_the_original_seq_fills_the_gap() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let mut state = State::new();
        let pages = [
            "01086522181a16080010021800200028003208587607000000041378016806",
            "010865221f1a173a0800000000000000005204000000006800700580010122040800103c60016806",
            "010865221c220c1884072000280330e01238012a0c08001000180020002800300060026806",
            "01086522172a1538004001480050005a08000000000000000098010060036806",
            "01086522202a1e620800000000000000006a0800000000000000007208000000000000000060046806",
            "01086522142a0b78008001008801009001003205080aa0010060056806",
        ];
        let mut last = None;
        for hex in pages {
            last = Some(process(&event(85, &hexbytes(hex)), &ctx, &mut state));
        }
        let out = last.expect("six frames");
        let cfg = out
            .published(&topics, DEV, Area::Config)
            .expect("the re-requested pages complete the dump");
        let doc: ConfigDocument = serde_json::from_slice(&cfg.payload).expect("document");
        assert_eq!(doc.seq, 101);
        assert_eq!(doc.page_count, 6);
        assert!(doc.config.len() > 30, "{}", doc.config.len());
    }

    /// A different `seq` is a different exchange even when it starts past page 0: merging two
    /// answers would join two snapshots of the configuration (a `SetParam` may sit between them).
    #[test]
    fn a_page_of_another_seq_never_continues_the_answer_in_flight() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let mut state = State::new();
        process(
            &event(85, &paged_frame(9, 0x22, &[0x22, 0x02, 0x10, 0x3c], 1, 2)),
            &ctx,
            &mut state,
        );
        let out = process(
            &event(85, &paged_frame(10, 0x22, &[0x22, 0x02, 0x10, 0x3c], 0, 2)),
            &ctx,
            &mut state,
        );
        let doc = command_doc(&out, &topics);
        assert_eq!(doc["abandoned_seq"], 9);
        assert!(doc.get("continues_seq").is_none());
    }

    /// An `Info` page carrying the `seq` of a dump in flight is another answer, not its next
    /// page: the dump is abandoned rather than joined with Info bytes.
    #[test]
    fn a_page_of_another_kind_abandons_the_reassembly_in_flight() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let mut state = State::new();

        process(
            &event(85, &paged_frame(9, 0x22, &[0x22, 0x02, 0x10, 0x3c], 0, 2)),
            &ctx,
            &mut state,
        );
        let info = process(
            &event(85, &paged_frame(9, 0x1a, &[0x08, 0x01], 0, 2)),
            &ctx,
            &mut state,
        );
        let doc = command_doc(&info, &topics);
        assert_eq!(doc["abandoned_seq"], 9);
        assert_eq!(doc["pages_received"], 1);
    }

    /// Two hours without the next page and the reassembly is given up: a later page starts a
    /// fresh one. An hour-long duty-cycle pause (the bench, 2026-09-23) still completes.
    #[test]
    fn a_reassembly_expires_after_two_hours_without_a_page() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let mut state = State::new();

        let t0 = "2026-09-23T12:00:00.000000+00:00";
        let late = "2026-09-23T14:00:30.000000+00:00";
        process(
            &event_at(85, &paged_frame(4, 0x1a, &[0x08, 0x01], 0, 2), t0),
            &ctx,
            &mut state,
        );
        let out = process(
            &event_at(85, &paged_frame(4, 0x1a, &[0x10, 0x05], 1, 2), late),
            &ctx,
            &mut state,
        );
        let doc = command_doc(&out, &topics);
        assert_eq!(doc["expired_seq"], 4);
        assert_eq!(doc["complete"], false);
        assert!(out.published(&topics, DEV, Area::DeviceInfo).is_none());

        // Within the window the same two pages complete.
        let mut state = State::new();
        // An hour's duty-cycle pause, as on the bench, is not an expiry.
        let soon = "2026-09-23T12:59:40.000000+00:00";
        process(
            &event_at(85, &paged_frame(4, 0x1a, &[0x08, 0x01], 0, 2), t0),
            &ctx,
            &mut state,
        );
        let out = process(
            &event_at(85, &paged_frame(4, 0x1a, &[0x10, 0x05], 1, 2), soon),
            &ctx,
            &mut state,
        );
        assert!(out.published(&topics, DEV, Area::DeviceInfo).is_some());
    }

    #[test]
    fn a_paged_w1_scan_reports_every_rom_on_its_last_page() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let mut state = State::new();
        let rom = |b: u8| {
            let mut v = vec![0x0au8, 0x08];
            v.extend([0x28, b, 0, 0, 0, 0, 0, 0x01]);
            v
        };
        let first = process(
            &event(85, &paged_frame(7, 0x3a, &rom(0xaa), 0, 2)),
            &ctx,
            &mut state,
        );
        assert_eq!(
            command_doc(&first, &topics)["roms"],
            serde_json::json!(["28aa000000000001"])
        );
        let last = process(
            &event(85, &paged_frame(7, 0x3a, &rom(0xbb), 1, 2)),
            &ctx,
            &mut state,
        );
        let doc = command_doc(&last, &topics);
        assert_eq!(doc["complete"], true);
        assert_eq!(
            doc["roms"],
            serde_json::json!(["28aa000000000001", "28bb000000000001"])
        );
    }

    /// GitLab #91 on the bench (2026-09-23): a `get-config` answered page 0 of 6 and nothing
    /// more. That half-finished exchange must not swallow the Node's next boot dump, which is a
    /// different exchange (seq 0, one page) and the only way its configuration refreshes.
    #[test]
    fn an_abandoned_paged_dump_does_not_swallow_the_next_boot_dump() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let mut state = State::new();

        let stale = process(
            &event(85, &dump_frame(6, 0, 6, &[0x10, 0x0a])),
            &ctx,
            &mut state,
        );
        assert!(stale.published(&topics, DEV, Area::Config).is_none());

        let boot = process(
            &event(
                85,
                &hexbytes(
                    "01222510012207103c18840720002a120800100018002000280030003800400148003a0400000000",
                ),
            ),
            &ctx,
            &mut state,
        );
        let cfg = boot
            .published(&topics, DEV, Area::Config)
            .expect("the boot dump is published, not rejected against the stale reassembly");
        let doc: ConfigDocument = serde_json::from_slice(&cfg.payload).expect("document");
        assert_eq!(doc.seq, 0);
        assert_eq!(doc.config["application.interval_sample"], 60);
        assert_eq!(boot.cache_writes.len(), 1, "and cached for node-get");
    }

    /// A fresh `get-config` whose pages reuse the stale exchange's page numbers must not have its
    /// page 0 discarded as a "retransmit" of the abandoned one — the dump would then join the
    /// stale page's bytes with the fresh ones.
    #[test]
    fn a_new_exchange_restarts_the_reassembly_instead_of_deduping_against_the_old_one() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let mut state = State::new();

        // seq 7, page 0 of 2 (interval_sample = 10) — then the exchange is abandoned.
        process(
            &event(85, &dump_frame(7, 0, 2, &[0x10, 0x0a])),
            &ctx,
            &mut state,
        );
        // seq 8, both pages: interval_sample = 60, then interval_report = 900.
        let p0 = process(
            &event(85, &dump_frame(8, 0, 2, &[0x10, 0x3c])),
            &ctx,
            &mut state,
        );
        assert!(p0.published(&topics, DEV, Area::Config).is_none());
        let p1 = process(
            &event(85, &dump_frame(8, 1, 2, &[0x18, 0x84, 0x07])),
            &ctx,
            &mut state,
        );
        let doc: ConfigDocument = serde_json::from_slice(
            &p1.published(&topics, DEV, Area::Config)
                .expect("seq 8 completes on its own two pages")
                .payload,
        )
        .expect("document");
        assert_eq!(doc.seq, 8);
        assert_eq!(
            doc.config["application.interval_sample"], 60,
            "seq 8's own page 0, not the abandoned seq 7 one"
        );
        assert_eq!(doc.config["application.interval_report"], 900);
    }

    /// The bench case (#409): a full `GetConfig` is cached, then an 8-key `GetParam` answer
    /// arrives. Both the retained publish and the cache must carry the union.
    #[test]
    fn a_partial_dump_is_folded_into_the_cached_config_before_it_goes_out() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let held = cache_doc(serde_json::json!({
            "config": { "at": "2026-09-23T12:30:12Z",
                        "value": { "dev_eui": DEV, "received_at": "2026-09-23T12:30:12Z",
                                   "seq": 9, "page_count": 6, "body_hex": "",
                                   "config": { "lorawan.region": "eu868",
                                               "application.interval_sample": 10 },
                                   "w1_slot_type": ["dallas"] } },
        }));

        // seq 2, one page: interval_sample = 60.
        let mut out = process(
            &event(85, &dump_frame(2, 0, 1, &[0x10, 0x3c])),
            &ctx,
            &mut State::new(),
        );
        fold_prior(&mut out, &topics, |id| (id == DEV).then(|| held.clone()));

        let published: ConfigDocument = serde_json::from_slice(
            &out.published(&topics, DEV, Area::Config)
                .expect("config")
                .payload,
        )
        .expect("document");
        assert_eq!(
            published.config["application.interval_sample"], 60,
            "newer wins"
        );
        assert_eq!(
            published.config["lorawan.region"], "eu868",
            "the held key stays"
        );
        assert_eq!(published.w1_slot_type, Some(vec!["dallas".to_string()]));
        assert_eq!(published.seq, 2);
        let cached: ConfigDocument =
            serde_json::from_value(out.cache_writes[0].value.clone()).expect("document");
        assert_eq!(
            cached, published,
            "cache and retained topic stay one document"
        );
    }

    /// Bench, 2026-09-23: a complete `get-config` (seq 117) must drop the alarm rules the
    /// device had cleared — the Hub sent a GetConfig under that seq, so its answer replaces.
    #[test]
    fn a_complete_answer_to_a_sent_get_config_replaces_the_held_config() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let held = cache_doc(serde_json::json!({
            "config": { "at": "T1",
                        "value": { "dev_eui": DEV, "received_at": "T1", "seq": 115,
                                   "page_count": 14, "body_hex": "",
                                   "config": { "alarms.alarm_0": "0300",
                                               "application.interval_sample": 10 },
                                   "w1_slot_type": ["dallas"] } },
            "sent_commands": [ { "seq": 115, "tag": 5 }, { "seq": 117, "tag": 5 } ],
        }));

        let mut out = process(
            &event(85, &dump_frame(117, 0, 1, &[0x10, 0x3c])),
            &ctx,
            &mut State::new(),
        );
        fold_prior(&mut out, &topics, |_| Some(held.clone()));
        let published: ConfigDocument = serde_json::from_slice(
            &out.published(&topics, DEV, Area::Config)
                .expect("config")
                .payload,
        )
        .expect("document");
        assert!(
            !published.config.contains_key("alarms.alarm_0"),
            "the rule the device cleared is gone"
        );
        assert_eq!(published.config["application.interval_sample"], 60);
        assert_eq!(published.config.len(), 1);
        assert_eq!(published.w1_slot_type, Some(vec!["dallas".to_string()]));

        // The same dump under a seq the Hub never sent (a raw --hex, a stranger): merged.
        let mut other = process(
            &event(85, &dump_frame(99, 0, 1, &[0x10, 0x3c])),
            &ctx,
            &mut State::new(),
        );
        fold_prior(&mut other, &topics, |_| Some(held.clone()));
        let merged: ConfigDocument = serde_json::from_slice(
            &other
                .published(&topics, DEV, Area::Config)
                .expect("config")
                .payload,
        )
        .expect("document");
        assert!(
            merged.config.contains_key("alarms.alarm_0"),
            "unknown seq merges"
        );
    }

    #[test]
    fn folding_without_a_held_config_leaves_the_dump_alone() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let mut out = process(
            &event(85, &dump_frame(2, 0, 1, &[0x10, 0x3c])),
            &ctx,
            &mut State::new(),
        );
        let before = out.clone();
        fold_prior(&mut out, &topics, |_| None);
        assert_eq!(out, before);
        // And a frame that completed no dump has nothing to fold.
        let mut info = process(
            &event(
                85,
                &hexbytes("011a15080110042002301238cfa4cbd506400150821b5803"),
            ),
            &ctx,
            &mut State::new(),
        );
        let before = info.clone();
        fold_prior(&mut info, &topics, |_| {
            panic!("no config write, no cache read")
        });
        assert_eq!(info, before);
    }

    /// The replay re-renders the latest dump over the accumulated config: keys an earlier dump
    /// contributed must survive a Hub reboot.
    #[test]
    fn a_replay_keeps_the_keys_earlier_dumps_contributed() {
        let doc = cache_doc(serde_json::json!({
            "config": { "at": "2026-09-23T12:30:58Z",
                        "value": { "dev_eui": DEV, "received_at": "2026-09-23T12:30:58Z",
                                   "seq": 2, "page_count": 1,
                                   // The latest dump: interval_sample = 60 only.
                                   "body_hex": "080010012202103c",
                                   "config": { "lorawan.region": "eu868",
                                               "application.interval_sample": 60 } } },
        }));
        let out = replay(&topics(), &record(), Some(&doc));
        let cfg: ConfigDocument = serde_json::from_slice(
            &out.iter()
                .find(|p| p.topic == topics().node(DEV, Area::Config))
                .expect("config")
                .payload,
        )
        .expect("document");
        assert_eq!(cfg.config["lorawan.region"], "eu868");
        assert_eq!(cfg.config["application.interval_sample"], 60);
        assert_eq!(cfg.seq, 2);
    }

    /// The replay publishes the cache, not the last dump re-rendered over it (§3, the cache is
    /// the truth): the retained `config` after a Hub restart is what `node-get config` answers.
    /// Where the two disagree, as in a cache an older build folded an acked write into
    /// (§2.2, withdrawn 2026-09-28), the cached value goes out until the Node reports again.
    #[test]
    fn a_replay_publishes_the_cached_config_not_the_last_dump_over_it() {
        let doc = cache_doc(serde_json::json!({
            "config": { "at": "2026-09-23T12:30:58Z",
                        "value": { "dev_eui": DEV, "received_at": "2026-09-23T12:30:58Z",
                                   "seq": 2, "page_count": 1,
                                   // The last dump: interval_sample = 60.
                                   "body_hex": "080010012202103c",
                                   // What the cache holds: 300, and a key from an older read.
                                   "config": { "application.interval_sample": 300,
                                               "lorawan.region": "eu868" } } },
        }));
        let out = replay(&topics(), &record(), Some(&doc));
        let cfg: ConfigDocument = serde_json::from_slice(
            &out.iter()
                .find(|p| p.topic == topics().node(DEV, Area::Config))
                .expect("config")
                .payload,
        )
        .expect("document");
        let cached: ConfigDocument =
            serde_json::from_value(doc.slot(CacheSlot::Config).expect("config slot").value)
                .expect("document");
        assert_eq!(cfg, cached, "the replay is the cached document");
        assert_eq!(cfg.config["application.interval_sample"], 300);
    }

    /// Q-N3 (node_reported_state.md §2.2, the ack fold withdrawn 2026-09-28): an ack of a
    /// `set-config` changes nothing in the Hub's config, even for a seq a cache from an older
    /// build still records a `written` body under. The ack goes out on `command`, as before;
    /// the config holds only what the Node reported.
    #[test]
    fn an_ack_of_a_write_changes_nothing_in_the_config() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let held = cache_doc(serde_json::json!({
            "config": { "at": "T1",
                        "value": { "dev_eui": DEV, "received_at": "T1", "seq": 21,
                                   "page_count": 6, "body_hex": "",
                                   "config": { "application.interval_report": 60 } } },
            // alarm_0 as the pre-Q-N3 `set-config` recorded it: `written` is ignored now.
            "sent_commands": [ { "seq": 22, "tag": 2,
                                 "written": "32131a1103000000000000a0410000c84100000000" } ],
        }));

        // `Response { seq 22, ack {} }`, through what the broker loop does after `process`.
        let mut out = process(&event(85, &hexbytes("0108161200")), &ctx, &mut State::new());
        fold_prior(&mut out, &topics, |_| Some(held.clone()));
        let ack: Value = serde_json::from_slice(
            &out.published(&topics, DEV, Area::Command)
                .expect("the ack goes out on command")
                .payload,
        )
        .expect("json");
        assert_eq!(
            (ack["kind"].as_str(), ack["seq"].as_u64()),
            (Some("ack"), Some(22))
        );
        assert!(out.published(&topics, DEV, Area::Config).is_none());
        assert!(
            out.cache_writes.iter().all(|w| w.slot != CacheSlot::Config),
            "{:?}",
            out.cache_writes
        );
    }

    /// P4-1, bench 2026-09-27 (build c57, STICKER 0413): the same 28-byte boot settings-info
    /// left 40 keys in the Hub's config on P2P and 55 on LoRaWAN, `alarms.alarm_1` …
    /// `alarm_15` added as all-zero strings. The firmware omits every empty alarm slot from
    /// every dump (`app_config_alarms_slot_empty()`), so the Node never sent them. What added
    /// them was the ack of the Portal's `--replace-alarms` write (E3), which the Hub folded
    /// into its config with every cleared slot as 17 zero bytes.
    ///
    /// Q-N3 (node_reported_state.md §2.2, withdrawn 2026-09-28): an ack changes nothing in the
    /// config. The bench sequence now keeps the 40 keys the Node reported through the write,
    /// its ack and the next boot, with `alarm_0` the only slot. The written rule shows in the
    /// config only once a `get-config` reports it.
    #[test]
    fn p4_1_an_acked_replace_leaves_the_config_as_the_node_reported_it() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };

        // The bench's full `get-config` (seq 55): 40 keys, only `alarm_0` among the slots;
        // 35 under the v2.0.0 key table, which no longer names `application.history_sensors`
        // nor the four GP bool caps (`sensors` 3, 4, 7, 19, reserved since #498).
        let mut state = State::new();
        let mut last = None;
        for hex in GET_CONFIG_7BE011E {
            last = Some(process(&event(85, &hexbytes(hex)), &ctx, &mut state));
        }
        let full = last.expect("six pages");
        let full = full
            .cache_writes
            .iter()
            .find(|w| w.slot == CacheSlot::Config)
            .expect("the sixth page completes the dump")
            .value
            .clone();
        let slots = |doc: &ConfigDocument| -> Vec<String> {
            doc.config
                .keys()
                .filter(|k| k.starts_with("alarms.alarm_") && k[13..].parse::<u8>().is_ok())
                .cloned()
                .collect()
        };
        let cache = |config: &Value| {
            cache_doc(serde_json::json!({
                "config": { "at": "T", "value": config },
                "sent_commands": [ { "seq": 55, "tag": command::CommandTag::GetConfig as u32 },
                                   { "seq": 56, "tag": command::CommandTag::SetParam as u32 } ],
            }))
        };
        // The boot settings-info exactly as both carriers carried it (seq 0, one page).
        let boot_body = hexbytes("2206103c183c20012a12080010001800200028003000380040004800");
        let boot =
            serde_json::to_value(ConfigDocument::render(DEV, "T", 0, 1, &boot_body)).expect("json");
        assert_eq!(boot_body.len(), 28);

        // Boot over the full dump: merged, still the 40 keys, still one slot.
        let booted = config_after_dump(&cache(&full), &boot).expect("a held config");
        assert_eq!(booted.config.len(), 35, "{:?}", booted.config.keys());
        assert_eq!(slots(&booted), ["alarms.alarm_0"]);
        let booted = serde_json::to_value(&booted).expect("json");

        // The ack of the replace (seq 56): no config goes out, none is cached.
        let mut acked = process(&event(85, &hexbytes("0108381200")), &ctx, &mut state);
        fold_prior(&mut acked, &topics, |_| Some(cache(&booted)));
        assert!(acked.published(&topics, DEV, Area::Config).is_none());
        assert!(acked
            .cache_writes
            .iter()
            .all(|w| w.slot != CacheSlot::Config));

        // The next boot page (P2P c768, then LoRaWAN after the move) merges over what the Node
        // reported: the same 40 keys.
        let rebooted = config_after_dump(&cache(&booted), &boot).expect("a held config");
        assert_eq!(rebooted.config.len(), 35, "{:?}", rebooted.config.keys());
        assert_eq!(slots(&rebooted), ["alarms.alarm_0"]);
    }

    const LOG: &str = include_str!("../../tests/fixtures/chirpstack-log-event.json");
    const LOG_ITEM: &str = "1b0d7f4e-63a2-4c55-9a0e-2f3b7c9d8e11";

    fn log_event() -> LogEvent {
        event::parse_log(LOG.as_bytes()).expect("parses")
    }

    /// #129: a queue item ChirpStack discards for its size closes the seq it carried as
    /// `undelivered`, with the sizes in the reason, and is logged as a warning.
    #[test]
    fn a_discarded_downlink_fails_its_seq_visibly() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let prior = cache_doc(serde_json::json!({
            "sent_commands": [
                {"seq": 12, "tag": 3, "downlink_id": "other-item"},
                {"seq": 13, "tag": 3, "downlink_id": LOG_ITEM},
            ]
        }));
        let report = process_log(&log_event(), &ctx, |id| {
            assert_eq!(id, DEV);
            Some(prior)
        });
        assert!(report.line.starts_with("warning: "), "{}", report.line);
        for part in [DEV, LOG_ITEM, "seq 13", "88 B", "51 B"] {
            assert!(report.line.contains(part), "{part}: {}", report.line);
        }
        let p = report
            .outcome
            .published(&topics, DEV, Area::Command)
            .expect("a command document");
        assert_eq!(p.retained, Area::Command.retained());
        let doc: Value = serde_json::from_slice(&p.payload).expect("json");
        assert_eq!(doc["seq"], 13);
        assert_eq!(doc["status"], crate::p2p::session::UNDELIVERED);
        assert_eq!(doc["kind"], crate::p2p::session::UNDELIVERED);
        assert_eq!(doc["attempts"], 0);
        assert_eq!(
            doc["reason"],
            "discarded by the network server: 88 B exceeds 51 B at the current data rate"
        );
        assert_eq!(doc["last_outcome"], doc["reason"]);
        assert_eq!(doc["code"], "DOWNLINK_PAYLOAD_SIZE");
        assert_eq!(doc["queue_item_id"], LOG_ITEM);
        assert_eq!(doc["item_size"], 88);
        assert_eq!(doc["max_payload_size"], 51);
        assert_eq!(doc["at"], "2026-10-13T12:08:18.412345Z");
        assert!(
            doc.get("serial").is_some(),
            "an enrolled Node is named by serial too"
        );
        assert!(report.outcome.cache_writes.is_empty());
    }

    /// An item no `node-send` recorded (a raw `--hex`, or older than the sent log) is still
    /// logged, and nothing is published: there is no seq to close.
    #[test]
    fn a_discarded_unknown_item_is_only_logged() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let report = process_log(&log_event(), &ctx, |_| None);
        assert!(report.line.starts_with("warning: "), "{}", report.line);
        assert!(report.line.contains(LOG_ITEM), "{}", report.line);
        assert!(report.outcome.publishes.is_empty());
    }

    /// Every other code is logged at its own level and changes nothing.
    #[test]
    fn other_log_codes_are_only_logged_at_their_level() {
        let (config, nodes, topics) = (config(), nodes(DEV), topics());
        let ctx = Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        };
        let mut ev = log_event();
        ev.code = "UPLINK_F_CNT_RETRANSMISSION".into();
        ev.level = "WARNING".into();
        ev.description = "Uplink was flagged as re-transmission / frame-counter reset".into();
        let report = process_log(&ev, &ctx, |_| panic!("the cache is not read"));
        assert!(report.line.starts_with("warning: "), "{}", report.line);
        assert!(report.line.contains("UPLINK_F_CNT_RETRANSMISSION"));
        assert!(report.outcome.publishes.is_empty());

        ev.level = "INFO".into();
        assert!(process_log(&ev, &ctx, |_| None).line.starts_with("info: "));
        ev.level = "ERROR".into();
        assert!(process_log(&ev, &ctx, |_| None).line.starts_with("error: "));
    }
}
