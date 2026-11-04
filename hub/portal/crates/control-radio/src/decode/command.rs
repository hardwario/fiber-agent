//! The fPort-85 command engine — `plan/control/radio/plan.md` §9.7, vocabulary from
//! `node-firmware-v1.4.0.md` §6.
//!
//! One engine, both transports: LoRaWAN carries it on fPort 85, P2P splits the same
//! bidirectional port into directional frame types `0x55` (response) and `0x56` (command) with
//! byte-identical bodies. Only the split is new.
//!
//! The engine never invents a synchronous request/response illusion over a Class-A link.
//! `node-send get-config` triggers a downlink; the answer arrives asynchronously and
//! `node-get config` reads what arrived.

use serde_json::{Map, Value};

use crate::decode::pb::{read_tag, read_varint, skip};
use crate::decode::{node, sensor_types};
use crate::error::{Component, Error, Kind, Result, Transport};

/// The downlink command tags — `node-firmware-v1.4.0.md` §6. These are protobuf field numbers
/// in the `DownlinkCommand` oneof, so they are wire contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum CommandTag {
    SetParam = 2,
    GetParam = 3,
    GetInfo = 4,
    GetConfig = 5,
    SettingsSave = 6,
    Reboot = 7,
    DeviceReset = 8,
    ForceSend = 9,
    ResetCounters = 10,
    ReqHistory = 11,
    ClockSync = 12,
    ReqHistoryPage = 13,
    W1Scan = 14,
    LrwReset = 16,
    LrwJoin = 17,
    EnterCalibration = 18,
    Sample = 21,
    FactoryReset = 23,
    SetSecretKey = 24,
    ClmAck = 25,
    VendorReset = 26,
    ClmRearm = 27,
    BuzzerPlay = 28,
    /// sticker-firmware #428/#429: the boot settings-info on request — the same one-page
    /// `ConfigDump` a Node sends after every join, answered under the command's `seq`.
    /// Field 31 because 29/30 are #414's and 15 (the retired `req_alarm_rules`) is not reused.
    GetSettings = 31,
}

/// Whether a command may be issued over a radio link at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioPolicy {
    /// Freely issuable over the radio.
    Allowed,
    /// Physically reachable only over NFC or a local shell. **Rejected over radio entirely**,
    /// not merely discouraged.
    NfcOnly,
    /// Allowed only behind a per-command policy check that phase 1 does not implement.
    PolicyGated,
}

impl CommandTag {
    pub const fn policy(self) -> RadioPolicy {
        match self {
            // NFC/shell only. `factory_reset` over the air would be a remote brick, and
            // `set_secret_key` would hand the NFC channel key to whoever owns the radio.
            //
            // `device_reset` belongs here too, on the firmware's authority rather than ours:
            // `app/src/app_config.yml` at v1.4.0 lists it `transports: [nfc, shell]`, and a
            // downlink sent anyway is answered `error { code: 3, "transport not allowed" }`
            // (measured on the bench, 2026-08-25). plan.md §5 and
            // node-firmware-v1.4.0.md §6 still describe it as radio-allowed; the pin wins.
            CommandTag::FactoryReset
            | CommandTag::SetSecretKey
            | CommandTag::DeviceReset
            | CommandTag::ReqHistoryPage => RadioPolicy::NfcOnly,
            CommandTag::EnterCalibration
            | CommandTag::Sample
            | CommandTag::ClmAck
            | CommandTag::VendorReset
            | CommandTag::ClmRearm
            | CommandTag::BuzzerPlay => RadioPolicy::PolicyGated,
            _ => RadioPolicy::Allowed,
        }
    }

    /// The `node-send cmd <name>` spelling, where one exists.
    pub const fn cli_name(self) -> Option<&'static str> {
        match self {
            CommandTag::Reboot => Some("reboot"),
            CommandTag::ClockSync => Some("clock-sync"),
            CommandTag::ResetCounters => Some("reset-counters"),
            CommandTag::ForceSend => Some("force-send"),
            CommandTag::ReqHistory => Some("history-replay"),
            CommandTag::SettingsSave => Some("settings-save"),
            CommandTag::W1Scan => Some("w1-scan"),
            CommandTag::LrwReset => Some("lrw-reset"),
            CommandTag::LrwJoin => Some("lrw-join"),
            _ => None,
        }
    }

    pub fn from_cli_name(name: &str) -> Option<Self> {
        ALL.iter().copied().find(|t| t.cli_name() == Some(name))
    }

    /// Whether the Node answers this command on the command port.
    ///
    /// `force_send` and `sample` answer with a measurement, not a `Response`: the firmware sends
    /// an extra reading and nothing on fPort 85 (HIL #432; the P2P arm the same since
    /// sticker-firmware#448). A carrier that waits for a `Response` to retire the command must
    /// not wait for these — the P2P central re-delivered a `force_send` every four uplinks
    /// (night test 2026-09-26, PN-4).
    pub const fn answers_on_the_command_port(self) -> bool {
        !matches!(self, CommandTag::ForceSend | CommandTag::Sample)
    }
}

/// The command a downlink body carries: the field beside `seq` (field 1) in
/// `Command { seq, <command> }`, the shape [`crate::decode::command_encode::Downlink::encode`]
/// writes. `None` for a body that is not one (an operator's raw `--hex`).
pub fn command_tag_of(body: &[u8]) -> Option<CommandTag> {
    let mut offset = 0;
    while offset < body.len() {
        let tag = super::pb::read_tag(body, offset);
        if tag.next <= offset {
            return None;
        }
        if tag.field != 1 {
            return ALL.iter().copied().find(|t| *t as u32 == tag.field);
        }
        // `seq` is a varint; anything else in field 1 is not a command body.
        if tag.wire != 0 {
            return None;
        }
        offset = super::pb::read_varint(body, tag.next).next;
    }
    None
}

pub const ALL: &[CommandTag] = &[
    CommandTag::SetParam,
    CommandTag::GetParam,
    CommandTag::GetInfo,
    CommandTag::GetConfig,
    CommandTag::SettingsSave,
    CommandTag::Reboot,
    CommandTag::DeviceReset,
    CommandTag::ForceSend,
    CommandTag::ResetCounters,
    CommandTag::ReqHistory,
    CommandTag::ClockSync,
    CommandTag::ReqHistoryPage,
    CommandTag::W1Scan,
    CommandTag::LrwReset,
    CommandTag::LrwJoin,
    CommandTag::EnterCalibration,
    CommandTag::Sample,
    CommandTag::FactoryReset,
    CommandTag::SetSecretKey,
    CommandTag::ClmAck,
    CommandTag::VendorReset,
    CommandTag::ClmRearm,
    CommandTag::BuzzerPlay,
    CommandTag::GetSettings,
];

/// The wire budget a transport can carry in one frame, which is what bounds page reassembly.
///
/// **Never a constant shared by both transports** (§9.7): LoRaWAN's DR0 cap and P2P's frame
/// budget are different numbers, and using one for the other either wastes payload or
/// silently truncates.
pub const fn wire_budget(transport: Transport) -> usize {
    match transport {
        // The worst case a LoRaWAN downlink must fit: DR0 in EU868.
        Transport::Lorawan => 51,
        // The 74 B a P2P frame's payload carries (the 96 B TOWER frame, #486 U-1), less the
        // `0x81 port` envelope (p2p_tower_gateway.md §2.1, §3.4).
        Transport::P2p => crate::p2p::tower::MAX_COMMAND_BODY,
    }
}

/// Per-exchange timeout, per key.
///
/// Cadence-aware by design, and the consequence is worth stating rather than discovering: a
/// multi-key `set-config` is a sequential per-key exchange, so a 38-key write takes **minutes**
/// and must be presented as an in-progress operation, not a hang.
pub const PER_KEY_TIMEOUT_SECS: u64 = 180;

/// Reject a command that must never travel over a radio link.
pub fn check_radio_policy(tag: CommandTag, transport: Transport) -> Result<()> {
    match tag.policy() {
        RadioPolicy::Allowed => Ok(()),
        RadioPolicy::NfcOnly => Err(Error::new(
            Kind::InvalidArgument,
            transport,
            Component::ControlSocket,
            format!("{tag:?} is an NFC/shell-only operation and is refused over radio entirely"),
        )),
        RadioPolicy::PolicyGated => Err(Error::new(
            Kind::Phase2NotImplemented,
            transport,
            Component::ControlSocket,
            format!("{tag:?} needs a per-command policy check that phase 1 does not implement"),
        )),
    }
}

/// Which counters a `reset_counters` clears.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CounterSelection {
    pub hall_left: bool,
    pub hall_right: bool,
    pub input_a: bool,
    pub input_b: bool,
}

impl CounterSelection {
    pub fn is_empty(self) -> bool {
        !(self.hall_left || self.hall_right || self.input_a || self.input_b)
    }
}

