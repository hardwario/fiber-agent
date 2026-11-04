//! Shared types for the alarm engine: wire-facing enums, engine inputs, and
//! the `AlarmAction` vocabulary the reducers in [`crate::alarm_engine`]
//! return. No I/O, no storage/MQTT crate dependency — see the crate-level
//! doc for the layering rule this crate must respect.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------
// Severity / alarm type / wire state — mirror plan/v1/viewer/rest.md's
// `AlarmEventDB`/`AlarmType`/`AlarmSeverity` and mqtt.md's alarm-state
// wire strings byte-for-byte, so the bin's `serde_json` (de)serialization
// of stored/wire values needs no manual string mapping.
// ---------------------------------------------------------------------

/// `AlarmEventDB.severity` (rest.md): `warning | critical`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warning,
    Critical,
}

impl Severity {
    /// Ordering helper: `true` when `self` is a strictly higher severity
    /// than `other` (`Critical > Warning`).
    pub fn is_escalation_over(self, other: Severity) -> bool {
        self > other
    }
}

/// `AlarmEventDB.alarm_type` (rest.md): `HIGH | LOW | DISCONNECT |
/// POWER_DISCONNECT | WIFI_DISCONNECT | ETHERNET_DISCONNECT`, plus MB3's
/// `OFFLINE`, `CONCENTRATOR_MISSING`, `HUB_OFFLINE` (decisions.md #30a), the
/// radio-diagnostics pair `NORTHBRIDGE_UNREACHABLE` / `CHIRPSTACK_DOWN`
/// (`plan/control/radio/northbridge_diagnostics.md` §4, §4a),
/// `NORTHBRIDGE_UPDATE_FAILED` (`northbridge_update.md` §3.5),
/// `SOUTHBRIDGE_UPDATE_FAILED` (`plan/system/southbridge_update.md` §3.7), and the
/// portal's `CONFIG_NOT_DELIVERED` (PF-3) and `CALIBRATION`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AlarmType {
    High,
    Low,
    Disconnect,
    PowerDisconnect,
    WifiDisconnect,
    EthernetDisconnect,
    /// A per-device liveness outage: the portal's own offline detector saw
    /// this Node or Beacon go silent (plan/portal/monitoring.md §6.3, MB3).
    ///
    /// **One row per outage**, not one per edge (decision 2026-09-08): the
    /// row opens on `WentOffline` and its `cleared_at` is stamped on
    /// `CameBack`, so the pair reads exactly as a value-class alarm does and
    /// "is this probe offline right now" is the same `cleared_at IS NULL`
    /// question everywhere in the portal. There is deliberately no `Online`
    /// variant — a recovery is a clear, not a second alarm.
    ///
    /// Distinct from [`AlarmType::Disconnect`], which is the *firmware's*
    /// wired-probe verdict echoed off the wire. These two must not collide
    /// in the UI: they are different scopes measured by different things.
    Offline,
    /// The Hub is in a LoRaWAN-serving radio mode (`lorawan`/`dual`) but its
    /// RAK5146 concentrator is not running — no `/dev/rak5146`, so
    /// `chirpstack-concentratord` never started. A hardware fault (card
    /// unseated/undervoltage, issue #39), recovered by a cold power cycle.
    ConcentratorMissing,
    /// The Hub itself went offline (its retained `status` topic reads
    /// `offline`), seen as an online→offline edge by this portal process
    /// (plan/decisions.md #30a). A connectivity alarm like Wi-Fi and
    /// Ethernet: one row per outage, cleared when the Hub returns.
    HubOffline,
    /// The Hub runs LoRa P2P but its northbridge has not answered for five
    /// minutes — the UART is dead or the modem hangs (radio diagnostics §4).
    NorthbridgeUnreachable,
    /// The Hub is in a LoRaWAN-serving mode but ChirpStack has not been running
    /// for five minutes (radio diagnostics §4a).
    ChirpstackDown,
    /// The Hub spent its retry budget flashing the bundled Northbridge firmware
    /// (`plan/control/radio/northbridge_update.md` §3.4–§3.5): the Northbridge
    /// runs another image than the Hub ships, or none. Cleared once an update
    /// succeeds or a Hub reboot opens a fresh budget.
    NorthbridgeUpdateFailed,
    /// The Hub's last Southbridge firmware update failed
    /// (`plan/system/southbridge_update.md` §3.4, §3.7): `system/info`'s
    /// `southbridge.update.state` is `failed`. Cleared once a later update
    /// succeeds, or a Hub restart leaves the state anything but `failed`.
    SouthbridgeUpdateFailed,
    /// A Node did not confirm the configuration the portal sent it in time,
    /// so the Hub and the portal hold a change the Node may not
    /// (plan/portal/nodes_phase1.cs.md, dodatek 2026-09-27, PF-3). Written by
    /// the portal's send walk, never by this engine: an operator task (send
    /// again or discard), not a fault, so it leaves the Node's value and
    /// liveness state alone.
    ConfigNotDelivered,
    /// A Node's or Beacon's calibration certificate is about to lapse, or
    /// has (plan/portal/calibration.cs.md §3). Written by the portal's own
    /// reminder sweep, never by this engine: nothing on the wire carries it.
    /// It is an operational task, not a measurement or connectivity fault,
    /// so it leaves the device's value and liveness state alone.
    Calibration,
    /// A Hub has fallen behind the configuration the portal holds for it
    /// (plan/portal/config_sync.md §10, Q12): online but no sync request for
    /// 30 min, or an item the Hub failed to apply for over an hour. One row
    /// per Hub (`sync:<hub>`), written by the portal's sync sweep; a system
    /// alarm outside "Poruchy", like [`AlarmType::Calibration`].
    ConfigSync,
}

