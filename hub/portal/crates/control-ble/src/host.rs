//! The host seams — `plan/control/ble/plan.md` §5 "Runtime form".
//!
//! Everything this component needs from whatever process hosts it, as traits. Part A ships
//! in-memory implementations for tests and file/no-op ones for the harness; Part B binds them to
//! `control-core`'s own subsystems, so the agent port is a dependency line rather than a rewrite.
//!
//! Two of these seams are load-bearing rather than tidy:
//!
//! - **`SubsystemControl` must be able to start the scan loop at runtime.** Without it §9.3's
//!   auto-enable is theatre: the flag reads `true` in YAML and the monitor stays dead until the next
//!   reboot. v1 cannot do this — its own code says so — which is why it is a trait and not a flag.
//! - **`StandbyGate` and `eye.enabled` are re-checked in the inner poll loop**, never once per
//!   session. Checked once, a `disable` persists the flag while a running loop keeps scanning.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub use crate::central::history::HistoryMessage;
use crate::central::state::ReportedBle;
use crate::config::{EyeSection, Sensor};
use crate::error::{Component, Error, Kind, Result};
use crate::mac::MacAddr;

/// Take a lock, recovering from poisoning instead of panicking.
///
/// A panic in one GATT handler must not make every later read of a shared buffer fail: these mutexes
/// guard snapshots and counters, not invariants a half-finished write could have broken. Panicking
/// here would turn one handler's bug into a dead scan loop — and the crate's own
/// `clippy::unwrap_used`/`expect_used` warning is there to stop exactly that reflex.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------------------------
// ConfigStore — the `eye:` fleet section: read, mutate atomically, audit
// ---------------------------------------------------------------------------------------------

/// The `eye:` fleet section. Part B binds it to control-core's `ConfigApplier`.
pub trait ConfigStore: Send + Sync {
    fn load(&self) -> Result<EyeSection>;
    /// Apply `f` to a fresh copy and persist atomically; returns the new section. A no-op mutation
    /// writes nothing — the add-and-enable case must be **one** write (AC 4).
    fn mutate(&self, f: &mut dyn FnMut(&mut EyeSection)) -> Result<EyeSection>;
    fn audit(&self, entry: &AuditEntry);
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AuditEntry {
    /// The v2 command vocabulary — `beacon-add`, `beacon-remove`, … (plan.md §9.8). Never a
    /// `*_eye_*` string.
    pub action: &'static str,
    pub mac: Option<String>,
    pub origin: Origin,
    pub detail: String,
}

/// Which of the three mutation surfaces made the change. The *effect* is identical on all three
/// (AC 4); only this field differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Cli,
    Ble,
    SignedMqtt,
}

// ---------------------------------------------------------------------------------------------
// Publisher — northbound
// ---------------------------------------------------------------------------------------------

/// Northbound: `eye/sensors`, `eye/aggregated`, `eye/detect`, `eye/history`, and the degraded state
/// (UI-4/UI-8).
pub trait Publisher: Send + Sync {
    fn publish_sensors(&self, snapshot: &SensorsSnapshot);
    /// One storage window's aggregates (`plan/control/storage_intervals.md` §5). A no-op by
    /// default, so a host that has not wired the topic still builds; the rows are stored either
    /// way.
    fn publish_aggregated(&self, _window: &AggregatedSnapshot) {}
    fn publish_detect(&self, result: &DetectResult);
    fn publish_degraded(&self, state: &DegradedState);
    /// One archive-job message on `eye/history`, not retained (beacon_history.md §2.2–2.3). Called
    /// from the dial worker and from the blocking download thread, never from the scan loop's hot
    /// path, so an implementation may wait briefly for room rather than drop a `records` batch.
    fn publish_history(&self, message: &HistoryMessage);
}

/// `eye/aggregated`: once per storage window, every Beacon that had frames in it
/// (`plan/control/storage_intervals.md` §5) — the Beacon analogue of `sensors/aggregated`.
/// The wire contract the portal parses is `fiber_protocol::payloads::eye_aggregated`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AggregatedSnapshot {
    /// RFC 3339 UTC of the window end.
    pub timestamp: String,
    pub period_start_ts: i64,
    pub period_end_ts: i64,
    pub duration_sec: i64,
    pub tags: Vec<BeaconAggregate>,
}

/// One Beacon's window: the same values as its `advertising` row in `eye_readings`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BeaconAggregate {
    pub mac: String,
    pub name: Option<String>,
    /// `{MAC}-adv-{window_start}` — the row's `message_id`, so a consumer dedups on the Hub's
    /// own identity for the window.
    pub message_id: String,
    pub window_start: i64,
    #[serde(flatten)]
    pub aggregate: WindowAggregate,
}

/// The `payload` of an `advertising` row: `{"window_s":600,"count":N,"alarm_state":…,
/// "fields":{"temperature":{"min":…,"avg":…,"max":…},…}}`. A threshold transition is stored in
/// the same shape at once (`window_s: 0`, `count: 1`, plus `transition`), so every reader of an
/// `advertising` row parses one shape.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WindowAggregate {
    pub window_s: i64,
    pub count: u64,
    /// The worst alarm level of the window's frames (NORMAL | WARNING | CRITICAL).
    pub alarm_state: String,
    /// Keyed by [`crate::central::aggregate::AGGREGATE_FIELDS`]; a field no frame carried is
    /// absent.
    pub fields: BTreeMap<String, FieldStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition: Option<AlarmTransition>,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FieldStats {
    pub min: f64,
    pub avg: f64,
    pub max: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AlarmTransition {
    pub from: String,
    pub to: String,
}

/// `EyeSensorsMsg`-compatible envelope. `discovered_capped` is the v2 addition
/// (`plan/portal/control.ble.md` §5): true while the discovered set sits at `discovered_max`,
/// necessarily false when `tags` is empty. An empty `tags: []` is published **deliberately** —
/// membership is authoritative and the portal prunes discovered-only beacons by absence.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SensorsSnapshot {
    pub timestamp: String,
    pub tags: Vec<BeaconPayload>,
    pub discovered_capped: bool,
}

/// One Beacon on the wire. Field names and types are the v1 contract the portal parses
/// (`fiber_protocol::payloads::eye::EyeTag`) — the narrower integer types here serialize into its
/// `i64` slots unchanged.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BeaconPayload {
    pub gateway: String,
    pub mac: String,
    pub name: Option<String>,
    pub temperature_c: Option<f64>,
    pub humidity_pct: Option<u8>,
    pub battery_mv: Option<u16>,
    // Option, not bool: the wire type is fully optional, and a beacon that has never been heard
    // must not claim flag knowledge (UI-10's principle). Concrete on decode.
    pub low_battery: Option<bool>,
    pub magnet_present: Option<bool>,
    pub magnet_detected: Option<bool>,
    pub moving: Option<bool>,
    pub movement_count: Option<u16>,
    pub pitch_deg: Option<i8>,
    pub roll_deg: Option<i16>,
    pub rssi: Option<i16>,
    pub last_seen_ts: Option<i64>,
    pub stale: bool,
    /// one of "pending" | "provisioning" | "provisioned" | "failed" | "read_only"
    pub provisioning: String,
    /// true = heard but not enrolled; false = enrolled (the portal also reads absent/null as
    /// enrolled).
    pub discovered: bool,
    pub is_en12830: Option<bool>,
    pub field_alarm_states: BTreeMap<String, String>,
    pub alarm_state: String,
    /// What the beacon reported the last time its configuration was read over GATT; `null` until
    /// it has been. Additive to the v1 set — the portal's parser ignores keys it does not know.
    pub ble_config: Option<BleConfigPayload>,
}

/// A read-back configuration on the wire (central.md §5, amended 2026-09-22 and 2026-09-23): the
/// characteristics as the beacon reports them, not as they were written. `protocol` names
/// `protocol_type`; `sensors` maps every bit of the active-sensor mask to on/off and is `null` on a
/// recorder beacon, which has no such characteristic; `read_at` is Unix seconds; `verified` says
/// whether the read closed a session that wrote the profile and matched it, or was a plain read.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BleConfigPayload {
    pub advertising_interval_ms: u16,
    pub tx_power_dbm: i8,
    pub protocol_type: u8,
    pub protocol: String,
    pub sensors: Option<BTreeMap<String, bool>>,
    pub read_at: i64,
    pub verified: bool,
    /// Whether the recorder's archive is recording, and at what cadence in minutes — from its
    /// record info, read over the raw ATT bearer (beacon_history.md §3). Absent until read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging_interval_min: Option<u16>,
}

impl From<&ReportedBle> for BleConfigPayload {
    fn from(r: &ReportedBle) -> Self {
        BleConfigPayload {
            advertising_interval_ms: r.config.advertising_interval_ms,
            tx_power_dbm: r.config.tx_power_dbm,
            protocol_type: r.config.protocol_type,
            protocol: crate::central::provision::BeaconProtocol::from_code(r.config.protocol_type)
                .as_str()
                .to_string(),
            sensors: r.config.active_sensors.map(|mask| {
                Sensor::ALL
                    .iter()
                    .map(|s| (s.as_str().to_string(), mask & s.bit() != 0))
                    .collect()
            }),
            read_at: r.read_at,
            verified: r.verified,
            recording: None,
            logging_interval_min: None,
        }
    }
}