/// Mandatory guard: an empty `ResetCounters` is rejected **before encode**.
///
/// The firmware treats an empty selection as "reset everything", so a UI that sends the
/// operator's empty selection through would silently clear counters they did not choose.
pub fn check_reset_counters(sel: CounterSelection, transport: Transport) -> Result<()> {
    if sel.is_empty() {
        return Err(Error::new(
            Kind::InvalidArgument,
            transport,
            Component::EnrollmentRecord,
            "reset-counters names no counter; an empty selection would clear every counter on \
             the device, so it is refused before encoding",
        ));
    }
    Ok(())
}

/// Plausible bounds for a one-shot `clock_sync` epoch.
///
/// 2020-01-01 .. 2100-01-01. A clock-sync carrying a wrong epoch does not fail loudly — it
/// silently re-times every subsequent alarm, so the range check is mandatory.
pub const CLOCK_SYNC_MIN_UNIX: i64 = 1_577_836_800;
pub const CLOCK_SYNC_MAX_UNIX: i64 = 4_102_444_800;

pub fn check_clock_sync_epoch(unix: Option<i64>, transport: Transport) -> Result<()> {
    let Some(t) = unix else {
        // An empty clock_sync means "re-sync from the network", which is always valid.
        return Ok(());
    };
    if !(CLOCK_SYNC_MIN_UNIX..=CLOCK_SYNC_MAX_UNIX).contains(&t) {
        return Err(Error::new(
            Kind::InvalidArgument,
            transport,
            Component::EnrollmentRecord,
            format!(
                "clock-sync epoch {t} is outside the plausible range \
                 [{CLOCK_SYNC_MIN_UNIX}, {CLOCK_SYNC_MAX_UNIX}]; a wrong epoch silently \
                 re-times every subsequent alarm"
            ),
        ));
    }
    Ok(())
}

/// Fields that must never reach a northbound publish.
///
/// `claim_token` is a 128-bit device-claiming secret that appears in an `Info` response; it is
/// redacted from every response before publish, not filtered at the display layer.
pub const REDACTED_FIELDS: &[&str] = &["claim_token"];

/// Strip secret fields from a decoded response, in place.
pub fn redact(data: &mut serde_json::Map<String, serde_json::Value>) {
    for key in REDACTED_FIELDS {
        data.remove(*key);
        // Also inside a nested `info` object, which is where it actually arrives.
        if let Some(serde_json::Value::Object(info)) = data.get_mut("info") {
            info.remove(*key);
        }
    }
}

/// Per-device sequence allocation — §9.7's correlation rule.
///
/// Monotonically increasing per device; a response is matched by sequence, matched once, and
/// duplicates are discarded rather than reprocessed.
#[derive(Debug, Default)]
pub struct SeqAllocator {
    next: u32,
}

impl SeqAllocator {
    /// Resume from the last sequence a device was sent.
    ///
    /// A `proximosctl` invocation is a process that exits, so "monotonically increasing per
    /// device" cannot live in memory: the last value is read back from the device's cache
    /// document ([`crate::cache::Cache::allocate_seq`]) and handed here. `0` means "nothing
    /// has ever been sent to this device", which is also the default state.
    pub fn from_last(last: u32) -> Self {
        SeqAllocator { next: last }
    }

    /// The last sequence issued, for persisting across processes.
    pub fn last(&self) -> u32 {
        self.next
    }

    /// Sequence 0 is never issued, so a response carrying no seq field (which decodes as 0)
    /// cannot be mistaken for a match.
    pub fn allocate(&mut self) -> u32 {
        self.next = self.next.wrapping_add(1);
        if self.next == 0 {
            self.next = 1;
        }
        self.next
    }
}

/// Reassembles a paged `ConfigDump` — §9.7.
///
/// Tolerates out-of-order arrival, matches once, and refuses to grow past the transport's wire
/// budget times the page count it was told to expect.
#[derive(Debug)]
pub struct PageReassembler {
    transport: Transport,
    page_count: Option<u32>,
    pages: std::collections::BTreeMap<u32, Vec<u8>>,
}

impl PageReassembler {
    pub fn new(transport: Transport) -> Self {
        PageReassembler {
            transport,
            page_count: None,
            pages: std::collections::BTreeMap::new(),
        }
    }

    /// Accept a page. Returns the joined body once every page has arrived.
    pub fn accept(
        &mut self,
        page_index: u32,
        page_count: u32,
        body: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        // `control-socket`, not `enrollment-record`: a malformed paged response is a
        // command-plane fault. The enrollment record is the serial↔dev_eui bookkeeping and has
        // nothing to do with it.
        let err =
            |m: String| Error::new(Kind::Transport, self.transport, Component::ControlSocket, m);

        if page_count == 0 {
            return Err(err("a paged response claims zero pages".into()));
        }
        match self.page_count {
            None => self.page_count = Some(page_count),
            Some(known) if known != page_count => {
                return Err(err(format!(
                    "page count changed mid-reassembly: {known} then {page_count}"
                )))
            }
            _ => {}
        }
        if page_index >= page_count {
            return Err(err(format!(
                "page index {page_index} is outside a {page_count}-page response"
            )));
        }

        // Dedup first. A retransmitted page adds nothing to the reassembly, so charging its
        // bytes against the ceiling turned an ordinary retransmit near the size limit into a
        // `Transport` error.
        if self.pages.contains_key(&page_index) {
            return Ok(None);
        }

        // The overflow guard derives from this transport's budget, never a shared constant.
        let budget = wire_budget(self.transport);
        let ceiling = budget.saturating_mul(page_count as usize);
        let projected: usize = self.pages.values().map(Vec::len).sum::<usize>() + body.len();
        if projected > ceiling {
            return Err(err(format!(
                "reassembly would reach {projected} B, past the {ceiling} B ceiling for \
                 {page_count} pages on {}",
                self.transport
            )));
        }

        self.pages.insert(page_index, body.to_vec());

        if self.pages.len() as u32 == page_count {
            // BTreeMap iterates in key order, so out-of-order arrival still joins correctly.
            let joined: Vec<u8> = self.pages.values().flatten().copied().collect();
            // Reset, so a page retransmitted after the dump completed starts a fresh
            // reassembly instead of handing the same dump back a second time.
            self.pages.clear();
            self.page_count = None;
            return Ok(Some(joined));
        }
        Ok(None)
    }

    /// How many distinct pages of the reassembly in flight have arrived.
    pub fn received(&self) -> usize {
        self.pages.len()
    }
}

/// Page `index` (0-based) of `count` — one frame of a paged answer.
///
/// Node firmware v2.0.0 (sticker-firmware #425) carries this in the `Response` **envelope**,
/// fields 12/13, for every response type. Earlier firmware numbered only `ConfigDump`, in its
/// own fields 1/2; [`Response::paging`] reads either. `count < 2` is a single frame and never
/// becomes a `Paging`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Paging {
    pub index: u32,
    pub count: u32,
}

/// The fPort-85 **uplink** — `node-firmware-v1.4.0.md` §5's `Response`.
///
/// `1 seq` (correlates to the command's seq), then a oneof: `2 ack{}`, `3 info`,
/// `4 config_dump`, `5 history_frame`, `6 error`, `7 w1_scan`, `8 sample`, and since
/// sticker-firmware #446 `14 radio_state`.
///
/// Read through [`crate::decode::pb`] rather than through prost, for the reason that module
/// states: there is no `proto/` directory at the pin, the committed schema is generated at
/// firmware build time, and the wire tables in §5 plus the reference decoder are what this
/// crate is proven against.
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseBody {
    /// `2 ack{}` — the command was accepted. An empty message; there is nothing to carry.
    Ack,
    /// `3 info` — §5's Info table, already redacted.
    Info(Map<String, Value>),
    /// `4 config_dump` — one page of the paged config read, numbered in the envelope (#425).
    ConfigDump {
        /// Which `AppConfigMessage` groups this page carries.
        groups: Vec<&'static str>,
        /// The page's whole submessage body, kept verbatim.
        ///
        /// **Why bytes and not named keys.** Turning the group submessages into §7's settable
        /// key space is the command surface's vocabulary, and it arrives with the fPort-85
        /// *encoder* that has to speak the same names in the other direction. Splitting that
        /// table across two work packages would guarantee the two halves disagree. The bytes
        /// are preserved losslessly so nothing has to be re-requested from the device — a
        /// `get-config` exchange costs minutes on a Class-A link.
        ///
        /// The encoder has since landed and the table is
        /// [`crate::decode::command_encode::CONFIG_KEYS`], with
        /// [`crate::decode::command_encode::key_by_wire`] as its reverse lookup. The consumer
        /// change arrived with `node_reported_state.md`: a *completed* dump is rendered through
        /// that table by [`crate::decode::snapshot::render_config_dump`] into the retained
        /// `config` document `node-get config` answers with. A page stays bytes here, because
        /// only the reassembled whole is a configuration.
        body: Vec<u8>,
    },
    /// `5 history_frame` — kept verbatim; the history surface is future work (§5).
    HistoryFrame(Vec<u8>),
    /// `6 error` — `1 code`, `2 fault_field` (`group×100 + tag`), `3 detail` (ASCII).
    Error(Map<String, Value>),
    /// `7 w1_scan` — the discovered ROMs, hex.
    W1Scan(Vec<String>),
    /// `8 sample` — a fresh Telemetry, NFC only. Kept verbatim; over radio the answer is the
    /// ordinary fPort-2 uplink.
    Sample(Vec<u8>),
    /// `14 radio_state` — link state and diagnostics of the active radio, the answer to
    /// `get_radio_state` (Command 32, sticker-firmware #446). Decoded as the firmware's `ttn.js`
    /// does; paged like any answer (#425).
    RadioState(Map<String, Value>),
    /// A oneof arm this build does not know. Named, never guessed at.
    Unknown(u32),
}

