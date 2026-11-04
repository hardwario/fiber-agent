//! Ingest/change event contract between the MQTT handler layer, the
//! [`crate::state::store::StateStore`], and every downstream consumer (SSE
//! hub, persistence). These types are a **contract** — wave-2 work packages
//! code directly against these names and shapes, so changes here ripple
//! wide; extend, don't rename, without checking who's depending on it.
//!
//! [`IngestEvent`] carries a fully-typed `fiber-protocol` payload in from
//! the MQTT layer. [`ChangeEvent`] is what [`StateStore::apply`] emits after
//! mutating state — coarse-grained "this rendered fragment is stale"
//! signals for the SSE hub, not a diff of what changed.

use fiber_protocol::payloads::alarm_event::AlarmEventMsg;
use fiber_protocol::payloads::config_challenge::ConfigChallengeMsg;
use fiber_protocol::payloads::config_response::ConfigResponseMsg;
use fiber_protocol::payloads::config_state::ConfigStateMsg;
use fiber_protocol::payloads::errors::DeviceErrorMsg;
use fiber_protocol::payloads::export::Probe1mData;
use fiber_protocol::payloads::eye::EyeSensorsMsg;
use fiber_protocol::payloads::eye_aggregated::EyeAggregatedMsg;
use fiber_protocol::payloads::lorawan::{
    LorawanSensorsMsg, NodeConfigMsg, NodeFullConfigMsg, NodeHistoryMsg,
};
use fiber_protocol::payloads::pair_invite::PairInviteMsg;
use fiber_protocol::payloads::pair_response::PairResponseMsg;
use fiber_protocol::payloads::sensors_aggregated::SensorsAggregatedMsg;
use fiber_protocol::payloads::standby::StandbyMsg;
use fiber_protocol::payloads::status::DeviceStatus;
use fiber_protocol::payloads::system_info::SystemInfoMsg;

use super::radio::GatewaysReport;

/// The v2 northbound envelope, reused from its producer — `control_radio`'s §7 step 3
/// `NodeReading`. Aliased rather than re-exported under its own name so the `IngestEvent`
/// variant reads like every other one (`…Msg`) while staying literally the producer's type.
pub use control_radio::decode::reading::NodeReading as NodeReadingMsg;

/// The two Node-reported state documents (`plan/control/radio/node_reported_state.md` §2),
/// reused from their producer the same way, under the same `…Msg` spelling.
pub use control_radio::decode::snapshot::{
    ConfigDocument as NodeConfigDumpMsg, DeviceInfoDocument as NodeDeviceInfoMsg,
};