/// The field list, in wire order — the fixture A7's test compares the serialized keys against.
pub const BEACON_PAYLOAD_FIELDS: [&str; 22] = [
    "gateway",
    "mac",
    "name",
    "temperature_c",
    "humidity_pct",
    "battery_mv",
    "low_battery",
    "magnet_present",
    "magnet_detected",
    "moving",
    "movement_count",
    "pitch_deg",
    "roll_deg",
    "rssi",
    "last_seen_ts",
    "stale",
    "provisioning",
    "discovered",
    "is_en12830",
    "field_alarm_states",
    "alarm_state",
    "ble_config",
];

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DetectResult {
    pub timestamp: String,
    pub mac: String,
    /// Tri-state: `None` = could not tell, and it never clobbers an already-known value.
    pub is_en12830: Option<bool>,
    /// "ok" | "unreachable" | "error"
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DegradedState {
    pub condition: &'static str,
    pub mac: Option<String>,
    pub active: bool,
    pub detail: String,
}

// ---------------------------------------------------------------------------------------------
// ReadingSink — durable readings
// ---------------------------------------------------------------------------------------------

/// Durable readings. Part B binds it to whichever store decision #13 lands on — which is exactly
/// why this is a trait and the crate hard-codes no database.
pub trait ReadingSink: Send + Sync {
    fn store_reading(&self, row: &ReadingRow) -> Result<()>;
    /// Newest `ts` stored for `(mac, event_type)` — the archive resume point.
    fn max_reading_ts(&self, mac: &MacAddr, event_type: &str) -> Result<Option<i64>>;
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReadingRow {
    pub mac: String,
    pub ts: i64,
    pub received_at: i64,
    /// `"{MAC}-adv-{received_at}"` or `"{MAC}-rec-{ts}"`. **The timestamp is part of the id**: the
    /// same payload seen twice is two readings, because a beacon re-broadcasts its state rather
    /// than emitting events (v1 hashed the payload alone and under-sampled a stable freezer).
    pub message_id: String,
    /// "advertising" | "recording"
    pub event_type: String,
    pub payload: serde_json::Value,
}

pub const EVENT_ADVERTISING: &str = "advertising";
pub const EVENT_RECORDING: &str = "recording";

// ---------------------------------------------------------------------------------------------
// Runtime control
// ---------------------------------------------------------------------------------------------

/// Start/stop the central scan loop at runtime. See the module header: without this, auto-enable
/// is theatre.
pub trait SubsystemControl: Send + Sync {
    /// `Ok(true)` if it started something.
    fn ensure_scanning(&self) -> Result<bool>;
    fn stop_scanning(&self);
    fn is_scanning(&self) -> bool;
    /// Forget a removed beacon's provisioning record and runtime state (central.md §5, amended
    /// 2026-09-23). A host with no central role in this process has nothing to forget.
    fn forget_beacon(&self, _mac: &MacAddr) {}
}

pub trait StandbyGate: Send + Sync {
    fn in_standby(&self) -> bool;
}

// ---------------------------------------------------------------------------------------------
// NetworkOps — FB02–FB04, FB08, FB09, FB0C
// ---------------------------------------------------------------------------------------------

/// Wi-Fi/LAN for the network-configuration characteristics. The `nmcli` exec surface stays in the
/// host (CP-1) — this component never spawns a process.
pub trait NetworkOps: Send + Sync {
    fn wifi_scan(&self) -> Result<Vec<WifiNetwork>>;
    fn wifi_connect(&self, ssid: &str, password: &str) -> WifiStatus;
    fn wifi_disconnect(&self) -> WifiStatus;
    fn wifi_status(&self) -> WifiStatus;
    fn lan_apply(&self, req: &LanConfigRequest) -> std::result::Result<(), NetworkErrorCategory>;
    fn lan_status(&self) -> LanStatus;
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WifiNetwork {
    pub ssid: String,
    pub signal: i32,
    pub security: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WifiStatus {
    pub connected: bool,
    pub ssid: String,
    pub ip_address: String,
    pub error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LanMode {
    Dhcp,
    Static,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Ipv4Config {
    pub address: String,
    pub prefix: u8,
    pub gateway: String,
    #[serde(default)]
    pub dns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LanConfigRequest {
    pub mode: LanMode,
    #[serde(default)]
    pub ipv4: Option<Ipv4Config>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LanStatus {
    pub connected: bool,
    pub link: bool,
    pub ip_address: String,
    pub mac: String,
    pub mode: String,
    pub error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkErrorCategory {
    InvalidIp,
    GatewayUnreachable,
    AuthFailed,
    NotFound,
    Timeout,
    Other,
}

// ---------------------------------------------------------------------------------------------
// Clock, device info, node enrollment
// ---------------------------------------------------------------------------------------------

pub trait SystemClock: Send + Sync {
    fn now_unix(&self) -> i64;
    fn set_time(&self, epoch: i64) -> std::result::Result<(), String>;
    fn synchronized(&self) -> bool;

    /// Seconds elapsed on a clock **nobody can set** — for rate limits, never for timestamps.
    ///
    /// [`Self::now_unix`] is the wall clock, and FB0B (`gatt/time.rs`) exists so an authenticated
    /// phone can *move* it. Anything that measures a window against the peer who is being limited
    /// must therefore not use it: FB1A's five-per-minute limit measured off `now_unix` was
    /// resettable by the very peer it bounds, with one `{"epoch": now + 61}` write
    /// (`plan/control/ble/peripheral.md` §6.5). This is the source that write cannot reach.
    ///
    /// The default is the process's own elapsed time, which is right for every real host — the
    /// value is meaningless in absolute terms and only differences are ever read, so there is no
    /// epoch to agree on. A test double overrides it to drive the two clocks apart on purpose.
    fn monotonic_secs(&self) -> u64 {
        static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        BASE.get_or_init(std::time::Instant::now)
            .elapsed()
            .as_secs()
    }
}

pub trait DeviceInfo: Send + Sync {
    fn hostname(&self) -> String;
    fn version(&self) -> String;
    fn uptime(&self) -> String;
    /// `system.device_label` — the FB0A read.
    fn label(&self) -> String;
    /// The FB0A write.
    fn set_label(&self, label: &str) -> std::result::Result<(), String>;
}

/// FB0D/FB11/FB12 — Node enrollment is the radio crate's; the host forwards.
///
/// **Three verbs, one seam.** `list` and `remove` join `add` here rather than in a second trait
/// because the inventory ask ([peripheral.md](../plan/control/ble/peripheral.md) §6.1) is about one
/// set of enrolled devices: a host that can add a Node and cannot say which Nodes it holds is
/// exactly the write-only surface FB11/FB12 exist to close.
pub trait NodeEnroll: Send + Sync {
    fn add(&self, req: &NodeAddRequest) -> NodeAddResponse;

    /// Every enrolled Node, in any order — the pager sorts.
    ///
    /// `Err` is reserved for "this Hub cannot answer the question" — an unreadable config, a seam
    /// that is not wired. It becomes an ATT-level error on FB11, because a phone must be able to
    /// tell "the Hub holds nothing" from "the Hub could not look".
    fn list(&self) -> std::result::Result<Vec<NodeEntry>, String>;

    /// Remove one Node, addressed by either identifier.
    ///
    /// There is no `Err`: a refusal is an outcome with a reason, the same contract FB10 keeps for an
    /// unknown MAC. An installer who mistypes a serial needs to read *why* nothing happened, and an
    /// ATT error carries no room to say so.
    fn remove(&self, target: &NodeRemoveTarget) -> NodeRemoveOutcome;
}

/// One enrolled Node, as the host knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeEntry {
    /// The LoRaWAN wire identity — present on every enrolled Node, and the list's key.
    pub deveui: String,
    /// The operator-facing identity printed on the device
    /// (`plan/control/radio/plan.md` §5 "One identifier"). Optional because v1 enrolments were
    /// allowed to omit it.
    pub serial: Option<String>,
    pub name: Option<String>,
}

/// Which identifier a removal was addressed by.
///
/// **Either, never both** — `plan/control/radio/plan.md` §5 "One identifier", decided 2026-08-19:
/// a caller never needs to know the transport to know how to address a node, whichever identifier it
/// happens to hold. The one-of check is the characteristic's; by the time a target reaches here it
/// is already exactly one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeRemoveTarget {
    DevEui(String),
    Serial(String),
}

impl NodeRemoveTarget {
    /// The identifier as written, for a message the installer reads back.
    pub fn value(&self) -> &str {
        match self {
            NodeRemoveTarget::DevEui(v) | NodeRemoveTarget::Serial(v) => v,
        }
    }

    /// What to call it in that message.
    pub fn label(&self) -> &'static str {
        match self {
            NodeRemoveTarget::DevEui(_) => "deveui",
            NodeRemoveTarget::Serial(_) => "serial",
        }
    }
}

/// The result of a removal — refusal included, which is why there is no `Err` beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRemoveOutcome {
    pub success: bool,
    pub message: String,
}

impl NodeRemoveOutcome {
    pub fn ok(message: impl Into<String>) -> NodeRemoveOutcome {
        NodeRemoveOutcome {
            success: true,
            message: message.into(),
        }
    }
    pub fn refused(message: impl Into<String>) -> NodeRemoveOutcome {
        NodeRemoveOutcome {
            success: false,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeAddRequest {
    pub deveui: String,
    #[serde(default)]
    pub profile_id: Option<u32>,
    #[serde(default = "default_join_eui")]
    pub joineui: String,
    pub appkey: String,
    pub name: String,
    pub serial_number: String,
}

fn default_join_eui() -> String {
    "0000000000000000".to_string()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NodeAddResponse {
    #[serde(default)]
    pub pending: bool,
    pub success: bool,
    pub message: String,
    pub deveui: String,
}

/// FB16/FB17 — has an operator finished setting this Hub up, and as what?
///
/// The state itself lives in the device's own config (`setup:`), never in the portal's database:
/// [decision #4](../plan/decisions.md), and
/// [peripheral.md](../plan/control/ble/peripheral.md) §6.3 for why — a client-role Hub shuts its
/// portal down, so on exactly the Hubs the app needs to ask about, v1's answer was held by a
/// component that is not running.
///
/// **Two verbs, one seam**, for the same reason `NodeEnroll` has three: a host that can be told
/// setup finished but cannot say whether it did is the write-only surface this ask exists to close.
pub trait SetupState: Send + Sync {
    /// The current state.
    ///
    /// `Err` is "this Hub cannot answer" — an unreadable config, an unwired seam — and becomes an
    /// ATT-level error on FB16, never a `provisioned: false` payload. The two are different claims:
    /// one says the Hub is out of the box, the other says nobody could look. A phone that cannot
    /// tell them apart walks the wizard against a configured Hub, which is the bug §6.3 is about.
    fn read(&self) -> std::result::Result<SetupStatus, String>;

    /// Record that setup completed, with the role the operator chose.
    ///
    /// Idempotent: it writes the state asked for rather than toggling, so a retried write or a
    /// second phone lands on the same file. There is deliberately no un-provision verb — decision
    /// #4 gives that job to the factory reset, and a write that could clear the flag would be a way
    /// to make a configured Hub claim it is new.
    ///
    /// No `Err` beside the outcome: a refusal is an outcome with a reason, as on FB10/FB12.
    fn mark_complete(&self, origin: Origin, role: HubRole) -> SetupOutcome;
}

/// The role an operator chose for this Hub. Serialized lowercase because that is what the config
/// file, the portal and the phone app all already call them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HubRole {
    Server,
    Client,
}

impl HubRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            HubRole::Server => "server",
            HubRole::Client => "client",
        }
    }
}

/// What FB16 serves.
///
/// `role` is serialized as an explicit `null` rather than omitted — the ask names
/// `{provisioned, role}` with `null` as a value, and an absent key would make "no role yet"
/// indistinguishable from "this Hub speaks an older shape".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SetupStatus {
    pub provisioned: bool,
    pub role: Option<HubRole>,
}

/// The result of an FB17 write — refusal included, which is why there is no `Err` beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupOutcome {
    pub success: bool,
    pub message: String,
}

impl SetupOutcome {
    pub fn ok(message: impl Into<String>) -> SetupOutcome {
        SetupOutcome {
            success: true,
            message: message.into(),
        }
    }
    pub fn refused(message: impl Into<String>) -> SetupOutcome {
        SetupOutcome {
            success: false,
            message: message.into(),
        }
    }
}

const SETUP_UNWIRED: &str = "setup state is not available on this build";

/// The harness bundle's seam: it says it cannot answer rather than answering "not provisioned",
/// for the reason [`SetupState::read`] gives.
#[derive(Debug, Default)]
pub struct UnwiredSetupState;

impl SetupState for UnwiredSetupState {
    fn read(&self) -> std::result::Result<SetupStatus, String> {
        Err(SETUP_UNWIRED.to_string())
    }

    fn mark_complete(&self, _origin: Origin, _role: HubRole) -> SetupOutcome {
        SetupOutcome::refused(SETUP_UNWIRED)
    }
}

/// A `SetupState` a test can seed and inspect.
///
/// It holds the one value both verbs move, so a test asserts what FB16 serves after FB17 ran is
/// what FB17 did — not a second piece of bookkeeping that happens to agree.
#[derive(Debug, Default)]
pub struct MemorySetupState {
    status: Mutex<SetupStatus>,
    /// Set to make `read` fail, for the "the Hub could not look" path.
    unreadable: Mutex<Option<String>>,
    /// Every `mark_complete` that got through, for the origin assertions.
    pub writes: Mutex<Vec<(Origin, HubRole)>>,
}

impl MemorySetupState {
    pub fn with_status(status: SetupStatus) -> MemorySetupState {
        MemorySetupState {
            status: Mutex::new(status),
            ..MemorySetupState::default()
        }
    }

    pub fn make_unreadable(&self, why: impl Into<String>) {
        *lock(&self.unreadable) = Some(why.into());
    }

    pub fn status(&self) -> SetupStatus {
        *lock(&self.status)
    }

    /// Seed the state directly, including shapes FB17 itself cannot produce.
    ///
    /// The one that matters is `{provisioned: true, role: None}`: `control-core`'s
    /// `ApplierSetupState::read` maps a `role:` string it does not recognise — `Client` with a
    /// capital C, or anything a later firmware writes — to exactly that, and a gate on the role
    /// has to treat it as "not established" rather than as a fresh Hub. (FB19 was the first such
    /// gate; it is retired, `peripheral::gatt::RETIRED`.)
    pub fn set_status(&self, status: SetupStatus) {
        *lock(&self.status) = status;
    }
}

impl SetupState for MemorySetupState {
    fn read(&self) -> std::result::Result<SetupStatus, String> {
        if let Some(why) = lock(&self.unreadable).clone() {
            return Err(why);
        }
        Ok(*lock(&self.status))
    }

    fn mark_complete(&self, origin: Origin, role: HubRole) -> SetupOutcome {
        let mut status = lock(&self.status);
        *status = SetupStatus {
            provisioned: true,
            role: Some(role),
        };
        lock(&self.writes).push((origin, role));
        SetupOutcome::ok(format!("setup complete, role {}", role.as_str()))
    }
}

/// FB18 — the one MQTT export destination a phone can configure.
///
/// `plan/control/ble/peripheral.md` §6.4, GitLab #59. The ask this closes is the app's client-role
/// wizard step, which collects an address and credentials and then cannot deliver them: the only
/// path today is the portal, over a LAN the phone usually does not share.
pub trait ExportDestination: Send + Sync {
    /// Write the destination. No `Err` beside the outcome: a refusal is an outcome with a reason,
    /// as on FB10/FB12/FB17 — an installer who half-filled the credential pair needs to read why.
    fn set(&self, origin: Origin, req: &ExportDestinationRequest) -> ExportOutcome;
}

/// FB18's payload.
///
/// **No port**: 8883 over TLS is the settled client contract, so there is nothing to get wrong.
/// **No CA**: the open question that blocked this characteristic is settled the other way round —
/// the Hub obtains the CA itself rather than the write carrying a PEM, which would not fit one MTU
/// and would need the paging contract in the write direction.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportDestinationRequest {
    pub host: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// Defaults to enabling it: a phone that went to the trouble of sending a destination meant
    /// to turn it on, and an explicit `false` is available for staging one.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// The result of an FB18 write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOutcome {
    pub success: bool,
    pub message: String,
}

impl ExportOutcome {
    pub fn ok(message: impl Into<String>) -> ExportOutcome {
        ExportOutcome {
            success: true,
            message: message.into(),
        }
    }
    pub fn refused(message: impl Into<String>) -> ExportOutcome {
        ExportOutcome {
            success: false,
            message: message.into(),
        }
    }
}

const EXPORT_UNWIRED: &str = "export configuration is not available on this build";

/// The harness bundle's seam.
#[derive(Debug, Default)]
pub struct UnwiredExportDestination;

impl ExportDestination for UnwiredExportDestination {
    fn set(&self, _origin: Origin, _req: &ExportDestinationRequest) -> ExportOutcome {
        ExportOutcome::refused(EXPORT_UNWIRED)
    }
}

/// An `ExportDestination` a test can inspect: it records what it was asked for and answers with
/// whatever outcome the test seeded.
///
/// Deliberately **dumb**. It used to re-implement the blank-username and blank-host refusals, which
/// made the FB18 handler tests read as if this characteristic enforced them — it does not.
/// `ConfigApplier::apply_export_destination` in `control-core` owns those rules and is where they
/// are tested; a second copy here could only ever drift from the real one and, while it agreed,
/// would let a handler test pass for the wrong reason. What the handler owes is this seam getting
/// the request it parsed and the phone getting the answer back, and that is all this double lets a
/// test see.
#[derive(Debug)]
pub struct MemoryExportDestination {
    /// Every request that reached the seam, refusals included — this is what the handler passed
    /// down, not what some config file accepted.
    pub requests: Mutex<Vec<(Origin, ExportDestinationRequest)>>,
    outcome: Mutex<ExportOutcome>,
}

impl Default for MemoryExportDestination {
    fn default() -> MemoryExportDestination {
        MemoryExportDestination {
            requests: Mutex::new(Vec::new()),
            outcome: Mutex::new(ExportOutcome::ok(
                "destination saved; applies at the next restart",
            )),
        }
    }
}

impl MemoryExportDestination {
    pub fn requests(&self) -> Vec<(Origin, ExportDestinationRequest)> {
        lock(&self.requests).clone()
    }

    /// Seed what the next `set` answers — the stand-in for whatever `control-core` decided.
    pub fn set_outcome(&self, outcome: ExportOutcome) {
        *lock(&self.outcome) = outcome;
    }
}

impl ExportDestination for MemoryExportDestination {
    fn set(&self, origin: Origin, req: &ExportDestinationRequest) -> ExportOutcome {
        lock(&self.requests).push((origin, req.clone()));
        lock(&self.outcome).clone()
    }
}

/// FB1A — submit the portal pairing code.
///
/// `plan/control/ble/peripheral.md` §6.5, GitLab #61. Today the app must POST to the portal over
/// the LAN, which is why the in-app code entry has been hidden in the wizard.
///
/// **What the Hub can and cannot do with a code.** It cannot check it: the pairing protocol uses
/// the code as the password the *response* is encrypted under, so a wrong one fails at the portal,
/// which is the only party that can tell. What this seam does is hand the code to the portal on
/// this machine and ask it to run the pairing an admin would otherwise click — which is the part
/// the phone cannot reach.
pub trait PortalPairing: Send + Sync {
    /// No `Err`: a refusal is an outcome with a reason, as everywhere else on this surface.
    fn submit_code(&self, origin: Origin, code: &str) -> PairingOutcome;
}

/// The result of an FB1A write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingOutcome {
    pub success: bool,
    pub message: String,
}

impl PairingOutcome {
    pub fn ok(message: impl Into<String>) -> PairingOutcome {
        PairingOutcome {
            success: true,
            message: message.into(),
        }
    }
    pub fn refused(message: impl Into<String>) -> PairingOutcome {
        PairingOutcome {
            success: false,
            message: message.into(),
        }
    }
}

const PAIRING_UNWIRED: &str = "portal pairing is not available on this build";

/// The harness bundle's seam.
#[derive(Debug, Default)]
pub struct UnwiredPortalPairing;

impl PortalPairing for UnwiredPortalPairing {
    fn submit_code(&self, _origin: Origin, _code: &str) -> PairingOutcome {
        PairingOutcome::refused(PAIRING_UNWIRED)
    }
}

/// A `PortalPairing` a test can inspect.
#[derive(Debug, Default)]
pub struct MemoryPortalPairing {
    pub submitted: Mutex<Vec<(Origin, String)>>,
}

impl MemoryPortalPairing {
    pub fn submitted(&self) -> Vec<(Origin, String)> {
        lock(&self.submitted).clone()
    }
}

impl PortalPairing for MemoryPortalPairing {
    fn submit_code(&self, origin: Origin, code: &str) -> PairingOutcome {
        lock(&self.submitted).push((origin, code.to_string()));
        PairingOutcome::ok("code handed to the portal on this Hub")
    }
}

/// FB1B — the Hub's radio mode: what it runs and may run, and the provisioning choice.
///
/// `plan/control/radio/node_registry.md` §12, `plan/control/ble/peripheral.md` §6.6. The Manager
/// app shows the allowed modes with the default preselected before setup-complete (FB17), and
/// writes the one the operator picks.
///
/// **One apply path.** The write reaches the same function the signed `radio-set-mode` does
/// (`control-core`'s `libs::radio_mode::apply`): the hardware refusal, the `fiber.config.yaml`
/// write with its audit row, then the service restart. Nothing in this crate judges a mode
/// beyond its spelling.
pub trait RadioModeControl: Send + Sync {
    /// The current state. `Err` is "this Hub cannot answer" and becomes an ATT-level error, as
    /// on FB16: a made-up mode would be a claim about the Hub out of a failure to look.
    fn read(&self) -> std::result::Result<RadioModeStatus, String>;

    /// Choose `mode`. No `Err` beside the outcome: a refusal (no RAK5146 for `lorawan`) is an
    /// outcome with a reason, as on FB12/FB17/FB18.
    fn set(&self, origin: Origin, mode: RadioMode) -> RadioModeOutcome;
}

/// `radio.mode`, spelled as the config, the signed command and the app spell it.
///
/// A local enum rather than control-radio's `Mode`: this crate depends on no other crate of the
/// workspace (control-radio would bring its gRPC client along), so the one place the two meet is
/// `control-core`'s binding, as with [`HubRole`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RadioMode {
    Off,
    Lorawan,
    P2p,
    Dual,
}