impl ResponseBody {
    /// The `kind` a northbound `command` document reports.
    pub const fn kind(&self) -> &'static str {
        match self {
            ResponseBody::Ack => "ack",
            ResponseBody::Info(_) => "info",
            ResponseBody::ConfigDump { .. } => "config_dump",
            ResponseBody::HistoryFrame(_) => "history_frame",
            ResponseBody::Error(_) => "error",
            ResponseBody::W1Scan(_) => "w1_scan",
            ResponseBody::Sample(_) => "sample",
            ResponseBody::RadioState(_) => "radio_state",
            ResponseBody::Unknown(_) => "unknown",
        }
    }
}

/// One decoded fPort-85 response.
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    /// The command sequence this answers. `0` means the frame carried none, and §9.7's
    /// [`SeqAllocator`] never issues `0` precisely so that cannot match an outstanding command.
    pub seq: u32,
    pub body: ResponseBody,
    /// The envelope's `page_index`/`page_count` (fields 12/13), when they say "paged".
    pub envelope_page: Option<Paging>,
    /// The oneof submessage as it came off the wire. A paged answer is reassembled from these:
    /// every page carries a disjoint part of one message, so the pages' submessages
    /// concatenated decode as their protobuf merge ([`decode_joined`]).
    pub raw: Vec<u8>,
}

impl Response {
    /// Which page of a paged answer this frame is, or `None` for a single frame.
    ///
    /// *Amended 2026-10-14 (v2.0.0 minimum):* only the envelope numbers pages; the
    /// `ConfigDump` body's own fields 1/2 (before #425) are no longer read.
    pub fn paging(&self) -> Option<Paging> {
        self.envelope_page
    }
}

/// Decode a reassembled answer: the submessages of all its pages concatenated, which protobuf
/// reads as one merged message. Every page is a disjoint part of the whole (#425 §2.3), so
/// scalars do not collide and repeated fields — `Info.active_alarms`, `W1Scan.roms` — come out
/// in page order. `None` for a kind that is never reassembled.
pub fn decode_joined(kind: &str, joined: &[u8]) -> Option<ResponseBody> {
    match kind {
        "info" => Some(ResponseBody::Info(decode_info(joined))),
        "w1_scan" => Some(ResponseBody::W1Scan(decode_w1_scan(joined))),
        "config_dump" => Some(decode_config_dump(joined)),
        "radio_state" => Some(ResponseBody::RadioState(decode_radio_state(joined))),
        _ => None,
    }
}

/// The `AppConfigMessage` group a `ConfigDump` field carries, from the encoder's own table so
/// a group added there (`p2p`, field 8, sticker-firmware 7be011e) is named here too.
fn config_group_name(field: u32) -> Option<&'static str> {
    crate::decode::command_encode::ConfigGroup::ALL
        .into_iter()
        .find(|g| g.config_dump_field() == field)
        .map(|g| g.as_str())
}

/// Read `Response` off the wire.
///
/// Never fails: like every other decoder here it mirrors the reference's tolerance, because a
/// corrupt frame that panics the uplink task takes every other device's readings down with it.
/// A frame it cannot make sense of yields [`ResponseBody::Unknown`], which the caller publishes
/// as such.
pub fn decode_response(bytes: &[u8]) -> Response {
    let mut seq = 0u32;
    let mut body = ResponseBody::Unknown(0);
    let mut raw = Vec::new();
    let (mut page_index, mut page_count) = (0u32, 0u32);
    let mut pos = 0usize;

    while pos < bytes.len() {
        let tag = read_tag(bytes, pos);
        pos = tag.next;

        if tag.wire == 0 {
            let v = read_varint(bytes, pos);
            pos = v.next;
            match tag.field {
                1 => seq = v.value,
                // #425: the page numbers every response type shares.
                12 => page_index = v.value,
                13 => page_count = v.value,
                _ => {}
            }
            continue;
        }
        if tag.wire != 2 {
            match skip(bytes, tag.wire, pos) {
                Some(next) => pos = next,
                None => break,
            }
            continue;
        }

        let len = read_varint(bytes, pos);
        pos = len.next;
        let end = (pos + len.value as usize).min(bytes.len());
        let sub = &bytes[pos..end];
        pos = end;
        raw = sub.to_vec();

        body = match tag.field {
            2 => ResponseBody::Ack,
            3 => ResponseBody::Info(decode_info(sub)),
            4 => decode_config_dump(sub),
            5 => ResponseBody::HistoryFrame(sub.to_vec()),
            6 => ResponseBody::Error(decode_error(sub)),
            7 => ResponseBody::W1Scan(decode_w1_scan(sub)),
            8 => ResponseBody::Sample(sub.to_vec()),
            14 => ResponseBody::RadioState(decode_radio_state(sub)),
            other => ResponseBody::Unknown(other),
        };
    }

    let envelope_page = (page_count >= 2).then_some(Paging {
        index: page_index,
        count: page_count,
    });
    Response {
        seq,
        body,
        envelope_page,
        raw,
    }
}

const BUILD_TYPES: &[&str] = &["main", "dev", "custom"];
/// The link state names of `Response.radio_state` (#446); `Info` field 12 that once carried
/// the same enum is retired.
const LRW_STATES: &[&str] = &[
    "idle",
    "joining",
    "healthy",
    "warning",
    "reconnect",
    "disabled",
];

/// §5's `Info` table.
///
/// `claim_token` (field 9) is **never** read into the map: §9.7 requires it redacted from every
/// response before any northbound publish, and the strongest form of "redacted" is a decoder
/// that never puts it there. [`redact`] stays as the belt to this braces, for documents that
/// arrive from anywhere else.
fn decode_info(bytes: &[u8]) -> Map<String, Value> {
    let mut m = Map::new();
    let (mut major, mut minor, mut patch) = (None, None, None);
    let mut alarms: Vec<Value> = Vec::new();
    let mut w1_slot_state: Option<Vec<Value>> = None;
    let mut pos = 0usize;

    while pos < bytes.len() {
        let tag = read_tag(bytes, pos);
        pos = tag.next;

        if tag.wire == 2 {
            let len = read_varint(bytes, pos);
            pos = len.next;
            let end = (pos + len.value as usize).min(bytes.len());
            let sub = &bytes[pos..end];
            pos = end;
            match tag.field {
                // 9 claim_token: deliberately dropped on the floor, see the note above.
                9 => {}
                13 => {
                    m.insert("dev_eui".into(), Value::from(hex(sub)));
                }
                15 => alarms.push(Value::Object(decode_alarm_status(sub))),
                // v2.0.0 (#430): packed `app_w1_slot_state` of slots 1..4.
                19 => {
                    let states = w1_slot_state.get_or_insert_with(Vec::new);
                    let mut p = 0usize;
                    while p < sub.len() {
                        let v = read_varint(sub, p);
                        p = v.next;
                        states.push(Value::from(sensor_types::w1_slot_state_label(v.value)));
                    }
                }
                _ => {}
            }
            continue;
        }
        if tag.wire != 0 {
            match skip(bytes, tag.wire, pos) {
                Some(next) => pos = next,
                None => break,
            }
            continue;
        }

        let v = read_varint(bytes, pos);
        pos = v.next;
        let raw = v.value;
        match tag.field {
            1 => major = Some(raw),
            2 => minor = Some(raw),
            3 => patch = Some(raw),
            4 => {
                m.insert("build_type".into(), Value::from(label(BUILD_TYPES, raw)));
            }
            5 => {
                m.insert("serial_number".into(), Value::from(raw));
            }
            6 => {
                m.insert("uptime_s".into(), Value::from(raw));
            }
            7 => {
                m.insert("unix_time".into(), Value::from(raw));
            }
            8 => {
                m.insert("debug".into(), Value::Bool(raw != 0));
            }
            10 => {
                // 0 = unavailable, not 0 mV.
                m.insert(
                    "battery".into(),
                    if raw == 0 {
                        Value::Null
                    } else {
                        Value::from(raw)
                    },
                );
            }
            11 => {
                m.insert("reset_cause".into(), Value::from(raw));
            }
            // 12 (lrw_state) is retired: the link state is `get-radio-state`'s (#446).
            14 => {
                m.insert("device_status".into(), Value::from(raw));
            }
            // The unpacked spelling of field 19, one varint per slot.
            19 => w1_slot_state
                .get_or_insert_with(Vec::new)
                .push(Value::from(sensor_types::w1_slot_state_label(raw))),
            // v2.0.0 (#485 B2): the fatal error that ended the previous boot, present only
            // on the first boot after it (`ttn.js` names the reason and prints pc/lr as hex).
            20 => {
                m.insert("fault_reason".into(), Value::from(raw));
                m.insert(
                    "fault_reason_name".into(),
                    Value::from(fault_reason_name(raw)),
                );
            }
            21 | 22 => {
                let key = if tag.field == 21 {
                    "fault_pc"
                } else {
                    "fault_lr"
                };
                m.insert(key.into(), Value::from(format!("0x{raw:08x}")));
            }
            _ => {}
        }
    }

    // proto3 never puts a zero scalar on the wire, so a v1.4.0 Info carries no field 3 at all.
    // Once any component is present the triple is known, and an absent one *is* 0; requiring
    // all three derived no version from any real frame whose patch (or minor) was zero.
    if major.is_some() || minor.is_some() || patch.is_some() {
        let (a, b, c) = (major.unwrap_or(0), minor.unwrap_or(0), patch.unwrap_or(0));
        m.insert("fw_major".into(), Value::from(a));
        m.insert("fw_minor".into(), Value::from(b));
        m.insert("fw_patch".into(), Value::from(c));
        // The derived spelling §5 asks for, so a consumer never has to reassemble it.
        m.insert("fw_version".into(), Value::from(format!("{a}.{b}.{c}")));
    }
    m.insert("active_alarms".into(), Value::Array(alarms));
    if let Some(states) = w1_slot_state {
        m.insert("w1_slot_state".into(), Value::Array(states));
    }
    m
}