/// Notification email/webhook `source` (ws.md): `fiber | sticker | eye |
/// system`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlarmSource {
    Fiber,
    Sticker,
    Eye,
    System,
}

/// Wired-probe / device alarm-state wire values (mqtt.md `alarms/events`):
/// `NORMAL|WARNING|CRITICAL|DISCONNECTED|RECONNECTING|NEVERCONNECTED`.
/// Field-level maps (`field_alarm_states`) only ever use the first three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum WireState {
    Normal,
    Warning,
    Critical,
    Disconnected,
    Reconnecting,
    NeverConnected,
}

impl WireState {
    pub fn as_str(self) -> &'static str {
        match self {
            WireState::Normal => "NORMAL",
            WireState::Warning => "WARNING",
            WireState::Critical => "CRITICAL",
            WireState::Disconnected => "DISCONNECTED",
            WireState::Reconnecting => "RECONNECTING",
            WireState::NeverConnected => "NEVERCONNECTED",
        }
    }

    /// `Some(severity)` for the two threshold-alarm levels, `None` for
    /// `NORMAL`/`DISCONNECTED`/`RECONNECTING`/`NEVERCONNECTED` (which are
    /// not HIGH/LOW threshold conditions).
    pub fn as_severity(self) -> Option<Severity> {
        match self {
            WireState::Warning => Some(Severity::Warning),
            WireState::Critical => Some(Severity::Critical),
            _ => None,
        }
    }
}

/// Returned by [`WireState`]'s `TryFrom<&str>` when the bin's ingest layer
/// hands the engine a wire string outside the six known alarm states.
#[derive(Debug, Clone, thiserror::Error)]
#[error("unknown alarm wire state: {0:?}")]
pub struct ParseWireStateError(pub String);

impl TryFrom<&str> for WireState {
    type Error = ParseWireStateError;

    fn try_from(s: &str) -> Result<Self, Self::Error> {
        match s {
            "NORMAL" => Ok(WireState::Normal),
            "WARNING" => Ok(WireState::Warning),
            "CRITICAL" => Ok(WireState::Critical),
            "DISCONNECTED" => Ok(WireState::Disconnected),
            "RECONNECTING" => Ok(WireState::Reconnecting),
            "NEVERCONNECTED" => Ok(WireState::NeverConnected),
            _ => Err(ParseWireStateError(s.to_string())),
        }
    }
}

/// The four threshold bounds carried by both wired-sensor config
/// (`critical_low_celsius`/… in mqtt.md `config/state`) and per-field
/// LoRaWAN/EYE thresholds (`field_thresholds`) — same shape, reused for
/// both HIGH/LOW inference paths.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ThresholdBounds {
    pub critical_low: Option<f64>,
    pub warning_low: Option<f64>,
    pub warning_high: Option<f64>,
    pub critical_high: Option<f64>,
}