impl RadioMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            RadioMode::Off => "off",
            RadioMode::Lorawan => "lorawan",
            RadioMode::P2p => "p2p",
            RadioMode::Dual => "dual",
        }
    }
}

/// What FB1B serves.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RadioModeStatus {
    /// The chosen mode when there is one — which is also the mode a Hub restarts into right after
    /// a write — else the one it was rendered with (the overlay's, or the hardware default).
    pub mode: RadioMode,
    /// A mode is recorded in `fiber.config.yaml`: provisioning or `radio-set-mode` chose it.
    pub chosen: bool,
    /// The RAK5146 LoRaWAN card is detected.
    pub lorawan_card: bool,
    /// The modes this hardware runs, and so the ones a write accepts (plan.md §1's table).
    pub allowed: Vec<RadioMode>,
    /// The mode this Hub runs when nothing chose one: `lorawan` with the card, `p2p` without.
    pub default: RadioMode,
}

/// The result of an FB1B write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioModeOutcome {
    pub success: bool,
    pub message: String,
}

impl RadioModeOutcome {
    pub fn ok(message: impl Into<String>) -> RadioModeOutcome {
        RadioModeOutcome {
            success: true,
            message: message.into(),
        }
    }
    pub fn refused(message: impl Into<String>) -> RadioModeOutcome {
        RadioModeOutcome {
            success: false,
            message: message.into(),
        }
    }
}