/// A parsed inbound MQTT message, tagged with the hostname the topic
/// resolved to (and, for `probe_1m`, the sensor line the topic path
/// carries).
#[derive(Debug, Clone)]
pub enum IngestEvent {
    Status {
        hostname: String,
        status: DeviceStatus,
    },
    SensorsAggregated {
        hostname: String,
        payload: SensorsAggregatedMsg,
    },
    /// `sensors/summary`: one sample cycle of every line, in the
    /// `sensors/aggregated` shape. Updates the live state; stores nothing.
    SensorsLive {
        hostname: String,
        payload: SensorsAggregatedMsg,
    },
    SystemInfo {
        hostname: String,
        // Boxed: `SystemInfoMsg` is several times the size of every other payload here, and
        // unboxed it made each `IngestEvent` on the ingest channel that large
        // (clippy::large_enum_variant).
        payload: Box<SystemInfoMsg>,
    },
    AlarmEvent {
        hostname: String,
        payload: AlarmEventMsg,
    },
    Standby {
        hostname: String,
        payload: StandbyMsg,
    },
    ConfigState {
        hostname: String,
        payload: ConfigStateMsg,
    },
    Probe1m {
        hostname: String,
        line: u8,
        payload: Probe1mData,
    },
    DeviceError {
        hostname: String,
        payload: DeviceErrorMsg,
    },
    MqttConnection {
        connected: bool,
        reason: Option<String>,
    },
    /// M4-C addition: routed to [`crate::relay::CommandRelay`]'s third sink
    /// only (see `mqtt::dispatch`), never applied to `StateStore` state —
    /// these three carry no rendered fragment and no liveness signal of
    /// their own; they resolve in-flight signed commands and pairing waits.
    ConfigChallenge {
        hostname: String,
        payload: ConfigChallengeMsg,
    },
    ConfigResponse {
        hostname: String,
        payload: ConfigResponseMsg,
    },
    PairResponse {
        hostname: String,
        payload: PairResponseMsg,
    },
    /// `{prefix}/{host}/pair/invite` — the device asking this portal to pair with it, so the
    /// operator does not need the LAN (GitLab #61). Handled by its own sink, like the relay's:
    /// it mutates no state and is not telemetry.
    PairInvite {
        hostname: String,
        payload: PairInviteMsg,
    },
    /// M5-C additions below: LoRaWAN Node (STICKER) + BLE beacon (EYE)
    /// topics. `node_key` comes from the topic path and may carry wire
    /// casing — the store lowercases it before keying state.
    ///
    /// `{prefix}/{host}/lorawan/sensors` — aggregate Node telemetry.
    LorawanSensors {
        hostname: String,
        payload: LorawanSensorsMsg,
    },
    /// `…/lorawan/sensors/{dev_eui}/config` — Node device-config pages.
    /// (The agent's empty retained tombstone on this topic fails JSON
    /// decode and is dropped in routing, warning only — same as v1.)
    StickerConfig {
        hostname: String,
        node_key: String,
        payload: NodeConfigMsg,
    },
    /// `…/lorawan/sensors/{dev_eui}/info` — retained, raw payload kept
    /// verbatim (ws.md: device-info is cached as the raw dict). Routing
    /// contract: an **empty payload must arrive as
    /// `serde_json::Value::Null`** — the store treats `Null` as the
    /// tombstone that removes the cached entry.
    StickerInfo {
        hostname: String,
        node_key: String,
        payload: serde_json::Value,
    },
    /// `…/lorawan/sensors/{dev_eui}/full-config`.
    StickerFullConfig {
        hostname: String,
        node_key: String,
        payload: NodeFullConfigMsg,
    },
    /// `…/lorawan/sensors/{dev_eui}/command` — Node command results. Raw
    /// passthrough: never cached (v1 parity), re-emitted verbatim as
    /// [`ChangeEvent::NodeCommandResult`].
    StickerCommandResult {
        hostname: String,
        node_key: String,
        payload: serde_json::Value,
    },
    /// `…/lorawan/sensors/{dev_eui}/history` — ring-buffer replay frames.
    /// Row persistence is the persistence sink's job; the store only emits
    /// the transient [`ChangeEvent::NodeHistoryProgress`].
    StickerHistory {
        hostname: String,
        node_key: String,
        payload: NodeHistoryMsg,
    },
    /// `{prefix}/{host}/eye/sensors` — BLE/EYE beacon snapshot.
    EyeSensors {
        hostname: String,
        payload: EyeSensorsMsg,
    },
    /// `{prefix}/{host}/eye/aggregated` — one storage window per Beacon
    /// (`plan/control/storage_intervals.md` §5, §6). History only: row
    /// persistence is the persistence sink's job, the live view stays on
    /// `eye/sensors`.
    EyeAggregated {
        hostname: String,
        payload: EyeAggregatedMsg,
    },
    /// `{prefix}/{server}/eye/homes`, retained — the cluster's Beacon registry and each
    /// Beacon's home Hub (decision #42). `hostname` is the Hub that published it.
    EyeHomes {
        hostname: String,
        payload: fiber_protocol::payloads::eye::EyeHomesMsg,
    },
    /// `{prefix}/{host}/eye/history` — a Beacon archive download's progress
    /// or a batch of its rows (plan/portal/beacon_history.md §2.2-2.3).
    /// Persisted only; the store keeps no state for it.
    BeaconHistory {
        hostname: String,
        payload: crate::beacon_history::BeaconHistoryMsg,
    },