impl ThresholdBounds {
    /// Where `value` sits against these bounds (nodes_phase2.cs.md P1): outside a critical
    /// bound is `Critical`, else outside a warning bound `Warning`, else `Normal`. A bound
    /// that is not set watches nothing.
    pub fn classify(&self, value: f64) -> WireState {
        let below = |bound: Option<f64>| bound.is_some_and(|b| value < b);
        let above = |bound: Option<f64>| bound.is_some_and(|b| value > b);
        if below(self.critical_low) || above(self.critical_high) {
            WireState::Critical
        } else if below(self.warning_low) || above(self.warning_high) {
            WireState::Warning
        } else {
            WireState::Normal
        }
    }
}

// ---------------------------------------------------------------------
// Active-alarm lookup — the engine's only "memory": the caller (bin's I/O
// layer, backed by `portal-storage::alarm_repo`) answers "is there
// currently an active (uncleared) alarm for this (hostname, sensor_id)".
// ---------------------------------------------------------------------

/// The subset of a stored alarm row the engine needs to decide
/// dedup/severity-change/clear. `to_state` is the wire state that was
/// stored on the most recent create/update (used only by the
/// DISCONNECTED<->RECONNECTING in-place-update dedup).
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveAlarm {
    pub id: String,
    pub severity: Severity,
    /// The worst level the row reached (plan/decisions.md #24). An
    /// escalation notifies only the first time it reaches critical, so an
    /// alarm wobbling across the critical bound sends one message, not one
    /// per crossing.
    pub max_severity: Severity,
    pub alarm_type: AlarmType,
    pub to_state: Option<WireState>,
    /// The row's `triggered_at` as unix seconds, `None` when the stored
    /// string does not parse. Only the wired snapshot reconcile reads it: a
    /// snapshot must prove it was measured after the alarm opened.
    pub triggered_at_unix: Option<i64>,
}

/// Caller-provided read of "what's currently active" — the engine never
/// remembers anything across calls itself.
pub trait ActiveLookup {
    fn active(&self, hostname: &str, sensor_id: &str) -> Option<ActiveAlarm>;
}

// ---------------------------------------------------------------------
// Engine inputs
// ---------------------------------------------------------------------

/// One `{prefix}/{host}/alarms/events` transition (mqtt.md), decoded into
/// plain fields. `thresholds` is the caller's cached copy of this line's
/// six-bound config (only the warning/critical high/low bounds are used
/// here) — the engine is stateless, so HIGH/LOW inference needs it passed
/// in on every call rather than remembered.
#[derive(Debug, Clone, PartialEq)]
pub struct WiredTransition {
    pub hostname: String,
    pub line: i64,
    pub name: Option<String>,
    pub location: Option<String>,
    pub from_state: WireState,
    pub to_state: WireState,
    pub temperature: Option<f64>,
    pub thresholds: Option<ThresholdBounds>,
    /// Rare: the wire payload's own `alarm_type` field, populated only
    /// when a POWER/WIFI/ETHERNET_DISCONNECT condition is (unusually)
    /// reported over this topic instead of `system/info`. When present it
    /// bypasses HIGH/LOW threshold inference entirely.
    pub alarm_type_hint: Option<ConnectivityKind>,
    pub ts_iso: String,
}

/// One line of a `{prefix}/{host}/sensors/aggregated` publish, reduced to what
/// the wired reconcile needs (`alarm_engine::on_wired_snapshot`). `state` is
/// the window's `dominant_alarm_state`, which the Hub computes as the *most
/// severe* state of any sample in the window, so `Normal` means every counted
/// sample was NORMAL. `window_start_unix` is the payload's `period_start_ts`;
/// `window_start_iso` is the same instant as RFC 3339: the last moment the
/// probe is known NORMAL, which a reconcile clear writes into the row's
/// message. `now_iso` is the portal's clock as it reduces the publish, the
/// moment it closes the row and so the row's `cleared_at` (decisions.md #28b).
#[derive(Debug, Clone, PartialEq)]
pub struct WiredSnapshot {
    pub hostname: String,
    pub line: i64,
    pub name: Option<String>,
    pub location: Option<String>,
    pub state: WireState,
    /// Connected samples in the window. A window with none counted nothing,
    /// and its `dominant_alarm_state` then falls through to NORMAL.
    pub sample_count: i64,
    pub temperature: Option<f64>,
    pub window_start_unix: i64,
    pub window_start_iso: String,
    pub now_iso: String,
}