const RADIO_MODE_UNWIRED: &str = "the radio mode is not available on this build";

/// The harness bundle's seam.
#[derive(Debug, Default)]
pub struct UnwiredRadioMode;

impl RadioModeControl for UnwiredRadioMode {
    fn read(&self) -> std::result::Result<RadioModeStatus, String> {
        Err(RADIO_MODE_UNWIRED.to_string())
    }

    fn set(&self, _origin: Origin, _mode: RadioMode) -> RadioModeOutcome {
        RadioModeOutcome::refused(RADIO_MODE_UNWIRED)
    }
}

/// A `RadioModeControl` a test can seed and inspect.
///
/// Deliberately dumb, like [`MemoryExportDestination`]: it records what it was asked and answers
/// with the outcome a test seeded. The hardware refusal belongs to `control-core`, where it is
/// tested; a copy here could only drift. The one thing it does besides is what a successful write
/// does to the state — `mode` becomes the written one and `chosen` true — so a test can see the
/// read after a write report the write.
#[derive(Debug)]
pub struct MemoryRadioMode {
    status: Mutex<RadioModeStatus>,
    outcome: Mutex<Option<RadioModeOutcome>>,
    unreadable: Mutex<Option<String>>,
    /// Every write that reached the seam.
    pub writes: Mutex<Vec<(Origin, RadioMode)>>,
}

impl Default for MemoryRadioMode {
    /// A Hub out of the box without the card: `p2p`, nothing chosen.
    fn default() -> MemoryRadioMode {
        MemoryRadioMode::with_status(RadioModeStatus {
            mode: RadioMode::P2p,
            chosen: false,
            lorawan_card: false,
            allowed: vec![RadioMode::Off, RadioMode::P2p],
            default: RadioMode::P2p,
        })
    }
}

impl MemoryRadioMode {
    pub fn with_status(status: RadioModeStatus) -> MemoryRadioMode {
        MemoryRadioMode {
            status: Mutex::new(status),
            outcome: Mutex::new(None),
            unreadable: Mutex::new(None),
            writes: Mutex::new(Vec::new()),
        }
    }

    pub fn status(&self) -> RadioModeStatus {
        lock(&self.status).clone()
    }

    pub fn writes(&self) -> Vec<(Origin, RadioMode)> {
        lock(&self.writes).clone()
    }

    /// Seed what the next writes answer — the stand-in for whatever `control-core` decided.
    pub fn set_outcome(&self, outcome: RadioModeOutcome) {
        *lock(&self.outcome) = Some(outcome);
    }

    pub fn make_unreadable(&self, why: impl Into<String>) {
        *lock(&self.unreadable) = Some(why.into());
    }
}

impl RadioModeControl for MemoryRadioMode {
    fn read(&self) -> std::result::Result<RadioModeStatus, String> {
        if let Some(why) = lock(&self.unreadable).clone() {
            return Err(why);
        }
        Ok(lock(&self.status).clone())
    }

    fn set(&self, origin: Origin, mode: RadioMode) -> RadioModeOutcome {
        lock(&self.writes).push((origin, mode));
        let outcome = lock(&self.outcome)
            .clone()
            .unwrap_or_else(|| RadioModeOutcome::ok(format!("radio mode {}", mode.as_str())));
        if outcome.success {
            let mut status = lock(&self.status);
            status.mode = mode;
            status.chosen = true;
        }
        outcome
    }
}

/// FB1C — the cluster join: this Hub becomes a Client of a Server Hub.
///
/// `plan/control/ble/peripheral.md` §6.7, `plan/control/cluster.md` §8. The Manager app scans the
/// join bundle off the Server's portal and writes it here, last in the wizard.
///
/// **One apply path.** The write reaches the same function `proximosctl control.radio create
/// --role client` does (`control-core`'s `libs::cluster_join::run`): the precondition, the
/// bundle's validation, the CA fetch and fingerprint check, the broker test-connect, the writes
/// with their audit row, and the reboot. Nothing in this crate judges a bundle beyond its shape.
pub trait ClusterJoin: Send + Sync {
    /// The current state. `Err` is "this Hub cannot answer" and becomes an ATT-level error, as on
    /// FB16.
    fn read(&self) -> std::result::Result<ClusterJoinStatus, String>;

    /// Join. **Blocks** for the length of the join — a few seconds, bounded at 20 — which is why
    /// the FB1C handler runs it on a thread of its own. A refusal is an outcome with a reason.
    fn join(&self, origin: Origin, bundle: &ClusterJoinRequest) -> ClusterJoinOutcome;
}

/// FB1C's write: the join bundle exactly as the QR code carries it. `deny_unknown_fields`, as
/// everywhere on this surface. `Debug` never prints `p`.
#[derive(Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterJoinRequest {
    pub v: u32,
    pub h: String,
    pub p: String,
    pub f: String,
}

impl std::fmt::Debug for ClusterJoinRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterJoinRequest")
            .field("v", &self.v)
            .field("h", &self.h)
            .field("p", &"<redacted>")
            .field("f", &self.f)
            .finish()
    }
}

/// What FB1C serves.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClusterJoinStatus {
    /// `client` when this Hub has joined a Server; otherwise an explicit `null`.
    pub role: Option<HubRole>,
    /// A join can succeed: setup is not complete and the Hub has not joined.
    pub joinable: bool,
    /// Only on a Client: the Server it joined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
}

/// The result of an FB1C write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterJoinOutcome {
    pub success: bool,
    pub message: String,
}

impl ClusterJoinOutcome {
    pub fn ok(message: impl Into<String>) -> ClusterJoinOutcome {
        ClusterJoinOutcome {
            success: true,
            message: message.into(),
        }
    }
    pub fn refused(message: impl Into<String>) -> ClusterJoinOutcome {
        ClusterJoinOutcome {
            success: false,
            message: message.into(),
        }
    }
}

const CLUSTER_JOIN_UNWIRED: &str = "the cluster join is not available on this build";

/// The harness bundle's seam.
#[derive(Debug, Default)]
pub struct UnwiredClusterJoin;

impl ClusterJoin for UnwiredClusterJoin {
    fn read(&self) -> std::result::Result<ClusterJoinStatus, String> {
        Err(CLUSTER_JOIN_UNWIRED.to_string())
    }

    fn join(&self, _origin: Origin, _bundle: &ClusterJoinRequest) -> ClusterJoinOutcome {
        ClusterJoinOutcome::refused(CLUSTER_JOIN_UNWIRED)
    }
}

/// A `ClusterJoin` a test can seed and inspect.
///
/// Deliberately dumb, like [`MemoryRadioMode`]: it records what it was asked and answers with the
/// outcome a test seeded. The refusals belong to `control-core`, where they are tested. What it
/// does besides is what a successful join does to the state — `role: client`, not joinable, the
/// Server named — so a test sees the read after a write report it. A join can be held open with
/// [`MemoryClusterJoin::hold`], so a test can read the `pending` state while it runs.
#[derive(Debug)]
pub struct MemoryClusterJoin {
    status: Mutex<ClusterJoinStatus>,
    outcome: Mutex<Option<ClusterJoinOutcome>>,
    unreadable: Mutex<Option<String>>,
    gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    /// Every join that reached the seam.
    pub joins: Mutex<Vec<(Origin, ClusterJoinRequest)>>,
}

impl Default for MemoryClusterJoin {
    /// A Hub out of the box: no role, joinable.
    fn default() -> MemoryClusterJoin {
        MemoryClusterJoin::with_status(ClusterJoinStatus {
            role: None,
            joinable: true,
            server: None,
        })
    }
}

impl MemoryClusterJoin {
    pub fn with_status(status: ClusterJoinStatus) -> MemoryClusterJoin {
        MemoryClusterJoin {
            status: Mutex::new(status),
            outcome: Mutex::new(None),
            unreadable: Mutex::new(None),
            gate: Mutex::new(None),
            joins: Mutex::new(Vec::new()),
        }
    }

    pub fn joins(&self) -> Vec<(Origin, ClusterJoinRequest)> {
        lock(&self.joins).clone()
    }

    /// Seed what the next joins answer.
    pub fn set_outcome(&self, outcome: ClusterJoinOutcome) {
        *lock(&self.outcome) = Some(outcome);
    }

    pub fn make_unreadable(&self, why: impl Into<String>) {
        *lock(&self.unreadable) = Some(why.into());
    }

    /// Hold the next join until the returned sender sends (or is dropped).
    pub fn hold(&self) -> std::sync::mpsc::Sender<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        *lock(&self.gate) = Some(rx);
        tx
    }
}

impl ClusterJoin for MemoryClusterJoin {
    fn read(&self) -> std::result::Result<ClusterJoinStatus, String> {
        if let Some(why) = lock(&self.unreadable).clone() {
            return Err(why);
        }
        Ok(lock(&self.status).clone())
    }