    // ---- WP-E: the v2 radio northbound family ---------------------------
    //
    // `proximos/{host}/nodes/{dev_eui}/{reading,alarm,info,device_info,config}`, published per node by
    // `proximos-control` (plan/control/radio/plan.md §9.4). One family for both transports:
    // the topic carries no transport level (GitLab #73), the retained `info` payload does.
    // These are **not** a second
    // spelling of `LorawanSensors` above: v1 published one array of every Node on
    // `{prefix}/{host}/lorawan/sensors`, which is banned in v2 (§9.4 — the full-array shape
    // overflowed rumqttc's packet cap and tore the client down). The v2 family is one
    // document per node per uplink, so the events below carry one node, never a list.
    //
    // The payload type is `control_radio`'s own `NodeReading`, reused rather than mirrored:
    // it is the published envelope, and a copy here would be free to drift from the producer.
    /// `…/nodes/{id}/reading` — one decoded uplink. QoS 1, not retained.
    NodeReading {
        hostname: String,
        /// The topic's identifier level, lowercased: the `dev_eui` on both transports. Same
        /// `node_key` space the M5 `Sticker*` events use, so both ingest paths address one
        /// node the same way.
        node_key: String,
        payload: Box<NodeReadingMsg>,
    },
    /// `…/nodes/{id}/alarm` — one decoded fPort-3 batch, in the same envelope.
    NodeAlarm {
        hostname: String,
        node_key: String,
        payload: Box<NodeReadingMsg>,
    },
    /// `…/nodes/{id}/info` — **retained** per-node inventory, raw payload verbatim.
    ///
    /// Routing contract, identical to [`IngestEvent::StickerInfo`]'s: an **empty retained
    /// payload arrives as `serde_json::Value::Null`** and means the node was removed
    /// (`node-remove` publishes exactly that tombstone), not "a document that failed to
    /// parse".
    ///
    /// The payload's `transport` field (`lorawan` | `p2p`) is the only place the radio path
    /// is stated — the topic no longer carries it (GitLab #73).
    NodeInventory {
        hostname: String,
        node_key: String,
        payload: serde_json::Value,
    },
    /// `…/nodes/{id}/device_info` — **retained**: the Node's last fPort-85 `Info`, what it
    /// reported about itself (node_reported_state.md §2.1). Not the `info` inventory above,
    /// which is the Hub's enrollment bookkeeping.
    ///
    /// `None` is the empty retained payload `node-remove` publishes: the entry goes.
    NodeDeviceInfo {
        hostname: String,
        node_key: String,
        payload: Option<Box<NodeDeviceInfoMsg>>,
    },
    /// `…/nodes/{id}/device_status` — **retained**: the Node's last `DevStatusAns` (battery,
    /// downlink RSSI/SNR, TX power, uptime; control-radio p2p_control_commands.md §5).
    /// `None` is the removal tombstone.
    NodeDeviceStatus {
        hostname: String,
        node_key: String,
        payload: Option<serde_json::Value>,
    },
    /// `…/nodes/{id}/config` — **retained**: the Node's last complete `ConfigDump`, rendered to
    /// `group.field` keys (§2.2). A subset of the keys in general, so the store merges it.
    /// `None` is the removal tombstone.
    NodeDeviceConfig {
        hostname: String,
        node_key: String,
        payload: Option<Box<NodeConfigDumpMsg>>,
    },
    /// `…/nodes/{id}/command` — one of the Node's fPort-85 answers, as control-radio's
    /// per-frame `command` document (`kind`, `seq`, …). Not retained. The store turns the ones
    /// that answer a command (`seq` ≠ 0) into [`ChangeEvent::NodeCommandResult`]s — the
    /// confirmation a v2 "Send to device" waits for, and the refusal an operator must see.
    NodeCommand {
        hostname: String,
        node_key: String,
        payload: serde_json::Value,
    },
    /// `…/nodes/{id}/history` — one decoded frame of a `ReqHistory` replay, in the v1
    /// `NodeHistoryMsg` shape (p2p_history_replay.md §14). Not retained. Persisted and credited
    /// like [`IngestEvent::StickerHistory`], under the Hub's canonical spelling.
    NodeHistory {
        hostname: String,
        node_key: String,
        payload: NodeHistoryMsg,
    },

    /// `radio/gateways` — the cluster-wide retained gateway report (devices.hub.md §12 A2).
    ///
    /// No hostname: the topic has no hostname level and one broker carries one report for
    /// the whole cluster; the rows attribute themselves. `None` is the empty retained
    /// payload, which means the producer withdrew the report — the store clears what it
    /// holds rather than leaving a stale table on screen.
    RadioGateways { report: Option<GatewaysReport> },