/// One of the three connectivity leaves in `{prefix}/{host}/system/info`
/// (mqtt.md). `has_internet` is carried for shape completeness but not
/// consumed by the alarm engine (no v1 alarm keys off it).
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectivitySnapshot {
    pub hostname: String,
    pub dc_connected: Option<bool>,
    pub wifi_connected: Option<bool>,
    pub ethernet_connected: Option<bool>,
    pub has_internet: Option<bool>,
    /// The RAK5146 concentrator's presence, but only when the Hub is supposed
    /// to have one. `Some(true)` = LoRaWAN-serving mode and the concentrator is
    /// running (clears any active alarm); `Some(false)` = LoRaWAN-serving mode
    /// and it is missing past the debounce grace (opens the alarm); `None` =
    /// `off`/`p2p` mode, unknown mode, or still inside the boot grace window —
    /// the ingest layer collapses "mode gating + grace debounce" into this one
    /// tri-state so the reducer stays as simple as the `dc_connected` path.
    pub concentrator_present: Option<bool>,
    /// The Hub's own retained `status` (plan/decisions.md #30a), not a
    /// `system/info` leaf: `Some(true)` = online (clears `sys:online`),
    /// `Some(false)` = an online→offline edge this process saw (opens it),
    /// `None` = no status in this input, or an offline this process did not
    /// see begin, which the ingest layer drops like UX-058's link edges.
    pub online: Option<bool>,
    /// The northbridge's reachability, from the Hub's `radio/northbridge`
    /// document: `Some(false)` after five minutes `unreachable` (opens),
    /// `Some(true)` once it answers or P2P is off (clears), `None` otherwise.
    pub northbridge_reachable: Option<bool>,
    /// ChirpStack's unit, from the Hub's `radio/lorawan` document: `Some(false)`
    /// after five minutes not `active` in a LoRaWAN mode (opens), `Some(true)`
    /// when active or LoRaWAN is off (clears), `None` otherwise.
    pub chirpstack_running: Option<bool>,
    /// The Northbridge firmware update, from the `update` object of the same
    /// `radio/northbridge` document: `Some(false)` when its state is `failed`
    /// (opens), `Some(true)` for any other state or no update object (clears),
    /// `None` when this input says nothing about it.
    pub northbridge_update_ok: Option<bool>,
    /// The Southbridge firmware update, from `system/info.southbridge.update`
    /// (`southbridge_update.md` §3.7): `Some(false)` when its state is
    /// `failed` (opens), `Some(true)` for any other state or no update object
    /// (clears), `None` when this input says nothing about it.
    pub southbridge_update_ok: Option<bool>,
}

/// Which connectivity leaf an alarm/notification is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConnectivityKind {
    Power,
    Wifi,
    Ethernet,
    Concentrator,
    Online,
    Northbridge,
    Chirpstack,
    NorthbridgeUpdate,
    SouthbridgeUpdate,
}