    fn join(&self, origin: Origin, bundle: &ClusterJoinRequest) -> ClusterJoinOutcome {
        let gate = lock(&self.gate).take();
        if let Some(gate) = gate {
            let _ = gate.recv();
        }
        lock(&self.joins).push((origin, bundle.clone()));
        let outcome = lock(&self.outcome)
            .clone()
            .unwrap_or_else(|| ClusterJoinOutcome::ok(format!("joined {}; restarting", bundle.h)));
        if outcome.success {
            *lock(&self.status) = ClusterJoinStatus {
                role: Some(HubRole::Client),
                joinable: false,
                server: Some(bundle.h.clone()),
            };
        }
        outcome
    }
}

// ---------------------------------------------------------------------------------------------
// The bundle
// ---------------------------------------------------------------------------------------------

/// Everything the crate needs from its host, in one bundle.
#[derive(Clone)]
pub struct Host {
    pub config: Arc<dyn ConfigStore>,
    pub publisher: Arc<dyn Publisher>,
    pub readings: Arc<dyn ReadingSink>,
    pub subsystem: Arc<dyn SubsystemControl>,
    pub standby: Arc<dyn StandbyGate>,
    pub network: Arc<dyn NetworkOps>,
    pub clock: Arc<dyn SystemClock>,
    pub device: Arc<dyn DeviceInfo>,
    pub nodes: Arc<dyn NodeEnroll>,
    pub setup: Arc<dyn SetupState>,
    pub export: Arc<dyn ExportDestination>,
    pub pairing: Arc<dyn PortalPairing>,
    pub radio: Arc<dyn RadioModeControl>,
    pub cluster: Arc<dyn ClusterJoin>,
}

impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Host { .. }")
    }
}

impl Host {
    /// The file-backed bundle the standalone harness runs with: a YAML fleet file, no MQTT, no
    /// database, no `nmcli`. Everything unavailable says so rather than pretending.
    pub fn for_harness(fleet_path: impl Into<PathBuf>) -> Host {
        Host {
            config: Arc::new(YamlFleetStore::new(fleet_path)),
            publisher: Arc::new(NullPublisher),
            readings: Arc::new(NullReadingSink),
            subsystem: Arc::new(NoopSubsystem),
            standby: Arc::new(NeverStandby),
            network: Arc::new(UnavailableNetwork),
            clock: Arc::new(WallClock),
            device: Arc::new(StaticDeviceInfo::from_env()),
            nodes: Arc::new(UnwiredNodeEnroll),
            setup: Arc::new(UnwiredSetupState),
            export: Arc::new(UnwiredExportDestination),
            pairing: Arc::new(UnwiredPortalPairing),
            radio: Arc::new(UnwiredRadioMode),
            cluster: Arc::new(UnwiredClusterJoin),
        }
    }

    /// The test bundle: everything in memory, everything inspectable.
    pub fn in_memory() -> Host {
        Host {
            config: Arc::new(MemoryConfigStore::default()),
            publisher: Arc::new(RecordingPublisher::default()),
            readings: Arc::new(MemoryReadingSink::default()),
            subsystem: Arc::new(NoopSubsystem),
            standby: Arc::new(NeverStandby),
            network: Arc::new(UnavailableNetwork),
            clock: Arc::new(WallClock),
            device: Arc::new(StaticDeviceInfo::for_tests()),
            nodes: Arc::new(UnwiredNodeEnroll),
            setup: Arc::new(MemorySetupState::default()),
            export: Arc::new(MemoryExportDestination::default()),
            pairing: Arc::new(MemoryPortalPairing::default()),
            radio: Arc::new(MemoryRadioMode::default()),
            cluster: Arc::new(MemoryClusterJoin::default()),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// YamlFleetStore — the harness ConfigStore
// ---------------------------------------------------------------------------------------------

/// A whole-document YAML fleet file, of which only the `eye:` mapping is ours.
///
/// **Comments are not preserved.** `serde_yaml` has no round-tripping representation for them, so a
/// mutation rewrites the document from its parsed form. That is acceptable here because this
/// implementation exists for the harness and the bench; in the agent, Part B routes every mutation
/// through `control-core`'s own `ConfigApplier`, which owns that file's formatting.
#[derive(Debug, Clone)]
pub struct YamlFleetStore {
    path: PathBuf,
}

impl YamlFleetStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        YamlFleetStore { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn err(&self, kind: Kind, why: String) -> Error {
        Error::new(
            kind,
            None,
            Component::DeviceConfig,
            format!("{}: {why}", self.path.display()),
        )
    }

    fn read_document(&self) -> Result<serde_yaml::Mapping> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) if s.trim().is_empty() => Ok(serde_yaml::Mapping::new()),
            Ok(s) => match serde_yaml::from_str::<serde_yaml::Value>(&s) {
                Ok(serde_yaml::Value::Mapping(m)) => Ok(m),
                Ok(serde_yaml::Value::Null) => Ok(serde_yaml::Mapping::new()),
                Ok(_) => Err(self.err(
                    Kind::InvalidArgument,
                    "the fleet config is not a YAML mapping".to_string(),
                )),
                Err(e) => Err(self.err(Kind::InvalidArgument, format!("cannot parse: {e}"))),
            },
            // Absent is the empty document: a Hub with no beacons enrolled yet is normal.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_yaml::Mapping::new()),
            Err(e) => Err(self.err(Kind::StorageError, format!("cannot read: {e}"))),
        }
    }

    fn write_document(&self, doc: &serde_yaml::Mapping) -> Result<()> {
        let body = serde_yaml::to_string(&serde_yaml::Value::Mapping(doc.clone()))
            .map_err(|e| self.err(Kind::StorageError, format!("cannot serialize: {e}")))?;

        if let Some(dir) = self.path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir).map_err(|e| {
                    self.err(
                        Kind::StorageError,
                        format!("cannot create {}: {e}", dir.display()),
                    )
                })?;
            }
        }

        // Preserve the existing mode: the fleet config may legitimately be tighter than the
        // default umask, and a rewrite must not widen it.
        #[cfg(unix)]
        let existing_mode = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&self.path)
                .ok()
                .map(|m| m.permissions().mode() & 0o777)
        };

        let tmp = self.path.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp).map_err(|e| {
                self.err(
                    Kind::StorageError,
                    format!("cannot create a temp file: {e}"),
                )
            })?;
            #[cfg(unix)]
            if let Some(mode) = existing_mode {
                use std::os::unix::fs::PermissionsExt;
                f.set_permissions(std::fs::Permissions::from_mode(mode))
                    .map_err(|e| self.err(Kind::StorageError, format!("cannot set mode: {e}")))?;
            }
            f.write_all(body.as_bytes())
                .map_err(|e| self.err(Kind::StorageError, format!("cannot write: {e}")))?;
            f.sync_all()
                .map_err(|e| self.err(Kind::StorageError, format!("cannot flush: {e}")))?;
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            self.err(Kind::StorageError, format!("cannot replace: {e}"))
        })?;
        if let Some(dir) = self.path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::File::open(dir)
                    .and_then(|d| d.sync_all())
                    .map_err(|e| self.err(Kind::StorageError, format!("cannot flush dir: {e}")))?;
            }
        }
        Ok(())
    }

    fn audit_path(&self) -> PathBuf {
        let mut p = self.path.clone().into_os_string();
        p.push(".audit.jsonl");
        PathBuf::from(p)
    }
}

const EYE_KEY: &str = "eye";

impl ConfigStore for YamlFleetStore {
    fn load(&self) -> Result<EyeSection> {
        let doc = self.read_document()?;
        match doc.get(serde_yaml::Value::String(EYE_KEY.to_string())) {
            None | Some(serde_yaml::Value::Null) => Ok(EyeSection::default()),
            Some(v) => serde_yaml::from_value::<EyeSection>(v.clone())
                .map_err(|e| self.err(Kind::InvalidArgument, format!("cannot parse `eye:`: {e}"))),
        }
    }

    fn mutate(&self, f: &mut dyn FnMut(&mut EyeSection)) -> Result<EyeSection> {
        let mut doc = self.read_document()?;
        let before = match doc.get(serde_yaml::Value::String(EYE_KEY.to_string())) {
            None | Some(serde_yaml::Value::Null) => EyeSection::default(),
            Some(v) => serde_yaml::from_value::<EyeSection>(v.clone()).map_err(|e| {
                self.err(Kind::InvalidArgument, format!("cannot parse `eye:`: {e}"))
            })?,
        };
        let mut after = before.clone();
        f(&mut after);
        // A no-op mutation writes nothing: an idempotent re-add must not churn the file, spend a
        // durability sync, or produce a second audit row.
        if after == before {
            return Ok(after);
        }
        let value = serde_yaml::to_value(&after)
            .map_err(|e| self.err(Kind::StorageError, format!("cannot serialize `eye:`: {e}")))?;
        doc.insert(serde_yaml::Value::String(EYE_KEY.to_string()), value);
        self.write_document(&doc)?;
        Ok(after)
    }