/// `Info.fault_reason` by name: Zephyr `K_ERR_*` 0..4, Cortex-M `K_ERR_ARM_*` 16..36
/// (`ttn.js` `_FAULT_REASONS`), `reason{N}` past the table.
fn fault_reason_name(v: u32) -> String {
    const LOW: [&str; 5] = [
        "cpu_exception",
        "spurious_irq",
        "stack_check_fail",
        "kernel_oops",
        "kernel_panic",
    ];
    const ARM: [&str; 21] = [
        "mem_fault",
        "mem_stacking",
        "mem_unstacking",
        "mem_data_access",
        "mem_instruction_access",
        "mem_fp_lazy_state",
        "bus_fault",
        "bus_stacking",
        "bus_unstacking",
        "bus_precise_data",
        "bus_imprecise_data",
        "bus_instruction",
        "bus_fp_lazy_state",
        "usage_fault",
        "div_0",
        "unaligned_access",
        "stack_overflow",
        "no_coprocessor",
        "illegal_exc_return",
        "illegal_epsr",
        "undefined_instruction",
    ];
    let name = match v {
        0..=4 => LOW.get(v as usize),
        16..=36 => ARM.get(v as usize - 16),
        _ => None,
    };
    name.map_or_else(|| format!("reason{v}"), |n| n.to_string())
}

/// One `Info.active_alarms` entry (v2.0.0, #430): `1 slot`, `3 type`, `4 channel`,
/// `5 sensor_type` (1-Wire slots only).
///
/// **`slot`, `channel` and `type` always.** proto3 leaves zero values off the wire, and zero is
/// a real value for all three: slot 0 is the motherboard and channel 0 its temperature. An
/// entry decoded only from the fields present came out as `{"type": 2}` for "onboard
/// temperature high" (bench, 2026-09-23). `sensor_type` is optional on the wire and stays
/// absent when it is. Beside the numbers, the names a Node card renders: `slot_name` (`mb`,
/// `s1`..), `channel_name` (registry name, `ch{N}` unknown, null for a sensor mismatch),
/// `type_name` — `ttn.js`'s `_decodeAlarmStatus` shape.
fn decode_alarm_status(bytes: &[u8]) -> Map<String, Value> {
    let (mut slot, mut channel, mut kind) = (0u32, 0u32, 0u32);
    let mut sensor_type: Option<u32> = None;
    let mut pos = 0usize;
    while pos < bytes.len() {
        let tag = read_tag(bytes, pos);
        pos = tag.next;
        if tag.wire != 0 {
            match skip(bytes, tag.wire, pos) {
                Some(next) => pos = next,
                None => break,
            }
            continue;
        }
        let v = read_varint(bytes, pos);
        pos = v.next;
        match tag.field {
            1 => slot = v.value,
            3 => kind = v.value,
            4 => channel = v.value,
            5 => sensor_type = Some(v.value),
            _ => {}
        }
    }
    let mut m = Map::new();
    m.insert("slot".into(), Value::from(slot));
    m.insert("channel".into(), Value::from(channel));
    m.insert("type".into(), Value::from(kind));
    if let Some(t) = sensor_type {
        m.insert("sensor_type".into(), Value::from(t));
    }
    let target = node::alarm_target(slot, channel, sensor_type, kind);
    m.insert("slot_name".into(), target["slot"].clone());
    m.insert("channel_name".into(), target["channel"].clone());
    if let Some(t) = target.get("sensor_type") {
        m.insert("sensor_type_name".into(), t.clone());
    }
    m.insert(
        "type_name".into(),
        Value::from(node::alarm_type_label(kind)),
    );
    m
}

fn decode_config_dump(bytes: &[u8]) -> ResponseBody {
    let mut groups = Vec::new();
    let mut pos = 0usize;

    while pos < bytes.len() {
        let tag = read_tag(bytes, pos);
        pos = tag.next;
        if tag.wire == 0 {
            pos = read_varint(bytes, pos).next;
            continue;
        }
        if tag.wire != 2 {
            match skip(bytes, tag.wire, pos) {
                Some(next) => pos = next,
                None => break,
            }
            continue;
        }
        let len = read_varint(bytes, pos);
        pos = len.next;
        pos = (pos + len.value as usize).min(bytes.len());
        if let Some(name) = config_group_name(tag.field) {
            groups.push(name);
        }
    }

    ResponseBody::ConfigDump {
        groups,
        body: bytes.to_vec(),
    }
}

/// §5's `Error`: `1 code`, `2 fault_field` encoded `group×100 + tag`, `3 detail` (ASCII).
/// The name of a `Response.Error.Code` (`app_config.proto`), for a log line.
pub const fn error_code_name(code: u64) -> &'static str {
    match code {
        1 => "BAD_REQUEST",
        2 => "OUT_OF_RANGE",
        3 => "NOT_READY",
        4 => "HISTORY_UNAVAILABLE",
        5 => "UNSUPPORTED_FIELD",
        6 => "PERSIST_FAILED",
        7 => "NOT_SUPPORTED",
        8 => "NOT_WRITABLE",
        9 => "BUDGET_TOO_SMALL",
        _ => "UNKNOWN",
    }
}

fn decode_error(bytes: &[u8]) -> Map<String, Value> {
    let mut m = Map::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let tag = read_tag(bytes, pos);
        pos = tag.next;
        if tag.wire == 2 {
            let len = read_varint(bytes, pos);
            pos = len.next;
            let end = (pos + len.value as usize).min(bytes.len());
            if tag.field == 3 {
                m.insert(
                    "detail".into(),
                    Value::from(String::from_utf8_lossy(&bytes[pos..end]).into_owned()),
                );
            }
            pos = end;
            continue;
        }
        if tag.wire != 0 {
            match skip(bytes, tag.wire, pos) {
                Some(next) => pos = next,
                None => break,
            }
            continue;
        }
        let v = read_varint(bytes, pos);
        pos = v.next;
        match tag.field {
            1 => {
                m.insert("code".into(), Value::from(v.value));
            }
            2 => {
                m.insert("fault_field".into(), Value::from(v.value));
                // The encoding is `group×100 + tag`, so an operator reading the log should
                // not have to do the division themselves.
                let (group, tag) = (v.value / 100, v.value % 100);
                m.insert("fault_group".into(), Value::from(group));
                m.insert("fault_tag".into(), Value::from(tag));
                // …nor look the numbers up: group 1 lorawan … 4 alarms, 5 p2p (7be011e), and
                // the key when the table knows the field. 0 is "not group-scoped".
                use crate::decode::command_encode::{key_by_wire, ConfigGroup};
                if let Some(g) = ConfigGroup::ALL
                    .into_iter()
                    .find(|g| g.fault_group() == group)
                {
                    m.insert("fault_group_name".into(), Value::from(g.as_str()));
                    if let Some(key) = key_by_wire(g, tag) {
                        m.insert("fault_key".into(), Value::from(key.full_name()));
                    }
                }
            }
            _ => {}
        }
    }
    m
}

/// `Response.RadioState` (sticker-firmware #446), by field number: the name and whether the
/// field is a `sint32` (zigzag). The firmware's own `ttn.js` `_RADIO_STATE_FIELDS`, one for one.
const RADIO_STATE_FIELDS: &[(u32, &str, bool)] = &[
    (1, "state", false),
    (2, "sf", false),
    (3, "datarate", false),
    (4, "tx_power_dbm", true),
    (5, "dl_rssi", true),
    (6, "dl_snr", true),
    (7, "dl_age_s", false),
    (8, "dl_unix_time", false),
    (9, "ul_rssi", true),
    (10, "ul_snr", true),
    (11, "ul_margin", false),
    (12, "ul_gw_count", false),
    (13, "dev_addr", false),
    (14, "fcnt_up", false),
    (15, "fail_streak", false),
    (16, "join_attempts", false),
    (17, "duty_blocked_s", false),
    (18, "airtime_hour_ms", false),
    (19, "uptime_s", false),
    (20, "tx_count", false),
    (21, "rx_count", false),
    (22, "retry_count", false),
    (23, "fail_count", false),
    (24, "tx_err_count", false),
    (25, "join_count", false),
];