    /// `proximos/{hostname}/radio/northbridge` or `…/radio/lorawan` — the Hub's radio
    /// status documents (`plan/control/radio/northbridge_diagnostics.md` §3, §4a), kept as
    /// JSON: the portal renders their fields and never rewrites them. `None` is the empty
    /// retained payload (withdrawn).
    RadioDiagnostics {
        hostname: String,
        kind: RadioDiagKind,
        doc: Option<serde_json::Value>,
    },
}

/// Which radio status document an [`IngestEvent::RadioDiagnostics`] carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioDiagKind {
    Northbridge,
    Lorawan,
}

/// Coarse-grained "this rendered fragment is stale" signal emitted after
/// [`StateStore::apply`](crate::state::store::StateStore::apply) mutates
/// state. Consumers re-read the affected slice from the store (clone-based
/// accessors) rather than reading a diff off the event itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeEvent {
    SensorLine {
        hostname: String,
        line: u8,
    },
    System {
        hostname: String,
    },
    Power {
        hostname: String,
    },
    Network {
        hostname: String,
    },
    Storage {
        hostname: String,
    },
    Standby {
        hostname: String,
    },
    DeviceStatus {
        hostname: String,
        online: bool,
    },
    Config {
        hostname: String,
    },
    Alarm {
        hostname: String,
    },
    MqttConnection {
        connected: bool,
    },
    /// M5-C additions below. Node keys (`node_key`) are lowercase, beacon
    /// keys (`mac`) uppercase — already normalized by the store.
    ///
    /// One `lorawan/sensors` publish upserted ≥1 Node (per-Node events are
    /// suppressed in favor of the batch, v1 parity).
    NodeBatch {
        hostname: String,
    },
    /// A single Node's inventory fields changed (`config/state` upsert).
    Node {
        hostname: String,
        node_key: String,
    },
    /// Node removed by the authoritative `config/state` inventory sync, or
    /// by the v2 `NodeInventory` tombstone.
    NodeRemoved {
        hostname: String,
        node_key: String,
    },
    /// A Node the store had never seen before was inserted (B4,
    /// devices.node.md §5, §7).
    ///
    /// **A true insert, not an upsert.** Both ingest arms create a
    /// `NodeState` with `.entry().or_insert_with()`, which runs on every
    /// telemetry frame; this event fires only when that call actually
    /// inserted. The distinction is the whole value of the event: `/nodes`
    /// prepends a row on it, and prepending on every reading would grow the
    /// table without bound.
    ///
    /// `Node` (or `NodeBatch`) still fires alongside it, so a connection
    /// that has the row already repaints it rather than seeing nothing —
    /// the prepend is for the connections that do not.
    NodeAdded {
        hostname: String,
        node_key: String,
    },
    /// The Node's cached config/info/full-config slice changed (covers all
    /// three `node_configs`/`node_infos`/`node_full_configs` maps,
    /// including the info tombstone removal).
    NodeConfig {
        hostname: String,
        node_key: String,
    },
    /// Transient: Node command result, **not cached** anywhere — SSE-only
    /// passthrough (v1's `sticker_command_result`). The raw payload rides
    /// on the event because there is no state to re-read.
    NodeCommandResult {
        hostname: String,
        node_key: String,
        payload: serde_json::Value,
    },
    /// Transient: the Node joined (again), on either carrier. Its first uplink after every
    /// join is an unsolicited `Info` with `seq 0`. On a carrier whose join empties the queue
    /// (`Transport::join_flushes_queue`: LoRaWAN, where ChirpStack flushes it on activate) a
    /// send waiting on the Node re-queues what it had queued before (nodes_phase1.cs.md,
    /// dodatek 2026-09-25, P-7); on P2P the frames are still queued. `at` is the store clock.
    NodeJoined {
        hostname: String,
        node_key: String,
        at: i64,
    },
    /// Transient: one history replay frame arrived (v1's
    /// `sticker_history_progress`). Not cached; `points` is the record
    /// count of this frame.
    NodeHistoryProgress {
        hostname: String,
        node_key: String,
        frame_index: i64,
        frame_count: i64,
        points: usize,
    },
    /// One `eye/sensors` publish upserted ≥1 beacon.
    BeaconBatch {
        hostname: String,
    },
    /// A single beacon changed outside a batch (`eye/detect`,
    /// `config/state` tag upsert).
    Beacon {
        hostname: String,
        mac: String,
    },
    /// A Beacon joined the enrolled fleet: first listed in
    /// `config/state.eye_tags`, either unknown before or a discovered
    /// candidate until now. `Beacon` still fires alongside it; this one is
    /// for the pages that have no card to swap it into yet.
    BeaconAdded {
        hostname: String,
        mac: String,
    },
    /// A beacon left the fleet: a discovered-only candidate absent from an
    /// `eye/sensors` snapshot, or an enrolled Beacon absent from
    /// `config/state.eye_tags`.
    BeaconRemoved {
        hostname: String,
        mac: String,
        /// Which of the two it was. The row is gone from the store by the
        /// time `sse.rs` sees this, so the answer has to ride the event —
        /// and the two need different frames: a candidate lives only in the
        /// scan panel, which its `BeaconBatch` companion already repaints,
        /// while an enrolled Beacon is a row on `/beacons` and on the group
        /// pages, which have no fragment for a disappearing card.
        discovered: bool,
    },
    /// MB2: the sensor-group arrangement changed — a group was created,
    /// renamed, deleted or reordered (`group_routes`' four mutations).
    ///
    /// **Deliberately coarse, and the only new variant MB2 adds.** Every
    /// other monitoring repaint rides an event that already fires on the
    /// reading itself; groups have no such signal, and any group mutation
    /// reshuffles whole sections rather than one row, so there is nothing
    /// smaller worth sending. `sse.rs` maps it to the page-level `refresh`
    /// hint — the acting session reloads through its own redirect anyway,
    /// the other open tabs reload through this.
    ///
    /// Carries no hostname: groups are a portal-side arrangement, not a
    /// device's state, and `refresh` reloads the whole page regardless.
    Groups,
    /// One Hub's `system/info.lorawan` block changed (devices.hub.md §12 A) — the service
    /// health chips and the local gateway EUI on the Hub page's "Radio & gateways" card.
    /// Emitted only on an actual change: `system/info` repeats every interval and the block
    /// is near-static between them.
    Radio {
        hostname: String,
    },
    /// The cluster-wide `radio/gateways` report changed (devices.hub.md §12 A2). No
    /// hostname, for the same reason [`IngestEvent::RadioGateways`] has none.
    RadioGateways,
}

