//! The Node pending-configuration draft and its send walk
//! (devices.backend.md §7 WP4.2, spec devices.node.md §3.5).
//!
//! # What changed about configuration writes
//!
//! Before B4, `POST /nodes/{k}/thresholds/{field}` and
//! `POST /nodes/{k}/config` relayed straight to the Hub: type a number,
//! press Save, a downlink leaves. devices.node.md §3.5 asks for the
//! opposite. An edit writes a **draft** — server-side, persistent, visible
//! to every user with who made it and when — and one deliberate "send
//! configuration" walks it. Those two routes keep their validation and lose
//! their relay call; this module owns everything downstream of the button.
//!
//! # Two clocks, and why the second one exists
//!
//! `relay.execute` answers in at most 20 s (`relay::COMMAND_TIMEOUT`), and
//! what it answers is **the Hub accepted the command** — the signed
//! challenge/confirm round trip completed. It says nothing about the Node,
//! which is asleep between uplinks and may take minutes to receive a
//! downlink and answer it.
//!
//! So a config unit is confirmed by the *device*, not by the Hub: after a
//! successful relay the walk waits for a `ChangeEvent::NodeCommandResult`
//! for that Node whose `payload["command"]` is `set_sticker_config` and
//! whose result is one of `sse::NODE_COMMAND_OK_RESULTS`, within the Node's
//! [`downlink_answer_budget`] (two report intervals and a minute, at least
//! [`DEVICE_CONFIRM_BUDGET`]'s 180 s). Only then is the row deleted.
//!
//! A **threshold** unit has only the first clock, and that is not a
//! shortcut: `set_lorawan_field_threshold` is a fleet-config write the Hub
//! evaluates itself and never sends to the Node (§20.1 established the same
//! thing for Beacons), so no `NodeCommandResult` will ever carry that verb
//! and waiting for one would time out every time. See [`THRESHOLD_COMMAND`].
//!
//! # Who runs the walk
//!
//! A detached `tokio::spawn`, claimed per Node (§21 review, defects 2 and 4
//! — [`PendingService::claim_send`]). The POST answers as soon as the walk
//! is running; the banner then follows it over SSE from the walk's own
//! nudges. Running it inline meant a dropped request left rows in `sending`
//! with Send *and* Discard disabled, and two managers pressing Send at once
//! ran two walks over one draft.
//!
//! A timeout is an **error and a stop**, not a skip (decision 2026-09-07).
//! Configuration keys are not independent — an `interval_sample` that
//! landed beside an `interval_report` that did not is a Node in a state
//! nobody asked for — so the walk stops at the first failure and leaves the
//! rest of the draft untouched for a human to look at and resend.
//!
//! # Ordering: the Hub, then the portal, then the Node (PF-3)
//!
//! Decision Hynek 2026-09-27 (nodes_phase1.cs.md, dodatek 2026-09-27), which
//! reverses N15's Node-first order. Each drafted field's Hub-side threshold
//! write goes out first, and the moment the Hub takes it the band is written
//! to `field_thresholds` ([`WriteThrough`]): the Hub and the portal show and
//! evaluate the new bounds at once. The Node's one `set-config` (the config
//! keys and, when a critical band moved, the whole alarm table) goes last,
//! and the alarm layout is recorded only once the Node confirms it.
//!
//! On the bench (PF-3) the Node's OK came two minutes after the budget, and
//! with the Node first the walk had stopped before the Hub or the portal
//! changed: the Node held the new band, the portal evaluated the old one.
//!
//! # A Node that does not confirm in time
//!
//! A bound is set once the Hub and the portal have it: telemetry evaluation
//! is the primary alarm path and the Node's own table an extension (decision
//! Hynek 2026-09-27). So a walk that stops at the Node's part changed every
//! bound it carried ([`SendStop::NodeUndelivered`] with no `config_keys`),
//! and nothing ever reverts on its own.
//!
//! The Node's rows go to `undelivered`, not `error`. For a threshold row the
//! Hub part is done, so "Send again" sends the Node its part only
//! ([`plan_send`] plans no Hub unit for an `undelivered` row), and every
//! such row remembers what the Hub held before (`previous_value`), which only
//! a manual Discard puts back ([`plan_revert`]). An undelivered bound opens a
//! `CONFIG_NOT_DELIVERED` warning keyed [`config_warning_sensor_id`], open
//! for as long as any bound is `undelivered`
//! ([`PendingService::sync_config_warning`]); undelivered intervals alone
//! leave an archived one. `node_threshold_delivery` records, per field of the
//! Node's alarm table, what the Node confirmed and when — the thresholds
//! table's "on the Node". The Node's late ok still finishes the unit
//! ([`PendingService::apply_late_answer`]), and the P-6 reconcile still
//! clears config keys the Node reports.
//!
//! # `synced_at`
//!
//! Nothing here stamps one. The Node's configuration timestamp comes from
//! the device's own refreshed config pages (`IngestEvent::StickerConfig` ->
//! `NodeState`), which is the only source that knows what the device
//! actually holds. A portal-side stamp would say "we sent this" while
//! showing itself as "the device has this".

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use futures_util::future::BoxFuture;
use portal_storage::{
    NewPendingConfig, PendingConfigRow, Storage, StorageError, PENDING_KIND_CONFIG,
    PENDING_KIND_THRESHOLD, PENDING_STATE_ERROR, PENDING_STATE_PENDING, PENDING_STATE_SENDING,
    PENDING_STATE_UNDELIVERED,
};
use serde_json::json;
use tokio::sync::{broadcast, mpsc};

/// The least one unit waits for the **device** to confirm, after the Hub has
/// already accepted the command: the floor of [`downlink_answer_budget`].
///
/// The Hub's own budget is `relay::COMMAND_TIMEOUT` (20 s) and it is a
/// different question — see the module doc's "two clocks". 180 s is three
/// minutes of a Node's own schedule, which covers a downlink queued behind
/// one or two uplink windows of a fast-reporting Node.
pub const DEVICE_CONFIRM_BUDGET: Duration = Duration::from_secs(180);

/// The firmware's default `application.interval_report` (900 s): the cadence
/// [`downlink_answer_budget`] assumes for a Node whose config the portal has not seen yet.
pub const FIRMWARE_DEFAULT_INTERVAL_REPORT_S: u64 = 900;

/// How long a Node's answer to a downlink may take, on either carrier (P2,
/// lrw_p2p_unification.cs.md §3): two report intervals plus a minute, never less than
/// [`DEVICE_CONFIRM_BUDGET`]. The one budget for "the Node has not answered": the send walk's
/// wait for its ack, the force-send watch's for its reading, and the draft banner's judgement
/// of a `sending` row.
///
/// A Node fetches a queued downlink only when it transmits. LoRaWAN delivers it at the next
/// uplink, up to one interval later; P2P at the second, because the first one's ACK only
/// announces it (p2p.md B4/D2, PN-3), and the answer trails that uplink by a second or two. Two
/// intervals cover both; the minute is the margin (N13, WP4 on combined22: with the firmware's
/// default 900 s a fixed 180 s marked every ordinary send an error while the downlink still
/// waited and applied minutes later).
///
/// An unknown interval is the firmware default's: a Node whose config the portal has not seen
/// yet reports every 900 s unless someone changed it, and the fixed 180 s this used to fall
/// back to was N13's error again.
pub fn downlink_answer_budget(interval_report_s: Option<u64>) -> Duration {
    let report = interval_report_s
        .filter(|r| *r > 0)
        .unwrap_or(FIRMWARE_DEFAULT_INTERVAL_REPORT_S);
    Duration::from_secs(2 * report + 60).max(DEVICE_CONFIRM_BUDGET)
}

/// `application.interval_report` from a Node's config info.
pub fn interval_report_of(
    config: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<u64> {
    config?.get("application.interval_report")?.as_u64()
}

/// How long past the Node's [`downlink_answer_budget`] a `sending` row may sit before
/// the banner stops believing it (§21 review, defect 2).
///
/// A walk marks a unit `sending`, waits at most the budget, and then either
/// deletes the row or writes its error — so a row still `sending` well past
/// the budget belongs to a walk that no longer exists. Before B4's review
/// that could only happen across a restart, which `reset_sending_to_error`
/// already cleans up; now that the walk is a detached task it can also be
/// dropped mid-flight, and the drop guard's own write can itself be lost
/// (a killed process, a poisoned lock). The banner is the last line of
/// defence: without it, Send and Discard stay disabled forever
/// (`pending_config_body.html`) and the operator has no way back.
///
/// The slack covers the relay's own 20 s per unit plus the walk's writes,
/// with room to spare: it exists to avoid calling a live walk dead, not to
/// be precise.
pub const SENDING_STALE_SLACK: Duration = Duration::from_secs(60);

/// The config half's wire verb: a signed downlink that reaches the Node and
/// is answered by it. This is the command the 180 s device clock waits on.
const SEND_COMMAND: &str = "set_sticker_config";

/// The threshold half's wire verb — what `set_node_threshold` has always
/// relayed.
///
/// **It has no device leg.** `set_lorawan_field_threshold` is a fleet-config
/// write on the Hub (the same shape as the Beacon side's
/// `set_eye_field_threshold`, devices.backend.md §20.1): the Hub evaluates
/// thresholds, the Node never sees them, and no `NodeCommandResult` will
/// ever name this verb. A threshold unit is therefore confirmed by the
/// relay's own answer and skips the second clock — waiting on it would time
/// out every single time.
const THRESHOLD_COMMAND: &str = "set_lorawan_field_threshold";

/// Nudge capacity, same reasoning as `BackfillService`'s: a lagging
/// subscriber misses an intermediate repaint and the next one corrects it.
const NUDGE_CHANNEL_CAPACITY: usize = 64;

/// The four bounds of a field threshold, in the order the wire and the form
/// both use.
pub const THRESHOLD_BOUNDS: [&str; 4] = [
    "critical_low",
    "warning_low",
    "warning_high",
    "critical_high",
];

/// "This Node's draft changed — re-render its banner." Carries no rows, the
/// same coarse-signal philosophy as [`crate::backfill::BackfillNudge`] and
/// [`crate::state::ChangeEvent`]: the SSE layer re-reads the draft.
#[derive(Debug, Clone)]
pub struct PendingNudge {
    pub hostname: String,
    pub node_key: String,
}

/// Why a send did not finish. Rendered into the banner's `last_error` and
/// into the audit row's details.
#[derive(Debug, Clone)]
pub enum SendStop {
    /// The relay refused or the Hub did not accept inside its 20 s budget.
    /// `applied` names the fields whose Hub write landed earlier in the same
    /// walk: those bounds are in effect (PF-3 review, item 10).
    Relay {
        param_key: String,
        message: String,
        applied: Vec<String>,
    },
    /// The Node's part of the send did not land (PF-3): the radio or the
    /// relay did not take it, the Node refused it, or it did not answer within
    /// the budget. Everything before it did land — each field's Hub write and
    /// its `field_thresholds` row — so a drafted bound is already in effect:
    /// telemetry evaluation is the primary alarm path, the Node's own table an
    /// extension. `config_keys` names what only the Node holds (intervals),
    /// which is therefore not in effect; empty = the send changed every bound
    /// it carried and only the Node has not got them yet.
    NodeUndelivered {
        param_key: String,
        message: String,
        config_keys: Vec<String>,
    },
    /// The draft moved under the walk — discarded, or edited to a different
    /// value — so what it was about to send is no longer what anybody is
    /// asking for (§21 review, defect 3).
    Superseded {
        param_key: String,
        applied: Vec<String>,
    },
    /// A stored `desired_value` will not parse, so there is nothing safe to
    /// put on the wire (§21 review, item 10).
    Unreadable { param_key: String },
    /// The unit carries a [`SendRefusal`] (MR !129 review).
    Refused {
        param_key: String,
        refusal: SendRefusal,
    },
    /// The draft could not be marked `sending` (§21 review round 2, item 3).
    ///
    /// The walk stops rather than publishing anyway. A row the portal
    /// cannot write is a row whose confirmation it also cannot record: the
    /// device would apply the value, the delete-on-confirm would fail the
    /// same way, and the draft would keep offering to send something that
    /// already landed.
    Storage { param_key: String, message: String },
    /// The draft was empty, or something else emptied it mid-walk.
    Nothing,
}

impl SendStop {
    pub fn param_key(&self) -> Option<&str> {
        match self {
            SendStop::Relay { param_key, .. }
            | SendStop::NodeUndelivered { param_key, .. }
            | SendStop::Superseded { param_key, .. }
            | SendStop::Unreadable { param_key }
            | SendStop::Refused { param_key, .. }
            | SendStop::Storage { param_key, .. } => Some(param_key),
            SendStop::Nothing => None,
        }
    }
}

/// The draft's service half: its own storage connection and the nudge
/// broadcast the SSE layer subscribes to.
///
/// Shaped on [`crate::backfill::BackfillService`] — cheap to clone
/// (`Arc`-backed), opens `<data_dir>/portal.db` itself rather than sharing
/// the request path's `Db`, because the send walk runs off a spawned task that
/// lives for minutes and has no business queueing behind (or ahead of) the
/// request path's reads. **The lock is never held across an await**: each
/// write takes it for the length of one statement and gives it straight
/// back. Most of them take it on the blocking pool; the one that does not
/// is [`PendingService::mark_interrupted_blocking`], and its doc comment
/// says why that is safe rather than lucky.
/// The radio `seq`s queued per (Hub, Node) since the last take (P-6).
type QueuedSeqs = std::collections::HashMap<(String, String), Vec<u64>>;

/// How long a Node unit the walk gave up on still waits for the Node's late answer (PF-3).
/// A day covers a Node that was out of range or asleep for hours; past it, an answer that
/// still correlates is more likely a stale retransmission than the send it once was.
pub const LATE_ANSWER_WINDOW: Duration = Duration::from_secs(24 * 3600);

/// A Node unit whose walk ran out of budget on the radio path (PF-3, nodes_phase1.cs.md
/// dodatek 2026-09-27), kept so the Node's late ok still finishes it as a timely one would.
#[derive(Debug, Clone)]
pub struct LateUnit {
    /// As the walk spelled it: the draft rows are stored under this spelling.
    pub hostname: String,
    pub node_key: String,
    pub label: String,
    /// Every `seq` the unit's last attempt was queued under, for the audit row.
    pub seqs: Vec<u64>,
    /// Those the Node has not answered yet. The unit is finished when this is empty.
    pub unanswered: Vec<u64>,
    /// The unit's rows, pinned to the values sent.
    pub rows: Vec<SendRow>,
    /// The layout of the alarm table the unit carried, recorded once the Node takes it.
    pub alarm_layout: Option<Vec<String>>,
    /// The slots that table fills, confirmed on the Node by the late ok.
    pub node_bands: Vec<portal_storage::NodeBand>,
    pub given_up_at: std::time::Instant,
}

/// `(hostname, node_key, config keys)` a restart turned `undelivered`, for their archived warning.
type ArchiveEntry = (String, String, Vec<String>);

/// Late units by `(hostname, node_key)`, lowercase. One per Node: a Node's draft is one walk
/// at a time, and a new walk forgets the old one.
type LateUnits = std::collections::HashMap<(String, String), LateUnit>;

/// The `sensor_id` prefix of a Node's "configuration not delivered" device fault,
/// `cfgsync:<dev_eui>` (config_sync.md §3, §13 Q10). Not `lrw:{key}:` on purpose, the precedent
/// the calibration reminder (`cal:`) set: the probe window and History read every row under
/// that prefix as a measurement event of the Node.
pub const CONFIG_WARNING_PREFIX: &str = "cfgsync:";

/// The prefix the same warning had before phase C (PF-3, `cfg:node:<dev_eui>`). An open row
/// under it is closed by the next sync of that Node, so an upgrade leaves no orphan.
const LEGACY_CONFIG_WARNING_PREFIX: &str = "cfg:node:";

/// How long a Node has to confirm its pending configuration before its `cfgsync:` device
/// fault opens (config_sync.md §13 Q7, Q10: 4 h on LoRaWAN and P2P alike). Each send ("Send
/// again" too) restarts it.
pub const NODE_CONFIG_DEADLINE: Duration = Duration::from_secs(4 * 3600);

/// The deadline a fresh [`PendingService`] uses: [`NODE_CONFIG_DEADLINE`], and none at all
/// under `cfg(test)`, where the PF-3 tests expect the warning the moment a bound is
/// undelivered. The 4 h tests set it with [`PendingService::with_deadline`].
fn default_deadline() -> Duration {
    if cfg!(test) {
        Duration::ZERO
    } else {
        NODE_CONFIG_DEADLINE
    }
}

/// `alarm_events.alarm_type` of that warning — `AlarmType::ConfigNotDelivered`.
const CONFIG_WARNING_TYPE: &str = "CONFIG_NOT_DELIVERED";

/// The one warning row per Node, `cfgsync:<dev_eui>`.
pub fn config_warning_sensor_id(node_key: &str) -> String {
    format!("{CONFIG_WARNING_PREFIX}{}", node_key.to_ascii_lowercase())
}

/// Where a Node's configuration warning is announced (PF-3): the store for the Node's name,
/// the notifier (the alarm e-mail and webhook), and the alarm table's nudge (the list, the
/// sidebar badge and the pop-up).
#[derive(Clone)]
struct WarningSink {
    store: Arc<crate::state::StateStore>,
    notify_tx: mpsc::UnboundedSender<crate::alarms::NotifyJob>,
    nudge_tx: broadcast::Sender<crate::alarms::AlarmNudge>,
}

/// What [`apply_config_warning`] did to a Node's warning row.
#[derive(Debug, Clone, PartialEq)]
enum WarningOutcome {
    Unchanged,
    Opened(portal_storage::AlarmEvent),
    Updated,
    Cleared(portal_storage::AlarmEvent),
}

/// Nodes (lowercase key) whose `cfgsync:` warning is open, for the monitoring
/// fault axis (config sync Q20c). Kept by [`apply_config_warning`]; the
/// deadline sweep refills it after a restart.
static CONFIG_SYNC_FAULTS: std::sync::LazyLock<StdMutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(Default::default);

/// Whether `node_key` counts a "settings not confirmed" fault in "Poruchy".
pub fn has_config_sync_fault(node_key: &str) -> bool {
    CONFIG_SYNC_FAULTS
        .lock()
        .map(|set| set.contains(&node_key.to_ascii_lowercase()))
        .unwrap_or(false)
}

fn note_config_sync_fault(node_key: &str, open: bool) {
    if let Ok(mut set) = CONFIG_SYNC_FAULTS.lock() {
        if open {
            set.insert(node_key.to_ascii_lowercase());
        } else {
            set.remove(&node_key.to_ascii_lowercase());
        }
    }
}

/// Opens, rewords or clears one Node's warning so the table matches `message` (`None` = no
/// warning). Idempotent: decided against the row open right now, under any spelling of the
/// Hub, so running it twice writes once.
fn apply_config_warning(
    storage: &Storage,
    hostname: &str,
    node_key: &str,
    identity: &NodeIdentity,
    message: Option<&str>,
) -> Result<WarningOutcome, StorageError> {
    let sensor_id = config_warning_sensor_id(node_key);
    let legacy = format!(
        "{LEGACY_CONFIG_WARNING_PREFIX}{}",
        node_key.to_ascii_lowercase()
    );
    let active = storage.active_alarms_any_case(hostname)?;
    let at = iso_now();
    for row in active
        .iter()
        .filter(|row| row.sensor_id.eq_ignore_ascii_case(&legacy))
    {
        storage.clear_alarm(&row.id, &at, None)?;
    }
    let open = active
        .into_iter()
        .find(|row| row.sensor_id.eq_ignore_ascii_case(&sensor_id));
    note_config_sync_fault(node_key, message.is_some());
    Ok(match (message, open) {
        (None, None) => WarningOutcome::Unchanged,
        (None, Some(open)) => {
            storage.clear_alarm(&open.id, &at, None)?;
            WarningOutcome::Cleared(open)
        }
        (Some(message), None) => {
            let event = portal_storage::NewAlarmEvent {
                hostname: hostname.to_string(),
                sensor_id,
                sensor_name: Some(identity.name.clone()),
                sensor_location: identity.location.clone(),
                alarm_type: CONFIG_WARNING_TYPE.to_string(),
                severity: "warning".to_string(),
                from_state: Some("NORMAL".to_string()),
                to_state: Some("WARNING".to_string()),
                message: Some(message.to_string()),
                // A configuration, not a measurement: nothing to put here.
                value: None,
                threshold: None,
                triggered_at: at,
                cleared_at: None,
                acknowledged_at: None,
                acknowledged_by_user_id: None,
                acknowledged_by_username: None,
                native_key: None,
            };
            let id = storage.insert_alarm_event(&event)?;
            match storage.get_alarm(&id)? {
                Some(row) => WarningOutcome::Opened(row),
                None => WarningOutcome::Updated,
            }
        }
        (Some(message), Some(open)) => {
            if open.message.as_deref() == Some(message) {
                WarningOutcome::Unchanged
            } else {
                storage.update_alarm_state(&open.id, "WARNING", Some(message))?;
                WarningOutcome::Updated
            }
        }
    })
}

/// The Node as the alarm list and the e-mail name it.
#[derive(Debug, Clone)]
struct NodeIdentity {
    name: String,
    location: Option<String>,
    serial_number: Option<String>,
}

/// What the warning says (PF-3). The bounds are in effect — evaluated from telemetry, the
/// primary alarm path — and only the Node's own copy, an extension, is missing.
fn config_warning_message(keys: &[String]) -> String {
    format!(
        "Limits could not be synced with the Node within {} h: {}. The Portal grades with \
         them already; send them again from the Node's pending settings.",
        NODE_CONFIG_DEADLINE.as_secs() / 3600,
        keys.join(", ")
    )
}

/// What the warning says when the Node took a discarded band (PF-3b).
fn config_differs_message(fields: &[String]) -> String {
    format!(
        "The Node applied a configuration that was discarded ({}): it holds other bounds than \
         the Hub and the portal. The portal drafted its own bounds again; send them from the \
         Node's configuration banner.",
        fields.join(", ")
    )
}

/// What the archived entry of undelivered config keys says (PF-3).
fn config_archive_message(keys: &[String]) -> String {
    format!(
        "The Node did not confirm its configuration in time: {}. Not in effect until it \
         does; the draft keeps Send again.",
        keys.join(", ")
    )
}

fn iso_now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

#[derive(Clone)]
pub struct PendingService {
    db: crate::db::Db,
    nudge_tx: broadcast::Sender<PendingNudge>,
    /// The Nodes a send walk is currently walking (§21 review, item 4).
    ///
    /// Two managers pressing Send at the same moment used to start two
    /// walks over one draft: both mark the same rows `sending`, both wait on
    /// the same `(node_key, command)` confirmations — which carry no request
    /// id, so either walk answers to either downlink — and the second
    /// deletes rows the first is still sending. This set is the whole guard:
    /// entry is claimed synchronously in the route before the task is
    /// spawned, and released by [`SendClaim`]'s `Drop` however the walk ends.
    ///
    /// A `HashSet` behind a `std::sync::Mutex` rather than a database flag:
    /// the question is "is a task in *this* process walking this Node", the
    /// same question `sending` answers wrongly across a restart.
    ///
    /// One lock with the Hub phase and the Discards beside it (PF-3 review
    /// round 2, item E), so a claim, the Hub phase it starts in and a Discard
    /// cannot slip between each other.
    activity: Arc<StdMutex<Activity>>,
    /// `(hostname, node_key)`, lowercase -> the `seq`s the walk queued on the radio for this
    /// Node's draft and no ack has cleared, so a Discard can take them back out of the
    /// ChirpStack queue (P-6, nodes_phase1.cs.md dodatek 2026-09-25). In-memory: after a
    /// restart the frames are the Node's to apply, and the late-apply reconcile covers them.
    queued: Arc<StdMutex<QueuedSeqs>>,
    /// Node units whose walk gave up at the budget and whose ack may still come (PF-3).
    /// In-memory like `queued`: a restart forgets them, and their rows stay `undelivered`
    /// with the warning open until the operator resends or discards.
    late: Arc<StdMutex<LateUnits>>,
    /// Where the configuration warning is announced, once [`Self::wire`] ran. `None` in tests;
    /// the warning row itself is written either way.
    warnings: Option<WarningSink>,
    /// Nodes whose config keys alone a restart turned from `sending` into `undelivered`, with
    /// those keys: the start-up pass leaves their archived warning (PF-3 review, item 9).
    restart_archive: Arc<StdMutex<Vec<ArchiveEntry>>>,
    /// How long a Node has to confirm its pending configuration before its `cfgsync:` fault
    /// opens ([`NODE_CONFIG_DEADLINE`]).
    deadline: Duration,
    /// A save starts the send walk itself (Q20a). Off under `cfg(test)` so
    /// route tests see the draft a save leaves.
    auto_send: bool,
    /// Nodes saved again while their walk was running, with who saved: the walk's end starts
    /// the next one, so the newest version goes out (config_sync.md §13 Q20b).
    resend: Arc<StdMutex<std::collections::HashMap<(String, String), crate::auth::CurrentUser>>>,
    /// The `seq`s of frames a Discard took back although the Node may have had them (PF-3b),
    /// by `(hostname, node_key)` lowercase, with when. The Node's answer to one of them still
    /// says what it holds. In memory like `late`, and for as long ([`LATE_ANSWER_WINDOW`]).
    discarded: Arc<StdMutex<std::collections::HashMap<(String, String), DiscardedSeqs>>>,
    /// Nodes whose last walk ended in the network server discarding a frame as too big for
    /// the data rate, and which were sent again once for it (#166). A second refusal in a
    /// row stays `undelivered`.
    size_retried: Arc<StdMutex<std::collections::HashSet<(String, String)>>>,
}

/// The network server's refusal of a frame larger than its data rate carries (#129), as
/// control-radio words it.
pub fn is_size_discard(message: &str) -> bool {
    message.contains("discarded by the network server")
        && message.contains("at the current data rate")
}

/// Frames a Discard took back that the Node may have had (PF-3b).
#[derive(Debug, Clone)]
struct DiscardedSeqs {
    seqs: Vec<u64>,
    at: std::time::Instant,
}

/// The author of the draft rows the portal writes itself when the Node proved it took a
/// discarded band (PF-3b): its own bounds again, for the operator to send.
pub const NODE_DIFFERS_AUTHOR: &str = "portal:node-differs";

/// What is happening to each Node's draft in this process, by `(hostname, node_key)` lowercase.
#[derive(Debug, Default)]
struct Activity {
    /// A walk is claimed for the Node (§21 review, item 4).
    walking: HashSet<(String, String)>,
    /// Its walk has not finished writing the Hub (PF-3 review, item 1): from the claim until the
    /// walk reaches the Node unit. A Discard waits for it — the Hub cannot be told to forget a
    /// write in flight.
    hub_phase: HashSet<(String, String)>,
    /// A Discard is working on the Node (PF-3 review round 2, item E). No walk starts meanwhile,
    /// so none can write the Hub while the Discard's revert does.
    discarding: HashSet<(String, String)>,
}

fn activity_key(hostname: &str, node_key: &str) -> (String, String) {
    (hostname.to_ascii_lowercase(), node_key.to_ascii_lowercase())
}

/// While alive, the walk of one Node is writing the Hub ([`PendingService::in_hub_phase`]).
pub struct HubPhase {
    activity: Arc<StdMutex<Activity>>,
    key: (String, String),
}

impl Drop for HubPhase {
    fn drop(&mut self) {
        self.activity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .hub_phase
            .remove(&self.key);
    }
}

/// While alive, a Discard holds the Node ([`PendingService::begin_discard`]).
pub struct DiscardGuard {
    activity: Arc<StdMutex<Activity>>,
    key: (String, String),
}

impl Drop for DiscardGuard {
    fn drop(&mut self) {
        self.activity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .discarding
            .remove(&self.key);
    }
}

/// A row that rides the Node unit, as the Hub unit of its field records it once the Hub takes
/// the band (PF-3): what the bound was before, and the field's whole band before.
#[derive(Debug, Clone)]
pub struct NodePrevious {
    pub row: SendRow,
    /// The bound before the send, as JSON scalar text.
    pub previous: String,
    /// The field's band before the send (all four bounds and the switch), as JSON.
    pub band: String,
}

/// A field's band as `previous_band` stores it (PF-3 review, item 3).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BandJson {
    pub critical_low: Option<f64>,
    pub warning_low: Option<f64>,
    pub warning_high: Option<f64>,
    pub critical_high: Option<f64>,
    pub enabled: bool,
}

/// The right to walk one Node's draft, released on drop.
///
/// Held by the spawned walk for its whole life, so every exit — a normal
/// return, an error, a panic, an abort — puts the Node back.
pub struct SendClaim {
    activity: Arc<StdMutex<Activity>>,
    key: (String, String),
}

impl Drop for SendClaim {
    fn drop(&mut self) {
        let mut guard = self
            .activity
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        guard.walking.remove(&self.key);
        guard.hub_phase.remove(&self.key);
    }
}

impl PendingService {
    /// Opens the shared database file as this service's own connection and
    /// clears any send the last process died in the middle of.
    ///
    /// The reset is here rather than in `main` because it is not optional:
    /// a `sending` row means "a task in this process is waiting for the
    /// device", and after a restart no such task exists. Leaving the row
    /// would show a spinner nothing can ever resolve.
    pub fn open(data_dir: &str, db_key: Option<&str>) -> Result<Self, StorageError> {
        std::fs::create_dir_all(data_dir)?;
        let path = Path::new(data_dir).join("portal.db");
        let storage = Storage::open(&path, db_key)?;
        // PF-3 review, item 9: config keys alone that a restart catches waiting for the Node get
        // the archived warning a timed-out walk would have left. Read before the reset turns
        // them `undelivered`.
        let restart_archive = restart_archive_of(
            &portal_storage::with_retry(|| storage.list_pending_in_state(PENDING_STATE_SENDING))
                .unwrap_or_default(),
        );
        match portal_storage::with_retry(|| {
            storage.reset_sending_to_error("portal restarted during send")
        }) {
            Ok(0) => {}
            Ok(n) => tracing::warn!(
                stranded = n,
                "pending config: send interrupted by a restart; drafts marked error"
            ),
            Err(err) => tracing::warn!(%err, "pending config: could not clear stranded sends"),
        }
        if let Err(err) = portal_storage::with_retry(|| storage.reset_node_delivery_pending()) {
            tracing::warn!(%err, "pending config: could not end the Nodes' pending alarm tables");
        }
        let (nudge_tx, _rx) = broadcast::channel(NUDGE_CHANNEL_CAPACITY);
        let service = Self {
            db: crate::db::Db::from_storage(storage, "pending"),
            nudge_tx,
            activity: Arc::new(StdMutex::new(Activity::default())),
            queued: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            late: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            warnings: None,
            restart_archive: Arc::new(StdMutex::new(Vec::new())),
            discarded: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            deadline: default_deadline(),
            auto_send: !cfg!(test),
            resend: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            size_retried: Arc::new(StdMutex::new(std::collections::HashSet::new())),
        };
        *service
            .restart_archive
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = restart_archive;
        Ok(service)
    }

    /// Whether a save starts the send walk itself.
    pub fn auto_send(&self) -> bool {
        self.auto_send
    }

    /// The same service with another delivery deadline ([`NODE_CONFIG_DEADLINE`]).
    #[cfg(test)]
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// The delivery deadline this service applies.
    #[cfg(test)]
    pub fn deadline(&self) -> Duration {
        self.deadline
    }

    /// A send of the Node's whole message: its deadline restarts and the attempt is counted
    /// (config_sync.md §13 Q11).
    pub async fn note_node_sent(&self, hostname: &str, node_key: &str) {
        let (h, n) = (hostname.to_string(), node_key.to_string());
        let window = i64::try_from(self.deadline.as_secs()).unwrap_or(i64::MAX / 2);
        if let Err(err) = self
            .db
            .write(move |s| s.note_node_config_sent(&h, &n, now_unix(), window))
            .await
        {
            tracing::warn!(%err, node_key, "the Node's delivery deadline was not recorded");
        }
    }

    /// The Node's delivery deadline, while a message waits for it.
    pub async fn node_deadline(
        &self,
        hostname: &str,
        node_key: &str,
    ) -> Option<portal_storage::NodeConfigDeadline> {
        let (h, n) = (hostname.to_string(), node_key.to_string());
        self.db
            .run(move |s| s.node_config_deadline(&h, &n))
            .await
            .ok()
            .flatten()
    }

    /// The sweep behind the `cfgsync:` fault (config_sync.md §13 Q10): every Node whose
    /// deadline row exists is synced, so a deadline that passed opens its fault and a Node
    /// with nothing left pending loses its row. Every minute, for the life of the process.
    pub fn spawn_deadline_sweep(&self) {
        let service = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                service.sweep_deadlines().await;
            }
        });
    }

    /// One pass of [`Self::spawn_deadline_sweep`].
    pub async fn sweep_deadlines(&self) {
        let rows = match self.db.run(|s| s.list_node_config_deadlines()).await {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(%err, "node config deadlines unreadable");
                return;
            }
        };
        for row in rows {
            self.sync_config_warning(&row.hostname, &row.node_key).await;
        }
    }

    /// Connects the configuration warning (PF-3) to the notifier and the alarm table. Call once
    /// from `main`, before the service is shared.
    pub fn wire(
        mut self,
        store: Arc<crate::state::StateStore>,
        notify_tx: mpsc::UnboundedSender<crate::alarms::NotifyJob>,
        nudge_tx: broadcast::Sender<crate::alarms::AlarmNudge>,
    ) -> Self {
        self.warnings = Some(WarningSink {
            store,
            notify_tx,
            nudge_tx,
        });
        self
    }

    /// P-6 (nodes_phase1.cs.md, dodatek 2026-09-25): a draft row that failed (the walk ran out
    /// of time) but whose value the Node later reports in its config info was applied late.
    /// It goes, as a confirmation would have removed it, with an audit row. Config keys only:
    /// the config info carries no alarm slots, so a threshold row cannot be matched this way.
    /// Since PF-3 such a row is `undelivered`, and clearing the last one closes the Node's
    /// configuration warning.
    pub async fn reconcile_late_applies(
        &self,
        hostname: &str,
        node_key: &str,
        config: &serde_json::Map<String, serde_json::Value>,
    ) -> usize {
        let rows = match self.list(hostname, node_key).await {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(%err, node_key, "late-apply reconcile: draft unreadable");
                return 0;
            }
        };
        let mut applied = 0usize;
        for row in rows {
            let failed = row.state == PENDING_STATE_ERROR || row.state == PENDING_STATE_UNDELIVERED;
            if !failed || row.kind != portal_storage::PENDING_KIND_CONFIG {
                continue;
            }
            let Some(desired) = row.desired_value.as_deref() else {
                continue;
            };
            let wanted: Option<serde_json::Value> = serde_json::from_str(desired).ok();
            let reported = config.get(&row.param_key);
            // `history_channels` compares as a set (MR !129 review): the Node may report the
            // same columns in another order.
            let same = match (wanted.as_ref(), reported) {
                (Some(serde_json::Value::String(w)), Some(serde_json::Value::String(r)))
                    if row.param_key == crate::render::HISTORY_CHANNELS_KEY =>
                {
                    crate::render::same_history_channels(w, r)
                }
                _ => json_equal(wanted.as_ref(), reported),
            };
            if !same {
                continue;
            }
            let (h, n, k, d) = (
                hostname.to_string(),
                node_key.to_string(),
                row.param_key.clone(),
                desired.to_string(),
            );
            let deleted = self
                .db
                .run(move |s| {
                    portal_storage::with_retry(|| s.delete_pending(&h, &n, &k, Some(Some(&d))))
                })
                .await
                .unwrap_or(false);
            if !deleted {
                continue;
            }
            applied += 1;
            let entry = portal_storage::NewAuditEntry {
                event_type: "node_config_applied_late".to_string(),
                username: Some("system:pending".to_string()),
                user_id: None,
                ip: None,
                target: Some(node_key.to_string()),
                details: serde_json::to_string(&serde_json::json!({
                    "hostname": hostname,
                    "key": row.param_key,
                    "value": desired,
                }))
                .ok(),
                success: true,
            };
            if let Err(err) = self.db.run(move |s| s.insert_audit(&entry)).await {
                tracing::warn!(%err, node_key, "late-apply reconcile: audit row not written");
            }
            tracing::info!(hostname, node_key, key = %row.param_key, "draft applied late by the Node; row reconciled");
        }
        if applied > 0 {
            self.sync_config_warning(hostname, node_key).await;
            self.nudge(hostname, node_key);
        }
        applied
    }

    /// Runs [`Self::reconcile_late_applies`] on every config info the store takes in.
    pub fn spawn_late_apply(&self, store: Arc<crate::state::StateStore>) {
        let service = self.clone();
        let mut changes = store.subscribe();
        tokio::spawn(async move {
            loop {
                match changes.recv().await {
                    Ok(crate::state::ChangeEvent::NodeConfig { hostname, node_key }) => {
                        // NT-4 (night test 2026-09-26): the event carries the hostname as the
                        // topic spells it (`fiber-4e970f4a`), the draft rows are stored under
                        // the Hub's display spelling (`DeviceState::hostname`, what
                        // `hostname_for_known_node` gives the routes), and storage matches
                        // exactly, so on a Hub named `FIBER-4E970F4A` nothing was reconciled.
                        let found = store.snapshot_device(&hostname).and_then(|d| {
                            d.node_configs
                                .get(&node_key)
                                .map(|e| (d.hostname.clone(), e.config.clone()))
                        });
                        if let Some((display, config)) = found {
                            service
                                .reconcile_late_applies(&display, &node_key, &config)
                                .await;
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    /// Records the `seq`s a walk queued for this Node (P-6).
    pub fn remember_queued(&self, hostname: &str, node_key: &str, seqs: &[u64]) {
        let key = (hostname.to_ascii_lowercase(), node_key.to_ascii_lowercase());
        let mut map = self.queued.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(key).or_default();
        entry.extend_from_slice(seqs);
        // A Node's draft is one walk at a time; a long tail is only stale sends.
        if entry.len() > 32 {
            let cut = entry.len() - 32;
            entry.drain(..cut);
        }
    }

    /// The `seq`s queued for this Node's draft, forgotten as they are taken (P-6).
    pub fn take_queued(&self, hostname: &str, node_key: &str) -> Vec<u64> {
        let key = (hostname.to_ascii_lowercase(), node_key.to_ascii_lowercase());
        self.queued
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key)
            .unwrap_or_default()
    }

    /// Remembers a Node unit the walk gave up on at its budget (PF-3). It replaces any earlier
    /// one for the Node, and every unit past [`LATE_ANSWER_WINDOW`] is dropped on the way.
    pub fn remember_late(&self, unit: LateUnit) {
        let key = (
            unit.hostname.to_ascii_lowercase(),
            unit.node_key.to_ascii_lowercase(),
        );
        let now = std::time::Instant::now();
        let mut map = self.late.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, u| now.saturating_duration_since(u.given_up_at) < LATE_ANSWER_WINDOW);
        map.insert(key, unit);
    }

    /// Forgets this Node's late unit (PF-3): the draft was discarded, or a new walk is about to
    /// send it again under new `seq`s. `true` = there was one.
    pub fn forget_late(&self, hostname: &str, node_key: &str) -> bool {
        let key = (hostname.to_ascii_lowercase(), node_key.to_ascii_lowercase());
        self.late
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key)
            .is_some()
    }

    /// Correlates one control.radio answer with this Node's late unit: `Some` once the last of
    /// its `seq`s has an ok answer, which also forgets it. A refusal forgets it and returns it
    /// at once, for [`Self::late_refused`]. An answer past [`LATE_ANSWER_WINDOW`] is `None`.
    fn take_late_answer(
        &self,
        hostname: &str,
        node_key: &str,
        seq: u64,
        ok: bool,
        now: std::time::Instant,
    ) -> Option<LateUnit> {
        let key = (hostname.to_ascii_lowercase(), node_key.to_ascii_lowercase());
        let mut map = self.late.lock().unwrap_or_else(|e| e.into_inner());
        let unit = map.get_mut(&key)?;
        if now.saturating_duration_since(unit.given_up_at) >= LATE_ANSWER_WINDOW {
            map.remove(&key);
            return None;
        }
        if !unit.unanswered.contains(&seq) {
            return None;
        }
        if !ok {
            let unit = map.remove(&key)?;
            tracing::info!(
                hostname = %unit.hostname,
                node_key = %unit.node_key,
                seq,
                "the Node refused a unit after the walk gave up on it; its rows turn error"
            );
            return Some(unit);
        }
        unit.unanswered.retain(|s| *s != seq);
        if unit.unanswered.is_empty() {
            map.remove(&key)
        } else {
            None
        }
    }

    /// PF-3 (nodes_phase1.cs.md dodatek 2026-09-27): a command result that answers a Node unit
    /// the walk gave up on at its budget. The same correlation as the walk's own
    /// ([`await_radio_acks`]): a `control.radio` answer for one of the unit's `seq`s, ok or
    /// not. `true` = it finished the unit.
    pub async fn apply_late_answer(
        &self,
        hostname: &str,
        node_key: &str,
        payload: &serde_json::Value,
    ) -> bool {
        self.apply_late_answer_at(hostname, node_key, payload, std::time::Instant::now())
            .await
    }

    async fn apply_late_answer_at(
        &self,
        hostname: &str,
        node_key: &str,
        payload: &serde_json::Value,
        now: std::time::Instant,
    ) -> bool {
        if payload.get("transport").and_then(serde_json::Value::as_str) != Some("control.radio") {
            return false;
        }
        let Some(seq) = payload.get("seq").and_then(serde_json::Value::as_u64) else {
            return false;
        };
        let result = payload.get("result").and_then(serde_json::Value::as_str);
        // The Hub's note that it gave a frame up is no answer from the Node: a unit the walk
        // gave up on still waits for the Node's own (§10.5, amended 2026-10-07).
        if result == Some(crate::relay::error_code::UNDELIVERED) {
            return false;
        }
        let ok = result.is_some_and(|r| crate::sse::NODE_COMMAND_OK_RESULTS.contains(&r));
        if self.take_discarded_answer(hostname, node_key, seq, now) {
            return self.discarded_answered(hostname, node_key, seq, ok).await;
        }
        if !ok {
            return self
                .late_refused(hostname, node_key, seq, payload, now)
                .await;
        }
        match self.take_late_answer(hostname, node_key, seq, ok, now) {
            Some(unit) => self.finish_late(unit).await,
            None => false,
        }
    }

    /// The Node refused a unit the walk gave up on: the whole message was rolled back, so its
    /// rows turn `error` with the field it named, and the fault stays (STICKER 2.0.0).
    async fn late_refused(
        &self,
        hostname: &str,
        node_key: &str,
        seq: u64,
        payload: &serde_json::Value,
        now: std::time::Instant,
    ) -> bool {
        let Some(unit) = self.take_late_answer(hostname, node_key, seq, false, now) else {
            return false;
        };
        let message = refusal_message(refusal_field(payload).as_deref());
        for row in &unit.rows {
            if let Err(err) = self
                .set_state(
                    hostname,
                    node_key,
                    &row.param_key,
                    Some(row.desired_value.clone()),
                    PENDING_STATE_ERROR,
                    Some(message.clone()),
                    false,
                )
                .await
            {
                tracing::warn!(%err, param_key = %row.param_key, "late refusal not recorded");
            }
        }
        self.sync_config_warning(hostname, node_key).await;
        self.nudge(hostname, node_key);
        true
    }

    /// Whether `seq` is one of the frames a Discard took back from this Node (PF-3b); a match
    /// forgets them, since the Node has now answered for the discarded send.
    fn take_discarded_answer(
        &self,
        hostname: &str,
        node_key: &str,
        seq: u64,
        now: std::time::Instant,
    ) -> bool {
        let key = activity_key(hostname, node_key);
        let mut map = self.discarded.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = map.get(&key) else {
            return false;
        };
        if now.saturating_duration_since(entry.at) >= LATE_ANSWER_WINDOW {
            map.remove(&key);
            return false;
        }
        if !entry.seqs.contains(&seq) {
            return false;
        }
        map.remove(&key);
        true
    }

    /// The Node answered a frame a Discard took back (PF-3b). A refusal: it holds what it last
    /// confirmed. An ok: it took the discarded band, and where that differs from the portal's
    /// it `differs` — the configuration warning opens, loudly, and the portal drafts its own
    /// bounds of those fields again, so one Send puts them back on the Node.
    ///
    /// The re-draft never overrides the operator (PF-3b review): only a key with no row is
    /// drafted, and nothing at all while a walk or a Discard holds the Node — a row written
    /// under a walk would stop it as superseded. `differs` and the warning are set either way,
    /// and while a field `differs` the form drafts its held bounds too (N17's exception,
    /// [`Self::field_differs`]), so the operator can always put them back.
    async fn discarded_answered(&self, hostname: &str, node_key: &str, seq: u64, ok: bool) -> bool {
        let (h, n) = (hostname.to_string(), node_key.to_string());
        if !ok {
            if let Err(err) = self
                .db
                .write(move |s| s.forget_node_delivery_unknown(&h, &n))
                .await
            {
                tracing::warn!(%err, node_key, "a refused discarded frame left its band unknown");
            }
            self.nudge(hostname, node_key);
            return true;
        }
        let portal: Vec<portal_storage::NodeBand> = self
            .held_thresholds(hostname, node_key)
            .await
            .into_iter()
            .map(|t| portal_storage::NodeBand {
                field: t.field,
                low: t.critical_low,
                high: t.critical_high,
                enabled: t.enabled,
            })
            .collect();
        let differs = match self
            .db
            .write({
                let portal = portal.clone();
                move |s| s.node_took_discarded(&h, &n, now_unix(), &portal)
            })
            .await
        {
            Ok(differs) => differs,
            Err(err) => {
                tracing::warn!(%err, node_key, "a discarded frame the Node took was not recorded");
                return false;
            }
        };
        let busy = self.node_is_busy(hostname, node_key);
        for band in differs.iter().filter(|_| !busy) {
            let held = portal.iter().find(|p| p.field == band.field);
            for (bound, value) in [
                ("critical_low", held.and_then(|p| p.low)),
                ("critical_high", held.and_then(|p| p.high)),
            ] {
                let row = NewPendingConfig {
                    hostname: hostname.to_string(),
                    node_key: node_key.to_string(),
                    param_key: threshold_param_key(&band.field, bound),
                    kind: PENDING_KIND_THRESHOLD.to_string(),
                    desired_value: Some(
                        serde_json::to_string(&value).unwrap_or_else(|_| "null".into()),
                    ),
                    author: NODE_DIFFERS_AUTHOR.to_string(),
                };
                if let Err(err) = self
                    .db
                    .run(move |s| portal_storage::with_retry(|| s.insert_pending_if_absent(&row)))
                    .await
                {
                    tracing::warn!(%err, node_key, "the portal's bounds were not drafted again");
                }
            }
        }
        let fields: Vec<&str> = differs.iter().map(|b| b.field.as_str()).collect();
        let entry = portal_storage::NewAuditEntry {
            event_type: "node_config_applied_late".to_string(),
            username: Some("system:pending".to_string()),
            user_id: None,
            ip: None,
            target: Some(node_key.to_string()),
            details: serde_json::to_string(&json!({
                "hostname": hostname,
                "via": "discarded",
                "seq": seq,
                "differs": fields,
            }))
            .ok(),
            success: true,
        };
        if let Err(err) = self.db.run(move |s| s.insert_audit(&entry)).await {
            tracing::warn!(%err, node_key, "discarded answer: audit row not written");
        }
        tracing::warn!(hostname, node_key, seq, differs = ?fields, "the Node took a discarded configuration");
        self.sync_config_warning(hostname, node_key).await;
        self.nudge(hostname, node_key);
        true
    }

    /// Finishes a late unit as a timely ack would have: the rows still holding the value that
    /// was sent go, the alarm layout is recorded, and the warning closes once nothing of the
    /// Node is `undelivered` any more. The Hub and `field_thresholds` already hold the bounds:
    /// the walk wrote them before it sent the Node its part.
    async fn finish_late(&self, unit: LateUnit) -> bool {
        let (hostname, node_key) = (unit.hostname.as_str(), unit.node_key.as_str());
        // The Node took what was sent, whatever the draft says since (PF-3 review, item 8): its
        // table and its bands are recorded as they are on the Node. Only rows still holding the
        // sent value go — an edit since is a newer intention and stays for the next Send.
        let mut cleared = Vec::new();
        for row in &unit.rows {
            match self
                .delete(
                    hostname,
                    node_key,
                    &row.param_key,
                    Some(row.desired_value.clone()),
                )
                .await
            {
                Ok(true) => cleared.push(row.param_key.clone()),
                Ok(false) => {}
                Err(err) => tracing::warn!(
                    %err,
                    param_key = %row.param_key,
                    "late ack: the row could not be cleared; it stays undelivered"
                ),
            }
        }
        WriteThrough {
            thresholds: Vec::new(),
            alarm_layout: unit.alarm_layout.clone(),
        }
        .apply(&self.db, hostname, node_key, None)
        .await;
        if unit.alarm_layout.is_some() {
            self.delivery_confirmed(hostname, node_key, &unit.node_bands)
                .await;
        }
        // Delivered and answered: nothing of it is left in the ChirpStack queue to withdraw.
        self.take_queued(hostname, node_key);

        let entry = portal_storage::NewAuditEntry {
            event_type: "node_config_applied_late".to_string(),
            username: Some("system:pending".to_string()),
            user_id: None,
            ip: None,
            target: Some(node_key.to_string()),
            details: serde_json::to_string(&json!({
                "hostname": hostname,
                "via": "ack",
                "unit": unit.label,
                "seqs": unit.seqs,
                "keys": cleared,
            }))
            .ok(),
            success: true,
        };
        if let Err(err) = self.db.run(move |s| s.insert_audit(&entry)).await {
            tracing::warn!(%err, node_key, "late ack: audit row not written");
        }
        tracing::info!(hostname, node_key, unit = %unit.label, "the Node took a unit after the walk gave up on it");
        self.sync_config_warning(hostname, node_key).await;
        self.nudge(hostname, node_key);
        true
    }

    /// Runs [`Self::apply_late_answer`] on every command result the store takes in (PF-3), after
    /// one pass that opens the warning of every Node a restart left `undelivered`
    /// ([`Self::sync_undelivered_warnings`]).
    ///
    /// On the narrow command-result stream, as the walk is: a late ack is one message among
    /// the fleet's telemetry, and on the main broadcast a busy fleet could lap it.
    pub fn spawn_late_answers(&self, store: Arc<crate::state::StateStore>) {
        let service = self.clone();
        let mut results = store.subscribe_command_results();
        tokio::spawn(async move {
            service.sync_undelivered_warnings().await;
            loop {
                match results.recv().await {
                    Ok(crate::state::ChangeEvent::NodeCommandResult {
                        hostname,
                        node_key,
                        payload,
                    }) => {
                        service
                            .apply_late_answer(&hostname, &node_key, &payload)
                            .await;
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(
                            missed = n,
                            "late answers: command-result subscriber lagged"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    /// Opens the warning of every Node with an `undelivered` row (PF-3). The warning row is
    /// persistent and survives a restart by itself; this covers the rows a restart turned from
    /// `sending` into `undelivered` (`Storage::reset_sending_to_error`), whose walk never got
    /// to open it.
    pub async fn sync_undelivered_warnings(&self) {
        let archive = std::mem::take(
            &mut *self
                .restart_archive
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        for (hostname, node_key, keys) in archive {
            self.archive_config_warning(&hostname, &node_key, &keys)
                .await;
        }
        let nodes = match self.db.run(|s| s.list_undelivered_nodes()).await {
            Ok(nodes) => nodes,
            Err(err) => {
                tracing::warn!(%err, "configuration warnings: undelivered drafts unreadable");
                return;
            }
        };
        for (hostname, node_key) in nodes {
            self.sync_config_warning(&hostname, &node_key).await;
        }
    }

    /// Makes the Node's "configuration not delivered" warning match its draft (PF-3): open,
    /// naming the keys, while any **threshold** row is `undelivered`; cleared once none is.
    /// Undelivered config keys alone get an archived entry instead, at the walk's failure
    /// ([`Self::archive_config_warning`]). Its onset and its clear go out through the notifier
    /// like any alarm's; the onset pops up, because the warning asks the operator to act.
    pub async fn sync_config_warning(&self, hostname: &str, node_key: &str) {
        self.sync_config_warning_as(hostname, node_key, false).await;
    }

    /// [`Self::sync_config_warning`]; `quiet` announces neither a close nor an open — the ones an
    /// operator's own edit of the draft causes (PF-3 review rounds 2 and 3): no e-mail, no
    /// pop-up. The alarm table still repaints.
    async fn sync_config_warning_as(&self, hostname: &str, node_key: &str, quiet: bool) {
        let identity = self.node_identity(hostname, node_key);
        let (h, n, who) = (hostname.to_string(), node_key.to_string(), identity.clone());
        let window = i64::try_from(self.deadline.as_secs()).unwrap_or(i64::MAX / 2);
        // The draft is read and the warning written under one hold of the connection (PF-3
        // review, item 7), so a draft edit cannot land between what was read and what was
        // written.
        let outcome = match self
            .db
            .run(move |s| {
                portal_storage::with_retry(|| {
                    let rows = s.list_pending(&h, &n)?;
                    // Q20 follow-up (Hynek 2026-10-15): an `error` row — the Hub was
                    // offline or refused — counts like an undelivered one.
                    let undelivered: Vec<String> = rows
                        .iter()
                        .filter(|r| {
                            r.state == PENDING_STATE_UNDELIVERED || r.state == PENDING_STATE_ERROR
                        })
                        .map(|r| r.param_key.clone())
                        .collect();
                    let bound_undelivered = rows.iter().any(|r| {
                        (r.state == PENDING_STATE_UNDELIVERED && r.kind == PENDING_KIND_THRESHOLD)
                            || r.state == PENDING_STATE_ERROR
                    });
                    // PF-3b: the Node took a discarded band and holds another than the portal.
                    let differs = s.node_delivery_differs(&h, &n)?;
                    // config_sync.md §13 Q10–Q11: nothing left to confirm ends the deadline; an
                    // undelivered bound is a fault only once its deadline passed. A bound with no
                    // deadline row (from before V43) starts its deadline now.
                    let now = now_unix();
                    if rows.is_empty() {
                        s.clear_node_config_deadline(&h, &n)?;
                    }
                    let expired = bound_undelivered
                        && match s.node_config_deadline(&h, &n)? {
                            Some(d) => d.deadline_at <= now,
                            None => {
                                s.note_node_config_sent(&h, &n, now, window)?;
                                window <= 0
                            }
                        };
                    let message = if !differs.is_empty() {
                        Some(config_differs_message(&differs))
                    } else {
                        expired.then(|| config_warning_message(&undelivered))
                    };
                    apply_config_warning(s, &h, &n, &who, message.as_deref())
                })
            })
            .await
        {
            Ok(outcome) => outcome,
            Err(err) => {
                tracing::warn!(%err, node_key, "configuration warning not written");
                return;
            }
        };
        let Some(sink) = &self.warnings else {
            return;
        };
        let (activated_ids, job) = match &outcome {
            WarningOutcome::Unchanged => return,
            WarningOutcome::Updated => (Vec::new(), None),
            WarningOutcome::Opened(_) if quiet => (Vec::new(), None),
            WarningOutcome::Opened(row) => (
                vec![row.id.clone()],
                Some(crate::alarms::NotifyJob::Activated(warning_notify_data(
                    row, node_key, &identity,
                ))),
            ),
            WarningOutcome::Cleared(_) if quiet => (Vec::new(), None),
            WarningOutcome::Cleared(row) => (
                Vec::new(),
                Some(crate::alarms::NotifyJob::Cleared {
                    data: warning_notify_data(row, node_key, &identity),
                    ack: row
                        .acknowledged_by_username
                        .clone()
                        // No name lookup here: `name` is display only and falls back to the
                        // username, as persist.rs's does on a failed lookup.
                        .map(|user| crate::notify::payload::AckInfo {
                            name: user.clone(),
                            user,
                            reason: None,
                        }),
                }),
            ),
        };
        if let Some(job) = job {
            let _ = sink.notify_tx.send(job);
        }
        let _ = sink.nudge_tx.send(crate::alarms::AlarmNudge {
            hostname: hostname.to_string(),
            activated_ids,
            escalated_ids: Vec::new(),
        });
    }

    /// A table carrying `bands` is on its way to the Node (PF-3).
    async fn delivery_pending(
        &self,
        hostname: &str,
        node_key: &str,
        bands: &[portal_storage::NodeBand],
    ) {
        let (h, n, bands) = (hostname.to_string(), node_key.to_string(), bands.to_vec());
        if let Err(err) = self
            .db
            .write(move |s| s.mark_node_delivery_pending(&h, &n, &bands, now_unix()))
            .await
        {
            tracing::warn!(%err, node_key, "the Node's alarm table was not recorded as on its way");
        }
    }

    /// The Node confirmed a table carrying `bands` (PF-3): "on the Node: yes", with the time.
    async fn delivery_confirmed(
        &self,
        hostname: &str,
        node_key: &str,
        bands: &[portal_storage::NodeBand],
    ) {
        let (h, n, bands) = (hostname.to_string(), node_key.to_string(), bands.to_vec());
        if let Err(err) = self
            .db
            .write(move |s| s.confirm_node_delivery(&h, &n, &bands, now_unix()))
            .await
        {
            tracing::warn!(%err, node_key, "the Node's confirmed alarm table was not recorded");
        }
    }

    /// The table on its way did not land (PF-3): "on the Node: no, not delivered".
    async fn delivery_undelivered(&self, hostname: &str, node_key: &str) {
        let (h, n) = (hostname.to_string(), node_key.to_string());
        if let Err(err) = self
            .db
            .write(move |s| s.mark_node_delivery_undelivered(&h, &n))
            .await
        {
            tracing::warn!(%err, node_key, "the Node's undelivered alarm table was not recorded");
        }
    }

    /// Leaves an already-closed warning in the alarm history (PF-3, decision Hynek 2026-09-27):
    /// a Node unit of config keys alone (intervals) did not land. There is a trace, and nothing
    /// that asks for action — the draft rows keep "Send again", and the P-6 reconcile clears
    /// them if the Node applies the keys late. No e-mail and no pop-up, only a repaint.
    async fn archive_config_warning(&self, hostname: &str, node_key: &str, keys: &[String]) {
        let identity = self.node_identity(hostname, node_key);
        let at = iso_now();
        let event = portal_storage::NewAlarmEvent {
            hostname: hostname.to_string(),
            sensor_id: config_warning_sensor_id(node_key),
            sensor_name: Some(identity.name.clone()),
            sensor_location: identity.location.clone(),
            alarm_type: CONFIG_WARNING_TYPE.to_string(),
            severity: "warning".to_string(),
            from_state: Some("NORMAL".to_string()),
            to_state: Some("WARNING".to_string()),
            message: Some(config_archive_message(keys)),
            value: None,
            threshold: None,
            triggered_at: at.clone(),
            cleared_at: Some(at),
            acknowledged_at: None,
            acknowledged_by_user_id: None,
            acknowledged_by_username: None,
            native_key: None,
        };
        if let Err(err) = self.db.write(move |s| s.insert_alarm_event(&event)).await {
            tracing::warn!(%err, node_key, "the undelivered configuration left no trace in the alarm history");
            return;
        }
        if let Some(sink) = &self.warnings {
            let _ = sink.nudge_tx.send(crate::alarms::AlarmNudge {
                hostname: hostname.to_string(),
                activated_ids: Vec::new(),
                escalated_ids: Vec::new(),
            });
        }
    }

    /// The Node's name, location and serial as the live snapshot holds them, the DevEUI when it
    /// holds none.
    fn node_identity(&self, hostname: &str, node_key: &str) -> NodeIdentity {
        let node = self.warnings.as_ref().and_then(|sink| {
            sink.store
                .snapshot_device(hostname)
                .and_then(|d| d.nodes.get(node_key).cloned())
        });
        NodeIdentity {
            name: node
                .as_ref()
                .and_then(|n| n.name.clone())
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| node_key.to_string()),
            location: node.as_ref().and_then(|n| n.location.clone()),
            serial_number: node.and_then(|n| n.serial_number),
        }
    }

    /// **Test fixture only**: a service over an already-open `Storage`,
    /// skipping `open`'s migrate-and-clear.
    ///
    /// It exists so a test can hand the walk a store whose writes *fail* —
    /// `Storage::open_at_version` at V12, before V13 created
    /// `pending_config`, makes every draft statement return "no such
    /// table". There is no other way to produce a storage error from
    /// outside: the connection is private and every method that touches it
    /// works.
    #[cfg(test)]
    fn from_storage(storage: Storage) -> Self {
        let (nudge_tx, _rx) = broadcast::channel(NUDGE_CHANNEL_CAPACITY);
        Self {
            db: crate::db::Db::from_storage(storage, "pending"),
            nudge_tx,
            activity: Arc::new(StdMutex::new(Activity::default())),
            queued: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            late: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            warnings: None,
            restart_archive: Arc::new(StdMutex::new(Vec::new())),
            discarded: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            deadline: default_deadline(),
            auto_send: !cfg!(test),
            resend: Arc::new(StdMutex::new(std::collections::HashMap::new())),
            size_retried: Arc::new(StdMutex::new(std::collections::HashSet::new())),
        }
    }

    /// How a walk ended, for the one automatic resend after a too-big discard (#166). `true`
    /// when this walk's Node part was discarded by the network server for its size and the
    /// previous walk's was not: send it again, now packed into what the NS said it carries
    /// (control-radio keeps that cap). Any other ending resets the Node.
    pub fn retry_after_size_discard(
        &self,
        hostname: &str,
        node_key: &str,
        stop: Option<&SendStop>,
    ) -> bool {
        let key = activity_key(hostname, node_key);
        let mut retried = self
            .size_retried
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match stop {
            Some(SendStop::NodeUndelivered { message, .. }) if is_size_discard(message) => {
                retried.insert(key)
            }
            _ => {
                retried.remove(&key);
                false
            }
        }
    }

    /// Claims the right to walk this Node's draft, or `None` when a walk is
    /// already running for it (§21 review, item 4).
    ///
    /// Synchronous and atomic on purpose: the route calls it *before*
    /// spawning, so two requests that arrive in the same millisecond cannot
    /// both get past it.
    pub fn claim_send(&self, hostname: &str, node_key: &str) -> Option<SendClaim> {
        let key = activity_key(hostname, node_key);
        let mut guard = self
            .activity
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // A Discard at work holds the Node too (PF-3 review round 2, item E).
        if guard.discarding.contains(&key) || !guard.walking.insert(key.clone()) {
            return None;
        }
        // The walk starts by writing the Hub; until it moves on to the Node a Discard waits.
        guard.hub_phase.insert(key.clone());
        drop(guard);
        // PF-3 review, item 5: the late-ack memory is not forgotten here. The new walk takes the
        // earlier attempt out of the Hub's queue first, and only a frame that came back out
        // can no longer be answered; one already delivered still finishes the unit.
        Some(SendClaim {
            activity: Arc::clone(&self.activity),
            key,
        })
    }

    /// A walk is claimed for the Node right now.
    pub fn is_walking(&self, hostname: &str, node_key: &str) -> bool {
        self.activity
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .walking
            .contains(&activity_key(hostname, node_key))
    }

    /// The Node was saved again while its walk runs: the walk's end sends the newest version
    /// (config_sync.md §13 Q20b), as `user`.
    pub fn want_resend(&self, hostname: &str, node_key: &str, user: crate::auth::CurrentUser) {
        self.resend
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(activity_key(hostname, node_key), user);
    }

    /// Who asked for a send after the walk that just ended, if anyone did.
    pub fn take_resend(&self, hostname: &str, node_key: &str) -> Option<crate::auth::CurrentUser> {
        self.resend
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&activity_key(hostname, node_key))
    }

    /// Holds the Node for a Discard (PF-3 review round 2, item E), or `None` while its walk is
    /// writing the Hub or another Discard is at work. While held, no walk can be claimed; a walk
    /// already waiting for the Node may be cut short by the Discard (N30).
    pub fn begin_discard(&self, hostname: &str, node_key: &str) -> Option<DiscardGuard> {
        let key = activity_key(hostname, node_key);
        let mut guard = self.activity.lock().unwrap_or_else(|e| e.into_inner());
        if guard.hub_phase.contains(&key) || !guard.discarding.insert(key.clone()) {
            return None;
        }
        Some(DiscardGuard {
            activity: Arc::clone(&self.activity),
            key,
        })
    }

    /// A walk is claimed for this Node, or a Discard holds it (PF-3b review).
    fn node_is_busy(&self, hostname: &str, node_key: &str) -> bool {
        let key = activity_key(hostname, node_key);
        let guard = self.activity.lock().unwrap_or_else(|e| e.into_inner());
        guard.walking.contains(&key) || guard.discarding.contains(&key)
    }

    /// The Node took a discarded band of `field` and holds another than the portal (PF-3b).
    /// While it does, an edit back to the held bounds drafts them rather than dropping the row
    /// (N17's exception): one Send is how the operator puts the portal's band back.
    pub async fn field_differs(&self, hostname: &str, node_key: &str, field: &str) -> bool {
        let (h, n) = (hostname.to_string(), node_key.to_string());
        self.db
            .run(move |s| s.node_delivery_differs(&h, &n))
            .await
            .map(|fields| fields.iter().any(|f| f == field))
            .unwrap_or(false)
    }

    /// The walk of this Node is writing the Hub right now (PF-3 review, item 1).
    pub fn in_hub_phase(&self, hostname: &str, node_key: &str) -> bool {
        self.activity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .hub_phase
            .contains(&activity_key(hostname, node_key))
    }

    pub(crate) fn enter_hub_phase(&self, hostname: &str, node_key: &str) -> HubPhase {
        let key = activity_key(hostname, node_key);
        self.activity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .hub_phase
            .insert(key.clone());
        HubPhase {
            activity: Arc::clone(&self.activity),
            key,
        }
    }

    /// Takes this Node's late unit out of memory, for a Discard's revert (PF-3 review round 2,
    /// item D): a late ack must not finish rows mid-revert, and a revert that did not happen
    /// puts it back ([`Self::restore_late`]).
    pub fn take_late(&self, hostname: &str, node_key: &str) -> Option<LateUnit> {
        self.late
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&activity_key(hostname, node_key))
    }

    /// Puts back a late unit [`Self::take_late`] took, unless a newer one was remembered since.
    pub fn restore_late(&self, unit: LateUnit) {
        let key = activity_key(&unit.hostname, &unit.node_key);
        self.late
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key)
            .or_insert(unit);
    }

    /// The Node's `field_thresholds` rows — what the Hub holds, mirrored.
    async fn held_thresholds(
        &self,
        hostname: &str,
        node_key: &str,
    ) -> Vec<portal_storage::FieldThreshold> {
        let (h, n) = (hostname.to_string(), node_key.to_string());
        self.db
            .run(move |s| s.list_field_thresholds(&h, portal_storage::THRESHOLD_SCOPE_NODE, &n))
            .await
            .unwrap_or_default()
    }

    /// A Discard takes this Node's frames back out of the Hub's queue (P-6), **a delivered one
    /// too** (PF-3b): the Hub cannot know whether the Node took a `0x56` in its RX1, and a
    /// discarded `SetParam` re-delivered later is always wrong. When the Node may have one of
    /// them — delivered, or no longer queued — its seqs are remembered, so its answer still
    /// tells ([`Self::apply_late_answer`]). `seqs` are the ones this Node's sends left behind.
    async fn withdraw_for_discard(
        &self,
        sender: &dyn UnitSender,
        hostname: &str,
        node_key: &str,
        seqs: Vec<u64>,
    ) -> crate::radio::Withdrawal {
        let queued = self.take_queued(hostname, node_key);
        let mut all = seqs;
        for seq in queued {
            if !all.contains(&seq) {
                all.push(seq);
            }
        }
        if all.is_empty() {
            return crate::radio::Withdrawal::default();
        }
        let withdrawal = match sender.withdraw_all(node_key, &all).await {
            Ok(w) => w,
            Err(err) => {
                tracing::warn!(%err, node_key, "could not take the draft's frames out of the queue");
                crate::radio::Withdrawal {
                    dropped: 0,
                    maybe_delivered: true,
                }
            }
        };
        if withdrawal.maybe_delivered {
            self.discarded
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    activity_key(hostname, node_key),
                    DiscardedSeqs {
                        seqs: all,
                        at: std::time::Instant::now(),
                    },
                );
        }
        withdrawal
    }

    /// The `seq`s this Node's sends left behind: still queued, or remembered for a late ack.
    fn seqs_left(&self, hostname: &str, node_key: &str) -> Vec<u64> {
        let key = activity_key(hostname, node_key);
        let mut seqs = self
            .queued
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
            .unwrap_or_default();
        if let Some(late) = self
            .late
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            for seq in &late.seqs {
                if !seqs.contains(seq) {
                    seqs.push(*seq);
                }
            }
        }
        seqs
    }

    /// What the Node holds after a Discard's withdrawal (PF-3b): a band on its way is `unknown`
    /// when the Node may have had a frame, and back to its last confirmation otherwise.
    /// `fields` = only those (a partial revert); `None` = every band on its way.
    async fn settle_discarded_delivery(
        &self,
        hostname: &str,
        node_key: &str,
        fields: Option<Vec<String>>,
        maybe_delivered: bool,
    ) {
        let (h, n) = (hostname.to_string(), node_key.to_string());
        let written = self
            .db
            .write(move |s| {
                if maybe_delivered {
                    s.mark_node_delivery_unknown(&h, &n, fields.as_deref())
                        .map(|_| ())
                } else if let Some(fields) = &fields {
                    for field in fields {
                        s.forget_node_delivery_field(&h, &n, field)?;
                    }
                    Ok(())
                } else {
                    s.forget_node_delivery_attempt(&h, &n)
                }
            })
            .await;
        if let Err(err) = written {
            tracing::warn!(%err, node_key, "discard: the Node's record kept its attempt");
        }
    }

    /// Every row still holds the value the walk planned with.
    async fn still_planned(&self, hostname: &str, node_key: &str, planned: &[SendRow]) -> bool {
        if planned.is_empty() {
            return true;
        }
        match self.list(hostname, node_key).await {
            Ok(rows) => planned.iter().all(|sent| {
                rows.iter().any(|row| {
                    row.param_key == sent.param_key && row.desired_value == sent.desired_value
                })
            }),
            Err(err) => {
                tracing::warn!(%err, node_key, "pending config: draft unreadable before the Hub write");
                false
            }
        }
    }

    /// Records a Hub unit the Hub took, as one write (`Storage::land_hub_threshold`). `true` =
    /// the draft was still the walk's plan.
    async fn land_hub(
        &self,
        hostname: &str,
        node_key: &str,
        unit: &SendUnit,
        actor: &str,
    ) -> Result<bool, StorageError> {
        let keep = unit.threshold.as_ref().is_some_and(|t| !t.warnings_drafted);
        let threshold = unit
            .threshold
            .clone()
            .map(|t| portal_storage::NewFieldThreshold {
                hostname: hostname.to_string(),
                scope: portal_storage::THRESHOLD_SCOPE_NODE.to_string(),
                entity: node_key.to_string(),
                field: t.field,
                critical_low: t.critical_low,
                warning_low: t.warning_low,
                warning_high: t.warning_high,
                critical_high: t.critical_high,
                enabled: t.enabled,
            });
        let hub_rows: Vec<(String, Option<String>)> = unit
            .param_keys
            .iter()
            .map(|r| (r.param_key.clone(), r.desired_value.clone()))
            .collect();
        let node_rows: Vec<(String, Option<String>, String)> = unit
            .node_previous
            .iter()
            .map(|p| {
                (
                    p.row.param_key.clone(),
                    p.row.desired_value.clone(),
                    p.previous.clone(),
                )
            })
            .collect();
        let band = unit.node_previous.first().map(|p| p.band.clone());
        let (h, n, actor) = (
            hostname.to_string(),
            node_key.to_string(),
            actor.to_string(),
        );
        self.db
            .run(move |s| {
                portal_storage::with_retry(|| {
                    let mut threshold = threshold.clone();
                    if let (true, Some(row)) = (keep, threshold.as_mut()) {
                        keep_stored_warnings(s, row)?;
                    }
                    s.land_hub_threshold(
                        &h,
                        &n,
                        &portal_storage::HubLanding {
                            hub_rows: &hub_rows,
                            node_rows: &node_rows,
                            previous_band: band.as_deref(),
                            threshold: threshold.as_ref(),
                            actor: Some(&actor),
                        },
                    )
                })
            })
            .await
    }

    pub fn subscribe_nudges(&self) -> broadcast::Receiver<PendingNudge> {
        self.nudge_tx.subscribe()
    }

    /// Tells every open tab that this Node's draft moved. No subscribers is
    /// normal (nobody is looking), so a send error is not one.
    pub fn nudge(&self, hostname: &str, node_key: &str) {
        let _ = self.nudge_tx.send(PendingNudge {
            hostname: hostname.to_string(),
            node_key: node_key.to_string(),
        });
    }

    pub async fn list(
        &self,
        hostname: &str,
        node_key: &str,
    ) -> Result<Vec<PendingConfigRow>, StorageError> {
        let (hostname, node_key) = (hostname.to_string(), node_key.to_string());
        self.db
            .run(move |s| s.list_pending(&hostname, &node_key))
            .await
    }

    /// Writes one draft value. An edit of an `undelivered` bound makes it `pending` again, so
    /// the configuration warning follows (PF-3 review, item 9).
    pub async fn upsert(&self, row: NewPendingConfig) -> Result<(), StorageError> {
        let (hostname, node_key) = (row.hostname.clone(), row.node_key.clone());
        self.db
            .run(move |s| portal_storage::with_retry(|| s.upsert_pending(&row)))
            .await?;
        // Closed by the operator's own edit: nobody needs an e-mail saying so.
        self.sync_config_warning_as(&hostname, &node_key, true)
            .await;
        Ok(())
    }

    /// An edit that puts a bound back to what the portal holds (N17, PF-3 review item 4): see
    /// `Storage::revert_pending_to_held`. While this Node's walk writes the Hub, an untaken row
    /// is kept with the held value rather than dropped, so the walk can still record the revert
    /// point on it. `None` = there was no such row.
    pub async fn revert_to_held(
        &self,
        hostname: &str,
        node_key: &str,
        param_key: &str,
        held: &str,
    ) -> Result<Option<portal_storage::HeldRevert>, StorageError> {
        let keep_unsent = self.in_hub_phase(hostname, node_key);
        let (h, n, k, held) = (
            hostname.to_string(),
            node_key.to_string(),
            param_key.to_string(),
            held.to_string(),
        );
        let outcome = self
            .db
            .run(move |s| {
                portal_storage::with_retry(|| {
                    s.revert_pending_to_held(&h, &n, &k, &held, keep_unsent)
                })
            })
            .await?;
        if outcome.is_some() {
            self.sync_config_warning_as(hostname, node_key, true).await;
        }
        Ok(outcome)
    }

    /// Drops one key's draft — an edit that puts a config value back to what the Node already
    /// reports (N17). A walk that was sending it stops at its next conditional write, as for an
    /// edit. `false` = there was no such row. Threshold rows go through [`Self::revert_to_held`].
    pub async fn remove(
        &self,
        hostname: &str,
        node_key: &str,
        param_key: &str,
    ) -> Result<bool, StorageError> {
        let (hostname, node_key, param_key) = (
            hostname.to_string(),
            node_key.to_string(),
            param_key.to_string(),
        );
        self.db
            .run(move |s| {
                portal_storage::with_retry(|| {
                    s.delete_pending(&hostname, &node_key, &param_key, None)
                })
            })
            .await
    }

    /// Throws the whole draft away, and with it the unit a late ack could still finish and the
    /// configuration warning (PF-3). Reverting the Hub for bounds a send already wrote there is
    /// the Discard route's, which holds the user's key ([`plan_revert`], [`revert_sent`]); the
    /// Node removal route calls this alone.
    pub async fn discard(&self, hostname: &str, node_key: &str) -> Result<usize, StorageError> {
        self.forget_late(hostname, node_key);
        let (h, n) = (hostname.to_string(), node_key.to_string());
        let dropped = self
            .db
            .run(move |s| {
                portal_storage::with_retry(|| {
                    s.forget_node_delivery_attempt(&h, &n)?;
                    s.delete_pending_for_node(&h, &n)
                })
            })
            .await;
        self.sync_config_warning(hostname, node_key).await;
        dropped
    }

    /// Records what the Hub held for a row's bound before the walk wrote the drafted one there
    /// (PF-3). The first write wins — see `Storage::set_pending_previous`. The walk records it
    /// with the band through `land_hub`; this is the tests' shortcut.
    #[cfg(test)]
    async fn set_previous(
        &self,
        hostname: &str,
        node_key: &str,
        row: &SendRow,
        previous: &str,
    ) -> Result<bool, StorageError> {
        let (h, n, k, expect, previous) = (
            hostname.to_string(),
            node_key.to_string(),
            row.param_key.clone(),
            row.desired_value.clone(),
            previous.to_string(),
        );
        self.db
            .run(move |s| {
                portal_storage::with_retry(|| {
                    s.set_pending_previous(&h, &n, &k, Some(expect.as_deref()), &previous)
                })
            })
            .await
    }

    /// `expect` is the walk's re-read — see
    /// [`portal_storage::Storage::set_pending_state`]. `Ok(false)` means the
    /// row is no longer what the caller planned against, which for a walk is
    /// a stop.
    // The storage call's argument list plus `&self` — see
    // `Storage::set_pending_state`.
    #[allow(clippy::too_many_arguments)]
    async fn set_state(
        &self,
        hostname: &str,
        node_key: &str,
        param_key: &str,
        expect: Option<Option<String>>,
        state: &'static str,
        error: Option<String>,
        bump: bool,
    ) -> Result<bool, StorageError> {
        let (hostname, node_key, param_key) = (
            hostname.to_string(),
            node_key.to_string(),
            param_key.to_string(),
        );
        self.db
            .run(move |s| {
                portal_storage::with_retry(|| {
                    s.set_pending_state(
                        &hostname,
                        &node_key,
                        &param_key,
                        expect.as_ref().map(|v| v.as_deref()),
                        state,
                        error.as_deref(),
                        bump,
                    )
                })
            })
            .await
    }

    async fn delete(
        &self,
        hostname: &str,
        node_key: &str,
        param_key: &str,
        expect: Option<Option<String>>,
    ) -> Result<bool, StorageError> {
        let (hostname, node_key, param_key) = (
            hostname.to_string(),
            node_key.to_string(),
            param_key.to_string(),
        );
        self.db
            .run(move |s| {
                portal_storage::with_retry(|| {
                    s.delete_pending(
                        &hostname,
                        &node_key,
                        &param_key,
                        expect.as_ref().map(|v| v.as_deref()),
                    )
                })
            })
            .await
    }

    /// Marks one unit `error` **without awaiting anything** — the only
    /// write [`InterruptGuard`] can make, because `Drop` is not async.
    ///
    /// **This one blocks a runtime thread**, and deliberately. It takes the
    /// storage lock directly (`Db::blocking`) rather than through `Db::run`'s
    /// `spawn_blocking`, so a `Drop` running on a runtime worker parks that
    /// worker until the mutex is free. That is safe here and nowhere else,
    /// for two reasons: every *other* holder of this lock takes it on the
    /// blocking pool for the length of a single statement — none of them is
    /// waiting on this runtime thread for anything, so there is no cycle to
    /// deadlock on — and the wait is therefore bounded by one SQLite write.
    /// The alternative, spawning a task from `Drop`, is a task the runtime
    /// may already be shutting down and would silently never run.
    ///
    /// It is also rare: since the walk became a detached `tokio::spawn`,
    /// dropping a `JoinHandle` does not cancel it, so the guard fires only
    /// on an explicit `abort()`, a panic inside the walk, or runtime
    /// shutdown (§21 review round 2, item 6).
    fn mark_interrupted_blocking(&self, hostname: &str, node_key: &str, rows: &[SendRow]) {
        self.db.blocking(|guard| {
            for row in rows {
                // Retried like every other write here. `with_retry` blocks, but
                // this path already blocks its runtime worker by design (see the
                // doc comment above), and ~350 ms of backoff is cheaper than
                // leaving a unit stuck in `sending` until the next restart's
                // `reset_sending_to_error`.
                // PF-3 review, item 9: the end a restart gives the row — `undelivered` for the
                // Node's part, `error` for the Hub's.
                if let Err(err) = portal_storage::with_retry(|| {
                    guard.mark_pending_interrupted(
                        hostname,
                        node_key,
                        &row.param_key,
                        Some(row.desired_value.as_deref()),
                        SEND_INTERRUPTED,
                    )
                }) {
                    tracing::warn!(
                        %err,
                        param_key = %row.param_key,
                        "pending config: could not record an interrupted send"
                    );
                }
            }
        });
    }
}

/// One unit of a send: the row(s) it covers, the verb, and the params.
///
/// A config key is one row and one unit. A threshold **field** is up to
/// four rows and one unit, because `set_lorawan_field_threshold` takes all
/// four bounds at once — sending one bound would silently reset the other
/// three to whatever the form's blanks said.
#[derive(Debug, Clone)]
pub struct SendUnit {
    /// The rows this unit clears on success, each pinned to the value it was
    /// planned with.
    pub param_keys: Vec<SendRow>,
    /// What the banner and the audit row call this unit.
    pub label: String,
    pub command: &'static str,
    pub params: serde_json::Value,
    /// Set for a Hub-side threshold unit: the four bounds and `enabled` the
    /// portal writes to `field_thresholds` the moment the Hub takes them
    /// (PF-3), before anything goes to the Node.
    pub threshold: Option<ThresholdWrite>,
    /// Set for a Hub-side threshold unit: the rows of its field that ride the
    /// Node unit, each with what the portal held for that bound and for the
    /// whole field before this send. Recorded on the row as `previous_value`
    /// and `previous_band` when the Hub takes the band, for a Discard to put
    /// back (PF-3). The unit writes the Hub for these rows too, so the walk
    /// re-reads them before the wire (PF-3 review, item 1).
    pub node_previous: Vec<NodePrevious>,
    /// Set on the Node unit when it carries the alarm table: each slot it fills, the field's
    /// critical band and switch. What the Node holds once it confirms, for the thresholds
    /// table's "on the Node" (PF-3, `node_threshold_delivery`).
    pub node_bands: Vec<portal_storage::NodeBand>,
    /// `Some(param_key)` when one of this unit's stored values will not
    /// parse. The walk refuses to send the unit and stops there (§21
    /// review, item 10) — the alternative was pushing `null` at the device
    /// for a value nobody could read, which clears a setting rather than
    /// reporting a problem.
    pub unreadable: Option<String>,
    /// Set when the unit must not be sent at all (MR !129 review): an alarm table that would
    /// drop a band from the Node, or a v2.0.0 key for older firmware. Send refuses it up front
    /// ([`refusal`]); the walk stops on it like on an unreadable value.
    pub refusal: Option<SendRefusal>,
}

/// Why a send is refused before anything goes out (MR !129 review).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendRefusal {
    /// The Node's alarm table cannot carry every band (node-firmware-v2.0.0.md §7).
    AlarmTable(crate::radio::alarm::TableRefusal),
    /// v2.0.0 keys (the alarm table, `history_channels`) for a Node whose firmware is older.
    FirmwareTooOld { keys: Vec<String>, version: String },
}

impl SendRefusal {
    /// The i18n key of the operator's message and its placeholders (`{fields}`, `{keys}`,
    /// `{version}`), so the draft banner can render it in the viewer's language (#126 item 5).
    pub fn parts(&self) -> (&'static str, Vec<(&'static str, String)>) {
        use crate::radio::alarm::TableRefusal;
        match self {
            SendRefusal::AlarmTable(TableRefusal::UnknownType(fields)) => (
                "monitoring.pending.alarm_table_unknown_type",
                vec![("{fields}", fields.join(", "))],
            ),
            SendRefusal::AlarmTable(TableRefusal::TooMany(fields)) => (
                "monitoring.pending.alarm_table_too_many",
                vec![("{fields}", fields.join(", "))],
            ),
            SendRefusal::FirmwareTooOld { keys, version } => (
                "monitoring.pending.firmware_too_old",
                vec![("{keys}", keys.join(", ")), ("{version}", version.clone())],
            ),
        }
    }

    /// The operator's message, through `t` (an i18n lookup).
    pub fn message(&self, t: impl Fn(&str) -> String) -> String {
        let (key, args) = self.parts();
        args.iter()
            .fold(t(key), |text, (name, value)| text.replace(name, value))
    }
}

/// The oldest Node firmware the Hub and the Portal speak to (v2.0.0, sticker-firmware PR #503):
/// its proto is the one vendored, and nothing older is decoded or written.
const FW_MINIMUM: (u64, u64, u64) = (2, 0, 0);

/// Whether a Node firmware `version` predates v2.0.0. An unknown or unparsable version is not
/// refused: a guard, not a gate.
pub fn firmware_predates_v200(version: Option<&(u64, u64, u64)>) -> bool {
    version.is_some_and(|v| *v < FW_MINIMUM)
}

/// Why `units` must not be sent, if they must not: a unit's own refusal, else, for a Node
/// whose known firmware `version` predates v2.0.0, every unit (*amended 2026-10-14:* not only
/// the alarm table and `history_channels`; that firmware's proto is not the Hub's).
pub fn refusal(units: &[SendUnit], version: Option<&(u64, u64, u64)>) -> Option<SendRefusal> {
    if let Some(r) = units.iter().find_map(|u| u.refusal.clone()) {
        return Some(r);
    }
    if !firmware_predates_v200(version) {
        return None;
    }
    let mut keys: Vec<String> = Vec::new();
    for unit in units {
        if unit.params.get("replace_alarms").is_some() {
            keys.push("alarms".to_string());
        }
        match unit
            .params
            .get("config")
            .and_then(serde_json::Value::as_object)
        {
            Some(config) => keys.extend(config.keys().cloned()),
            None if unit.params.get("replace_alarms").is_none() => {
                keys.push(unit.command.to_string())
            }
            None => {}
        }
    }
    keys.dedup();
    let (major, minor, patch) = version.copied().unwrap_or_default();
    (!keys.is_empty()).then(|| SendRefusal::FirmwareTooOld {
        keys,
        version: format!("{major}.{minor}.{patch}"),
    })
}

/// One draft row a unit covers, pinned to the exact `desired_value` the
/// plan was built from (§21 review, defect 3).
///
/// The walk carries this rather than a bare key because a unit can wait
/// three minutes for a device, and in that window any manager may overwrite
/// the value or discard the draft. Every state write and the final delete is
/// conditioned on this value, so the walk can only ever speak for the row it
/// actually sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendRow {
    pub param_key: String,
    /// Exactly what `pending_config.desired_value` held when the plan was
    /// built — `None` for a SQL NULL, which is a real drafted value ("unset
    /// this bound") and not an absence.
    pub desired_value: Option<String>,
}

impl SendRow {
    fn of(row: &PendingConfigRow) -> Self {
        Self {
            param_key: row.param_key.clone(),
            desired_value: row.desired_value.clone(),
        }
    }
}

/// The local mirror of one threshold field, written the moment the Hub takes
/// it (PF-3).
#[derive(Debug, Clone, PartialEq)]
pub struct ThresholdWrite {
    pub field: String,
    pub critical_low: Option<f64>,
    pub warning_low: Option<f64>,
    pub warning_high: Option<f64>,
    pub critical_high: Option<f64>,
    pub enabled: bool,
    /// The write carries warning bounds a draft row asked for. Only a draft from before
    /// 2026-10-13 has such rows: since then a save stores the warnings at once and never drafts
    /// them (devices.node.md, amendment 2026-10-13), so a write without them keeps the warnings
    /// the portal holds when it lands ([`keep_stored_warnings`]) rather than the ones it was
    /// planned with — a save during the send must not be overwritten by the plan's older copy.
    pub warnings_drafted: bool,
}

/// Puts the warning bounds the portal holds for `row`'s field into it, when it holds a row.
/// Called inside the write, so a warning save landing between the plan and the write wins.
pub(crate) fn keep_stored_warnings(
    s: &portal_storage::Storage,
    row: &mut portal_storage::NewFieldThreshold,
) -> Result<(), StorageError> {
    if let Some(held) = s.get_field_threshold(&row.hostname, &row.scope, &row.entity, &row.field)? {
        row.warning_low = held.warning_low;
        row.warning_high = held.warning_high;
    }
    Ok(())
}

/// A threshold row's key names a warning bound.
pub fn is_warning_key(param_key: &str) -> bool {
    parse_threshold_param_key(param_key)
        .is_some_and(|(_, bound)| bound == "warning_low" || bound == "warning_high")
}

/// What a landed send writes on the portal's side: the Node's `field_thresholds` rows, which
/// the portal's own alarm engine evaluates (`persist::run_node_threshold_engine`), and the
/// layout the Node's alarm table was written under (nodes_phase1.cs.md WP2).
///
/// The walk writes a Hub unit's band the moment the Hub takes it, and a Node unit's layout the
/// moment the Node confirms its table (PF-3); a late ack and a Discard's revert write through
/// the same path. This is the ONLY place an operator's Node threshold bound reaches
/// `field_thresholds` from the portal side (§21 review, defect 1). The one other writer is
/// the default band a new sensor gets (`threshold_defaults::seed`): switched off, never over
/// an existing row, and so never a bound anyone drafted.
#[derive(Debug, Clone, Default)]
pub struct WriteThrough {
    pub thresholds: Vec<ThresholdWrite>,
    pub alarm_layout: Option<Vec<String>>,
}

impl WriteThrough {
    /// Writes it. `author` is whom the threshold rows name. A failure is logged, not returned:
    /// the device already holds these values, and there is nothing to roll back to.
    pub async fn apply(
        &self,
        db: &crate::db::Db,
        hostname: &str,
        node_key: &str,
        author: Option<&str>,
    ) {
        for t in &self.thresholds {
            let keep = !t.warnings_drafted;
            let row = portal_storage::NewFieldThreshold {
                hostname: hostname.to_string(),
                scope: portal_storage::THRESHOLD_SCOPE_NODE.to_string(),
                entity: node_key.to_string(),
                field: t.field.clone(),
                critical_low: t.critical_low,
                warning_low: t.warning_low,
                warning_high: t.warning_high,
                critical_high: t.critical_high,
                enabled: t.enabled,
            };
            let author = author.map(str::to_string);
            if let Err(err) = db
                .write(move |s| {
                    let mut row = row.clone();
                    if keep {
                        keep_stored_warnings(s, &mut row)?;
                    }
                    s.upsert_field_threshold(&row, author.as_deref())
                })
                .await
            {
                tracing::warn!(%err, node_key, field = %t.field, "threshold write-through failed after the Hub took the send");
            }
        }
        if let Some(layout) = &self.alarm_layout {
            let (h, n, layout) = (hostname.to_string(), node_key.to_string(), layout.clone());
            if let Err(err) = db
                .write(move |s| s.set_node_alarm_layout(&h, &n, &layout, now_unix()))
                .await
            {
                tracing::warn!(%err, node_key, "alarm layout not recorded after the Node took the send");
            }
        }
    }
}

/// The layout a Node unit's alarm table was planned under, when it carries one, as it is stored:
/// with the slot types its rules name ([`crate::radio::alarm::stored_layout`]).
fn alarm_layout_of(unit: &SendUnit) -> Option<Vec<String>> {
    let layout = unit.params.get("alarm_layout")?;
    let layout: Vec<String> = serde_json::from_value(layout.clone()).unwrap_or_default();
    match unit
        .params
        .get("alarm_types")
        .and_then(|t| serde_json::from_value::<[u8; 4]>(t.clone()).ok())
    {
        Some(types) => Some(crate::radio::alarm::stored_layout(
            &layout,
            &crate::radio::alarm::SlotTypes(types),
        )),
        None => Some(layout),
    }
}

/// The canonical `param_key` for one bound of one field.
pub fn threshold_param_key(field: &str, bound: &str) -> String {
    format!("threshold.{field}.{bound}")
}

/// Splits `threshold.{field}.{bound}` back into its two halves.
///
/// A field name can contain dots in principle, so the bound is taken from
/// the **end** rather than by splitting on the first separator: the bound
/// vocabulary is closed ([`THRESHOLD_BOUNDS`]) and the field is whatever is
/// left.
pub fn parse_threshold_param_key(param_key: &str) -> Option<(String, &'static str)> {
    let rest = param_key.strip_prefix("threshold.")?;
    for bound in THRESHOLD_BOUNDS {
        if let Some(field) = rest.strip_suffix(&format!(".{bound}")) {
            if field.is_empty() {
                return None;
            }
            return Some((field.to_string(), bound));
        }
    }
    None
}

/// The `param_key` for a field threshold's `enabled` flag. `enabled` is drafted
/// like the four bounds (OQ3) rather than written through immediately, so
/// toggling it reaches the Node on Send and a Discard reverts it with the rest.
pub fn threshold_enabled_param_key(field: &str) -> String {
    format!("threshold.{field}.enabled")
}

/// Splits `threshold.{field}.enabled` back to its field, or `None`. `enabled` is
/// not in [`THRESHOLD_BOUNDS`], so [`parse_threshold_param_key`] never claims it.
pub fn parse_threshold_enabled_key(param_key: &str) -> Option<String> {
    param_key
        .strip_prefix("threshold.")?
        .strip_suffix(".enabled")
        .filter(|field| !field.is_empty())
        .map(str::to_string)
}

/// The one send unit that goes to the Node (nodes_phase1.cs.md WP2, WP5): the drafted config
/// keys and, when a critical band moved, the Node's whole alarm table, as one `set-config`.
pub const NODE_CONFIG_UNIT: &str = "node.config";

/// The author of the draft rows the portal writes itself when a Node's active sensors move its
/// alarm slots; the draft banner says why they are there.
pub const ALARM_LAYOUT_AUTHOR: &str = "portal";

/// Turns a draft into the units a walk sends: each drafted field's Hub-side threshold write,
/// then the Node's one `set-config` last (PF-3, decision Hynek 2026-09-27).
///
/// A field whose every row is `undelivered` gets no Hub unit: the Hub and the portal already
/// hold its band, and "Send again" sends the Node its part only.
///
/// `current_thresholds` is [`crate::render::merge_node_thresholds`]'s output
/// — the bounds the field has *now*, from the firmware echo merged with the
/// portal's own rows. A draft only ever holds the bounds the operator
/// actually touched, so every untouched bound of a drafted field is filled
/// from here; sending the drafted ones alone would blank the rest.
///
/// `slot_types` are the Node's 1-Wire slot types a rule is written for
/// ([`crate::radio::alarm::SlotTypes::of_node`]).
pub fn plan_send(
    rows: &[PendingConfigRow],
    node_key: &str,
    current_thresholds: &std::collections::BTreeMap<String, crate::render::ThresholdEntryCtx>,
    alarm_layout: &[String],
    slot_types: &crate::radio::alarm::SlotTypes,
) -> Vec<SendUnit> {
    use std::collections::BTreeMap;

    let mut units = Vec::new();
    // The critical band each drafted field will hold once this send lands; the alarm table
    // below reads these first and the stored thresholds for every other field.
    let mut drafted_bands: BTreeMap<String, crate::radio::alarm::CriticalBand> = BTreeMap::new();
    let mut table_touched = false;

    // ---- thresholds, grouped per field --------------------------------
    let mut by_field: BTreeMap<String, Vec<(&'static str, &PendingConfigRow)>> = BTreeMap::new();
    // `enabled` is drafted alongside the bounds (OQ3): its own row per field,
    // carrying the flag and the pending row the walk clears on success.
    let mut enabled_drafts: BTreeMap<String, (bool, &PendingConfigRow)> = BTreeMap::new();
    for row in rows.iter().filter(|r| r.kind == PENDING_KIND_THRESHOLD) {
        if let Some((field, bound)) = parse_threshold_param_key(&row.param_key) {
            by_field.entry(field).or_default().push((bound, row));
        } else if let Some(field) = parse_threshold_enabled_key(&row.param_key) {
            let value = row
                .desired_value
                .as_deref()
                .and_then(|raw| serde_json::from_str::<bool>(raw).ok())
                .unwrap_or(true);
            // A field touched only through its `enabled` toggle still gets a
            // unit, so ensure it has an entry even with no bound rows.
            by_field.entry(field.clone()).or_default();
            enabled_drafts.insert(field, (value, row));
        } else {
            tracing::warn!(
                param_key = %row.param_key,
                "pending config: unparseable threshold key; skipped"
            );
        }
    }

    // The drafted rows whose effect is on the Node — a slotted field's critical bounds and its
    // on/off. The Node unit below owns them, so the banner shows them "sending" until the
    // Node acks the table; on the Hub-side unit they vanished while the Node leg was still in
    // flight (WP4 on 0413, N5).
    let mut device_rows: Vec<SendRow> = Vec::new();
    for (field, drafted) in by_field {
        let current = current_thresholds.get(&field);
        let on_device = alarm_layout.contains(&field);
        // PF-3: what the portal holds for each bound now, which the Hub held before this send.
        let held = |bound: &str| -> String {
            let value = match bound {
                "critical_low" => current.and_then(|c| c.critical_low),
                "warning_low" => current.and_then(|c| c.warning_low),
                "warning_high" => current.and_then(|c| c.warning_high),
                _ => current.and_then(|c| c.critical_high),
            };
            serde_json::to_string(&value).unwrap_or_else(|_| "null".into())
        };
        let band_before = serde_json::to_string(&BandJson {
            critical_low: current.and_then(|c| c.critical_low),
            warning_low: current.and_then(|c| c.warning_low),
            warning_high: current.and_then(|c| c.warning_high),
            critical_high: current.and_then(|c| c.critical_high),
            enabled: current.map(|c| c.enabled).unwrap_or(true),
        })
        .unwrap_or_default();
        let mut node_previous: Vec<NodePrevious> = Vec::new();
        let mut hub_needed = false;
        let mut bounds: [Option<f64>; 4] = [
            current.and_then(|c| c.critical_low),
            current.and_then(|c| c.warning_low),
            current.and_then(|c| c.warning_high),
            current.and_then(|c| c.critical_high),
        ];
        let mut param_keys = Vec::new();
        let mut unreadable: Option<String> = None;
        for (bound, row) in &drafted {
            let idx = THRESHOLD_BOUNDS
                .iter()
                .position(|b| b == bound)
                .expect("bound came from THRESHOLD_BOUNDS");
            // A stored value that will not parse is NOT "unset" (§21
            // review, item 10): `null` on the wire clears a bound the
            // operator never asked to clear. The unit is planned as
            // unsendable instead, and the walk stops on it with the reason
            // in the banner.
            match row.desired_value.as_deref() {
                Some(raw) => match serde_json::from_str::<Option<f64>>(raw) {
                    Ok(value) => bounds[idx] = value,
                    Err(_) => {
                        unreadable.get_or_insert_with(|| row.param_key.clone());
                    }
                },
                None => bounds[idx] = None,
            }
            hub_needed |= row.state != PENDING_STATE_UNDELIVERED;
            if on_device && (*bound == "critical_low" || *bound == "critical_high") {
                device_rows.push(SendRow::of(row));
                node_previous.push(NodePrevious {
                    row: SendRow::of(row),
                    previous: held(bound),
                    band: band_before.clone(),
                });
            } else {
                param_keys.push(SendRow::of(row));
            }
        }
        // NT-3 (night test 2026-09-26): the Node holds a field's rule only while it has a
        // critical bound. Switching on a warning-only threshold sent the whole alarm table
        // (a downlink and a restart) for a slot that stays empty.
        let slotted = on_device && (bounds[0].is_some() || bounds[3].is_some());
        // `enabled` is drafted like the bounds (OQ3): the drafted value if the
        // operator touched the toggle, else the current on-device/DB state. Its
        // pending row rides a unit's `param_keys`, so the walk clears it on a
        // successful send exactly like a bound — the Node unit's for a slotted field.
        let enabled = match enabled_drafts.get(&field) {
            Some((value, row)) => {
                hub_needed |= row.state != PENDING_STATE_UNDELIVERED;
                if slotted {
                    device_rows.push(SendRow::of(row));
                    let was = current.map(|c| c.enabled).unwrap_or(true);
                    node_previous.push(NodePrevious {
                        row: SendRow::of(row),
                        previous: was.to_string(),
                        band: band_before.clone(),
                    });
                } else {
                    param_keys.push(SendRow::of(row));
                }
                *value
            }
            None => current.map(|c| c.enabled).unwrap_or(true),
        };
        // A row only the Hub holds is never `undelivered`; guard it all the same, so a Hub row
        // cannot be left without a unit that clears it.
        hub_needed |= !param_keys.is_empty();
        if hub_needed {
            units.push(SendUnit {
                param_keys,
                label: format!("threshold.{field}"),
                command: THRESHOLD_COMMAND,
                params: json!({
                    "dev_eui": node_key,
                    "field": field,
                    "critical_low": bounds[0],
                    "warning_low": bounds[1],
                    "warning_high": bounds[2],
                    "critical_high": bounds[3],
                    "enabled": enabled,
                }),
                threshold: Some(ThresholdWrite {
                    field: field.clone(),
                    critical_low: bounds[0],
                    warning_low: bounds[1],
                    warning_high: bounds[2],
                    critical_high: bounds[3],
                    enabled,
                    warnings_drafted: drafted
                        .iter()
                        .any(|(_, row)| is_warning_key(&row.param_key)),
                }),
                node_previous,
                node_bands: Vec::new(),
                unreadable,
                refusal: None,
            });
        }

        // A warning-only edit changes nothing the Node evaluates; a critical bound or the
        // on/off does, and then the whole table goes (below).
        let critical_touched = drafted
            .iter()
            .any(|(bound, _)| *bound == "critical_low" || *bound == "critical_high");
        if critical_touched || (slotted && enabled_drafts.contains_key(&field)) {
            table_touched = true;
        }
        drafted_bands.insert(
            field.clone(),
            crate::radio::alarm::CriticalBand {
                low: bounds[0],
                high: bounds[3],
                enabled,
            },
        );
    }

    // ---- one set-config for the Node (nodes_phase1.cs.md WP2, WP5) ---------
    // The drafted config keys and, when a critical band or an on/off moved, the Node's whole
    // alarm table go as ONE `set-config`: control-radio packs it into as few frames as the
    // Node's data rate allows and puts `save` on the last, so the Node commits and restarts
    // once. Two units with a `save` each restarted it twice, the second frame landing while it
    // rebooted.
    let mut config = serde_json::Map::new();
    let mut param_keys = Vec::new();
    let mut unreadable: Option<String> = None;
    for row in rows.iter().filter(|r| r.kind == PENDING_KIND_CONFIG) {
        // Same rule as a threshold bound (§21 review, item 10): a value the
        // portal cannot read is not a `null` to push at the device. Before
        // this, a corrupt row silently wrote `{"application.interval_report":
        // null}` into a Node's configuration.
        let value = match row.desired_value.as_deref() {
            Some(raw) => match serde_json::from_str::<serde_json::Value>(raw) {
                Ok(value) => value,
                Err(_) => {
                    unreadable.get_or_insert_with(|| row.param_key.clone());
                    serde_json::Value::Null
                }
            },
            None => serde_json::Value::Null,
        };
        config.insert(row.param_key.clone(), value);
        param_keys.push(SendRow::of(row));
    }

    // The portal owns the table: every banded field of the Node's layout, sent with
    // `alarms_replace` so the firmware clears every slot first and a rule changed over NFC
    // cannot survive. A table that cannot carry every band is refused, not trimmed (MR !129
    // review, node-firmware-v2.0.0.md §7 amended 2026-10-12).
    let table = table_touched && !alarm_layout.is_empty();
    let mut node_bands: Vec<portal_storage::NodeBand> = Vec::new();
    let mut written_layout: Vec<String> = Vec::new();
    let mut refusal: Option<SendRefusal> = None;
    if table {
        let band_of = |field: &str| {
            drafted_bands.get(field).copied().or_else(|| {
                current_thresholds
                    .get(field)
                    .map(|c| crate::radio::alarm::CriticalBand {
                        low: c.critical_low,
                        high: c.critical_high,
                        enabled: c.enabled,
                    })
            })
        };
        match crate::radio::alarm::alarm_table(alarm_layout, slot_types, band_of) {
            Ok(written) => {
                for (field, assignment) in written.layout.iter().zip(&written.assignments) {
                    if let Some(band) = band_of(field) {
                        node_bands.push(portal_storage::NodeBand {
                            field: field.clone(),
                            low: band.low,
                            high: band.high,
                            enabled: band.enabled,
                        });
                    }
                    if let Some((key, hex)) = assignment.split_once('=') {
                        config.insert(key.to_string(), serde_json::Value::String(hex.to_string()));
                    }
                }
                written_layout = written.layout;
            }
            Err(r) => refusal = Some(SendRefusal::AlarmTable(r)),
        }
    }

    param_keys.extend(device_rows);
    if !param_keys.is_empty() || table {
        let mut params = json!({ "dev_eui": node_key, "config": config, "save": true });
        if table {
            params["replace_alarms"] = json!(true);
            // The banded fields in slot order and the slot types their rules name: what the
            // portal records once the Node takes the table (`alarm_layout_of`).
            params["alarm_layout"] = json!(written_layout);
            params["alarm_types"] = json!(slot_types.0);
        }
        // LAST, after the Hub-side threshold units (PF-3, decision Hynek 2026-09-27, which
        // reverses N15's order). The Hub and the portal take the new bounds at once; a Node
        // that does not answer in time leaves its rows `undelivered`, with a warning, for a
        // resend of this unit alone. Node first (N15) meant a late ack left the Node holding a
        // band the portal never wrote (PF-3 on the P2P bench).
        units.push(SendUnit {
            param_keys,
            label: NODE_CONFIG_UNIT.to_string(),
            command: SEND_COMMAND,
            params,
            threshold: None,
            node_previous: Vec::new(),
            node_bands,
            unreadable,
            refusal,
        });
    }

    units
}

/// How many command frames the units' Node leg takes on `carrier` at data rate `dr`, the
/// number the draft banner states (nodes_phase1.cs.md WP5). control-radio plans the same
/// set-config again with the same budget rule
/// ([`control_radio::decode::command_encode::carrier_downlink_budget`]): on LoRaWAN the data
/// rate of the Node's last uplink, so the two agree when the Node has not changed DR since;
/// on P2P the fixed 72 B body (FW v2.0.0 `P2P_MAX_BODY`), whatever the data rate. `carrier` is the carrier resolver's (PU-4: this
/// planned every Node as LoRaWAN). `None` when nothing goes to the Node or the plan fails;
/// the send then reports control-radio's own reason.
pub fn downlink_count(
    units: &[SendUnit],
    dr: Option<i64>,
    carrier: portal_domain::Transport,
) -> Option<usize> {
    use control_radio::decode::command_encode::{carrier_downlink_budget, plan_with_budget};
    let transport = match carrier {
        portal_domain::Transport::Lorawan => control_radio::Transport::Lorawan,
        portal_domain::Transport::P2p => control_radio::Transport::P2p,
    };
    let budget = carrier_downlink_budget(transport, dr.and_then(|d| u64::try_from(d).ok()));
    let mut total = None;
    for unit in units.iter().filter(|u| u.command == SEND_COMMAND) {
        let config = unit
            .params
            .get("config")
            .and_then(serde_json::Value::as_object)
            .cloned()
            .unwrap_or_default();
        let kind = control_radio::cli::NodeSendKind::SetConfig {
            assignments: crate::radio::config_assignments(&config),
            replace_alarms: unit
                .params
                .get("replace_alarms")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            supersede: true,
        };
        let frames = plan_with_budget(&kind, transport, budget)
            .ok()?
            .downlinks
            .len();
        total = Some(total.unwrap_or(0) + frames);
    }
    total
}

/// What the walk needs from the relay, as a trait so the state machine can
/// be tested without a broker.
///
/// [`crate::relay::CommandRelay`] is the only production implementor. Both
/// of the walk's verbs ride the signed `config_request` dialect (the only
/// one left since the MQTT rebuild), so a real relay in a test would sit
/// out the full 20 s timeout per unit and answer "timeout" — which is one
/// outcome out of the several the walk has to get right. Injecting the
/// outcomes is what makes
/// stop-on-first-failure, delete-only-after-the-device and the threshold
/// unit's single clock testable at all (§21 review, item 6).
pub trait UnitSender: Send + Sync {
    fn send_unit<'a>(
        &'a self,
        actor: &'a str,
        key: &'a crate::keys::UserSigningKey,
        hostname: &'a str,
        command: &'a str,
        params: serde_json::Value,
    ) -> BoxFuture<'a, crate::relay::CommandOutcome>;

    /// Takes frames still waiting, undelivered, out of the Hub's downlink queue and says how
    /// many went (PF-3 review, item 5) — the call Discard and a history retry use. A sender
    /// that cannot do that drops nothing.
    fn drop_queued<'a>(
        &'a self,
        _node_key: &'a str,
        _seqs: &'a [u64],
    ) -> BoxFuture<'a, Result<usize, String>> {
        Box::pin(async { Ok(0) })
    }

    /// Whether a join on the Node's carrier empties its downlink queue (P-7,
    /// `portal_domain::Transport::join_flushes_queue`), so the walk sends a unit again after the
    /// Node rejoined. A sender with no carrier resolver is the relay's, and the relay reaches
    /// LoRaWAN Nodes, whose join does.
    fn join_flushes_queue(&self, _node_key: &str) -> bool {
        true
    }

    /// A Discard's withdrawal (PF-3b): as [`Self::drop_queued`], but a frame already delivered
    /// (a P2P head in `Sent`) goes too, and the answer says whether the Node may have one of
    /// them (`crate::radio::RadioControl::withdraw_queued`). Only the Discard calls it. A sender
    /// with no queue withdraws nothing, and cannot rule out that the Node has the frames.
    fn withdraw_all<'a>(
        &'a self,
        _node_key: &'a str,
        seqs: &'a [u64],
    ) -> BoxFuture<'a, Result<crate::radio::Withdrawal, String>> {
        let maybe_delivered = !seqs.is_empty();
        Box::pin(async move {
            Ok(crate::radio::Withdrawal {
                dropped: 0,
                maybe_delivered,
            })
        })
    }
}

impl UnitSender for crate::relay::CommandRelay {
    fn send_unit<'a>(
        &'a self,
        actor: &'a str,
        key: &'a crate::keys::UserSigningKey,
        hostname: &'a str,
        command: &'a str,
        params: serde_json::Value,
    ) -> BoxFuture<'a, crate::relay::CommandOutcome> {
        Box::pin(async move {
            self.execute(actor, key, hostname, command, params, None)
                .await
        })
    }
}

/// The `request_id` prefix a [`RadioUnitSender`] answers with: the sequence numbers the
/// control.radio seam queued the unit's downlinks under, comma-separated. The walk confirms such
/// a unit on the Node's own ack for exactly those sequences — a real correlation, where the
/// relay path could only match on `(node, command)`.
const RADIO_SEQ_PREFIX: &str = "control.radio:seq=";

/// The walk's sender on a Hub whose radio is control-radio (a v2 Hub).
///
/// The config half (`set_sticker_config`: config keys, and a threshold's critical band as an
/// on-device alarm rule) goes through the in-process seam as `node-send set-config`. On a v2
/// Hub the signed relay to `fiber_app` sends nothing, because that agent has no LoRaWAN stack,
/// so "Send to device" and the threshold push never reached the Node (bench, 2026-09-23). The
/// Hub-side threshold write (`set_lorawan_field_threshold`) has no device leg and stays on the
/// relay. A Hub with no rendered radio config (a v1 Hub) falls back to the relay for
/// everything, as before.
pub struct RadioUnitSender<'s> {
    pub radio: &'s crate::radio::RadioControl,
    pub relay: &'s crate::relay::CommandRelay,
}

impl UnitSender for RadioUnitSender<'_> {
    fn join_flushes_queue(&self, node_key: &str) -> bool {
        self.radio
            .carrier_for(node_key)
            .carrier
            .join_flushes_queue()
    }

    fn withdraw_all<'a>(
        &'a self,
        node_key: &'a str,
        seqs: &'a [u64],
    ) -> BoxFuture<'a, Result<crate::radio::Withdrawal, String>> {
        Box::pin(async move {
            self.radio
                .withdraw_queued(node_key, seqs)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn drop_queued<'a>(
        &'a self,
        node_key: &'a str,
        seqs: &'a [u64],
    ) -> BoxFuture<'a, Result<usize, String>> {
        Box::pin(async move {
            self.radio
                .drop_queued(node_key, seqs)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn send_unit<'a>(
        &'a self,
        actor: &'a str,
        key: &'a crate::keys::UserSigningKey,
        hostname: &'a str,
        command: &'a str,
        params: serde_json::Value,
    ) -> BoxFuture<'a, crate::relay::CommandOutcome> {
        Box::pin(async move {
            if command != SEND_COMMAND {
                return self
                    .relay
                    .send_unit(actor, key, hostname, command, params)
                    .await;
            }
            let dev_eui = params
                .get("dev_eui")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let config = params
                .get("config")
                .and_then(serde_json::Value::as_object)
                .cloned()
                .unwrap_or_default();
            let replace_alarms = params
                .get("replace_alarms")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            match self
                .radio
                .set_config(
                    &dev_eui,
                    crate::radio::config_assignments(&config),
                    replace_alarms,
                    // The walk's message is the Node's whole pending configuration (Q20b).
                    true,
                )
                .await
            {
                Ok(accepted) => {
                    let seqs: Vec<String> = accepted
                        .get("downlinks")
                        .and_then(serde_json::Value::as_array)
                        .map(|d| {
                            d.iter()
                                .filter_map(|x| x.get("seq").and_then(serde_json::Value::as_u64))
                                .map(|s| s.to_string())
                                .collect()
                        })
                        .unwrap_or_default();
                    crate::relay::CommandOutcome {
                        request_id: Some(format!("{RADIO_SEQ_PREFIX}{}", seqs.join(","))),
                        command_type: command.to_string(),
                        status: "success",
                        message: None,
                        error_code: None,
                        result: None,
                    }
                }
                Err(crate::radio::RadioError::NotConfigured(_)) => {
                    self.relay
                        .send_unit(actor, key, hostname, command, params)
                        .await
                }
                Err(e) => crate::relay::CommandOutcome {
                    request_id: None,
                    command_type: command.to_string(),
                    status: "error",
                    message: Some(e.to_string()),
                    error_code: e.code(),
                    result: None,
                },
            }
        })
    }
}

/// [`RadioUnitSender`] owning its halves, for a task that outlives the request (a Discard,
/// PF-3 review round 3).
#[derive(Clone)]
pub struct RadioSender {
    pub radio: crate::radio::RadioControl,
    pub relay: crate::relay::CommandRelay,
}

impl UnitSender for RadioSender {
    fn join_flushes_queue(&self, node_key: &str) -> bool {
        self.radio
            .carrier_for(node_key)
            .carrier
            .join_flushes_queue()
    }

    fn withdraw_all<'a>(
        &'a self,
        node_key: &'a str,
        seqs: &'a [u64],
    ) -> BoxFuture<'a, Result<crate::radio::Withdrawal, String>> {
        Box::pin(async move {
            self.radio
                .withdraw_queued(node_key, seqs)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn send_unit<'a>(
        &'a self,
        actor: &'a str,
        key: &'a crate::keys::UserSigningKey,
        hostname: &'a str,
        command: &'a str,
        params: serde_json::Value,
    ) -> BoxFuture<'a, crate::relay::CommandOutcome> {
        Box::pin(async move {
            RadioUnitSender {
                radio: &self.radio,
                relay: &self.relay,
            }
            .send_unit(actor, key, hostname, command, params)
            .await
        })
    }

    fn drop_queued<'a>(
        &'a self,
        node_key: &'a str,
        seqs: &'a [u64],
    ) -> BoxFuture<'a, Result<usize, String>> {
        Box::pin(async move {
            self.radio
                .drop_queued(node_key, seqs)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

/// JSON equality that reads `60` and `60.0` as the same number: a draft holds what the form
/// coerced, the config info what the decoder produced.
fn json_equal(a: Option<&serde_json::Value>, b: Option<&serde_json::Value>) -> bool {
    match (a, b) {
        (Some(serde_json::Value::Number(x)), Some(serde_json::Value::Number(y))) => {
            x.as_f64() == y.as_f64()
        }
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// How long before a join the frames must have been queued for the join to have flushed
/// them (P-7). The unsolicited Info arrives a second or two after the join accept.
const REJOIN_MARGIN_S: i64 = 5;

/// What a dropped walk writes into the rows it was in the middle of.
const SEND_INTERRUPTED: &str = "send interrupted";

/// Marks the unit in flight as `error` if the walk's task goes away without
/// finishing it (§21 review, defect 2).
///
/// The walk is a detached `tokio::spawn`, so it can be aborted or dropped —
/// graceful shutdown, a panic upstream, a `JoinHandle` going out of scope —
/// while a unit sits in `sending`. Without this, those rows stay `sending`
/// forever and `pending_config_body.html` leaves both Send and Discard
/// disabled: the operator's only way out is a portal restart.
struct InterruptGuard<'a> {
    pending: &'a PendingService,
    hostname: &'a str,
    node_key: &'a str,
    in_flight: Vec<SendRow>,
}

impl<'a> InterruptGuard<'a> {
    fn new(pending: &'a PendingService, hostname: &'a str, node_key: &'a str) -> Self {
        Self {
            pending,
            hostname,
            node_key,
            in_flight: Vec::new(),
        }
    }

    /// From here until [`Self::clear`], this row belongs to a unit that is
    /// being marked `sending` and has not resolved.
    ///
    /// **One row at a time, armed before its write** (§21 review round 2,
    /// item 2). Arming the whole unit after the marking loop left a
    /// multi-row unit — a threshold field is four rows — with rows already
    /// `sending` and nothing watching them the moment the loop stopped
    /// early: a `Superseded` on the second bound stranded the first. Arming
    /// before the write rather than after also closes the abort landing
    /// between the `UPDATE` and the arm; the cost is that an abort in the
    /// other order marks a still-`pending` row `error`, which is visible
    /// and re-sendable where a stranded `sending` row is neither.
    fn arm_row(&mut self, row: &SendRow) {
        self.in_flight.push(row.clone());
    }

    /// The unit resolved (confirmed, refused or timed out) and has written
    /// its own state — nothing left for the guard to say about it.
    fn clear(&mut self) {
        self.in_flight.clear();
    }
}

impl Drop for InterruptGuard<'_> {
    fn drop(&mut self) {
        if self.in_flight.is_empty() {
            return;
        }
        tracing::warn!(
            node_key = %self.node_key,
            rows = self.in_flight.len(),
            "pending config: send walk dropped mid-unit; marking the rows error"
        );
        self.pending
            .mark_interrupted_blocking(self.hostname, self.node_key, &self.in_flight);
        self.pending.nudge(self.hostname, self.node_key);
        // The warning follows the rows. Best effort: at shutdown the runtime may not run it,
        // and the start-up pass then opens it (`sync_undelivered_warnings`).
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let (pending, hostname, node_key) = (
                self.pending.clone(),
                self.hostname.to_string(),
                self.node_key.to_string(),
            );
            runtime.spawn(async move {
                pending.sync_config_warning(&hostname, &node_key).await;
            });
        }
    }
}

/// Runs one draft's send walk to completion (or to its first failure).
///
/// The caller owns the relay, the signing key and the audit row; this
/// function owns the state machine. `confirmations` is a fresh
/// `store.subscribe_command_results()` taken **before** the first relay
/// call, so a device that answers unusually fast cannot beat the
/// subscription — and the narrow stream rather than the fleet firehose, so
/// a busy fleet cannot lap the channel and drop the answer (see
/// `StateStore::results_tx`).
///
/// **It runs on a detached task** (§21 review, defect 2), not inside the
/// POST: a config unit can take three minutes, and a browser that navigates
/// away or a proxy that times out used to drop the future mid-`sending`,
/// stranding rows in a state that disables both Send and Discard. The route
/// answers immediately and the banner follows the walk over SSE, which is
/// where the detail was going anyway.
///
/// **It re-reads before it writes** (§21 review, defect 3). Every state
/// write and every delete carries the `desired_value` the unit was planned
/// with, so an edit or a Discard landing mid-walk makes the next write miss
/// and stops the walk instead of confirming somebody else's value.
///
/// Returns `Ok(units_sent)` on a full drain, `Err(stop)` otherwise. The
/// caller writes one audit row either way, with the real outcome.
#[allow(clippy::too_many_arguments)]
pub async fn run_send_walk(
    pending: &PendingService,
    relay: &dyn UnitSender,
    key: &crate::keys::UserSigningKey,
    actor: &str,
    hostname: &str,
    node_key: &str,
    units: &[SendUnit],
    mut confirmations: broadcast::Receiver<crate::state::ChangeEvent>,
    budget: Duration,
) -> Result<usize, SendStop> {
    if units.is_empty() {
        return Err(SendStop::Nothing);
    }
    // What each row was before this walk touched it, so a stop puts back exactly that (PF-3
    // review, item 9): a resend's `undelivered` config key is `undelivered` again, not `pending`.
    let initial: std::collections::HashMap<String, String> = pending
        .list(hostname, node_key)
        .await
        .map(|rows| rows.into_iter().map(|r| (r.param_key, r.state)).collect())
        .unwrap_or_default();
    let mut progress = WalkProgress::default();
    let walk = Walk {
        pending,
        relay,
        key,
        actor,
        hostname,
        node_key,
        budget,
        initial: &initial,
    };
    match walk.units(units, &mut confirmations, &mut progress).await {
        Ok(sent) => Ok(sent),
        Err(mut stop) => {
            // One end state for the Node's rows however the walk stopped (PF-3 review, items 9
            // and 10): a bound the Hub already took is `undelivered` — in effect, the Node
            // does not have it — and everything else is what it was before this walk.
            if !progress.node_settled {
                settle_node_rows(
                    pending,
                    hostname,
                    node_key,
                    units.iter().find(|u| u.command == SEND_COMMAND),
                    &progress,
                    &initial,
                )
                .await;
            }
            if let SendStop::Relay { applied, .. } | SendStop::Superseded { applied, .. } =
                &mut stop
            {
                applied.clone_from(&progress.landed);
            }
            Err(stop)
        }
    }
}

/// How far a walk got, for the end state of a walk that stopped (PF-3 review).
#[derive(Debug, Default)]
struct WalkProgress {
    /// The fields whose Hub write landed in this walk: bounds in effect.
    landed: Vec<String>,
    /// The Node unit reached the wire.
    node_sent: bool,
    /// The Node unit's own failure already wrote its rows' end state.
    node_settled: bool,
}

/// One walk's fixed context.
struct Walk<'w> {
    pending: &'w PendingService,
    relay: &'w dyn UnitSender,
    key: &'w crate::keys::UserSigningKey,
    actor: &'w str,
    hostname: &'w str,
    node_key: &'w str,
    budget: Duration,
    initial: &'w std::collections::HashMap<String, String>,
}

impl Walk<'_> {
    async fn units(
        &self,
        units: &[SendUnit],
        confirmations: &mut broadcast::Receiver<crate::state::ChangeEvent>,
        progress: &mut WalkProgress,
    ) -> Result<usize, SendStop> {
        let (pending, relay, key, actor) = (self.pending, self.relay, self.key, self.actor);
        let (hostname, node_key, budget) = (self.hostname, self.node_key, self.budget);
        let mut guard = InterruptGuard::new(pending, hostname, node_key);
        // PF-3 review, item 1: while the walk writes the Hub, a Discard waits. The Hub cannot be
        // told to forget a write in flight, and a Discard landing under it left the Hub and the
        // portal on a band nobody drafted any more. Released before the Node unit, whose wait
        // can be long and which a Discard may still cut short (N30).
        // The claim put the Node in its Hub phase; held here (a walk without a claim, in tests,
        // enters it itself) and ended at the Node unit — at once for a walk with no Hub unit.
        let mut hub_phase = Some(pending.enter_hub_phase(hostname, node_key));
        if units.iter().all(|u| u.command == SEND_COMMAND) {
            hub_phase = None;
        }

        for unit in units {
            // PF-3: the Node's part of a send that does not land is `undelivered`, not `error` —
            // for a threshold row the Hub already holds it — and the Node gets a warning.
            let node_unit = unit.command == SEND_COMMAND;
            if node_unit {
                hub_phase = None;
            }
            let failed = if node_unit {
                PENDING_STATE_UNDELIVERED
            } else {
                PENDING_STATE_ERROR
            };

            // A refused unit never reaches the wire (Send refuses it up front; this is the
            // backstop). Its rows stay drafted with the reason.
            if let Some(refusal) = &unit.refusal {
                let message =
                    refusal.message(|k| crate::i18n::lookup(crate::i18n::current_lang(), k));
                mark_failed(pending, hostname, node_key, unit, failed, &message).await;
                if node_unit {
                    progress.node_settled = true;
                    pending.sync_config_warning(hostname, node_key).await;
                }
                return Err(SendStop::Refused {
                    param_key: unit.label.clone(),
                    refusal: refusal.clone(),
                });
            }

            // A value the portal cannot read never reaches the wire.
            if let Some(param_key) = &unit.unreadable {
                let message = format!("stored value for {param_key} is not readable");
                mark_failed(pending, hostname, node_key, unit, failed, &message).await;
                if node_unit {
                    progress.node_settled = true;
                    pending.sync_config_warning(hostname, node_key).await;
                }
                return Err(SendStop::Unreadable {
                    param_key: param_key.clone(),
                });
            }

            // PF-3 review, item 1: a Hub unit writes the Hub for its field's Node rows too, and a
            // field whose every row rides the Node unit has no row of its own to mark. Those rows
            // are re-read here, before the wire, like the unit's own below.
            if !node_unit {
                let represented: Vec<SendRow> =
                    unit.node_previous.iter().map(|p| p.row.clone()).collect();
                if !pending
                    .still_planned(hostname, node_key, &represented)
                    .await
                {
                    pending.nudge(hostname, node_key);
                    return Err(SendStop::Superseded {
                        param_key: unit.label.clone(),
                        applied: Vec::new(),
                    });
                }
            }

            // The unit's rows carry one state between them: a threshold field's
            // four bounds move together because they are sent together, so
            // marking one and not the others would render a banner that says
            // three different things about one row of the table.
            //
            // This is also the re-read. `Ok(false)` = the row is gone or holds a
            // different value than the plan was built from, so the walk has been
            // overtaken by a Discard or an edit and stops here rather than
            // sending a value nobody is asking for any more.
            for (marked, row) in unit.param_keys.iter().enumerate() {
                guard.arm_row(row);
                match pending
                    .set_state(
                        hostname,
                        node_key,
                        &row.param_key,
                        Some(row.desired_value.clone()),
                        PENDING_STATE_SENDING,
                        None,
                        true,
                    )
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        // Nothing went on the wire, so the rows this loop
                        // already marked go back to what they were before the
                        // walk: "not sent yet" is what is true of them, and it
                        // is the state the operator's next Send starts from.
                        // The row that missed is left exactly as whoever
                        // overtook the walk left it — `rollback_sending` covers
                        // only `..marked`.
                        guard.clear();
                        rollback_sending(
                            pending,
                            hostname,
                            node_key,
                            &unit.param_keys[..marked],
                            self.initial,
                        )
                        .await;
                        pending.nudge(hostname, node_key);
                        return Err(SendStop::Superseded {
                            param_key: unit.label.clone(),
                            applied: Vec::new(),
                        });
                    }
                    Err(err) => {
                        // A write the portal cannot make is a stop, not a
                        // warning to publish past (§21 review round 2, item 3).
                        tracing::warn!(%err, param_key = %row.param_key, "pending config: could not mark sending");
                        let message = format!("could not record the send: {err}");
                        guard.clear();
                        mark_failed(pending, hostname, node_key, unit, failed, &message).await;
                        if node_unit {
                            progress.node_settled = true;
                            pending.sync_config_warning(hostname, node_key).await;
                        }
                        return Err(SendStop::Storage {
                            param_key: unit.label.clone(),
                            message,
                        });
                    }
                }
            }
            pending.nudge(hostname, node_key);
            if node_unit {
                if !unit.node_bands.is_empty() {
                    pending
                        .delivery_pending(hostname, node_key, &unit.node_bands)
                        .await;
                }
                // PF-3 review, item 5: an earlier attempt still waiting in the Hub's queue goes
                // before this one is queued, or "Send again" puts a second set-config with
                // `save` behind the first — two restarts, twice the air time. A frame already
                // delivered stays: its late ack still finishes the unit, so the unit is
                // forgotten only when every earlier frame came back out.
                let stale = pending.take_queued(hostname, node_key);
                if !stale.is_empty() {
                    match relay.drop_queued(node_key, &stale).await {
                        Ok(dropped) => {
                            tracing::info!(
                                hostname,
                                node_key,
                                queued = stale.len(),
                                dropped,
                                "earlier attempt taken out of the queue before the resend"
                            );
                            if dropped >= stale.len() {
                                pending.forget_late(hostname, node_key);
                            }
                        }
                        Err(err) => tracing::warn!(
                            %err,
                            node_key,
                            "the earlier attempt could not be taken out of the queue"
                        ),
                    }
                }
            }

            // Anything already queued on the receiver was emitted before this
            // unit's downlink and therefore cannot be its confirmation
            // (§21 review, item 5). The wire carries no request id, so
            // `(node_key, command)` is the whole correlation and a stale result
            // for the *previous* unit would otherwise confirm this one
            // instantly. Same receiver throughout, which is what keeps the
            // subscribe-before-relay guarantee.
            //
            // **Caveat this does not close**: an admin using the service card's
            // SendParam (`/nodes/{k}/service/send-param`, also
            // `set_sticker_config`) *while* a walk is running can still have
            // their device answer counted as this unit's confirmation. Closing
            // it needs a request id on the `.../command` result, which the Hub
            // does not send — recorded in devices.backend.md §21.12.
            // P-7 (nodes_phase1.cs.md, dodatek 2026-09-25): on a carrier whose join flushes the
            // Node's queue (LoRaWAN), a unit whose frames were queued before the Node rejoined is
            // sent once more. On P2P the central still holds them, and a resend would deliver the
            // command twice (bench 2026-09-27, 5722), so the walk keeps waiting instead.
            let join_flushes = relay.join_flushes_queue(node_key);
            let mut resent = false;
            // The last attempt's radio `seq`s, and those the Node has not answered yet (PF-3).
            let mut on_radio: bool;
            let mut sent_seqs: Vec<u64>;
            let mut unanswered: Vec<u64> = Vec::new();
            let confirmation = loop {
                drain_stale_confirmations(confirmations);
                let queued_at = now_unix();
                if node_unit {
                    // config_sync.md §13 Q11: each send of the Node's message restarts its
                    // 4 h deadline and counts an attempt.
                    pending.note_node_sent(hostname, node_key).await;
                }
                // Decision #41 (Hynek 2026-10-15): the Hub no longer grades Node thresholds, so
                // `set_lorawan_field_threshold` is not sent any more. A threshold unit is the
                // portal's own write: it lands below without a Hub round trip.
                if !node_unit {
                    on_radio = false;
                    sent_seqs = Vec::new();
                    break Confirmation::Confirmed;
                }

                let outcome = relay
                    .send_unit(actor, key, hostname, unit.command, unit.params.clone())
                    .await;

                // `send_unit` goes through `relay::execute`, which waits, so
                // "success" is the Hub having accepted; anything else is it
                // refusing or not answering.
                if outcome.status != "success" {
                    let message = outcome
                        .message
                        .clone()
                        .unwrap_or_else(|| outcome.status.to_string());
                    guard.clear();
                    mark_failed(pending, hostname, node_key, unit, failed, &message).await;
                    if node_unit {
                        progress.node_settled = true;
                        return Err(
                            node_undelivered(pending, hostname, node_key, unit, message).await
                        );
                    }
                    return Err(SendStop::Relay {
                        param_key: unit.label.clone(),
                        message,
                        applied: Vec::new(),
                    });
                }
                if node_unit {
                    progress.node_sent = true;
                }

                // The Hub accepted. Now the device — see the module doc's two
                // clocks.
                let radio_seqs = outcome
                    .request_id
                    .as_deref()
                    .and_then(|r| r.strip_prefix(RADIO_SEQ_PREFIX))
                    .map(|list| {
                        list.split(',')
                            .filter_map(|s| s.parse::<u64>().ok())
                            .collect::<Vec<_>>()
                    });
                if let Some(seqs) = &radio_seqs {
                    pending.remember_queued(hostname, node_key, seqs);
                }
                on_radio = radio_seqs.is_some();
                sent_seqs = radio_seqs.unwrap_or_default();
                unanswered.clone_from(&sent_seqs);
                // A Hub-side write has no device leg (`THRESHOLD_COMMAND`): the Hub's answer is
                // the whole answer, and an edit under it is caught by the landing write below.
                if !node_unit {
                    break Confirmation::Confirmed;
                }
                // Waited on beside the draft (N30, night test): a Discard or an edit under a walk
                // that was waiting for the Node used to keep the Node claimed until the budget
                // ran out.
                let mut nudges = pending.subscribe_nudges();
                let answer = async {
                    if on_radio {
                        await_radio_acks(
                            &mut *confirmations,
                            hostname,
                            node_key,
                            &mut unanswered,
                            budget,
                            join_flushes.then_some(queued_at),
                        )
                        .await
                    } else {
                        await_device_confirmation(
                            &mut *confirmations,
                            hostname,
                            node_key,
                            unit.command,
                            budget,
                        )
                        .await
                    }
                };
                let confirmation = tokio::select! {
                    confirmation = answer => confirmation,
                    () = overtaken(pending, hostname, node_key, &unit.param_keys, &mut nudges) => {
                        Confirmation::Overtaken
                    }
                };
                match confirmation {
                    Confirmation::Rejoined if !resent => {
                        tracing::info!(
                            hostname,
                            node_key,
                            unit = %unit.label,
                            "the Node rejoined before confirming; its queue was flushed, sending again"
                        );
                        resent = true;
                    }
                    Confirmation::Rejoined => break Confirmation::Silent,
                    other => break other,
                }
            };
            if confirmation == Confirmation::Overtaken {
                // Nothing of this unit is ours to mark any more: the rows are gone or hold a
                // newer value. The frames still queued come back out (PF-3 review, item 5): the
                // Node would otherwise apply what nobody asks for any more. If some already
                // left, the Node's late ack still tells what it took.
                guard.clear();
                if node_unit {
                    let mine = pending.take_queued(hostname, node_key);
                    let dropped = if mine.is_empty() {
                        0
                    } else {
                        relay.drop_queued(node_key, &mine).await.unwrap_or(0)
                    };
                    if on_radio && dropped < mine.len() {
                        let late = LateUnit {
                            hostname: hostname.to_string(),
                            node_key: node_key.to_string(),
                            label: unit.label.clone(),
                            seqs: sent_seqs,
                            unanswered,
                            rows: unit.param_keys.clone(),
                            alarm_layout: alarm_layout_of(unit),
                            node_bands: unit.node_bands.clone(),
                            given_up_at: std::time::Instant::now(),
                        };
                        remember_late_unit(pending, confirmations, late).await;
                    }
                }
                pending.nudge(hostname, node_key);
                return Err(SendStop::Superseded {
                    param_key: unit.label.clone(),
                    applied: Vec::new(),
                });
            }
            if confirmation != Confirmation::Confirmed {
                let secs = budget.as_secs();
                let message = match &confirmation {
                    Confirmation::Lost => format!(
                        "no confirmation within {secs} s — the portal dropped messages \
                         while waiting, so the device may have applied this already; \
                         check the Node before resending"
                    ),
                    Confirmation::Refused(field) => refusal_message(field.as_deref()),
                    // Discarded before it was sent (#129): the reason says why.
                    Confirmation::Undelivered(Some(reason)) => format!("not sent: {reason}"),
                    Confirmation::Undelivered(None) => format!(
                        "the Hub gave the downlink up after {} attempts",
                        control_radio::p2p::session::UNDELIVERED_LIMIT
                    ),
                    _ => format!("device did not confirm within {secs} s"),
                };
                guard.clear();
                // A Node's refusal rolled the whole message back: nothing of it is on the
                // Node, so its rows are `error`, not `undelivered` (STICKER 2.0.0).
                let failed = if matches!(confirmation, Confirmation::Refused(_)) {
                    PENDING_STATE_ERROR
                } else {
                    failed
                };
                mark_failed(pending, hostname, node_key, unit, failed, &message).await;
                // PF-3: out of budget is not a refusal. The Node may still answer, and its late
                // ok finishes the unit.
                if on_radio
                    && matches!(
                        confirmation,
                        Confirmation::Silent | Confirmation::Lost | Confirmation::Undelivered(_)
                    )
                {
                    let late = LateUnit {
                        hostname: hostname.to_string(),
                        node_key: node_key.to_string(),
                        label: unit.label.clone(),
                        seqs: sent_seqs,
                        unanswered,
                        rows: unit.param_keys.clone(),
                        alarm_layout: alarm_layout_of(unit),
                        node_bands: unit.node_bands.clone(),
                        given_up_at: std::time::Instant::now(),
                    };
                    remember_late_unit(pending, confirmations, late).await;
                }
                progress.node_settled = node_unit;
                if node_unit {
                    return Err(node_undelivered(pending, hostname, node_key, unit, message).await);
                }
                return Err(SendStop::Relay {
                    param_key: unit.label.clone(),
                    message,
                    applied: Vec::new(),
                });
            }

            guard.clear();
            if !node_unit {
                // PF-3: the Hub has the band, so the portal follows at once — the revert point on
                // the Node's rows, `field_thresholds`, and the Hub's own rows, as one write (PF-3
                // review, item 1). An edit that landed while the Hub was being written still
                // gets the revert point, and stops the walk: its plan is stale.
                let field = unit
                    .threshold
                    .as_ref()
                    .map(|t| t.field.clone())
                    .unwrap_or_else(|| unit.label.clone());
                match pending.land_hub(hostname, node_key, unit, actor).await {
                    Ok(true) => progress.landed.push(field),
                    Ok(false) => {
                        progress.landed.push(field);
                        pending.nudge(hostname, node_key);
                        return Err(SendStop::Superseded {
                            param_key: unit.label.clone(),
                            applied: Vec::new(),
                        });
                    }
                    Err(err) => {
                        tracing::warn!(%err, unit = %unit.label, "the Hub took the band, but the portal could not record it");
                        let message =
                            format!("the Hub took this, but the portal could not record it: {err}");
                        mark_failed(pending, hostname, node_key, unit, failed, &message).await;
                        return Err(SendStop::Storage {
                            param_key: unit.label.clone(),
                            message,
                        });
                    }
                }
                pending.nudge(hostname, node_key);
                continue;
            }

            // Acked: nothing of this unit is left in the queue for a Discard to take out.
            pending.take_queued(hostname, node_key);
            for row in &unit.param_keys {
                // Conditioned on the value that was actually sent: a row edited
                // while the device was being waited on is a NEW intention, and
                // deleting it here would throw away a change nobody sent.
                match pending
                    .delete(
                        hostname,
                        node_key,
                        &row.param_key,
                        Some(row.desired_value.clone()),
                    )
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => tracing::info!(
                        param_key = %row.param_key,
                        "pending config: confirmed row was edited under the walk; kept"
                    ),
                    Err(err) => {
                        // `guard.clear()` ran above, so nothing owns this row
                        // any more: fall back to a terminal state instead — the
                        // send did land, so the message says so.
                        tracing::warn!(%err, param_key = %row.param_key, "pending config: confirmed row could not be dropped");
                        let stuck = pending
                            .set_state(
                                hostname,
                                node_key,
                                &row.param_key,
                                Some(row.desired_value.clone()),
                                PENDING_STATE_ERROR,
                                Some(
                                    "the device confirmed this value, but the draft row \
                                     could not be cleared — discard it by hand"
                                        .to_string(),
                                ),
                                false,
                            )
                            .await;
                        if let Err(err) = stuck {
                            tracing::error!(
                                %err,
                                param_key = %row.param_key,
                                "pending config: row is stranded in `sending` until restart"
                            );
                        }
                    }
                }
            }
            // The table the Node now holds, and the warning a confirmed resend closes.
            let layout = alarm_layout_of(unit);
            if layout.is_some() {
                pending
                    .delivery_confirmed(hostname, node_key, &unit.node_bands)
                    .await;
            }
            WriteThrough {
                thresholds: Vec::new(),
                alarm_layout: layout,
            }
            .apply(&pending.db, hostname, node_key, Some(actor))
            .await;
            progress.node_settled = true;
            pending.sync_config_warning(hostname, node_key).await;
            pending.nudge(hostname, node_key);
        }
        drop(hub_phase);

        Ok(units.len())
    }
}

/// The end state of the Node unit's rows after a walk that stopped (PF-3 review, items 9 and
/// 10). For each row still holding the value the walk planned: a bound **the Hub holds** is
/// `undelivered` — in effect, not on the Node — and every other row is what it was before the
/// walk, except that a row that was `undelivered` and whose value the Hub no longer holds owes
/// the Hub its write again and is `pending`. A table that went on the wire and was not
/// confirmed is "not delivered". Then the warning follows the rows.
///
/// "The Hub holds it" is this walk having landed the field's Hub write, or `field_thresholds`
/// (which mirrors the Hub) holding the row's value (PF-3 review round 2, item A). A
/// `previous_value` alone says only that *some* send reached the Hub: an `undelivered` 30 edited
/// to 33, whose Hub write then failed, carries one while the Hub holds 30 — and marking 33
/// `undelivered` made "Send again" put 33 on the Node alone.
async fn settle_node_rows(
    pending: &PendingService,
    hostname: &str,
    node_key: &str,
    node_unit: Option<&SendUnit>,
    progress: &WalkProgress,
    initial: &std::collections::HashMap<String, String>,
) {
    let node_sent = progress.node_sent;
    if let Some(unit) = node_unit {
        let rows = pending.list(hostname, node_key).await.unwrap_or_default();
        let hub = pending.held_thresholds(hostname, node_key).await;
        for planned in &unit.param_keys {
            let Some(row) = rows.iter().find(|r| r.param_key == planned.param_key) else {
                continue;
            };
            if row.desired_value != planned.desired_value {
                continue;
            }
            let threshold = row.kind == PENDING_KIND_THRESHOLD;
            let landed = threshold_field_of(&row.param_key)
                .is_some_and(|field| progress.landed.contains(&field));
            let hub_holds = threshold && (landed || hub_holds_value(&hub, row));
            let taken = row.previous_value.is_some() && hub_holds;
            let before = state_before(initial, &row.param_key);
            let target = if taken {
                PENDING_STATE_UNDELIVERED
            } else if threshold && before == PENDING_STATE_UNDELIVERED {
                PENDING_STATE_PENDING
            } else {
                before
            };
            if row.state == target {
                continue;
            }
            let message = taken.then(|| {
                if node_sent {
                    "the draft changed while the Node was waited for; the bound is in effect, \
                     the Node does not have it"
                        .to_string()
                } else {
                    "the send stopped before the Node's part; the bound is in effect, the Node \
                     does not have it"
                        .to_string()
                }
            });
            if let Err(err) = pending
                .set_state(
                    hostname,
                    node_key,
                    &row.param_key,
                    Some(planned.desired_value.clone()),
                    target,
                    message,
                    false,
                )
                .await
            {
                tracing::warn!(%err, param_key = %row.param_key, "pending config: a stopped walk's row kept its state");
            }
        }
        if node_sent && !unit.node_bands.is_empty() {
            pending.delivery_undelivered(hostname, node_key).await;
        }
    }
    pending.sync_config_warning(hostname, node_key).await;
    pending.nudge(hostname, node_key);
}

/// The field a threshold row (bound or `enabled`) belongs to.
fn threshold_field_of(param_key: &str) -> Option<String> {
    parse_threshold_param_key(param_key)
        .map(|(field, _)| field)
        .or_else(|| parse_threshold_enabled_key(param_key))
}

/// `field_thresholds` — the Hub's mirror — holds this threshold row's value.
fn hub_holds_value(held: &[portal_storage::FieldThreshold], row: &PendingConfigRow) -> bool {
    let Some(raw) = row.desired_value.as_deref() else {
        return false;
    };
    if let Some((field, bound)) = parse_threshold_param_key(&row.param_key) {
        let Some(t) = held.iter().find(|t| t.field == field) else {
            return false;
        };
        let Ok(want) = serde_json::from_str::<Option<f64>>(raw) else {
            return false;
        };
        let have = match bound {
            "critical_low" => t.critical_low,
            "warning_low" => t.warning_low,
            "warning_high" => t.warning_high,
            _ => t.critical_high,
        };
        return match (want, have) {
            (Some(a), Some(b)) => (a - b).abs() < 1e-9,
            (None, None) => true,
            _ => false,
        };
    }
    if let Some(field) = parse_threshold_enabled_key(&row.param_key) {
        let want = serde_json::from_str::<bool>(raw).ok();
        return held
            .iter()
            .find(|t| t.field == field)
            .is_some_and(|t| Some(t.enabled) == want);
    }
    false
}

/// A row's state before the walk, as one of the constants `set_state` takes. A row that was
/// `sending` then belonged to a walk that no longer exists, and is simply not sent.
fn state_before(initial: &std::collections::HashMap<String, String>, key: &str) -> &'static str {
    match initial.get(key).map(String::as_str) {
        Some(s) if s == PENDING_STATE_ERROR => PENDING_STATE_ERROR,
        Some(s) if s == PENDING_STATE_UNDELIVERED => PENDING_STATE_UNDELIVERED,
        _ => PENDING_STATE_PENDING,
    }
}

/// The Node's part of the send did not land (PF-3): its table is "not delivered", and either
/// the warning opens (a bound is undelivered) or, for config keys alone, an archived entry is
/// left. The rows are already `undelivered` ([`mark_failed`]).
async fn node_undelivered(
    pending: &PendingService,
    hostname: &str,
    node_key: &str,
    unit: &SendUnit,
    message: String,
) -> SendStop {
    if !unit.node_bands.is_empty() {
        pending.delivery_undelivered(hostname, node_key).await;
    }
    let config_keys: Vec<String> = unit
        .param_keys
        .iter()
        .filter(|r| !is_threshold_key(&r.param_key))
        .map(|r| r.param_key.clone())
        .collect();
    let bounds = unit.param_keys.len() > config_keys.len();
    if !bounds && !config_keys.is_empty() {
        pending
            .archive_config_warning(hostname, node_key, &config_keys)
            .await;
    }
    pending.sync_config_warning(hostname, node_key).await;
    SendStop::NodeUndelivered {
        param_key: unit.label.clone(),
        message,
        config_keys,
    }
}

/// A threshold row's key, bound or `enabled`.
fn is_threshold_key(param_key: &str) -> bool {
    parse_threshold_param_key(param_key).is_some()
        || parse_threshold_enabled_key(param_key).is_some()
}

/// Remembers a Node unit the walk gave up on (PF-3), so the Node's late ok still finishes it.
///
/// An answer that landed between the deadline and the remember is still on the walk's own
/// receiver; the listener may have seen it while the unit was not yet remembered, so it is
/// replayed from here. Answering twice is harmless: the first answer forgets the `seq`.
async fn remember_late_unit(
    pending: &PendingService,
    confirmations: &mut broadcast::Receiver<crate::state::ChangeEvent>,
    late: LateUnit,
) {
    if late.unanswered.is_empty() || (late.rows.is_empty() && late.alarm_layout.is_none()) {
        return;
    }
    tracing::info!(
        hostname = %late.hostname,
        node_key = %late.node_key,
        unit = %late.label,
        seqs = ?late.unanswered,
        "walk gave up on a Node unit; a late ack still finishes it"
    );
    pending.remember_late(late);
    loop {
        match confirmations.try_recv() {
            Ok(crate::state::ChangeEvent::NodeCommandResult {
                hostname,
                node_key,
                payload,
            }) => {
                pending
                    .apply_late_answer(&hostname, &node_key, &payload)
                    .await;
            }
            Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(_) => return,
        }
    }
}

/// The Hub writes a Discard puts back (PF-3, nodes_phase1.cs.md dodatek 2026-09-27, rule 9):
/// one `set_lorawan_field_threshold` per field with a row whose value a send already wrote to
/// the Hub (`previous_value` set), carrying **the field's whole band from before that send**
/// (`previous_band` of its oldest row, PF-3 review item 3 and rounds 2–3). The Hub took the warning bounds in the same write and
/// their own rows are gone by then, so a bound-by-bound revert left half a send behind and could
/// invert a band. A field whose rows carry only a bound (no band) falls back to the band the
/// portal holds now with those bounds back. Each unit's rows are every threshold row of the field:
/// once the Hub has it back, that field's draft is finished. Nothing for a draft no send has
/// reached the Hub with.
/// *Amended 2026-10-15 (config_sync.md §13 Q20e):* the Discard route no longer reverts; this
/// stays for the revert machinery's tests.
#[cfg_attr(not(test), allow(dead_code))]
pub fn plan_revert(
    rows: &[PendingConfigRow],
    node_key: &str,
    current_thresholds: &std::collections::BTreeMap<String, crate::render::ThresholdEntryCtx>,
) -> Vec<SendUnit> {
    use std::collections::BTreeMap;

    let field_of = |param_key: &str| {
        parse_threshold_param_key(param_key)
            .map(|(field, bound)| (field, Some(bound)))
            .or_else(|| parse_threshold_enabled_key(param_key).map(|field| (field, None)))
    };
    let mut by_field: BTreeMap<String, Vec<&PendingConfigRow>> = BTreeMap::new();
    for row in rows.iter().filter(|r| r.kind == PENDING_KIND_THRESHOLD) {
        if let Some((field, _)) = field_of(&row.param_key) {
            by_field.entry(field).or_default().push(row);
        }
    }

    let mut units = Vec::new();
    for (field, rows) in by_field {
        if !rows.iter().any(|r| r.previous_value.is_some()) {
            continue;
        }
        // The oldest landing still in the draft (PF-3 review rounds 2 and 3): the band from before
        // the field's first undelivered change, or — once a late ack finished that change's rows
        // — from before the next one.
        let band = rows
            .iter()
            .filter(|r| r.previous_band.is_some())
            .min_by_key(|r| r.previous_order.unwrap_or(i64::MAX))
            .and_then(|r| r.previous_band.as_deref())
            .and_then(|raw| serde_json::from_str::<BandJson>(raw).ok());
        // The warning bounds are not the send's to take back (devices.node.md, amendment
        // 2026-10-13): a save stores them at once, so a revert carries the ones the portal holds
        // now, and the write keeps whatever it holds when it lands. A warning a draft from
        // before then sent stays in effect.
        let warnings_drafted = false;
        let write = match band {
            Some(b) => ThresholdWrite {
                field: field.clone(),
                critical_low: b.critical_low,
                warning_low: b.warning_low,
                warning_high: b.warning_high,
                critical_high: b.critical_high,
                enabled: b.enabled,
                warnings_drafted,
            },
            None => {
                let current = current_thresholds.get(&field);
                let mut write = ThresholdWrite {
                    field: field.clone(),
                    critical_low: current.and_then(|c| c.critical_low),
                    warning_low: current.and_then(|c| c.warning_low),
                    warning_high: current.and_then(|c| c.warning_high),
                    critical_high: current.and_then(|c| c.critical_high),
                    enabled: current.map(|c| c.enabled).unwrap_or(true),
                    warnings_drafted,
                };
                for row in &rows {
                    let (Some(previous), Some((_, bound))) =
                        (row.previous_value.as_deref(), field_of(&row.param_key))
                    else {
                        continue;
                    };
                    match bound {
                        Some(bound) => {
                            if let Ok(value) = serde_json::from_str::<Option<f64>>(previous) {
                                match bound {
                                    "critical_low" => write.critical_low = value,
                                    "warning_low" => write.warning_low = value,
                                    "warning_high" => write.warning_high = value,
                                    _ => write.critical_high = value,
                                }
                            }
                        }
                        None => {
                            if let Ok(value) = serde_json::from_str::<bool>(previous) {
                                write.enabled = value;
                            }
                        }
                    }
                }
                write
            }
        };
        let mut write = write;
        if let Some(c) = current_thresholds.get(&field) {
            write.warning_low = c.warning_low;
            write.warning_high = c.warning_high;
        }
        units.push(SendUnit {
            param_keys: rows.iter().map(|r| SendRow::of(r)).collect(),
            label: format!("threshold.{field}"),
            command: THRESHOLD_COMMAND,
            params: json!({
                "dev_eui": node_key,
                "field": field,
                "critical_low": write.critical_low,
                "warning_low": write.warning_low,
                "warning_high": write.warning_high,
                "critical_high": write.critical_high,
                "enabled": write.enabled,
            }),
            threshold: Some(write),
            node_previous: Vec::new(),
            node_bands: Vec::new(),
            unreadable: None,
            refusal: None,
        });
    }
    units
}

/// A Discard's revert the Hub did not take (PF-3 review, item 2).
#[derive(Debug, Clone, PartialEq)]
pub struct RevertFailure {
    /// The field the Hub did not take back, and why.
    pub reason: String,
    /// The fields reverted before it: their draft is gone, the rest is kept.
    pub reverted: Vec<String>,
}

/// Sends a Discard's reverts ([`plan_revert`]) to the Hub, signed with the discarding user's
/// key. Each field the Hub takes back is finished at once (PF-3 review, item 2): its band goes
/// to `field_thresholds`, its draft rows go, and what the Node holds is its last confirmation
/// again. So a revert the Hub refuses half-way leaves only the refused fields' draft, and a
/// resend cannot put the Node on a value the Hub and the portal no longer hold. `Err` names the
/// first field the Hub did not take; `Ok` lists the fields put back.
///
/// The late unit is taken out of memory for the length of the revert, so a late ack cannot
/// finish rows under it, and put back when nothing was reverted (PF-3 review round 2, item D).
/// Taking the frames back and what the Node then holds are the Discard's
/// ([`spawn_discard`]), which knows whether anything was reverted.
pub async fn revert_sent(
    pending: &PendingService,
    sender: &dyn UnitSender,
    key: &crate::keys::UserSigningKey,
    actor: &str,
    hostname: &str,
    node_key: &str,
    reverts: &[SendUnit],
) -> Result<Vec<String>, RevertFailure> {
    let late = pending.take_late(hostname, node_key);
    let mut fields = Vec::new();
    for unit in reverts {
        let outcome = sender
            .send_unit(actor, key, hostname, unit.command, unit.params.clone())
            .await;
        if outcome.status != "success" {
            let reason = outcome
                .message
                .unwrap_or_else(|| outcome.status.to_string());
            if fields.is_empty() {
                if let Some(late) = late {
                    pending.restore_late(late);
                }
            }
            pending.sync_config_warning(hostname, node_key).await;
            pending.nudge(hostname, node_key);
            return Err(RevertFailure {
                reason: format!("{}: {reason}", unit.label),
                reverted: fields,
            });
        }
        WriteThrough {
            thresholds: unit.threshold.clone().into_iter().collect(),
            alarm_layout: None,
        }
        .apply(&pending.db, hostname, node_key, Some(actor))
        .await;
        let field = unit
            .threshold
            .as_ref()
            .map(|t| t.field.clone())
            .unwrap_or_default();
        for row in &unit.param_keys {
            if let Err(err) = pending
                .delete(
                    hostname,
                    node_key,
                    &row.param_key,
                    Some(row.desired_value.clone()),
                )
                .await
            {
                tracing::warn!(%err, param_key = %row.param_key, "discard: a reverted row could not be cleared");
            }
        }
        fields.push(field);
    }
    Ok(fields)
}

/// What a Discard did once it ran to its end.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscardDone {
    /// Fields whose band went back on the Hub and in the portal.
    pub reverted: Vec<String>,
    /// Draft rows thrown away, or why they could not be.
    pub dropped: Result<usize, String>,
    /// Frames taken back out of the Hub's queue.
    pub withdrawn: usize,
}

/// A Discard's end result: done, or the Hub refused a revert (and only the refused fields'
/// draft is left).
pub type DiscardResult = Result<DiscardDone, RevertFailure>;

/// Called with a Discard's result when it ends, whoever is still listening (the audit row).
pub type DiscardAudit = Box<dyn FnOnce(DiscardResult) -> BoxFuture<'static, ()> + Send>;

/// Runs a Discard from its revert to its end **on its own task** (PF-3 review round 3), holding
/// the Node's [`DiscardGuard`] until then. It used to run inline in the request, and a client
/// that went away mid-revert dropped the future between two fields: the late ack taken and
/// never put back, frames for a band the Hub no longer held left queued, rows claiming the Hub
/// had what it no longer had. A task is not dropped with the request — the pattern of the
/// detached send walk. The route answers from the task's result while the client is there;
/// either way the task finishes, writes its audit row (`audit`) and nudges the banner.
#[allow(clippy::too_many_arguments)]
pub fn spawn_discard(
    pending: PendingService,
    sender: Arc<dyn UnitSender>,
    key: Option<crate::keys::UserSigningKey>,
    actor: String,
    hostname: String,
    node_key: String,
    reverts: Vec<SendUnit>,
    guard: DiscardGuard,
    audit: DiscardAudit,
) -> tokio::task::JoinHandle<DiscardResult> {
    tokio::spawn(async move {
        let _guard = guard;
        let result = run_discard(
            &pending,
            &*sender,
            key.as_ref(),
            &actor,
            &hostname,
            &node_key,
            &reverts,
        )
        .await;
        audit(result.clone()).await;
        pending.nudge(&hostname, &node_key);
        result
    })
}

async fn run_discard(
    pending: &PendingService,
    sender: &dyn UnitSender,
    key: Option<&crate::keys::UserSigningKey>,
    actor: &str,
    hostname: &str,
    node_key: &str,
    reverts: &[SendUnit],
) -> DiscardResult {
    // Read before the revert sets the late unit aside: the frames this Node's sends left behind.
    let seqs = pending.seqs_left(hostname, node_key);
    let reverted = match (reverts.is_empty(), key) {
        (true, _) => Vec::new(),
        (false, Some(key)) => {
            match revert_sent(pending, sender, key, actor, hostname, node_key, reverts).await {
                Ok(reverted) => reverted,
                Err(failure) => {
                    // Item C: something was reverted, so the frames still queued carry a band
                    // the Hub no longer holds; they come back out, and the reverted fields'
                    // bands on the Node follow.
                    if !failure.reverted.is_empty() {
                        let w = pending
                            .withdraw_for_discard(sender, hostname, node_key, seqs)
                            .await;
                        pending
                            .settle_discarded_delivery(
                                hostname,
                                node_key,
                                Some(failure.reverted.clone()),
                                w.maybe_delivered,
                            )
                            .await;
                        pending.forget_late(hostname, node_key);
                        pending.sync_config_warning(hostname, node_key).await;
                    }
                    return Err(failure);
                }
            }
        }
        (false, None) => {
            return Err(RevertFailure {
                reason: "no signing key for the revert".to_string(),
                reverted: Vec::new(),
            })
        }
    };
    // P-6 and PF-3b: what the sends queued comes back out, a delivered frame too, before the
    // draft goes; a band the Node may have had is `unknown` rather than its old confirmation.
    let withdrawal = pending
        .withdraw_for_discard(sender, hostname, node_key, seqs)
        .await;
    pending
        .settle_discarded_delivery(hostname, node_key, None, withdrawal.maybe_delivered)
        .await;
    let dropped = pending
        .discard(hostname, node_key)
        .await
        .map_err(|e| e.to_string());
    Ok(DiscardDone {
        reverted,
        dropped,
        withdrawn: withdrawal.dropped,
    })
}

/// Groups the rows a restart catches in `sending` by Node, keeping the Nodes whose rows are all
/// config keys (PF-3 review, item 9): those get the archived warning a timed-out walk leaves.
fn restart_archive_of(stranded: &[PendingConfigRow]) -> Vec<ArchiveEntry> {
    let mut by_node: std::collections::BTreeMap<(String, String), (bool, Vec<String>)> =
        std::collections::BTreeMap::new();
    for row in stranded {
        let entry = by_node
            .entry((row.hostname.clone(), row.node_key.clone()))
            .or_insert((true, Vec::new()));
        entry.0 &= row.kind == PENDING_KIND_CONFIG;
        entry.1.push(row.param_key.clone());
    }
    by_node
        .into_iter()
        .filter(|(_, (config_only, _))| *config_only)
        .map(|((h, n), (_, keys))| (h, n, keys))
        .collect()
}

/// The notifier's view of a Node's configuration warning (PF-3).
fn warning_notify_data(
    row: &portal_storage::AlarmEvent,
    node_key: &str,
    identity: &NodeIdentity,
) -> crate::alarms::notifier::AlarmNotifyData {
    crate::alarms::notifier::AlarmNotifyData {
        alarm_id: row.id.clone(),
        hostname: row.hostname.clone(),
        severity: row.severity.clone(),
        alarm_type: row.alarm_type.clone(),
        source: crate::notify::payload::AlarmSource::Sticker,
        value: None,
        threshold: None,
        message: row.message.clone(),
        sensor_name: identity.name.clone(),
        sensor_id: row.sensor_id.clone(),
        line: None,
        location: identity.location.clone(),
        node_key: Some(node_key.to_string()),
        serial_number: identity.serial_number.clone(),
        mac: None,
        triggered_at: row.triggered_at.clone(),
    }
}

/// Empties the confirmation receiver without waiting. See the call site for
/// why a queued event can never be the next unit's answer.
fn drain_stale_confirmations(confirmations: &mut broadcast::Receiver<crate::state::ChangeEvent>) {
    loop {
        match confirmations.try_recv() {
            Ok(_stale) => continue,
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                tracing::warn!(missed = n, "pending config: confirmation subscriber lagged");
                continue;
            }
            // Empty (nothing queued) or Closed (nothing ever again).
            Err(_) => return,
        }
    }
}

/// Puts rows this walk marked `sending` back to `pending` after a stop that
/// published nothing (§21 review round 2, item 2).
///
/// Conditioned on the planned `desired_value`, like every other write the
/// walk makes: a row somebody edited while this was happening is a new
/// intention and keeps its own state.
async fn rollback_sending(
    pending: &PendingService,
    hostname: &str,
    node_key: &str,
    rows: &[SendRow],
    initial: &std::collections::HashMap<String, String>,
) {
    for row in rows {
        if let Err(err) = pending
            .set_state(
                hostname,
                node_key,
                &row.param_key,
                Some(row.desired_value.clone()),
                state_before(initial, &row.param_key),
                None,
                false,
            )
            .await
        {
            tracing::warn!(%err, param_key = %row.param_key, "pending config: could not roll a marked row back to pending");
        }
    }
}

/// Writes a unit's failure into its rows: `error`, or `undelivered` for the Node's part of a
/// send (PF-3).
async fn mark_failed(
    pending: &PendingService,
    hostname: &str,
    node_key: &str,
    unit: &SendUnit,
    state: &'static str,
    message: &str,
) {
    for row in &unit.param_keys {
        if let Err(err) = pending
            .set_state(
                hostname,
                node_key,
                &row.param_key,
                Some(row.desired_value.clone()),
                state,
                Some(message.to_string()),
                false,
            )
            .await
        {
            tracing::warn!(%err, param_key = %row.param_key, "pending config: could not record the failure");
            // Same hole as the delete loop above: the walk is about to
            // return, so a row left in `sending` has nothing watching it.
            // Retry without the value guard — the row may have been edited
            // under us, and a terminal state with a stale message still
            // beats one that disables both buttons until a restart.
            if let Err(err) = pending
                .set_state(
                    hostname,
                    node_key,
                    &row.param_key,
                    None,
                    state,
                    Some(message.to_string()),
                    false,
                )
                .await
            {
                tracing::error!(
                    %err,
                    param_key = %row.param_key,
                    "pending config: row is stranded in `sending` until restart"
                );
            }
        }
    }
    pending.nudge(hostname, node_key);
}

/// Waits for this Node to answer the command it was just sent.
///
/// `true` = the device confirmed inside [`DEVICE_CONFIRM_BUDGET`]. A lagged
/// subscriber is read as **not confirmed**: the alternative is deleting a
/// draft row on the strength of an event nobody saw.
///
/// A threshold write is a fleet-config change on the Hub and never reaches
/// the Node, so it has no device confirmation to wait for — the Hub's own
/// "success" is the whole answer, and this returns immediately for it.
///
/// **The correlation is `(hostname, node_key, command)`** (§21 review round
/// 2, item 8). `hostname` used to be dropped, which left one hole nothing
/// else covered: `node_key` is a DevEUI, and the same physical Node moved
/// between two Hubs — or two Hubs told about the same key — would have
/// either Hub's answer confirm the other Hub's downlink. Phase 1 is
/// single-Hub so it cannot happen today, but the event carries the field
/// and filtering on it costs one comparison. What is still open is
/// narrower and needs the wire to change: the walk cannot tell its own
/// downlink from a *concurrent* one to the same Node and command — the
/// service card's SendParam — because the `.../command` result carries no
/// request id (devices.backend.md §21.12).
/// Why a wait ended, so the row's message can be honest about it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Confirmation {
    /// The Node joined again while its frames were queued: ChirpStack flushed them (P-7).
    Rejoined,
    /// The device answered, or the command is one that needs no answer.
    Confirmed,
    /// The budget ran out with the channel intact: the device really was
    /// silent.
    Silent,
    /// The budget ran out, but the subscriber lagged at least once while
    /// waiting — so the answer may have arrived and been dropped. Reported
    /// apart from `Silent` because the two call for different operator
    /// action: resending is right after silence and risks a duplicate
    /// downlink after a loss, and telling them "the device did not answer"
    /// when we simply stopped listening is a lie the row would carry.
    Lost,
    /// The device answered with a refusal (a control.radio `error` for the unit's seq). The
    /// STICKER rolls the whole message back; carries the field it named, if any.
    Refused(Option<String>),
    /// The Hub gave one of the unit's frames up after its unacked attempts, the `undelivered`
    /// command document (p2p_tower_gateway.md §10.5, amended 2026-10-07). Ends the wait at
    /// once and reads as a silent Node, not a refusal: the Node may have applied it and only
    /// its ACK was lost, so a late answer still finishes the unit. Carries the document's
    /// `reason` when the network server discarded the frame unsent (#129).
    Undelivered(Option<String>),
    /// The unit's rows were discarded or edited while the walk waited (N30, night test). The
    /// walk stops there and gives the Node back; the rows are someone else's now.
    Overtaken,
}

/// Waits for the Node's control.radio answers to the sequences a [`RadioUnitSender`] queued:
/// an `ack` (or any ok answer) for each, correlated on `(hostname, node_key, seq)`. A refusal
/// for any of them ends the wait as [`Confirmation::Refused`]. Same budget and lag rules as
/// [`await_device_confirmation`]. The hostname compares case-insensitively, because the Hub is
/// one Hub under both of its spellings (#94).
/// Resolves once any of `rows` is gone from the draft or holds another value than the one this
/// walk sent: a Discard or an edit overtook the walk while it waited for the Node (N30). Checked
/// once up front and again on every nudge for this Node; never resolves otherwise.
async fn overtaken(
    pending: &PendingService,
    hostname: &str,
    node_key: &str,
    rows: &[SendRow],
    nudges: &mut broadcast::Receiver<PendingNudge>,
) {
    loop {
        if let Ok(current) = pending.list(hostname, node_key).await {
            let still_ours = rows.iter().all(|sent| {
                current.iter().any(|row| {
                    row.param_key == sent.param_key && row.desired_value == sent.desired_value
                })
            });
            if !still_ours {
                return;
            }
        }
        loop {
            match nudges.recv().await {
                Ok(n) if n.hostname.eq_ignore_ascii_case(hostname) && n.node_key == node_key => {
                    break
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => break,
                Err(broadcast::error::RecvError::Closed) => std::future::pending::<()>().await,
            }
        }
    }
}

/// `seqs` is left holding the sequences nobody answered, which is what a walk that gives up
/// remembers for the Node's late ack (PF-3).
///
/// `flushed_after` is when the frames were queued, on a carrier whose join empties the queue
/// (P-7): a join after it ends the wait as [`Confirmation::Rejoined`]. `None` on a carrier
/// whose join keeps the queue (P2P), where a join is no reason to stop waiting.
async fn await_radio_acks(
    confirmations: &mut broadcast::Receiver<crate::state::ChangeEvent>,
    hostname: &str,
    node_key: &str,
    seqs: &mut Vec<u64>,
    budget: Duration,
    flushed_after: Option<i64>,
) -> Confirmation {
    if seqs.is_empty() {
        return Confirmation::Confirmed;
    }
    let mut lagged = false;
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return if lagged {
                Confirmation::Lost
            } else {
                Confirmation::Silent
            };
        }
        let event = match tokio::time::timeout(remaining, confirmations.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                tracing::warn!(missed = n, "pending config: confirmation subscriber lagged");
                lagged = true;
                continue;
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => return Confirmation::Silent,
            Err(_elapsed) => {
                return if lagged {
                    Confirmation::Lost
                } else {
                    Confirmation::Silent
                }
            }
        };
        // A join at least REJOIN_MARGIN_S after the frames were queued flushed them. One sooner
        // may be the join they were queued behind, whose flush they missed.
        if let crate::state::ChangeEvent::NodeJoined {
            hostname: from_hub,
            node_key: joined,
            at,
        } = &event
        {
            if from_hub.eq_ignore_ascii_case(hostname)
                && joined == node_key
                && flushed_after.is_some_and(|queued_at| *at >= queued_at + REJOIN_MARGIN_S)
            {
                return Confirmation::Rejoined;
            }
            continue;
        }
        let crate::state::ChangeEvent::NodeCommandResult {
            hostname: from_hub,
            node_key: answered,
            payload,
        } = event
        else {
            continue;
        };
        if !from_hub.eq_ignore_ascii_case(hostname) || answered != node_key {
            continue;
        }
        if payload.get("transport").and_then(serde_json::Value::as_str) != Some("control.radio") {
            continue;
        }
        let Some(seq) = payload.get("seq").and_then(serde_json::Value::as_u64) else {
            continue;
        };
        if !seqs.contains(&seq) {
            continue;
        }
        let result = payload.get("result").and_then(serde_json::Value::as_str);
        // The Hub gave this frame up (p2p_tower_gateway.md §10.5, amended 2026-10-07): no
        // answer will come for it, so the wait ends now, as at the end of the budget.
        if result == Some(crate::relay::error_code::UNDELIVERED) {
            let reason = payload.get("reason").and_then(serde_json::Value::as_str);
            return Confirmation::Undelivered(reason.map(str::to_string));
        }
        let ok = result.is_some_and(|r| crate::sse::NODE_COMMAND_OK_RESULTS.contains(&r));
        if !ok {
            return Confirmation::Refused(refusal_field(&payload));
        }
        seqs.retain(|s| *s != seq);
        if seqs.is_empty() {
            return Confirmation::Confirmed;
        }
    }
}

/// The key a STICKER `Error` named (`fault_field` = group×100 + tag, decoded by control-radio
/// into `fault_key` / `fault_group_name`), from a command document.
fn refusal_field(payload: &serde_json::Value) -> Option<String> {
    let error = payload.get("error")?;
    error
        .get("fault_key")
        .or_else(|| error.get("fault_group_name"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// The row message for a refused message: the firmware applies all of it or nothing.
fn refusal_message(field: Option<&str>) -> String {
    match field {
        Some(field) => format!("the Node refused the message at {field}; nothing was applied"),
        None => "the Node refused the message; nothing was applied".to_string(),
    }
}

async fn await_device_confirmation(
    confirmations: &mut broadcast::Receiver<crate::state::ChangeEvent>,
    hostname: &str,
    node_key: &str,
    command: &str,
    budget: Duration,
) -> Confirmation {
    if command != SEND_COMMAND {
        return Confirmation::Confirmed;
    }
    // Sticky: one lap anywhere in the wait is enough to make "the device
    // was silent" unprovable, even if later messages arrive normally.
    let mut lagged = false;
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return if lagged {
                Confirmation::Lost
            } else {
                Confirmation::Silent
            };
        }
        let event = match tokio::time::timeout(remaining, confirmations.recv()).await {
            Ok(Ok(event)) => event,
            // Lagged: the answer may have been among the dropped messages.
            // Keep waiting — a later one may still correlate — but remember
            // that a silent verdict is no longer honest.
            Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                tracing::warn!(missed = n, "pending config: confirmation subscriber lagged");
                lagged = true;
                continue;
            }
            Ok(Err(broadcast::error::RecvError::Closed)) => return Confirmation::Silent,
            Err(_elapsed) => {
                return if lagged {
                    Confirmation::Lost
                } else {
                    Confirmation::Silent
                }
            }
        };
        let crate::state::ChangeEvent::NodeCommandResult {
            hostname: from_hub,
            node_key: answered,
            payload,
        } = event
        else {
            continue;
        };
        if from_hub != hostname || answered != node_key {
            continue;
        }
        if payload.get("command").and_then(serde_json::Value::as_str) != Some(command) {
            continue;
        }
        let ok = match payload.get("result").and_then(serde_json::Value::as_str) {
            Some(r) => crate::sse::NODE_COMMAND_OK_RESULTS.contains(&r),
            // The Node answered our command with no result field: the same
            // "no evidence of failure, do not guess one" rule
            // `sse::handle_node_command_result` applies.
            None => true,
        };
        // A refusal is an answer: the device spoke, so this is not silence
        // and not a loss either — the walk stops on the negative below.
        return if ok {
            Confirmation::Confirmed
        } else {
            Confirmation::Silent
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::ThresholdEntryCtx;
    use std::collections::BTreeMap;

    fn row(param_key: &str, kind: &str, value: &str) -> PendingConfigRow {
        PendingConfigRow {
            hostname: "hub-1".into(),
            node_key: "aa11".into(),
            param_key: param_key.into(),
            kind: kind.into(),
            desired_value: Some(value.into()),
            author: "anna".into(),
            created_at: 0,
            state: "pending".into(),
            attempts: 0,
            last_error: None,
            updated_at: 0,
            previous_value: None,
            previous_band: None,
            previous_order: None,
        }
    }

    #[test]
    fn a_threshold_key_round_trips_through_its_two_halves() {
        for (field, bound) in [
            ("temperature", "critical_low"),
            ("humidity", "warning_high"),
            ("some.dotted.field", "critical_high"),
        ] {
            let key = threshold_param_key(field, bound);
            assert_eq!(
                parse_threshold_param_key(&key),
                Some((field.to_string(), bound)),
                "{key} did not round-trip"
            );
        }
        // Not a threshold key, and a threshold key with no bound.
        assert!(parse_threshold_param_key("application.interval_report").is_none());
        assert!(parse_threshold_param_key("threshold.temperature").is_none());
        assert!(parse_threshold_param_key("threshold..critical_low").is_none());
    }

    /// The whole reason the walk groups: `set_lorawan_field_threshold`
    /// takes four bounds, a draft holds only the ones the operator touched,
    /// and sending the drafted ones alone would blank the rest.
    #[test]
    fn an_untouched_bound_is_filled_from_the_current_thresholds() {
        let mut current = BTreeMap::new();
        current.insert(
            "temperature".to_string(),
            ThresholdEntryCtx {
                critical_low: Some(-5.0),
                warning_low: Some(0.0),
                warning_high: Some(6.0),
                critical_high: Some(8.0),
                enabled: true,
                hysteresis: None,
            },
        );
        let rows = vec![row(
            "threshold.temperature.critical_high",
            PENDING_KIND_THRESHOLD,
            "9.5",
        )];

        let units = plan_send(
            &rows,
            "aa11",
            &current,
            &["temperature".to_string(), "humidity".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        // The Hub-side threshold unit, plus the on-device critical alarm: a
        // critical bound was drafted and the resolved band is two-sided.
        assert_eq!(units.len(), 2, "threshold unit + critical alarm unit");
        let t = units
            .iter()
            .find(|u| u.label == "threshold.temperature")
            .expect("threshold unit");
        let p = &t.params;
        assert_eq!(p["critical_high"], json!(9.5), "the drafted bound");
        assert_eq!(p["critical_low"], json!(-5.0), "the untouched ones survive");
        assert_eq!(p["warning_low"], json!(0.0));
        assert_eq!(p["warning_high"], json!(6.0));
        assert_eq!(p["enabled"], json!(true));
        assert_eq!(t.command, THRESHOLD_COMMAND);
        assert!(
            t.param_keys.is_empty(),
            "a slotted field's critical row belongs to the Node leg (N5)"
        );
        let node = units
            .iter()
            .find(|u| u.label == NODE_CONFIG_UNIT)
            .expect("the critical band also rides the Node's alarm table");
        assert_eq!(node.param_keys.len(), 1, "one drafted row is cleared");
    }

    #[test]
    fn every_drafted_bound_of_one_field_rides_one_command() {
        let rows = vec![
            row(
                "threshold.temperature.critical_low",
                PENDING_KIND_THRESHOLD,
                "-10",
            ),
            row(
                "threshold.temperature.critical_high",
                PENDING_KIND_THRESHOLD,
                "10",
            ),
            row(
                "threshold.humidity.warning_high",
                PENDING_KIND_THRESHOLD,
                "80",
            ),
        ];
        let units = plan_send(
            &rows,
            "aa11",
            &BTreeMap::new(),
            &["temperature".to_string(), "humidity".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        // temperature's two critical bounds -> its Hub-side threshold unit + the
        // Node's alarm table; humidity touched only a warning bound, so it adds no
        // slot. Two threshold units, one table.
        assert_eq!(units.len(), 3, "two threshold fields + the alarm table");
        let temperature = units
            .iter()
            .find(|u| u.label == "threshold.temperature")
            .expect("temperature unit");
        assert!(
            temperature.param_keys.is_empty(),
            "the two critical rows ride the Node leg"
        );
        let humidity = units
            .iter()
            .find(|u| u.label == "threshold.humidity")
            .expect("humidity unit");
        assert_eq!(humidity.param_keys.len(), 1, "a warning row stays Hub-side");
        assert_eq!(temperature.params["critical_low"], json!(-10.0));
        assert_eq!(temperature.params["critical_high"], json!(10.0));
        assert_eq!(
            temperature.params["warning_low"],
            serde_json::Value::Null,
            "a bound nobody has ever set stays unset"
        );
        let table = units
            .iter()
            .find(|u| u.label == NODE_CONFIG_UNIT)
            .expect("the alarm table");
        let config = table.params["config"].as_object().expect("config");
        assert!(
            config.contains_key("alarms.alarm_0"),
            "temperature fills slot 0"
        );
        assert!(
            !config.contains_key("alarms.alarm_1"),
            "a warning-only humidity fills no slot"
        );
    }

    /// A drafted *critical* band rewrites the Node's whole alarm table (nodes_phase1.cs.md
    /// WP2): one unit with `alarms_replace`, only the filled slots. The exact 18-byte encoding
    /// is pinned in `radio::alarm`'s own tests; here the unit's shape and its slot.
    #[test]
    fn a_drafted_critical_band_also_pushes_an_on_device_alarm_slot() {
        let rows = vec![
            row(
                "threshold.temperature.critical_low",
                PENDING_KIND_THRESHOLD,
                "2",
            ),
            row(
                "threshold.temperature.critical_high",
                PENDING_KIND_THRESHOLD,
                "8",
            ),
        ];
        let units = plan_send(
            &rows,
            "aa11",
            &BTreeMap::new(),
            &["temperature".to_string(), "humidity".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        let alarm = units
            .iter()
            .find(|u| u.label == NODE_CONFIG_UNIT)
            .expect("alarm unit");
        assert_eq!(alarm.command, SEND_COMMAND);
        assert_eq!(alarm.params["save"], json!(true));
        assert_eq!(alarm.params["replace_alarms"], json!(true));
        assert_eq!(
            alarm.params["config"].as_object().map(|c| c.len()),
            Some(1),
            "only the filled slot"
        );
        let owned: Vec<&str> = alarm
            .param_keys
            .iter()
            .map(|r| r.param_key.as_str())
            .collect();
        assert_eq!(
            owned,
            [
                "threshold.temperature.critical_low",
                "threshold.temperature.critical_high"
            ],
            "the Node unit owns the critical rows, so the banner shows them until the ack (N5)"
        );
        let hex = alarm.params["config"]["alarms.alarm_0"]
            .as_str()
            .expect("the slot value is a hex string");
        assert_eq!(hex.len(), 36, "an 18-byte rule");
        assert!(hex.starts_with("03"), "present + enabled");
    }

    /// The table is the portal's whole truth: a field the draft does not touch keeps its
    /// stored band in its own slot, and slots follow the Node's layout, not the field name.
    #[test]
    fn the_alarm_table_carries_every_stored_band_in_layout_order() {
        let mut current = BTreeMap::new();
        current.insert(
            "w1_1_temperature".to_string(),
            ThresholdEntryCtx {
                critical_low: Some(2.0),
                warning_low: None,
                warning_high: None,
                critical_high: Some(8.0),
                enabled: true,
                hysteresis: None,
            },
        );
        let rows = vec![
            row(
                "threshold.temperature.critical_low",
                PENDING_KIND_THRESHOLD,
                "15",
            ),
            row(
                "threshold.temperature.critical_high",
                PENDING_KIND_THRESHOLD,
                "25",
            ),
        ];
        let layout: Vec<String> = ["temperature", "humidity", "w1_1_temperature"]
            .map(String::from)
            .to_vec();
        let units = plan_send(
            &rows,
            "aa11",
            &current,
            &layout,
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        let table = units
            .iter()
            .find(|u| u.label == NODE_CONFIG_UNIT)
            .expect("the table");
        let config = table.params["config"].as_object().expect("config");
        let mut keys: Vec<&String> = config.keys().collect();
        keys.sort();
        // MR !129 review: humidity has no band and takes no slot.
        assert_eq!(keys, ["alarms.alarm_0", "alarms.alarm_1"]);
        assert_eq!(
            table.params["alarm_layout"],
            json!(["temperature", "w1_1_temperature"])
        );
        assert_eq!(table.params["alarm_types"], json!([2, 0, 0, 0]));
        assert!(refusal(&units, None).is_none());
    }

    /// MR !129 review: a band on a 1-Wire field whose slot has no configured type refuses the
    /// send, naming the field, instead of the replace dropping it from the Node; a Node on
    /// older firmware is refused the table.
    #[test]
    fn a_table_that_would_drop_a_band_is_refused() {
        use crate::render::ThresholdEntryCtx;
        let mut current = BTreeMap::new();
        current.insert(
            "w1_1_temperature".to_string(),
            ThresholdEntryCtx {
                critical_low: Some(2.0),
                warning_low: None,
                warning_high: None,
                critical_high: Some(8.0),
                enabled: true,
                hysteresis: None,
            },
        );
        let rows = vec![
            row(
                "threshold.temperature.critical_low",
                PENDING_KIND_THRESHOLD,
                "15",
            ),
            row(
                "threshold.temperature.critical_high",
                PENDING_KIND_THRESHOLD,
                "25",
            ),
        ];
        let layout: Vec<String> = ["temperature", "w1_1_temperature"]
            .map(String::from)
            .to_vec();
        let units = plan_send(
            &rows,
            "aa11",
            &current,
            &layout,
            &crate::radio::alarm::SlotTypes::default(),
        );
        let refused = refusal(&units, None).expect("refused");
        assert_eq!(
            refused,
            SendRefusal::AlarmTable(crate::radio::alarm::TableRefusal::UnknownType(vec![
                "w1_1_temperature".into()
            ]))
        );
        assert!(refused
            .message(|k| crate::i18n::lookup("en", k))
            .contains("w1_1_temperature"));

        // Typed, but the Node runs firmware older than 2.0.0: nothing is sent.
        let units = plan_send(
            &rows,
            "aa11",
            &current,
            &layout,
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        assert!(refusal(&units, Some(&(2, 0, 0))).is_none());
        assert!(refusal(&units, None).is_none(), "unknown version: allowed");
        for old in [(1, 5, 0), (1, 4, 0)] {
            // Every unit is refused on pre-2.0.0 firmware, not only the alarm table.
            match refusal(&units, Some(&old)) {
                Some(SendRefusal::FirmwareTooOld { keys, version }) => {
                    assert!(keys.iter().any(|k| k == "alarms"), "{keys:?}");
                    assert_eq!(version, format!("{}.{}.{}", old.0, old.1, old.2));
                }
                other => panic!("refused as too old, got {other:?}"),
            }
        }
    }

    /// A warning-only edit changes nothing the Node evaluates — no device leg.
    #[test]
    fn a_warning_only_edit_pushes_no_on_device_alarm() {
        let rows = vec![row(
            "threshold.temperature.warning_high",
            PENDING_KIND_THRESHOLD,
            "6",
        )];
        let units = plan_send(
            &rows,
            "aa11",
            &BTreeMap::new(),
            &["temperature".to_string(), "humidity".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        assert!(
            units.iter().all(|u| u.label != NODE_CONFIG_UNIT),
            "no critical bound touched -> no on-device alarm"
        );
        // A warning row only a draft from before 2026-10-13 holds is still sent, and its
        // write sets the warning it carries rather than keeping the stored one.
        let hub = units
            .iter()
            .find(|u| u.command == THRESHOLD_COMMAND)
            .expect("hub unit");
        assert_eq!(hub.params["warning_high"], json!(6.0));
        assert!(hub.threshold.as_ref().expect("write").warnings_drafted);
    }

    /// OQ3: `enabled` is drafted like a bound. A field touched only through its
    /// enabled toggle still sends — the drafted flag overrides the current
    /// state, rides the unit's `param_keys` (so the walk clears it), and flips
    /// the on-device alarm slot's enabled bit for a pushable field.
    #[test]
    fn a_drafted_enabled_toggle_rides_the_send_and_flips_the_device_alarm() {
        let mut current = BTreeMap::new();
        current.insert(
            "temperature".to_string(),
            ThresholdEntryCtx {
                critical_low: Some(2.0),
                warning_low: None,
                warning_high: None,
                critical_high: Some(8.0),
                enabled: true,
                hysteresis: None,
            },
        );
        let key = crate::pending::threshold_enabled_param_key("temperature");
        let rows = vec![row(&key, PENDING_KIND_THRESHOLD, "false")];
        let units = plan_send(
            &rows,
            "aa11",
            &current,
            &["temperature".to_string(), "humidity".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );

        let t = units
            .iter()
            .find(|u| u.label == "threshold.temperature")
            .expect("threshold unit");
        assert_eq!(
            t.params["enabled"],
            json!(false),
            "drafted enabled overrides the current state"
        );
        assert!(
            t.param_keys.is_empty(),
            "the Hub-side unit owns no row here"
        );

        let alarm = units
            .iter()
            .find(|u| u.label == NODE_CONFIG_UNIT)
            .expect("alarm unit");
        assert_eq!(
            alarm.param_keys.len(),
            1,
            "the enabled row is cleared on send"
        );
        assert_eq!(alarm.param_keys[0].param_key, key);
        let hex = alarm.params["config"]["alarms.alarm_0"]
            .as_str()
            .expect("slot hex");
        assert_eq!(&hex[0..2], "01", "present but disabled");
    }

    /// N13 and PN-3: the answer budget follows the report interval, outlasts the slower
    /// carrier's delivery (P2P's second uplink, plus the answer's own seconds) and never drops
    /// below the floor. An unknown interval is the firmware default's.
    #[test]
    fn the_answer_budget_follows_the_report_interval() {
        assert_eq!(downlink_answer_budget(Some(900)), Duration::from_secs(1860));
        assert_eq!(
            downlink_answer_budget(None),
            downlink_answer_budget(Some(FIRMWARE_DEFAULT_INTERVAL_REPORT_S))
        );
        assert_eq!(
            downlink_answer_budget(Some(0)),
            downlink_answer_budget(None)
        );
        assert_eq!(
            downlink_answer_budget(Some(60)),
            DEVICE_CONFIRM_BUDGET,
            "floor"
        );
        for report in [30, 60, 300, 900, 3600] {
            // LoRaWAN: the next uplink; P2P: the second, and the answer a few seconds after.
            let worst = Duration::from_secs(2 * report + 5);
            assert!(downlink_answer_budget(Some(report)) > worst, "{report} s");
        }
        let mut config = serde_json::Map::new();
        config.insert("application.interval_report".into(), json!(600));
        assert_eq!(interval_report_of(Some(&config)), Some(600));
        assert_eq!(interval_report_of(None), None);
    }

    /// NT-3: switching on a warning-only threshold is a Hub-side write; the Node's table does
    /// not go, so there is no downlink and no restart for a slot that stays empty.
    #[test]
    fn a_warning_only_threshold_switched_on_sends_nothing_to_the_node() {
        let rows = vec![
            row(
                "threshold.temperature.warning_high",
                PENDING_KIND_THRESHOLD,
                "25",
            ),
            row(
                "threshold.temperature.enabled",
                PENDING_KIND_THRESHOLD,
                "true",
            ),
        ];
        let units = plan_send(
            &rows,
            "aa11",
            &BTreeMap::new(),
            &["temperature".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        assert!(
            units.iter().all(|u| u.command != SEND_COMMAND),
            "{:?}",
            units.iter().map(|u| &u.label).collect::<Vec<_>>()
        );
        assert_eq!(
            downlink_count(&units, Some(5), portal_domain::Transport::Lorawan),
            None
        );
        let hub = units
            .iter()
            .find(|u| u.command == THRESHOLD_COMMAND)
            .expect("the Hub-side threshold");
        assert_eq!(hub.param_keys.len(), 2, "both rows cleared by the Hub unit");

        // With a critical bound the table goes as before.
        let rows = vec![
            row(
                "threshold.temperature.critical_high",
                PENDING_KIND_THRESHOLD,
                "35",
            ),
            row(
                "threshold.temperature.enabled",
                PENDING_KIND_THRESHOLD,
                "true",
            ),
        ];
        let units = plan_send(
            &rows,
            "aa11",
            &BTreeMap::new(),
            &["temperature".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        assert_eq!(units.last().map(|u| u.command), Some(SEND_COMMAND));
    }

    /// PF-3 (decision Hynek 2026-09-27, reversing N15): the Hub-side thresholds first, the
    /// Node's set-config last. The Hub and the portal take a bound at once, and it is
    /// evaluated from telemetry whatever the Node does; with the Node first, a late ack left
    /// the Node on a band the portal never wrote (PF-3 on the P2P bench).
    #[test]
    fn the_hub_thresholds_are_planned_before_the_node_unit() {
        let rows = vec![
            row("application.interval_report", PENDING_KIND_CONFIG, "600"),
            row(
                "threshold.temperature.critical_high",
                PENDING_KIND_THRESHOLD,
                "8",
            ),
        ];
        let units = plan_send(
            &rows,
            "aa11",
            &BTreeMap::new(),
            &["temperature".to_string(), "humidity".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        let labels: Vec<&str> = units.iter().map(|u| u.label.as_str()).collect();
        assert_eq!(labels, ["threshold.temperature", NODE_CONFIG_UNIT]);
        assert_eq!(units[0].command, THRESHOLD_COMMAND);
        assert_eq!(units[1].command, SEND_COMMAND);
        assert_eq!(
            units[1].params["config"]["application.interval_report"],
            json!(600)
        );
        assert_eq!(units[1].params["save"], json!(true));
    }

    /// nodes_phase1.cs.md WP5: the config keys and the alarm table are one `set-config`, so the
    /// Node saves and restarts once; the unit clears every config row it carries.
    #[test]
    fn config_keys_and_the_alarm_table_are_one_set_config() {
        let rows = vec![
            row("application.interval_report", PENDING_KIND_CONFIG, "600"),
            row("application.interval_sample", PENDING_KIND_CONFIG, "0"),
            row(
                "threshold.temperature.critical_low",
                PENDING_KIND_THRESHOLD,
                "2",
            ),
            row(
                "threshold.temperature.critical_high",
                PENDING_KIND_THRESHOLD,
                "8",
            ),
        ];
        let units = plan_send(
            &rows,
            "aa11",
            &BTreeMap::new(),
            &["temperature".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        let device: Vec<&SendUnit> = units.iter().filter(|u| u.command == SEND_COMMAND).collect();
        assert_eq!(device.len(), 1, "one downlink unit");
        let unit = device[0];
        let config = unit.params["config"].as_object().expect("config");
        let mut keys: Vec<&String> = config.keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "alarms.alarm_0",
                "application.interval_report",
                "application.interval_sample"
            ]
        );
        assert_eq!(unit.params["replace_alarms"], json!(true));
        assert_eq!(unit.params["alarm_layout"], json!(["temperature"]));
        let cleared: Vec<&str> = unit
            .param_keys
            .iter()
            .map(|r| r.param_key.as_str())
            .collect();
        assert_eq!(
            cleared,
            [
                "application.interval_report",
                "application.interval_sample",
                "threshold.temperature.critical_low",
                "threshold.temperature.critical_high"
            ]
        );

        // Without a critical edit the unit is the config keys alone, no replace.
        let units = plan_send(
            &rows[..1],
            "aa11",
            &BTreeMap::new(),
            &["temperature".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        assert_eq!(units.len(), 1);
        assert!(units[0].params.get("replace_alarms").is_none());
        assert!(units[0].params.get("alarm_layout").is_none());
    }

    /// The banner's frame count is control-radio's own plan at the Node's data rate.
    #[test]
    fn the_downlink_count_follows_the_data_rate() {
        let rows = vec![
            row("application.interval_report", PENDING_KIND_CONFIG, "600"),
            row("application.interval_sample", PENDING_KIND_CONFIG, "60"),
        ];
        let layout: Vec<String> = ["temperature", "humidity", "w1_1_temperature"]
            .map(String::from)
            .to_vec();
        let band = |lo, hi| ThresholdEntryCtx {
            critical_low: Some(lo),
            warning_low: None,
            warning_high: None,
            critical_high: Some(hi),
            enabled: true,
            hysteresis: None,
        };
        let mut current = BTreeMap::new();
        current.insert("humidity".to_string(), band(10.0, 90.0));
        current.insert("w1_1_temperature".to_string(), band(2.0, 8.0));
        let mut with_table = rows.clone();
        with_table.push(row(
            "threshold.temperature.critical_low",
            PENDING_KIND_THRESHOLD,
            "0",
        ));
        with_table.push(row(
            "threshold.temperature.critical_high",
            PENDING_KIND_THRESHOLD,
            "30",
        ));

        let intervals = plan_send(
            &rows,
            "aa11",
            &current,
            &layout,
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        let table = plan_send(
            &with_table,
            "aa11",
            &current,
            &layout,
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        // One scenario, both carriers: a draft that fits one frame is one on either.
        for carrier in portal_domain::Transport::ALL {
            assert_eq!(
                downlink_count(&intervals, Some(0), carrier),
                Some(1),
                "{carrier:?}"
            );
        }
        assert!(
            downlink_count(&table, Some(0), portal_domain::Transport::Lorawan).expect("plan") > 1,
            "three 18-byte rules and two intervals exceed DR0's 51 B"
        );
        assert_eq!(
            downlink_count(&table, Some(5), portal_domain::Transport::Lorawan),
            Some(1),
            "222 B at DR5"
        );
        // P2P packs at its fixed 72 B body (FW v2.0.0 P2P_MAX_BODY) whatever its last
        // uplink said: the same count at DR0 and DR5, never more than LoRaWAN DR0's.
        let p2p = downlink_count(&table, Some(0), portal_domain::Transport::P2p).expect("plan");
        assert_eq!(
            downlink_count(&table, Some(5), portal_domain::Transport::P2p),
            Some(p2p),
            "P2P ignores the data rate"
        );
        assert!(p2p <= downlink_count(&table, Some(0), portal_domain::Transport::Lorawan).unwrap());

        let warning_only = vec![row(
            "threshold.temperature.warning_high",
            PENDING_KIND_THRESHOLD,
            "6",
        )];
        let units = plan_send(
            &warning_only,
            "aa11",
            &current,
            &layout,
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        for carrier in portal_domain::Transport::ALL {
            assert_eq!(
                downlink_count(&units, Some(0), carrier),
                None,
                "{carrier:?}: nothing goes to the Node"
            );
        }
    }

    /// A key nothing can parse is skipped rather than sent as garbage, and
    /// it does not take the rest of the draft with it — the walk still has
    /// something to do.
    #[test]
    fn an_unparseable_threshold_key_is_skipped_not_sent() {
        let rows = vec![
            row("threshold.nonsense", PENDING_KIND_THRESHOLD, "1"),
            row("application.interval_sample", PENDING_KIND_CONFIG, "60"),
        ];
        let units = plan_send(
            &rows,
            "aa11",
            &BTreeMap::new(),
            &["temperature".to_string(), "humidity".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].label, NODE_CONFIG_UNIT);
        assert_eq!(
            units[0].params["config"]["application.interval_sample"],
            json!(60)
        );
    }

    // ---- the walk itself -------------------------------------------------

    use crate::state::{ChangeEvent, StateStore};
    use portal_storage::NewPendingConfig;

    const HOST: &str = "h1";
    const NODE: &str = "aa11";

    /// #166: a size discard is sent again once; a second one in a row stays undelivered, and
    /// any other ending arms the retry again.
    #[test]
    fn a_size_discard_is_retried_once() {
        let (_dir, pending) = service();
        let discard = SendStop::NodeUndelivered {
            param_key: "node.config".into(),
            message: "not sent: discarded by the network server: 70 B exceeds 51 B at the \
                      current data rate"
                .into(),
            config_keys: Vec::new(),
        };
        let silent = SendStop::NodeUndelivered {
            param_key: "node.config".into(),
            message: "device did not confirm within 900 s".into(),
            config_keys: Vec::new(),
        };
        assert!(pending.retry_after_size_discard("hub", "n1", Some(&discard)));
        assert!(!pending.retry_after_size_discard("hub", "n1", Some(&discard)));
        assert!(!pending.retry_after_size_discard("hub", "n1", Some(&silent)));
        assert!(pending.retry_after_size_discard("hub", "n1", Some(&discard)));
        assert!(!pending.retry_after_size_discard("hub", "n1", None));
        assert!(pending.retry_after_size_discard("hub", "n1", Some(&discard)));
    }

    fn service() -> (tempfile::TempDir, PendingService) {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = PendingService::open(dir.path().to_str().expect("utf8"), None)
            .expect("open pending service");
        (dir, service)
    }

    async fn draft(pending: &PendingService, key: &str, kind: &str, value: &str) {
        pending
            .upsert(NewPendingConfig {
                hostname: HOST.into(),
                node_key: NODE.into(),
                param_key: key.into(),
                kind: kind.into(),
                desired_value: Some(value.into()),
                author: "anna".into(),
            })
            .await
            .expect("draft");
    }

    fn ok_result() -> ChangeEvent {
        ChangeEvent::NodeCommandResult {
            hostname: HOST.into(),
            node_key: NODE.into(),
            payload: json!({ "command": "set_sticker_config", "result": "ok" }),
        }
    }

    /// The second clock exists, and it is what deletes a row.
    ///
    /// The relay answering "the Hub accepted" is not the device answering
    /// "I applied it" — a Node is asleep between uplinks and can take
    /// minutes. Nothing is cleared out of the draft until the device says
    /// so, which is the property this asserts by *withholding* the answer
    /// until after the relay has returned.
    #[tokio::test]
    async fn a_config_unit_waits_for_the_device_after_the_hub_accepts() {
        let store = StateStore::new();
        let mut rx = store.subscribe();

        // Nothing has answered yet: the wait is still running.
        let waiting = tokio::spawn(async move {
            await_device_confirmation(&mut rx, HOST, NODE, SEND_COMMAND, DEVICE_CONFIRM_BUDGET)
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            !waiting.is_finished(),
            "the walk does not delete on the Hub's word alone"
        );

        store.emit(ok_result());
        assert_eq!(
            waiting.await.expect("join"),
            Confirmation::Confirmed,
            "the device's answer confirms it"
        );
    }

    /// Only THIS Node's answer to THIS command counts. A different Node's
    /// result, or this Node's answer to some other command, is somebody
    /// else's conversation.
    #[tokio::test]
    async fn a_confirmation_for_another_node_or_command_is_not_ours() {
        let store = StateStore::new();
        let mut rx = store.subscribe();
        let waiting = tokio::spawn(async move {
            await_device_confirmation(&mut rx, HOST, NODE, SEND_COMMAND, DEVICE_CONFIRM_BUDGET)
                .await
        });

        store.emit(ChangeEvent::NodeCommandResult {
            hostname: HOST.into(),
            node_key: "bb22".into(),
            payload: json!({ "command": "set_sticker_config", "result": "ok" }),
        });
        store.emit(ChangeEvent::NodeCommandResult {
            hostname: HOST.into(),
            node_key: NODE.into(),
            payload: json!({ "command": "sticker_reboot", "result": "ok" }),
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!waiting.is_finished());

        store.emit(ok_result());
        assert_eq!(waiting.await.expect("join"), Confirmation::Confirmed);
    }

    /// A device that answers with a failure is not a confirmation. The row
    /// stays in the draft with its reason rather than being deleted on the
    /// strength of having been answered at all.
    #[tokio::test]
    async fn a_device_that_refuses_does_not_confirm() {
        let store = StateStore::new();
        let mut rx = store.subscribe();
        let waiting = tokio::spawn(async move {
            await_device_confirmation(&mut rx, HOST, NODE, SEND_COMMAND, DEVICE_CONFIRM_BUDGET)
                .await
        });
        store.emit(ChangeEvent::NodeCommandResult {
            hostname: HOST.into(),
            node_key: NODE.into(),
            payload: json!({ "command": "set_sticker_config", "result": "unsupported_key" }),
        });
        assert_ne!(waiting.await.expect("join"), Confirmation::Confirmed);
    }

    /// A threshold unit has ONE clock, and that is not a shortcut.
    ///
    /// `set_lorawan_field_threshold` is a fleet-config write the Hub
    /// evaluates itself and never sends to the Node, so no
    /// `NodeCommandResult` will ever carry that verb — waiting for one
    /// would time out every single time. The relay's own answer is the
    /// whole answer.
    ///
    /// `start_paused` because the assertion is that this *returns* rather
    /// than waiting: if the early return ever regresses, a real clock makes
    /// the test hang for the whole 180 s budget before failing.
    #[tokio::test(start_paused = true)]
    async fn a_threshold_unit_has_no_device_leg_to_wait_for() {
        let store = StateStore::new();
        let mut rx = store.subscribe();
        assert_eq!(
            await_device_confirmation(
                &mut rx,
                HOST,
                NODE,
                THRESHOLD_COMMAND,
                DEVICE_CONFIRM_BUDGET
            )
            .await,
            Confirmation::Confirmed,
            "a threshold write is confirmed by the Hub, not by the Node"
        );
    }

    /// The 180 s budget, on a paused clock.
    ///
    /// Real time would make this test three minutes long, so the clock is
    /// virtual — `start_paused` advances it as soon as the runtime has
    /// nothing else to do, which is exactly the state a walk waiting for a
    /// A lap is not silence. The walk holds this receiver for three
    /// minutes; if it drops messages while waiting, the device's answer may
    /// have been among them — so the row must not claim the device stayed
    /// quiet. (It is on the narrow command-result stream precisely so this
    /// takes a deliberate flood rather than a busy fleet.)
    #[tokio::test(start_paused = true)]
    async fn a_lapped_channel_is_reported_as_lost_not_as_silence() {
        let store = StateStore::new();
        let mut rx = store.subscribe_command_results();

        // Overrun the channel before the wait starts reading, so its first
        // `recv()` is the `Lagged`. None of these correlate: they are a
        // different Node, so even undropped they could not confirm.
        for i in 0..512 {
            store.emit(crate::state::ChangeEvent::NodeCommandResult {
                hostname: HOST.to_string(),
                node_key: format!("other{i}"),
                payload: serde_json::json!({ "command": SEND_COMMAND, "result": "ok" }),
            });
        }

        assert_eq!(
            await_device_confirmation(&mut rx, HOST, NODE, SEND_COMMAND, DEVICE_CONFIRM_BUDGET)
                .await,
            Confirmation::Lost,
            "a wait that lapped cannot honestly report silence"
        );
    }

    /// silent device is in. The assertion is that the wait ends at all and
    /// ends as a failure.
    #[tokio::test(start_paused = true)]
    async fn a_send_that_times_out_keeps_the_draft_with_its_error() {
        let (_dir, pending) = service();
        draft(
            &pending,
            "application.interval_report",
            PENDING_KIND_CONFIG,
            "600",
        )
        .await;

        let store = StateStore::new();
        let started = tokio::time::Instant::now();
        let mut rx = store.subscribe();
        let confirmed =
            await_device_confirmation(&mut rx, HOST, NODE, SEND_COMMAND, DEVICE_CONFIRM_BUDGET)
                .await;
        assert_eq!(
            confirmed,
            Confirmation::Silent,
            "nothing answered, and the channel never lapped, so this is silence"
        );
        assert!(
            started.elapsed() >= DEVICE_CONFIRM_BUDGET,
            "the wait ran its whole budget"
        );

        // What the walk does with that answer: the row keeps its value and
        // gains a reason. It is NOT deleted — the operator has to be able
        // to see what did not land and press send again.
        let unit = SendUnit {
            param_keys: vec![SendRow {
                param_key: "application.interval_report".into(),
                desired_value: Some("600".into()),
            }],
            label: "application.interval_report".into(),
            command: SEND_COMMAND,
            params: json!({}),
            threshold: None,
            node_previous: Vec::new(),
            node_bands: Vec::new(),
            unreadable: None,
            refusal: None,
        };
        mark_failed(
            &pending,
            HOST,
            NODE,
            &unit,
            PENDING_STATE_UNDELIVERED,
            "device did not confirm within 180 s",
        )
        .await;

        let rows = pending.list(HOST, NODE).await.expect("draft");
        assert_eq!(rows.len(), 1, "the draft survives its failure");
        assert_eq!(
            rows[0].state, PENDING_STATE_UNDELIVERED,
            "the Node's part of a send that did not land (PF-3)"
        );
        assert_eq!(
            rows[0].last_error.as_deref(),
            Some("device did not confirm within 180 s")
        );
        assert_eq!(
            rows[0].desired_value.as_deref(),
            Some("600"),
            "the value the operator typed is still there to resend"
        );
    }

    /// `PendingService::open` is where an interrupted send is cleaned up,
    /// because a restart is the only thing that can produce one: `sending`
    /// means "a task in this process is waiting", and that task is gone.
    #[tokio::test]
    async fn a_restart_marks_an_interrupted_send_as_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8");

        {
            let pending = PendingService::open(data_dir, None).expect("open");
            pending
                .upsert(NewPendingConfig {
                    hostname: HOST.into(),
                    node_key: NODE.into(),
                    param_key: "application.interval_report".into(),
                    kind: PENDING_KIND_CONFIG.into(),
                    desired_value: Some("600".into()),
                    author: "anna".into(),
                })
                .await
                .expect("draft");
            pending
                .set_state(
                    HOST,
                    NODE,
                    "application.interval_report",
                    None,
                    PENDING_STATE_SENDING,
                    None,
                    true,
                )
                .await
                .expect("to sending");
        }

        // The process dies here. The next one opens the same file.
        let restarted = PendingService::open(data_dir, None).expect("reopen");
        let rows = restarted.list(HOST, NODE).await.expect("draft");
        assert_eq!(
            rows[0].state, PENDING_STATE_UNDELIVERED,
            "a config key waits for the Node, which may never have got it (PF-3)"
        );
        assert_eq!(
            rows[0].last_error.as_deref(),
            Some("portal restarted during send"),
            "the banner says what happened instead of spinning forever"
        );
    }

    /// P-7: a join after the frames were queued means ChirpStack flushed them (the device
    /// profiles flush the queue on activate), so the walk resends instead of waiting out the
    /// budget. A join within the margin may be the one the frames were queued behind.
    #[tokio::test]
    async fn a_join_after_the_frames_were_queued_reports_a_rejoin() {
        let store = StateStore::new();
        let mut rx = store.subscribe_command_results();
        let queued_at = 1_000;
        let waiting = tokio::spawn(async move {
            await_radio_acks(
                &mut rx,
                HOST,
                NODE,
                &mut vec![7],
                DEVICE_CONFIRM_BUDGET,
                Some(queued_at),
            )
            .await
        });
        let join = |at| ChangeEvent::NodeJoined {
            hostname: HOST.into(),
            node_key: NODE.into(),
            at,
        };
        store.emit(join(queued_at + 1));
        store.emit(ChangeEvent::NodeJoined {
            hostname: HOST.into(),
            node_key: "bb22".into(),
            at: queued_at + 60,
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            !waiting.is_finished(),
            "a join inside the margin, or another Node's, is not ours"
        );
        store.emit(join(queued_at + 60));
        assert_eq!(waiting.await.expect("join"), Confirmation::Rejoined);
    }

    /// §10.5 (amended 2026-10-07): the Hub's `undelivered` note for one of the unit's seqs
    /// ends the wait at once as `Undelivered`, not as a refusal; one for another seq does not.
    #[tokio::test]
    async fn an_undelivered_seq_ends_the_wait_at_once() {
        let store = StateStore::new();
        let mut rx = store.subscribe_command_results();
        let waiting = tokio::spawn(async move {
            await_radio_acks(
                &mut rx,
                HOST,
                NODE,
                &mut vec![7, 8],
                DEVICE_CONFIRM_BUDGET,
                None,
            )
            .await
        });
        let undelivered = |seq| ChangeEvent::NodeCommandResult {
            hostname: HOST.into(),
            node_key: NODE.into(),
            payload: json!({
                "transport": "control.radio", "command": "undelivered",
                "result": "undelivered", "seq": seq, "detail": "5 attempts, last NotDelivered",
            }),
        };
        store.emit(undelivered(9));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!waiting.is_finished(), "not one of the unit's seqs");
        store.emit(undelivered(8));
        assert_eq!(
            waiting.await.expect("undelivered"),
            Confirmation::Undelivered(None)
        );
    }

    /// §10.5 (amended 2026-10-07): a unit the walk gave up on keeps waiting for the Node's own
    /// answer through the Hub's `undelivered` note — the Node may have applied it and only its
    /// ACK was lost — and the Node's late ok still finishes it.
    #[tokio::test]
    async fn an_undelivered_note_is_no_late_answer() {
        let (_dir, pending) = service();
        pending.remember_late(LateUnit {
            hostname: HOST.into(),
            node_key: NODE.into(),
            label: NODE_CONFIG_UNIT.into(),
            seqs: vec![7],
            unanswered: vec![7],
            rows: Vec::new(),
            alarm_layout: None,
            node_bands: Vec::new(),
            given_up_at: std::time::Instant::now(),
        });
        assert!(
            !pending
                .apply_late_answer(HOST, NODE, &radio_answer(7, "undelivered"))
                .await
        );
        assert!(
            pending
                .take_late_answer(HOST, NODE, 7, true, std::time::Instant::now())
                .is_some(),
            "the unit still waited for the Node, whose late ok finishes it"
        );
    }

    /// P-6: a row that failed while the Node applied it anyway goes away once the Node reports
    /// the value, with an audit row; a row whose value the Node does not report stays.
    #[tokio::test]
    async fn a_failed_draft_the_node_reports_as_applied_is_reconciled() {
        let (_dir, pending) = service();
        draft(
            &pending,
            "application.interval_report",
            PENDING_KIND_CONFIG,
            "60",
        )
        .await;
        draft(
            &pending,
            "application.interval_sample",
            PENDING_KIND_CONFIG,
            "30",
        )
        .await;
        for key in ["application.interval_report", "application.interval_sample"] {
            pending
                .set_state(
                    HOST,
                    NODE,
                    key,
                    None,
                    PENDING_STATE_ERROR,
                    Some("no answer".into()),
                    true,
                )
                .await
                .expect("to error");
        }
        let config = json!({
            "application.interval_report": 60.0,
            "application.interval_sample": 60,
        });
        let applied = pending
            .reconcile_late_applies(HOST, NODE, config.as_object().expect("object"))
            .await;
        assert_eq!(applied, 1);
        let rows = pending.list(HOST, NODE).await.expect("draft");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].param_key, "application.interval_sample");
        let (audit, _) = pending
            .db
            .run(|s| {
                s.list_audit(&portal_storage::AuditFilter {
                    event_type: Some("node_config_applied_late".into()),
                    ..Default::default()
                })
            })
            .await
            .expect("audit");
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].target.as_deref(), Some(NODE));
    }

    /// NT-4: the store's config event names the Hub in lowercase while the draft rows carry
    /// its display spelling; the reconcile still finds and removes the applied row.
    #[tokio::test]
    async fn a_late_apply_matches_a_hub_whose_display_name_is_upper_case() {
        let (_dir, pending) = service();
        let display = "FIBER-4E970F4A";
        pending
            .upsert(NewPendingConfig {
                hostname: display.to_string(),
                node_key: NODE.to_string(),
                kind: PENDING_KIND_CONFIG.to_string(),
                param_key: "application.interval_sample".to_string(),
                desired_value: Some("30".to_string()),
                author: "admin".to_string(),
            })
            .await
            .expect("draft");
        pending
            .set_state(
                display,
                NODE,
                "application.interval_sample",
                None,
                PENDING_STATE_ERROR,
                Some("no answer".into()),
                true,
            )
            .await
            .expect("to error");

        let store = Arc::new(crate::state::StateStore::new());
        pending.spawn_late_apply(Arc::clone(&store));
        store.apply(crate::state::events::IngestEvent::Status {
            hostname: display.into(),
            status: fiber_protocol::DeviceStatus::Online,
        });
        let mut config = serde_json::Map::new();
        config.insert("application.interval_sample".into(), json!(30));
        store.apply(crate::state::events::IngestEvent::StickerConfig {
            hostname: "fiber-4e970f4a".into(),
            node_key: NODE.into(),
            payload: fiber_protocol::NodeConfigMsg {
                dev_eui: None,
                config,
                page_index: None,
                page_count: None,
                synced_at: None,
                last_ack: None,
                read_status: None,
                missing: None,
                error: None,
                chunks_done: None,
                chunks_total: None,
            },
        });
        for _ in 0..50 {
            if pending.list(display, NODE).await.expect("draft").is_empty() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("the applied row was not reconciled");
    }

    /// A row still waiting for its walk is not the reconcile's business, even when the Node
    /// already reports the value: the walk confirms it and removes it itself.
    #[tokio::test]
    async fn a_pending_draft_is_left_to_its_walk() {
        let (_dir, pending) = service();
        draft(
            &pending,
            "application.interval_report",
            PENDING_KIND_CONFIG,
            "60",
        )
        .await;
        let config = json!({ "application.interval_report": 60 });
        let applied = pending
            .reconcile_late_applies(HOST, NODE, config.as_object().expect("object"))
            .await;
        assert_eq!(applied, 0);
        assert_eq!(pending.list(HOST, NODE).await.expect("draft").len(), 1);
    }

    /// P-6: the frames a walk queued are remembered once, for the discard to withdraw.
    #[test]
    fn the_queued_seqs_are_taken_once() {
        let (_dir, pending) = service();
        pending.remember_queued(HOST, NODE, &[4, 5]);
        pending.remember_queued(HOST, NODE, &[6]);
        assert_eq!(pending.take_queued(HOST, NODE), vec![4, 5, 6]);
        assert!(pending.take_queued(HOST, NODE).is_empty());
    }

    /// The nudge is what makes the banner move while a walk runs, and what
    /// makes a second session see a first session's edit. No subscribers is
    /// normal, so it must not be an error.
    #[tokio::test]
    async fn a_nudge_names_the_node_and_survives_having_no_listeners() {
        let (_dir, pending) = service();
        pending.nudge(HOST, NODE); // nobody listening yet

        let mut rx = pending.subscribe_nudges();
        pending.nudge(HOST, NODE);
        let nudge = rx.recv().await.expect("nudge");
        assert_eq!(nudge.hostname, HOST);
        assert_eq!(nudge.node_key, NODE);
    }

    // ---- run_send_walk, with the relay injected (§21 review, item 6) ------

    /// A relay that answers from a script and records what it was asked to
    /// send. Everything the walk's state machine does hangs off these two
    /// facts, and a real `CommandRelay` cannot supply either without a
    /// broker (both verbs are signed `config_request`s, so it would sit out
    /// a 20 s timeout per unit and always answer the same way).
    #[derive(Default)]
    struct ScriptedRelay {
        outcomes: StdMutex<std::collections::VecDeque<&'static str>>,
        sent: StdMutex<Vec<(String, serde_json::Value)>>,
        /// Emitted on the store as soon as a `set_sticker_config` is sent,
        /// so the device leg resolves without the test racing it.
        confirm_with: Option<Arc<StateStore>>,
    }

    impl ScriptedRelay {
        fn new(statuses: &[&'static str], confirm_with: Option<Arc<StateStore>>) -> Self {
            Self {
                outcomes: StdMutex::new(statuses.iter().copied().collect()),
                sent: StdMutex::new(Vec::new()),
                confirm_with,
            }
        }
        fn commands(&self) -> Vec<String> {
            self.sent
                .lock()
                .expect("lock")
                .iter()
                .map(|(c, _)| c.clone())
                .collect()
        }
    }

    impl UnitSender for ScriptedRelay {
        fn send_unit<'a>(
            &'a self,
            _actor: &'a str,
            _key: &'a crate::keys::UserSigningKey,
            _hostname: &'a str,
            command: &'a str,
            params: serde_json::Value,
        ) -> BoxFuture<'a, crate::relay::CommandOutcome> {
            self.sent
                .lock()
                .expect("lock")
                .push((command.to_string(), params));
            let status = self
                .outcomes
                .lock()
                .expect("lock")
                .pop_front()
                .unwrap_or("success");
            if command == SEND_COMMAND {
                if let Some(store) = &self.confirm_with {
                    store.emit(ok_result());
                }
            }
            Box::pin(async move {
                crate::relay::CommandOutcome {
                    request_id: None,
                    command_type: command.to_string(),
                    status,
                    message: None,
                    error_code: None,
                    result: None,
                }
            })
        }
    }

    fn test_signing_key() -> crate::keys::UserSigningKey {
        crate::keys::UserSigningKey {
            signer_id: "user-1-anna".into(),
            seed: zeroize::Zeroizing::new([3u8; 32]),
            certificate: serde_json::json!({
                "permissions": ["set_lorawan_sensor_config", "level:read",
                                "level:operate", "level:manage"],
            }),
            ceiling: fiber_protocol::Level::Manage,
        }
    }

    fn config_unit(param_key: &str, value: &str) -> SendUnit {
        SendUnit {
            param_keys: vec![SendRow {
                param_key: param_key.into(),
                desired_value: Some(value.into()),
            }],
            label: param_key.into(),
            command: SEND_COMMAND,
            params: json!({ "dev_eui": NODE, "config": { param_key: 600 }, "save": true }),
            threshold: None,
            node_previous: Vec::new(),
            node_bands: Vec::new(),
            unreadable: None,
            refusal: None,
        }
    }

    async fn walk(
        pending: &PendingService,
        relay: &ScriptedRelay,
        store: &Arc<StateStore>,
        units: &[SendUnit],
    ) -> Result<usize, SendStop> {
        let key = test_signing_key();
        run_send_walk(
            pending,
            relay,
            &key,
            "anna",
            HOST,
            NODE,
            units,
            store.subscribe_command_results(),
            DEVICE_CONFIRM_BUDGET,
        )
        .await
    }

    /// A sender like the control.radio seam: it accepts, names the seq it queued the unit's
    /// downlink under, and (optionally) makes the Node answer that seq on the store.
    struct SeqSender {
        seq: u64,
        answer: Option<(Arc<StateStore>, u64, &'static str)>,
    }

    impl UnitSender for SeqSender {
        fn send_unit<'a>(
            &'a self,
            _actor: &'a str,
            _key: &'a crate::keys::UserSigningKey,
            _hostname: &'a str,
            command: &'a str,
            _params: serde_json::Value,
        ) -> BoxFuture<'a, crate::relay::CommandOutcome> {
            if let Some((store, seq, result)) = &self.answer {
                store.emit(ChangeEvent::NodeCommandResult {
                    // The other spelling of the same Hub: the correlation must not care.
                    hostname: HOST.to_ascii_uppercase(),
                    node_key: NODE.into(),
                    payload: json!({
                        "transport": "control.radio", "command": "ack",
                        "result": result, "seq": seq,
                    }),
                });
            }
            let seq = self.seq;
            Box::pin(async move {
                crate::relay::CommandOutcome {
                    request_id: Some(format!("{RADIO_SEQ_PREFIX}{seq}")),
                    command_type: command.to_string(),
                    status: "success",
                    message: None,
                    error_code: None,
                    result: None,
                }
            })
        }
    }

    async fn walk_with(
        pending: &PendingService,
        sender: &dyn UnitSender,
        store: &Arc<StateStore>,
        units: &[SendUnit],
    ) -> Result<usize, SendStop> {
        let key = test_signing_key();
        run_send_walk(
            pending,
            sender,
            &key,
            "anna",
            HOST,
            NODE,
            units,
            store.subscribe_command_results(),
            DEVICE_CONFIRM_BUDGET,
        )
        .await
    }

    /// A sender whose Node rejoins after each of the first `rejoins` sends, on `carrier`. Where
    /// the join flushes the queue (LoRaWAN) that send's frame is lost; where it does not (P2P)
    /// the frame still reaches the Node and is acked. Every later send is acked.
    struct RejoinSender {
        store: Arc<StateStore>,
        rejoins: usize,
        carrier: portal_domain::Transport,
        sent: std::sync::atomic::AtomicUsize,
    }

    impl UnitSender for RejoinSender {
        fn join_flushes_queue(&self, _node_key: &str) -> bool {
            self.carrier.join_flushes_queue()
        }

        fn send_unit<'a>(
            &'a self,
            _actor: &'a str,
            _key: &'a crate::keys::UserSigningKey,
            _hostname: &'a str,
            command: &'a str,
            _params: serde_json::Value,
        ) -> BoxFuture<'a, crate::relay::CommandOutcome> {
            let n = self.sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let seq = 40 + n as u64;
            let rejoined = n < self.rejoins;
            if rejoined {
                self.store.emit(ChangeEvent::NodeJoined {
                    hostname: HOST.into(),
                    node_key: NODE.into(),
                    at: now_unix() + 60,
                });
            }
            if !rejoined || !self.carrier.join_flushes_queue() {
                self.store.emit(ChangeEvent::NodeCommandResult {
                    hostname: HOST.into(),
                    node_key: NODE.into(),
                    payload: json!({
                        "transport": "control.radio", "command": "ack",
                        "result": "ok", "seq": seq,
                    }),
                });
            }
            Box::pin(async move {
                crate::relay::CommandOutcome {
                    request_id: Some(format!("{RADIO_SEQ_PREFIX}{seq}")),
                    command_type: command.to_string(),
                    status: "success",
                    message: None,
                    error_code: None,
                    result: None,
                }
            })
        }
    }

    /// P-7 on both carriers, one scenario. LoRaWAN: a rejoin before the ack means the queue was
    /// flushed, so the walk sends the unit once more, and a second rejoin does not start a third
    /// send (the row fails as unanswered). P2P: the join keeps the queue, so the walk sends
    /// once and waits for the ack, however often the Node rejoins (bench 2026-09-27, 5722).
    #[tokio::test]
    async fn a_walk_resends_after_a_rejoin_only_where_the_join_flushed_the_queue() {
        for carrier in portal_domain::Transport::ALL {
            for rejoins in [1, 2] {
                let flushes = carrier.join_flushes_queue();
                let (sends, confirmed) = if flushes { (2, rejoins < 2) } else { (1, true) };
                let (_dir, pending) = service();
                draft(
                    &pending,
                    "application.interval_report",
                    PENDING_KIND_CONFIG,
                    "600",
                )
                .await;
                let store = Arc::new(StateStore::new());
                let sender = RejoinSender {
                    store: Arc::clone(&store),
                    rejoins,
                    carrier,
                    sent: std::sync::atomic::AtomicUsize::new(0),
                };
                let units = [config_unit("application.interval_report", "600")];
                let outcome = walk_with(&pending, &sender, &store, &units).await;
                let case = format!("{carrier:?}, {rejoins} rejoin(s)");
                assert_eq!(
                    sender.sent.load(std::sync::atomic::Ordering::SeqCst),
                    sends,
                    "{case}"
                );
                assert_eq!(outcome.is_ok(), confirmed, "{case}: {outcome:?}");
                assert_eq!(
                    pending.list(HOST, NODE).await.expect("draft").is_empty(),
                    confirmed,
                    "{case}"
                );
            }
        }
    }

    /// The seam's senders take P-7 from the one carrier resolver, not from the Node's retained
    /// `info`.
    #[test]
    fn the_radio_sender_asks_the_carrier_resolver_whether_a_join_flushes() {
        use control_radio::Mode;
        let relay = crate::relay::CommandRelay::test_instance();
        for (mode, flushes) in [(Mode::Lorawan, true), (Mode::P2p, false)] {
            let dir = tempfile::tempdir().expect("tempdir");
            let radio = crate::radio::RadioControl::for_tests(mode, dir.path());
            let sender = RadioUnitSender {
                radio: &radio,
                relay: &relay,
            };
            assert_eq!(sender.join_flushes_queue(NODE), flushes, "{mode:?}");
            let owned = RadioSender {
                radio: radio.clone(),
                relay: relay.clone(),
            };
            assert_eq!(owned.join_flushes_queue(NODE), flushes, "{mode:?}");
        }
        assert!(
            relay.join_flushes_queue(NODE),
            "the relay reaches LoRaWAN Nodes"
        );
    }

    /// N30 (night test): a Discard or an edit while the walk waits for the Node ends the walk
    /// at once, rather than keeping the Node claimed until the budget runs out. Real time with a
    /// short budget: paused time would jump over the budget while the draft is being read.
    #[tokio::test]
    async fn a_discard_while_the_walk_waits_frees_the_node_at_once() {
        for edit in [false, true] {
            let (_dir, pending) = service();
            draft(
                &pending,
                "application.interval_report",
                PENDING_KIND_CONFIG,
                "600",
            )
            .await;
            let store = Arc::new(StateStore::new());
            let units = [config_unit("application.interval_report", "600")];
            let silent = SeqSender {
                seq: 7,
                answer: None,
            };
            let key = test_signing_key();
            let budget = Duration::from_secs(5);
            let started = std::time::Instant::now();

            let walk = run_send_walk(
                &pending,
                &silent,
                &key,
                "anna",
                HOST,
                NODE,
                &units,
                store.subscribe_command_results(),
                budget,
            );
            let overtake = async {
                // Once the row says `sending`, the walk is waiting for the Node.
                loop {
                    let rows = pending.list(HOST, NODE).await.expect("draft");
                    if rows.iter().any(|r| r.state == PENDING_STATE_SENDING) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                if edit {
                    draft(
                        &pending,
                        "application.interval_report",
                        PENDING_KIND_CONFIG,
                        "900",
                    )
                    .await;
                } else {
                    pending.discard(HOST, NODE).await.expect("discard");
                }
                pending.nudge(HOST, NODE);
            };
            let (stop, ()) = tokio::join!(walk, overtake);
            assert!(
                matches!(stop, Err(SendStop::Superseded { .. })),
                "edit={edit}: {stop:?}"
            );
            assert!(
                started.elapsed() < budget,
                "edit={edit}: waited out the budget"
            );
            let rows = pending.list(HOST, NODE).await.expect("draft");
            if edit {
                assert_eq!(rows.len(), 1, "the newer value stays");
                assert_eq!(rows[0].desired_value.as_deref(), Some("900"));
                assert_eq!(rows[0].state, PENDING_STATE_PENDING, "and is untouched");
            } else {
                assert!(rows.is_empty(), "the discard stands");
            }
        }
    }

    /// A seam-sent unit is confirmed by the Node's ack for **its** seq — and only that.
    #[tokio::test(start_paused = true)]
    async fn a_seam_sent_unit_is_confirmed_by_the_ack_for_its_own_seq() {
        let (_dir, pending) = service();
        draft(
            &pending,
            "application.interval_report",
            PENDING_KIND_CONFIG,
            "600",
        )
        .await;
        let store = Arc::new(StateStore::new());
        let units = [config_unit("application.interval_report", "600")];

        let acked = SeqSender {
            seq: 22,
            answer: Some((store.clone(), 22, "ok")),
        };
        walk_with(&pending, &acked, &store, &units)
            .await
            .expect("confirmed by the ack for seq 22");
        assert!(
            pending.list(HOST, NODE).await.expect("draft").is_empty(),
            "the confirmed row is gone"
        );

        // An ack for another exchange confirms nothing: the budget runs out.
        draft(
            &pending,
            "application.interval_report",
            PENDING_KIND_CONFIG,
            "600",
        )
        .await;
        let other = SeqSender {
            seq: 23,
            answer: Some((store.clone(), 99, "ok")),
        };
        let stop = walk_with(&pending, &other, &store, &units)
            .await
            .expect_err("no ack for seq 23");
        assert!(matches!(stop, SendStop::NodeUndelivered { .. }));
    }

    /// A refusal for the unit's seq stops the walk and says so on the row.
    #[tokio::test(start_paused = true)]
    async fn a_refused_seam_unit_stops_the_walk_with_the_reason() {
        let (_dir, pending) = service();
        draft(
            &pending,
            "application.interval_report",
            PENDING_KIND_CONFIG,
            "600",
        )
        .await;
        let store = Arc::new(StateStore::new());
        let refused = SeqSender {
            seq: 30,
            answer: Some((store.clone(), 30, "error")),
        };
        walk_with(
            &pending,
            &refused,
            &store,
            &[config_unit("application.interval_report", "600")],
        )
        .await
        .expect_err("refused");
        let rows = pending.list(HOST, NODE).await.expect("draft");
        assert_eq!(rows.len(), 1, "a refused value stays in the draft");
        // STICKER 2.0.0 rolls a refused message back whole: `error`, not `undelivered`.
        assert_eq!(rows[0].state, PENDING_STATE_ERROR);
        assert_eq!(
            rows[0].last_error.as_deref(),
            Some("the Node refused the message; nothing was applied")
        );
    }

    /// The walk is sequential and stops at the first failure: the second
    /// unit is never published, and its draft row is left untouched for a
    /// human to look at. Configuration keys are not independent (module
    /// doc), which is the whole reason for the rule.
    #[tokio::test]
    async fn a_walk_stops_at_the_first_failure_and_sends_nothing_after_it() {
        let (_dir, pending) = service();
        draft(
            &pending,
            "application.interval_report",
            PENDING_KIND_CONFIG,
            "600",
        )
        .await;
        draft(
            &pending,
            "application.interval_sample",
            PENDING_KIND_CONFIG,
            "60",
        )
        .await;

        let store = Arc::new(StateStore::new());
        let relay = ScriptedRelay::new(&["error"], Some(store.clone()));
        let units = [
            config_unit("application.interval_report", "600"),
            config_unit("application.interval_sample", "60"),
        ];

        let stop = walk(&pending, &relay, &store, &units)
            .await
            .expect_err("the first unit failed");
        // A config unit is the Node's part: it did not land (PF-3).
        assert!(
            matches!(&stop, SendStop::NodeUndelivered { config_keys, .. } if !config_keys.is_empty()),
            "{stop:?}"
        );
        assert_eq!(
            relay.commands().len(),
            1,
            "the second unit never reached the wire"
        );

        let rows = pending.list(HOST, NODE).await.expect("draft");
        assert_eq!(rows.len(), 2, "nothing was deleted");
        let failed = rows
            .iter()
            .find(|r| r.param_key == "application.interval_report")
            .expect("first row");
        assert_eq!(failed.state, PENDING_STATE_UNDELIVERED);
        // The relay's own word, not the interrupt guard's. `ScriptedRelay`
        // answers with status "error" and no message, so the row's reason
        // is "error" — if the walk stopped clearing the guard before it
        // marks the failure, `Drop` would overwrite this with
        // `SEND_INTERRUPTED` and the operator would be told the portal was
        // shut down mid-send rather than that the Hub refused.
        assert_eq!(failed.last_error.as_deref(), Some("error"));
        let untouched = rows
            .iter()
            .find(|r| r.param_key == "application.interval_sample")
            .expect("second row");
        assert_eq!(
            untouched.state, "pending",
            "the rest of the draft is left exactly as it was"
        );
    }

    /// The Hub's "success" is not what deletes a row — the device's answer
    /// is. Held apart here by withholding the device event: the relay
    /// accepts, the walk times out on the device, and the row survives.
    #[tokio::test(start_paused = true)]
    async fn a_config_row_is_deleted_only_after_the_device_answers() {
        let (_dir, pending) = service();
        draft(
            &pending,
            "application.interval_report",
            PENDING_KIND_CONFIG,
            "600",
        )
        .await;

        let store = Arc::new(StateStore::new());
        // No `confirm_with`: the Hub accepts and the device never answers.
        let silent = ScriptedRelay::new(&["success"], None);
        let units = [config_unit("application.interval_report", "600")];
        let stop = walk(&pending, &silent, &store, &units)
            .await
            .expect_err("no device answer");
        assert!(matches!(stop, SendStop::NodeUndelivered { .. }));
        assert_eq!(
            pending.list(HOST, NODE).await.expect("draft").len(),
            1,
            "the Hub accepting is not the device confirming"
        );

        // The same unit, this time with the device answering.
        let answering = ScriptedRelay::new(&["success"], Some(store.clone()));
        assert_eq!(
            walk(&pending, &answering, &store, &units)
                .await
                .expect("confirmed"),
            1
        );
        assert!(
            pending.list(HOST, NODE).await.expect("draft").is_empty(),
            "the device confirmed, so the row is gone"
        );
    }

    /// A threshold unit has one clock. Nothing will ever emit a
    /// `NodeCommandResult` for `set_lorawan_field_threshold` (the Hub
    /// evaluates thresholds itself), so a walk that waited for one would
    /// time out every time — this asserts the row is cleared on the relay's
    /// answer alone, with no device event anywhere.
    ///
    /// `start_paused` for the same reason as
    /// [`a_threshold_unit_has_no_device_leg_to_wait_for`]: a regression that
    /// starts waiting would otherwise hang this test for 180 s of real
    /// time before it failed.
    #[tokio::test(start_paused = true)]
    async fn a_threshold_unit_clears_on_the_hubs_answer_alone() {
        let (_dir, pending) = service();
        draft(
            &pending,
            "threshold.temperature.critical_high",
            PENDING_KIND_THRESHOLD,
            "9.5",
        )
        .await;

        let store = Arc::new(StateStore::new());
        let relay = ScriptedRelay::new(&["success"], None);
        let units = [SendUnit {
            param_keys: vec![SendRow {
                param_key: "threshold.temperature.critical_high".into(),
                desired_value: Some("9.5".into()),
            }],
            label: "threshold.temperature".into(),
            command: THRESHOLD_COMMAND,
            params: json!({ "dev_eui": NODE, "field": "temperature" }),
            threshold: None,
            node_previous: Vec::new(),
            node_bands: Vec::new(),
            unreadable: None,
            refusal: None,
        }];

        assert_eq!(
            walk(&pending, &relay, &store, &units).await.expect("sent"),
            1
        );
        // Decision #41: the threshold unit is the portal's own write, nothing goes to the Hub.
        assert!(relay.commands().is_empty());
        assert!(pending.list(HOST, NODE).await.expect("draft").is_empty());
    }

    /// §21 review, defect 3. The walk plans once and writes many times, so
    /// what it plans can stop matching what is stored — this is the
    /// re-read that catches it.
    ///
    /// The plan is built from a row that says `60`; the stored row says
    /// `120` by the time the walk reaches it. The `expect`-conditioned
    /// write misses, the unit stops with `Superseded`, the stale value
    /// never reaches the wire, and the newer one is still there afterwards.
    /// (An edit landing *during* the 180 s device wait is the same
    /// mismatch, arrived at from the other direction; this stages it
    /// before the walk starts because that needs no timing at all.)
    #[tokio::test]
    async fn a_stale_plan_loses_to_the_live_row_and_stops_the_walk() {
        let (_dir, pending) = service();
        draft(
            &pending,
            "application.interval_report",
            PENDING_KIND_CONFIG,
            "600",
        )
        .await;
        draft(
            &pending,
            "application.interval_sample",
            PENDING_KIND_CONFIG,
            "60",
        )
        .await;

        let store = Arc::new(StateStore::new());
        let relay = ScriptedRelay::new(&["success", "success"], Some(store.clone()));
        let units = [
            config_unit("application.interval_report", "600"),
            config_unit("application.interval_sample", "60"),
        ];

        // Boris retypes the SECOND key. The plan above still holds `60`;
        // the row now holds `120`, which is the state the walk finds when
        // it gets there.
        draft(
            &pending,
            "application.interval_sample",
            PENDING_KIND_CONFIG,
            "120",
        )
        .await;

        let stop = walk(&pending, &relay, &store, &units)
            .await
            .expect_err("the second unit was overtaken");
        assert!(
            matches!(stop, SendStop::Superseded { .. }),
            "an overtaken unit is a stop, not a silent send: {stop:?}"
        );
        assert_eq!(
            relay.commands().len(),
            1,
            "the stale value never reached the wire"
        );

        let rows = pending.list(HOST, NODE).await.expect("draft");
        assert_eq!(
            rows.len(),
            1,
            "the confirmed first unit went, the edit stayed"
        );
        assert_eq!(rows[0].param_key, "application.interval_sample");
        assert_eq!(
            rows[0].desired_value.as_deref(),
            Some("120"),
            "the newer value is intact and unsent"
        );
    }

    /// §21 review round 2, item 2. A unit is not always one row: a
    /// threshold **field** is up to four, marked `sending` one statement at
    /// a time. If the second one is overtaken, the first must not be left
    /// behind in `sending` — that is the state the banner renders with both
    /// Send and Discard disabled, and nothing published to justify it.
    #[tokio::test]
    async fn a_supersede_mid_unit_leaves_no_earlier_row_in_sending() {
        let (_dir, pending) = service();
        let low = threshold_param_key("temperature", "critical_low");
        let high = threshold_param_key("temperature", "critical_high");
        draft(&pending, &low, PENDING_KIND_THRESHOLD, "-5.0").await;
        draft(&pending, &high, PENDING_KIND_THRESHOLD, "9.5").await;

        // One unit over both bounds, planned against `9.5` for the high one.
        let units = [SendUnit {
            param_keys: vec![
                SendRow {
                    param_key: low.clone(),
                    desired_value: Some("-5.0".into()),
                },
                SendRow {
                    param_key: high.clone(),
                    desired_value: Some("9.5".into()),
                },
            ],
            label: "threshold.temperature".into(),
            command: THRESHOLD_COMMAND,
            params: json!({ "dev_eui": NODE, "field": "temperature" }),
            threshold: None,
            node_previous: Vec::new(),
            node_bands: Vec::new(),
            unreadable: None,
            refusal: None,
        }];

        // Boris retypes the second bound. The plan is now stale for it and
        // still current for the first.
        draft(&pending, &high, PENDING_KIND_THRESHOLD, "12.0").await;

        let store = Arc::new(StateStore::new());
        let relay = ScriptedRelay::new(&["success"], Some(store.clone()));
        let stop = walk(&pending, &relay, &store, &units)
            .await
            .expect_err("the unit was overtaken");
        assert!(matches!(stop, SendStop::Superseded { .. }), "{stop:?}");
        assert!(
            relay.commands().is_empty(),
            "the unit stopped before the wire"
        );

        let rows = pending.list(HOST, NODE).await.expect("draft");
        let first = rows.iter().find(|r| r.param_key == low).expect("low bound");
        assert_ne!(
            first.state, PENDING_STATE_SENDING,
            "the row marked before the stop is not stranded in sending"
        );
        assert_eq!(
            first.state, PENDING_STATE_PENDING,
            "nothing was sent for it, so it is still simply unsent"
        );
        assert_eq!(first.desired_value.as_deref(), Some("-5.0"));

        let second = rows
            .iter()
            .find(|r| r.param_key == high)
            .expect("high bound");
        assert_eq!(
            second.desired_value.as_deref(),
            Some("12.0"),
            "the edit that overtook the walk is untouched"
        );
    }

    /// §21 review round 2, item 3. A draft the portal cannot write is a
    /// stop, not a warning to publish past.
    ///
    /// The old code logged the failure and sent anyway, which is the worst
    /// of both: the device applies the value, and the delete-on-confirm
    /// fails the same way the mark did, so the draft keeps offering to send
    /// something that already landed. The failing store here is a real
    /// `Storage` opened one migration short of the table (see
    /// `PendingService::from_storage`).
    #[tokio::test]
    async fn a_store_that_cannot_mark_sending_stops_before_the_wire() {
        let dir = tempfile::tempdir().expect("tempdir");
        // V13 is what creates `pending_config`; V12 is the schema without it.
        let storage =
            portal_storage::Storage::open_at_version(&dir.path().join("portal.db"), None, 12)
                .expect("open at V12");
        let pending = PendingService::from_storage(storage);
        assert!(
            pending.list(HOST, NODE).await.is_err(),
            "the fixture is only meaningful if the draft table is really missing"
        );

        let store = Arc::new(StateStore::new());
        let relay = ScriptedRelay::new(&["success"], Some(store.clone()));
        let units = [config_unit("application.interval_report", "600")];

        let stop = walk(&pending, &relay, &store, &units)
            .await
            .expect_err("the draft could not be marked");
        assert!(
            matches!(stop, SendStop::Storage { .. }),
            "a storage failure has its own stop, distinguishable in the audit \
             row from a Hub that refused: {stop:?}"
        );
        assert!(
            relay.commands().is_empty(),
            "nothing is published on the strength of a write that failed"
        );
    }

    /// §21 review round 2, item 8. Two Hubs, one DevEUI: the answer that
    /// confirms a walk has to come from the Hub the walk published to.
    ///
    /// `node_key` alone used to be the whole device half of the
    /// correlation, so a second Hub told about the same key — a Node moved
    /// between them, or two Hubs with overlapping registrations — would
    /// have either one's result clear the other's draft. Single-Hub today,
    /// one comparison to keep it true when that changes.
    #[tokio::test]
    async fn another_hubs_answer_for_the_same_node_is_not_ours() {
        let store = StateStore::new();
        let mut rx = store.subscribe();
        let waiting = tokio::spawn(async move {
            await_device_confirmation(&mut rx, HOST, NODE, SEND_COMMAND, DEVICE_CONFIRM_BUDGET)
                .await
        });

        // Same Node key, same command, different Hub.
        store.emit(ChangeEvent::NodeCommandResult {
            hostname: "h2".into(),
            node_key: NODE.into(),
            payload: json!({ "command": "set_sticker_config", "result": "ok" }),
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            !waiting.is_finished(),
            "the other Hub's answer is somebody else's conversation"
        );

        store.emit(ok_result());
        assert_eq!(
            waiting.await.expect("join"),
            Confirmation::Confirmed,
            "ours confirms"
        );
    }

    /// §21 review, defect 2. The walk is a detached task now, so it can be
    /// aborted mid-unit — and a row left in `sending` disables both Send and
    /// Discard in the banner, with no way back short of a restart.
    #[tokio::test]
    async fn an_aborted_walk_leaves_no_row_in_sending() {
        let (_dir, pending) = service();
        draft(
            &pending,
            "application.interval_report",
            PENDING_KIND_CONFIG,
            "600",
        )
        .await;

        let store = Arc::new(StateStore::new());
        let units = vec![config_unit("application.interval_report", "600")];
        let task = {
            let pending = pending.clone();
            let store = store.clone();
            tokio::spawn(async move {
                // Accepted by the Hub, then a device that never answers:
                // the walk parks in its 180 s wait with the row `sending`.
                let relay = ScriptedRelay::new(&["success"], None);
                let key = test_signing_key();
                let _ = run_send_walk(
                    &pending,
                    &relay,
                    &key,
                    "anna",
                    HOST,
                    NODE,
                    &units,
                    store.subscribe_command_results(),
                    DEVICE_CONFIRM_BUDGET,
                )
                .await;
            })
        };

        // Wait until the row is actually in flight, then kill the task.
        for _ in 0..200 {
            let rows = pending.list(HOST, NODE).await.expect("draft");
            if rows[0].state == PENDING_STATE_SENDING {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        task.abort();
        let _ = task.await;

        let rows = pending.list(HOST, NODE).await.expect("draft");
        assert_eq!(
            rows[0].state, PENDING_STATE_UNDELIVERED,
            "a dropped walk marks what it was holding as a restart would (PF-3 review): a \
             config key waits for the Node"
        );
        assert_eq!(rows[0].last_error.as_deref(), Some(SEND_INTERRUPTED));
        assert_eq!(
            rows[0].desired_value.as_deref(),
            Some("600"),
            "the value is still there to resend"
        );
    }

    /// §21 review, item 4. Two managers pressing Send at the same moment
    /// get one walk; the second is refused before anything is spawned.
    #[test]
    fn only_one_walk_per_node_may_be_claimed_at_a_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pending = PendingService::open(dir.path().to_str().expect("utf8"), None).expect("open");

        let first = pending.claim_send(HOST, NODE).expect("first claim");
        assert!(
            pending.claim_send(HOST, NODE).is_none(),
            "a second Send for the same Node is refused"
        );
        // A different Node is a different walk.
        let _other = pending
            .claim_send(HOST, "70b3d5e05001aaaa")
            .expect("another Node is unaffected");

        drop(first);
        assert!(
            pending.claim_send(HOST, NODE).is_some(),
            "the claim is released however the walk ends"
        );
    }

    /// §21 review, item 10. A stored value nothing can parse is a stop, not
    /// a `null` pushed at the device — which would clear a setting rather
    /// than report a problem.
    #[tokio::test]
    async fn an_unreadable_value_stops_the_unit_instead_of_sending_null() {
        let (_dir, pending) = service();
        draft(
            &pending,
            "application.interval_report",
            PENDING_KIND_CONFIG,
            "not json at all",
        )
        .await;

        let rows = pending.list(HOST, NODE).await.expect("draft");
        let units = plan_send(
            &rows,
            NODE,
            &BTreeMap::new(),
            &["temperature".to_string(), "humidity".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        assert_eq!(units.len(), 1);
        assert_eq!(
            units[0].unreadable.as_deref(),
            Some("application.interval_report")
        );

        let store = Arc::new(StateStore::new());
        let relay = ScriptedRelay::new(&["success"], Some(store.clone()));
        let stop = walk(&pending, &relay, &store, &units)
            .await
            .expect_err("nothing safe to send");
        assert!(matches!(stop, SendStop::Unreadable { .. }));
        assert!(
            relay.commands().is_empty(),
            "no downlink left the portal for a value nobody can read"
        );
        assert_eq!(
            pending.list(HOST, NODE).await.expect("draft")[0].state,
            PENDING_STATE_UNDELIVERED
        );
    }

    // ---- PF-3: the Hub, then the portal, then the Node ------------------------
    //
    // nodes_phase1.cs.md, dodatek 2026-09-27 (decision Hynek). A bound is set once the Hub
    // and the portal have it — telemetry evaluation is the primary alarm path — and the
    // Node's own table is an extension, confirmed, late or not delivered.

    /// The field's band before the draft: what the Hub, the portal and the Node hold.
    fn current_temperature() -> BTreeMap<String, ThresholdEntryCtx> {
        let mut current = BTreeMap::new();
        current.insert(
            "temperature".to_string(),
            ThresholdEntryCtx {
                critical_low: Some(-10.0),
                warning_low: Some(0.0),
                warning_high: Some(20.0),
                critical_high: Some(25.0),
                enabled: true,
                hysteresis: None,
            },
        );
        current
    }

    const BENCH_SEQ: u64 = 50;
    const SHORT_BUDGET: Duration = Duration::from_millis(150);

    fn radio_answer(seq: u64, result: &str) -> serde_json::Value {
        json!({ "transport": "control.radio", "command": "ack", "result": result, "seq": seq })
    }

    /// The P2P bench in miniature: the Hub takes a threshold write at once (the relay's
    /// answer, no device leg), and the Node's set-config is queued under `seq` and answered,
    /// or not, like the control.radio seam's. It also notes what `field_thresholds` held for
    /// the bound when the Node's part left.
    struct BenchSender {
        answer: Option<(Arc<StateStore>, &'static str)>,
        hub_status: &'static str,
        pending: PendingService,
        sent: StdMutex<Vec<String>>,
        seen_at_node_step: StdMutex<Option<Option<f64>>>,
    }

    impl BenchSender {
        fn new(pending: &PendingService, answer: Option<(Arc<StateStore>, &'static str)>) -> Self {
            Self {
                answer,
                hub_status: "success",
                pending: pending.clone(),
                sent: StdMutex::new(Vec::new()),
                seen_at_node_step: StdMutex::new(None),
            }
        }
        fn commands(&self) -> Vec<String> {
            self.sent.lock().expect("lock").clone()
        }
    }

    impl UnitSender for BenchSender {
        fn send_unit<'a>(
            &'a self,
            _actor: &'a str,
            _key: &'a crate::keys::UserSigningKey,
            _hostname: &'a str,
            command: &'a str,
            _params: serde_json::Value,
        ) -> BoxFuture<'a, crate::relay::CommandOutcome> {
            self.sent.lock().expect("lock").push(command.to_string());
            Box::pin(async move {
                if command != SEND_COMMAND {
                    return crate::relay::CommandOutcome {
                        request_id: None,
                        command_type: command.to_string(),
                        status: self.hub_status,
                        message: None,
                        error_code: None,
                        result: None,
                    };
                }
                let seen = node_thresholds(&self.pending)
                    .await
                    .into_iter()
                    .find(|t| t.field == "temperature")
                    .and_then(|t| t.critical_high);
                *self.seen_at_node_step.lock().expect("lock") = Some(seen);
                if let Some((store, result)) = &self.answer {
                    store.emit(ChangeEvent::NodeCommandResult {
                        hostname: HOST.into(),
                        node_key: NODE.into(),
                        payload: radio_answer(BENCH_SEQ, result),
                    });
                }
                crate::relay::CommandOutcome {
                    request_id: Some(format!("{RADIO_SEQ_PREFIX}{BENCH_SEQ}")),
                    command_type: command.to_string(),
                    status: "success",
                    message: None,
                    error_code: None,
                    result: None,
                }
            })
        }
    }

    async fn walk_bench(
        pending: &PendingService,
        sender: &BenchSender,
        store: &Arc<StateStore>,
        units: &[SendUnit],
    ) -> Result<usize, SendStop> {
        let key = test_signing_key();
        run_send_walk(
            pending,
            sender,
            &key,
            "anna",
            HOST,
            NODE,
            units,
            store.subscribe_command_results(),
            SHORT_BUDGET,
        )
        .await
    }

    /// The thresholds the portal holds now, merged as the routes merge them (no Hub echo).
    async fn held(pending: &PendingService) -> BTreeMap<String, ThresholdEntryCtx> {
        crate::render::merge_node_thresholds(None, &node_thresholds(pending).await)
    }

    /// The bench draft: a critical bound of a slotted field (it rides the Node's table) and a
    /// warning bound (the Hub's alone), with `config` an interval as well, planned against
    /// what the portal holds.
    async fn bench_plan(pending: &PendingService, config: bool) -> Vec<SendUnit> {
        draft(
            pending,
            &threshold_param_key("temperature", "critical_high"),
            PENDING_KIND_THRESHOLD,
            "30.0",
        )
        .await;
        draft(
            pending,
            &threshold_param_key("temperature", "warning_high"),
            PENDING_KIND_THRESHOLD,
            "22.0",
        )
        .await;
        if config {
            draft(
                pending,
                "application.interval_report",
                PENDING_KIND_CONFIG,
                "600",
            )
            .await;
        }
        let rows = pending.list(HOST, NODE).await.expect("draft");
        let mut current = held(pending).await;
        if current.is_empty() {
            current = current_temperature();
        }
        plan_send(
            &rows,
            NODE,
            &current,
            &["temperature".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        )
    }

    async fn node_thresholds(pending: &PendingService) -> Vec<portal_storage::FieldThreshold> {
        pending
            .db
            .run(|s| s.list_field_thresholds(HOST, portal_storage::THRESHOLD_SCOPE_NODE, NODE))
            .await
            .expect("thresholds")
    }

    fn temperature(thresholds: &[portal_storage::FieldThreshold]) -> (Option<f64>, Option<f64>) {
        thresholds
            .iter()
            .find(|t| t.field == "temperature")
            .map(|t| (t.warning_high, t.critical_high))
            .unwrap_or_default()
    }

    /// The Node's open "configuration not delivered" warning, if any.
    async fn open_warning(pending: &PendingService) -> Option<portal_storage::AlarmEvent> {
        pending
            .db
            .run(|s| s.active_alarms_any_case(HOST))
            .await
            .expect("alarms")
            .into_iter()
            .find(|a| a.sensor_id == config_warning_sensor_id(NODE))
    }

    /// Every configuration warning row of the Node, open or archived.
    async fn warnings(pending: &PendingService) -> Vec<portal_storage::AlarmEvent> {
        pending
            .db
            .run(|s| s.list_recent_alarms(HOST, 50))
            .await
            .expect("alarms")
            .into_iter()
            .filter(|a| a.sensor_id == config_warning_sensor_id(NODE))
            .collect()
    }

    async fn delivery(pending: &PendingService) -> Option<portal_storage::NodeThresholdDelivery> {
        pending
            .db
            .run(|s| s.list_node_delivery(HOST, NODE))
            .await
            .expect("delivery")
            .into_iter()
            .find(|d| d.field == "temperature")
    }

    fn state_of(rows: &[PendingConfigRow], key: &str) -> Option<String> {
        rows.iter()
            .find(|r| r.param_key == key)
            .map(|r| r.state.clone())
    }

    async fn audits(pending: &PendingService, event: &str) -> Vec<portal_storage::AuditEntry> {
        let event = event.to_string();
        pending
            .db
            .run(move |s| {
                s.list_audit(&portal_storage::AuditFilter {
                    event_type: Some(event.clone()),
                    ..Default::default()
                })
            })
            .await
            .expect("audit")
            .0
    }

    /// Rules 1 and 3: the Hub first, the portal at once, the Node last. When the Node's part
    /// left, `field_thresholds` already held the new bound; a confirmation in time clears the
    /// rows and records the band as on the Node, with its time.
    #[tokio::test]
    async fn the_hub_and_the_portal_take_a_bound_before_the_node_is_sent() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        let units = bench_plan(&pending, false).await;
        let sender = BenchSender::new(&pending, Some((Arc::clone(&store), "ok")));

        assert_eq!(
            walk_bench(&pending, &sender, &store, &units)
                .await
                .expect("sent"),
            2
        );
        assert_eq!(
            sender.commands(),
            [SEND_COMMAND.to_string()],
            "decision #41: no Hub threshold write"
        );
        assert_eq!(
            *sender.seen_at_node_step.lock().expect("lock"),
            Some(Some(30.0)),
            "the portal evaluated the new bound before the Node was sent it"
        );
        assert!(pending.list(HOST, NODE).await.expect("draft").is_empty());
        assert_eq!(
            temperature(&node_thresholds(&pending).await),
            (Some(22.0), Some(30.0))
        );
        let on_node = delivery(&pending).await.expect("on the Node");
        assert_eq!(on_node.state, portal_storage::DELIVERY_STATE_CONFIRMED);
        assert_eq!(on_node.confirmed_high, Some(30.0));
        assert!(on_node.confirmed_at.is_some(), "with the time it was set");
        let layout = pending
            .db
            .run(|s| s.node_alarm_layout(HOST, NODE))
            .await
            .expect("layout");
        assert_eq!(
            layout.map(crate::radio::alarm::parse_stored_layout),
            Some((
                vec!["temperature".to_string()],
                Some(crate::radio::alarm::SlotTypes([2, 0, 0, 0]))
            )),
            "with the slot types the rules name"
        );
        assert!(warnings(&pending).await.is_empty(), "nothing to warn about");
    }

    /// Rule 2: while the Node's part waits, a bound the Hub already has says so, and quotes
    /// what the Node still holds.
    #[tokio::test]
    async fn the_banner_says_the_hub_has_the_bound_while_the_node_is_waited_for() {
        let (_dir, pending) = service();
        let key = threshold_param_key("temperature", "critical_high");
        draft(&pending, &key, PENDING_KIND_THRESHOLD, "30.0").await;
        let row = SendRow {
            param_key: key.clone(),
            desired_value: Some("30.0".into()),
        };
        pending
            .set_previous(HOST, NODE, &row, "25.0")
            .await
            .expect("previous");
        pending
            .set_state(HOST, NODE, &key, None, PENDING_STATE_SENDING, None, true)
            .await
            .expect("sending");

        let rows = pending.list(HOST, NODE).await.expect("draft");
        let ctx = crate::render::pending_ctx(NODE, &rows, None, &BTreeMap::new(), true, now_unix());
        assert!(ctx.rows[0].hub_set);
        // Displayed like the drafted bound (UX-038): a number, not the stored JSON text.
        assert_eq!(ctx.rows[0].current.as_deref(), Some("25"));
        for lang in ["cs", "en"] {
            for key in [
                "monitoring.pending.hub_set_node_pending",
                "monitoring.pending.undelivered_bound",
                "monitoring.pending.undelivered_note",
                "monitoring.threshold_table.on_node_undelivered",
            ] {
                assert_ne!(crate::i18n::lookup(lang, key), key, "{lang}: {key}");
            }
        }
        assert_eq!(
            crate::i18n::lookup("cs", "monitoring.pending.hub_set_node_pending"),
            "Mez nastavena na Hubu, čeká na potvrzení Nodu"
        );
    }

    /// §10.5 (amended 2026-10-07): the Hub giving the Node's frame up ends the walk as a silent
    /// Node does — the bound in effect, its row `undelivered`, the warning open — without
    /// waiting out the budget, and the row says the Hub gave up.
    #[tokio::test]
    async fn a_frame_the_hub_gave_up_is_undelivered_at_once() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        let units = bench_plan(&pending, false).await;
        let gave_up = BenchSender::new(&pending, Some((Arc::clone(&store), "undelivered")));

        // The row's message is the Hub's giving up, not the budget's "did not confirm".
        let stop = walk_bench(&pending, &gave_up, &store, &units)
            .await
            .expect_err("the Hub gave the frame up");
        assert!(
            matches!(&stop, SendStop::NodeUndelivered { config_keys, .. } if config_keys.is_empty()),
            "{stop:?}"
        );
        let rows = pending.list(HOST, NODE).await.expect("draft");
        let high = rows
            .iter()
            .find(|r| r.param_key == threshold_param_key("temperature", "critical_high"))
            .expect("the Node's row");
        assert_eq!(high.state, PENDING_STATE_UNDELIVERED);
        assert!(
            high.last_error
                .as_deref()
                .is_some_and(|e| e.contains("gave the downlink up after 5 attempts")),
            "{:?}",
            high.last_error
        );
        assert!(
            open_warning(&pending).await.is_some(),
            "the warning is open"
        );
    }

    /// Rule 4: a Node that does not confirm in time. The bound stays in effect — the walk is
    /// the success of the bound, the Node's part is not delivered — its row is `undelivered`,
    /// the band is "not on the Node", and a warning opens. Nothing reverts on its own. "Send
    /// again" sends the Node's part only, and its confirmation closes the warning.
    #[tokio::test]
    async fn a_bound_the_node_does_not_confirm_stays_in_effect_and_warns() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        let units = bench_plan(&pending, false).await;
        let silent = BenchSender::new(&pending, None);

        let stop = walk_bench(&pending, &silent, &store, &units)
            .await
            .expect_err("the Node did not answer");
        assert!(
            matches!(&stop, SendStop::NodeUndelivered { config_keys, .. } if config_keys.is_empty()),
            "every bound it carried is in effect: {stop:?}"
        );
        assert_eq!(
            temperature(&node_thresholds(&pending).await),
            (Some(22.0), Some(30.0)),
            "the Hub and the portal keep the new band; nothing reverts on its own"
        );
        let rows = pending.list(HOST, NODE).await.expect("draft");
        let high = threshold_param_key("temperature", "critical_high");
        assert_eq!(
            state_of(&rows, &high).as_deref(),
            Some(PENDING_STATE_UNDELIVERED)
        );
        assert_eq!(
            rows.iter()
                .find(|r| r.param_key == high)
                .and_then(|r| r.previous_value.clone()),
            Some("25.0".to_string()),
            "what the Hub held before, for a Discard"
        );
        assert_eq!(
            state_of(&rows, &threshold_param_key("temperature", "warning_high")),
            None,
            "the Hub's own bound is done"
        );
        assert_eq!(
            delivery(&pending).await.map(|d| d.state).as_deref(),
            Some(portal_storage::DELIVERY_STATE_UNDELIVERED)
        );
        let warning = open_warning(&pending).await.expect("the warning is open");
        assert_eq!(warning.alarm_type, CONFIG_WARNING_TYPE);
        assert_eq!(warning.severity, "warning");

        // Send again: the Node's part alone.
        let resend = plan_send(
            &pending.list(HOST, NODE).await.expect("draft"),
            NODE,
            &held(&pending).await,
            &["temperature".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        assert_eq!(
            resend.iter().map(|u| u.command).collect::<Vec<_>>(),
            [SEND_COMMAND],
            "no Hub write again"
        );
        let answering = BenchSender::new(&pending, Some((Arc::clone(&store), "ok")));
        walk_bench(&pending, &answering, &store, &resend)
            .await
            .expect("confirmed");
        assert_eq!(answering.commands(), [SEND_COMMAND.to_string()]);
        assert!(pending.list(HOST, NODE).await.expect("draft").is_empty());
        assert!(
            open_warning(&pending).await.is_none(),
            "closed by the resend"
        );
        assert_eq!(
            delivery(&pending)
                .await
                .map(|d| (d.state, d.confirmed_high)),
            Some((
                portal_storage::DELIVERY_STATE_CONFIRMED.to_string(),
                Some(30.0)
            ))
        );
    }

    /// Rule 5: the Node's late ok finishes the unit through the listener — the rows go, the
    /// band is on the Node, the warning closes, and it is audited. A late refusal marks
    /// nothing new.
    #[tokio::test]
    async fn a_late_ack_finishes_the_unit_and_closes_the_warning() {
        for (result, finished) in [("ok", true), ("error", false)] {
            let (_dir, pending) = service();
            let store = Arc::new(StateStore::new());
            pending.spawn_late_answers(Arc::clone(&store));
            let units = bench_plan(&pending, false).await;
            walk_bench(&pending, &BenchSender::new(&pending, None), &store, &units)
                .await
                .expect_err("timed out");
            assert!(open_warning(&pending).await.is_some());

            store.emit(ChangeEvent::NodeCommandResult {
                // The topic's spelling of the Hub; the rows carry the walk's.
                hostname: HOST.to_ascii_uppercase(),
                node_key: NODE.into(),
                payload: radio_answer(BENCH_SEQ, result),
            });
            for _ in 0..100 {
                if pending.late.lock().expect("lock").is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // `late` empties when the listener takes the unit, before `finish_late` writes
            // anything; on a loaded runner a fixed 50 ms is not always enough for those writes,
            // so the ok case waits for the outcome itself, bounded.
            for _ in 0..500 {
                if !finished
                    || (pending.list(HOST, NODE).await.expect("draft").is_empty()
                        && open_warning(&pending).await.is_none()
                        && !audits(&pending, "node_config_applied_late")
                            .await
                            .is_empty()
                        && delivery(&pending)
                            .await
                            .is_some_and(|d| d.state == portal_storage::DELIVERY_STATE_CONFIRMED))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;

            let rows = pending.list(HOST, NODE).await.expect("draft");
            let late = audits(&pending, "node_config_applied_late").await;
            if finished {
                assert!(rows.is_empty(), "{result}: {rows:?}");
                assert!(open_warning(&pending).await.is_none(), "{result}");
                assert_eq!(late.len(), 1, "{result}");
                assert_eq!(
                    delivery(&pending).await.map(|d| d.state).as_deref(),
                    Some(portal_storage::DELIVERY_STATE_CONFIRMED)
                );
            } else {
                assert_eq!(rows.len(), 1, "{result}");
                assert_eq!(rows[0].state, PENDING_STATE_ERROR, "{result}");
                assert!(open_warning(&pending).await.is_some(), "{result}");
                assert!(late.is_empty(), "{result}");
                assert!(
                    !pending
                        .apply_late_answer(HOST, NODE, &radio_answer(BENCH_SEQ, "ok"))
                        .await,
                    "the refusal forgot the unit"
                );
            }
        }
    }

    /// Amendment 2026-10-13: warnings are stored at once, so a Send of a critical bound carries
    /// the warnings the portal holds, and a warning save between the plan and the Hub's landing
    /// is not overwritten by the plan's older copy — on landing or on a Discard's revert.
    #[tokio::test]
    async fn a_send_keeps_the_warnings_stored_after_it_was_planned() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        let band = |warning_high: f64| portal_storage::NewFieldThreshold {
            hostname: HOST.into(),
            scope: portal_storage::THRESHOLD_SCOPE_NODE.into(),
            entity: NODE.into(),
            field: "temperature".into(),
            critical_low: Some(-10.0),
            warning_low: Some(0.0),
            warning_high: Some(warning_high),
            critical_high: Some(25.0),
            enabled: true,
        };
        let first = band(20.0);
        pending
            .db
            .write(move |s| s.upsert_field_threshold(&first, Some("seed")))
            .await
            .expect("seed");
        draft(
            &pending,
            &threshold_param_key("temperature", "critical_high"),
            PENDING_KIND_THRESHOLD,
            "30.0",
        )
        .await;
        let rows = pending.list(HOST, NODE).await.expect("draft");
        let units = plan_send(
            &rows,
            NODE,
            &held(&pending).await,
            &["temperature".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        let hub = units
            .iter()
            .find(|u| u.command == THRESHOLD_COMMAND)
            .expect("hub unit");
        assert_eq!(
            hub.params["warning_high"],
            json!(20.0),
            "the held warnings ride along"
        );
        assert!(!hub.threshold.as_ref().expect("write").warnings_drafted);

        // A manager stores new warnings while the send is on its way.
        let second = band(23.0);
        pending
            .db
            .write(move |s| s.upsert_field_threshold(&second, Some("manager1")))
            .await
            .expect("save");
        walk_bench(&pending, &BenchSender::new(&pending, None), &store, &units)
            .await
            .expect_err("timed out");
        assert_eq!(
            temperature(&node_thresholds(&pending).await),
            (Some(23.0), Some(30.0)),
            "the critical bound landed, the newer warning stayed"
        );

        // A Discard puts the critical bound back and still keeps the warning.
        let rows = pending.list(HOST, NODE).await.expect("draft");
        let reverts = plan_revert(&rows, NODE, &held(&pending).await);
        assert_eq!(reverts[0].params["warning_high"], json!(23.0));
        let third = band(24.0);
        pending
            .db
            .write(move |s| s.upsert_field_threshold(&third, Some("manager1")))
            .await
            .expect("save");
        let relay = ScriptedRelay::new(&["success"], None);
        revert_sent(
            &pending,
            &relay,
            &test_signing_key(),
            "boris",
            HOST,
            NODE,
            &reverts,
        )
        .await
        .expect("reverted");
        assert_eq!(
            temperature(&node_thresholds(&pending).await),
            (Some(24.0), Some(25.0))
        );
    }

    /// Rules 5 and 6: only a manual Discard reverts. It puts the Hub and the portal back to
    /// the value from before the send (a signed Hub write, then `field_thresholds`), and the
    /// Discard itself clears the rows, the warning and the unit a late ack could finish. A
    /// Hub that refuses the revert leaves the portal as it was, for the route to keep the
    /// draft.
    #[tokio::test]
    async fn only_a_discard_puts_the_hub_and_the_portal_back() {
        for hub in ["success", "error"] {
            let (_dir, pending) = service();
            let store = Arc::new(StateStore::new());
            let units = bench_plan(&pending, false).await;
            walk_bench(&pending, &BenchSender::new(&pending, None), &store, &units)
                .await
                .expect_err("timed out");

            let rows = pending.list(HOST, NODE).await.expect("draft");
            let reverts = plan_revert(&rows, NODE, &held(&pending).await);
            assert_eq!(reverts.len(), 1, "{hub}");
            assert_eq!(reverts[0].command, THRESHOLD_COMMAND);
            assert_eq!(reverts[0].params["critical_high"], json!(25.0), "{hub}");
            assert_eq!(
                reverts[0].params["warning_high"],
                json!(22.0),
                "{hub}: the warnings the portal holds now, not the send's to take back \
                 (amendment 2026-10-13); the legacy warning row sent 22"
            );

            let relay = ScriptedRelay::new(&[hub], None);
            let key = test_signing_key();
            let reverted = revert_sent(&pending, &relay, &key, "boris", HOST, NODE, &reverts).await;
            assert_eq!(relay.commands(), [THRESHOLD_COMMAND.to_string()], "{hub}");
            if hub == "success" {
                assert_eq!(reverted, Ok(vec!["temperature".to_string()]));
                assert_eq!(
                    temperature(&node_thresholds(&pending).await),
                    (Some(22.0), Some(25.0)),
                    "the critical bound goes back, the warning stays (amendment 2026-10-13)"
                );
                assert!(
                    pending.list(HOST, NODE).await.expect("draft").is_empty(),
                    "a field the Hub took back is finished at once"
                );
                pending.discard(HOST, NODE).await.expect("discard");
                assert!(open_warning(&pending).await.is_none());
                assert!(delivery(&pending).await.is_none(), "never on the Node");
                assert!(pending.late.lock().expect("lock").is_empty());
            } else {
                assert!(reverted.is_err());
                assert_eq!(
                    temperature(&node_thresholds(&pending).await),
                    (Some(22.0), Some(30.0)),
                    "the Hub did not take it back, so neither does the portal"
                );
            }
        }
    }

    /// Rule 7: intervals alone that the Node does not confirm leave an archived warning — a
    /// trace in the alarm history, nothing open — and keep "Send again"; the P-6 reconcile
    /// still clears them when the Node applies them late. With a bound in the same unit the
    /// warning is the active one.
    #[tokio::test]
    async fn undelivered_intervals_leave_an_archived_warning() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        draft(
            &pending,
            "application.interval_report",
            PENDING_KIND_CONFIG,
            "600",
        )
        .await;
        let rows = pending.list(HOST, NODE).await.expect("draft");
        let units = plan_send(
            &rows,
            NODE,
            &BTreeMap::new(),
            &["temperature".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        let stop = walk_bench(&pending, &BenchSender::new(&pending, None), &store, &units)
            .await
            .expect_err("timed out");
        assert!(
            matches!(&stop, SendStop::NodeUndelivered { config_keys, .. } if config_keys.len() == 1),
            "{stop:?}"
        );
        let rows = pending.list(HOST, NODE).await.expect("draft");
        assert_eq!(rows[0].state, PENDING_STATE_UNDELIVERED);
        assert!(
            open_warning(&pending).await.is_none(),
            "nothing asks for action"
        );
        let archived = warnings(&pending).await;
        assert_eq!(archived.len(), 1);
        assert!(archived[0].cleared_at.is_some(), "archived at once");
        let ctx = crate::render::pending_ctx(NODE, &rows, None, &BTreeMap::new(), true, now_unix());
        assert!(ctx.resend, "the draft keeps Send again");

        let config = json!({ "application.interval_report": 600 });
        assert_eq!(
            pending
                .reconcile_late_applies(HOST, NODE, config.as_object().expect("object"))
                .await,
            1,
            "the Node reported it: applied late"
        );

        // A bound in the same unit: the active warning, not an archived one.
        let (_dir, pending) = service();
        let units = bench_plan(&pending, true).await;
        let stop = walk_bench(&pending, &BenchSender::new(&pending, None), &store, &units)
            .await
            .expect_err("timed out");
        assert!(
            matches!(&stop, SendStop::NodeUndelivered { config_keys, .. } if !config_keys.is_empty())
        );
        assert!(open_warning(&pending).await.is_some());
        assert_eq!(
            warnings(&pending).await.len(),
            1,
            "no archived entry beside it"
        );
    }

    /// Q20c: an unconfirmed bound waits 4 h before the warning and the
    /// `config_sync` fault open; the deadline row counts the attempts.
    #[tokio::test]
    async fn an_unconfirmed_node_opens_the_fault_only_after_the_deadline() {
        let (_dir, pending) = service();
        let pending = pending.with_deadline(NODE_CONFIG_DEADLINE);
        assert_eq!(pending.deadline(), NODE_CONFIG_DEADLINE);
        let store = Arc::new(StateStore::new());
        let units = bench_plan(&pending, true).await;
        walk_bench(&pending, &BenchSender::new(&pending, None), &store, &units)
            .await
            .expect_err("timed out");
        let d = pending
            .node_deadline(HOST, NODE)
            .await
            .expect("deadline row");
        assert_eq!(d.attempts, 1);
        assert!(d.deadline_at - d.sent_at >= 4 * 3600);
        pending.sweep_deadlines().await;
        assert!(open_warning(&pending).await.is_none(), "inside the 4 h");
        assert!(open_warning(&pending).await.is_none());

        // Sent 5 h ago: the deadline has passed.
        let past = now_unix() - 5 * 3600;
        pending
            .db
            .run(move |s| s.note_node_config_sent(HOST, NODE, past, 4 * 3600))
            .await
            .expect("backdate");
        pending.sweep_deadlines().await;
        let open = open_warning(&pending).await.expect("warning after 4 h");
        assert!(open.message.unwrap_or_default().contains("4 h"));
        assert!(open_warning(&pending).await.is_some());
        let rows = pending.list(HOST, NODE).await.expect("draft");
        let d = pending
            .node_deadline(HOST, NODE)
            .await
            .expect("deadline row");
        let ctx = crate::render::AwaitingCtx::of(&d, now_unix());
        assert!(ctx.passed && d.attempts == 2);

        // Send again restarts the 4 h; the open warning stays until confirmed.
        pending.note_node_sent(HOST, NODE).await;
        let d = pending
            .node_deadline(HOST, NODE)
            .await
            .expect("deadline row");
        assert_eq!(d.attempts, 3);
        assert!(!crate::render::AwaitingCtx::of(&d, now_unix()).passed);

        // Confirmation: the draft empties, the warning, fault and deadline go.
        drop(rows);
        pending
            .db
            .run(|s| s.delete_pending_for_node(HOST, NODE))
            .await
            .expect("confirm");
        pending.sync_config_warning(HOST, NODE).await;
        assert!(open_warning(&pending).await.is_none());
        assert!(open_warning(&pending).await.is_none());
        assert!(pending.node_deadline(HOST, NODE).await.is_none());
    }

    /// STICKER 2.0.0: an `Error` names `fault_field`; the row says which key, and that
    /// nothing of the message was applied.
    #[test]
    fn a_refusal_names_the_field_the_node_faulted_on() {
        let payload = json!({
            "transport": "control.radio", "seq": 7, "result": "error",
            "error": { "code": 3, "fault_field": 203, "fault_key": "application.interval_report" }
        });
        assert_eq!(
            refusal_message(refusal_field(&payload).as_deref()),
            "the Node refused the message at application.interval_report; nothing was applied"
        );
        assert_eq!(refusal_field(&radio_answer(7, "error")), None);
    }

    /// The Node's direct ack confirms delivery: the deadline row, the cfgsync warning and the
    /// fault go with the draft (Q20c, Hynek 2026-10-15).
    #[tokio::test]
    async fn an_ack_clears_the_deadline_the_warning_and_the_fault() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        let units = bench_plan(&pending, true).await;
        walk_bench(&pending, &BenchSender::new(&pending, None), &store, &units)
            .await
            .expect_err("timed out");
        assert!(
            open_warning(&pending).await.is_some(),
            "deadline zero under cfg(test)"
        );
        assert!(pending.node_deadline(HOST, NODE).await.is_some());
        assert!(
            pending
                .apply_late_answer(HOST, NODE, &radio_answer(BENCH_SEQ, "ok"))
                .await,
            "the late ack finishes the unit"
        );
        assert!(pending.list(HOST, NODE).await.expect("draft").is_empty());
        assert!(open_warning(&pending).await.is_none());
        assert!(open_warning(&pending).await.is_none());
        assert!(pending.node_deadline(HOST, NODE).await.is_none());
    }

    /// Hynek 2026-10-15: a send that ended in `error` (Hub offline) opens
    /// the cfgsync fault after the deadline, like an undelivered one.
    #[tokio::test]
    async fn an_error_row_opens_the_fault_after_the_deadline() {
        let (_dir, pending) = service();
        let pending = pending.with_deadline(NODE_CONFIG_DEADLINE);
        draft(
            &pending,
            "application.interval_report",
            PENDING_KIND_CONFIG,
            "600",
        )
        .await;
        pending
            .db
            .run(|s| {
                s.set_pending_state(
                    HOST,
                    NODE,
                    "application.interval_report",
                    None,
                    PENDING_STATE_ERROR,
                    Some("hub offline"),
                    false,
                )
            })
            .await
            .expect("error state");
        pending.sync_config_warning(HOST, NODE).await;
        assert!(open_warning(&pending).await.is_none(), "inside the 4 h");
        let past = now_unix() - 5 * 3600;
        pending
            .db
            .run(move |s| s.note_node_config_sent(HOST, NODE, past, 4 * 3600))
            .await
            .expect("backdate");
        pending.sweep_deadlines().await;
        assert!(open_warning(&pending).await.is_some());
        assert!(open_warning(&pending).await.is_some());
    }

    /// Rule 8: the warning and the `undelivered` rows survive a restart; only the memory for a
    /// late ack is gone. A restart that caught the walk waiting for the Node leaves its rows
    /// `undelivered` and its band "not delivered", and the start-up pass opens the warning.
    #[tokio::test]
    async fn a_restart_keeps_the_warning_and_ends_a_wait_as_undelivered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8");
        let store = Arc::new(StateStore::new());
        {
            let pending = PendingService::open(data_dir, None).expect("open");
            let units = bench_plan(&pending, false).await;
            walk_bench(&pending, &BenchSender::new(&pending, None), &store, &units)
                .await
                .expect_err("timed out");
            assert!(open_warning(&pending).await.is_some());
        }
        let restarted = PendingService::open(data_dir, None).expect("reopen");
        assert!(open_warning(&restarted).await.is_some(), "persistent");
        let rows = restarted.list(HOST, NODE).await.expect("draft");
        assert_eq!(rows[0].state, PENDING_STATE_UNDELIVERED);
        assert!(
            !restarted
                .apply_late_answer(HOST, NODE, &radio_answer(BENCH_SEQ, "ok"))
                .await,
            "the late-ack memory did not survive"
        );

        // A walk the restart interrupted while it waited for the Node.
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8");
        {
            let pending = PendingService::open(data_dir, None).expect("open");
            let key = threshold_param_key("temperature", "critical_high");
            draft(&pending, &key, PENDING_KIND_THRESHOLD, "30.0").await;
            let row = SendRow {
                param_key: key.clone(),
                desired_value: Some("30.0".into()),
            };
            pending
                .set_previous(HOST, NODE, &row, "25.0")
                .await
                .expect("the Hub took it");
            pending
                .set_state(HOST, NODE, &key, None, PENDING_STATE_SENDING, None, true)
                .await
                .expect("sending");
            pending
                .delivery_pending(
                    HOST,
                    NODE,
                    &[portal_storage::NodeBand {
                        field: "temperature".into(),
                        low: Some(-10.0),
                        high: Some(30.0),
                        enabled: true,
                    }],
                )
                .await;
        }
        let restarted = PendingService::open(data_dir, None).expect("reopen");
        let rows = restarted.list(HOST, NODE).await.expect("draft");
        assert_eq!(rows[0].state, PENDING_STATE_UNDELIVERED);
        assert_eq!(
            delivery(&restarted).await.map(|d| d.state).as_deref(),
            Some(portal_storage::DELIVERY_STATE_UNDELIVERED)
        );
        assert!(open_warning(&restarted).await.is_none());
        restarted.sync_undelivered_warnings().await;
        assert!(
            open_warning(&restarted).await.is_some(),
            "opened after the start"
        );
    }

    /// Rule 4: the warning goes the normal alarm way once wired — the e-mail/webhook job on its
    /// onset and its clear, the pop-up on its onset — under the Node's own name.
    #[tokio::test]
    async fn the_warning_is_announced_like_any_alarm() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        let (notify_tx, mut notify_rx) = mpsc::unbounded_channel();
        let (nudge_tx, mut nudge_rx) = broadcast::channel(16);
        let pending = pending.wire(Arc::clone(&store), notify_tx, nudge_tx);

        let units = bench_plan(&pending, false).await;
        walk_bench(&pending, &BenchSender::new(&pending, None), &store, &units)
            .await
            .expect_err("timed out");
        let opened = open_warning(&pending).await.expect("open");
        match notify_rx.try_recv() {
            Ok(crate::alarms::NotifyJob::Activated(data)) => {
                assert_eq!(data.alarm_id, opened.id);
                assert_eq!(data.alarm_type, CONFIG_WARNING_TYPE);
                assert_eq!(data.node_key.as_deref(), Some(NODE));
            }
            other => panic!("the onset is notified: {other:?}"),
        }
        let mut popped = false;
        while let Ok(nudge) = nudge_rx.try_recv() {
            popped |= nudge.activated_ids.contains(&opened.id);
        }
        assert!(popped, "the onset pops up");

        pending.discard(HOST, NODE).await.expect("discard");
        assert!(matches!(
            notify_rx.try_recv(),
            Ok(crate::alarms::NotifyJob::Cleared { .. })
        ));
    }

    // ---- PF-3 review: re-reads, partial reverts, resends, stopped walks --------

    /// A sender for the review's cases: the Hub takes a threshold write, and during the n-th
    /// one an edit can land under it; the Node's set-config is queued under a fresh seq and
    /// answered or not; frames taken back out of the queue are recorded. It also notes whether
    /// the walk was in its Hub phase at each send.
    struct HookSender {
        pending: PendingService,
        answer: Option<(Arc<StateStore>, &'static str)>,
        on_hub: Option<(usize, NewPendingConfig)>,
        log: StdMutex<Vec<String>>,
        hub_calls: std::sync::atomic::AtomicUsize,
        next_seq: std::sync::atomic::AtomicU64,
        in_hub_phase: StdMutex<Vec<(String, bool)>>,
        /// The Hub's answers to its threshold writes, in order; "success" once they run out.
        hub_statuses: StdMutex<std::collections::VecDeque<&'static str>>,
        /// The n-th threshold write signals `reached` and waits for `release`.
        hub_gate: Option<(usize, Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
        /// Whether a frame asked back out of the queue is still there (not yet delivered).
        frames_queued: bool,
        /// What a Discard's withdrawal reports: the Node may have had a frame (PF-3b).
        maybe_delivered: bool,
    }

    impl HookSender {
        fn new(pending: &PendingService, answer: Option<(Arc<StateStore>, &'static str)>) -> Self {
            Self {
                pending: pending.clone(),
                answer,
                on_hub: None,
                log: StdMutex::new(Vec::new()),
                hub_calls: std::sync::atomic::AtomicUsize::new(0),
                next_seq: std::sync::atomic::AtomicU64::new(60),
                in_hub_phase: StdMutex::new(Vec::new()),
                hub_statuses: StdMutex::new(std::collections::VecDeque::new()),
                hub_gate: None,
                frames_queued: true,
                maybe_delivered: false,
            }
        }
        fn log(&self) -> Vec<String> {
            self.log.lock().expect("lock").clone()
        }
    }

    impl UnitSender for HookSender {
        fn send_unit<'a>(
            &'a self,
            _actor: &'a str,
            _key: &'a crate::keys::UserSigningKey,
            _hostname: &'a str,
            command: &'a str,
            _params: serde_json::Value,
        ) -> BoxFuture<'a, crate::relay::CommandOutcome> {
            self.log.lock().expect("lock").push(command.to_string());
            self.in_hub_phase
                .lock()
                .expect("lock")
                .push((command.to_string(), self.pending.in_hub_phase(HOST, NODE)));
            Box::pin(async move {
                if command != SEND_COMMAND {
                    let n = self
                        .hub_calls
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if let Some((at, row)) = &self.on_hub {
                        if *at == n {
                            self.pending.upsert(row.clone()).await.expect("edit");
                        }
                    }
                    if let Some((at, reached, release)) = &self.hub_gate {
                        if *at == n {
                            reached.notify_one();
                            release.notified().await;
                        }
                    }
                    let status = self
                        .hub_statuses
                        .lock()
                        .expect("lock")
                        .pop_front()
                        .unwrap_or("success");
                    return crate::relay::CommandOutcome {
                        request_id: None,
                        command_type: command.to_string(),
                        status,
                        message: None,
                        error_code: None,
                        result: None,
                    };
                }
                let seq = self
                    .next_seq
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if let Some((store, result)) = &self.answer {
                    store.emit(ChangeEvent::NodeCommandResult {
                        hostname: HOST.into(),
                        node_key: NODE.into(),
                        payload: radio_answer(seq, result),
                    });
                }
                crate::relay::CommandOutcome {
                    request_id: Some(format!("{RADIO_SEQ_PREFIX}{seq}")),
                    command_type: command.to_string(),
                    status: "success",
                    message: None,
                    error_code: None,
                    result: None,
                }
            })
        }

        fn drop_queued<'a>(
            &'a self,
            _node_key: &'a str,
            seqs: &'a [u64],
        ) -> BoxFuture<'a, Result<usize, String>> {
            self.log.lock().expect("lock").push(format!(
                "drop:{}",
                seqs.iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            let dropped = if self.frames_queued { seqs.len() } else { 0 };
            Box::pin(async move { Ok(dropped) })
        }

        fn withdraw_all<'a>(
            &'a self,
            _node_key: &'a str,
            seqs: &'a [u64],
        ) -> BoxFuture<'a, Result<crate::radio::Withdrawal, String>> {
            self.log.lock().expect("lock").push(format!(
                "withdraw:{}",
                seqs.iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            let withdrawal = crate::radio::Withdrawal {
                dropped: seqs.len(),
                maybe_delivered: self.maybe_delivered,
            };
            Box::pin(async move { Ok(withdrawal) })
        }
    }

    async fn hook_walk(
        pending: &PendingService,
        sender: &HookSender,
        store: &Arc<StateStore>,
        units: &[SendUnit],
    ) -> Result<usize, SendStop> {
        let key = test_signing_key();
        run_send_walk(
            pending,
            sender,
            &key,
            "anna",
            HOST,
            NODE,
            units,
            store.subscribe_command_results(),
            SHORT_BUDGET,
        )
        .await
    }

    /// Two slotted fields with a critical bound each: neither Hub unit has a row of its own.
    fn two_fields() -> BTreeMap<String, ThresholdEntryCtx> {
        let mut current = current_temperature();
        current.insert(
            "humidity".to_string(),
            ThresholdEntryCtx {
                critical_low: Some(10.0),
                warning_low: Some(20.0),
                warning_high: Some(80.0),
                critical_high: Some(90.0),
                enabled: true,
                hysteresis: None,
            },
        );
        current
    }

    async fn two_field_plan(pending: &PendingService) -> Vec<SendUnit> {
        let hum = threshold_param_key("humidity", "critical_high");
        let temp = threshold_param_key("temperature", "critical_high");
        draft(pending, &hum, PENDING_KIND_THRESHOLD, "95.0").await;
        draft(pending, &temp, PENDING_KIND_THRESHOLD, "30.0").await;
        let rows = pending.list(HOST, NODE).await.expect("draft");
        plan_send(
            &rows,
            NODE,
            &two_fields(),
            &["temperature".to_string(), "humidity".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        )
    }

    type Band4 = (Option<f64>, Option<f64>, Option<f64>, Option<f64>);

    fn field_band(thresholds: &[portal_storage::FieldThreshold], field: &str) -> Option<Band4> {
        thresholds.iter().find(|t| t.field == field).map(|t| {
            (
                t.critical_low,
                t.warning_low,
                t.warning_high,
                t.critical_high,
            )
        })
    }

    /// Review item 1: a Hub unit with no row of its own (every row of its field rides the Node
    /// unit) is re-read before the wire. An edit landing while the first field is written to the
    /// Hub stops the walk before the second field's Hub write, the field that landed is in
    /// effect and `undelivered`, and a Discard would have waited: the walk was in its Hub phase.
    #[tokio::test]
    #[ignore = "decision #41 (2026-10-15): no Hub threshold write is sent any more"]
    async fn a_field_whose_rows_all_ride_the_node_is_re_read_before_its_hub_write() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        let units = two_field_plan(&pending).await;
        assert_eq!(
            units.iter().map(|u| u.label.as_str()).collect::<Vec<_>>(),
            [
                "threshold.humidity",
                "threshold.temperature",
                NODE_CONFIG_UNIT
            ]
        );
        assert!(
            units[0].param_keys.is_empty() && units[1].param_keys.is_empty(),
            "no Hub row of their own"
        );
        let temp = threshold_param_key("temperature", "critical_high");
        let mut sender = HookSender::new(&pending, None);
        sender.on_hub = Some((
            0,
            NewPendingConfig {
                hostname: HOST.into(),
                node_key: NODE.into(),
                param_key: temp.clone(),
                kind: PENDING_KIND_THRESHOLD.into(),
                desired_value: Some("33.0".into()),
                author: "boris".into(),
            },
        ));

        let stop = hook_walk(&pending, &sender, &store, &units)
            .await
            .expect_err("overtaken");
        assert!(
            matches!(&stop, SendStop::Superseded { applied, .. } if applied == &["humidity".to_string()]),
            "{stop:?}"
        );
        assert_eq!(
            sender.log(),
            [THRESHOLD_COMMAND.to_string()],
            "the second Hub write never left"
        );
        assert_eq!(
            *sender.in_hub_phase.lock().expect("lock"),
            [(THRESHOLD_COMMAND.to_string(), true)]
        );
        assert!(!pending.in_hub_phase(HOST, NODE), "released with the walk");
        let held = node_thresholds(&pending).await;
        assert_eq!(
            field_band(&held, "humidity"),
            Some((Some(10.0), Some(20.0), Some(80.0), Some(95.0)))
        );
        assert_eq!(field_band(&held, "temperature"), None, "not written");
        let rows = pending.list(HOST, NODE).await.expect("draft");
        assert_eq!(
            state_of(&rows, &threshold_param_key("humidity", "critical_high")).as_deref(),
            Some(PENDING_STATE_UNDELIVERED),
            "in effect, not on the Node"
        );
        assert_eq!(
            state_of(&rows, &temp).as_deref(),
            Some(PENDING_STATE_PENDING),
            "the edit stays for the next Send"
        );
        assert!(open_warning(&pending).await.is_some());
    }

    /// Review item 1: an edit of the very field the Hub is being written for still gets the
    /// revert point, the portal mirrors what the Hub took, and the walk stops before the Node.
    #[tokio::test]
    #[ignore = "decision #41 (2026-10-15): no Hub threshold write is sent any more"]
    async fn an_edit_under_the_hub_write_keeps_the_revert_point_and_stops_the_walk() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        let units = bench_plan(&pending, false).await;
        let high = threshold_param_key("temperature", "critical_high");
        let mut sender = HookSender::new(&pending, Some((Arc::clone(&store), "ok")));
        sender.on_hub = Some((
            0,
            NewPendingConfig {
                hostname: HOST.into(),
                node_key: NODE.into(),
                param_key: high.clone(),
                kind: PENDING_KIND_THRESHOLD.into(),
                desired_value: Some("35.0".into()),
                author: "boris".into(),
            },
        ));
        let stop = hook_walk(&pending, &sender, &store, &units)
            .await
            .expect_err("overtaken");
        assert!(matches!(stop, SendStop::Superseded { .. }), "{stop:?}");
        assert_eq!(sender.log(), [THRESHOLD_COMMAND.to_string()]);
        assert_eq!(
            temperature(&node_thresholds(&pending).await),
            (Some(22.0), Some(30.0)),
            "the portal mirrors what the Hub took"
        );
        let rows = pending.list(HOST, NODE).await.expect("draft");
        assert_eq!(rows.len(), 1, "the Hub's own row went");
        assert_eq!(rows[0].desired_value.as_deref(), Some("35.0"));
        assert_eq!(rows[0].state, PENDING_STATE_PENDING);
        assert_eq!(rows[0].previous_value.as_deref(), Some("25.0"));
        let reverts = plan_revert(&rows, NODE, &held(&pending).await);
        assert_eq!(reverts[0].params["critical_high"], json!(25.0));
        // The warnings are the portal's (amendment 2026-10-13): the one held now goes back.
        assert_eq!(reverts[0].params["warning_high"], json!(22.0));
    }

    /// Review item 5: "Send again" takes the earlier attempt out of the Hub's queue before it
    /// queues its own, so the Node does not get two set-configs with `save`. Every earlier
    /// frame came back, so the late-ack memory goes with them. A Discard is not held off while
    /// the Node is waited for.
    #[tokio::test]
    async fn send_again_takes_the_earlier_attempt_out_of_the_queue() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        let units = bench_plan(&pending, false).await;
        let first = HookSender::new(&pending, None);
        hook_walk(&pending, &first, &store, &units)
            .await
            .expect_err("timed out");
        assert!(pending
            .late
            .lock()
            .expect("lock")
            .contains_key(&(HOST.to_string(), NODE.to_string())));

        let resend = plan_send(
            &pending.list(HOST, NODE).await.expect("draft"),
            NODE,
            &held(&pending).await,
            &["temperature".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        let second = HookSender::new(&pending, Some((Arc::clone(&store), "ok")));
        second
            .next_seq
            .store(70, std::sync::atomic::Ordering::SeqCst);
        hook_walk(&pending, &second, &store, &resend)
            .await
            .expect("confirmed");
        assert_eq!(
            second.log(),
            ["drop:60".to_string(), SEND_COMMAND.to_string()],
            "the earlier frame goes before the new one is queued"
        );
        assert_eq!(
            *second.in_hub_phase.lock().expect("lock"),
            [(SEND_COMMAND.to_string(), false)]
        );
        assert!(pending.late.lock().expect("lock").is_empty());
        assert!(pending.list(HOST, NODE).await.expect("draft").is_empty());
        assert!(open_warning(&pending).await.is_none());
    }

    /// Review item 2: a Discard the Hub refuses half-way. The field it took back is finished —
    /// its band from before the send, its rows gone — and only the refused field keeps its
    /// draft, so a resend cannot put the Node on a value the Hub no longer holds.
    #[tokio::test]
    async fn a_partial_revert_keeps_only_the_refused_fields_draft() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        let units = two_field_plan(&pending).await;
        hook_walk(&pending, &HookSender::new(&pending, None), &store, &units)
            .await
            .expect_err("timed out");
        let rows = pending.list(HOST, NODE).await.expect("draft");
        assert!(rows.iter().all(|r| r.state == PENDING_STATE_UNDELIVERED));

        let reverts = plan_revert(&rows, NODE, &held(&pending).await);
        assert_eq!(reverts.len(), 2);
        let relay = ScriptedRelay::new(&["success", "error"], None);
        let key = test_signing_key();
        let failure = revert_sent(&pending, &relay, &key, "boris", HOST, NODE, &reverts)
            .await
            .expect_err("the second field was refused");
        assert_eq!(failure.reverted, ["humidity".to_string()]);
        assert!(failure.reason.starts_with("threshold.temperature"));

        let held_now = node_thresholds(&pending).await;
        assert_eq!(
            field_band(&held_now, "humidity"),
            Some((Some(10.0), Some(20.0), Some(80.0), Some(90.0))),
            "back to the band from before the send"
        );
        assert_eq!(
            temperature(&held_now),
            (Some(20.0), Some(30.0)),
            "the refused field keeps what the Hub holds"
        );
        let rows = pending.list(HOST, NODE).await.expect("draft");
        assert_eq!(
            rows.iter()
                .map(|r| r.param_key.as_str())
                .collect::<Vec<_>>(),
            [threshold_param_key("temperature", "critical_high").as_str()]
        );
        assert!(open_warning(&pending).await.is_some(), "still undelivered");
    }

    /// Review items 5 and 9: an edit while the Node is waited for ends the wait. The frames come
    /// back out of the queue, the bound the Hub took stays `undelivered` (not `sending`), the
    /// table is "not delivered", and the warning follows.
    #[tokio::test]
    async fn an_overtaken_wait_leaves_the_bound_undelivered_and_takes_the_frames_back() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        let units = bench_plan(&pending, true).await;
        let sender = HookSender::new(&pending, None);
        let high = threshold_param_key("temperature", "critical_high");
        let key = test_signing_key();
        let walk = run_send_walk(
            &pending,
            &sender,
            &key,
            "anna",
            HOST,
            NODE,
            &units,
            store.subscribe_command_results(),
            Duration::from_secs(5),
        );
        let overtake = async {
            loop {
                let rows = pending.list(HOST, NODE).await.expect("draft");
                if state_of(&rows, &high).as_deref() == Some(PENDING_STATE_SENDING) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            draft(
                &pending,
                "application.interval_report",
                PENDING_KIND_CONFIG,
                "900",
            )
            .await;
            pending.nudge(HOST, NODE);
        };
        let (stop, ()) = tokio::join!(walk, overtake);
        assert!(matches!(stop, Err(SendStop::Superseded { .. })), "{stop:?}");
        assert_eq!(
            sender.log(),
            [SEND_COMMAND.to_string(), "drop:60".to_string()]
        );
        let rows = pending.list(HOST, NODE).await.expect("draft");
        assert_eq!(
            state_of(&rows, &high).as_deref(),
            Some(PENDING_STATE_UNDELIVERED)
        );
        assert_eq!(
            state_of(&rows, "application.interval_report").as_deref(),
            Some(PENDING_STATE_PENDING)
        );
        assert_eq!(
            delivery(&pending).await.map(|d| d.state).as_deref(),
            Some(portal_storage::DELIVERY_STATE_UNDELIVERED)
        );
        assert!(open_warning(&pending).await.is_some());
        assert!(
            pending.late.lock().expect("lock").is_empty(),
            "every frame came back"
        );
    }

    /// Review item 8: a late ack for a unit whose draft was edited since still records what the
    /// Node took; only the edited row stays.
    #[tokio::test]
    async fn a_late_ack_under_an_edit_still_records_what_the_node_took() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        let units = bench_plan(&pending, true).await;
        hook_walk(&pending, &HookSender::new(&pending, None), &store, &units)
            .await
            .expect_err("timed out");
        draft(
            &pending,
            "application.interval_report",
            PENDING_KIND_CONFIG,
            "900",
        )
        .await;
        assert!(
            pending
                .apply_late_answer(HOST, NODE, &radio_answer(60, "ok"))
                .await
        );
        let rows = pending.list(HOST, NODE).await.expect("draft");
        assert_eq!(
            rows.iter()
                .map(|r| r.param_key.as_str())
                .collect::<Vec<_>>(),
            ["application.interval_report"]
        );
        assert_eq!(
            delivery(&pending)
                .await
                .map(|d| (d.state, d.confirmed_high)),
            Some((
                portal_storage::DELIVERY_STATE_CONFIRMED.to_string(),
                Some(30.0)
            ))
        );
        assert!(open_warning(&pending).await.is_none());
    }

    /// Review item 9: the warning follows the rows. An edit of an undelivered bound makes it
    /// `pending` and closes the warning; a stale resend stopped at its re-read puts a row back
    /// to what it was (`undelivered`, not `pending`); a restart that catches intervals alone in
    /// `sending` leaves their archived warning after the start.
    #[tokio::test]
    async fn the_warning_and_the_states_follow_the_rows_after_every_stop() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        let units = bench_plan(&pending, false).await;
        hook_walk(&pending, &HookSender::new(&pending, None), &store, &units)
            .await
            .expect_err("timed out");
        assert!(open_warning(&pending).await.is_some());
        draft(
            &pending,
            &threshold_param_key("temperature", "critical_high"),
            PENDING_KIND_THRESHOLD,
            "28.0",
        )
        .await;
        assert!(
            open_warning(&pending).await.is_none(),
            "nothing undelivered any more"
        );

        // A stale resend of two undelivered intervals.
        let (_dir, pending) = service();
        for (key, value) in [
            ("application.interval_report", "600"),
            ("application.interval_sample", "60"),
        ] {
            draft(&pending, key, PENDING_KIND_CONFIG, value).await;
        }
        let rows = pending.list(HOST, NODE).await.expect("draft");
        let units = plan_send(
            &rows,
            NODE,
            &BTreeMap::new(),
            &["temperature".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        hook_walk(&pending, &HookSender::new(&pending, None), &store, &units)
            .await
            .expect_err("timed out");
        let rows = pending.list(HOST, NODE).await.expect("draft");
        let resend = plan_send(
            &rows,
            NODE,
            &BTreeMap::new(),
            &["temperature".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        draft(
            &pending,
            "application.interval_sample",
            PENDING_KIND_CONFIG,
            "120",
        )
        .await;
        let stop = hook_walk(&pending, &HookSender::new(&pending, None), &store, &resend)
            .await
            .expect_err("stale");
        assert!(matches!(stop, SendStop::Superseded { .. }), "{stop:?}");
        let rows = pending.list(HOST, NODE).await.expect("draft");
        assert_eq!(
            state_of(&rows, "application.interval_report").as_deref(),
            Some(PENDING_STATE_UNDELIVERED),
            "back to what it was before the walk"
        );

        // A restart catching intervals alone on their way to the Node.
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8");
        {
            let pending = PendingService::open(data_dir, None).expect("open");
            draft(
                &pending,
                "application.interval_report",
                PENDING_KIND_CONFIG,
                "600",
            )
            .await;
            pending
                .set_state(
                    HOST,
                    NODE,
                    "application.interval_report",
                    None,
                    PENDING_STATE_SENDING,
                    None,
                    true,
                )
                .await
                .expect("sending");
        }
        let restarted = PendingService::open(data_dir, None).expect("reopen");
        restarted.sync_undelivered_warnings().await;
        let archived = warnings(&restarted).await;
        assert_eq!(archived.len(), 1);
        assert!(archived[0].cleared_at.is_some(), "archived, nothing open");
    }

    // ---- PF-3 review round 2 ------------------------------------------------------

    /// Item A: an `undelivered` 30 edited to 33, whose Hub write then fails. The Hub still
    /// holds 30, so the 33 is not `undelivered` — it is `pending`, with the Hub write still
    /// owed, and the next plan sends it to the Hub before the Node.
    #[tokio::test]
    #[ignore = "decision #41 (2026-10-15): no Hub threshold write is sent any more"]
    async fn a_hub_stop_after_an_edit_of_an_undelivered_bound_owes_the_hub_its_write() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        let units = bench_plan(&pending, false).await;
        hook_walk(&pending, &HookSender::new(&pending, None), &store, &units)
            .await
            .expect_err("timed out");
        let high = threshold_param_key("temperature", "critical_high");
        draft(&pending, &high, PENDING_KIND_THRESHOLD, "33.0").await;
        assert!(open_warning(&pending).await.is_none(), "an edit closes it");

        let resend = plan_send(
            &pending.list(HOST, NODE).await.expect("draft"),
            NODE,
            &held(&pending).await,
            &["temperature".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        assert_eq!(
            resend[0].command, THRESHOLD_COMMAND,
            "33 owes the Hub its write"
        );
        let refusing = HookSender::new(&pending, None);
        refusing
            .hub_statuses
            .lock()
            .expect("lock")
            .push_back("error");
        let stop = hook_walk(&pending, &refusing, &store, &resend)
            .await
            .expect_err("the Hub refused");
        assert!(
            matches!(&stop, SendStop::Relay { applied, .. } if applied.is_empty()),
            "{stop:?}"
        );
        let rows = pending.list(HOST, NODE).await.expect("draft");
        assert_eq!(
            state_of(&rows, &high).as_deref(),
            Some(PENDING_STATE_PENDING),
            "the Hub holds 30, not 33"
        );
        assert_eq!(
            temperature(&node_thresholds(&pending).await),
            (Some(22.0), Some(30.0))
        );
        let again = plan_send(
            &rows,
            NODE,
            &held(&pending).await,
            &["temperature".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        assert_eq!(
            again.iter().map(|u| u.command).collect::<Vec<_>>(),
            [THRESHOLD_COMMAND, SEND_COMMAND],
            "the next Send writes the Hub first"
        );
        assert!(open_warning(&pending).await.is_none());
    }

    /// Item B: two sends, two bounds of one field, neither confirmed by the Node. The Discard
    /// goes back to the band from before the **first** — not to the one between the sends,
    /// which already carried the first send's bound.
    #[tokio::test]
    async fn a_second_send_keeps_the_first_revert_point() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        let low = threshold_param_key("temperature", "critical_low");
        let high = threshold_param_key("temperature", "critical_high");
        draft(&pending, &low, PENDING_KIND_THRESHOLD, "-5.0").await;
        let first = plan_send(
            &pending.list(HOST, NODE).await.expect("draft"),
            NODE,
            &current_temperature(),
            &["temperature".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        hook_walk(&pending, &HookSender::new(&pending, None), &store, &first)
            .await
            .expect_err("timed out");

        draft(&pending, &high, PENDING_KIND_THRESHOLD, "30.0").await;
        let second = plan_send(
            &pending.list(HOST, NODE).await.expect("draft"),
            NODE,
            &held(&pending).await,
            &["temperature".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        assert_eq!(second[0].command, THRESHOLD_COMMAND);
        hook_walk(&pending, &HookSender::new(&pending, None), &store, &second)
            .await
            .expect_err("timed out");
        assert_eq!(
            temperature(&node_thresholds(&pending).await),
            (Some(20.0), Some(30.0))
        );

        let rows = pending.list(HOST, NODE).await.expect("draft");
        let reverts = plan_revert(&rows, NODE, &held(&pending).await);
        assert_eq!(reverts.len(), 1);
        assert_eq!(
            reverts[0].params["critical_low"],
            json!(-10.0),
            "before the first send"
        );
        assert_eq!(reverts[0].params["critical_high"], json!(25.0));
    }

    /// Items C and D: a Discard the Hub refuses half-way takes the queued frames back out —
    /// they carry a band the Hub no longer holds — and drops the late ack with them. One the Hub
    /// refuses at its first field reverted nothing, and keeps the late ack.
    #[tokio::test]
    async fn a_partial_revert_takes_the_frames_back_and_a_refused_one_keeps_the_late_ack() {
        for (statuses, partial) in [(&["success", "error"][..], true), (&["error"][..], false)] {
            let (_dir, pending) = service();
            let store = Arc::new(StateStore::new());
            let units = two_field_plan(&pending).await;
            hook_walk(&pending, &HookSender::new(&pending, None), &store, &units)
                .await
                .expect_err("timed out");
            let key = (HOST.to_string(), NODE.to_string());
            assert!(pending.late.lock().expect("lock").contains_key(&key));

            let rows = pending.list(HOST, NODE).await.expect("draft");
            let reverts = plan_revert(&rows, NODE, &held(&pending).await);
            let sender = HookSender::new(&pending, None);
            sender
                .hub_statuses
                .lock()
                .expect("lock")
                .extend(statuses.iter().copied());
            let signing = test_signing_key();
            let failure = run_discard(
                &pending,
                &sender,
                Some(&signing),
                "boris",
                HOST,
                NODE,
                &reverts,
            )
            .await
            .expect_err("refused");
            assert_eq!(failure.reverted.is_empty(), !partial);
            // A Discard's withdrawal, which takes a delivered frame too (PF-3b).
            let dropped = sender.log().iter().any(|l| l == "withdraw:60");
            let remembered = pending.late.lock().expect("lock").contains_key(&key);
            if partial {
                assert!(dropped, "the frames came back out: {:?}", sender.log());
                assert!(!remembered, "and the late ack with them");
                assert_eq!(
                    pending.take_queued(HOST, NODE),
                    Vec::<u64>::new(),
                    "nothing left to withdraw"
                );
            } else {
                assert!(!dropped, "nothing was reverted, so nothing is taken back");
                assert!(remembered, "a refused Discard keeps the late ack");
                assert_eq!(pending.take_queued(HOST, NODE), vec![60]);
            }
        }
    }

    // ---- PF-3 review round 3 ------------------------------------------------------

    /// A Discard runs on its own task: a request dropped while the revert waits on the Hub for
    /// the second field does not cut it. The task finishes — the first field back and its rows
    /// gone, the refused second field still `undelivered`, the frames taken back, the late ack
    /// dropped, the audit written — and gives the Node back.
    #[tokio::test]
    async fn a_discard_whose_request_goes_away_still_ends_consistently() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        let units = two_field_plan(&pending).await;
        hook_walk(&pending, &HookSender::new(&pending, None), &store, &units)
            .await
            .expect_err("timed out");
        let rows = pending.list(HOST, NODE).await.expect("draft");
        let reverts = plan_revert(&rows, NODE, &held(&pending).await);

        let (reached, release) = (
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tokio::sync::Notify::new()),
        );
        let mut sender = HookSender::new(&pending, None);
        sender.hub_gate = Some((1, Arc::clone(&reached), Arc::clone(&release)));
        sender
            .hub_statuses
            .lock()
            .expect("lock")
            .extend(["success", "error"]);
        let sender = Arc::new(sender);
        let audited = Arc::new(StdMutex::new(None));
        let audit: DiscardAudit = {
            let audited = Arc::clone(&audited);
            Box::new(move |result: DiscardResult| -> BoxFuture<'static, ()> {
                Box::pin(async move {
                    *audited.lock().expect("lock") = Some(result);
                })
            })
        };
        let guard = pending.begin_discard(HOST, NODE).expect("free");
        let request = spawn_discard(
            pending.clone(),
            Arc::clone(&sender) as Arc<dyn UnitSender>,
            Some(test_signing_key()),
            "boris".into(),
            HOST.into(),
            NODE.into(),
            reverts,
            guard,
            audit,
        );
        // The client goes away while the second field is on its way to the Hub.
        reached.notified().await;
        drop(request);
        assert!(pending.begin_discard(HOST, NODE).is_none(), "still at work");
        release.notify_one();

        let mut freed = None;
        for _ in 0..200 {
            freed = pending.begin_discard(HOST, NODE);
            if freed.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            freed.is_some(),
            "the task ran to its end and gave the Node back"
        );
        let rows = pending.list(HOST, NODE).await.expect("draft");
        assert_eq!(
            rows.iter()
                .map(|r| (r.param_key.as_str(), r.state.as_str()))
                .collect::<Vec<_>>(),
            [(
                threshold_param_key("temperature", "critical_high").as_str(),
                PENDING_STATE_UNDELIVERED
            )]
        );
        assert_eq!(
            field_band(&node_thresholds(&pending).await, "humidity"),
            Some((Some(10.0), Some(20.0), Some(80.0), Some(90.0)))
        );
        assert!(sender.log().iter().any(|l| l == "withdraw:60"));
        assert!(pending.late.lock().expect("lock").is_empty());
        assert!(matches!(
            audited.lock().expect("lock").as_ref(),
            Some(Err(RevertFailure { reverted, .. })) if reverted == &["humidity".to_string()]
        ));
    }

    /// Item B-stale: while a second send of the field waits for the Node, the Node's late ack
    /// for the first send clears that send's rows. The field's revert point moves on with them —
    /// to the band after the first send, which the Node confirmed — so a Discard no longer
    /// reverts a bound the Node holds.
    #[tokio::test]
    async fn a_late_ack_under_a_second_send_moves_the_revert_point_on() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        let low = threshold_param_key("temperature", "critical_low");
        let high = threshold_param_key("temperature", "critical_high");
        draft(&pending, &low, PENDING_KIND_THRESHOLD, "-5.0").await;
        let first = plan_send(
            &pending.list(HOST, NODE).await.expect("draft"),
            NODE,
            &current_temperature(),
            &["temperature".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        hook_walk(&pending, &HookSender::new(&pending, None), &store, &first)
            .await
            .expect_err("timed out");

        draft(&pending, &high, PENDING_KIND_THRESHOLD, "30.0").await;
        let second = plan_send(
            &pending.list(HOST, NODE).await.expect("draft"),
            NODE,
            &held(&pending).await,
            &["temperature".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        // The first send's frame already left the queue, so its late ack is still awaited.
        let mut sender = HookSender::new(&pending, None);
        sender.frames_queued = false;
        sender
            .next_seq
            .store(70, std::sync::atomic::Ordering::SeqCst);
        let key = test_signing_key();
        let walk = run_send_walk(
            &pending,
            &sender,
            &key,
            "anna",
            HOST,
            NODE,
            &second,
            store.subscribe_command_results(),
            Duration::from_secs(5),
        );
        let late = async {
            loop {
                let rows = pending.list(HOST, NODE).await.expect("draft");
                if state_of(&rows, &high).as_deref() == Some(PENDING_STATE_SENDING) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(
                pending
                    .apply_late_answer(HOST, NODE, &radio_answer(60, "ok"))
                    .await,
                "the first send finished late"
            );
            pending.nudge(HOST, NODE);
        };
        let (stop, ()) = tokio::join!(walk, late);
        assert!(matches!(stop, Err(SendStop::Superseded { .. })), "{stop:?}");

        let rows = pending.list(HOST, NODE).await.expect("draft");
        assert_eq!(
            rows.iter()
                .map(|r| r.param_key.as_str())
                .collect::<Vec<_>>(),
            [high.as_str()]
        );
        let reverts = plan_revert(&rows, NODE, &held(&pending).await);
        assert_eq!(
            reverts[0].params["critical_low"],
            json!(-5.0),
            "the Node confirmed -5, so it stays"
        );
        assert_eq!(reverts[0].params["critical_high"], json!(25.0));
    }

    /// Round 3, small item 1: a warning the operator's own edit reopens (a bound edited back to
    /// the value the Hub holds) opens without an e-mail or a pop-up, as its close did.
    #[tokio::test]
    async fn an_edit_reopens_and_closes_the_warning_quietly() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        let (notify_tx, mut notify_rx) = mpsc::unbounded_channel();
        let (nudge_tx, mut nudge_rx) = broadcast::channel(64);
        let pending = pending.wire(Arc::clone(&store), notify_tx, nudge_tx);
        let units = bench_plan(&pending, false).await;
        hook_walk(&pending, &HookSender::new(&pending, None), &store, &units)
            .await
            .expect_err("timed out");
        assert!(matches!(
            notify_rx.try_recv(),
            Ok(crate::alarms::NotifyJob::Activated(_))
        ));
        while nudge_rx.try_recv().is_ok() {}

        let high = threshold_param_key("temperature", "critical_high");
        draft(&pending, &high, PENDING_KIND_THRESHOLD, "33.0").await;
        assert!(open_warning(&pending).await.is_none());
        assert_eq!(
            pending
                .revert_to_held(HOST, NODE, &high, "30.0")
                .await
                .expect("revert"),
            Some(portal_storage::HeldRevert::Undelivered)
        );
        assert!(open_warning(&pending).await.is_some(), "open again");
        assert!(
            notify_rx.try_recv().is_err(),
            "neither the close nor the reopen mailed"
        );
        while let Ok(nudge) = nudge_rx.try_recv() {
            assert!(nudge.activated_ids.is_empty(), "and nothing popped up");
        }
    }

    // ---- PF-3b: a Discard takes back a delivered frame too --------------------------------

    /// A Discard whose frames were all still queued: they come back out through the Discard's
    /// own withdrawal (not the resend's, which keeps a delivered head), and the Node holds what
    /// it last confirmed.
    #[tokio::test]
    async fn a_discard_withdraws_through_its_own_call_and_keeps_the_node_state_when_nothing_left() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        let units = bench_plan(&pending, false).await;
        hook_walk(&pending, &HookSender::new(&pending, None), &store, &units)
            .await
            .expect_err("timed out");
        let rows = pending.list(HOST, NODE).await.expect("draft");
        let reverts = plan_revert(&rows, NODE, &held(&pending).await);
        let sender = HookSender::new(&pending, None);
        let done = run_discard(
            &pending,
            &sender,
            Some(&test_signing_key()),
            "boris",
            HOST,
            NODE,
            &reverts,
        )
        .await
        .expect("discarded");
        assert_eq!(done.withdrawn, 1);
        assert!(sender.log().contains(&"withdraw:60".to_string()));
        assert!(!sender.log().iter().any(|l| l.starts_with("drop:")));
        assert!(
            delivery(&pending).await.is_none(),
            "never confirmed, and the frame never reached the Node"
        );
        assert!(pending.discarded.lock().expect("lock").is_empty());
    }

    /// PF-3b on the bench: the central had delivered the 0x56 into a muted Node's RX1, so the
    /// Node may hold the discarded bound. The Discard takes the frame back all the same, and "on
    /// the Node" is unknown — not the last confirmation's time. When the Node then answers that
    /// frame, it holds another band than the Hub and the portal: `differs`, an open warning,
    /// and the portal's own bounds drafted again for one Send to put back.
    #[tokio::test]
    async fn a_discarded_frame_the_node_may_have_is_unknown_and_its_answer_differs() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        // The Node confirmed 25 once.
        let high = threshold_param_key("temperature", "critical_high");
        pending
            .delivery_confirmed(
                HOST,
                NODE,
                &[portal_storage::NodeBand {
                    field: "temperature".into(),
                    low: Some(-10.0),
                    high: Some(25.0),
                    enabled: true,
                }],
            )
            .await;
        let units = bench_plan(&pending, false).await;
        hook_walk(&pending, &HookSender::new(&pending, None), &store, &units)
            .await
            .expect_err("timed out");
        let rows = pending.list(HOST, NODE).await.expect("draft");
        let reverts = plan_revert(&rows, NODE, &held(&pending).await);
        let mut sender = HookSender::new(&pending, None);
        sender.maybe_delivered = true;
        run_discard(
            &pending,
            &sender,
            Some(&test_signing_key()),
            "boris",
            HOST,
            NODE,
            &reverts,
        )
        .await
        .expect("discarded");
        assert_eq!(
            temperature(&node_thresholds(&pending).await),
            (Some(22.0), Some(25.0)),
            "the critical bound goes back, the warning stays (amendment 2026-10-13)"
        );
        let record = delivery(&pending).await.expect("record");
        assert_eq!(record.state, portal_storage::DELIVERY_STATE_UNKNOWN);
        let cell = crate::render::node_delivery_ctx(
            &held(&pending).await,
            &["temperature".to_string()],
            &[record],
            false,
        );
        assert_eq!(cell["temperature"].state, "unknown_maybe");
        assert!(
            open_warning(&pending).await.is_none(),
            "nothing to act on yet"
        );

        // After the unmute the Node answers the discarded frame.
        assert!(
            pending
                .apply_late_answer(HOST, NODE, &radio_answer(60, "ok"))
                .await
        );
        let record = delivery(&pending).await.expect("record");
        assert_eq!(record.state, portal_storage::DELIVERY_STATE_DIFFERS);
        assert_eq!(
            record.confirmed_high,
            Some(30.0),
            "the Node holds the discarded 30"
        );
        let cell = crate::render::node_delivery_ctx(
            &held(&pending).await,
            &["temperature".to_string()],
            &[record],
            false,
        );
        assert_eq!(cell["temperature"].state, "differs");
        let warning = open_warning(&pending).await.expect("the operator must act");
        assert!(warning
            .message
            .as_deref()
            .is_some_and(|m| m.contains("discarded")));
        let rows = pending.list(HOST, NODE).await.expect("draft");
        assert_eq!(
            rows.iter()
                .map(|r| (
                    r.param_key.as_str(),
                    r.desired_value.as_deref(),
                    r.author.as_str()
                ))
                .collect::<Vec<_>>(),
            [
                (
                    threshold_param_key("temperature", "critical_high").as_str(),
                    Some("25.0"),
                    NODE_DIFFERS_AUTHOR
                ),
                (
                    threshold_param_key("temperature", "critical_low").as_str(),
                    Some("-10.0"),
                    NODE_DIFFERS_AUTHOR
                ),
            ],
            "the portal's own band, drafted for one Send"
        );
        let ctx = crate::render::pending_ctx(NODE, &rows, None, &BTreeMap::new(), true, now_unix());
        assert!(ctx.node_differs);

        // That Send puts the portal's band on the Node and closes the warning.
        let resend = plan_send(
            &rows,
            NODE,
            &held(&pending).await,
            &["temperature".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        let answering = HookSender::new(&pending, Some((Arc::clone(&store), "ok")));
        hook_walk(&pending, &answering, &store, &resend)
            .await
            .expect("confirmed");
        assert!(open_warning(&pending).await.is_none());
        assert_eq!(
            delivery(&pending)
                .await
                .map(|d| (d.state, d.confirmed_high)),
            Some((
                portal_storage::DELIVERY_STATE_CONFIRMED.to_string(),
                Some(25.0)
            ))
        );
        let _ = high;
    }

    /// PF-3b setup: the Node confirmed -10..25 once, a send of 30 timed out, and a Discard took
    /// back a frame the Node may have had — "on the Node" is unknown.
    async fn discard_maybe_delivered(pending: &PendingService, store: &Arc<StateStore>) {
        pending
            .delivery_confirmed(
                HOST,
                NODE,
                &[portal_storage::NodeBand {
                    field: "temperature".into(),
                    low: Some(-10.0),
                    high: Some(25.0),
                    enabled: true,
                }],
            )
            .await;
        let units = bench_plan(pending, false).await;
        hook_walk(pending, &HookSender::new(pending, None), store, &units)
            .await
            .expect_err("timed out");
        let rows = pending.list(HOST, NODE).await.expect("draft");
        let reverts = plan_revert(&rows, NODE, &held(pending).await);
        let mut sender = HookSender::new(pending, None);
        sender.maybe_delivered = true;
        run_discard(
            pending,
            &sender,
            Some(&test_signing_key()),
            "boris",
            HOST,
            NODE,
            &reverts,
        )
        .await
        .expect("discarded");
        assert_eq!(
            delivery(pending).await.map(|d| d.state),
            Some(portal_storage::DELIVERY_STATE_UNKNOWN.to_string())
        );
    }

    fn draft_of(rows: &[PendingConfigRow]) -> Vec<(String, Option<String>, String)> {
        rows.iter()
            .map(|r| {
                (
                    r.param_key.clone(),
                    r.desired_value.clone(),
                    r.author.clone(),
                )
            })
            .collect()
    }

    /// PF-3b review, blocker: after the Discard the operator drafts critical_high 28 and has not
    /// sent it. The late ok of the discarded frame must not put the portal's 25 over it: only
    /// the key with no row is drafted, and `differs` and the warning open all the same.
    #[tokio::test]
    async fn a_late_ok_of_a_discarded_frame_keeps_the_operators_new_draft() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        discard_maybe_delivered(&pending, &store).await;
        let high = threshold_param_key("temperature", "critical_high");
        let low = threshold_param_key("temperature", "critical_low");
        pending
            .upsert(NewPendingConfig {
                hostname: HOST.into(),
                node_key: NODE.into(),
                param_key: high.clone(),
                kind: PENDING_KIND_THRESHOLD.into(),
                desired_value: Some("28.0".into()),
                author: "boris".into(),
            })
            .await
            .expect("draft 28");

        assert!(
            pending
                .apply_late_answer(HOST, NODE, &radio_answer(60, "ok"))
                .await
        );
        assert_eq!(
            delivery(&pending).await.map(|d| d.state),
            Some(portal_storage::DELIVERY_STATE_DIFFERS.to_string())
        );
        assert!(open_warning(&pending).await.is_some());
        assert_eq!(
            draft_of(&pending.list(HOST, NODE).await.expect("draft")),
            [
                (high, Some("28.0".into()), "boris".into()),
                (low, Some("-10.0".into()), NODE_DIFFERS_AUTHOR.into()),
            ],
            "the operator's 28 stays; only the missing bound is drafted"
        );
    }

    /// PF-3b review, blocker: while a send claim or a Discard guard holds the Node, the late ok
    /// drafts nothing — a row written under a walk would stop it as superseded. `differs` and
    /// the warning still open.
    #[tokio::test]
    async fn a_late_ok_of_a_discarded_frame_drafts_nothing_while_the_node_is_busy() {
        for discarding in [false, true] {
            let (_dir, pending) = service();
            let store = Arc::new(StateStore::new());
            discard_maybe_delivered(&pending, &store).await;
            let claim = (!discarding).then(|| pending.claim_send(HOST, NODE).expect("claim"));
            let guard = discarding.then(|| pending.begin_discard(HOST, NODE).expect("guard"));

            assert!(
                pending
                    .apply_late_answer(HOST, NODE, &radio_answer(60, "ok"))
                    .await
            );
            assert_eq!(
                delivery(&pending).await.map(|d| d.state),
                Some(portal_storage::DELIVERY_STATE_DIFFERS.to_string()),
                "discarding={discarding}"
            );
            assert!(open_warning(&pending).await.is_some());
            assert!(
                pending.list(HOST, NODE).await.expect("draft").is_empty(),
                "nothing drafted under a claim or a guard (discarding={discarding})"
            );
            drop((claim, guard));
        }
    }

    /// PF-3b: a refusal of the discarded frame means the Node kept what it had: the band goes
    /// back to its last confirmation, and nothing is drafted.
    #[tokio::test]
    async fn a_refused_discarded_frame_restores_the_last_confirmation() {
        let (_dir, pending) = service();
        let store = Arc::new(StateStore::new());
        pending
            .delivery_confirmed(
                HOST,
                NODE,
                &[portal_storage::NodeBand {
                    field: "temperature".into(),
                    low: Some(-10.0),
                    high: Some(25.0),
                    enabled: true,
                }],
            )
            .await;
        let units = bench_plan(&pending, false).await;
        hook_walk(&pending, &HookSender::new(&pending, None), &store, &units)
            .await
            .expect_err("timed out");
        let rows = pending.list(HOST, NODE).await.expect("draft");
        let reverts = plan_revert(&rows, NODE, &held(&pending).await);
        let mut sender = HookSender::new(&pending, None);
        sender.maybe_delivered = true;
        run_discard(
            &pending,
            &sender,
            Some(&test_signing_key()),
            "boris",
            HOST,
            NODE,
            &reverts,
        )
        .await
        .expect("discarded");
        assert!(
            pending
                .apply_late_answer(HOST, NODE, &radio_answer(60, "error"))
                .await
        );
        assert_eq!(
            delivery(&pending)
                .await
                .map(|d| (d.state, d.confirmed_high)),
            Some((
                portal_storage::DELIVERY_STATE_CONFIRMED.to_string(),
                Some(25.0)
            ))
        );
        assert!(pending.list(HOST, NODE).await.expect("draft").is_empty());
        assert!(open_warning(&pending).await.is_none());
    }

    /// The memory for a late ack: a new Send and a Discard forget it, a day ends it, and a unit
    /// of several frames is finished by the ok for its last one.
    #[tokio::test]
    async fn the_late_ack_memory_is_forgotten_and_counts_every_frame() {
        let (_dir, pending) = service();
        let late = |unanswered: Vec<u64>| LateUnit {
            hostname: HOST.to_ascii_uppercase(),
            node_key: NODE.into(),
            label: NODE_CONFIG_UNIT.into(),
            seqs: vec![4, 5, 6],
            unanswered,
            rows: Vec::new(),
            alarm_layout: Some(vec!["temperature".into()]),
            node_bands: Vec::new(),
            given_up_at: std::time::Instant::now(),
        };
        pending.remember_late(late(vec![5, 6]));
        let now = std::time::Instant::now();
        assert!(pending.take_late_answer(HOST, NODE, 5, true, now).is_none());
        assert!(pending
            .take_late_answer(HOST, NODE, 99, true, now)
            .is_none());
        assert!(pending
            .take_late_answer(HOST, NODE, 6, true, now)
            .is_some_and(|u| u.unanswered.is_empty()));

        // A new Send keeps it (PF-3 review, item 5): a frame already delivered may still be
        // answered. The walk forgets it once every earlier frame came back out of the queue.
        pending.remember_late(late(vec![7]));
        let claim = pending.claim_send(HOST, NODE).expect("claim");
        assert!(pending.forget_late(HOST, NODE), "the claim alone keeps it");
        drop(claim);

        pending.remember_late(late(vec![7]));
        pending.discard(HOST, NODE).await.expect("discard");
        assert!(!pending.forget_late(HOST, NODE), "the Discard forgot it");

        pending.remember_late(late(vec![7]));
        let tomorrow = now + LATE_ANSWER_WINDOW + Duration::from_secs(1);
        assert!(
            !pending
                .apply_late_answer_at(HOST, NODE, &radio_answer(7, "ok"), tomorrow)
                .await
        );
        assert!(!pending.forget_late(HOST, NODE), "a day ended it");
    }

    /// The thresholds table's "on the Node": the confirmed band with its time, not delivered,
    /// on its way, another band, never seen, and a warning-only band that never lives there.
    #[test]
    fn the_thresholds_table_says_whether_a_band_is_on_the_node() {
        let mut thresholds = current_temperature();
        thresholds.insert(
            "humidity".into(),
            ThresholdEntryCtx {
                critical_low: None,
                warning_low: None,
                warning_high: Some(80.0),
                critical_high: None,
                enabled: true,
                hysteresis: None,
            },
        );
        let layout = ["temperature".to_string(), "humidity".to_string()];
        let record = |state: &str, high: f64| portal_storage::NodeThresholdDelivery {
            hostname: HOST.into(),
            node_key: NODE.into(),
            field: "temperature".into(),
            confirmed_low: Some(-10.0),
            confirmed_high: Some(high),
            confirmed_enabled: Some(true),
            confirmed_at: Some(1_790_000_000),
            state: state.into(),
            updated_at: 0,
            sent_low: None,
            sent_high: None,
            sent_enabled: None,
        };
        let cell = |delivery: &[portal_storage::NodeThresholdDelivery]| {
            crate::render::node_delivery_ctx(&thresholds, &layout, delivery, false)
        };
        let yes = cell(&[record(portal_storage::DELIVERY_STATE_CONFIRMED, 25.0)]);
        assert_eq!(yes["temperature"].state, "yes");
        assert!(yes["temperature"].at.is_some());
        assert_eq!(
            yes["humidity"].state, "hub_only",
            "warning bounds never live there"
        );
        for (state, want) in [
            (portal_storage::DELIVERY_STATE_UNDELIVERED, "undelivered"),
            (portal_storage::DELIVERY_STATE_PENDING, "pending"),
        ] {
            assert_eq!(cell(&[record(state, 30.0)])["temperature"].state, want);
        }
        assert_eq!(
            cell(&[record(portal_storage::DELIVERY_STATE_CONFIRMED, 30.0)])["temperature"].state,
            "no",
            "the Node holds another band"
        );
        assert_eq!(cell(&[])["temperature"].state, "unknown");
    }
}