/// `Response.RadioState`, as `ttn.js` `_decodeRadioState` renders it: every field optional and
/// absent when not sent (a paged answer carries some of them), plus `state_name` and
/// `dev_addr_hex` beside the numbers. A field this table does not know is skipped.
fn decode_radio_state(bytes: &[u8]) -> Map<String, Value> {
    let mut m = Map::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let tag = read_tag(bytes, pos);
        pos = tag.next;
        if tag.wire != 0 {
            match skip(bytes, tag.wire, pos) {
                Some(next) => pos = next,
                None => break,
            }
            continue;
        }
        let v = read_varint(bytes, pos);
        pos = v.next;
        if let Some((_, name, signed)) = RADIO_STATE_FIELDS.iter().find(|(f, ..)| *f == tag.field) {
            let value = if *signed {
                Value::from(crate::decode::pb::zigzag(v.value))
            } else {
                Value::from(v.value)
            };
            m.insert((*name).into(), value);
        }
    }
    if let Some(state) = m.get("state").and_then(Value::as_u64) {
        let name = LRW_STATES.get(state as usize).copied().unwrap_or("unknown");
        m.insert("state_name".into(), Value::from(name));
    }
    if let Some(addr) = m.get("dev_addr").and_then(Value::as_u64) {
        m.insert("dev_addr_hex".into(), Value::from(format!("{addr:08x}")));
    }
    m
}

fn decode_w1_scan(bytes: &[u8]) -> Vec<String> {
    let mut roms = Vec::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let tag = read_tag(bytes, pos);
        pos = tag.next;
        if tag.wire != 2 {
            match skip(bytes, tag.wire, pos) {
                Some(next) => pos = next,
                None => break,
            }
            continue;
        }
        let len = read_varint(bytes, pos);
        pos = len.next;
        let end = (pos + len.value as usize).min(bytes.len());
        if tag.field == 1 {
            roms.push(hex(&bytes[pos..end]));
        }
        pos = end;
    }
    roms
}