impl ChangeEvent {
    /// The Hub this change is about, mutable — `None` for the fleet-wide events.
    ///
    /// It carries the spelling the event arrived with — lowercase since decision #21, an
    /// older agent's uppercase otherwise. The SSE hub rewrites it to the store's canonical
    /// spelling so event names meet the ids the pages render (#87).
    /// Exhaustive on purpose, so a new variant has to say which Hub it names.
    pub fn hostname_mut(&mut self) -> Option<&mut String> {
        match self {
            ChangeEvent::SensorLine { hostname, .. }
            | ChangeEvent::System { hostname }
            | ChangeEvent::Power { hostname }
            | ChangeEvent::Network { hostname }
            | ChangeEvent::Storage { hostname }
            | ChangeEvent::Standby { hostname }
            | ChangeEvent::DeviceStatus { hostname, .. }
            | ChangeEvent::Config { hostname }
            | ChangeEvent::Alarm { hostname }
            | ChangeEvent::NodeBatch { hostname }
            | ChangeEvent::Node { hostname, .. }
            | ChangeEvent::NodeRemoved { hostname, .. }
            | ChangeEvent::NodeAdded { hostname, .. }
            | ChangeEvent::NodeConfig { hostname, .. }
            | ChangeEvent::NodeCommandResult { hostname, .. }
            | ChangeEvent::NodeJoined { hostname, .. }
            | ChangeEvent::NodeHistoryProgress { hostname, .. }
            | ChangeEvent::BeaconBatch { hostname }
            | ChangeEvent::Beacon { hostname, .. }
            | ChangeEvent::BeaconAdded { hostname, .. }
            | ChangeEvent::BeaconRemoved { hostname, .. }
            | ChangeEvent::Radio { hostname } => Some(hostname),
            ChangeEvent::MqttConnection { .. }
            | ChangeEvent::Groups
            | ChangeEvent::RadioGateways => None,
        }
    }
}