impl ConnectivityKind {
    /// `sys:power|sys:wifi|sys:ethernet|sys:concentrator|sys:online` (ws.md
    /// anti-spam key).
    pub fn sensor_id(self) -> &'static str {
        match self {
            ConnectivityKind::Power => "sys:power",
            ConnectivityKind::Wifi => "sys:wifi",
            ConnectivityKind::Ethernet => "sys:ethernet",
            ConnectivityKind::Concentrator => "sys:concentrator",
            ConnectivityKind::Online => "sys:online",
            ConnectivityKind::Northbridge => "sys:northbridge",
            ConnectivityKind::Chirpstack => "sys:chirpstack",
            ConnectivityKind::NorthbridgeUpdate => "sys:northbridge-update",
            ConnectivityKind::SouthbridgeUpdate => "sys:southbridge-update",
        }
    }

    pub fn alarm_type(self) -> AlarmType {
        match self {
            ConnectivityKind::Power => AlarmType::PowerDisconnect,
            ConnectivityKind::Wifi => AlarmType::WifiDisconnect,
            ConnectivityKind::Ethernet => AlarmType::EthernetDisconnect,
            ConnectivityKind::Concentrator => AlarmType::ConcentratorMissing,
            ConnectivityKind::Online => AlarmType::HubOffline,
            ConnectivityKind::Northbridge => AlarmType::NorthbridgeUnreachable,
            ConnectivityKind::Chirpstack => AlarmType::ChirpstackDown,
            ConnectivityKind::NorthbridgeUpdate => AlarmType::NorthbridgeUpdateFailed,
            ConnectivityKind::SouthbridgeUpdate => AlarmType::SouthbridgeUpdateFailed,
        }
    }

    /// POWER_DISCONNECT and CONCENTRATOR_MISSING are critical (a Hub in a
    /// LoRaWAN mode with no concentrator serves no LoRaWAN traffic);
    /// WIFI/ETHERNET_DISCONNECT are warning one-shots (mqtt.md `system/info`
    /// side effects). HUB_OFFLINE is critical: nothing behind the Hub is
    /// monitored while it is gone. NORTHBRIDGE_UPDATE_FAILED is a warning: the
    /// Northbridge usually still runs its previous image, and one that does
    /// not answer raises NORTHBRIDGE_UNREACHABLE (critical) on its own.
    /// SOUTHBRIDGE_UPDATE_FAILED is a warning for the same reason: the
    /// Southbridge usually keeps its previous image.
    pub fn severity(self) -> Severity {
        match self {
            ConnectivityKind::Power
            | ConnectivityKind::Concentrator
            | ConnectivityKind::Online
            | ConnectivityKind::Northbridge
            | ConnectivityKind::Chirpstack => Severity::Critical,
            ConnectivityKind::Wifi
            | ConnectivityKind::Ethernet
            | ConnectivityKind::NorthbridgeUpdate
            | ConnectivityKind::SouthbridgeUpdate => Severity::Warning,
        }
    }
}

/// The radio path a Node speaks — a STICKER is the same Node entity whether
/// it joins over LoRaWAN or LoRa P2P (plan/viewer/p2p_nodes.md §1,
/// plan/decisions.md #7); this is an axis on the entity, not a second
/// device class.
///
/// What differs between the two is a property of the carrier, asked here, never a
/// `transport == P2p` test at the call site (decision #23).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    #[default]
    Lorawan,
    P2p,
}

impl Transport {
    /// Both carriers, LoRaWAN first.
    pub const ALL: [Transport; 2] = [Transport::Lorawan, Transport::P2p];

    /// The wire spelling: control-radio's `--radio` value, the `info` document's `transport`
    /// and the `monitoring.transport.*` i18n suffix.
    pub const fn as_str(self) -> &'static str {
        match self {
            Transport::Lorawan => "lorawan",
            Transport::P2p => "p2p",
        }
    }

    /// The carrier that is not this one.
    pub const fn other(self) -> Transport {
        match self {
            Transport::Lorawan => Transport::P2p,
            Transport::P2p => Transport::Lorawan,
        }
    }

    /// Whether a join empties the Node's downlink queue (P-7, nodes_phase1.cs.md dodatek
    /// 2026-09-25). ChirpStack flushes a Node's queue when it activates (the device profiles
    /// set `flush_queue_on_activate`), so a frame queued before the join is lost and has to be
    /// sent again. The P2P central keeps the queue across a reboot and a re-join
    /// (control-radio `p2p/runtime.rs` carries it into the new session), so sending again
    /// would deliver it twice (bench 2026-09-27, 5722).
    pub const fn join_flushes_queue(self) -> bool {
        match self {
            Transport::Lorawan => true,
            Transport::P2p => false,
        }
    }
}

/// The `sensor_id` tag every Node alarm row starts with, on either carrier:
/// `lrw:{node_key}:{…}`. One constant, not derived from [`Transport`]: the stored rows, the
/// probe windows, the notifier and external webhook receivers all read `lrw:`, and renaming it
/// would need a row migration (lrw_p2p_unification.cs.md §3, krok 4).
pub const NODE_SENSOR_ID_TAG: &str = "lrw";

/// `lrw:{node_key}:` — the prefix every alarm of one Node shares, whatever its carrier.
pub fn node_sensor_prefix(node_key: &str) -> String {
    format!("{NODE_SENSOR_ID_TAG}:{node_key}:")
}