    fn audit(&self, entry: &AuditEntry) {
        // Best effort by design: losing an audit line must never fail the operation that produced
        // it. Part B routes audit through the agent's own journal, which is the regulated record.
        let Ok(line) = serde_json::to_string(entry) else {
            return;
        };
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.audit_path())
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// In-memory / no-op implementations
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct MemoryConfigStore {
    section: Mutex<EyeSection>,
    writes: AtomicUsize,
    audits: Mutex<Vec<AuditEntry>>,
}

impl MemoryConfigStore {
    pub fn with_section(section: EyeSection) -> Self {
        MemoryConfigStore {
            section: Mutex::new(section),
            writes: AtomicUsize::new(0),
            audits: Mutex::new(Vec::new()),
        }
    }
    /// How many times a mutation actually changed something — the AC 4 counter.
    pub fn write_count(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }
    pub fn audit_entries(&self) -> Vec<AuditEntry> {
        lock(&self.audits).clone()
    }
}

impl ConfigStore for MemoryConfigStore {
    fn load(&self) -> Result<EyeSection> {
        Ok(lock(&self.section).clone())
    }
    fn mutate(&self, f: &mut dyn FnMut(&mut EyeSection)) -> Result<EyeSection> {
        let mut guard = lock(&self.section);
        let mut after = guard.clone();
        f(&mut after);
        if after == *guard {
            return Ok(after);
        }
        *guard = after.clone();
        self.writes.fetch_add(1, Ordering::SeqCst);
        Ok(after)
    }
    fn audit(&self, entry: &AuditEntry) {
        lock(&self.audits).push(entry.clone());
    }
}

#[derive(Debug, Default)]
pub struct RecordingPublisher {
    sensors: Mutex<Vec<SensorsSnapshot>>,
    aggregated: Mutex<Vec<AggregatedSnapshot>>,
    detects: Mutex<Vec<DetectResult>>,
    degraded: Mutex<Vec<DegradedState>>,
    history: Mutex<Vec<HistoryMessage>>,
}

impl RecordingPublisher {
    pub fn sensors(&self) -> Vec<SensorsSnapshot> {
        lock(&self.sensors).clone()
    }
    pub fn aggregated(&self) -> Vec<AggregatedSnapshot> {
        lock(&self.aggregated).clone()
    }
    pub fn detects(&self) -> Vec<DetectResult> {
        lock(&self.detects).clone()
    }
    pub fn degraded(&self) -> Vec<DegradedState> {
        lock(&self.degraded).clone()
    }
    pub fn history(&self) -> Vec<HistoryMessage> {
        lock(&self.history).clone()
    }
}

impl Publisher for RecordingPublisher {
    fn publish_sensors(&self, snapshot: &SensorsSnapshot) {
        lock(&self.sensors).push(snapshot.clone());
    }
    fn publish_aggregated(&self, window: &AggregatedSnapshot) {
        lock(&self.aggregated).push(window.clone());
    }
    fn publish_detect(&self, result: &DetectResult) {
        lock(&self.detects).push(result.clone());
    }
    fn publish_degraded(&self, state: &DegradedState) {
        lock(&self.degraded).push(state.clone());
    }
    fn publish_history(&self, message: &HistoryMessage) {
        lock(&self.history).push(message.clone());
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NullPublisher;
impl Publisher for NullPublisher {
    fn publish_sensors(&self, _: &SensorsSnapshot) {}
    fn publish_detect(&self, _: &DetectResult) {}
    fn publish_degraded(&self, _: &DegradedState) {}
    fn publish_history(&self, _: &HistoryMessage) {}
}

/// Dedups on `message_id`, exactly as the durable store's unique index does.
#[derive(Debug, Default)]
pub struct MemoryReadingSink {
    rows: Mutex<BTreeMap<String, ReadingRow>>,
}

impl MemoryReadingSink {
    pub fn rows(&self) -> Vec<ReadingRow> {
        lock(&self.rows).values().cloned().collect()
    }
    pub fn len(&self) -> usize {
        lock(&self.rows).len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ReadingSink for MemoryReadingSink {
    fn store_reading(&self, row: &ReadingRow) -> Result<()> {
        lock(&self.rows)
            .entry(row.message_id.clone())
            .or_insert_with(|| row.clone());
        Ok(())
    }
    fn max_reading_ts(&self, mac: &MacAddr, event_type: &str) -> Result<Option<i64>> {
        let wanted = mac.to_string();
        Ok(lock(&self.rows)
            .values()
            .filter(|r| r.mac.eq_ignore_ascii_case(&wanted) && r.event_type == event_type)
            .map(|r| r.ts)
            .max())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NullReadingSink;
impl ReadingSink for NullReadingSink {
    fn store_reading(&self, _: &ReadingRow) -> Result<()> {
        Ok(())
    }
    fn max_reading_ts(&self, _: &MacAddr, _: &str) -> Result<Option<i64>> {
        Ok(None)
    }
}

/// The one-shot CLI has no loops to start. `ensure_scanning` returning `Ok(false)` is what makes
/// the enrollment seam report `monitoring: unavailable` instead of claiming success.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSubsystem;
impl SubsystemControl for NoopSubsystem {
    fn ensure_scanning(&self) -> Result<bool> {
        Ok(false)
    }
    fn stop_scanning(&self) {}
    fn is_scanning(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NeverStandby;
impl StandbyGate for NeverStandby {
    fn in_standby(&self) -> bool {
        false
    }
}

/// Every operation says it is unavailable rather than returning a plausible empty answer — a phone
/// shown "no networks found" would be a lie about a seam that is simply not wired here.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableNetwork;

const NO_NETWORK: &str = "network operations are not available in this host";

impl NetworkOps for UnavailableNetwork {
    fn wifi_scan(&self) -> Result<Vec<WifiNetwork>> {
        Err(Error::new(
            Kind::Phase2NotImplemented,
            None,
            Component::GattServer,
            NO_NETWORK,
        ))
    }
    fn wifi_connect(&self, _: &str, _: &str) -> WifiStatus {
        WifiStatus {
            error: NO_NETWORK.to_string(),
            ..Default::default()
        }
    }
    fn wifi_disconnect(&self) -> WifiStatus {
        WifiStatus {
            error: NO_NETWORK.to_string(),
            ..Default::default()
        }
    }
    fn wifi_status(&self) -> WifiStatus {
        WifiStatus {
            error: NO_NETWORK.to_string(),
            ..Default::default()
        }
    }
    fn lan_apply(&self, _: &LanConfigRequest) -> std::result::Result<(), NetworkErrorCategory> {
        Err(NetworkErrorCategory::Other)
    }
    fn lan_status(&self) -> LanStatus {
        LanStatus {
            error: NO_NETWORK.to_string(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct WallClock;
impl SystemClock for WallClock {
    fn now_unix(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
    fn set_time(&self, _epoch: i64) -> std::result::Result<(), String> {
        Err("setting the system clock is not available in this host".to_string())
    }
    /// The harness cannot query timesyncd, so it reports the one thing it can actually check: that
    /// the clock is past a plausible floor. An RTC-less Hub that booted to 1970 reads as unsynced.
    fn synchronized(&self) -> bool {
        self.now_unix() >= EPOCH_2020
    }
}

/// 2020-01-01T00:00:00Z — the floor FB0B validates against too.
pub const EPOCH_2020: i64 = 1_577_836_800;
/// 2100-01-01T00:00:00Z — FB0B's ceiling.
pub const EPOCH_2100: i64 = 4_102_444_800;

#[derive(Debug, Clone)]
pub struct StaticDeviceInfo {
    pub hostname: String,
    pub version: String,
    pub label: String,
}

impl StaticDeviceInfo {
    pub fn from_env() -> Self {
        // Lowercase (decision #21): the QR's `n` field and the advertised name come from here.
        let hostname = std::fs::read_to_string("/etc/hostname")
            .map(|s| s.trim().to_lowercase())
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "proximos".to_string());
        StaticDeviceInfo {
            label: hostname.clone(),
            hostname,
            version: crate::VERSION.to_string(),
        }
    }
    pub fn for_tests() -> Self {
        StaticDeviceInfo {
            hostname: "test-hub".to_string(),
            version: crate::VERSION.to_string(),
            label: "test-hub".to_string(),
        }
    }
}

impl DeviceInfo for StaticDeviceInfo {
    fn hostname(&self) -> String {
        self.hostname.clone()
    }
    fn version(&self) -> String {
        self.version.clone()
    }
    /// Read in-process from `/proc/uptime` — decision #6 replaces v1's `uptime -p` shell-out.
    fn uptime(&self) -> String {
        let secs = std::fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
            .map(|f| f as u64)
            .unwrap_or(0);
        let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3_600, (secs % 3_600) / 60);
        if d > 0 {
            format!("{d}d {h}h {m}m")
        } else if h > 0 {
            format!("{h}h {m}m")
        } else {
            format!("{m}m")
        }
    }
    fn label(&self) -> String {
        self.label.clone()
    }
    fn set_label(&self, _label: &str) -> std::result::Result<(), String> {
        Err("device label is not writable in this host".to_string())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct UnwiredNodeEnroll;

/// The refusal all three verbs share, so an unwired host says the same thing however it is asked.
const NODES_UNWIRED: &str = "node enrollment is not wired in this host";

impl NodeEnroll for UnwiredNodeEnroll {
    fn add(&self, req: &NodeAddRequest) -> NodeAddResponse {
        NodeAddResponse {
            pending: false,
            success: false,
            message: NODES_UNWIRED.to_string(),
            deveui: req.deveui.clone(),
        }
    }

    /// `Err`, not an empty list. An unwired host does not *hold* no Nodes — it cannot look, and a
    /// phone shown an empty list would read that as a fact about the site.
    fn list(&self) -> std::result::Result<Vec<NodeEntry>, String> {
        Err(NODES_UNWIRED.to_string())
    }

    fn remove(&self, _target: &NodeRemoveTarget) -> NodeRemoveOutcome {
        NodeRemoveOutcome::refused(NODES_UNWIRED)
    }
}

/// A `NodeEnroll` a test can seed and inspect — the Node-side counterpart of [`MemoryConfigStore`].
///
/// It holds the same list `add` and `remove` move, so a test can assert the thing that matters:
/// what FB11 serves after FB12 has run is what FB12 actually did, not a second bookkeeping.
#[derive(Debug, Default)]
pub struct MemoryNodeEnroll {
    entries: Mutex<Vec<NodeEntry>>,
    /// Set to make `list` fail, for the "the Hub could not look" path.
    unreadable: Mutex<Option<String>>,
}

impl MemoryNodeEnroll {
    pub fn with_entries(entries: Vec<NodeEntry>) -> MemoryNodeEnroll {
        MemoryNodeEnroll {
            entries: Mutex::new(entries),
            unreadable: Mutex::new(None),
        }
    }

    pub fn fail_list_with(&self, message: impl Into<String>) {
        *lock(&self.unreadable) = Some(message.into());
    }

    pub fn entries(&self) -> Vec<NodeEntry> {
        lock(&self.entries).clone()
    }
}

impl NodeEnroll for MemoryNodeEnroll {
    fn add(&self, req: &NodeAddRequest) -> NodeAddResponse {
        let deveui = req.deveui.trim().to_lowercase();
        let mut entries = lock(&self.entries);
        if !entries.iter().any(|e| e.deveui == deveui) {
            entries.push(NodeEntry {
                deveui: deveui.clone(),
                serial: Some(req.serial_number.clone()),
                name: Some(req.name.clone()),
            });
        }
        NodeAddResponse {
            pending: false,
            success: true,
            message: "node enrolled".to_string(),
            deveui,
        }
    }

    fn list(&self) -> std::result::Result<Vec<NodeEntry>, String> {
        if let Some(e) = lock(&self.unreadable).clone() {
            return Err(e);
        }
        Ok(lock(&self.entries).clone())
    }

    fn remove(&self, target: &NodeRemoveTarget) -> NodeRemoveOutcome {
        let mut entries = lock(&self.entries);
        let matches: Vec<usize> = match target {
            NodeRemoveTarget::DevEui(v) => {
                let want = v.trim().to_lowercase();
                entries
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e.deveui == want)
                    .map(|(i, _)| i)
                    .collect()
            }
            NodeRemoveTarget::Serial(v) => {
                let want = v.trim();
                entries
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e.serial.as_deref() == Some(want))
                    .map(|(i, _)| i)
                    .collect()
            }
        };
        match matches.as_slice() {
            [] => NodeRemoveOutcome::refused(format!(
                "no node is enrolled with {} {}",
                target.label(),
                target.value()
            )),
            [i] => {
                let e = entries.remove(*i);
                NodeRemoveOutcome::ok(format!("{} removed", e.deveui))
            }
            many => NodeRemoveOutcome::refused(format!(
                "{} {} matches {} enrolled nodes; nothing was removed",
                target.label(),
                target.value(),
                many.len()
            )),
        }
    }
}

/// A `StandbyGate` a test can flip — the inner-loop re-check is only testable against something
/// that changes under it.
#[derive(Debug, Default)]
pub struct SwitchableStandby {
    in_standby: std::sync::atomic::AtomicBool,
}

impl SwitchableStandby {
    pub fn set(&self, v: bool) {
        self.in_standby.store(v, Ordering::SeqCst);
    }
}

impl StandbyGate for SwitchableStandby {
    fn in_standby(&self) -> bool {
        self.in_standby.load(Ordering::SeqCst)
    }
}

/// A `SubsystemControl` a test can observe and that actually flips — `NoopSubsystem` can only ever
/// report `unavailable`.
#[derive(Debug, Default)]
pub struct SwitchableSubsystem {
    scanning: std::sync::atomic::AtomicBool,
    starts: AtomicUsize,
    stops: AtomicUsize,
}

impl SwitchableSubsystem {
    pub fn starts(&self) -> usize {
        self.starts.load(Ordering::SeqCst)
    }
    pub fn stops(&self) -> usize {
        self.stops.load(Ordering::SeqCst)
    }
}

impl SubsystemControl for SwitchableSubsystem {
    fn ensure_scanning(&self) -> Result<bool> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Ok(!self.scanning.swap(true, Ordering::SeqCst))
    }
    fn stop_scanning(&self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
        self.scanning.store(false, Ordering::SeqCst);
    }
    fn is_scanning(&self) -> bool {
        self.scanning.load(Ordering::SeqCst)
    }
}

/// A clock a test drives by hand.
///
/// It keeps **two** hands, because the distinction they encode is the one FB1A's rate limit turns
/// on: [`Self::advance`] is time *passing* and moves both, while [`Self::set`] — and so every
/// `set_time`, which is what FB0B performs — is a clock *step* and moves only the wall clock. A
/// test can therefore write an epoch an hour into the future and still be one second later on the
/// monotonic source, exactly as the peer doing it would be.
#[derive(Debug)]
pub struct FakeClock {
    now: Mutex<i64>,
    monotonic: Mutex<u64>,
}

impl FakeClock {
    pub fn at(now: i64) -> Self {
        FakeClock {
            now: Mutex::new(now),
            monotonic: Mutex::new(0),
        }
    }
    /// Step the wall clock. The monotonic hand does not move — a step is not elapsed time.
    pub fn set(&self, now: i64) {
        *lock(&self.now) = now;
    }
    /// Let both hands run forward, the way waiting does.
    pub fn advance(&self, by: i64) {
        *lock(&self.now) += by;
        let mut m = lock(&self.monotonic);
        *m = m.saturating_add_signed(by);
    }
    /// Move only the monotonic hand, for a test that wants a window to reopen without touching the
    /// wall clock it is also asserting on.
    pub fn advance_monotonic(&self, by: u64) {
        let mut m = lock(&self.monotonic);
        *m = m.saturating_add(by);
    }
}

impl SystemClock for FakeClock {
    fn now_unix(&self) -> i64 {
        *lock(&self.now)
    }
    fn set_time(&self, epoch: i64) -> std::result::Result<(), String> {
        self.set(epoch);
        Ok(())
    }
    fn synchronized(&self) -> bool {
        true
    }
    fn monotonic_secs(&self) -> u64 {
        *lock(&self.monotonic)
    }
}

/// The known-beacon allowlist is daemon state, never config — collected here because both the
/// central state and the dispatch layer need the same shape.
pub type KnownBeacons = BTreeSet<MacAddr>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EyeTag;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let base = std::env::temp_dir().join(format!(
                "control-ble-host-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).expect("mkdir");
            TempDir(base)
        }
        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tag(mac: &str) -> EyeTag {
        EyeTag {
            mac: mac.to_string(),
            name: None,
            enabled: true,
            logging_interval_min: None,
            recording: None,
            field_thresholds: Vec::new(),
            provisioned: None,
        }
    }

    #[test]
    fn mutating_the_eye_section_leaves_every_other_section_untouched() {
        let t = TempDir::new("other-sections");
        let path = t.file("fiber.config.yaml");
        std::fs::write(
            &path,
            "system:\n  device_label: Freezer room\nmqtt:\n  host: 127.0.0.1\n  port: 1883\neye:\n  enabled: false\n",
        )
        .expect("seed");

        let store = YamlFleetStore::new(&path);
        store
            .mutate(&mut |e| {
                e.enabled = true;
                e.tags.push(tag("AA:BB:CC:DD:EE:FF"));
            })
            .expect("mutates");

        let raw = std::fs::read_to_string(&path).expect("read back");
        let doc: serde_yaml::Value = serde_yaml::from_str(&raw).expect("parse");
        assert_eq!(
            doc["system"]["device_label"],
            serde_yaml::Value::from("Freezer room")
        );
        assert_eq!(doc["mqtt"]["host"], serde_yaml::Value::from("127.0.0.1"));
        assert_eq!(doc["mqtt"]["port"], serde_yaml::Value::from(1883));
        assert_eq!(doc["eye"]["enabled"], serde_yaml::Value::from(true));

        let reloaded = store.load().expect("reload");
        assert!(reloaded.enabled);
        assert_eq!(reloaded.tags.len(), 1);
    }

    #[test]
    fn a_mutation_that_changes_nothing_writes_nothing() {
        let t = TempDir::new("noop");
        let path = t.file("fleet.yaml");
        std::fs::write(&path, "eye:\n  enabled: true\n").expect("seed");
        let store = YamlFleetStore::new(&path);

        let before = std::fs::metadata(&path).expect("stat").modified().ok();
        let raw_before = std::fs::read_to_string(&path).expect("read");
        // Setting a flag to the value it already has must not rewrite the file.
        store.mutate(&mut |e| e.enabled = true).expect("mutates");
        let raw_after = std::fs::read_to_string(&path).expect("read");
        assert_eq!(
            raw_before, raw_after,
            "a no-op mutation must leave the bytes alone"
        );
        assert_eq!(
            before,
            std::fs::metadata(&path).expect("stat").modified().ok()
        );

        // And the in-memory store counts it the same way.
        let mem = MemoryConfigStore::default();
        mem.mutate(&mut |e| e.enabled = true).expect("noop");
        assert_eq!(mem.write_count(), 0);
        mem.mutate(&mut |e| e.enabled = false).expect("real");
        assert_eq!(mem.write_count(), 1);
    }

    #[test]
    fn the_enable_flip_and_the_tag_append_land_in_one_write() {
        // AC 4: enrolling while monitoring is off is ONE atomic write, not two.
        let mem = MemoryConfigStore::with_section(EyeSection {
            enabled: false,
            ..EyeSection::default()
        });
        mem.mutate(&mut |e| {
            e.tags.push(tag("AA:BB:CC:DD:EE:FF"));
            e.enabled = true;
        })
        .expect("mutates");
        assert_eq!(mem.write_count(), 1, "one write, not two");
        let after = mem.load().expect("load");
        assert!(after.enabled);
        assert_eq!(after.tags.len(), 1);
    }

    #[test]
    fn an_absent_fleet_file_loads_as_defaults_and_the_first_mutation_creates_it() {
        let t = TempDir::new("absent");
        let path = t.file("nested/fleet.yaml");
        let store = YamlFleetStore::new(&path);
        let loaded = store.load().expect("absent loads");
        assert!(loaded.enabled);
        assert!(loaded.tags.is_empty());
        assert!(!path.exists());

        store
            .mutate(&mut |e| e.tags.push(tag("AA:BB:CC:DD:EE:FF")))
            .expect("mutates");
        assert!(path.exists(), "the first real mutation creates the file");
        assert_eq!(store.load().expect("reload").tags.len(), 1);
    }

    #[test]
    fn readings_dedup_on_message_id() {
        let sink = MemoryReadingSink::default();
        let mac = MacAddr::parse("AA:BB:CC:DD:EE:FF").expect("mac");
        let row = |id: &str, ts: i64| ReadingRow {
            mac: mac.to_string(),
            ts,
            received_at: ts,
            message_id: id.to_string(),
            event_type: EVENT_ADVERTISING.to_string(),
            payload: serde_json::json!({"temperature_c": 4.0}),
        };
        sink.store_reading(&row("AA:BB:CC:DD:EE:FF-adv-100", 100))
            .expect("first");
        sink.store_reading(&row("AA:BB:CC:DD:EE:FF-adv-100", 100))
            .expect("duplicate");
        assert_eq!(sink.len(), 1, "the same message_id is one row");

        // The timestamp is part of the id, so the same payload a tick later IS a second reading.
        sink.store_reading(&row("AA:BB:CC:DD:EE:FF-adv-130", 130))
            .expect("second");
        assert_eq!(sink.len(), 2);

        assert_eq!(
            sink.max_reading_ts(&mac, EVENT_ADVERTISING).expect("max"),
            Some(130)
        );
        assert_eq!(
            sink.max_reading_ts(&mac, EVENT_RECORDING).expect("max"),
            None,
            "event types do not bleed into each other"
        );
        let other = MacAddr::parse("00:11:22:33:44:55").expect("mac");
        assert_eq!(
            sink.max_reading_ts(&other, EVENT_ADVERTISING).expect("max"),
            None
        );
    }

    #[test]
    fn payloads_serialize_with_the_v1_field_names() {
        let p = BeaconPayload {
            gateway: "hub-1".to_string(),
            mac: "AA:BB:CC:DD:EE:FF".to_string(),
            name: Some("Freezer 1".to_string()),
            temperature_c: Some(-18.5),
            humidity_pct: Some(41),
            battery_mv: Some(3060),
            low_battery: Some(false),
            magnet_present: None,
            magnet_detected: None,
            moving: None,
            movement_count: None,
            pitch_deg: None,
            roll_deg: None,
            rssi: Some(-70),
            last_seen_ts: Some(1_736_775_000),
            stale: false,
            provisioning: "provisioned".to_string(),
            discovered: false,
            is_en12830: Some(true),
            field_alarm_states: BTreeMap::from([("temperature".to_string(), "NORMAL".to_string())]),
            alarm_state: "NORMAL".to_string(),
            ble_config: None,
        };
        let v = serde_json::to_value(&p).expect("serialize");
        let obj = v.as_object().expect("object");
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        let mut expected = BEACON_PAYLOAD_FIELDS.to_vec();
        expected.sort_unstable();
        let mut actual = keys.clone();
        actual.sort_unstable();
        assert_eq!(actual, expected, "the wire field set is the v1 contract");
        // Absent fields are present-as-null, never omitted and never zero: the portal reads
        // `null` as "not known", and omitting them would be a different message.
        assert!(obj["magnet_present"].is_null());
        assert!(obj["movement_count"].is_null());
        assert_eq!(obj["temperature_c"], serde_json::json!(-18.5));

        let snap = SensorsSnapshot {
            timestamp: "2026-08-26T10:00:00Z".to_string(),
            tags: vec![p],
            discovered_capped: false,
        };
        let json = serde_json::to_string(&snap).expect("serialize");
        let back: SensorsSnapshot = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(back, snap);
        assert!(
            json.contains("\"discovered_capped\":false"),
            "the v2 addition is always on the wire: {json}"
        );
    }

    #[test]
    fn a_read_back_names_its_protocol_and_every_sensor_bit_and_a_recorder_reports_none() {
        let reported = ReportedBle {
            config: crate::central::provision::BleConfig {
                advertising_interval_ms: 10_000,
                tx_power_dbm: -8,
                protocol_type: 0x02,
                active_sensors: Some(0x03),
            },
            read_at: 1_790_106_435,
            verified: true,
        };
        let v = serde_json::to_value(BleConfigPayload::from(&reported)).expect("serialize");
        assert_eq!(
            v,
            serde_json::json!({
                "advertising_interval_ms": 10000,
                "tx_power_dbm": -8,
                "protocol_type": 2,
                "protocol": "eye_sensor",
                "sensors": {
                    "temperature": true,
                    "humidity": true,
                    "magnet": false,
                    "movement": false
                },
                "read_at": 1790106435,
                "verified": true
            })
        );
        let ibeacon = ReportedBle {
            config: crate::central::provision::BleConfig {
                protocol_type: 3,
                ..reported.config
            },
            ..reported
        };
        assert_eq!(
            serde_json::to_value(BleConfigPayload::from(&ibeacon)).expect("serialize")["protocol"],
            "ibeacon_eye_sensor",
            "the protocol byte by name, from the BTSMP1 table"
        );

        let recorder = ReportedBle {
            config: crate::central::provision::BleConfig {
                active_sensors: None,
                ..reported.config
            },
            ..reported
        };
        let v = serde_json::to_value(BleConfigPayload::from(&recorder)).expect("serialize");
        assert!(
            v["sensors"].is_null(),
            "no characteristic is not every sensor off"
        );
    }

    #[test]
    fn an_empty_snapshot_still_serializes_tags_as_an_empty_array() {
        // `tags: []` is the wire signal that the discovered set emptied — it must not vanish.
        let snap = SensorsSnapshot {
            timestamp: "2026-08-26T10:00:00Z".to_string(),
            tags: Vec::new(),
            discovered_capped: false,
        };
        let json = serde_json::to_string(&snap).expect("serialize");
        assert!(json.contains("\"tags\":[]"), "{json}");
        assert!(json.contains("\"discovered_capped\":false"), "{json}");
    }

    #[test]
    fn the_audit_trail_is_one_json_line_per_entry() {
        let t = TempDir::new("audit");
        let path = t.file("fleet.yaml");
        let store = YamlFleetStore::new(&path);
        store.audit(&AuditEntry {
            action: "beacon-add",
            mac: Some("AA:BB:CC:DD:EE:FF".to_string()),
            origin: Origin::Cli,
            detail: "enrolled; eye.enabled flipped to true in the same write".to_string(),
        });
        store.audit(&AuditEntry {
            action: "beacon-remove",
            mac: Some("AA:BB:CC:DD:EE:FF".to_string()),
            origin: Origin::Ble,
            detail: "removed".to_string(),
        });
        let raw = std::fs::read_to_string(store.audit_path()).expect("audit file");
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).expect("json");
        assert_eq!(first["action"], "beacon-add");
        assert_eq!(first["origin"], "cli");
        let second: serde_json::Value = serde_json::from_str(lines[1]).expect("json");
        assert_eq!(second["origin"], "ble");
        // The v1 vocabulary never appears in an audit action.
        assert!(!raw.contains("_eye_"), "{raw}");
    }

    #[test]
    fn the_unwired_seams_say_so_rather_than_returning_a_plausible_empty_answer() {
        let h = Host::in_memory();
        assert!(h.network.wifi_scan().is_err());
        assert!(h.network.wifi_status().error.contains("not available"));
        assert!(h.network.lan_status().error.contains("not available"));
        assert!(h.clock.set_time(EPOCH_2020).is_err());
        assert!(h.device.set_label("x").is_err());
        let r = h.nodes.add(&NodeAddRequest {
            deveui: "0011223344556677".to_string(),
            profile_id: None,
            joineui: default_join_eui(),
            appkey: "k".to_string(),
            name: "n".to_string(),
            serial_number: "s".to_string(),
        });
        assert!(!r.success);
        assert_eq!(r.deveui, "0011223344556677");
        assert!(r.message.contains("not wired"));
        // The same for the two verbs FB11/FB12 sit on. `list` is an `Err` rather than an empty
        // list: an unwired host does not hold no Nodes, it cannot look, and a phone shown an
        // empty list would read that as a fact about the site.
        let e = h.nodes.list().expect_err("cannot look");
        assert!(e.contains("not wired"), "{e}");
        let out = h
            .nodes
            .remove(&NodeRemoveTarget::Serial("SN-1".to_string()));
        assert!(!out.success);
        assert!(out.message.contains("not wired"));
        // The one-shot CLI cannot start a loop, and says so rather than claiming success.
        assert!(!h.subsystem.ensure_scanning().expect("noop"));
        assert!(!h.subsystem.is_scanning());
    }

    fn entry(deveui: &str, serial: Option<&str>, name: Option<&str>) -> NodeEntry {
        NodeEntry {
            deveui: deveui.to_string(),
            serial: serial.map(str::to_string),
            name: name.map(str::to_string),
        }
    }

    #[test]
    fn the_memory_node_seam_removes_from_the_very_list_it_serves() {
        // The property the FB11/FB12 tests lean on: what the list shows after a removal is what the
        // removal did, not a second set of bookkeeping that happens to agree.
        let nodes = MemoryNodeEnroll::with_entries(vec![
            entry("0011223344556677", Some("SN-1"), Some("Fridge 1")),
            entry("8899aabbccddeeff", Some("SN-2"), None),
        ]);
        assert_eq!(nodes.list().expect("lists").len(), 2);

        let out = nodes.remove(&NodeRemoveTarget::Serial("SN-2".to_string()));
        assert!(out.success, "{}", out.message);
        assert!(out.message.contains("8899aabbccddeeff"), "{}", out.message);
        let left = nodes.list().expect("lists");
        assert_eq!(
            left,
            vec![entry("0011223344556677", Some("SN-1"), Some("Fridge 1"))]
        );

        // Either identifier addresses the same device — `plan/control/radio/plan.md` §5.
        assert!(
            nodes
                .remove(&NodeRemoveTarget::DevEui("0011223344556677".to_string()))
                .success
        );
        assert!(nodes.list().expect("lists").is_empty());
    }

    #[test]
    fn an_unknown_target_is_a_refusal_with_a_reason_never_an_error() {
        // FB12 keeps FB10's contract: a mistyped identifier has to come back as something the
        // installer can read, and an ATT error has no room to say which identifier was wrong.
        let nodes =
            MemoryNodeEnroll::with_entries(vec![entry("0011223344556677", Some("SN-1"), None)]);
        let out = nodes.remove(&NodeRemoveTarget::Serial("SN-9".to_string()));
        assert!(!out.success);
        assert!(
            out.message.contains("serial") && out.message.contains("SN-9"),
            "{}",
            out.message
        );
        assert_eq!(nodes.list().expect("lists").len(), 1, "nothing was removed");
    }

    #[test]
    fn an_ambiguous_serial_removes_nothing_and_says_how_many_it_matched() {
        // Two devices carrying the same serial is a data defect, but guessing which one the
        // installer meant would turn it into a deleted device.
        let nodes = MemoryNodeEnroll::with_entries(vec![
            entry("0011223344556677", Some("SN-DUP"), None),
            entry("8899aabbccddeeff", Some("SN-DUP"), None),
        ]);
        let out = nodes.remove(&NodeRemoveTarget::Serial("SN-DUP".to_string()));
        assert!(!out.success);
        assert!(out.message.contains('2'), "{}", out.message);
        assert_eq!(nodes.list().expect("lists").len(), 2, "nothing was removed");
    }

    #[test]
    fn a_remove_target_names_itself_for_the_message_the_installer_reads() {
        assert_eq!(NodeRemoveTarget::DevEui("aa".into()).label(), "deveui");
        assert_eq!(NodeRemoveTarget::DevEui("aa".into()).value(), "aa");
        assert_eq!(NodeRemoveTarget::Serial("SN-1".into()).label(), "serial");
        assert_eq!(NodeRemoveTarget::Serial("SN-1".into()).value(), "SN-1");
    }

    #[test]
    fn the_node_add_request_rejects_unknown_fields_and_defaults_the_join_eui() {
        let r: NodeAddRequest = serde_json::from_str(
            r#"{"deveui":"0011223344556677","appkey":"k","name":"n","serial_number":"s"}"#,
        )
        .expect("parses");
        assert_eq!(r.joineui, "0000000000000000");
        assert_eq!(r.profile_id, None);
        assert!(serde_json::from_str::<NodeAddRequest>(
            r#"{"deveui":"a","appkey":"k","name":"n","serial_number":"s","surprise":1}"#
        )
        .is_err());
    }

    #[test]
    fn the_switchable_subsystem_reports_whether_it_actually_started_something() {
        let s = SwitchableSubsystem::default();
        assert!(s.ensure_scanning().expect("start"), "it started the loop");
        assert!(
            !s.ensure_scanning().expect("again"),
            "already running, so nothing was started"
        );
        assert!(s.is_scanning());
        s.stop_scanning();
        assert!(!s.is_scanning());
        assert_eq!(s.starts(), 2);
        assert_eq!(s.stops(), 1);
    }

    #[test]
    fn a_malformed_fleet_file_is_invalid_argument_not_a_silent_default() {
        let t = TempDir::new("malformed");
        let path = t.file("fleet.yaml");
        std::fs::write(&path, "eye:\n  tags: \"not a list\"\n").expect("seed");
        let err = YamlFleetStore::new(&path).load().expect_err("must refuse");
        assert_eq!(err.kind(), Kind::InvalidArgument);
        assert_eq!(err.component(), Component::DeviceConfig);
    }
}