fn label(table: &[&str], idx: u32) -> String {
    table
        .get(idx as usize)
        .map(|s| (*s).to_string())
        .unwrap_or_else(|| idx.to_string())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #96: the page numbers of sticker-firmware #425 ride the envelope (fields 12/13) for every
    /// response type. `a` = `0x60` is field 12, `0x68` field 13.
    #[test]
    fn the_envelope_pages_every_response_kind() {
        // seq 3, info{fw_major 1, fw_minor 5}, page_index 1, page_count 2.
        let r = decode_response(&[
            0x08, 0x03, 0x1a, 0x04, 0x08, 0x01, 0x10, 0x05, 0x60, 0x01, 0x68, 0x02,
        ]);
        assert_eq!(r.seq, 3);
        assert_eq!(r.paging(), Some(Paging { index: 1, count: 2 }));
        assert_eq!(
            r.raw,
            vec![0x08, 0x01, 0x10, 0x05],
            "the submessage, verbatim"
        );

        // `page_count` 1 (or absent) is a single frame.
        let single = decode_response(&[0x08, 0x03, 0x1a, 0x02, 0x08, 0x01, 0x68, 0x01]);
        assert_eq!(single.paging(), None);
    }

    /// The pre-#425 `ConfigDump` body numbering (fields 1/2) is no longer read: the envelope
    /// alone numbers pages.
    #[test]
    fn a_config_dump_pages_by_the_envelope_only() {
        // seq 5, config_dump{page_index 1, page_count 3}.
        let r = decode_response(&[0x08, 0x05, 0x22, 0x04, 0x08, 0x01, 0x10, 0x03]);
        assert_eq!(r.paging(), None);
        let enveloped = decode_response(&[
            0x08, 0x05, 0x22, 0x04, 0x08, 0x01, 0x10, 0x03, 0x60, 0x00, 0x68, 0x02,
        ]);
        assert_eq!(enveloped.paging(), Some(Paging { index: 0, count: 2 }));
    }

    /// Joined pages decode as their protobuf merge: disjoint scalars side by side, repeated
    /// fields in page order.
    #[test]
    fn joined_info_pages_merge_into_one_answer() {
        let page0 = [0x08u8, 0x01, 0x10, 0x05]; // fw 1.5.(0)
        let mut page1 = vec![0x50u8, 0xb5, 0x19]; // battery 3253
        page1.extend([0x7a, 0x06, 0x08, 0x01, 0x10, 0x00, 0x18, 0x02]); // active_alarms[0]
        let joined: Vec<u8> = page0.iter().chain(page1.iter()).copied().collect();
        let Some(ResponseBody::Info(info)) = decode_joined("info", &joined) else {
            panic!("an info");
        };
        assert_eq!(info["fw_version"], "1.5.0");
        assert_eq!(info["battery"], 3253);
        assert_eq!(info["active_alarms"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn joined_w1_scan_pages_keep_their_rom_order() {
        let rom = |b: u8| {
            let mut v = vec![0x0au8, 0x08];
            v.extend([0x28, b, 0, 0, 0, 0, 0, 0x01]);
            v
        };
        let joined: Vec<u8> = rom(0xaa).into_iter().chain(rom(0xbb)).collect();
        let Some(ResponseBody::W1Scan(roms)) = decode_joined("w1_scan", &joined) else {
            panic!("a scan");
        };
        assert_eq!(roms, vec!["28aa000000000001", "28bb000000000001"]);
        assert_eq!(decode_joined("history_frame", &joined), None);
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    /// The exact `get-info` response the 2026-09-15 bench captured, byte for byte.
    ///
    /// Defect D9a was recorded there as `node-send get-info` completing while
    /// `node-get --radio p2p … device-info` still answered `NotFound/enrollment-record`, with the
    /// `command` document showing `kind=info` and no "cache write failed" line in the log.
    /// Reading the source could not reproduce it — the path from `decode_command_frame` through
    /// `cd.cache` to `merge_write` is shared with LoRaWAN and covered — so the bytes are pinned
    /// instead. If the frame decodes to `Info` and fills the slot, the fault was not in this
    /// code at the version the bench ran.
    #[test]
    fn the_bench_get_info_response_decodes_to_info() {
        let frame = unhex("0108011a1908011004200228918b80870830a60c400150b91a5803708010");
        assert_eq!(
            frame[0],
            crate::decode::APP_PROTO_VERSION,
            "the captured frame opens with the version byte decode_command_frame strips"
        );
        let r = decode_response(&frame[1..]);
        assert_eq!(r.seq, 1, "the seq node-send allocated");
        match &r.body {
            ResponseBody::Info(m) => assert!(!m.is_empty(), "§5's Info table, decoded"),
            other => panic!("expected ResponseBody::Info, got {other:?}"),
        }
    }

    /// The commands that must never travel over a radio link.
    #[test]
    fn nfc_only_commands_are_refused_over_radio() {
        for tag in [
            CommandTag::FactoryReset,
            CommandTag::SetSecretKey,
            CommandTag::ReqHistoryPage,
        ] {
            for t in [Transport::Lorawan, Transport::P2p] {
                let err = check_radio_policy(tag, t).expect_err("must be refused");
                assert_eq!(err.kind(), Kind::InvalidArgument, "{tag:?}");
                assert!(err.message().contains("refused over radio"));
            }
        }
    }

    #[test]
    fn the_ordinary_commands_are_allowed() {
        for tag in [
            CommandTag::Reboot,
            CommandTag::ClockSync,
            CommandTag::ForceSend,
            CommandTag::GetInfo,
            CommandTag::W1Scan,
        ] {
            assert!(
                check_radio_policy(tag, Transport::Lorawan).is_ok(),
                "{tag:?}"
            );
        }
    }

    /// The tag numbers are protobuf field numbers, so they are wire contract.
    #[test]
    fn command_tags_are_the_documented_wire_numbers() {
        assert_eq!(CommandTag::SetParam as u32, 2);
        assert_eq!(CommandTag::GetInfo as u32, 4);
        assert_eq!(CommandTag::Reboot as u32, 7);
        assert_eq!(CommandTag::ResetCounters as u32, 10);
        assert_eq!(CommandTag::ClockSync as u32, 12);
        assert_eq!(CommandTag::FactoryReset as u32, 23);
        assert_eq!(CommandTag::SetSecretKey as u32, 24);
        assert_eq!(CommandTag::GetSettings as u32, 31);
    }

    #[test]
    fn an_empty_reset_counters_is_refused_before_encode() {
        let err = check_reset_counters(CounterSelection::default(), Transport::Lorawan)
            .expect_err("empty must be refused");
        assert_eq!(err.kind(), Kind::InvalidArgument);
        assert!(err.message().contains("every counter"));

        let ok = CounterSelection {
            hall_left: true,
            ..Default::default()
        };
        assert!(check_reset_counters(ok, Transport::Lorawan).is_ok());
    }

    #[test]
    fn clock_sync_epochs_are_range_checked() {
        // An empty clock_sync is a network re-sync and always valid.
        assert!(check_clock_sync_epoch(None, Transport::Lorawan).is_ok());
        assert!(check_clock_sync_epoch(Some(1_787_227_200), Transport::Lorawan).is_ok());

        for bad in [0, -1, 1_000_000_000, 9_999_999_999] {
            let err = check_clock_sync_epoch(Some(bad), Transport::Lorawan)
                .expect_err("implausible epoch");
            assert_eq!(err.kind(), Kind::InvalidArgument, "epoch {bad}");
        }
    }

    #[test]
    fn the_claim_token_is_redacted_before_publish() {
        let mut data: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(r#"{"seq":1,"info":{"claim_token":"deadbeef","fw":"1.4.0"}}"#)
                .expect("parses");
        redact(&mut data);
        let rendered = serde_json::to_string(&data).expect("serializes");
        assert!(!rendered.contains("deadbeef"), "leaked: {rendered}");
        assert!(rendered.contains("1.4.0"), "the rest of Info survives");
    }

    /// The budget is per transport, never shared: using LoRaWAN's DR0 cap for P2P wastes
    /// payload, and using P2P's for LoRaWAN silently truncates.
    #[test]
    fn the_wire_budget_differs_per_transport() {
        assert_eq!(wire_budget(Transport::Lorawan), 51);
        assert_eq!(wire_budget(Transport::P2p), 72);
        assert_ne!(wire_budget(Transport::Lorawan), wire_budget(Transport::P2p));
    }

    #[test]
    fn sequences_are_monotonic_and_never_zero() {
        let mut a = SeqAllocator::default();
        assert_eq!(a.allocate(), 1);
        assert_eq!(a.allocate(), 2);
        // Wrapping must skip 0, so a response with no seq cannot match an outstanding one.
        let mut a = SeqAllocator { next: u32::MAX };
        assert_eq!(a.allocate(), 1);
    }

    #[test]
    fn pages_reassemble_out_of_order() {
        let mut r = PageReassembler::new(Transport::Lorawan);
        assert_eq!(r.accept(2, 3, b"cc").expect("accepts"), None);
        assert_eq!(r.accept(0, 3, b"aa").expect("accepts"), None);
        let done = r.accept(1, 3, b"bb").expect("accepts").expect("complete");
        assert_eq!(done, b"aabbcc".to_vec());
    }

    /// Matched once: a retransmitted page must not be appended twice.
    #[test]
    fn a_duplicate_page_is_discarded_not_appended() {
        let mut r = PageReassembler::new(Transport::Lorawan);
        assert_eq!(r.accept(0, 2, b"aa").expect("accepts"), None);
        assert_eq!(r.accept(0, 2, b"aa").expect("duplicate"), None);
        let done = r.accept(1, 2, b"bb").expect("accepts").expect("complete");
        assert_eq!(done, b"aabb".to_vec());
    }

    #[test]
    fn malformed_paging_is_rejected() {
        let mut r = PageReassembler::new(Transport::Lorawan);
        assert!(r.accept(0, 0, b"x").is_err(), "zero pages");

        let mut r = PageReassembler::new(Transport::Lorawan);
        assert!(r.accept(5, 3, b"x").is_err(), "index outside the range");

        let mut r = PageReassembler::new(Transport::Lorawan);
        r.accept(0, 3, b"x").expect("accepts");
        assert!(
            r.accept(1, 4, b"x").is_err(),
            "page count changed mid-reassembly"
        );
    }

    /// The overflow guard derives from the transport's budget, so the same oversized stream is
    /// refused on LoRaWAN and accepted on P2P.
    #[test]
    fn the_overflow_guard_uses_the_transports_own_budget() {
        let big = vec![0u8; 60];
        let mut lorawan = PageReassembler::new(Transport::Lorawan);
        let err = lorawan
            .accept(0, 1, &big)
            .expect_err("past a 51 B page budget");
        assert!(err.message().contains("51 B ceiling"));

        let mut p2p = PageReassembler::new(Transport::P2p);
        assert!(p2p.accept(0, 1, &big).is_ok(), "inside a 72 B budget");
    }

    #[test]
    fn cli_names_round_trip() {
        for tag in ALL.iter().copied() {
            if let Some(name) = tag.cli_name() {
                assert_eq!(CommandTag::from_cli_name(name), Some(tag), "{name}");
            }
        }
        assert_eq!(CommandTag::from_cli_name("no-such-command"), None);
        // The NFC-only ones deliberately have no CLI spelling at all.
        assert_eq!(CommandTag::FactoryReset.cli_name(), None);
        assert_eq!(CommandTag::SetSecretKey.cli_name(), None);
        assert_eq!(CommandTag::DeviceReset.cli_name(), None);
    }

    /// F6, measured on the bench 2026-08-25 against firmware v1.4.0: an fPort-85
    /// `device_reset` downlink is answered
    /// `error { code: 3, detail: "transport not allowed" }`, not `ack`. The firmware's own
    /// allow-list (`app/src/app_config.yml`, `device_reset, proto_id 8,
    /// transports: [nfc, shell]`) is the authority, and the pin wins over the plan text.
    #[test]
    fn device_reset_is_nfc_only_at_the_pinned_firmware() {
        assert_eq!(CommandTag::DeviceReset.policy(), RadioPolicy::NfcOnly);
        assert_eq!(CommandTag::from_cli_name("device-reset"), None);
        for transport in [Transport::Lorawan, Transport::P2p] {
            let err = check_radio_policy(CommandTag::DeviceReset, transport)
                .expect_err("refused over every radio transport");
            assert_eq!(err.kind(), Kind::InvalidArgument);
        }
    }

    /// F17: a retransmitted page adds nothing to the reassembly, so charging its bytes
    /// against the size ceiling turned an ordinary retransmit near the limit into a
    /// `Transport` error — a fault reported for a frame that was perfectly fine.
    #[test]
    fn a_duplicate_page_at_the_ceiling_is_discarded_not_an_error() {
        let budget = wire_budget(Transport::Lorawan);
        let mut r = PageReassembler::new(Transport::Lorawan);
        let full = vec![0xAA; budget];
        assert!(r.accept(0, 2, &full).expect("first page").is_none());
        // The same page again: at the ceiling, but it contributes nothing.
        assert!(
            r.accept(0, 2, &full)
                .expect("a retransmit is not a fault")
                .is_none(),
            "the duplicate is discarded"
        );
        let joined = r
            .accept(1, 2, &full)
            .expect("second page")
            .expect("complete");
        assert_eq!(joined.len(), budget * 2, "each page counted exactly once");
    }

    /// F17: the reassembler kept its pages after handing back a completed dump, so a
    /// retransmitted final page returned the very same dump a second time.
    #[test]
    fn a_page_retransmitted_after_completion_does_not_re_return_the_dump() {
        let mut r = PageReassembler::new(Transport::Lorawan);
        assert!(r.accept(0, 2, b"aa").expect("p0").is_none());
        assert!(r.accept(1, 2, b"bb").expect("p1").is_some(), "completes");
        assert!(
            r.accept(1, 2, b"bb").expect("late retransmit").is_none(),
            "the dump is not handed back twice"
        );
    }

    /// F17: policy refusals are command-plane faults. `enrollment-record` is the serial↔dev_eui
    /// bookkeeping and has nothing to do with them.
    #[test]
    fn policy_and_reassembly_faults_report_the_control_socket() {
        let err =
            check_radio_policy(CommandTag::FactoryReset, Transport::Lorawan).expect_err("refused");
        assert_eq!(err.component(), Component::ControlSocket);

        let mut r = PageReassembler::new(Transport::Lorawan);
        let err = r.accept(0, 0, b"x").expect_err("zero pages is malformed");
        assert_eq!(err.component(), Component::ControlSocket);
    }

    /// The same allow-list puts these two alongside `device_reset`; they were already
    /// unreachable over radio, and this pins that they stay so.
    #[test]
    fn the_other_nfc_shell_only_commands_stay_unreachable() {
        for tag in [CommandTag::FactoryReset, CommandTag::SetSecretKey] {
            assert_eq!(tag.policy(), RadioPolicy::NfcOnly);
            assert!(tag.cli_name().is_none());
        }
    }
}

#[cfg(test)]
mod response_tests {
    use super::*;

    /// Hand-encode a length-delimited submessage: tag byte, length, body.
    ///
    /// Single-byte tag and length, so `field` must be 15 or less and the body under 128 bytes
    /// — true of every message in §5's `Response`.
    /// A protobuf varint; tags of field 16 and up take two bytes.
    fn varint(mut v: u32) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return out;
            }
            out.push(b | 0x80);
        }
    }

    fn sub(field: u32, body: &[u8]) -> Vec<u8> {
        let mut out = varint((field << 3) | 2);
        out.push(body.len() as u8);
        out.extend_from_slice(body);
        out
    }

    fn varint_field(field: u32, value: u32) -> Vec<u8> {
        let mut out = varint(field << 3);
        let mut v = value;
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
        out
    }

    /// `seq 7` + `ack{}` — the shortest response there is, and the correlation half of §9.7.
    #[test]
    fn an_ack_carries_only_its_sequence() {
        let mut frame = varint_field(1, 7);
        frame.extend(sub(2, &[]));
        let r = decode_response(&frame);
        assert_eq!(r.seq, 7);
        assert_eq!(r.body, ResponseBody::Ack);
        assert_eq!(r.body.kind(), "ack");
    }

    #[test]
    fn info_decodes_the_documented_fields_and_derives_the_version() {
        let mut info = Vec::new();
        info.extend(varint_field(1, 1)); // fw_major
        info.extend(varint_field(2, 4)); // fw_minor
        info.extend(varint_field(3, 0)); // fw_patch
        info.extend(varint_field(4, 0)); // build_type MAIN
        info.extend(varint_field(5, 1234)); // serial_number
        info.extend(varint_field(6, 86_400)); // uptime_s
        info.extend(varint_field(8, 0)); // debug = false
        info.extend(varint_field(10, 3_320)); // battery mV
        info.extend(varint_field(11, 8)); // reset_cause bitmask
        info.extend(varint_field(14, 0)); // device_status
        info.extend(sub(15, &{
            let mut a = varint_field(1, 2); // slot s2
            a.extend(varint_field(3, 3)); // type trigger
            a.extend(varint_field(4, 5)); // channel 5: tilt
            a.extend(varint_field(5, 3)); // sensor_type machine-probe
            a
        }));
        // w1_slot_state, packed: s1 ok, s2 ok, s3 absent, s4 none.
        info.extend(sub(19, &[1, 1, 2, 0]));
        info.extend(varint_field(12, 2)); // retired lrw_state

        let mut frame = varint_field(1, 3);
        frame.extend(sub(3, &info));

        let r = decode_response(&frame);
        assert_eq!(r.seq, 3);
        let ResponseBody::Info(m) = r.body else {
            panic!("info body")
        };
        assert_eq!(m.get("fw_version"), Some(&Value::from("1.4.0")));
        assert_eq!(m.get("build_type"), Some(&Value::from("main")));
        assert_eq!(m.get("serial_number"), Some(&Value::from(1234)));
        assert_eq!(m.get("uptime_s"), Some(&Value::from(86_400)));
        assert_eq!(m.get("debug"), Some(&Value::Bool(false)));
        assert_eq!(m.get("battery"), Some(&Value::from(3320)));
        assert_eq!(m.get("reset_cause"), Some(&Value::from(8)));
        let Some(Value::Array(alarms)) = m.get("active_alarms") else {
            panic!("active_alarms is always present, empty when all is well")
        };
        assert_eq!(alarms.len(), 1);
        assert_eq!(
            alarms[0],
            serde_json::json!({
                "slot": 2, "channel": 5, "type": 3, "sensor_type": 3,
                "slot_name": "s2", "channel_name": "tilt",
                "sensor_type_name": "machine-probe", "type_name": "trigger",
            })
        );
        assert_eq!(
            m.get("w1_slot_state"),
            Some(&serde_json::json!(["ok", "ok", "absent", "none"]))
        );
        assert!(!m.contains_key("lrw_state"), "retired");
    }

    /// Field 19 unpacked is the same list, and a state past the table is named.
    #[test]
    fn w1_slot_state_unpacked_and_unknown() {
        let mut info = varint_field(19, 4);
        info.extend(varint_field(19, 7));
        let ResponseBody::Info(m) = decode_response(&sub(3, &info)).body else {
            panic!("info body")
        };
        assert_eq!(
            m["w1_slot_state"],
            serde_json::json!(["mismatch", "state7"])
        );
        let ResponseBody::Info(m) = decode_response(&sub(3, &varint_field(6, 1))).body else {
            panic!("info body")
        };
        assert!(!m.contains_key("w1_slot_state"), "absent from the wire");
    }

    /// v2.0.0 Info 20..22: the fault record, as `ttn.js` decodes it.
    #[test]
    fn info_fault_record_decodes_like_ttn_js() {
        let mut info = varint_field(20, 32);
        info.extend(varint_field(21, 0x0800_1234));
        info.extend(varint_field(22, 0xffff_fff9));
        let ResponseBody::Info(m) = decode_response(&sub(3, &info)).body else {
            panic!("info body")
        };
        assert_eq!(m["fault_reason"], 32);
        assert_eq!(m["fault_reason_name"], "stack_overflow");
        assert_eq!(m["fault_pc"], "0x08001234");
        assert_eq!(m["fault_lr"], "0xfffffff9");
        assert_eq!(fault_reason_name(3), "kernel_oops");
        assert_eq!(fault_reason_name(9), "reason9");
    }

    /// The autonomous boot `Info` a v1.4.0 STICKER sent on 2026-09-22 (GitLab #82 note), version
    /// byte stripped. proto3 left `fw_patch = 0` off the wire, so field 3 is simply absent — the
    /// version must still come out whole.
    #[test]
    fn a_real_info_without_a_zero_patch_still_derives_the_version() {
        let frame = hexbytes("1a15080110042002301238cfa4cbd506400150821b5803");
        let r = decode_response(&frame);
        assert_eq!(r.seq, 0, "the boot frame is unsolicited");
        let ResponseBody::Info(m) = r.body else {
            panic!("info body")
        };
        assert_eq!(m.get("fw_version"), Some(&Value::from("1.4.0")));
        assert_eq!(m.get("fw_patch"), Some(&Value::from(0)));
        assert_eq!(m.get("build_type"), Some(&Value::from("custom")));
        assert_eq!(m.get("uptime_s"), Some(&Value::from(18)));
        assert_eq!(m.get("unix_time"), Some(&Value::from(1_790_104_143u32)));
        assert_eq!(m.get("debug"), Some(&Value::Bool(true)));
        assert_eq!(m.get("battery"), Some(&Value::from(3458)));
        assert_eq!(m.get("reset_cause"), Some(&Value::from(3)));
    }

    /// `{"type": 2}` on the bench (2026-09-23) was "onboard temperature high" with source 0 and
    /// quantity 0 left off the wire. Every entry carries slot, channel and type (v2.0.0).
    #[test]
    fn an_active_alarm_with_zero_fields_keeps_all_three_keys() {
        let mut info = Vec::new();
        info.extend(sub(15, &varint_field(3, 2))); // onboard temperature, high
        info.extend(sub(15, &{
            let mut a = varint_field(4, 1); // mb humidity
            a.extend(varint_field(3, 2));
            a
        }));
        // A sensor mismatch on s1: no channel, the expected type.
        info.extend(sub(15, &{
            let mut a = varint_field(1, 1);
            a.extend(varint_field(3, 5));
            a.extend(varint_field(5, 2));
            a
        }));
        let ResponseBody::Info(m) = decode_response(&sub(3, &info)).body else {
            panic!("info body")
        };
        let alarms = m["active_alarms"].as_array().expect("array");
        assert_eq!(
            alarms[0],
            serde_json::json!({"slot": 0, "channel": 0, "type": 2, "slot_name": "mb",
                               "channel_name": "temperature", "type_name": "high"})
        );
        assert_eq!(
            alarms[1],
            serde_json::json!({"slot": 0, "channel": 1, "type": 2, "slot_name": "mb",
                               "channel_name": "humidity", "type_name": "high"})
        );
        assert_eq!(
            alarms[2],
            serde_json::json!({"slot": 1, "channel": 0, "type": 5, "sensor_type": 2,
                               "slot_name": "s1", "channel_name": null,
                               "sensor_type_name": "dallas", "type_name": "sensor_mismatch"})
        );
    }

    /// No version field at all is not a version of 0.0.0.
    #[test]
    fn an_info_without_any_version_field_derives_none() {
        let mut frame = Vec::new();
        frame.extend(sub(3, &varint_field(6, 60)));
        let ResponseBody::Info(m) = decode_response(&frame).body else {
            panic!("info body")
        };
        assert!(!m.contains_key("fw_version"));
        assert!(!m.contains_key("fw_major"));
    }

    fn hexbytes(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    /// §5: battery `0` means unavailable, not 0 mV. A dashboard graphing 0 V would report a
    /// dead device on every unit whose supply rail is not instrumented.
    #[test]
    fn an_unavailable_battery_is_null_rather_than_zero_volts() {
        let mut frame = varint_field(1, 1);
        frame.extend(sub(3, &varint_field(10, 0)));
        let ResponseBody::Info(m) = decode_response(&frame).body else {
            panic!("info")
        };
        assert_eq!(m.get("battery"), Some(&Value::Null));
    }

    /// §9.7's guard, in its strongest form: the decoder never puts `claim_token` in the map at
    /// all, so no publish path can be the one that forgets to redact it.
    #[test]
    fn the_claim_token_never_enters_the_decoded_map() {
        let mut info = varint_field(1, 1);
        info.extend(sub(9, &[0xde, 0xad, 0xbe, 0xef])); // claim_token
        info.extend(varint_field(5, 42)); // and a field that must survive
        let mut frame = varint_field(1, 1);
        frame.extend(sub(3, &info));

        let ResponseBody::Info(m) = decode_response(&frame).body else {
            panic!("info")
        };
        assert!(!m.contains_key("claim_token"));
        let rendered = serde_json::to_string(&m).expect("serializes");
        assert!(!rendered.contains("deadbeef"), "leaked: {rendered}");
        assert_eq!(m.get("serial_number"), Some(&Value::from(42)));
    }

    #[test]
    fn a_config_dump_page_names_its_groups_and_keeps_its_bytes() {
        let mut dump = sub(3, &[0x08, 0x01]); // lorawan group
        dump.extend(sub(4, &[0x10, 0x02])); // application group
        let mut frame = varint_field(1, 9);
        frame.extend(sub(4, &dump));

        let r = decode_response(&frame);
        assert_eq!(r.seq, 9);
        let ResponseBody::ConfigDump { groups, body } = r.body else {
            panic!("config dump")
        };
        assert_eq!(groups, vec!["lorawan", "application"]);
        assert_eq!(body, dump, "the page's bytes survive verbatim");
    }

    /// §5's `fault_field` is `group×100 + tag`, so `application.interval_report` reports 203.
    /// An operator reading a log should not have to do the division.
    #[test]
    fn an_error_splits_its_fault_field_into_group_and_tag() {
        let mut err = varint_field(1, 3); // code NOT_READY
        err.extend(varint_field(2, 203)); // fault_field
        err.extend(sub(3, b"transport not allowed"));
        let mut frame = varint_field(1, 11);
        frame.extend(sub(6, &err));

        let ResponseBody::Error(m) = decode_response(&frame).body else {
            panic!("error")
        };
        assert_eq!(m.get("code"), Some(&Value::from(3)));
        assert_eq!(m.get("fault_field"), Some(&Value::from(203)));
        assert_eq!(m.get("fault_group"), Some(&Value::from(2)));
        assert_eq!(m.get("fault_tag"), Some(&Value::from(3)));
        assert_eq!(m.get("detail"), Some(&Value::from("transport not allowed")));
        assert_eq!(m.get("fault_group_name"), Some(&Value::from("application")));
        assert_eq!(
            m.get("fault_key"),
            Some(&Value::from("application.interval_report"))
        );
    }

    /// Group 5 is `p2p` (sticker-firmware 7be011e, `Error.fault_field` note): a refused
    /// `p2p.spreading_factor` write names 502.
    #[test]
    fn an_error_in_group_5_names_the_p2p_group() {
        let mut err = varint_field(1, 8); // code NOT_WRITABLE
        err.extend(varint_field(2, 502));
        let mut frame = varint_field(1, 12);
        frame.extend(sub(6, &err));
        let ResponseBody::Error(m) = decode_response(&frame).body else {
            panic!("error")
        };
        assert_eq!(m.get("fault_group"), Some(&Value::from(5)));
        assert_eq!(m.get("fault_group_name"), Some(&Value::from("p2p")));
        assert_eq!(
            m.get("fault_key"),
            Some(&Value::from("p2p.spreading_factor"))
        );

        // Not group-scoped (0), and a group past the table: numbers only, never a guess.
        for fault in [0u32, 7, 699] {
            let mut err = varint_field(1, 1);
            err.extend(varint_field(2, fault));
            let mut frame = varint_field(1, 13);
            frame.extend(sub(6, &err));
            let ResponseBody::Error(m) = decode_response(&frame).body else {
                panic!("error")
            };
            assert!(!m.contains_key("fault_group_name"), "{fault}: {m:?}");
            assert!(!m.contains_key("fault_key"), "{fault}");
        }
    }

    /// `Response.radio_state` (field 14, sticker-firmware #446): the `sint32` fields zigzag, the
    /// state and DevAddr get their `ttn.js` companions, an unknown field is skipped and an
    /// absent one stays absent.
    #[test]
    fn a_radio_state_decodes_as_the_firmware_decoder_does() {
        let frame = hexbytes(concat!(
            "0809", "7219",   // seq 9, radio_state (25 bytes)
            "0802",   // state 2 = healthy
            "1007",   // sf 7
            "201c",   // tx_power_dbm 14 (zigzag 28)
            "28ad01", // dl_rssi -87
            "3012",   // dl_snr 9
            "48bd01", // ul_rssi -95
            "6812",   // dev_addr 0x12
            "70ac02", // fcnt_up 300
            "c80101", // join_count 1 (field 25, two-byte tag)
            "f00105", // field 30: not in the table
        ));
        let r = decode_response(&frame);
        assert_eq!(r.seq, 9);
        assert_eq!(r.body.kind(), "radio_state");
        let ResponseBody::RadioState(m) = r.body else {
            panic!("radio_state")
        };
        assert_eq!(
            Value::Object(m),
            serde_json::json!({
                "state": 2, "state_name": "healthy", "sf": 7, "tx_power_dbm": 14,
                "dl_rssi": -87, "dl_snr": 9, "ul_rssi": -95,
                "dev_addr": 18, "dev_addr_hex": "00000012",
                "fcnt_up": 300, "join_count": 1,
            })
        );
        // Two pages of one answer merge like any paged answer (#425).
        let joined = hexbytes("0803100a1814");
        let Some(ResponseBody::RadioState(m)) = decode_joined("radio_state", &joined) else {
            panic!("joined radio_state")
        };
        assert_eq!(m["state_name"], "warning");
        assert_eq!(m["sf"], 10);
        assert_eq!(m["datarate"], 20);
    }

    #[test]
    fn a_w1_scan_reports_its_roms_as_hex() {
        let mut scan = sub(1, &[0x28, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x11]);
        scan.extend(sub(1, &[0x28, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07]));
        let mut frame = varint_field(1, 2);
        frame.extend(sub(7, &scan));

        let ResponseBody::W1Scan(roms) = decode_response(&frame).body else {
            panic!("w1 scan")
        };
        assert_eq!(
            roms,
            vec![
                "28aabbccddeeff11".to_string(),
                "2801020304050607".to_string()
            ]
        );
    }

    /// A oneof arm this build does not know is named, not guessed at — and a corrupt frame
    /// must never panic the uplink task, which would take every other device down with it.
    #[test]
    fn an_unknown_arm_is_named_and_garbage_never_panics() {
        let mut frame = varint_field(1, 4);
        frame.extend(sub(15, &[0x01, 0x02]));
        let r = decode_response(&frame);
        assert_eq!(r.seq, 4);
        assert_eq!(r.body, ResponseBody::Unknown(15));
        assert_eq!(r.body.kind(), "unknown");

        for garbage in [
            vec![],
            vec![0xff],
            vec![0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            vec![0x1a, 0x7f, 0x08],
            vec![0x0b, 0x08],
        ] {
            let _ = decode_response(&garbage);
        }
    }

    /// A response carrying no seq decodes as `0`, and [`SeqAllocator`] never issues `0` — so
    /// it cannot be mistaken for an answer to an outstanding command.
    #[test]
    fn a_response_without_a_sequence_cannot_match_an_outstanding_command() {
        let r = decode_response(&sub(2, &[]));
        assert_eq!(r.seq, 0);
        let mut a = SeqAllocator::default();
        for _ in 0..4 {
            assert_ne!(a.allocate(), 0);
        }
    }

    /// PN-4: the P2P central reads the command back out of a queued body to know whether a
    /// `Response` will retire it.
    #[test]
    fn a_downlink_body_names_its_command() {
        use crate::decode::command_encode::Downlink;
        for (downlink, tag) in [
            (Downlink::ForceSend, CommandTag::ForceSend),
            (Downlink::Reboot, CommandTag::Reboot),
            (
                Downlink::ClockSync { unix_time: None },
                CommandTag::ClockSync,
            ),
            (Downlink::LrwJoin, CommandTag::LrwJoin),
        ] {
            for seq in [0, 15, 300, 70_000] {
                let body = downlink.encode(seq, Transport::P2p).unwrap();
                assert_eq!(command_tag_of(&body), Some(tag), "{downlink:?} seq {seq}");
            }
        }
        // The bench's force_send, seq 15: 4 B on the air.
        assert_eq!(
            command_tag_of(&[0x08, 0x0f, 0x4a, 0x00]),
            Some(CommandTag::ForceSend)
        );
        assert_eq!(command_tag_of(&[]), None);
        assert_eq!(
            command_tag_of(&[0x0a, 0x01, 0x00]),
            None,
            "field 1 not a varint"
        );
        assert!(!CommandTag::ForceSend.answers_on_the_command_port());
        assert!(!CommandTag::Sample.answers_on_the_command_port());
        assert!(CommandTag::Reboot.answers_on_the_command_port());
        assert!(CommandTag::ReqHistory.answers_on_the_command_port());
    }
}