/// Whether `sensor_id` belongs to a Node alarm ([`node_sensor_prefix`]).
pub fn is_node_sensor_id(sensor_id: &str) -> bool {
    sensor_id
        .strip_prefix(NODE_SENSOR_ID_TAG)
        .is_some_and(|rest| rest.starts_with(':'))
}

/// LoRaWAN join mode — meaningful only when [`Transport::Lorawan`]
/// (plan/decisions.md #8, reusing v1's `AddNodeRequest.mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Activation {
    Otaa,
    Abp,
}

/// LoRa P2P session lifecycle (plan/viewer/p2p_nodes.md §2/§4) — `p2p` only.
/// Read from the retained `info` document the P2P central publishes, whose
/// spellings these are (`control-radio` `p2p::session::SessionState`);
/// `Gone` is the portal's own word for "tombstoned after a detach"
/// (devices.node.md §6) and never arrives on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Joined,
    /// A `Detach` is owed to the node and is delivered in its next RX1 window.
    DetachPending,
    /// A `RejoinRequest` is owed; the session is still live meanwhile.
    RejoinPending,
    Stale,
    Gone,
}

/// Where a set of per-field alarm states came from — picks the
/// `lrw:{node_key}:{Label}` (a Node, on either carrier: [`node_sensor_prefix`]) vs
/// `eye:{MAC}:{Label}` sensor_id convention and the `sticker`/`eye` notification source.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FieldSource {
    Node { node_key: String },
    Beacon { mac: String },
}

/// One `lorawan/sensors` or `eye/sensors` publish's per-field alarm view
/// (mqtt.md). `states`/`labels`/`values`/`thresholds` are all keyed by the
/// wire field name (e.g. `temperature`); `labels` supplies the
/// human/display label used in the `lrw:`/`eye:` sensor_id.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldStates {
    pub hostname: String,
    pub source: FieldSource,
    pub states: HashMap<String, WireState>,
    pub labels: HashMap<String, String>,
    pub values: HashMap<String, f64>,
    pub thresholds: HashMap<String, ThresholdBounds>,
}

/// `edge` on a native fPort-3 alarm event (mqtt.md `lorawan/sensors`
/// `events[].type == "alarm"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Raise,
    Clear,
}

/// One decoded native fPort-3 alarm event
/// (`{slot, source, quantity, edge, alarm_type|side, value, base_time,
/// rel_s, ts}`, mqtt.md). `side` is whichever of the wire's ambiguous
/// `alarm_type`/`side` keys was present, raw (`"high"`/`"low"`,
/// case-insensitive) — the caller resolves which key existed on the wire.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeEvent {
    pub hostname: String,
    pub node_key: String,
    pub slot: i64,
    /// The rule's source (`onboard`, `s1`…, `battery`). Only the no-data watchdog needs it for
    /// its key: its events share slot 0xFF across every source.
    pub source: Option<String>,
    pub quantity: Option<String>,
    pub edge: Edge,
    pub side: Option<String>,
    pub value: Option<f64>,
    pub base_time: i64,
    pub rel_s: i64,
    pub ts_iso: String,
}

// ---------------------------------------------------------------------
// Engine outputs
// ---------------------------------------------------------------------

/// Fields needed to open a new alarm row. Shaped after
/// `portal_storage::models::NewAlarmEvent` (rest.md `AlarmEventDB`) but
/// defined independently — this crate has no dependency on `portal-storage`;
/// mapping one to the other is the bin's ingest-layer job.
#[derive(Debug, Clone, PartialEq)]
pub struct NewAlarm {
    pub hostname: String,
    pub sensor_id: String,
    pub sensor_name: Option<String>,
    pub alarm_type: AlarmType,
    pub severity: Severity,
    pub from_state: Option<WireState>,
    pub to_state: Option<WireState>,
    pub message: Option<String>,
    pub value: Option<f64>,
    pub threshold: Option<f64>,
    pub triggered_at: String,
    /// Native fPort-3 idempotency key (`node_key:slot:base_time+rel_s`);
    /// `None` for every other alarm source. Lets the caller reject a
    /// redelivered raise even after the alarm it originally opened has
    /// since been cleared (a plain "no active row" check can't catch that).
    pub dedup_key: Option<String>,
}

/// Notify actions ride alongside `Open`/`Clear` — never alongside
/// `UpdateSeverity`/`UpdateStateInPlace`, since v1 has no "severity
/// changed" or "state refreshed" email/webhook template (ws.md only
/// documents `activated`/`cleared`/`acknowledged`).
#[derive(Debug, Clone, PartialEq)]
pub enum NotifyKind {
    Activated(NotifyPayload),
    Cleared(NotifyPayload),
    /// An open alarm reached critical for the first time
    /// (plan/decisions.md #26). Its payload is the row at the new level.
    Escalated(NotifyPayload),
}

/// Everything a notifier (email/webhook, ws.md) needs, gathered so the
/// bin's notify layer doesn't have to re-derive it from the action stream.
#[derive(Debug, Clone, PartialEq)]
pub struct NotifyPayload {
    pub severity: Severity,
    pub alarm_type: AlarmType,
    pub source: AlarmSource,
    pub value: Option<f64>,
    /// The crossed threshold value. Only ever populated on `Activated`
    /// (see [`ActiveAlarm`] — clears don't have it in the engine's memory).
    pub threshold: Option<f64>,
    pub message: Option<String>,
    pub hostname: String,
    pub device_label: Option<String>,
    pub sensor_name: Option<String>,
    pub sensor_id: String,
    /// Wired-probe line number, when applicable.
    pub line: Option<i64>,
    pub location: Option<String>,
    pub triggered_at: String,
}

/// One thing the caller should do in response to an input event. Multiple
/// actions from one call are meant to be applied in order (e.g. a `Clear`
/// followed by an `Open` when an alarm's *kind* changes across a single
/// transition — see `alarm_engine` module docs).
#[derive(Debug, Clone, PartialEq)]
pub enum AlarmAction {
    Open(NewAlarm),
    /// Escalation/de-escalation of an open row. `alarm_type`/`to_state`/
    /// `threshold` are re-derived for the new severity and travel with it:
    /// the stored ones describe the level the alarm opened at, so leaving
    /// them behind makes the row contradict itself (a HIGH CRITICAL alarm
    /// showing State "Warning" and the warning threshold it no longer sits
    /// at). `alarm_type` can genuinely flip — a value that crosses from
    /// below the low bound to above the high bound between two messages
    /// re-infers as the other side.
    UpdateSeverity {
        id: String,
        severity: Severity,
        alarm_type: AlarmType,
        to_state: Option<WireState>,
        value: Option<f64>,
        threshold: Option<f64>,
        message: Option<String>,
        /// The row reaches critical for the first time: its acknowledgement,
        /// if any, was given to a lesser alarm and is cleared
        /// (plan/decisions.md #28c). Notes stay; they are history.
        clears_acknowledgement: bool,
    },
    /// DISCONNECTED<->RECONNECTING dedup: refresh the one active row's
    /// `to_state` in place instead of creating a new row.
    UpdateStateInPlace {
        id: String,
        to_state: WireState,
        message: Option<String>,
    },
    Clear {
        id: String,
        cleared_at_iso: String,
        /// Written to the row's `message` when `Some`; `None` leaves it as
        /// is. Only the wired snapshot reconcile sets it, so the detail page
        /// and the cleared notification can say why the row closed.
        message: Option<String>,
    },
    Notify(NotifyKind),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P-7 as a carrier property: only a LoRaWAN join empties the queue.
    #[test]
    fn carrier_properties() {
        for carrier in Transport::ALL {
            let expected_flush = carrier == Transport::Lorawan;
            assert_eq!(carrier.join_flushes_queue(), expected_flush, "{carrier:?}");
            assert_eq!(carrier.other().other(), carrier);
            assert_ne!(carrier.other(), carrier);
        }
        assert_eq!(Transport::Lorawan.as_str(), "lorawan");
        assert_eq!(Transport::P2p.as_str(), "p2p");
    }

    /// One Node prefix, whatever the carrier; a Beacon or a lookalike tag is not a Node.
    #[test]
    fn the_node_sensor_prefix_is_one_constant() {
        assert_eq!(node_sensor_prefix("aa11"), "lrw:aa11:");
        assert!(is_node_sensor_id("lrw:aa11:Temperature"));
        assert!(!is_node_sensor_id("lrwx:aa11:Temperature"));
        assert!(!is_node_sensor_id("eye:AA:BB:Temperature"));
        assert!(!is_node_sensor_id("p2p:aa11:Temperature"));
    }
}
