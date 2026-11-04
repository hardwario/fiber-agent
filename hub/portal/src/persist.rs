//! Persistence bridge: maps a stream of [`IngestEvent`]s onto the
//! `portal-storage` repositories from a dedicated blocking thread
//! (`rusqlite::Connection` is `!Sync`; the DB has exactly one writer).
//!
//! Mapping rules ported from v1 (plan/v1/viewer/mqtt.md):
//! - `Probe1m` → `readings_minute` upsert (the authoritative hot tier).
//!   All rows are `SOURCE_LIVE` in M1 — the replay stream is not routed yet.
//!   Ledger dedup by `message_id` is skipped for probe_1m: the minute-slot
//!   primary key makes the upsert idempotent on its own.
//! - `SensorsAggregated` → `readings_raw` insert per sensor, only when
//!   `sample_count > 0 && avg_celsius.is_some()`, and each line through
//!   `portal_domain::on_wired_snapshot`, which clears a HIGH/LOW row whose
//!   NORMAL transition never arrived (`plan/decisions.md` #27).
//!
//! M6-E: the alarm *engine* (`portal_domain::alarm_engine`) now drives every
//! alarm-shaped event (`AlarmEvent`, `SystemInfo` connectivity leaves,
//! `LorawanSensors`/`EyeSensors` field states + native fPort-3 events) —
//! replacing M1's bare `AlarmEvent`-to-`alarm_events` append. See
//! `apply_alarm_actions` for the shared action-application/notify-job/nudge
//! plumbing every one of those call sites funnels through.
//!
//! M5-E additions (Node/EYE):
//! - `LorawanSensors` → per Node, gated on `last_seen` actually advancing
//!   (`lorawan_record_on_uplink_only`: `None → Some` counts as an advance,
//!   an unchanged value skips the whole sensor): `node_raw_messages`
//!   append, `node_field_readings`/`node_counter_readings` batch insert,
//!   `node_events` insert per event carrying a parseable `ts`, and — when
//!   the payload carries `field_thresholds` (authoritative) —
//!   `upsert_field_threshold` per field with actor `firmware-echo` — the
//!   four bounds only; the stored `enabled` is read back and preserved,
//!   because that flag is the portal's own switch and the echo has no
//!   opinion about it (§21 review, item 18). The
//!   "last persisted `last_seen` per Node" gate lives in a plain
//!   `HashMap` owned by this module's spawned thread; since that thread is
//!   the DB's only writer and processes events strictly one at a time, an
//!   ordinary local variable *is* thread-local state here — no
//!   `std::thread_local!` needed.
//! - `StickerHistory` → `node_field_readings`/`node_counter_readings`
//!   insert from the frame's records (source `live` — v1 parity: the
//!   natural-key PK dedups a replayed frame regardless), then credits any
//!   active backfill job for this `(hostname, node_key)` via
//!   `credit_frames`, and marks the job `done` when the frame is the last
//!   one. **Ownership split**: the backfill *service* (elsewhere) owns job
//!   creation, requesting history from the device, and retries; this sink
//!   owns row persistence + frame crediting + marking done, simply because
//!   the DB connection lives on this thread.
//! - `EyeSensors` → the alarm engine and default bands only; no history row
//!   (storage_intervals.md §6: the snapshot is the live view).
//! - `EyeAggregated` → one `eye_readings` row per Beacon per Hub storage
//!   window, `event_type = 'advertising'`, the window average in the value
//!   columns, deduped on the Hub's `{MAC}-adv-{window_start}` `message_id`.
//! - `StickerConfig`/`StickerFullConfig`/`StickerInfo`/
//!   `StickerCommandResult`, and the v2 `NodeInventory`/`NodeDeviceInfo`/
//!   `NodeDeviceConfig` → no persistent home (state-only /
//!   SSE passthrough), explicit no-op arms below.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::{broadcast, mpsc};

use fiber_protocol::fields::eye::{eye_field, eye_registry};
use fiber_protocol::fields::lorawan::lorawan_field;
use fiber_protocol::payloads::alarm_event::{AlarmEventMsg, AlarmType as WireAlarmTypeHint};
use fiber_protocol::payloads::eye::EyeTag;
use fiber_protocol::payloads::eye_aggregated::EyeAggregatedMsg;
use fiber_protocol::payloads::lorawan::{
    LorawanEvent, LorawanSensor, LorawanSensorsMsg, NodeHistoryMsg, NodeHistoryRecord,
};
use fiber_protocol::payloads::sensors_aggregated::AggregatedSensor;
use fiber_protocol::payloads::status::DeviceStatus;
use fiber_protocol::payloads::system_info::SystemInfoMsg;
use portal_domain::{
    ActiveAlarm, ActiveLookup, AlarmAction, AlarmSource as DomainAlarmSource, AlarmType,
    ConnectivityKind, ConnectivitySnapshot, Edge, FieldSource, FieldStates, NativeEvent, NewAlarm,
    NotifyKind as EngineNotifyKind, NotifyPayload, Severity, ThresholdBounds, WireState,
    WiredTransition,
};
use portal_storage::{
    AlarmEvent as AlarmEventRow, AlarmSeverityChange, EyeReading, EyeWindowRange, Landed,
    MinuteReading, NewAlarmEvent, NewFieldThreshold, NodeCounterReading, NodeEvent,
    NodeFieldReading, RawReading, SpillGate, SpillKind, Storage, JOB_DONE, JOB_RECEIVING,
    JOB_REQUESTED, SOURCE_LIVE, SOURCE_REPLAY, THRESHOLD_SCOPE_EYE, THRESHOLD_SCOPE_HUB,
    THRESHOLD_SCOPE_NODE,
};

use crate::alarms::notifier::{source_from_sensor_id, AlarmNotifyData, NotifyJob};
use crate::alarms::AlarmNudge;
use crate::durable;
use crate::mqtt::routing;
use crate::notify::checkers::SystemSnapshot;
use crate::notify::payload::AckInfo;
use crate::state::{IngestEvent, NodeReadingMsg};

/// Cache key for the `last_seen` gate: `(hostname, node_key)`.
type NodeKey = (String, String);

/// Cached wired-sensor identity/thresholds from the most recent
/// `config/state` (the authoritative source — `AlarmEventMsg` only ever
/// carries `name`, never `location`/thresholds).
#[derive(Debug, Clone, Default)]
struct WiredMeta {
    name: Option<String>,
    location: Option<String>,
    thresholds: Option<ThresholdBounds>,
}

/// Per-hostname merged "last known" telemetry, built incrementally from
/// `system/info` publishes (which may each carry only a subset of leaves —
/// same keep-old-on-null reasoning as `state::store`'s `merge` helper) so
/// the notifier's system-alert checker always evaluates against a coherent
/// snapshot rather than whatever one publish happened to include.
#[derive(Debug, Clone, Default)]
struct DeviceSnapshotCache {
    wifi_connected: Option<bool>,
    ethernet_connected: Option<bool>,
    has_internet: Option<bool>,
    battery_pct: Option<f64>,
    storage_used_pct: Option<f64>,
    version: Option<String>,
}

/// How the persist thread learns a Hub's canonical hostname spelling — the one
/// `DeviceState::hostname` holds and every page queries storage with.
///
/// One Hub publishes under two spellings: the v1 agent's `FIBER-4E970F4A` and
/// control-radio's lowercase `fiber-4e970f4a` on `nodes/…`. The store folds them
/// into one Hub and keeps the agent's spelling (#94), but the v2 radio events
/// carry the lowercase one from their topic, and storage matches hostnames
/// exactly. Written as they arrive, a v2 Node's readings, raw messages and
/// alarms landed where no page looked: an empty chart, an empty message ring and
/// no "last alarm" on the Node detail (bench, 2026-09-23). `None` from the
/// resolver means the store does not know the Hub yet, and the spelling is kept.
pub type HostnameCanon = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Where a v2 Node's graded field states go (nodes_phase2.cs.md, P-9 / N32): the store, so the
/// value axis the pages read is the one the alarms were raised on. `(hostname, node_key,
/// field → NORMAL/WARNING/CRITICAL)`.
pub type FieldStatesSink =
    Arc<dyn Fn(&str, &str, &std::collections::BTreeMap<String, String>) + Send + Sync>;

/// All state this thread keeps across events — a plain struct (rather than
/// several loose `HashMap`s) now that M6-E adds four more caches alongside
/// M5-E's `node_last_seen`.
#[derive(Default)]
pub(crate) struct PersistCache {
    /// See [`HostnameCanon`]. `None` in tests that exercise one spelling only.
    hostnames: Option<HostnameCanon>,
    /// See [`FieldStatesSink`]. `None` where nothing renders (tests).
    field_states: Option<FieldStatesSink>,
    /// The portal's own data directory, whose fill level feeds the "Portal storage low"
    /// system alert (HN-18). `None` in tests.
    data_dir: Option<PathBuf>,
    /// The Hub the portal runs on, lowercase: the only one whose snapshot carries
    /// `data_dir`'s fill level. `None` (tests) gives it to every Hub.
    local_hostname: Option<String>,
    node_last_seen: HashMap<NodeKey, String>,
    /// WP-E: recently-seen `NodeReading.message_id`s per node, newest last.
    ///
    /// The v2 envelope carries its own dedup identifier (ChirpStack's `deduplicationId` when
    /// there is one — so every gateway's copy of a frame shares it by construction), which is
    /// a stronger key than the v1 path's "did `last_seen` advance". A short ring rather than a
    /// single last value: a retransmit does not always arrive adjacent to its original, and an
    /// unbounded set on a long-running writer thread would grow without limit.
    node_message_ids: HashMap<NodeKey, std::collections::VecDeque<String>>,
    wired_meta: HashMap<(String, i64), WiredMeta>,
    node_location: HashMap<NodeKey, Option<String>>,
    eye_location: HashMap<(String, String), Option<String>>,
    /// When a Hub's radio component was first seen down, keyed by `(hostname, component)`
    /// — the five-minute debounce of the radio-diagnostics alarms (§4, §4a).
    radio_down_since: HashMap<(String, &'static str), i64>,
    /// The newest `activate` of each native alarm slot, keyed by `(hostname lowercased, node_key,
    /// slot, source)`, as [`persist_node_reading`] saw it. [`reraised_since`] reads it first:
    /// until V27 `node_events` keyed a row by `(event_type, ts)`, so of an alarm page that
    /// carries several events only the first was stored (bench 2026-09-28, E6: six slots on two
    /// pages). V27's `seq` keeps every row now; this stays as a second guard, since it costs
    /// nothing and covers a database migrated with only the first rows (devices.backend.md §23).
    native_activations: HashMap<(String, String, i64, Option<String>), i64>,
    system_snapshot: HashMap<String, DeviceSnapshotCache>,
    online: HashMap<String, bool>,
    /// Each Node's `application.interval_report` from its last config document, which sizes
    /// the restart grace window ([`restart_grace_s`]).
    node_interval_report: HashMap<NodeKey, i64>,
    /// Restart closes waiting out their grace window (node_reported_state.md §4, amended
    /// 2026-09-27), one per Node: a newer boot replaces an older one, whose rows it covers.
    restart_closes: HashMap<NodeKey, PendingRestartClose>,
    /// The sensor fields whose default bands this process has already offered
    /// (plan/portal/threshold_defaults.md §3), keyed by `(hostname lowercased, scope,
    /// entity, field)`: a field is looked up once, not on every reading. A removed sensor
    /// leaves the set, so one registered again at its key is offered them again.
    defaults_offered: std::collections::HashSet<(String, &'static str, String, String)>,
    /// Where a Beacon archive job's change is announced for the History tab
    /// (`crate::beacon_history`). `None` in tests that do not watch it.
    beacon_history_nudge: Option<broadcast::Sender<crate::beacon_history::BeaconHistoryNudge>>,
    /// Per archive job id: the oldest and newest record `ts` seen and the
    /// `interval_s` the batches gave — the window and tolerance its `done`
    /// cross-check uses (§7 Q6). In-memory: after a restart the job's own
    /// window and the rows' spacing stand in.
    beacon_history_spans: HashMap<String, (i64, i64, Option<i64>)>,
    /// The cluster's Beacon registry and home Hubs, from the Server's retained `eye/homes`
    /// (decision #42). `None` until one arrives: every Beacon is then its own Hub's, as before.
    beacon_cluster: Option<crate::state::models::BeaconCluster>,
}

/// Where one Beacon's data from one Hub goes (decision #42).
#[derive(Debug, Clone, PartialEq, Eq)]
enum BeaconSource {
    /// No `eye/homes` yet: keyed by the Hub that sent it, as before #42.
    Legacy,
    /// A registered Beacon from its home Hub (or from any Hub while no home is known): stored
    /// once, under the registry's key.
    Home { registry: String },
    /// A registered Beacon from a Hub that is not its home: not stored (Q1, no fallback).
    NotHome,
    /// A Beacon the cluster has not registered: no history; its alarms close (registry key).
    Stranger { registry: String },
}

/// An Info that arrived within the grace window after a boot, held until the window has passed
/// so a condition the Node still holds is re-raised onto its open row rather than closed and
/// opened again.
#[derive(Debug, Clone)]
struct PendingRestartClose {
    doc: crate::state::NodeDeviceInfoMsg,
    booted: i64,
    due: i64,
}

impl PersistCache {
    /// Where `mac`'s data from `hub` goes — see [`BeaconSource`].
    fn beacon_source(&self, hub: &str, mac: &str) -> BeaconSource {
        let Some(cluster) = &self.beacon_cluster else {
            return BeaconSource::Legacy;
        };
        let registry = self.canonical_hostname(&cluster.server);
        if !cluster.is_registered(mac) {
            return BeaconSource::Stranger { registry };
        }
        match cluster.home_of(mac) {
            Some(home) if !home.eq_ignore_ascii_case(hub.trim()) => BeaconSource::NotHome,
            _ => BeaconSource::Home { registry },
        }
    }

    /// The hostname a Beacon archive job and its rows are keyed by: the registry in a cluster.
    fn beacon_registry_or(&self, hub: &str) -> String {
        self.beacon_cluster
            .as_ref()
            .map(|c| self.canonical_hostname(&c.server))
            .unwrap_or_else(|| hub.to_string())
    }

    /// Whether `hostname` is the Hub this portal runs on (or the portal does not know).
    fn runs_portal(&self, hostname: &str) -> bool {
        self.local_hostname
            .as_deref()
            .is_none_or(|local| local.eq_ignore_ascii_case(hostname.trim()))
    }

    /// The spelling storage keys this Hub under — see [`HostnameCanon`].
    fn canonical_hostname(&self, hostname: &str) -> String {
        self.hostnames
            .as_ref()
            .and_then(|canon| canon(hostname))
            .unwrap_or_else(|| hostname.to_string())
    }
}

/// Opens the database under `data_dir` and spawns the writer thread.
/// Returns the sender the MQTT ingest loop clones events into; dropping
/// every sender ends the thread cleanly.
///
/// `notify_tx`/`nudge_tx` are the M6-E alarm-engine outputs: every
/// `AlarmAction::Notify` the engine returns becomes a [`NotifyJob`] pushed
/// onto `notify_tx` (consumed by `alarms::notifier::spawn_notifier`), and
/// every batch of engine actions that actually wrote a row broadcasts an
/// [`AlarmNudge`] on `nudge_tx` (the same sender `AlarmService` hands out
/// via `subscribe_nudges`, so `sse.rs` re-renders off one shared channel).
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    data_dir: &str,
    db_key: Option<&str>,
    topic_prefix: String,
    spill: Arc<SpillGate>,
    notify_tx: mpsc::UnboundedSender<NotifyJob>,
    nudge_tx: broadcast::Sender<AlarmNudge>,
    hostnames: Option<HostnameCanon>,
    field_states: Option<FieldStatesSink>,
    beacon_history_nudge: Option<broadcast::Sender<crate::beacon_history::BeaconHistoryNudge>>,
    local_hostname: Option<String>,
) -> Result<mpsc::UnboundedSender<PersistMsg>, portal_storage::StorageError> {
    std::fs::create_dir_all(data_dir)?;
    let db_path = Path::new(data_dir).join("portal.db");
    let storage = Storage::open(&db_path, db_key)?;
    tracing::info!(path = %db_path.display(), encrypted = db_key.is_some(), "storage open");

    let data_dir_owned = data_dir.to_string();
    let (tx, mut rx) = mpsc::unbounded_channel::<PersistMsg>();
    std::thread::Builder::new()
        .name("portal-persist".into())
        .spawn(move || {
            let mut cache = PersistCache {
                hostnames,
                field_states,
                data_dir: Some(PathBuf::from(&data_dir_owned)),
                beacon_history_nudge,
                local_hostname,
                ..PersistCache::default()
            };

            // A journal left behind by a process that died mid-rewrite.
            // `SpillGate::open` has already raised the gate if there is
            // anything in it, so every other writer in the process is spilling
            // rather than writing directly while this runs — that is what makes
            // "replay before any newer work" true for them too, not just for
            // this thread's own FIFO.
            drain_until_empty(
                &storage,
                &spill,
                &topic_prefix,
                &mut cache,
                &notify_tx,
                &nudge_tx,
                "startup",
            );
            spill.resume_direct();

            // The battery's bands from before it stopped being a sensor
            // (plan/portal/threshold_defaults.md §5): deleted before the first event is
            // graded. A failure is logged and changes nothing else — the engine grades none
            // of them anyway (`threshold_defaults::node_field_is_graded`).
            if let Err(err) = crate::threshold_defaults::forget_battery_thresholds(&storage) {
                tracing::warn!(%err, "the battery's old thresholds were not deleted");
            }

            let mut spilled_run = 0u64;

            while let Some(msg) = rx.blocking_recv() {
                report_backlog(rx.len());

                match msg {
                    PersistMsg::Publish {
                        topic,
                        payload,
                        event,
                    } => {
                        let encoded = durable::encode_publish(&topic, &payload);
                        let landed = spill.write_or_spill(SpillKind::Mqtt, &encoded, || {
                            handle(
                                &storage,
                                (*event).clone(),
                                &mut cache,
                                &notify_tx,
                                &nudge_tx,
                            )
                        });

                        match landed {
                            Ok(Landed::Committed) => {}
                            Ok(Landed::Spilled(_)) => {
                                spilled_run += 1;
                                report_spill_depth(&spill, spilled_run);
                            }
                            // Both the database and the journal refused it.
                            // `write_or_spill` has already logged the detail at
                            // `error!`; this names the message that was at
                            // risk so the topic is in the same log line.
                            Err(err) => tracing::error!(
                                %err, topic,
                                "ingest event could not be persisted or spilled"
                            ),
                        }
                    }

                    PersistMsg::ReplaySpill { owns_gate } => {
                        let reason = if owns_gate {
                            // The gate is already up — the rewrite raised it
                            // when it claimed the run. `begin_replaying`
                            // records that the backlog is now draining.
                            spill.begin_replaying();
                            "post-maintenance"
                        } else {
                            "stray"
                        };
                        drain_until_empty(
                            &storage,
                            &spill,
                            &topic_prefix,
                            &mut cache,
                            &notify_tx,
                            &nudge_tx,
                            reason,
                        );
                        if owns_gate {
                            // Ordered *after* the last replayed row commits: a
                            // direct write must never overtake the backlog.
                            spill.resume_direct();
                            spilled_run = 0;
                        }
                        // A stray replay resolves nothing: it did not raise the
                        // gate, so it leaves whatever phase it found. See
                        // `PersistMsg::ReplaySpill`.
                    }
                }
            }

            tracing::info!("persistence thread stopping (channel closed)");
        })
        .map_err(portal_storage::StorageError::Io)?;

    Ok(tx)
}

/// Depth past which a queue is worth saying out loud. A stuck run should read
/// as "N events waiting", not as silence.
const BACKLOG_LOG_EVERY: u64 = 1_000;

fn report_backlog(queued: usize) {
    if queued > 0 && (queued as u64).is_multiple_of(BACKLOG_LOG_EVERY) {
        tracing::warn!(queued, "persist channel is backing up");
    }
}

fn report_spill_depth(spill: &SpillGate, spilled_run: u64) {
    if !spilled_run.is_multiple_of(BACKLOG_LOG_EVERY) {
        return;
    }
    match spill.depth() {
        Ok(depth) => tracing::warn!(depth, "events waiting in the spill journal for replay"),
        Err(err) => tracing::warn!(%err, "could not measure spill depth"),
    }
}

/// Drain until the journal is genuinely empty, then let the caller lower the
/// gate.
///
/// One [`drain_spill`] pass is not enough on its own: `SpillGate::drain` is
/// bounded by the `MAX(id)` it snapshots when it starts, so a writer that
/// spilled while it ran appended *above* that ceiling and was not replayed.
/// Without the re-check that row would sit in the journal until the next
/// maintenance run or the next restart. Bounded in practice by the fact that
/// spilling stops as soon as the gate comes down, which is the statement
/// immediately after this returns.
#[allow(clippy::too_many_arguments)]
fn drain_until_empty(
    storage: &Storage,
    spill: &SpillGate,
    topic_prefix: &str,
    cache: &mut PersistCache,
    notify_tx: &mpsc::UnboundedSender<NotifyJob>,
    nudge_tx: &broadcast::Sender<AlarmNudge>,
    reason: &str,
) {
    loop {
        let replayed = drain_spill(
            storage,
            spill,
            topic_prefix,
            cache,
            notify_tx,
            nudge_tx,
            reason,
        );
        match spill.depth() {
            Ok(0) => return,
            // Rows are still there. Go round again only if the last pass
            // actually moved some: a pass that replayed nothing and left a
            // non-empty journal has hit a row it cannot apply, and retrying it
            // immediately would spin forever. It stays queued — durable, and
            // retried by the next drain — which is the documented contract.
            Ok(remaining) => {
                if replayed == 0 {
                    tracing::warn!(
                        remaining,
                        reason,
                        "spill replay made no progress; rows stay queued for the next drain"
                    );
                    return;
                }
            }
            // A journal we cannot even measure will not drain either;
            // `drain_spill` has already logged why. Stop rather than spin.
            Err(err) => {
                tracing::warn!(%err, reason, "could not confirm the spill journal is empty");
                return;
            }
        }
    }
}

/// Replay the spill journal into the main database, in acceptance order.
///
/// Failures are logged rather than propagated: the rows stay in the journal
/// (see [`portal_storage::SpillJournal::drain`]) and the next drain — the next
/// maintenance run, or the next startup — retries them. Losing them is the one
/// outcome this whole path exists to prevent, so nothing here deletes a row it
/// could not apply.
#[allow(clippy::too_many_arguments)]
fn drain_spill(
    storage: &Storage,
    spill: &SpillGate,
    topic_prefix: &str,
    cache: &mut PersistCache,
    notify_tx: &mpsc::UnboundedSender<NotifyJob>,
    nudge_tx: &broadcast::Sender<AlarmNudge>,
    reason: &str,
) -> usize {
    /// Rows per SELECT, and per acquisition of the journal's mutex —
    /// `SpillGate::drain` hands the lock back between batches. Large enough
    /// that a long backlog is not thousands of round-trips, small enough that a
    /// writer forced to spill mid-replay waits one batch rather than the whole
    /// backlog.
    const BATCH: usize = 256;

    let started = std::time::Instant::now();

    let result = spill.drain(BATCH, |row| match row.kind {
        SpillKind::Mqtt => {
            let Some((topic, payload)) = durable::decode_publish(&row.payload) else {
                // Refusing rather than skipping: a row we cannot decode is a
                // row we would otherwise silently drop.
                return Err(portal_storage::StorageError::Conflict(format!(
                    "spill row {} is not a decodable publish",
                    row.id
                )));
            };
            match routing::route_publish(&topic, &payload, topic_prefix) {
                routing::RouteResult::Event(event) => {
                    handle(storage, *event, cache, notify_tx, nudge_tx)
                }
                // The same non-persisting outcomes the live path has. Nothing
                // to write, so the row is done.
                _ => Ok(()),
            }
        }
        SpillKind::Audit | SpillKind::ReportHistory => {
            durable::replay_row(storage, row.kind, &row.payload)
        }
        // Replayed here rather than in `durable::replay_row` for the same
        // reason the ingest kind is: the side effect needs the nudge sender.
        // `apply_liveness` re-decides against the *current* open alarm, so a
        // row replayed twice is a no-op the second time.
        SpillKind::Liveness => {
            let edge: durable::LivenessEdge =
                serde_json::from_slice(&row.payload).map_err(portal_storage::StorageError::from)?;
            if durable::apply_liveness(storage, &edge)? {
                // The SSE re-render the live path would have sent, sent now
                // instead — delayed, not dropped (sqlite-vacuum.md §3.3).
                let _ = nudge_tx.send(AlarmNudge {
                    hostname: edge.hostname,
                    activated_ids: Vec::new(),
                    escalated_ids: Vec::new(),
                });
            }
            Ok(())
        }
    });

    match result {
        Ok(0) => 0,
        Ok(replayed) => {
            tracing::info!(
                replayed,
                reason,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "spill journal replayed"
            );
            replayed
        }
        Err(err) => {
            tracing::error!(
                %err, reason,
                "spill replay stopped; the remaining rows stay queued for the next drain"
            );
            // `drain` deletes each row only once it has committed, so whatever
            // it managed before the failure is already gone from the journal.
            // Reported as no progress on purpose: the *next* row is the one
            // that is stuck, and retrying it in a tight loop would spin.
            0
        }
    }
}

/// One unit of work for the persist thread.
pub enum PersistMsg {
    /// A routed MQTT publish.
    ///
    /// Carries the broker's raw `(topic, payload)` alongside the parsed event:
    /// a write that has to be deferred is spilled as the original bytes and
    /// replayed through [`routing::route_publish`], so there is one decoder in
    /// the tree rather than a serialised-event format to keep in step with the
    /// `fiber-protocol` / `control_radio` wire types.
    Publish {
        topic: String,
        payload: Vec<u8>,
        event: Box<IngestEvent>,
    },
    /// Drain the spill journal.
    ///
    /// `owns_gate` says whether the sender is the rewrite that raised the gate,
    /// and therefore whether this replay is allowed to lower it again.
    ///
    /// - `true` — the maintenance task, whose rewrite has just finished. It
    ///   raised the gate when it claimed the run, so lowering it here is the
    ///   second half of that protocol.
    /// - `false` — [`crate::retention::RetentionTask::kick_a_stray_replay`],
    ///   which asks for a drain of rows that reached the journal outside a
    ///   maintenance run. It never raised anything, so it must not resolve a
    ///   phase it does not own: `kick_a_stray_replay` and `maintenance_tick`
    ///   are adjacent statements in the sweep loop, and with any backlog on
    ///   this channel a stray message is dequeued *after* the rewrite raised
    ///   the gate. Lowering it there would put every writer back on the
    ///   locked database for the rest of the run — four attempts against a
    ///   30 s `busy_timeout` before each one finally spills.
    ReplaySpill { owns_gate: bool },
}

fn handle(
    storage: &Storage,
    event: IngestEvent,
    cache: &mut PersistCache,
    notify_tx: &mpsc::UnboundedSender<NotifyJob>,
    nudge_tx: &broadcast::Sender<AlarmNudge>,
) -> Result<(), portal_storage::StorageError> {
    // Events arrive every few seconds, which is timer enough for a window of minutes, and a
    // failed close must not cost the event its own write.
    if let Err(err) =
        run_due_restart_closes(storage, cache, chrono_free_now_unix(), notify_tx, nudge_tx)
    {
        tracing::warn!(%err, "deferred restart close of native alarms failed");
    }
    match event {
        IngestEvent::Probe1m {
            hostname,
            line,
            payload,
        } => {
            let Some(minute_ts) = payload.minute_ts else {
                tracing::debug!(%hostname, line, "probe_1m without minute_ts — skipped");
                return Ok(());
            };
            // HS-2 (Hub sweep 2026-09-26): a disconnected line's minute arrives with no
            // samples and 0.0 °C, which the chart and the CSV drew as a real 0 °C. No sample,
            // no temperature: the row stays (its disconnect count and worst alarm are real),
            // the values are NULL, like the raw tier below. The Hub's `sample_count` counts
            // the disconnected samples too (HIL 2026-10-12: every disconnected minute came in
            // as 8 samples, 8 disconnects, 0.0 °C), so what is measured is the difference.
            let no_samples = payload
                .sample_count
                .is_some_and(|n| n <= payload.disconnect_count.unwrap_or(0));
            let measured = |v: Option<f64>| if no_samples { None } else { v };
            storage.upsert_minute(&MinuteReading {
                hostname,
                sensor_line: payload.sensor_line.unwrap_or(i64::from(line)),
                minute_ts,
                min_c: measured(payload.min_c),
                avg_c: measured(payload.avg_c),
                max_c: measured(payload.max_c),
                sample_count: payload.sample_count,
                disconnect_count: payload.disconnect_count,
                worst_alarm: payload.worst_alarm,
                source: SOURCE_LIVE.to_string(),
                data_hmac: payload.data_hmac,
            })
        }
        IngestEvent::SensorsAggregated { hostname, payload } => {
            let ts = payload.period_end_ts.unwrap_or_else(chrono_free_now_unix);
            for sensor in payload.sensors {
                let connected = sensor.sample_count > 0;
                let avg = sensor.temperature.as_ref().and_then(|t| t.avg_celsius);
                if connected && avg.is_some() {
                    let temp = sensor.temperature.as_ref();
                    storage.insert_raw(&RawReading {
                        hostname: hostname.clone(),
                        sensor_line: sensor.line,
                        ts,
                        min_c: temp.and_then(|t| t.min_celsius),
                        avg_c: avg,
                        max_c: temp.and_then(|t| t.max_celsius),
                        sample_count: Some(sensor.sample_count),
                    })?;
                }
                // Decision #41: the portal grades the line itself from the live sample,
                // so the window's state no longer closes a HIGH/LOW row (#27's reconcile
                // would close one the portal's hysteresis still holds).
            }
            Ok(())
        }

        // The live sample, one per line per cycle: the portal grades each line's value
        // against the line's bounds and hysteresis (decision #41). Nothing is stored;
        // `sensors/aggregated` is the stored reading.
        IngestEvent::SensorsLive { hostname, payload } => {
            let ts_iso = payload
                .period_end_ts
                .or(payload.period_start_ts)
                .map(unix_to_iso)
                .unwrap_or_else(iso_now);
            for sensor in payload.sensors {
                grade_wired_line(
                    storage, cache, &hostname, &sensor, &ts_iso, notify_tx, nudge_tx,
                )?;
            }
            Ok(())
        }

        IngestEvent::AlarmEvent { hostname, payload } => {
            handle_wired_alarm(storage, cache, &hostname, payload, notify_tx, nudge_tx)
        }

        IngestEvent::SystemInfo { hostname, payload } => {
            handle_system_info(storage, cache, &hostname, *payload, notify_tx, nudge_tx)
        }

        IngestEvent::RadioDiagnostics {
            hostname,
            kind,
            doc,
        } => {
            // The producer spells the Hub in lowercase; alarms are stored under the spelling
            // every page queries with (#94) — see [`HostnameCanon`].
            let hostname = cache.canonical_hostname(&hostname);
            handle_radio_diagnostics(
                storage,
                cache,
                &hostname,
                kind,
                doc.as_ref(),
                notify_tx,
                nudge_tx,
            )
        }

        IngestEvent::Status { hostname, status } => {
            handle_status(storage, cache, &hostname, status, notify_tx, nudge_tx)
        }

        IngestEvent::ConfigState { hostname, payload } => {
            for entry in &payload.sensors {
                let thresholds = entry.thresholds.as_ref().map(|t| ThresholdBounds {
                    critical_low: t.critical_low,
                    warning_low: t.warning_low,
                    warning_high: t.warning_high,
                    critical_high: t.critical_high,
                });
                // Decision #41: the portal holds a line's bounds. The first sight of a line
                // imports what the Hub has; a row that exists (the operator's) wins.
                if let Some(t) = &thresholds {
                    let row = NewFieldThreshold {
                        hostname: cache.canonical_hostname(&hostname),
                        scope: THRESHOLD_SCOPE_HUB.to_string(),
                        entity: entry.line.to_string(),
                        field: "temperature".to_string(),
                        critical_low: t.critical_low,
                        warning_low: t.warning_low,
                        warning_high: t.warning_high,
                        critical_high: t.critical_high,
                        enabled: true,
                    };
                    if let Err(err) =
                        storage.insert_missing_field_thresholds(&[row], HUB_IMPORT_ACTOR)
                    {
                        tracing::warn!(%hostname, line = entry.line, %err, "hub line bounds not imported");
                    }
                }
                cache.wired_meta.insert(
                    (hostname.clone(), entry.line),
                    WiredMeta {
                        name: entry.name.clone(),
                        location: entry.location.clone(),
                        thresholds,
                    },
                );
            }
            for entry in &payload.lorawan_sensors {
                let node_key = entry.dev_eui.to_lowercase();
                cache
                    .node_location
                    .insert((hostname.clone(), node_key), entry.location.clone());
            }
            for tag in payload.eye_tags.iter().flatten() {
                let mac = tag.mac.to_uppercase();
                cache
                    .eye_location
                    .insert((hostname.clone(), mac), tag.location.clone());
            }
            // The Hub names these Beacons registered: each gets its default bands once
            // (threshold_defaults.md §3). The list is authoritative, so one it no longer names
            // is forgotten and offered them again if it comes back.
            if let Some(tags) = &payload.eye_tags {
                let canonical = cache.canonical_hostname(&hostname);
                let macs: Vec<String> = tags.iter().map(|t| t.mac.to_uppercase()).collect();
                forget_offered_defaults(cache, &canonical, THRESHOLD_SCOPE_EYE, |entity| {
                    !macs.iter().any(|m| m == entity)
                });
                for mac in &macs {
                    offer_threshold_defaults(
                        storage,
                        cache,
                        &canonical,
                        THRESHOLD_SCOPE_EYE,
                        mac,
                        crate::threshold_defaults::beacon_fields(),
                    );
                }
            }
            Ok(())
        }

        IngestEvent::LorawanSensors { hostname, payload } => {
            for sensor in &payload.sensors {
                let node_key = sensor.dev_eui.to_lowercase();
                run_lorawan_alarm_engine(
                    storage, cache, &hostname, &node_key, sensor, notify_tx, nudge_tx,
                )?;
            }
            persist_lorawan_sensors(storage, &hostname, payload, &mut cache.node_last_seen)
        }

        IngestEvent::StickerHistory {
            hostname,
            node_key,
            payload,
        } => persist_sticker_history(storage, &hostname, &node_key, payload),

        // The v2 replay frame: the same rows and crediting, keyed by the spelling the backfill
        // job was stored under (p2p_history_replay.md §14).
        IngestEvent::NodeHistory {
            hostname,
            node_key,
            payload,
        } => {
            let hostname = cache.canonical_hostname(&hostname);
            persist_sticker_history(storage, &hostname, &node_key, payload)
        }

        // History, not telemetry: no alarm engine is called on this arm, so an
        // archive row never opens, closes or notifies an alarm (§4.4, Q4).
        IngestEvent::BeaconHistory { hostname, payload } => {
            // In a cluster the job and its rows are the registry's; the home Hub only reads.
            let key = cache.beacon_registry_or(&hostname);
            persist_beacon_history(storage, cache, &key, payload)
        }

        // Decision #42. Only the Server's own document names the cluster; a restarted portal
        // gets it back as a retained publish before the Hubs' next snapshots.
        IngestEvent::EyeHomes { hostname, payload } => {
            if payload.server.trim().eq_ignore_ascii_case(hostname.trim()) {
                cache.beacon_cluster =
                    Some(crate::state::models::BeaconCluster::from_msg(&payload));
            }
            Ok(())
        }

        IngestEvent::EyeSensors { hostname, payload } => {
            if cache.beacon_cluster.is_none() {
                for tag in &payload.tags {
                    run_eye_alarm_engine(storage, cache, &hostname, tag, notify_tx, nudge_tx)?;
                }
                offer_beacon_defaults(storage, cache, &hostname, &payload.tags);
                // No history row: the snapshot is the live view, and history is the Hub's
                // window (`EyeAggregated` below, storage_intervals.md §6).
                return Ok(());
            }
            // A cluster: a registered Beacon's alarms are the registry's and graded from its
            // home Hub only (Q1), so two Hubs hearing it never raise the same alarm twice. A
            // stranger is graded as unregistered under the registry, which only ever closes
            // an alarm it raised while it was registered.
            let mut by_registry: std::collections::BTreeMap<String, Vec<EyeTag>> =
                std::collections::BTreeMap::new();
            for tag in &payload.tags {
                let (registry, tag) = match cache.beacon_source(&hostname, &tag.mac) {
                    BeaconSource::Home { registry } => (registry, tag.clone()),
                    BeaconSource::Stranger { registry } => (
                        registry,
                        EyeTag {
                            discovered: Some(true),
                            ..tag.clone()
                        },
                    ),
                    BeaconSource::NotHome | BeaconSource::Legacy => continue,
                };
                run_eye_alarm_engine(storage, cache, &registry, &tag, notify_tx, nudge_tx)?;
                by_registry.entry(registry).or_default().push(tag);
            }
            for (registry, tags) in by_registry {
                offer_beacon_defaults(storage, cache, &registry, &tags);
            }
            Ok(())
        }

        IngestEvent::EyeAggregated { hostname, payload } => {
            persist_eye_aggregated(storage, cache, &hostname, payload)
        }

        // ---- WP-E: the v2 radio northbound family -----------------------
        //
        // One node per document, so no loop; and no `last_seen`-advanced gate, because the
        // envelope carries a real dedup identifier (see `node_reading_is_new`).
        IngestEvent::NodeReading {
            hostname,
            node_key,
            payload,
            ..
        } => {
            let hostname = cache.canonical_hostname(&hostname);
            // The sensor's alarm is the portal's to evaluate, on telemetry only; the Node's own
            // alarms arrive on the `alarm` document (nodes_phase2.cs.md P1/P2).
            run_node_threshold_engine(
                storage, cache, &hostname, &node_key, &payload, notify_tx, nudge_tx,
            )?;
            // A probe that reports before its config info declares it is registered by its
            // first reading (threshold_defaults.md §3).
            offer_threshold_defaults(
                storage,
                cache,
                &hostname,
                THRESHOLD_SCOPE_NODE,
                &node_key,
                payload.fields.keys().map(String::as_str),
            );
            persist_node_reading(storage, cache, &hostname, &node_key, &payload)
        }

        IngestEvent::NodeAlarm {
            hostname,
            node_key,
            payload,
            ..
        } => {
            let hostname = cache.canonical_hostname(&hostname);
            // The engine runs before — and independently of — the dedup gate below, exactly
            // as `run_lorawan_alarm_engine` does for the v1 path: an alarm state change
            // matters even on a document the telemetry sink dedups away, and re-running it on
            // a genuine duplicate is a no-op (an already-active native alarm ⇒ `Nothing`,
            // plus the engine's own `dedup_key`).
            run_node_alarm_engine(
                storage, cache, &hostname, &node_key, &payload, notify_tx, nudge_tx,
            )?;
            persist_node_reading(storage, cache, &hostname, &node_key, &payload)
        }

        // The removal tombstone (the empty retained payload `node-remove` publishes) closes the
        // Node's open alarms. Nothing else ever would: they close on the Node's next reading,
        // alarm document or Info, and a removed Node sends none, so an alarm of a removed Node
        // stayed open for good (night test 2026-09-26, PN-9). The rows stay, cleared, with the
        // rest of the Node's history.
        IngestEvent::NodeInventory {
            hostname,
            node_key,
            payload,
        } if payload.is_null() => {
            let hostname = cache.canonical_hostname(&hostname);
            forget_offered_defaults(cache, &hostname, THRESHOLD_SCOPE_NODE, |e| e == node_key);
            close_removed_node_alarms(storage, &hostname, &node_key, nudge_tx)
        }

        // The retained per-node inventory document. Its home is the in-memory `StateStore`
        // (`device.node_infos` + `device.nodes`), which is what the portal renders a Node
        // list from and where the empty-payload tombstone removes it — there is no node
        // inventory *table* in portal storage (V4 stores readings, events and raw messages,
        // all keyed by `node_key`, never an identity row). Nothing to write here, and
        // deliberately nothing to delete on removal either: a removed Node's medical history
        // stays under retention, it does not leave with the device.
        //
        // What it does write: the default bands of the board's temperature and humidity, which
        // every Node has (threshold_defaults.md §3). This document is how the portal learns of
        // a Node registered anywhere, the CLI included, and the Hub replays it on every
        // connect, which is also what gives the Nodes of an older build theirs.
        IngestEvent::NodeInventory {
            hostname, node_key, ..
        } => {
            let hostname = cache.canonical_hostname(&hostname);
            offer_threshold_defaults(
                storage,
                cache,
                &hostname,
                THRESHOLD_SCOPE_NODE,
                &node_key,
                crate::render::BOARD_FIELDS.iter().copied(),
            );
            Ok(())
        }

        // An Info that says the Node holds no latched alarm closes the native alarms a
        // reboot cleared without a `deactivate` (node_reported_state.md §4, F24).
        IngestEvent::NodeDeviceInfo {
            hostname,
            node_key,
            payload: Some(doc),
        } => {
            let hostname = cache.canonical_hostname(&hostname);
            // GitLab #135: the Node sends its crash record on one boot only, and the next Info
            // replaces it on the Hub, so the portal keeps it as an event. Keyed by the receive
            // time: the retained document replayed on every connect is the same row again.
            if let (Some(record), Some(ts)) = (
                crate::render::fault_record(&doc.device_info, &doc.received_at),
                parse_rfc3339(&doc.received_at),
            ) {
                storage.insert_event(&NodeEvent {
                    hostname: hostname.clone(),
                    node_key: node_key.clone(),
                    event_type: crate::render::FATAL_ERROR_EVENT.to_string(),
                    ts,
                    seq: 0,
                    payload: Some(record.to_string()),
                })?;
            }
            close_forgotten_native_alarms(
                storage,
                cache,
                &hostname,
                &node_key,
                &doc,
                chrono_free_now_unix(),
                notify_tx,
                nudge_tx,
            )
        }

        // Config info that moves the Node's alarm slots (its active sensors changed) drafts the
        // table again, for the operator to send (nodes_phase1.cs.md WP2).
        IngestEvent::NodeDeviceConfig {
            hostname,
            node_key,
            payload: Some(doc),
        } => {
            let hostname = cache.canonical_hostname(&hostname);
            if let Some(interval) = doc
                .config
                .get("application.interval_report")
                .and_then(serde_json::Value::as_i64)
            {
                cache
                    .node_interval_report
                    .insert((hostname.clone(), node_key.clone()), interval);
            }
            // The 1-Wire probes it declares are registered by it (threshold_defaults.md §3).
            if let Some(declared) = declared_node_fields(&doc) {
                offer_threshold_defaults(
                    storage,
                    cache,
                    &hostname,
                    THRESHOLD_SCOPE_NODE,
                    &node_key,
                    declared.iter().map(String::as_str),
                );
            }
            redraft_alarm_table_on_layout_change(storage, &hostname, &node_key, &doc)
        }

        // The Node's own reports (node_reported_state.md §4). Same answer, for the same
        // reason: in-memory `StateStore` state, and restart recovery is the Hub replaying the
        // retained documents from its `node-get` cache on every connect — not a table here.
        IngestEvent::NodeDeviceInfo { .. } | IngestEvent::NodeDeviceConfig { .. } => Ok(()),

        // Every fPort-85 frame also joins the Node's message ring as port 85 — the command
        // engine's traffic (answers, the boot Info and settings-info, dump pages) — so the
        // detail page shows the last ten per port (G7, Nodes test 2026-09-23). The store relays
        // the answers as `NodeCommandResult` besides.
        IngestEvent::NodeCommand {
            hostname,
            node_key,
            payload,
        } => {
            // Not a frame: control-radio's note that a newer identical ask took a queued frame's
            // place (Q-N1). In the ring it would read as traffic from the Node and push one of
            // its last ten frames out.
            // The same for its note that it gave a downlink up (`undelivered`, §10.5).
            if matches!(
                payload.get("status").and_then(serde_json::Value::as_str),
                Some(
                    control_radio::coalesce::SUPERSEDED | control_radio::p2p::session::UNDELIVERED
                )
            ) {
                return Ok(());
            }
            let hostname = cache.canonical_hostname(&hostname);
            let ts = payload
                .get("received_at")
                .and_then(serde_json::Value::as_str)
                .and_then(|t| OffsetDateTime::parse(t, &Rfc3339).ok())
                .map(OffsetDateTime::unix_timestamp)
                .unwrap_or_else(|| OffsetDateTime::now_utc().unix_timestamp());
            let mut row = payload;
            if let Some(obj) = row.as_object_mut() {
                obj.insert("fport".into(), serde_json::Value::from(85));
            }
            match serde_json::to_string(&row) {
                Ok(raw) => storage
                    .insert_raw_message(&hostname, &node_key.to_lowercase(), ts, &raw)
                    .map(|_| ()),
                Err(err) => {
                    tracing::warn!(%err, "command document not serializable; not stored");
                    Ok(())
                }
            }
        }

        // State-only (StateStore cache) or SSE-only passthrough — no
        // persistent home, same as Standby/DeviceError/… below.
        IngestEvent::StickerConfig { .. }
        | IngestEvent::StickerFullConfig { .. }
        | IngestEvent::StickerInfo { .. }
        | IngestEvent::StickerCommandResult { .. } => Ok(()),

        // Standby/DeviceError/MqttConnection/ConfigChallenge/ConfigResponse/
        // PairResponse have no persistent home in M1/M4-C. PairInvite has none
        // either and never arrives here at all — `mqtt::dispatch` keeps it out
        // of this sink so the operator's pairing code cannot reach the spill
        // journal (GitLab #61).
        _ => Ok(()),
    }
}

// ---- Alarm engine wiring (M6-E) -----------------------------------------

/// Reads `alarm_events`' current active row through the alarm engine's
/// `ActiveLookup` trait — the engine's only "memory".
struct StorageActiveLookup<'a>(&'a Storage);

impl ActiveLookup for StorageActiveLookup<'_> {
    fn active(&self, hostname: &str, sensor_id: &str) -> Option<ActiveAlarm> {
        match self.0.active_alarm(hostname, sensor_id) {
            Ok(Some(row)) => Some(ActiveAlarm {
                id: row.id,
                severity: parse_severity(&row.severity),
                max_severity: parse_severity(&row.max_severity),
                alarm_type: parse_alarm_type(&row.alarm_type),
                to_state: row
                    .to_state
                    .as_deref()
                    .and_then(|s| WireState::try_from(s).ok()),
                triggered_at_unix: parse_rfc3339(&row.triggered_at),
            }),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(%err, hostname, sensor_id, "active_alarm lookup failed — treating as no active alarm");
                None
            }
        }
    }
}

fn severity_str(s: Severity) -> &'static str {
    match s {
        Severity::Warning => "warning",
        Severity::Critical => "critical",
    }
}

fn parse_severity(s: &str) -> Severity {
    if s.eq_ignore_ascii_case("critical") {
        Severity::Critical
    } else {
        Severity::Warning
    }
}

fn alarm_type_str(t: AlarmType) -> &'static str {
    match t {
        AlarmType::High => "HIGH",
        AlarmType::Low => "LOW",
        AlarmType::Disconnect => "DISCONNECT",
        AlarmType::PowerDisconnect => "POWER_DISCONNECT",
        AlarmType::WifiDisconnect => "WIFI_DISCONNECT",
        AlarmType::EthernetDisconnect => "ETHERNET_DISCONNECT",
        AlarmType::Offline => "OFFLINE",
        AlarmType::ConcentratorMissing => "CONCENTRATOR_MISSING",
        AlarmType::HubOffline => "HUB_OFFLINE",
        AlarmType::NorthbridgeUnreachable => "NORTHBRIDGE_UNREACHABLE",
        AlarmType::ChirpstackDown => "CHIRPSTACK_DOWN",
        AlarmType::NorthbridgeUpdateFailed => "NORTHBRIDGE_UPDATE_FAILED",
        AlarmType::SouthbridgeUpdateFailed => "SOUTHBRIDGE_UPDATE_FAILED",
        AlarmType::ConfigNotDelivered => "CONFIG_NOT_DELIVERED",
        AlarmType::Calibration => "CALIBRATION",
        AlarmType::ConfigSync => "CONFIG_SYNC",
    }
}

/// The inverse of [`alarm_type_str`].
///
/// **Total, and no longer silent.** The catch-all still answers `High`,
/// because every caller needs *an* answer and there is no error channel on
/// this path — but it now logs, which is the half MB3 needed. An
/// `alarm_type` this build does not know is either a row written by a newer
/// portal or a typo in a writer, and the failure mode it used to have was
/// to render a liveness outage as a high-temperature alarm with no trace of
/// the substitution anywhere (monitoring.backend.md §6, WP3.2).
fn parse_alarm_type(s: &str) -> AlarmType {
    match s {
        "HIGH" => AlarmType::High,
        "LOW" => AlarmType::Low,
        "DISCONNECT" => AlarmType::Disconnect,
        "POWER_DISCONNECT" => AlarmType::PowerDisconnect,
        "WIFI_DISCONNECT" => AlarmType::WifiDisconnect,
        "ETHERNET_DISCONNECT" => AlarmType::EthernetDisconnect,
        "OFFLINE" => AlarmType::Offline,
        "CONCENTRATOR_MISSING" => AlarmType::ConcentratorMissing,
        "HUB_OFFLINE" => AlarmType::HubOffline,
        "NORTHBRIDGE_UNREACHABLE" => AlarmType::NorthbridgeUnreachable,
        "CHIRPSTACK_DOWN" => AlarmType::ChirpstackDown,
        "NORTHBRIDGE_UPDATE_FAILED" => AlarmType::NorthbridgeUpdateFailed,
        "SOUTHBRIDGE_UPDATE_FAILED" => AlarmType::SouthbridgeUpdateFailed,
        "CONFIG_NOT_DELIVERED" => AlarmType::ConfigNotDelivered,
        "CALIBRATION" => AlarmType::Calibration,
        "CONFIG_SYNC" => AlarmType::ConfigSync,
        other => {
            tracing::warn!(
                alarm_type = other,
                "unknown alarm_type read back as HIGH — a newer writer, or a typo in one"
            );
            AlarmType::High
        }
    }
}

fn notify_source(s: DomainAlarmSource) -> crate::notify::payload::AlarmSource {
    match s {
        DomainAlarmSource::Fiber => crate::notify::payload::AlarmSource::Fiber,
        DomainAlarmSource::Sticker => crate::notify::payload::AlarmSource::Sticker,
        DomainAlarmSource::Eye => crate::notify::payload::AlarmSource::Eye,
        DomainAlarmSource::System => crate::notify::payload::AlarmSource::System,
    }
}

fn map_new_alarm(hostname: &str, alarm: &NewAlarm, location: Option<String>) -> NewAlarmEvent {
    NewAlarmEvent {
        hostname: hostname.to_string(),
        sensor_id: alarm.sensor_id.clone(),
        sensor_name: alarm.sensor_name.clone(),
        sensor_location: location,
        alarm_type: alarm_type_str(alarm.alarm_type).to_string(),
        severity: severity_str(alarm.severity).to_string(),
        from_state: alarm.from_state.map(|s| s.as_str().to_string()),
        to_state: alarm.to_state.map(|s| s.as_str().to_string()),
        message: alarm.message.clone(),
        value: alarm.value,
        threshold: alarm.threshold,
        triggered_at: alarm.triggered_at.clone(),
        cleared_at: None,
        acknowledged_at: None,
        acknowledged_by_user_id: None,
        acknowledged_by_username: None,
        native_key: alarm.dedup_key.clone(),
    }
}

fn notify_data_from_payload(
    alarm_id: String,
    hostname: &str,
    payload: &NotifyPayload,
    node_key: Option<&str>,
    serial_number: Option<&str>,
    mac: Option<&str>,
) -> AlarmNotifyData {
    AlarmNotifyData {
        alarm_id,
        hostname: hostname.to_string(),
        severity: severity_str(payload.severity).to_string(),
        alarm_type: alarm_type_str(payload.alarm_type).to_string(),
        source: notify_source(payload.source),
        value: payload.value,
        threshold: payload.threshold,
        message: payload.message.clone(),
        sensor_name: payload
            .sensor_name
            .clone()
            .unwrap_or_else(|| payload.sensor_id.clone()),
        sensor_id: payload.sensor_id.clone(),
        line: payload.line.map(|l| l as i32),
        location: payload.location.clone(),
        node_key: node_key.map(str::to_string),
        serial_number: serial_number.map(str::to_string),
        mac: mac.map(str::to_string),
        triggered_at: payload.triggered_at.clone(),
    }
}

fn notify_data_from_row(
    row: &AlarmEventRow,
    node_key: Option<&str>,
    serial_number: Option<&str>,
    mac: Option<&str>,
) -> AlarmNotifyData {
    AlarmNotifyData {
        alarm_id: row.id.clone(),
        hostname: row.hostname.clone(),
        severity: row.severity.clone(),
        alarm_type: row.alarm_type.clone(),
        source: notify_source(match source_from_sensor_id(&row.sensor_id) {
            crate::notify::payload::AlarmSource::Fiber => DomainAlarmSource::Fiber,
            crate::notify::payload::AlarmSource::Sticker => DomainAlarmSource::Sticker,
            crate::notify::payload::AlarmSource::Eye => DomainAlarmSource::Eye,
            crate::notify::payload::AlarmSource::System => DomainAlarmSource::System,
        }),
        value: row.value,
        threshold: row.threshold,
        message: row.message.clone(),
        sensor_name: row
            .sensor_name
            .clone()
            .unwrap_or_else(|| row.sensor_id.clone()),
        sensor_id: row.sensor_id.clone(),
        line: row
            .sensor_id
            .strip_prefix("line-")
            .and_then(|n| n.parse().ok()),
        location: row.sensor_location.clone(),
        node_key: node_key.map(str::to_string),
        serial_number: serial_number.map(str::to_string),
        mac: mac.map(str::to_string),
        triggered_at: row.triggered_at.clone(),
    }
}

/// Applies one reducer call's `AlarmAction`s: `Open`→insert (native-dedup
/// `Conflict` is skipped quietly, per the work order), `UpdateSeverity`/
/// `UpdateStateInPlace`/`Clear`→the matching repo call, `Notify`→a
/// [`NotifyJob`] built from the just-applied `Open`'s new id (`Activated`)
/// or from re-reading the just-cleared row (`Cleared` — the engine's
/// `ActiveAlarm` doesn't carry `threshold`, see `alarm_engine`'s module
/// doc). Sends exactly one [`AlarmNudge`] when the batch wrote anything,
/// carrying the ids of any rows the batch *opened* and any it *escalated to
/// critical*, so the SSE layer can pop a toast for each (see
/// `AlarmNudge::activated_ids`/`escalated_ids`).
#[allow(clippy::too_many_arguments)]
fn apply_alarm_actions(
    storage: &Storage,
    hostname: &str,
    node_key: Option<&str>,
    serial_number: Option<&str>,
    mac: Option<&str>,
    location: Option<String>,
    actions: Vec<AlarmAction>,
    notify_tx: &mpsc::UnboundedSender<NotifyJob>,
    nudge_tx: &broadcast::Sender<AlarmNudge>,
) -> Result<(), portal_storage::StorageError> {
    if actions.is_empty() {
        return Ok(());
    }

    let mut last_open_id: Option<String> = None;
    let mut last_clear_id: Option<String> = None;
    let mut last_update_id: Option<String> = None;
    let mut activated_ids: Vec<String> = Vec::new();
    let mut escalated_ids: Vec<String> = Vec::new();
    let mut wrote = false;

    for action in actions {
        match action {
            AlarmAction::Open(new_alarm) => {
                let event = map_new_alarm(hostname, &new_alarm, location.clone());
                match storage.insert_alarm_event(&event) {
                    Ok(id) => {
                        activated_ids.push(id.clone());
                        last_open_id = Some(id);
                        last_clear_id = None;
                        wrote = true;
                    }
                    Err(portal_storage::StorageError::Conflict(msg)) => {
                        tracing::debug!(hostname, %msg, "alarm engine: native dedup — skipping quietly");
                        last_open_id = None;
                    }
                    // Not warn-and-continue any more: this is the 2026-08-24
                    // defect, where a VACUUM outlasting `busy_timeout` silently
                    // discarded alarm rows *and* their notifications. The
                    // caller retries the whole event and spills if that fails;
                    // the engine is a reducer over current state, so a
                    // partially applied batch re-derives correctly on replay.
                    Err(err) => return Err(err),
                }
            }
            AlarmAction::UpdateSeverity {
                id,
                severity,
                alarm_type,
                to_state,
                value,
                threshold,
                message,
                clears_acknowledgement,
            } => match storage.update_alarm_severity(
                &id,
                AlarmSeverityChange {
                    severity: severity_str(severity),
                    alarm_type: alarm_type_str(alarm_type),
                    to_state: to_state.map(|s| s.as_str()),
                    value,
                    threshold,
                    message: message.as_deref(),
                    clear_acknowledgement: clears_acknowledgement,
                },
            ) {
                Ok(true) => {
                    wrote = true;
                    // The escalation toast follows `Notify(Escalated)` below,
                    // not every climb to critical: the page and the mailbox
                    // announce the same first escalation (decisions.md #26),
                    // and a probe wobbling across the bound pops nothing new.
                    last_update_id = Some(id);
                }
                Ok(false) => tracing::warn!(hostname, id, "UpdateSeverity: alarm row vanished"),
                Err(err) => return Err(err),
            },
            AlarmAction::UpdateStateInPlace {
                id,
                to_state,
                message,
            } => match storage.update_alarm_state(&id, to_state.as_str(), message.as_deref()) {
                Ok(true) => wrote = true,
                Ok(false) => tracing::warn!(hostname, id, "UpdateStateInPlace: alarm row vanished"),
                Err(err) => return Err(err),
            },
            AlarmAction::Clear {
                id,
                cleared_at_iso,
                message,
            } => match storage.clear_alarm(&id, &cleared_at_iso, message.as_deref()) {
                Ok(true) => {
                    last_clear_id = Some(id);
                    last_open_id = None;
                    wrote = true;
                }
                Ok(false) => tracing::warn!(hostname, id, "Clear: alarm row vanished"),
                Err(err) => return Err(err),
            },
            AlarmAction::Notify(kind) => match kind {
                EngineNotifyKind::Activated(payload) => {
                    if let Some(id) = last_open_id.take() {
                        let data = notify_data_from_payload(
                            id,
                            hostname,
                            &payload,
                            node_key,
                            serial_number,
                            mac,
                        );
                        if notify_tx.send(NotifyJob::Activated(data)).is_err() {
                            tracing::warn!(
                                "notify channel closed — activated notification dropped"
                            );
                        }
                    }
                }
                // Read back from the row it follows, like Cleared: the
                // notification describes the alarm as stored, at its new level.
                EngineNotifyKind::Escalated(_payload) => {
                    if let Some(id) = last_update_id.take() {
                        escalated_ids.push(id.clone());
                        match storage.get_alarm(&id) {
                            Ok(Some(row)) => {
                                let data = notify_data_from_row(&row, node_key, serial_number, mac);
                                if notify_tx.send(NotifyJob::Escalated(data)).is_err() {
                                    tracing::warn!(
                                        "notify channel closed — escalated notification dropped"
                                    );
                                }
                            }
                            Ok(None) => {}
                            Err(err) => return Err(err),
                        }
                    }
                }
                EngineNotifyKind::Cleared(_payload) => {
                    if let Some(id) = last_clear_id.take() {
                        match storage.get_alarm(&id) {
                            Ok(Some(row)) => {
                                let ack = row.acknowledged_at.as_ref().map(|_| {
                                    let user =
                                        row.acknowledged_by_username.clone().unwrap_or_default();
                                    // `name` is display only (the e-mail, the webhook's
                                    // `acknowledgement.name`), so a failed lookup falls
                                    // back to the username.
                                    let names =
                                        storage.full_names_for(&[&user]).unwrap_or_default();
                                    // Display only too: a failed read sends
                                    // the clear without it.
                                    let reason = storage.ack_reason(&row).unwrap_or_else(|err| {
                                        tracing::warn!(%err, "ack reason unreadable");
                                        None
                                    });
                                    AckInfo {
                                        name: crate::render::person_label(&user, &names),
                                        user,
                                        reason,
                                    }
                                });
                                let data = notify_data_from_row(&row, node_key, serial_number, mac);
                                if notify_tx.send(NotifyJob::Cleared { data, ack }).is_err() {
                                    tracing::warn!(
                                        "notify channel closed — cleared notification dropped"
                                    );
                                }
                            }
                            Ok(None) => {}
                            Err(err) => return Err(err),
                        }
                    }
                }
            },
        }
    }

    if wrote {
        let _ = nudge_tx.send(AlarmNudge {
            hostname: hostname.to_string(),
            activated_ids,
            escalated_ids,
        });
    }

    Ok(())
}

fn map_alarm_type_hint(t: Option<&WireAlarmTypeHint>) -> Option<ConnectivityKind> {
    match t {
        Some(WireAlarmTypeHint::PowerDisconnect) => Some(ConnectivityKind::Power),
        Some(WireAlarmTypeHint::WifiDisconnect) => Some(ConnectivityKind::Wifi),
        Some(WireAlarmTypeHint::EthernetDisconnect) => Some(ConnectivityKind::Ethernet),
        Some(WireAlarmTypeHint::Unknown) | None => None,
    }
}

/// `{prefix}/{host}/alarms/events` → `portal_domain::on_wired_transition`.
fn handle_wired_alarm(
    storage: &Storage,
    cache: &mut PersistCache,
    hostname: &str,
    payload: AlarmEventMsg,
    notify_tx: &mpsc::UnboundedSender<NotifyJob>,
    nudge_tx: &broadcast::Sender<AlarmNudge>,
) -> Result<(), portal_storage::StorageError> {
    let Ok(from_state) = WireState::try_from(payload.from_state.as_str()) else {
        tracing::warn!(hostname, from_state = %payload.from_state, "unknown wired alarm from_state — skipped");
        return Ok(());
    };
    let Ok(mut to_state) = WireState::try_from(payload.to_state.as_str()) else {
        tracing::warn!(hostname, to_state = %payload.to_state, "unknown wired alarm to_state — skipped");
        return Ok(());
    };

    // Decision #41: the portal grades a line's value itself (`grade_wired_line`). Of the
    // Hub's transitions it takes the line's connection only: into DISCONNECTED,
    // RECONNECTING or NEVERCONNECTED, and back out of them, which closes the disconnect
    // alarm as NORMAL (the next live sample grades the value). A value-to-value transition
    // is the Hub's own grade and is dropped.
    let is_value = |s: WireState| {
        matches!(
            s,
            WireState::Normal | WireState::Warning | WireState::Critical
        )
    };
    if payload.alarm_type.is_none() && is_value(to_state) {
        if is_value(from_state) {
            return Ok(());
        }
        let lookup = StorageActiveLookup(storage);
        if portal_domain::held_wired_state(&lookup, hostname, payload.line).is_some() {
            // No disconnect open: nothing of the connection's to close.
            return Ok(());
        }
        to_state = WireState::Normal;
    }

    let meta = cache
        .wired_meta
        .get(&(hostname.to_string(), payload.line))
        .cloned()
        .unwrap_or_default();
    let ts_iso = payload.timestamp.clone().unwrap_or_else(iso_now);

    let transition = WiredTransition {
        hostname: hostname.to_string(),
        line: payload.line,
        name: payload.name.clone().or_else(|| meta.name.clone()),
        location: meta.location.clone(),
        from_state,
        to_state,
        temperature: payload.temperature_celsius,
        thresholds: meta.thresholds,
        alarm_type_hint: map_alarm_type_hint(payload.alarm_type.as_ref()),
        ts_iso,
    };

    let lookup = StorageActiveLookup(storage);
    let actions = portal_domain::on_wired_transition(&lookup, &transition);
    apply_alarm_actions(
        storage,
        hostname,
        None,
        None,
        None,
        meta.location,
        actions,
        notify_tx,
        nudge_tx,
    )?;
    Ok(())
}

/// A Node's (`node`) or Beacon's (`eye`) location for its alarm rows: the Portal's own
/// (`device_meta`, decision #41) when an operator set one, else what the Hub last reported.
fn portal_location(
    storage: &Storage,
    cache: &PersistCache,
    hostname: &str,
    scope: &str,
    entity: &str,
) -> Option<String> {
    let own = [hostname.to_string(), cache.canonical_hostname(hostname)]
        .iter()
        .find_map(|h| storage.get_device_meta(h, scope, entity).ok().flatten())
        .and_then(|m| m.location);
    own.or_else(|| {
        let key = (hostname.to_string(), entity.to_string());
        if scope == THRESHOLD_SCOPE_EYE {
            cache.eye_location.get(&key).cloned().flatten()
        } else {
            cache.node_location.get(&key).cloned().flatten()
        }
    })
}

/// Who wrote a Hub line's bounds into `field_thresholds` from the Hub's own `config/state`
/// (decision #41: the first sight of a line imports what the Hub holds).
const HUB_IMPORT_ACTOR: &str = "system:hub-import";

/// One 1-Wire line of a live sample → the portal's own grade (decision #41) →
/// `portal_domain::on_wired_transition`.
///
/// The bounds are the line's `hub` row in `field_thresholds`, else the ones the Hub reports
/// in `config/state`; the hysteresis the row's own, else the temperature default. The state
/// the line holds is its open alarm. A line with an open disconnect is left to the Hub's
/// transitions until they close it, and a sample without a value grades nothing.
fn grade_wired_line(
    storage: &Storage,
    cache: &PersistCache,
    hostname: &str,
    sensor: &AggregatedSensor,
    ts_iso: &str,
    notify_tx: &mpsc::UnboundedSender<NotifyJob>,
    nudge_tx: &broadcast::Sender<AlarmNudge>,
) -> Result<(), portal_storage::StorageError> {
    if sensor.sample_count <= 0 {
        return Ok(());
    }
    let Some(value) = sensor.temperature.as_ref().and_then(|t| t.avg_celsius) else {
        return Ok(());
    };
    let meta = cache
        .wired_meta
        .get(&(hostname.to_string(), sensor.line))
        .cloned()
        .unwrap_or_default();

    let lookup = StorageActiveLookup(storage);
    let Some(held) = portal_domain::held_wired_state(&lookup, hostname, sensor.line) else {
        return Ok(());
    };

    let canonical = cache.canonical_hostname(hostname);
    let entity = sensor.line.to_string();
    let row =
        storage.get_field_threshold(&canonical, THRESHOLD_SCOPE_HUB, &entity, "temperature")?;
    let bounds = match &row {
        Some(r) if !r.enabled => None,
        Some(r) => Some(ThresholdBounds {
            critical_low: r.critical_low,
            warning_low: r.warning_low,
            warning_high: r.warning_high,
            critical_high: r.critical_high,
        }),
        None => meta.thresholds,
    };
    let to_state = match &bounds {
        Some(b) => {
            let own = if row.is_some() {
                storage.field_threshold_hysteresis(
                    &canonical,
                    THRESHOLD_SCOPE_HUB,
                    &entity,
                    "temperature",
                )?
            } else {
                None
            };
            let h = crate::threshold_defaults::hysteresis(
                &crate::threshold_defaults::configured(),
                THRESHOLD_SCOPE_HUB,
                "temperature",
                own,
            );
            grade_value(b, h, value, Some(held))
        }
        None => WireState::Normal,
    };
    if to_state == held {
        return Ok(());
    }

    let location = sensor.location.clone().or(meta.location);
    let transition = WiredTransition {
        hostname: hostname.to_string(),
        line: sensor.line,
        name: sensor.name.clone().or(meta.name),
        location: location.clone(),
        from_state: held,
        to_state,
        temperature: Some(value),
        thresholds: bounds,
        alarm_type_hint: None,
        ts_iso: ts_iso.to_string(),
    };
    let actions = portal_domain::on_wired_transition(&lookup, &transition);
    apply_alarm_actions(
        storage, hostname, None, None, None, location, actions, notify_tx, nudge_tx,
    )
}

/// `{prefix}/{host}/system/info` connectivity leaves →
/// `portal_domain::on_connectivity`, plus the merged [`SystemSnapshot`] fed
/// to the notifier's system-alert checker.
fn handle_system_info(
    storage: &Storage,
    cache: &mut PersistCache,
    hostname: &str,
    payload: SystemInfoMsg,
    notify_tx: &mpsc::UnboundedSender<NotifyJob>,
    nudge_tx: &broadcast::Sender<AlarmNudge>,
) -> Result<(), portal_storage::StorageError> {
    let ts_iso = payload.timestamp.clone().unwrap_or_else(iso_now);

    let dc_connected = payload
        .power
        .as_ref()
        .and_then(|p| p.dc.as_ref())
        .and_then(|d| d.connected);
    let wifi_connected = payload
        .network
        .as_ref()
        .and_then(|n| n.wifi.as_ref())
        .and_then(|w| w.connected);
    let ethernet_connected = payload
        .network
        .as_ref()
        .and_then(|n| n.ethernet.as_ref())
        .and_then(|e| e.connected);
    let has_internet = payload.network.as_ref().and_then(|n| n.has_internet);

    // Concentrator presence — only meaningful when the Hub is in a LoRaWAN-serving
    // radio mode (`lorawan`/`dual`); an `off`/`p2p` Hub having no concentrator is
    // normal, not a fault. The RAK5146 enumerates over USB by udev hotplug and can
    // appear slightly after boot, so a missing concentrator only opens the alarm
    // once past a short grace window keyed on uptime — otherwise every clean boot
    // would false-positive before enumeration. A *running* concentrator clears any
    // active alarm immediately (no grace needed to clear). `None` = "does not
    // apply" and `on_connectivity` takes no action (off/p2p, unknown, or in grace).
    const CONCENTRATOR_GRACE_SECS: i64 = 120;
    let concentrator_present = {
        let lorawan = payload.lorawan.as_ref();
        let serving = matches!(
            lorawan.and_then(|l| l.mode.as_deref()),
            Some("lorawan") | Some("dual")
        );
        let past_grace = payload
            .uptime_seconds
            .is_some_and(|u| u >= CONCENTRATOR_GRACE_SECS);
        match (serving, lorawan.and_then(|l| l.concentratord_running)) {
            (true, Some(true)) => Some(true),
            (true, Some(false)) if past_grace => Some(false),
            _ => None,
        }
    };

    // UX-058: the Hub publishes both link flags every time, so a Hub on one
    // uplink reports the other `false` forever. The engine opens on `false`,
    // so only a true→false edge this process saw reaches it as `false`
    // (plan/v1/viewer/mqtt.md:79, v1 handlers.py:735,763); `true` always
    // passes, to clear. The previous value is the one `system_snapshot`
    // already keeps below, empty after a restart: a first sample never opens.
    let prev = cache.system_snapshot.get(hostname);
    let edge = |now: Option<bool>, before: Option<bool>| match now {
        Some(false) if before != Some(true) => None,
        other => other,
    };
    let snap = ConnectivitySnapshot {
        hostname: hostname.to_string(),
        dc_connected,
        wifi_connected: edge(wifi_connected, prev.and_then(|e| e.wifi_connected)),
        ethernet_connected: edge(ethernet_connected, prev.and_then(|e| e.ethernet_connected)),
        has_internet,
        concentrator_present,
        online: None,
        northbridge_reachable: None,
        chirpstack_running: None,
        northbridge_update_ok: None,
        // southbridge_update.md §3.7: "Aktualizace firmwaru Southbridge selhala" while the
        // block says `failed`; any other state, no update object or no block clears.
        southbridge_update_ok: Some(southbridge_update_ok(payload.southbridge.as_ref())),
    };
    let lookup = StorageActiveLookup(storage);
    let actions = portal_domain::on_connectivity(&lookup, &snap, &ts_iso);
    apply_alarm_actions(
        storage, hostname, None, None, None, None, actions, notify_tx, nudge_tx,
    )?;

    let entry = cache
        .system_snapshot
        .entry(hostname.to_string())
        .or_default();
    if let Some(v) = wifi_connected {
        entry.wifi_connected = Some(v);
    }
    if let Some(v) = ethernet_connected {
        entry.ethernet_connected = Some(v);
    }
    if let Some(v) = has_internet {
        entry.has_internet = Some(v);
    }
    if let Some(pct) = payload
        .power
        .as_ref()
        .and_then(|p| p.battery.as_ref())
        .and_then(|b| b.percentage)
    {
        entry.battery_pct = Some(pct as f64);
    }
    if let Some(pct) = payload
        .storage
        .as_ref()
        .and_then(|s| s.data_partition.as_ref())
        .and_then(|d| d.used_percent)
    {
        entry.storage_used_pct = Some(pct);
    }
    if let Some(version) = payload.firmware_version {
        entry.version = Some(version);
    }

    send_system_snapshot(cache, hostname, notify_tx);
    Ok(())
}

/// How long a radio component must stay down before its alarm opens (radio diagnostics
/// §4 Q2, decided: five minutes).
const RADIO_DOWN_GRACE_SECS: i64 = 300;

/// The radio-diagnostics alarm verdict for one component: `Some(false)` once it has been down
/// for [`RADIO_DOWN_GRACE_SECS`] (opens), `Some(true)` when it is up or does not apply
/// (clears), `None` while it is down but inside the grace, or undecidable.
fn radio_verdict(
    cache: &mut PersistCache,
    hostname: &str,
    component: &'static str,
    down: Option<bool>,
    now: i64,
) -> Option<bool> {
    let key = (hostname.to_string(), component);
    match down {
        Some(true) => {
            let since = *cache.radio_down_since.entry(key).or_insert(now);
            (now.saturating_sub(since) >= RADIO_DOWN_GRACE_SECS).then_some(false)
        }
        Some(false) => {
            cache.radio_down_since.remove(&key);
            Some(true)
        }
        None => None,
    }
}

/// `proximos/{host}/radio/northbridge` and `…/radio/lorawan` — the radio status documents
/// (`plan/control/radio/northbridge_diagnostics.md` §3, §4a). The documents themselves are
/// rendered from the store; here they only feed the two Hub alarms, "Northbridge neodpovídá"
/// and "ChirpStack neběží", each after five minutes down. A withdrawn document, or one that
/// says the component is `disabled` (the mode no longer uses it), clears.
///
/// The northbridge document's `update` object (`northbridge_update.md` §3.5) feeds a third,
/// "Aktualizace firmwaru Northbridge selhala": open while `update.state` is `failed`, cleared
/// by any other state, a `null` update (no bundled firmware) or a withdrawn document.
fn handle_radio_diagnostics(
    storage: &Storage,
    cache: &mut PersistCache,
    hostname: &str,
    kind: crate::state::events::RadioDiagKind,
    doc: Option<&serde_json::Value>,
    notify_tx: &mpsc::UnboundedSender<NotifyJob>,
    nudge_tx: &broadcast::Sender<AlarmNudge>,
) -> Result<(), portal_storage::StorageError> {
    use crate::state::events::RadioDiagKind;
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let state = doc.and_then(|d| d.get("state")).and_then(|s| s.as_str());
    let mut snap = ConnectivitySnapshot {
        hostname: hostname.to_string(),
        dc_connected: None,
        wifi_connected: None,
        ethernet_connected: None,
        has_internet: None,
        concentrator_present: None,
        online: None,
        northbridge_reachable: None,
        chirpstack_running: None,
        northbridge_update_ok: None,
        southbridge_update_ok: None,
    };
    match kind {
        RadioDiagKind::Northbridge => {
            let down = match state {
                None | Some("disabled") => Some(false),
                Some("unreachable") => Some(true),
                Some("alive" | "error") => Some(false),
                _ => None, // `starting`, or a state this build does not know
            };
            snap.northbridge_reachable = radio_verdict(cache, hostname, "northbridge", down, now);
            snap.northbridge_update_ok = Some(northbridge_update_ok(doc));
        }
        RadioDiagKind::Lorawan => {
            let unit = doc
                .and_then(|d| d.pointer("/chirpstack/unit"))
                .and_then(|u| u.as_str());
            let down = match (state, unit) {
                (None | Some("disabled"), _) => Some(false),
                (Some("starting"), _) => None,
                (_, Some("active")) => Some(false),
                (_, Some(_)) => Some(true),
                (_, None) => None,
            };
            snap.chirpstack_running = radio_verdict(cache, hostname, "chirpstack", down, now);
        }
    }
    if snap.northbridge_reachable.is_none()
        && snap.chirpstack_running.is_none()
        && snap.northbridge_update_ok.is_none()
    {
        return Ok(());
    }
    let ts_iso = iso_now();
    let lookup = StorageActiveLookup(storage);
    let actions = portal_domain::on_connectivity(&lookup, &snap, &ts_iso);
    apply_alarm_actions(
        storage, hostname, None, None, None, None, actions, notify_tx, nudge_tx,
    )
}

/// Whether the northbridge document reports a working firmware update: `false` only for
/// `update.state == "failed"` — the Hub spent its retry budget (§3.4).
fn northbridge_update_ok(doc: Option<&serde_json::Value>) -> bool {
    doc.and_then(|d| d.pointer("/update/state"))
        .and_then(|s| s.as_str())
        != Some("failed")
}

/// Whether `system/info.southbridge` reports a working firmware update: `false` only for
/// `update.state == "failed"` (`plan/system/southbridge_update.md` §3.7).
fn southbridge_update_ok(sb: Option<&fiber_protocol::payloads::system_info::Southbridge>) -> bool {
    sb.and_then(|sb| sb.update.as_ref())
        .and_then(|u| u.state.as_deref())
        != Some("failed")
}

/// `{prefix}/{host}/status` — feeds `DeviceStatus offline` into the
/// system-alert checker (work order item 2). Also re-arms the debounce
/// promptly on a return to online, rather than waiting for the next
/// `system/info` publish.
///
/// It also drives the Hub's connectivity alarm `sys:online` (decisions.md
/// #30a) under UX-058's edge rule: `offline` reaches the engine only as an
/// online→offline edge this process saw, so a status already retained as
/// offline when the portal starts opens nothing; `online` always passes, to
/// clear. The onset-only `DeviceOffline` system alert above stays: it goes
/// to the system-alert recipients, the alarm to the Hub's alarm recipients.
fn handle_status(
    storage: &Storage,
    cache: &mut PersistCache,
    hostname: &str,
    status: DeviceStatus,
    notify_tx: &mpsc::UnboundedSender<NotifyJob>,
    nudge_tx: &broadcast::Sender<AlarmNudge>,
) -> Result<(), portal_storage::StorageError> {
    let online = matches!(status, DeviceStatus::Online);
    // The cache moves only once the alarm write has gone through: a write
    // that fails is retried (or spilled and replayed) through this same
    // function, and a cache already holding the new state would read the
    // edge as none — `handle_system_info` keeps the same order.
    let before = cache.online.get(hostname).copied();

    let edge = match online {
        false if before != Some(true) => None,
        now => Some(now),
    };
    let snap = ConnectivitySnapshot {
        hostname: hostname.to_string(),
        dc_connected: None,
        wifi_connected: None,
        ethernet_connected: None,
        has_internet: None,
        concentrator_present: None,
        online: edge,
        northbridge_reachable: None,
        chirpstack_running: None,
        northbridge_update_ok: None,
        southbridge_update_ok: None,
    };
    let lookup = StorageActiveLookup(storage);
    let actions = portal_domain::on_connectivity(&lookup, &snap, &iso_now());
    apply_alarm_actions(
        storage, hostname, None, None, None, None, actions, notify_tx, nudge_tx,
    )?;

    cache.online.insert(hostname.to_string(), online);
    send_system_snapshot(cache, hostname, notify_tx);
    Ok(())
}

fn send_system_snapshot(
    cache: &PersistCache,
    hostname: &str,
    notify_tx: &mpsc::UnboundedSender<NotifyJob>,
) {
    let entry = cache
        .system_snapshot
        .get(hostname)
        .cloned()
        .unwrap_or_default();
    let online = cache.online.get(hostname).copied().unwrap_or(true);
    let snapshot = SystemSnapshot {
        battery_pct: entry.battery_pct,
        storage_used_pct: entry.storage_used_pct,
        // The portal's own partition, measured here (HN-18): it has no wire source, and left
        // unmeasured the "Portal storage low" alert could never fire. It is the Hub's the
        // portal runs on: a Client's row does not carry the Server's disk.
        portal_disk_pct: cache
            .runs_portal(hostname)
            .then(|| {
                cache
                    .data_dir
                    .as_deref()
                    .and_then(crate::diskspace::used_pct)
            })
            .flatten(),
        network_down: !(entry.wifi_connected.unwrap_or(true)
            || entry.ethernet_connected.unwrap_or(true)),
        has_internet: entry.has_internet.unwrap_or(true),
        online,
        version: entry.version,
    };
    if notify_tx
        .send(NotifyJob::SystemSnapshot {
            hostname: hostname.to_string(),
            snapshot,
        })
        .is_err()
    {
        tracing::warn!(hostname, "notify channel closed — system snapshot dropped");
    }
}

/// One Node's `field_alarm_states` (+ any native fPort-3 `events[].type ==
/// "alarm"`) → `portal_domain::on_field_states`/`on_native_event`. Runs
/// independently of `persist_lorawan_sensors`'s `last_seen` gate — alarm
/// state changes matter even on a payload the telemetry-history sink
/// dedups away, and re-running the engine on a genuine duplicate is a no-op
/// (same severity ⇒ `Nothing`, an already-active native alarm ⇒ `Nothing`).
fn run_lorawan_alarm_engine(
    storage: &Storage,
    cache: &PersistCache,
    hostname: &str,
    node_key: &str,
    sensor: &LorawanSensor,
    notify_tx: &mpsc::UnboundedSender<NotifyJob>,
    nudge_tx: &broadcast::Sender<AlarmNudge>,
) -> Result<(), portal_storage::StorageError> {
    let location = portal_location(storage, cache, hostname, THRESHOLD_SCOPE_NODE, node_key);

    if !sensor.field_alarm_states.is_empty() {
        let mut states = HashMap::new();
        let mut labels = HashMap::new();
        let mut values = HashMap::new();
        let mut thresholds = HashMap::new();

        for (field, state_str) in &sensor.field_alarm_states {
            let Ok(state) = WireState::try_from(state_str.to_ascii_uppercase().as_str()) else {
                continue;
            };
            states.insert(field.clone(), state);
            let label = lorawan_field(field)
                .map(|f| f.label.to_string())
                .unwrap_or_else(|| field.clone());
            labels.insert(field.clone(), label);
        }
        for (field, value) in &sensor.fields {
            values.insert(field.clone(), *value);
        }
        if let Some(wire_thresholds) = &sensor.field_thresholds {
            for t in wire_thresholds {
                thresholds.insert(
                    t.field.clone(),
                    ThresholdBounds {
                        critical_low: t.critical_low,
                        warning_low: t.warning_low,
                        warning_high: t.warning_high,
                        critical_high: t.critical_high,
                    },
                );
            }
        }

        if !states.is_empty() {
            let input = FieldStates {
                hostname: hostname.to_string(),
                source: FieldSource::Node {
                    node_key: node_key.to_string(),
                },
                states,
                labels,
                values,
                thresholds,
            };
            let ts_iso = sensor.last_seen.clone().unwrap_or_else(iso_now);
            let lookup = StorageActiveLookup(storage);
            let actions = portal_domain::on_field_states(&lookup, &input, &ts_iso);
            apply_alarm_actions(
                storage,
                hostname,
                Some(node_key),
                sensor.serial_number.as_deref(),
                None,
                location.clone(),
                actions,
                notify_tx,
                nudge_tx,
            )?;
        }
    }

    for event in &sensor.events {
        if event.event_type != "alarm" {
            continue;
        }
        let Some(native) = decode_native_event(hostname, node_key, event) else {
            continue;
        };
        let lookup = StorageActiveLookup(storage);
        let actions = portal_domain::on_native_event(&lookup, &native);
        apply_alarm_actions(
            storage,
            hostname,
            Some(node_key),
            sensor.serial_number.as_deref(),
            None,
            location.clone(),
            actions,
            notify_tx,
            nudge_tx,
        )?;
    }

    Ok(())
}

/// The engine identity of a native alarm event: `(slot, source, quantity)` of
/// [`NativeEvent`].
///
/// v2.0.0 (`node-firmware-v2.0.0.md` §4) names the target `slot` (`mb`, `s1`.. — or its number
/// on the legacy stack), `channel` and `rule` (0..15, `null` for a watchdog). The engine keys on
/// the rule as before, `native:{rule}`; a watchdog keys on `native:255:{slot}:{channel}`
/// (`255:s1:temperature`, `255:mb:battery-voltage`, `255:s1:sensor_mismatch` for a mismatch,
/// which names no channel), and the alarm is named after the channel (`Node · Temperature`,
/// `Node · S1 temperature aux`). *Amended 2026-10-14 (v2.0.0 minimum):* an event without
/// `rule` (the v1.4.0 shape) names nothing; both stacks emit `rule`.
fn native_identity(
    extra: &serde_json::Map<String, serde_json::Value>,
) -> Option<(i64, Option<String>, Option<String>)> {
    use control_radio::decode::sensor_types;
    let rule = extra.get("rule")?;
    let slot_label = match extra.get("slot")? {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => sensor_types::slot_label(u32::try_from(n.as_u64()?).ok()?),
        _ => return None,
    };
    let slot_no = sensor_types::slot_of_label(&slot_label);
    let channel = match extra.get("channel") {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        // The legacy stack's channel number, named through the registry.
        Some(serde_json::Value::Number(n)) => {
            let ch = n.as_u64().and_then(|c| u8::try_from(c).ok())?;
            let w1_type = match extra.get("sensor_type") {
                Some(serde_json::Value::Number(t)) => t.as_u64().and_then(|t| u8::try_from(t).ok()),
                Some(serde_json::Value::String(t)) => sensor_types::w1_type_id(t),
                _ => None,
            };
            Some(
                slot_no
                    .and_then(|slot| sensor_types::channel(slot, w1_type, ch))
                    .map_or_else(|| format!("ch{ch}"), |c| c.name.to_string()),
            )
        }
        _ => None,
    };
    // A mismatch concerns the whole slot and names no channel: its type stands in.
    let target = channel.or_else(|| {
        extra
            .get("type")
            .or_else(|| extra.get("side"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    })?;
    let quantity = if slot_no == Some(0) {
        target.clone()
    } else {
        format!("{slot_label} {target}")
    };
    match rule.as_i64() {
        Some(rule) => Some((rule, None, Some(quantity))),
        None => Some((
            portal_domain::alarm_engine::NO_DATA_SLOT,
            Some(format!("{slot_label}:{target}")),
            Some(quantity),
        )),
    }
}

/// Decodes one native fPort-3 alarm event from a `LorawanEvent`'s flattened
/// `extra` map (`{slot, source, quantity, edge, alarm_type|side, value,
/// base_time, rel_s, ts}`, mqtt.md). `None` on a shape that doesn't decode
/// (missing `slot`/`edge`, or an unrecognized `edge`) — logged and dropped
/// by the caller, same as every other malformed-payload path in this file.
fn decode_native_event(
    hostname: &str,
    node_key: &str,
    event: &LorawanEvent,
) -> Option<NativeEvent> {
    let extra = &event.extra;
    let (slot, source, quantity) = native_identity(extra)?;
    let edge = match extra.get("edge")?.as_str()?.to_ascii_lowercase().as_str() {
        "raise" => Edge::Raise,
        "clear" => Edge::Clear,
        other => {
            tracing::debug!(
                hostname,
                node_key,
                edge = other,
                "native alarm event: unrecognized edge — skipped"
            );
            return None;
        }
    };
    // The wire's ambiguous key: either `side` or `alarm_type` carries
    // "high"/"low" (NativeEvent's own doc — the caller resolves which key
    // existed).
    let side = extra
        .get("side")
        .and_then(|v| v.as_str())
        .or_else(|| extra.get("alarm_type").and_then(|v| v.as_str()))
        .map(str::to_string);
    let value = extra.get("value").and_then(|v| v.as_f64());
    let base_time = extra.get("base_time").and_then(|v| v.as_i64()).unwrap_or(0);
    let rel_s = extra.get("rel_s").and_then(|v| v.as_i64()).unwrap_or(0);
    let ts_iso = event.ts.clone().unwrap_or_else(iso_now);

    Some(NativeEvent {
        hostname: hostname.to_string(),
        node_key: node_key.to_string(),
        slot,
        source,
        quantity,
        edge,
        side,
        value,
        base_time,
        rel_s,
        ts_iso,
    })
}

/// One EYE tag → `portal_domain::on_field_states`.
///
/// Decision #41: the portal grades a Beacon's thresholdable fields itself, from the values
/// on `eye/sensors` and the bounds and hysteresis in `field_thresholds`
/// (`THRESHOLD_SCOPE_EYE`). The Hub's `field_alarm_states` is read only for the fields with
/// no bounds (the battery's low-battery flag).
pub(crate) fn run_eye_alarm_engine(
    storage: &Storage,
    cache: &PersistCache,
    hostname: &str,
    tag: &EyeTag,
    notify_tx: &mpsc::UnboundedSender<NotifyJob>,
    nudge_tx: &broadcast::Sender<AlarmNudge>,
) -> Result<(), portal_storage::StorageError> {
    // A Beacon no Hub has registered — a candidate, or one just removed that keeps
    // advertising — has no bounds. An alarm it raised while it was registered would then
    // never be graded again and stay open for good (seen 2026-09-23 on fiber-4e970f4a). So
    // every thresholdable field reads Normal for it, which walks the engine's clearing branch
    // and raises nothing.
    let unregistered = tag.discovered == Some(true);
    let mac = tag.mac.to_uppercase();
    let label_of = |field: &str| {
        eye_field(field)
            .map(|f| f.label.to_string())
            .unwrap_or_else(|| field.to_string())
    };
    let source = FieldSource::Beacon { mac: mac.clone() };
    let lookup = StorageActiveLookup(storage);

    let mut states = HashMap::new();
    let mut labels = HashMap::new();
    if unregistered {
        for def in eye_registry().iter().filter(|f| f.thresholdable) {
            states.insert(def.name.to_string(), WireState::Normal);
            labels.insert(def.name.to_string(), def.label.to_string());
        }
    } else {
        for (field, state_str) in &tag.field_alarm_states {
            if eye_field(field).is_some_and(|f| f.thresholdable) {
                continue;
            }
            let Ok(state) = WireState::try_from(state_str.to_ascii_uppercase().as_str()) else {
                continue;
            };
            states.insert(field.clone(), state);
            labels.insert(field.clone(), label_of(field));
        }
    }

    let mut values = HashMap::new();
    if let Some(v) = tag.temperature_c {
        values.insert("temperature".to_string(), v);
    }
    if let Some(v) = tag.humidity_pct {
        values.insert("humidity".to_string(), v as f64);
    }
    // A switched-off sensor is not graded (GitLab #120).
    let disabled = storage.disabled_sensor_fields(hostname, THRESHOLD_SCOPE_EYE, &mac)?;
    values.retain(|field, _| !disabled.contains(field));
    states.retain(|field, _| !disabled.contains(field));

    let mut thresholds = HashMap::new();
    if !unregistered {
        let defaults = crate::threshold_defaults::configured();
        let own_hysteresis =
            storage.field_threshold_hysteresis_map(hostname, THRESHOLD_SCOPE_EYE, &mac)?;
        match storage.list_field_thresholds(hostname, THRESHOLD_SCOPE_EYE, &mac) {
            Ok(rows) => {
                for row in rows.into_iter().filter(|r| !disabled.contains(&r.field)) {
                    let label = label_of(&row.field);
                    // GitLab #80: `enabled` is the portal's own switch. A switched-off band
                    // reads Normal rather than being dropped, so the engine also CLEARS an
                    // alarm the band had already raised.
                    if !row.enabled {
                        states.insert(row.field.clone(), WireState::Normal);
                        labels.insert(row.field.clone(), label);
                        continue;
                    }
                    let bounds = ThresholdBounds {
                        critical_low: row.critical_low,
                        warning_low: row.warning_low,
                        warning_high: row.warning_high,
                        critical_high: row.critical_high,
                    };
                    if let Some(value) = values.get(&row.field) {
                        let h = crate::threshold_defaults::hysteresis(
                            &defaults,
                            THRESHOLD_SCOPE_EYE,
                            &row.field,
                            own_hysteresis.get(&row.field).copied(),
                        );
                        let held =
                            portal_domain::held_field_state(&lookup, hostname, &source, &label);
                        states.insert(row.field.clone(), grade_value(&bounds, h, *value, held));
                        labels.insert(row.field.clone(), label);
                    }
                    thresholds.insert(row.field.clone(), bounds);
                }
            }
            Err(err) => tracing::warn!(hostname, mac, %err, "failed to load eye field thresholds"),
        }
    }
    if states.is_empty() {
        return Ok(());
    }

    let input = FieldStates {
        hostname: hostname.to_string(),
        source,
        states,
        labels,
        values,
        thresholds,
    };
    let ts_iso = tag.last_seen_ts.map(unix_to_iso).unwrap_or_else(iso_now);
    let actions = portal_domain::on_field_states(&lookup, &input, &ts_iso);
    let location = portal_location(storage, cache, hostname, THRESHOLD_SCOPE_EYE, &mac);
    apply_alarm_actions(
        storage,
        hostname,
        None,
        None,
        Some(&mac),
        location,
        actions,
        notify_tx,
        nudge_tx,
    )
}

fn unix_to_iso(ts: i64) -> String {
    OffsetDateTime::from_unix_timestamp(ts)
        .ok()
        .and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_else(iso_now)
}

/// Clear every open alarm of a Node that was removed from its Hub: the value alarms of its
/// fields, its own (native) alarms and the offline alarm — every class keys its row
/// `lrw:{node_key}:…`, on both carriers. The same for LoRaWAN and P2P, since the tombstone is.
fn close_removed_node_alarms(
    storage: &Storage,
    hostname: &str,
    node_key: &str,
    nudge_tx: &broadcast::Sender<AlarmNudge>,
) -> Result<(), portal_storage::StorageError> {
    let prefix = portal_domain::node_sensor_prefix(&node_key.to_lowercase());
    // PF-3: the Node's "configuration not delivered" warning goes with it.
    let config_warning = crate::pending::config_warning_sensor_id(node_key);
    let cleared_at = iso_now();
    let mut closed = 0usize;
    for row in storage.active_alarms_any_case(hostname)? {
        let sensor_id = row.sensor_id.to_lowercase();
        if !sensor_id.starts_with(&prefix) && sensor_id != config_warning {
            continue;
        }
        // `device_removed` (V37): the list says why it ended.
        if storage.close_alarm_with_reason(
            &row.id,
            &cleared_at,
            crate::alarms::orphans::EndReason::DeviceRemoved.as_str(),
        )? {
            closed += 1;
        }
    }
    if closed > 0 {
        tracing::info!(
            hostname,
            node_key,
            closed,
            "node removed; its open alarms closed"
        );
        let _ = nudge_tx.send(AlarmNudge {
            hostname: hostname.to_string(),
            activated_ids: Vec::new(),
            escalated_ids: Vec::new(),
        });
    }
    Ok(())
}

fn iso_now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

// ---- LorawanSensors (M5-E telemetry/history persistence, unchanged) -----

/// `last_seen` gate (`lorawan_record_on_uplink_only`): `true` when this
/// Node's `last_seen` advanced since the last call (including the very
/// first observation); `false` — meaning "nothing new to persist" — when it
/// repeats the cached value.
///
/// Read-only on purpose, paired with [`remember_last_seen`]. Marking the cache
/// here (as this did until the spill journal landed) makes the gate lie when
/// the write that follows it fails: the retry, or the replay after a
/// maintenance run, sees "already persisted" and skips the row for good. The
/// cache may only advance once the row is committed.
fn last_seen_advanced(
    cache: &HashMap<NodeKey, String>,
    hostname: &str,
    node_key: &str,
    last_seen: &str,
) -> bool {
    let key = (hostname.to_string(), node_key.to_string());
    cache.get(&key).map(String::as_str) != Some(last_seen)
}

/// Record that this `last_seen` is now durably persisted. Called only after
/// the sensor's rows have committed — see [`last_seen_advanced`].
fn remember_last_seen(
    cache: &mut HashMap<NodeKey, String>,
    hostname: &str,
    node_key: &str,
    last_seen: &str,
) {
    cache.insert(
        (hostname.to_string(), node_key.to_string()),
        last_seen.to_string(),
    );
}

/// RFC3339 → unix seconds, or `None` on a missing/unparseable timestamp.
/// Pure so the gate/mapping logic around it stays unit-testable.
fn parse_rfc3339(s: &str) -> Option<i64> {
    OffsetDateTime::parse(s, &Rfc3339)
        .ok()
        .map(|dt| dt.unix_timestamp())
}

/// One Node's `fields` → `node_field_readings` rows at `ts`, alarm state
/// looked up per field from `field_alarm_states`.
fn lorawan_field_rows(
    hostname: &str,
    node_key: &str,
    ts: i64,
    sensor: &LorawanSensor,
) -> Vec<NodeFieldReading> {
    sensor
        .fields
        .iter()
        .map(|(field, value)| NodeFieldReading {
            hostname: hostname.to_string(),
            node_key: node_key.to_string(),
            field: field.clone(),
            ts,
            value: Some(*value),
            alarm_state: sensor.field_alarm_states.get(field).cloned(),
            source: SOURCE_LIVE.to_string(),
        })
        .collect()
}

/// One Node's `counters` → `node_counter_readings` rows at `ts`.
fn lorawan_counter_rows(
    hostname: &str,
    node_key: &str,
    ts: i64,
    sensor: &LorawanSensor,
) -> Vec<NodeCounterReading> {
    sensor
        .counters
        .iter()
        .map(|(counter, value)| NodeCounterReading {
            hostname: hostname.to_string(),
            node_key: node_key.to_string(),
            counter: counter.clone(),
            ts,
            value: Some(*value),
        })
        .collect()
}

fn persist_lorawan_sensors(
    storage: &Storage,
    hostname: &str,
    payload: LorawanSensorsMsg,
    node_last_seen: &mut HashMap<NodeKey, String>,
) -> Result<(), portal_storage::StorageError> {
    for sensor in &payload.sensors {
        let node_key = sensor.dev_eui.to_lowercase();

        let Some(last_seen) = sensor.last_seen.as_deref() else {
            tracing::debug!(%hostname, %node_key, "lorawan sensor without last_seen — skipped");
            continue;
        };
        if !last_seen_advanced(node_last_seen, hostname, &node_key, last_seen) {
            continue;
        }
        let ts = parse_rfc3339(last_seen).unwrap_or_else(|| {
            tracing::warn!(%hostname, %node_key, last_seen, "failed to parse last_seen — using now");
            chrono_free_now_unix()
        });

        match serde_json::to_string(sensor) {
            Ok(raw_json) => {
                storage.insert_raw_message(hostname, &node_key, ts, &raw_json)?;
            }
            Err(err) => {
                tracing::warn!(%hostname, %node_key, %err, "failed to serialize lorawan sensor for raw message log");
            }
        }

        let field_rows = lorawan_field_rows(hostname, &node_key, ts, sensor);
        if !field_rows.is_empty() {
            storage.insert_field_readings(&field_rows)?;
        }
        let counter_rows = lorawan_counter_rows(hostname, &node_key, ts, sensor);
        if !counter_rows.is_empty() {
            storage.insert_counter_readings(&counter_rows)?;
        }

        for event in &sensor.events {
            let Some(event_ts) = event.ts.as_deref().and_then(parse_rfc3339) else {
                tracing::debug!(%hostname, %node_key, event_type = %event.event_type, "lorawan event without usable ts — skipped");
                continue;
            };
            storage.insert_event(&NodeEvent {
                hostname: hostname.to_string(),
                node_key: node_key.clone(),
                event_type: event.event_type.clone(),
                ts: event_ts,
                // Not `event_seqs`: the v1 aggregate's `events` is the Node's rolling window
                // of recent events (32 on the v1 Hub), republished on every uplink, so an
                // event's place in it moves from one message to the next and a position-based
                // key would store it again each time. Its events carry their own times too.
                seq: 0,
                payload: serde_json::to_value(event).ok().map(|v| v.to_string()),
            })?;
        }

        if let Some(thresholds) = &sensor.field_thresholds {
            for threshold in thresholds {
                // **`enabled` is the portal's, not the firmware's** (§21
                // review, item 18 — pre-existing). The echo is authoritative
                // about the four bounds and says nothing at all about
                // whether the portal evaluates them: `enabled` is a
                // portal-side switch a manager sets (`set_node_threshold`,
                // `set_beacon_threshold`). Writing `true` here re-enabled a
                // silenced threshold on the next aggregate publish — every
                // ~30 s — so a manager could not silence a noisy threshold
                // at all. The stored flag is read back and preserved; a
                // field the portal has never stored defaults to enabled,
                // which is what an unmanaged threshold has always been.
                // The warning bounds are the portal's too (decision #41, devices.node.md
                // amendment 2026-10-13): a manager's save stores them at once and the Hub
                // only learns them on the field's next send, so an echo must not put the
                // Hub's older ones back. A field the portal has never stored takes the echo's.
                let stored = storage.get_field_threshold(
                    hostname,
                    THRESHOLD_SCOPE_NODE,
                    &node_key,
                    &threshold.field,
                )?;
                let enabled = stored.as_ref().map(|s| s.enabled).unwrap_or(true);
                let (warning_low, warning_high) = match &stored {
                    Some(s) => (s.warning_low, s.warning_high),
                    None => (threshold.warning_low, threshold.warning_high),
                };
                storage.upsert_field_threshold(
                    &NewFieldThreshold {
                        hostname: hostname.to_string(),
                        scope: THRESHOLD_SCOPE_NODE.to_string(),
                        entity: node_key.clone(),
                        field: threshold.field.clone(),
                        critical_low: threshold.critical_low,
                        warning_low,
                        warning_high,
                        critical_high: threshold.critical_high,
                        enabled,
                    },
                    Some("firmware-echo"),
                )?;
            }
        }

        // Every row for this sensor is committed; only now may the dedup gate
        // advance. A failure above leaves it untouched, so the retry (or the
        // post-VACUUM replay) writes the sensor rather than skipping it.
        remember_last_seen(node_last_seen, hostname, &node_key, last_seen);
    }
    Ok(())
}

// ---- WP-E: the v2 radio northbound family --------------------------------
//
// `proximos/{host}/{transport}/nodes/{id}/{reading,alarm}` → the *same rows*
// `persist_lorawan_sensors` writes above (`node_raw_messages`,
// `node_field_readings`, `node_counter_readings`, `node_events`), so both
// ingest paths land in one table set keyed by `node_key` and every query,
// export and retention sweep already written keeps working unchanged.
//
// Three shape differences between the two envelopes, and what each costs:
//
// - **Dedup.** v1 gated on "`last_seen` advanced"; the v2 envelope carries
//   `message_id` (plan.md §7 step 3), which is ChirpStack's own
//   `deduplicationId` when there is one — the identity of a deduplication
//   set, so every gateway's copy of a frame shares it rather than relying on
//   two timestamps agreeing to the microsecond. The row-level tables dedup
//   themselves (`INSERT OR IGNORE` on natural keys), but `node_raw_messages`
//   is a plain `INSERT`, so without this gate a redelivered document would
//   append a second raw copy of the same uplink.
// - **No `field_alarm_states` / `field_thresholds`.** The v1 Hub echoed both
//   back on the wire and this sink stored the thresholds with actor
//   `firmware-echo`. `proximos-control` publishes neither: it decodes, it
//   does not evaluate. So a v2 reading writes `alarm_state: None` on its
//   field rows and touches no threshold row. ⚠️ Carried consequence:
//   threshold-derived (HIGH/LOW) alarming for v2 Nodes needs portal-side
//   evaluation against the `field_thresholds` table — the engine call exists
//   (`portal_domain::on_field_states`) but nothing computes the states for
//   it, so today only the device's own native fPort-3 alarms raise. Flagged
//   for the UI work package that owns Node thresholds.
// - **Timestamps.** `received_at` (RFC3339) is the row `ts` and the raw
//   message's `received_at`, matching v1's use of `last_seen` verbatim.

/// Enough to absorb a multi-gateway burst and a broker redelivery without
/// growing per node without bound.
const REMEMBERED_MESSAGE_IDS: usize = 16;

/// True the first time this `(node, message_id)` is seen; false for a
/// redelivery. Remembers a short window per node — see `node_message_ids`.
///
/// Read-only, paired with [`remember_node_message_id`], for the same reason
/// [`last_seen_advanced`] is: a gate that marks before the write turns a failed
/// write into a permanently skipped row.
fn node_reading_is_new(
    cache: &PersistCache,
    hostname: &str,
    node_key: &str,
    message_id: &str,
) -> bool {
    cache
        .node_message_ids
        .get(&(hostname.to_string(), node_key.to_string()))
        .is_none_or(|seen| !seen.iter().any(|id| id == message_id))
}

/// Record that this document's rows are committed — see [`node_reading_is_new`].
fn remember_node_message_id(
    cache: &mut PersistCache,
    hostname: &str,
    node_key: &str,
    message_id: &str,
) {
    let seen = cache
        .node_message_ids
        .entry((hostname.to_string(), node_key.to_string()))
        .or_default();
    if seen.iter().any(|id| id == message_id) {
        return;
    }
    seen.push_back(message_id.to_string());
    if seen.len() > REMEMBERED_MESSAGE_IDS {
        seen.pop_front();
    }
}

/// One `NodeReading` → the Node telemetry rows. Shared by `reading` and
/// `alarm`: the envelope is the same, an alarm batch simply carries events
/// where a telemetry frame carries fields.
fn persist_node_reading(
    storage: &Storage,
    cache: &mut PersistCache,
    hostname: &str,
    node_key: &str,
    reading: &NodeReadingMsg,
) -> Result<(), portal_storage::StorageError> {
    if !node_reading_is_new(cache, hostname, node_key, &reading.message_id) {
        tracing::debug!(
            %hostname, %node_key, message_id = %reading.message_id,
            "duplicate node reading — skipped"
        );
        return Ok(());
    }

    let ts = parse_rfc3339(&reading.received_at).unwrap_or_else(|| {
        tracing::warn!(
            %hostname, %node_key, received_at = %reading.received_at,
            "failed to parse received_at — using now"
        );
        chrono_free_now_unix()
    });

    match serde_json::to_string(reading) {
        Ok(raw_json) => {
            storage.insert_raw_message(hostname, node_key, ts, &raw_json)?;
        }
        Err(err) => {
            tracing::warn!(%hostname, %node_key, %err, "failed to serialize node reading for the raw message log");
        }
    }

    let field_rows: Vec<NodeFieldReading> = reading
        .fields
        .iter()
        .map(|(field, value)| NodeFieldReading {
            hostname: hostname.to_string(),
            node_key: node_key.to_string(),
            field: field.clone(),
            ts,
            value: Some(*value),
            // Nothing on the v2 wire says what state a field is in — see the
            // module note above.
            alarm_state: None,
            source: SOURCE_LIVE.to_string(),
        })
        .collect();
    if !field_rows.is_empty() {
        storage.insert_field_readings(&field_rows)?;
    }

    let counter_rows: Vec<NodeCounterReading> = reading
        .counters
        .iter()
        // `u64` on the wire, `i64` in the column. A count past `i64::MAX` is
        // not a count; dropped rather than wrapped into a negative one.
        .filter_map(|(counter, value)| {
            i64::try_from(*value).ok().map(|value| NodeCounterReading {
                hostname: hostname.to_string(),
                node_key: node_key.to_string(),
                counter: counter.clone(),
                ts,
                value: Some(value),
            })
        })
        .collect();
    if !counter_rows.is_empty() {
        storage.insert_counter_readings(&counter_rows)?;
    }

    for (event, seq) in reading.events.iter().zip(event_seqs(&reading.events)) {
        let Some(event_ts) = parse_rfc3339(&event.ts) else {
            tracing::debug!(%hostname, %node_key, event_type = %event.event_type, "node event without usable ts — skipped");
            continue;
        };
        storage.insert_event(&NodeEvent {
            hostname: hostname.to_string(),
            node_key: node_key.to_string(),
            event_type: event.event_type.clone(),
            ts: event_ts,
            seq,
            payload: serde_json::to_value(event).ok().map(|v| v.to_string()),
        })?;
        record_native_activation(cache, hostname, node_key, event, event_ts);
    }

    // Committed — only now does the redelivery gate close (see
    // `node_reading_is_new`).
    remember_node_message_id(cache, hostname, node_key, &reading.message_id);

    Ok(())
}

/// How far apart the pages of one alarm batch number their events — see [`event_seqs`].
const EVENT_SEQ_PAGE_STRIDE: i64 = 256;

/// Each event's `node_events.seq`, in the order of the message's `events[]`:
/// `page_index × 256 + index` (devices.backend.md §23, V27).
///
/// Every event of one message carries the message's `ts`, so `(event_type, ts)` alone kept only
/// the first alarm of a page. `index` tells the events of a message apart, and `page_index` —
/// the `alarm_batch` event's, 0 for a message that is not paged — the pages of one batch, which
/// can arrive within one second (0413's c1290 and c1291 were consecutive frames). A redelivered
/// message numbers its events the same, so its rows are still ignored. The index counts every
/// event, including one skipped for its `ts`, so the numbering depends on the message alone. A
/// page holds a handful of events; one of more than 256 would reach into the next page's.
fn event_seqs(
    events: &[control_radio::decode::reading::NodeEvent],
) -> impl Iterator<Item = i64> + '_ {
    let page_index = events
        .iter()
        .find(|e| e.event_type == "alarm_batch")
        .and_then(|e| e.extra.get("page_index")?.as_i64())
        .unwrap_or(0);
    (0i64..).take(events.len()).map(move |index| {
        page_index
            .saturating_mul(EVENT_SEQ_PAGE_STRIDE)
            .saturating_add(index)
    })
}

/// The portal's one threshold grade (plan/control/alarm_hysteresis.md, decision #41): the
/// shared `fiber_protocol::grading` rule with the field's hysteresis, stepping down from the
/// state the sensor holds (`held`, from its open alarm; `None` = nothing to hold).
fn grade_value(
    bounds: &ThresholdBounds,
    hysteresis: f64,
    value: f64,
    held: Option<WireState>,
) -> WireState {
    use fiber_protocol::grading::{Bounds, Grade};
    let band = Bounds {
        critical_low: bounds.critical_low,
        warning_low: bounds.warning_low,
        warning_high: bounds.warning_high,
        critical_high: bounds.critical_high,
    };
    let previous = held.and_then(|s| match s {
        WireState::Normal => Some(Grade::Normal),
        WireState::Warning => Some(Grade::Warning),
        WireState::Critical => Some(Grade::Critical),
        _ => None,
    });
    match band.grade(value, hysteresis, previous) {
        Grade::Normal => WireState::Normal,
        Grade::Warning => WireState::Warning,
        Grade::Critical => WireState::Critical,
    }
}

/// A v2 telemetry reading against the Node's bounds → `portal_domain::on_field_states`
/// (nodes_phase2.cs.md P1, decision by Hynek 2026-09-24).
///
/// The portal evaluates a Node's thresholds, warning and critical, on every telemetry message
/// and on nothing else: that is the sensor's alarm. The Node's own fPort-3 alarms are a bonus,
/// an alarm on the Node (`run_node_alarm_engine`). The bounds are the ones the portal sent
/// (`list_field_thresholds`, enabled only). A reported field without bounds is `NORMAL`, which
/// closes an alarm whose bounds were switched off or removed. The engine is idempotent (same
/// state ⇒ nothing), so a redelivered reading changes nothing.
fn run_node_threshold_engine(
    storage: &Storage,
    cache: &PersistCache,
    hostname: &str,
    node_key: &str,
    reading: &NodeReadingMsg,
    notify_tx: &mpsc::UnboundedSender<NotifyJob>,
    nudge_tx: &broadcast::Sender<AlarmNudge>,
) -> Result<(), portal_storage::StorageError> {
    if reading.fields.is_empty() {
        return Ok(());
    }
    // Enabled rows only: a switched-off row, a default band included, grades nothing
    // (threshold_defaults.md §3). Nor does a row on the battery (§5).
    let bounds: HashMap<String, ThresholdBounds> = storage
        .list_field_thresholds(hostname, THRESHOLD_SCOPE_NODE, node_key)?
        .into_iter()
        .filter(|t| t.enabled && crate::threshold_defaults::node_field_is_graded(&t.field))
        .map(|t| {
            (
                t.field,
                ThresholdBounds {
                    critical_low: t.critical_low,
                    warning_low: t.warning_low,
                    warning_high: t.warning_high,
                    critical_high: t.critical_high,
                },
            )
        })
        .collect();

    // Decision #41: graded with the field's hysteresis against the state it holds.
    let own_hysteresis =
        storage.field_threshold_hysteresis_map(hostname, THRESHOLD_SCOPE_NODE, node_key)?;
    let defaults = crate::threshold_defaults::configured();
    let lookup = StorageActiveLookup(storage);
    let source = FieldSource::Node {
        node_key: node_key.to_string(),
    };

    // A switched-off sensor is not graded (GitLab #120): its bound may still be stored until
    // the Send that removes it from the Hub and the Node.
    let disabled = storage.disabled_sensor_fields(hostname, THRESHOLD_SCOPE_NODE, node_key)?;
    let mut states = HashMap::new();
    let mut labels = HashMap::new();
    let mut values = HashMap::new();
    for (field, value) in reading
        .fields
        .iter()
        .filter(|(f, _)| !disabled.contains(*f))
    {
        let label = lorawan_field(field)
            .map(|f| f.label.to_string())
            .unwrap_or_else(|| field.clone());
        let state = bounds.get(field).map_or(WireState::Normal, |b| {
            let h = crate::threshold_defaults::hysteresis(
                &defaults,
                THRESHOLD_SCOPE_NODE,
                field,
                own_hysteresis.get(field).copied(),
            );
            let held = portal_domain::held_field_state(&lookup, hostname, &source, &label);
            grade_value(b, h, *value, held)
        });
        states.insert(field.clone(), state);
        labels.insert(field.clone(), label);
        values.insert(field.clone(), *value);
    }

    let location = portal_location(storage, cache, hostname, THRESHOLD_SCOPE_NODE, node_key);
    let input = FieldStates {
        hostname: hostname.to_string(),
        // Both carriers: a Node's alarms key under the one `lrw:` prefix
        // (`portal_domain::node_sensor_prefix`), which every reader of the rows expects.
        source,
        states,
        labels,
        values,
        thresholds: bounds,
    };
    // P-9 / N32: the same grade drives the value axis the pages read. Published before the
    // alarm writes, and on a redelivered reading too, so a restart repaints from the first one.
    if let Some(sink) = &cache.field_states {
        let graded: std::collections::BTreeMap<String, String> = input
            .states
            .iter()
            .map(|(field, state)| (field.clone(), state.as_str().to_string()))
            .collect();
        sink(hostname, node_key, &graded);
    }
    let actions = portal_domain::on_field_states(&lookup, &input, &reading.received_at);
    apply_alarm_actions(
        storage,
        hostname,
        Some(node_key),
        reading.serial.as_deref(),
        None,
        location,
        actions,
        notify_tx,
        nudge_tx,
    )
}

/// The `alarm` document's native fPort-3 events → `portal_domain::on_native_event`,
/// the same engine entry `run_lorawan_alarm_engine` uses for v1's.
///
/// Only the native path: the v2 envelope carries no `field_alarm_states`, so
/// there is nothing to feed `on_field_states` with (module note above).
fn run_node_alarm_engine(
    storage: &Storage,
    cache: &PersistCache,
    hostname: &str,
    node_key: &str,
    reading: &NodeReadingMsg,
    notify_tx: &mpsc::UnboundedSender<NotifyJob>,
    nudge_tx: &broadcast::Sender<AlarmNudge>,
) -> Result<(), portal_storage::StorageError> {
    let location = portal_location(storage, cache, hostname, THRESHOLD_SCOPE_NODE, node_key);

    for event in &reading.events {
        if v2_alarm_side(&event.event_type, &event.extra).is_none() {
            continue;
        }
        let Some(native) = decode_v2_native_event(hostname, node_key, event) else {
            continue;
        };
        let lookup = StorageActiveLookup(storage);
        let actions = portal_domain::on_native_event(&lookup, &native);
        apply_alarm_actions(
            storage,
            hostname,
            Some(node_key),
            reading.serial.as_deref(),
            None,
            location.clone(),
            actions,
            notify_tx,
            nudge_tx,
        )?;
    }

    Ok(())
}

/// F24 (node_reported_state.md §4, amended 2026-09-24): **a Node restart deactivates all of its
/// native alarms**, and the Node announces again what is still active (decision by Hynek). Its
/// latches live in RAM, so a reboot (after every saved configuration change, or a watchdog)
/// clears them without a `deactivate`, and the Node re-raises every condition that still holds
/// with a fresh `activate` after it boots.
///
/// Every Info dates the boot: `received_at − uptime_s`. An open native alarm of this Node raised
/// before that instant is closed through the engine's recovery path, unless the Node has already
/// re-raised that slot since the boot (its `activate` can reach the portal before the Info does;
/// the engine then kept the old row, and closing it would lose a live alarm). An Info whose
/// `active_alarms` is empty closes, on top, every open native alarm raised before it was received:
/// the Node holds no latch at all, whatever happened to its `deactivate`. Both bounds only reach
/// back in time, so a replayed older Info never closes a newer alarm.
///
/// *Amended 2026-09-27:* an Info within the grace window after a boot ([`restart_grace_s`])
/// closes nothing when it arrives. It is held, and at the end of the window only the restart
/// rule runs: an alarm raised before the boot and not re-raised since. A condition the Node
/// still holds is re-raised inside the window onto the open row, which the engine keeps, so it
/// stays one alarm with one notification instead of a close and a new row (bench 2026-09-27).
#[allow(clippy::too_many_arguments)]
fn close_forgotten_native_alarms(
    storage: &Storage,
    cache: &mut PersistCache,
    hostname: &str,
    node_key: &str,
    doc: &crate::state::NodeDeviceInfoMsg,
    now: i64,
    notify_tx: &mpsc::UnboundedSender<NotifyJob>,
    nudge_tx: &broadcast::Sender<AlarmNudge>,
) -> Result<(), portal_storage::StorageError> {
    let Some(received) = parse_rfc3339(&doc.received_at) else {
        return Ok(());
    };
    let none_latched = matches!(
        doc.device_info.get("active_alarms"),
        Some(serde_json::Value::Array(alarms)) if alarms.is_empty()
    );
    let booted = doc
        .device_info
        .get("uptime_s")
        .and_then(serde_json::Value::as_i64)
        .map(|uptime| received - uptime);
    if let Some(booted) = booted {
        let grace = restart_grace_s(cache, hostname, node_key);
        if received - booted < grace {
            // An Info right after a boot: every row raised between the boot and the Info is a
            // re-raise, so the restart rule alone decides, and not before the window is over.
            let due = booted + grace;
            if now < due {
                cache.restart_closes.insert(
                    (hostname.to_string(), node_key.to_string()),
                    PendingRestartClose {
                        doc: doc.clone(),
                        booted,
                        due,
                    },
                );
                return Ok(());
            }
            return close_native_alarms_forgotten_by(
                storage,
                cache,
                hostname,
                node_key,
                doc,
                Some(booted),
                false,
                notify_tx,
                nudge_tx,
            );
        }
    }
    if !none_latched && booted.is_none() {
        return Ok(());
    }
    close_native_alarms_forgotten_by(
        storage,
        cache,
        hostname,
        node_key,
        doc,
        booted,
        none_latched,
        notify_tx,
        nudge_tx,
    )
}

/// The restart grace window: 2 × the Node's `application.interval_report` from its last config
/// document, and at least 120 s (node_reported_state.md §4, amended 2026-09-27). The Node
/// re-raises a condition after its first sample and the rule's dwell, well inside two reports.
fn restart_grace_s(cache: &PersistCache, hostname: &str, node_key: &str) -> i64 {
    const MIN_GRACE_S: i64 = 120;
    cache
        .node_interval_report
        .get(&(hostname.to_string(), node_key.to_string()))
        .map_or(0, |interval| interval.saturating_mul(2))
        .max(MIN_GRACE_S)
}

/// Runs the restart closes whose grace window has passed ([`close_forgotten_native_alarms`]).
fn run_due_restart_closes(
    storage: &Storage,
    cache: &mut PersistCache,
    now: i64,
    notify_tx: &mpsc::UnboundedSender<NotifyJob>,
    nudge_tx: &broadcast::Sender<AlarmNudge>,
) -> Result<(), portal_storage::StorageError> {
    if cache.restart_closes.is_empty() {
        return Ok(());
    }
    let due: Vec<NodeKey> = cache
        .restart_closes
        .iter()
        .filter(|(_, pending)| pending.due <= now)
        .map(|(key, _)| key.clone())
        .collect();
    for key in due {
        let Some(pending) = cache.restart_closes.remove(&key) else {
            continue;
        };
        close_native_alarms_forgotten_by(
            storage,
            cache,
            &key.0,
            &key.1,
            &pending.doc,
            Some(pending.booted),
            false,
            notify_tx,
            nudge_tx,
        )?;
    }
    Ok(())
}

/// The close itself: `booted` closes a row raised before it that was not re-raised since, and
/// `by_empty_list` closes every row raised before the Info was received.
#[allow(clippy::too_many_arguments)]
fn close_native_alarms_forgotten_by(
    storage: &Storage,
    cache: &PersistCache,
    hostname: &str,
    node_key: &str,
    doc: &crate::state::NodeDeviceInfoMsg,
    booted: Option<i64>,
    by_empty_list: bool,
    notify_tx: &mpsc::UnboundedSender<NotifyJob>,
    nudge_tx: &broadcast::Sender<AlarmNudge>,
) -> Result<(), portal_storage::StorageError> {
    let Some(received) = parse_rfc3339(&doc.received_at) else {
        return Ok(());
    };
    let none_latched = by_empty_list;
    let location = portal_location(storage, cache, hostname, THRESHOLD_SCOPE_NODE, node_key);
    let prefix = format!("{}native:", portal_domain::node_sensor_prefix(node_key));

    // Any case: the Hub's retained replay can arrive before the portal knows its display
    // spelling, and the rows carry that spelling (Nodes test 2026-09-24, combined17 boot).
    for row in storage.active_alarms_any_case(hostname)? {
        // `native:<slot>`, or `native:255:<source>` for the no-data watchdog.
        let Some((slot, source)) = row.sensor_id.strip_prefix(&prefix).and_then(|rest| {
            let (slot, source) = match rest.split_once(':') {
                Some((slot, source)) => (slot, Some(source.to_string())),
                None => (rest, None),
            };
            slot.parse::<i64>().ok().map(|slot| (slot, source))
        }) else {
            continue;
        };
        let Some(raised) = parse_rfc3339(&row.triggered_at) else {
            continue;
        };
        let cleared_by_empty_list = none_latched && raised < received;
        let cleared_by_restart = match booted {
            Some(booted) if raised < booted => !reraised_since(
                storage,
                cache,
                &row.hostname,
                node_key,
                slot,
                source.as_deref(),
                booted,
            )?,
            _ => false,
        };
        if !(cleared_by_empty_list || cleared_by_restart) {
            continue;
        }
        let native = NativeEvent {
            hostname: row.hostname.clone(),
            node_key: node_key.to_string(),
            slot,
            source,
            // The row's name is the Node alarm's ("Node · Temperature", or the bare quantity
            // on a row opened before P2); the engine names the clear from the quantity.
            quantity: row
                .sensor_name
                .as_deref()
                .map(|n| n.strip_prefix("Node \u{b7} ").unwrap_or(n).to_lowercase()),
            edge: Edge::Clear,
            side: None,
            value: None,
            base_time: 0,
            rel_s: 0,
            ts_iso: doc.received_at.clone(),
        };
        let actions = portal_domain::on_native_event(&StorageActiveLookup(storage), &native);
        apply_alarm_actions(
            storage,
            &row.hostname,
            Some(node_key),
            None,
            None,
            location.clone(),
            actions,
            notify_tx,
            nudge_tx,
        )?;
    }
    Ok(())
}

/// The alarm slots the portal wrote before it owned the whole table: the fixed field order it
/// used then. A Node with a band but no recorded layout was written under this one.
const PRE_TABLE_ALARM_LAYOUT: &[&str] = &[
    "temperature",
    "humidity",
    "ext_temperature_1",
    "ext_temperature_2",
];

/// nodes_phase1.cs.md WP2: the portal owns a Node's alarm table, slot `i` for the i-th active
/// quantity it can watch. Config info with different active sensors moves that order, and the
/// Node's rules then sit in the wrong slots. When a field with a critical band would land in
/// another slot than the one it was last written to, the portal drafts every banded field's
/// `enabled` again (its current value), so the next "send configuration" rewrites the table.
/// Only a draft: the operator still sends it. A row the operator already drafted is left alone,
/// and a replayed document that changes nothing drafts nothing.
///
/// *Amended 2026-10-12 (MR !129 review):* slots go to banded fields only
/// ([`crate::radio::alarm::alarm_table`]) and a v2.0.0 rule names its own target, so a slot
/// index moving is no reason to resend: the table is redrafted when a banded field is missing
/// from the written one, and when the configured
/// `sensorN_type` of a slot holding a banded 1-Wire field changed since the table was written:
/// its rule names the old type and is inert on the Node. A layout recorded before the types
/// were (no marker, [`crate::radio::alarm::parse_stored_layout`]) is compared by slot alone.
fn redraft_alarm_table_on_layout_change(
    storage: &Storage,
    hostname: &str,
    node_key: &str,
    doc: &crate::state::NodeConfigDumpMsg,
) -> Result<(), portal_storage::StorageError> {
    let Some(declared) = declared_node_fields(doc) else {
        return Ok(());
    };
    // A send walk in flight for this Node writes a new table; comparing against the layout
    // written before it would see changes the walk is delivering (HIL 5722, 2026-10-15).
    let drafts = storage.list_pending(hostname, node_key)?;
    if drafts
        .iter()
        .any(|r| r.state == portal_storage::PENDING_STATE_SENDING)
    {
        return Ok(());
    }
    let candidates = crate::radio::alarm::slot_layout(declared.iter().map(String::as_str));
    let (written, written_types) = match storage.node_alarm_layout(hostname, node_key)? {
        Some(written) => crate::radio::alarm::parse_stored_layout(written),
        None => (
            PRE_TABLE_ALARM_LAYOUT
                .iter()
                .map(|f| f.to_string())
                .collect(),
            None,
        ),
    };
    let slot_of = |l: &[String], field: &str| l.iter().position(|f| f == field);

    // A switched-off band the Node was never sent has no rule on the Node to move: a default
    // band (threshold_defaults.md §3) above all, which must draft nothing. It still goes out
    // with the rest of the table on the next send that carries one.
    let delivered: std::collections::HashSet<String> = storage
        .list_node_delivery(hostname, node_key)?
        .into_iter()
        .map(|d| d.field)
        .collect();
    let thresholds = storage.list_field_thresholds(hostname, THRESHOLD_SCOPE_NODE, node_key)?;
    let has_band = |t: &portal_storage::FieldThreshold| matches!((t.critical_low, t.critical_high), (Some(lo), Some(hi)) if (hi as f32) > (lo as f32));
    // The slots the next table gives: every banded candidate, in candidate order.
    let layout: Vec<String> = candidates
        .iter()
        .filter(|f| thresholds.iter().any(|t| &t.field == *f && has_band(t)))
        .cloned()
        .collect();
    let banded: Vec<_> = thresholds
        .into_iter()
        .filter(|t| t.critical_low.is_some() && t.critical_high.is_some())
        .filter(|t| slot_of(&candidates, &t.field).is_some())
        .filter(|t| t.enabled || delivered.contains(&t.field))
        .collect();
    // v2.0.0 rules name their own target (slot, channel, type), so the index a rule sits at
    // does not change what the Node evaluates: only a banded field the written table lacks
    // does. The written table carries enabled bands only, so a switched-off band absent from
    // it is where it belongs, not a move (HIL 5722, 2026-10-15).
    let moved = banded.iter().any(|t| {
        t.enabled && slot_of(&layout, &t.field).is_some() && slot_of(&written, &t.field).is_none()
    });
    // A banded 1-Wire field whose slot's configured type is not the one its rule was written
    // for. Only for a field the written table carried: one it did not is a move already.
    let types_now = crate::radio::alarm::SlotTypes::of_node(Some(&crate::state::NodeConfigEntry {
        config: doc.config.clone(),
        ..crate::state::NodeConfigEntry::default()
    }));
    let retyped = written_types.is_some_and(|was| {
        banded
            .iter()
            .any(|t| w1_slot_of(&t.field).is_some_and(|slot| was.of(slot) != types_now.of(slot)))
    });
    if !moved && !retyped {
        return Ok(());
    }

    let drafted: std::collections::HashSet<String> =
        drafts.into_iter().map(|r| r.param_key).collect();
    for t in banded {
        let param_key = crate::pending::threshold_enabled_param_key(&t.field);
        if drafted.contains(&param_key) {
            continue;
        }
        storage.upsert_pending(&portal_storage::NewPendingConfig {
            hostname: hostname.to_string(),
            node_key: node_key.to_string(),
            param_key,
            kind: portal_storage::PENDING_KIND_THRESHOLD.to_string(),
            desired_value: Some(t.enabled.to_string()),
            author: crate::pending::ALARM_LAYOUT_AUTHOR.to_string(),
        })?;
    }
    tracing::info!(
        hostname,
        node_key,
        moved,
        retyped,
        "active sensors or slot types changed the alarm table; it is drafted for sending"
    );
    Ok(())
}

/// The 1-Wire slot (1..4) a `w1_{n}_{channel}` field belongs to.
fn w1_slot_of(field: &str) -> Option<u8> {
    let (slot, _) = field.strip_prefix("w1_")?.split_once('_')?;
    slot.parse().ok()
}

/// The fields a Node's config info declares (`render::declared_fields`), or `None` when it
/// declares no sensors.
fn declared_node_fields(doc: &crate::state::NodeConfigDumpMsg) -> Option<Vec<String>> {
    let entry = crate::state::NodeConfigEntry {
        config: doc.config.clone(),
        w1_slot_type: doc.w1_slot_type.clone(),
        ..crate::state::NodeConfigEntry::default()
    };
    crate::render::declared_fields(&entry, None)
        .map(|fields| fields.into_iter().map(|f| f.name).collect())
}

/// Offers one sensor's `fields` their default bands (`threshold_defaults::seed`), each field
/// once per process: the write inserts only what has no row, and a field it has seen is not
/// looked up again. A failure is logged and not remembered, so the next document retries it;
/// it never fails the event that carried the fields.
fn offer_threshold_defaults<'a>(
    storage: &Storage,
    cache: &mut PersistCache,
    hostname: &str,
    scope: &'static str,
    entity: &str,
    fields: impl IntoIterator<Item = &'a str>,
) {
    let host = hostname.to_ascii_lowercase();
    let fresh: Vec<&str> = fields
        .into_iter()
        .filter(|f| {
            !cache.defaults_offered.contains(&(
                host.clone(),
                scope,
                entity.to_string(),
                f.to_string(),
            ))
        })
        .collect();
    if fresh.is_empty() {
        return;
    }
    let defaults = crate::threshold_defaults::configured();
    match crate::threshold_defaults::seed(
        storage,
        &defaults,
        hostname,
        scope,
        entity,
        fresh.iter().copied(),
    ) {
        Ok(_) => {
            for field in fresh {
                cache.defaults_offered.insert((
                    host.clone(),
                    scope,
                    entity.to_string(),
                    field.to_string(),
                ));
            }
        }
        Err(err) => {
            tracing::warn!(%err, hostname, scope, entity, "default thresholds not written");
        }
    }
}

/// Forgets the offered fields of this Hub's sensors in `scope` whose entity `gone` names.
fn forget_offered_defaults(
    cache: &mut PersistCache,
    hostname: &str,
    scope: &'static str,
    gone: impl Fn(&str) -> bool,
) {
    let host = hostname.to_ascii_lowercase();
    cache
        .defaults_offered
        .retain(|(h, s, entity, _)| !(h == &host && *s == scope && gone(entity)));
}

/// `eye/sensors`: a Beacon the Hub has registered gets its default bands; one it lists as a
/// discovered candidate is not registered (any more), and is forgotten.
fn offer_beacon_defaults(
    storage: &Storage,
    cache: &mut PersistCache,
    hostname: &str,
    tags: &[EyeTag],
) {
    let hostname = cache.canonical_hostname(hostname);
    for tag in tags {
        let mac = tag.mac.to_uppercase();
        if tag.discovered == Some(true) {
            forget_offered_defaults(cache, &hostname, THRESHOLD_SCOPE_EYE, |e| e == mac);
            continue;
        }
        offer_threshold_defaults(
            storage,
            cache,
            &hostname,
            THRESHOLD_SCOPE_EYE,
            &mac,
            crate::threshold_defaults::beacon_fields(),
        );
    }
}

/// Notes a native `activate` for [`reraised_since`] (see `PersistCache::native_activations`).
fn record_native_activation(
    cache: &mut PersistCache,
    hostname: &str,
    node_key: &str,
    event: &control_radio::decode::reading::NodeEvent,
    event_ts: i64,
) {
    let obj = &event.extra;
    if v2_alarm_side(&event.event_type, obj).is_none()
        || obj.get("event").and_then(serde_json::Value::as_str) != Some("activate")
    {
        return;
    }
    let Some((slot, source, _)) = native_identity(obj) else {
        return;
    };
    let at = cache
        .native_activations
        .entry((hostname.to_lowercase(), node_key.to_string(), slot, source))
        .or_insert(event_ts);
    *at = (*at).max(event_ts);
}

/// Whether the Node sent an `activate` for `slot` (and, for the no-data watchdog, `source`) at
/// or after `since`, read from the `node_events` rows `persist_node_reading` writes for every
/// alarm document.
fn reraised_since(
    storage: &Storage,
    cache: &PersistCache,
    hostname: &str,
    node_key: &str,
    slot: i64,
    source: Option<&str>,
    since: i64,
) -> Result<bool, portal_storage::StorageError> {
    let seen = |source: Option<String>| {
        cache
            .native_activations
            .get(&(hostname.to_lowercase(), node_key.to_string(), slot, source))
            .is_some_and(|at| *at >= since)
    };
    // The watchdog's rows name their source; a slot rule's rows do not, and its events may.
    let cached = match source {
        Some(s) => seen(Some(s.to_string())),
        None => cache.native_activations.iter().any(|((h, n, sl, _), at)| {
            *h == hostname.to_lowercase() && n == node_key && *sl == slot && *at >= since
        }),
    };
    if cached {
        return Ok(true);
    }
    let events = storage.query_events(hostname, node_key, None, since, i64::MAX, None)?;
    Ok(events.iter().any(|e| {
        let payload: Option<serde_json::Value> = e
            .payload
            .as_deref()
            .and_then(|p| serde_json::from_str(p).ok());
        let Some(obj) = payload.as_ref().and_then(|p| p.as_object()) else {
            return false;
        };
        let identity = native_identity(obj);
        v2_alarm_side(&e.event_type, obj).is_some()
            && identity.as_ref().is_some_and(|(sl, src, _)| {
                *sl == slot && source.is_none_or(|s| src.as_deref() == Some(s))
            })
            && obj.get("event").and_then(serde_json::Value::as_str) == Some("activate")
    }))
}

/// One `alarm` event from a v2 `NodeReading` → [`NativeEvent`].
///
/// **A different vocabulary from v1's, which is why this is not
/// `decode_native_event`.** The v2 decoder is a port of the firmware's own
/// `ttn.js`, so it emits the firmware's spellings: the edge key is `event`
/// with values `activate`/`deactivate` (v1 published `edge: raise|clear`),
/// and the threshold side is `type` with `low`/`high`/`trigger`/`no_data`
/// (v1 published `side`/`alarm_type`). Mapping them here rather than
/// normalizing on the way in keeps the published document a faithful decode
/// of the wire.
///
/// The engine's idempotency key is `node_key:slot:base_time+rel_s`; the v2
/// event carries a single resolved `time` instead of the pair, so it goes in
/// as `base_time` with `rel_s = 0` — same key material, same stability. A
/// batch whose clock had not synced reports `time: null`; those events fall
/// back to `0`, and the `message_id` gate above is what keeps a redelivered
/// batch from re-raising them.
fn decode_v2_native_event(
    hostname: &str,
    node_key: &str,
    event: &control_radio::decode::reading::NodeEvent,
) -> Option<NativeEvent> {
    let extra = &event.extra;
    let (slot, source, quantity) = native_identity(extra)?;
    let edge = match extra.get("event")?.as_str()? {
        "activate" => Edge::Raise,
        "deactivate" => Edge::Clear,
        other => {
            tracing::debug!(
                hostname,
                node_key,
                edge = other,
                "native alarm event: unrecognized edge — skipped"
            );
            return None;
        }
    };
    let side = v2_alarm_side(&event.event_type, extra).flatten();
    let value = extra.get("value").and_then(|v| v.as_f64());
    let base_time = extra.get("time").and_then(|v| v.as_i64()).unwrap_or(0);

    Some(NativeEvent {
        hostname: hostname.to_string(),
        node_key: node_key.to_string(),
        slot,
        source,
        quantity,
        edge,
        side,
        value,
        base_time,
        rel_s: 0,
        ts_iso: event.ts.clone(),
    })
}

/// Whether a v2 event is a native alarm, and the threshold side it names (`low`/`high`/
/// `trigger`/`no_data`) when it names one. `None` is "not an alarm".
///
/// control-radio publishes an alarm as `type: "alarm"` with the side under `side`. Before
/// the Nodes test 2026-09-23 the decoder's own `type` (the side) overwrote the envelope's on
/// the wire, so a document from a Hub built before that reads `type: "high"` with the
/// `event` edge beside it. That shape is an alarm too, and its `type` is the side.
pub(crate) fn v2_alarm_side(
    event_type: &str,
    extra: &serde_json::Map<String, serde_json::Value>,
) -> Option<Option<String>> {
    const SIDES: &[&str] = &["low", "high", "trigger", "no_data"];
    if event_type == "alarm" {
        return Some(
            extra
                .get("side")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        );
    }
    (SIDES.contains(&event_type) && extra.contains_key("event"))
        .then(|| Some(event_type.to_string()))
}

// ---- StickerHistory ------------------------------------------------------

/// `records` → `node_field_readings` rows, skipping records with no `time`
/// (nothing to key them by) and fields with no value.
fn history_field_rows(
    hostname: &str,
    node_key: &str,
    records: &[NodeHistoryRecord],
) -> Vec<NodeFieldReading> {
    records
        .iter()
        .filter_map(|record| Some((record.time?, record)))
        .flat_map(|(ts, record)| {
            record.fields.iter().filter_map(move |(field, value)| {
                let value = (*value)?;
                Some(NodeFieldReading {
                    hostname: hostname.to_string(),
                    node_key: node_key.to_string(),
                    field: field.clone(),
                    ts,
                    value: Some(value),
                    alarm_state: None,
                    // A history frame is a replay by definition, on either topic family.
                    source: SOURCE_REPLAY.to_string(),
                })
            })
        })
        .collect()
}

/// `records` → `node_counter_readings` rows, same `time`/value rules as
/// [`history_field_rows`].
fn history_counter_rows(
    hostname: &str,
    node_key: &str,
    records: &[NodeHistoryRecord],
) -> Vec<NodeCounterReading> {
    records
        .iter()
        .filter_map(|record| Some((record.time?, record)))
        .flat_map(|(ts, record)| {
            record.counters.iter().filter_map(move |(counter, value)| {
                let value = (*value)?;
                Some(NodeCounterReading {
                    hostname: hostname.to_string(),
                    node_key: node_key.to_string(),
                    counter: counter.clone(),
                    ts,
                    value: Some(value),
                })
            })
        })
        .collect()
}

/// The spacing of a frame's records: the one the frame states, else the smallest step between
/// its records, else a minute.
fn record_interval_s(payload: &NodeHistoryMsg) -> i64 {
    if let Some(i) = payload.interval_s.filter(|i| *i > 0) {
        return i;
    }
    let mut times: Vec<i64> = payload.records.iter().filter_map(|r| r.time).collect();
    times.sort_unstable();
    times
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|d| *d > 0)
        .min()
        .unwrap_or(60)
}

/// Whether a stored reading of `name` lies within `tolerance` seconds of `ts`.
fn covered(stored: &[(String, i64)], name: &str, ts: i64, tolerance: i64) -> bool {
    stored
        .iter()
        .any(|(n, t)| n == name && (t - ts).abs() <= tolerance)
}

/// A history frame completes its job when it's the last one (`frame_index`
/// is the highest index) or the device reports `frame_count == 0` (nothing
/// to replay at all).
fn frame_completes_job(frame_index: Option<i64>, frame_count: Option<i64>) -> bool {
    let frame_index = frame_index.unwrap_or(0);
    let frame_count = frame_count.unwrap_or(0);
    frame_count == 0 || frame_index + 1 >= frame_count
}

fn persist_sticker_history(
    storage: &Storage,
    hostname: &str,
    node_key: &str,
    payload: NodeHistoryMsg,
) -> Result<(), portal_storage::StorageError> {
    let mut field_rows = history_field_rows(hostname, node_key, &payload.records);
    let mut counter_rows = history_counter_rows(hostname, node_key, &payload.records);

    // A span request returns what the Node holds for the whole span, the minutes the live
    // readings cover too; a record a stored reading of the same field already covers is left
    // out, or the chart would draw it twice (devices.node.md §3.9, amendment 2026-09-25,
    // item 5).
    let tolerance = record_interval_s(&payload) / 2;
    let times: Vec<i64> = payload.records.iter().filter_map(|r| r.time).collect();
    if let (Some(&first), Some(&last)) = (times.iter().min(), times.iter().max()) {
        let (from, to) = (first - tolerance, last + tolerance + 1);
        let fields = storage.field_reading_times(hostname, node_key, from, to)?;
        field_rows.retain(|r| !covered(&fields, &r.field, r.ts, tolerance));
        let counters = storage.counter_reading_times(hostname, node_key, from, to)?;
        counter_rows.retain(|r| !covered(&counters, &r.counter, r.ts, tolerance));
    }

    let rows_inserted = storage.insert_field_readings(&field_rows)?
        + storage.insert_counter_readings(&counter_rows)?;
    let records_seen = payload.records.len() as i64;

    // Row persistence + frame crediting + marking done are this sink's job
    // because the DB connection lives here; job creation/requesting
    // history/retries stay the backfill service's job (module docs).
    for job in storage.active_jobs(hostname)? {
        let is_target_job = job.node_key == node_key
            && matches!(job.status.as_str(), JOB_REQUESTED | JOB_RECEIVING);
        if !is_target_job {
            continue;
        }

        // The distinction `credit_frames` keeps both numbers for: a frame
        // that carried records and inserted none is rumqttc's replay
        // (design.md §6.2), not an empty frame — worth a line, not a credit.
        if records_seen > 0 && rows_inserted == 0 {
            tracing::debug!(
                job_id = job.job_id,
                records_seen,
                frame_index = payload.frame_index,
                "backfill frame carried records but inserted none — a replayed frame, credited nothing"
            );
        }
        let credited = storage.credit_frames(
            job.job_id,
            records_seen,
            rows_inserted as i64,
            chrono_free_now_unix(),
        )?;
        if credited.is_none() {
            continue;
        }

        if frame_completes_job(payload.frame_index, payload.frame_count) {
            storage.update_job_status(job.job_id, JOB_DONE, None, false)?;
        }
    }
    Ok(())
}

// ---- EyeAggregated -------------------------------------------------------

/// The Portal's `event_type` for a Hub window row — the Hub's own name for
/// it (`control_ble::host::EVENT_ADVERTISING`).
const EYE_EVENT_ADVERTISING: &str = "advertising";

/// One `eye_readings` row per Beacon per Hub storage window
/// (`plan/control/storage_intervals.md` §6), exactly as `sensors/aggregated`
/// becomes one `readings_raw` row per line.
///
/// The value columns hold the window **average**: it is what every chart,
/// summary and export reads, so they need no change. The window's min/max
/// go into the V31 range columns beside it.
///
/// Deduped through `idx_eye_readings_message_id` on the Hub's `message_id`
/// (`{MAC}-adv-{window_start}`) **under the Hub's hostname**, so a QoS 1
/// redelivery or a spill replay is a no-op. The hostname is part of the key
/// because two Hubs that hear one Beacon mint the same id for the same
/// window, and the index carries no hostname: without it the second Hub's
/// row would be dropped and its chart would show holes. A tag without an id
/// (an older producer) falls back to the same spelling built from its
/// `window_start`.
fn persist_eye_aggregated(
    storage: &Storage,
    cache: &PersistCache,
    hostname: &str,
    payload: EyeAggregatedMsg,
) -> Result<(), portal_storage::StorageError> {
    for tag in &payload.tags {
        let mac = tag.mac.to_uppercase();
        // Decision #42: a registered Beacon's window is stored once, from its home Hub, under
        // the registry; an unregistered one is not stored at all.
        let (key, source_hub) = match cache.beacon_source(hostname, &mac) {
            BeaconSource::Legacy => (hostname.to_string(), None),
            BeaconSource::Home { registry } => (registry, Some(hostname)),
            BeaconSource::NotHome | BeaconSource::Stranger { .. } => continue,
        };
        let hostname = key.as_str();
        let Some(ts) = tag.window_start.or(payload.period_start_ts) else {
            continue; // no window to place the row in
        };
        let temperature_c = tag.avg("temperature");
        let humidity_pct = tag.avg("humidity").map(|h| h.round() as i64);
        if temperature_c.is_none() && humidity_pct.is_none() {
            continue; // nothing the history can plot
        }
        let hub_id = tag
            .message_id
            .clone()
            .unwrap_or_else(|| format!("{mac}-adv-{ts}"));
        let range = EyeWindowRange {
            temperature_min_c: tag.min("temperature"),
            temperature_max_c: tag.max("temperature"),
            humidity_min_pct: tag.min("humidity").map(|h| h.round() as i64),
            humidity_max_pct: tag.max("humidity").map(|h| h.round() as i64),
        };
        let row = EyeReading {
            hostname: hostname.to_string(),
            mac,
            ts,
            temperature_c,
            humidity_pct,
            event_type: EYE_EVENT_ADVERTISING.to_string(),
            message_id: Some(format!("{hostname}/{hub_id}")),
        };
        match source_hub {
            Some(hub) => storage.insert_eye_window_from(&row, &range, hub)?,
            None => storage.insert_eye_window(&row, &range)?,
        };
    }
    Ok(())
}

/// One `eye/history` message (plan/portal/beacon_history.md §2.2-2.3).
///
/// `records` go into `eye_readings` as `event_type = 'recording'` under the
/// Hub's own key `{MAC}-rec-{ts}` (`INSERT OR IGNORE`, so a replayed batch
/// is a no-op) and count as Hub activity on the job. A state message moves
/// the job; the first time it reads `done`, the cross-check of §7 Q6 layer 3
/// runs and the layer-4 audit row is written. `compared` is the marker: it
/// is set last, so a retried write finishes the check instead of skipping
/// it.
fn persist_beacon_history(
    storage: &Storage,
    cache: &mut PersistCache,
    hostname: &str,
    msg: crate::beacon_history::BeaconHistoryMsg,
) -> Result<(), portal_storage::StorageError> {
    use crate::beacon_history as bh;

    let mac = msg.mac.trim().to_uppercase();
    if msg.state == bh::STATE_RECORDS {
        // Rows actually added, not rows sent: a replayed batch credits
        // nothing, so the count stays right under `write_or_spill` replays.
        let mut stored = 0;
        for (ts, temperature_c) in &msg.records {
            if *ts <= 0 {
                continue;
            }
            stored += i64::from(storage.insert_eye_reading(&EyeReading {
                hostname: hostname.to_string(),
                mac: mac.clone(),
                ts: *ts,
                temperature_c: *temperature_c,
                humidity_pct: None,
                event_type: "recording".to_string(),
                message_id: Some(format!("{mac}-rec-{ts}")),
            })?);
        }
        let (lo, hi) = msg
            .records
            .iter()
            .map(|(ts, _)| *ts)
            .filter(|ts| *ts > 0)
            .fold((i64::MAX, i64::MIN), |(lo, hi), ts| {
                (lo.min(ts), hi.max(ts))
            });
        if lo <= hi {
            let span = cache
                .beacon_history_spans
                .entry(msg.job.clone())
                .or_insert((lo, hi, None));
            span.0 = span.0.min(lo);
            span.1 = span.1.max(hi);
            span.2 = msg.interval_s.filter(|i| *i > 0).or(span.2);
        }
        storage.credit_beacon_history_records(&msg.job, stored)?;
        nudge_beacon_history(cache, hostname, &mac);
        return Ok(());
    }

    if !matches!(
        msg.state.as_str(),
        JOB_REQUESTED | JOB_RECEIVING | JOB_DONE | portal_storage::JOB_FAILED
    ) {
        tracing::debug!(state = %msg.state, job = %msg.job, "eye/history: unknown state ignored");
        return Ok(());
    }
    let report = portal_storage::BeaconHistoryReport {
        id: msg.job.clone(),
        hostname: hostname.to_string(),
        mac: mac.clone(),
        state: msg.state.clone(),
        received: msg.received,
        expected: msg.expected,
        from_ts: msg.from,
        to_ts: msg.to,
        retry: msg.retry,
        reason: msg.reason.clone(),
        sessions: msg.sessions,
        rejected_chunks: msg.rejected_chunks,
        future_dropped: msg.future_dropped,
        crc_failed: msg.crc_failed,
    };
    storage.apply_beacon_history_report(&report)?;
    // Read back rather than trust the update's result: a replayed `done`
    // finds the row already final, and still owes the check if `compared`
    // never landed.
    let job = storage.get_beacon_history_job(&msg.job)?;
    if let Some(job) = job.filter(|j| j.state == JOB_DONE && j.compared.is_none()) {
        let span = cache.beacon_history_spans.remove(&job.id);
        let (from, to) = match span {
            Some((lo, hi, _)) => (lo, hi),
            None => (
                job.from_ts.unwrap_or(0),
                job.to_ts.unwrap_or_else(chrono_free_now_unix),
            ),
        };
        let points = storage.eye_points_for_mac(&mac, from - 3600, to + 3600)?;
        let interval = span
            .and_then(|(_, _, i)| i)
            .or_else(|| bh::recording_interval(&points))
            .unwrap_or(bh::DEFAULT_RECORDING_INTERVAL_S);
        let in_window: Vec<_> = points
            .into_iter()
            .filter(|(ts, _, kind)| kind != "recording" || (from..=to).contains(ts))
            .collect();
        let (compared, disagreed) = bh::cross_check(&in_window, interval / 2);
        storage.insert_audit(&portal_storage::NewAuditEntry {
            event_type: "beacon_history_download".to_string(),
            username: Some(bh::SYSTEM_ACTOR.to_string()),
            user_id: None,
            ip: None,
            target: Some(mac.clone()),
            details: serde_json::to_string(&serde_json::json!({
                "job": job.id,
                "hostname": hostname,
                "from": job.from_ts,
                "to": job.to_ts,
                "received": job.received,
                "expected": job.expected,
                "sessions": job.sessions,
                "rejected_chunks": job.rejected_chunks,
                "future_dropped": job.future_dropped,
                "crc_failed": job.crc_failed,
                "compared": compared,
                "disagreed": disagreed,
            }))
            .ok(),
            success: true,
        })?;
        storage.set_beacon_history_crosscheck(&job.id, compared, disagreed)?;
        if disagreed > 0 {
            tracing::warn!(job = %job.id, mac, compared, disagreed, "beacon archive disagrees with live readings");
        }
    }
    nudge_beacon_history(cache, hostname, &mac);
    Ok(())
}

fn nudge_beacon_history(cache: &PersistCache, hostname: &str, mac: &str) {
    if let Some(tx) = &cache.beacon_history_nudge {
        // No subscriber (no tab open) is normal.
        let _ = tx.send(crate::beacon_history::BeaconHistoryNudge {
            hostname: hostname.to_string(),
            mac: mac.to_string(),
        });
    }
}

/// Unix seconds without pulling chrono in (consistent with state/models.rs).
fn chrono_free_now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    /// northbridge_update.md §3.5: only a spent retry budget (`failed`) is an alarm; a Hub
    /// without bundled firmware (`update: null`), an older Hub (no key) and a withdrawn
    /// document all clear.
    /// node-firmware-v2.0.0.md §4: a rule keys on its index, a watchdog on slot and channel,
    /// a mismatch on its type; the name is the channel.
    #[test]
    fn native_identity_follows_the_v150_alarm_target() {
        let id = |v: serde_json::Value| native_identity(v.as_object().expect("object"));
        assert_eq!(
            id(serde_json::json!({"slot": "mb", "channel": "temperature", "rule": 2})),
            Some((2, None, Some("temperature".into())))
        );
        assert_eq!(
            id(serde_json::json!({
                "slot": "s1", "channel": "temperature-aux", "sensor_type": "machine-probe",
                "rule": null, "type": "no_data"
            })),
            Some((
                255,
                Some("s1:temperature-aux".into()),
                Some("s1 temperature-aux".into())
            ))
        );
        assert_eq!(
            id(
                serde_json::json!({"slot": "s1", "channel": null, "rule": null, "type": "sensor_mismatch"})
            ),
            Some((
                255,
                Some("s1:sensor_mismatch".into()),
                Some("s1 sensor_mismatch".into())
            ))
        );
        assert_eq!(
            id(serde_json::json!({"slot": 0, "channel": 20, "rule": null, "side": "low"})),
            Some((
                255,
                Some("mb:battery-voltage".into()),
                Some("battery-voltage".into())
            )),
            "the legacy stack's numbers, named through the registry"
        );
        assert_eq!(
            id(serde_json::json!({"slot": 3, "source": "onboard", "quantity": "temperature"})),
            None,
            "the v1.4.0 shape names nothing"
        );
    }

    #[test]
    fn only_a_failed_northbridge_update_opens_its_alarm() {
        let doc =
            |update: serde_json::Value| serde_json::json!({ "state": "alive", "update": update });
        let failed = doc(serde_json::json!({ "state": "failed", "attempts": 3 }));
        assert!(!northbridge_update_ok(Some(&failed)));
        for state in ["idle", "flashing"] {
            let d = doc(serde_json::json!({ "state": state }));
            assert!(northbridge_update_ok(Some(&d)), "{state}");
        }
        assert!(northbridge_update_ok(Some(&doc(serde_json::Value::Null))));
        assert!(northbridge_update_ok(Some(
            &serde_json::json!({ "state": "alive" })
        )));
        assert!(northbridge_update_ok(None));
    }

    /// southbridge_update.md §3.7: only `failed` is an alarm; `idle`, `running`, no update
    /// object and no block at all (an older agent) clear.
    #[test]
    fn only_a_failed_southbridge_update_opens_its_alarm() {
        use fiber_protocol::payloads::system_info::{Southbridge, SouthbridgeUpdate};
        let sb = |state: Option<&str>| Southbridge {
            update: Some(SouthbridgeUpdate {
                state: state.map(str::to_string),
                ..SouthbridgeUpdate::default()
            }),
            ..Southbridge::default()
        };
        assert!(!southbridge_update_ok(Some(&sb(Some("failed")))));
        for state in [Some("idle"), Some("running"), None] {
            assert!(southbridge_update_ok(Some(&sb(state))), "{state:?}");
        }
        assert!(southbridge_update_ok(Some(&Southbridge::default())));
        assert!(southbridge_update_ok(None));
    }

    #[test]
    fn a_radio_component_down_opens_only_after_five_minutes_and_up_clears() {
        let mut cache = PersistCache::default();
        let t0 = 1_790_400_000;
        assert_eq!(
            radio_verdict(&mut cache, "HUB", "northbridge", Some(true), t0),
            None
        );
        assert_eq!(
            radio_verdict(&mut cache, "HUB", "northbridge", Some(true), t0 + 299),
            None
        );
        assert_eq!(
            radio_verdict(&mut cache, "HUB", "northbridge", Some(true), t0 + 300),
            Some(false),
            "five minutes down opens"
        );
        assert_eq!(
            radio_verdict(&mut cache, "HUB", "northbridge", Some(false), t0 + 310),
            Some(true),
            "up clears"
        );
        // The debounce restarted: down again waits another five minutes.
        assert_eq!(
            radio_verdict(&mut cache, "HUB", "northbridge", Some(true), t0 + 320),
            None
        );
        assert_eq!(
            radio_verdict(&mut cache, "HUB", "chirpstack", None, t0),
            None
        );
    }

    use std::collections::HashMap;

    use fiber_protocol::payloads::alarm_event::AlarmEventMsg;
    use fiber_protocol::payloads::config_state::{
        ConfigStateMsg, SensorConfigEntry, SensorThresholds,
    };
    use fiber_protocol::payloads::lorawan::{
        LorawanEvent, LorawanSensor, LorawanSensorsMsg, NodeHistoryMsg, NodeHistoryRecord,
    };
    use portal_storage::{NewBackfillJob, Storage, JOB_DONE, JOB_REQUESTED};
    use tokio::sync::{broadcast, mpsc};

    use super::*;
    use fiber_protocol::payloads::eye::EyeSensorsMsg;

    /// "Portal storage low" is the Hub the portal runs on: a Client's snapshot does not carry
    /// the Server's partition (a full Server disk put the fault triangle on every Hub).
    #[test]
    fn only_the_portals_own_hub_carries_its_partition() {
        let cache = PersistCache {
            local_hostname: Some("proximos-4e970f4a".into()),
            ..PersistCache::default()
        };
        assert!(cache.runs_portal("proximos-4e970f4a"));
        assert!(
            cache.runs_portal("PROXIMOS-4E970F4A"),
            "the hub may uppercase its name"
        );
        assert!(!cache.runs_portal("proximos-2200ad33"));
        assert!(
            PersistCache::default().runs_portal("anything"),
            "unknown: every Hub, as before"
        );
    }

    // ---- pure helpers ----------------------------------------------------

    #[test]
    fn last_seen_advanced_gates_on_change() {
        let mut cache = HashMap::new();
        assert!(
            last_seen_advanced(&cache, "hub1", "aa11", "t1"),
            "None -> Some counts as advanced"
        );
        remember_last_seen(&mut cache, "hub1", "aa11", "t1");
        assert!(
            !last_seen_advanced(&cache, "hub1", "aa11", "t1"),
            "unchanged value must be gated out"
        );
        assert!(
            last_seen_advanced(&cache, "hub1", "aa11", "t2"),
            "changed value advances again"
        );
        // A different Node on the same host has its own cache slot.
        assert!(last_seen_advanced(&cache, "hub1", "bb22", "t1"));
    }

    #[test]
    fn the_dedup_gate_does_not_close_until_the_write_commits() {
        // The regression this guards: the gate used to mark the cache as it
        // checked it, so a write that failed (a VACUUM outlasting
        // `busy_timeout`) left the reading permanently skipped — the retry and
        // the post-VACUUM replay both saw "already persisted".
        let mut cache = HashMap::new();
        assert!(last_seen_advanced(&cache, "hub1", "aa11", "t1"));
        // ... the write fails here, so nothing is remembered ...
        assert!(
            last_seen_advanced(&cache, "hub1", "aa11", "t1"),
            "a failed write must leave the reading replayable"
        );

        remember_last_seen(&mut cache, "hub1", "aa11", "t1");
        assert!(!last_seen_advanced(&cache, "hub1", "aa11", "t1"));
    }

    #[test]
    fn the_message_id_gate_does_not_close_until_the_write_commits() {
        let mut cache = PersistCache::default();
        assert!(node_reading_is_new(&cache, "hub1", "aa11", "msg-1"));
        assert!(
            node_reading_is_new(&cache, "hub1", "aa11", "msg-1"),
            "a failed write must leave the document replayable"
        );

        remember_node_message_id(&mut cache, "hub1", "aa11", "msg-1");
        assert!(!node_reading_is_new(&cache, "hub1", "aa11", "msg-1"));
        assert!(
            node_reading_is_new(&cache, "hub1", "aa11", "msg-2"),
            "a different document is still new"
        );
        assert!(
            node_reading_is_new(&cache, "hub1", "bb22", "msg-1"),
            "a different node has its own window"
        );
    }

    /// A liveness edge refused by a locked database is journalled, and the
    /// replay both applies it and nudges — the SSE re-render is delayed, not
    /// dropped. Without this the edge would be gone: the offline detector
    /// broadcasts one message per *change* of verdict and never re-announces.
    #[test]
    fn a_spilled_liveness_edge_is_replayed_and_nudges() {
        let (dir, storage) = temp_storage();
        let gate = crate::durable::test_gate(dir.path().to_str().expect("utf8"));
        let (ntx, _nrx, gtx, mut grx) = channels();
        let mut cache = PersistCache::default();

        let edge = crate::durable::LivenessEdge {
            hostname: "hub-1".to_string(),
            sensor_id: "node:aa11".to_string(),
            opening: true,
            message: "offline".to_string(),
            at: "2026-09-11T02:30:00Z".to_string(),
            display_name: "Lednice 1".to_string(),
            display_location: Some("Sklad".to_string()),
        };
        gate.append(
            SpillKind::Liveness,
            1_700_000_000,
            &serde_json::to_vec(&edge).expect("encode"),
        )
        .expect("append");

        assert!(
            storage
                .active_alarm("hub-1", "node:aa11")
                .expect("active")
                .is_none(),
            "nothing is in the database while the row sits in the journal"
        );

        drain_spill(&storage, &gate, "proximos", &mut cache, &ntx, &gtx, "test");

        let open = storage
            .active_alarm("hub-1", "node:aa11")
            .expect("active")
            .expect("replay must open the outage row");
        assert_eq!(open.alarm_type, "OFFLINE");
        assert_eq!(
            open.sensor_name.as_deref(),
            Some("Lednice 1"),
            "the name resolved at the edge, not at replay"
        );
        assert_eq!(gate.depth().expect("depth"), 0);
        assert!(
            grx.try_recv().is_ok(),
            "the SSE nudge the live path would have sent fires on replay instead"
        );
    }

    /// Applying the same edge twice is a no-op the second time — which is what
    /// makes replay safe when a crash lands between the write and the delete.
    #[test]
    fn replaying_a_liveness_edge_twice_opens_one_row() {
        let (dir, storage) = temp_storage();
        let _ = dir;
        let edge = crate::durable::LivenessEdge {
            hostname: "hub-1".to_string(),
            sensor_id: "node:aa11".to_string(),
            opening: true,
            message: "offline".to_string(),
            at: "2026-09-11T02:30:00Z".to_string(),
            display_name: "Lednice 1".to_string(),
            display_location: None,
        };

        assert!(crate::durable::apply_liveness(&storage, &edge).expect("first"));
        assert!(
            !crate::durable::apply_liveness(&storage, &edge).expect("second"),
            "a row is already open; the second application writes nothing"
        );

        let closing = crate::durable::LivenessEdge {
            opening: false,
            ..edge
        };
        assert!(crate::durable::apply_liveness(&storage, &closing).expect("clear"));
        assert!(
            !crate::durable::apply_liveness(&storage, &closing).expect("clear again"),
            "and there is nothing left to close"
        );
    }

    #[test]
    fn parse_rfc3339_roundtrips_and_rejects_garbage() {
        assert_eq!(parse_rfc3339("2026-01-13T14:30:00Z"), Some(1_768_314_600));
        assert_eq!(parse_rfc3339("not a timestamp"), None);
    }

    #[test]
    fn frame_completes_job_on_last_frame_or_zero_count() {
        assert!(frame_completes_job(Some(0), Some(1)), "only frame is last");
        assert!(!frame_completes_job(Some(0), Some(2)), "more frames coming");
        assert!(frame_completes_job(Some(1), Some(2)), "final index");
        assert!(
            frame_completes_job(Some(0), Some(0)),
            "frame_count == 0 means nothing to replay"
        );
        assert!(
            frame_completes_job(None, None),
            "missing fields default to the completing case"
        );
    }

    fn history_record(time: Option<i64>, field_value: Option<f64>) -> NodeHistoryRecord {
        NodeHistoryRecord {
            time,
            fields: HashMap::from([("temperature".to_string(), field_value)]),
            counters: HashMap::from([("fCnt".to_string(), Some(3i64))]),
        }
    }

    #[test]
    fn history_field_rows_skip_records_without_time_or_value() {
        let records = vec![
            history_record(None, Some(1.0)), // no time — dropped
            history_record(Some(100), None), // null field value — dropped
            history_record(Some(200), Some(2.0)),
        ];
        let rows = history_field_rows("hub1", "aa11", &records);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ts, 200);
        assert_eq!(rows[0].value, Some(2.0));
        assert_eq!(rows[0].source, SOURCE_REPLAY, "a history frame is a replay");
    }

    #[test]
    fn history_counter_rows_skip_records_without_time() {
        let records = vec![
            history_record(None, Some(1.0)),
            history_record(Some(200), Some(2.0)),
        ];
        let rows = history_counter_rows("hub1", "aa11", &records);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ts, 200);
        assert_eq!(rows[0].value, Some(3));
    }

    // ---- Beacon history: the Hub's storage window (storage_intervals.md §6)

    fn eye_tag_at(mac: &str, last_seen_ts: i64) -> fiber_protocol::payloads::eye::EyeTag {
        fiber_protocol::payloads::eye::EyeTag {
            gateway: None,
            mac: mac.to_string(),
            name: None,
            temperature_c: Some(5.0),
            humidity_pct: Some(45),
            battery_mv: Some(2900),
            low_battery: Some(false),
            magnet_present: None,
            magnet_detected: None,
            moving: None,
            movement_count: None,
            pitch_deg: None,
            roll_deg: None,
            rssi: Some(-70),
            last_seen_ts: Some(last_seen_ts),
            stale: Some(false),
            provisioning: None,
            discovered: None,
            field_alarm_states: HashMap::new(),
            alarm_state: None,
            ble_config: None,
        }
    }

    fn aggregated_tag(mac: &str, window_start: i64, avg: f64) -> serde_json::Value {
        serde_json::json!({
            "mac": mac,
            "name": null,
            "message_id": format!("{mac}-adv-{window_start}"),
            "window_start": window_start,
            "window_s": 600,
            "count": 60,
            "alarm_state": "NORMAL",
            "fields": {
                "temperature": {"min": avg - 1.0, "avg": avg, "max": avg + 1.0},
                "humidity": {"min": 40.0, "avg": 41.6, "max": 43.0},
                "rssi": {"min": -80.0, "avg": -70.0, "max": -60.0}
            }
        })
    }

    fn aggregated(tags: Vec<serde_json::Value>) -> EyeAggregatedMsg {
        serde_json::from_value(serde_json::json!({
            "period_start_ts": 1_791_799_200,
            "period_end_ts": 1_791_799_800,
            "duration_sec": 600,
            "tags": tags,
        }))
        .expect("decode")
    }

    const MAC: &str = "AA:BB:CC:DD:EE:FF";

    /// storage_intervals.md §6: one row per Beacon per Hub window, the
    /// average in the value columns, the Hub's `message_id` as the key.
    #[test]
    fn a_hub_window_is_one_row_per_beacon_with_the_average() {
        let (_dir, storage) = temp_storage();
        let t0 = 1_791_799_200;
        persist_eye_aggregated(
            &storage,
            &PersistCache::default(),
            "h1",
            aggregated(vec![
                aggregated_tag("aa:bb:cc:dd:ee:ff", t0, 4.25),
                aggregated_tag("AA:BB:CC:DD:EE:01", t0, -18.0),
            ]),
        )
        .expect("persist");
        let rows = storage
            .query_eye_readings("h1", MAC, 0, i64::MAX, None)
            .expect("query");
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.mac, MAC, "normalized uppercase");
        assert_eq!(r.ts, t0, "placed at the window start");
        assert_eq!(r.temperature_c, Some(4.25));
        assert_eq!(
            r.humidity_pct,
            Some(42),
            "the integer column gets the rounded average"
        );
        assert_eq!(r.event_type, "advertising");
        assert_eq!(
            r.message_id.as_deref(),
            Some("h1/aa:bb:cc:dd:ee:ff-adv-1791799200"),
            "the Hub's id under its hostname"
        );
        assert_eq!(
            storage
                .eye_window_range("h1/aa:bb:cc:dd:ee:ff-adv-1791799200")
                .expect("range"),
            Some(EyeWindowRange {
                temperature_min_c: Some(3.25),
                temperature_max_c: Some(5.25),
                humidity_min_pct: Some(40),
                humidity_max_pct: Some(43),
            }),
            "the window's min/max beside the average"
        );
        assert_eq!(
            storage
                .query_eye_readings("h1", "AA:BB:CC:DD:EE:01", 0, i64::MAX, None)
                .expect("query")
                .len(),
            1
        );
    }

    /// A QoS 1 redelivery or a spill replay of the same window is one row,
    /// and two windows are two.
    #[test]
    fn a_redelivered_window_is_deduped_by_message_id() {
        let (_dir, storage) = temp_storage();
        let t0 = 1_791_799_200;
        for _ in 0..3 {
            persist_eye_aggregated(
                &storage,
                &PersistCache::default(),
                "h1",
                aggregated(vec![aggregated_tag(MAC, t0, 4.0)]),
            )
            .expect("persist");
        }
        persist_eye_aggregated(
            &storage,
            &PersistCache::default(),
            "h1",
            aggregated(vec![aggregated_tag(MAC, t0 + 600, 5.0)]),
        )
        .expect("persist");
        let rows = storage
            .query_eye_readings("h1", MAC, 0, i64::MAX, None)
            .expect("query");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].temperature_c, Some(5.0));
    }

    /// Two Hubs hearing one Beacon mint the same id for the same window;
    /// each keeps its own row, so neither Hub's chart has holes.
    #[test]
    fn two_hubs_store_the_same_beacon_window_each() {
        let (_dir, storage) = temp_storage();
        let t0 = 1_791_799_200;
        for host in ["h1", "h2"] {
            persist_eye_aggregated(
                &storage,
                &PersistCache::default(),
                host,
                aggregated(vec![aggregated_tag(MAC, t0, 4.0)]),
            )
            .expect("persist");
        }
        for host in ["h1", "h2"] {
            let rows = storage
                .query_eye_readings(host, MAC, 0, i64::MAX, None)
                .expect("query");
            assert_eq!(rows.len(), 1, "{host}");
        }
    }

    /// A cache that has seen the Server `srv`'s `eye/homes`: `MAC` registered, homed on `home`
    /// (or homeless with `None`).
    fn cluster_cache(home: Option<&str>) -> PersistCache {
        let mut homes = std::collections::BTreeMap::new();
        if let Some(h) = home {
            homes.insert(MAC.to_string(), h.to_string());
        }
        PersistCache {
            beacon_cluster: Some(crate::state::models::BeaconCluster::from_msg(
                &fiber_protocol::payloads::eye::EyeHomesMsg {
                    v: 1,
                    server: "srv".into(),
                    registered: vec![MAC.into()],
                    homes,
                },
            )),
            ..PersistCache::default()
        }
    }

    /// Decision #42 (Q1): a registered Beacon's window is stored once, from its home Hub, under
    /// the registry's key, with the Hub that heard it in `source_hub`; the same window from
    /// another Hub and any window of an unregistered Beacon are not stored.
    #[test]
    fn a_cluster_beacon_window_is_stored_once_from_its_home_under_the_server() {
        let (_dir, storage) = temp_storage();
        let cache = cluster_cache(Some("c1"));
        let t0 = 1_791_799_200;
        for host in ["srv", "c1", "c2"] {
            persist_eye_aggregated(
                &storage,
                &cache,
                host,
                aggregated(vec![
                    aggregated_tag(MAC, t0, 4.0),
                    aggregated_tag("AA:BB:CC:DD:EE:01", t0, 9.0),
                ]),
            )
            .expect("persist");
        }
        let rows = storage
            .query_eye_readings("srv", MAC, 0, i64::MAX, None)
            .expect("query");
        assert_eq!(rows.len(), 1, "one row for three Hubs");
        let id = rows[0].message_id.clone().expect("id");
        assert_eq!(id, format!("srv/{MAC}-adv-{t0}"));
        assert_eq!(
            storage.eye_window_source_hub(&id).expect("read").as_deref(),
            Some("c1")
        );
        for host in ["c1", "c2"] {
            assert!(storage
                .query_eye_readings(host, MAC, 0, i64::MAX, None)
                .expect("query")
                .is_empty());
        }
        for host in ["srv", "c1", "c2"] {
            assert!(
                storage
                    .query_eye_readings(host, "AA:BB:CC:DD:EE:01", 0, i64::MAX, None)
                    .expect("query")
                    .is_empty(),
                "an unregistered Beacon has no history"
            );
        }
    }

    /// Before the Server has computed a home every Hub's copy is accepted, and the shared key
    /// dedupes them into one row.
    #[test]
    fn a_homeless_cluster_beacon_window_from_two_hubs_is_one_row() {
        let (_dir, storage) = temp_storage();
        let cache = cluster_cache(None);
        let t0 = 1_791_799_200;
        for host in ["c1", "c2"] {
            persist_eye_aggregated(
                &storage,
                &cache,
                host,
                aggregated(vec![aggregated_tag(MAC, t0, 4.0)]),
            )
            .expect("persist");
        }
        let rows = storage
            .query_eye_readings("srv", MAC, 0, i64::MAX, None)
            .expect("query");
        assert_eq!(rows.len(), 1);
    }

    /// Q1 for alarms: graded under the registry from the home Hub only, so a Hub that hears the
    /// Beacon worse neither raises nor clears it.
    #[test]
    fn a_cluster_beacon_alarm_is_graded_from_its_home_only() {
        let (_dir, storage) = temp_storage();
        let mut cache = cluster_cache(Some("c1"));
        let (ntx, _nrx, gtx, _grx) = channels();
        storage
            .upsert_field_threshold(
                &portal_storage::NewFieldThreshold {
                    hostname: "srv".to_string(),
                    scope: THRESHOLD_SCOPE_EYE.to_string(),
                    entity: MAC.to_string(),
                    field: "temperature".to_string(),
                    critical_low: None,
                    warning_low: None,
                    warning_high: Some(8.0),
                    critical_high: Some(10.0),
                    enabled: true,
                },
                Some("mgr1"),
            )
            .expect("band");
        let sensors = |host: &str, temperature: f64| {
            let mut tag = eye_tag_at(MAC, 100);
            tag.temperature_c = Some(temperature);
            tag.discovered = Some(false);
            IngestEvent::EyeSensors {
                hostname: host.to_string(),
                payload: fiber_protocol::payloads::eye::EyeSensorsMsg {
                    timestamp: None,
                    tags: vec![tag],
                    discovered_capped: false,
                },
            }
        };
        let sensor_id = format!("eye:{MAC}:Temperature");
        // The non-home Hub reads it too warm: nothing.
        handle(&storage, sensors("c2", 12.0), &mut cache, &ntx, &gtx).expect("c2");
        assert!(storage
            .active_alarm("srv", &sensor_id)
            .expect("read")
            .is_none());
        assert!(storage
            .active_alarm("c2", &sensor_id)
            .expect("read")
            .is_none());
        // The home does: one alarm, under the registry.
        handle(&storage, sensors("c1", 12.0), &mut cache, &ntx, &gtx).expect("c1");
        assert!(storage
            .active_alarm("srv", &sensor_id)
            .expect("read")
            .is_some());
        // The non-home Hub reading it normal does not close it.
        handle(&storage, sensors("c2", 4.0), &mut cache, &ntx, &gtx).expect("c2");
        assert!(storage
            .active_alarm("srv", &sensor_id)
            .expect("read")
            .is_some());
        handle(&storage, sensors("c1", 4.0), &mut cache, &ntx, &gtx).expect("c1");
        assert!(storage
            .active_alarm("srv", &sensor_id)
            .expect("read")
            .is_none());
    }

    /// The cluster is learnt from the Server's own `eye/homes` only.
    #[test]
    fn only_the_servers_own_homes_document_names_the_cluster() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, _nrx, gtx, _grx) = channels();
        let doc = |host: &str| IngestEvent::EyeHomes {
            hostname: host.to_string(),
            payload: fiber_protocol::payloads::eye::EyeHomesMsg {
                v: 1,
                server: "SRV".into(),
                registered: vec![MAC.into()],
                homes: Default::default(),
            },
        };
        handle(&storage, doc("c1"), &mut cache, &ntx, &gtx).expect("stray");
        assert!(cache.beacon_cluster.is_none());
        handle(&storage, doc("srv"), &mut cache, &ntx, &gtx).expect("own");
        assert_eq!(
            cache.beacon_source("c2", MAC),
            BeaconSource::Home {
                registry: "srv".into()
            }
        );
        assert_eq!(
            cache.beacon_source("c2", "AA:BB:CC:DD:EE:01"),
            BeaconSource::Stranger {
                registry: "srv".into()
            }
        );
    }

    /// The live snapshot no longer writes history at all (S4: the
    /// snapshot-driven store gate is gone).
    #[test]
    fn an_eye_sensors_snapshot_writes_no_history_row() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, _nrx, gtx, _grx) = channels();
        for ts in [0, 10, 900, 1800] {
            handle(
                &storage,
                IngestEvent::EyeSensors {
                    hostname: "h1".into(),
                    payload: EyeSensorsMsg {
                        timestamp: None,
                        discovered_capped: false,
                        tags: vec![eye_tag_at(MAC, ts)],
                    },
                },
                &mut cache,
                &ntx,
                &gtx,
            )
            .expect("handled");
        }
        assert!(storage
            .query_eye_readings("h1", MAC, 0, i64::MAX, None)
            .expect("query")
            .is_empty());
    }

    /// Routed through `handle`, so a spill replay takes the same path.
    #[test]
    fn the_aggregated_event_is_persisted_through_handle() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, _nrx, gtx, _grx) = channels();
        handle(
            &storage,
            IngestEvent::EyeAggregated {
                hostname: "h1".into(),
                payload: aggregated(vec![aggregated_tag(MAC, 1_791_799_200, 3.5)]),
            },
            &mut cache,
            &ntx,
            &gtx,
        )
        .expect("handled");
        assert_eq!(
            storage
                .query_eye_readings("h1", MAC, 0, i64::MAX, None)
                .expect("query")
                .len(),
            1
        );
    }

    // ---- integration: temp Storage ---------------------------------------

    fn temp_storage() -> (tempfile::TempDir, Storage) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("portal.db");
        let storage = Storage::open(&db_path, None).expect("open storage");
        (dir, storage)
    }

    /// HS-2: a disconnected line's minute (no samples, 0.0 °C on the wire) keeps its row but
    /// no temperature, so a chart or CSV shows a gap and not a real 0 °C.
    #[test]
    fn a_minute_without_samples_has_no_temperature() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, _nrx, gtx, _grx) = channels();
        // (ts, sample_count, disconnect_count, avg): the Hub counts disconnects as
        // samples, so a fully disconnected minute has as many of each.
        for (ts, count, disconnects, avg) in [
            (60, Some(0), 60, 0.0),
            (120, Some(3), 0, 4.5),
            (180, Some(8), 8, 0.0),
            (240, Some(8), 2, 5.0),
        ] {
            handle(
                &storage,
                IngestEvent::Probe1m {
                    hostname: "h1".into(),
                    line: 0,
                    payload: fiber_protocol::payloads::export::Probe1mData {
                        minute_ts: Some(ts),
                        sensor_line: Some(0),
                        min_c: Some(avg),
                        avg_c: Some(avg),
                        max_c: Some(avg),
                        sample_count: count,
                        disconnect_count: Some(disconnects),
                        worst_alarm: None,
                        data_hmac: None,
                    },
                },
                &mut cache,
                &ntx,
                &gtx,
            )
            .expect("persist");
        }
        let rows = storage.query_minutes("h1", 0, 0, 300).expect("read");
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].avg_c, None, "no samples, no temperature");
        assert_eq!(rows[0].disconnect_count, Some(60), "the row itself stays");
        assert_eq!(rows[1].avg_c, Some(4.5));
        assert_eq!(rows[2].avg_c, None, "only disconnects, no temperature");
        assert_eq!(
            rows[3].avg_c,
            Some(5.0),
            "some measured samples keep theirs"
        );
    }

    /// LE-16: the cleared e-mail after an acknowledge names the acknowledger's reason, the
    /// note the acknowledge wrote.
    #[test]
    fn a_cleared_alarm_carries_the_acknowledge_reason() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, mut nrx, gtx, _grx) = channels();
        wired_band(&storage);
        live_line0(&storage, &mut cache, &ntx, &gtx, 15.0);
        let _ = nrx.try_recv();
        let (rows, _) = storage.list_alarms(&Default::default()).unwrap();
        let id = rows[0].id.clone();
        storage
            .acknowledge_alarm(&id, Some(7), "anna", "2026-01-01T00:00:00Z")
            .unwrap();
        storage
            .insert_alarm_note(&id, Some(7), "anna", "door left open, closed it")
            .unwrap();
        live_line0(&storage, &mut cache, &ntx, &gtx, 5.0);
        match nrx.try_recv() {
            Ok(NotifyJob::Cleared { ack: Some(ack), .. }) => {
                assert_eq!(ack.user, "anna");
                assert_eq!(ack.reason.as_deref(), Some("door left open, closed it"));
            }
            other => panic!("expected a cleared job with the ack: {other:?}"),
        }
    }

    // ---- the spill journal: replay of deferred ingest ---------------------

    /// One `sensors/aggregated` publish, as the broker delivers it — real wire
    /// JSON, so replay exercises the actual decoder rather than a shape invented
    /// for the test.
    fn aggregated_publish(hostname: &str, ts: i64, avg: f64) -> (String, Vec<u8>) {
        let topic = format!("proximos/{hostname}/sensors/aggregated");
        let payload = serde_json::json!({
            "period_end_ts": ts,
            "sensors": [{
                "line": 1,
                "sample_count": 3,
                "temperature": { "min_celsius": 1.0, "avg_celsius": avg, "max_celsius": 9.0 },
            }],
        });
        (topic, serde_json::to_vec(&payload).expect("encode"))
    }

    /// The 2026-08-24 defect, pinned.
    ///
    /// `apply_alarm_actions` used to answer a storage failure with
    /// `tracing::warn!` and carry on, so `handle` returned `Ok(())` for an
    /// alarm row that was never written — and because the notify job is gated
    /// on the insert's returned id, the notification vanished with it. That is
    /// how a full VACUUM came to discard alarms silently.
    ///
    /// The error must now reach the caller, which is what lets the writer loop
    /// retry it and, failing that, spill it.
    #[test]
    fn a_refused_alarm_insert_is_reported_rather_than_swallowed() {
        let (dir, storage) = temp_storage();
        let cache = PersistCache::default();
        let (ntx, mut nrx, gtx, _grx) = channels();

        // Impatient, so the held lock below is a refusal rather than a wait.
        storage
            .set_busy_timeout(std::time::Duration::from_millis(1))
            .expect("busy_timeout");

        let blocker =
            rusqlite::Connection::open(dir.path().join("portal.db")).expect("open blocker");
        blocker
            .execute_batch(
                "BEGIN EXCLUSIVE; INSERT INTO readings_raw \
                 (hostname, sensor_line, ts, min_c, avg_c, max_c, sample_count) \
                 VALUES ('blocker', 9, 1, 0, 0, 0, 1);",
            )
            .expect("hold the write lock");

        let extra: serde_json::Map<String, serde_json::Value> = serde_json::json!({
            "slot": "mb", "rule": 2, "channel": "temperature", "edge": "raise", "side": "high",
            "value": 42.0, "base_time": 1000, "rel_s": 30
        })
        .as_object()
        .unwrap()
        .clone();
        let mut sensor = lorawan_sensor("aa11", "2026-08-18T00:00:00Z", 21.0);
        sensor.events.push(LorawanEvent {
            event_type: "alarm".to_string(),
            ts: Some("2026-08-18T00:00:00Z".to_string()),
            extra,
        });

        let result = run_lorawan_alarm_engine(&storage, &cache, "h1", "aa11", &sensor, &ntx, &gtx);

        assert!(
            result.is_err(),
            "a refused alarm write must be reported, not logged and forgotten"
        );
        assert!(
            result.as_ref().unwrap_err().is_retryable(),
            "and classified as contention, so the caller retries before spilling: {:?}",
            result.unwrap_err()
        );
        assert!(
            nrx.try_recv().is_err(),
            "no notification for a row that never committed — it fires on replay instead"
        );

        blocker.execute_batch("ROLLBACK").expect("release the lock");
    }

    /// A publish spilled during a rewrite lands in the database on replay —
    /// through `route_publish`, the same decoder the live path uses.
    #[test]
    fn a_spilled_publish_is_replayed_into_the_database() {
        let (dir, storage) = temp_storage();
        let gate = crate::durable::test_gate(dir.path().to_str().expect("utf8"));
        let (ntx, _nrx, gtx, _grx) = channels();
        let mut cache = PersistCache::default();

        let (topic, payload) = aggregated_publish("hub-1", 1_700_000_000, 4.5);
        gate.append(
            SpillKind::Mqtt,
            1_700_000_000,
            &durable::encode_publish(&topic, &payload),
        )
        .expect("append");

        assert_eq!(
            storage.count_raw("hub-1", 1).expect("count"),
            0,
            "nothing is in the database while the row sits in the journal"
        );
        drain_spill(&storage, &gate, "proximos", &mut cache, &ntx, &gtx, "test");

        assert_eq!(
            storage.count_raw("hub-1", 1).expect("count"),
            1,
            "replay must land the deferred reading"
        );
        assert_eq!(gate.depth().expect("depth"), 0, "and empty the journal");
    }

    /// The crash case: a process dies mid-rewrite with events in the journal.
    /// Reopening replays every one of them, in order, exactly once — and doing
    /// it twice does not double-count, which is what makes a crash *during*
    /// replay survivable.
    #[test]
    fn a_journal_left_by_a_crash_replays_in_order_exactly_once() {
        let (dir, storage) = temp_storage();
        let gate = crate::durable::test_gate(dir.path().to_str().expect("utf8"));
        let (ntx, _nrx, gtx, _grx) = channels();
        let mut cache = PersistCache::default();

        for i in 0..20i64 {
            let (topic, payload) = aggregated_publish("hub-1", 1_700_000_000 + i * 60, i as f64);
            gate.append(
                SpillKind::Mqtt,
                1_700_000_000 + i * 60,
                &durable::encode_publish(&topic, &payload),
            )
            .expect("append");
        }

        // ... the process dies here, without replaying ...
        drain_spill(
            &storage, &gate, "proximos", &mut cache, &ntx, &gtx, "startup",
        );

        assert_eq!(
            storage.count_raw("hub-1", 1).expect("count"),
            20,
            "every accepted event must survive the crash"
        );

        // (Replay *order* is pinned in `portal-storage/tests/spill.rs`
        // — `rows_replay_in_the_order_they_were_accepted` — where the journal
        // can be observed directly. This crate has no range reader for
        // `readings_raw` to assert it through.)

        // A second drain is a no-op: the rows were deleted as they committed.
        drain_spill(
            &storage, &gate, "proximos", &mut cache, &ntx, &gtx, "startup",
        );
        assert_eq!(
            storage.count_raw("hub-1", 1).expect("count"),
            20,
            "replaying an empty journal must not duplicate anything"
        );
    }

    /// An audit row deferred by `AuthService` replays through the same drain,
    /// even though a different connection spilled it.
    #[test]
    fn a_spilled_audit_row_is_replayed_too() {
        let (dir, storage) = temp_storage();
        let gate = crate::durable::test_gate(dir.path().to_str().expect("utf8"));
        let (ntx, _nrx, gtx, _grx) = channels();
        let mut cache = PersistCache::default();

        let entry = portal_storage::NewAuditEntry {
            event_type: "login".to_string(),
            username: Some("nurse".to_string()),
            user_id: None,
            ip: None,
            target: None,
            details: None,
            success: false,
        };
        gate.append(
            SpillKind::Audit,
            1_700_000_000,
            &serde_json::to_vec(&entry).expect("encode"),
        )
        .expect("append");
        drain_spill(&storage, &gate, "proximos", &mut cache, &ntx, &gtx, "test");

        let (rows, _) = storage
            .list_audit(&portal_storage::AuditFilter {
                limit: 10,
                ..Default::default()
            })
            .expect("list audit");
        assert_eq!(rows.len(), 1, "audit rows are evidence — never dropped");
        assert_eq!(rows[0].username.as_deref(), Some("nurse"));
    }

    /// The §3 ordering claim, for the writers that are *not* the persist
    /// thread.
    ///
    /// `AuthService`, the notifier and the request path's `Db` hold their own connections and
    /// are opened by `main` before the startup drain runs. While the gate
    /// opened `Direct`, a fresh audit row from one of them could commit ahead of
    /// an older, already-accepted one still sitting in the journal — "replay
    /// before any newer work" held only for ingest. A journal with rows in it
    /// now opens the gate raised, so they spill and take their turn.
    #[test]
    fn a_writer_that_starts_before_the_startup_replay_finishes_spills_instead_of_overtaking_it() {
        let (dir, storage) = temp_storage();
        let data_dir = dir.path().to_str().expect("utf8").to_string();

        let crashed = portal_storage::NewAuditEntry {
            event_type: "accepted_before_the_crash".to_string(),
            username: Some("nurse".to_string()),
            user_id: None,
            ip: None,
            target: None,
            details: None,
            success: true,
        };
        {
            let gate = crate::durable::test_gate(&data_dir);
            gate.append(
                SpillKind::Audit,
                1_700_000_000,
                &serde_json::to_vec(&crashed).expect("encode"),
            )
            .expect("append");
        }

        // Reopened the way `main` does, with the journal still holding a row.
        let gate = crate::durable::test_gate(&data_dir);
        assert_eq!(
            gate.phase(),
            portal_storage::SpillPhase::Replaying,
            "a backlog must defer every writer, not just the persist thread"
        );

        let fresh = portal_storage::NewAuditEntry {
            event_type: "written_after_the_restart".to_string(),
            ..crashed.clone()
        };
        assert!(
            matches!(
                crate::durable::insert_audit(&gate, &storage, &fresh),
                Ok(Landed::Spilled(_))
            ),
            "a writer opening during the backlog must spill, not commit ahead of it"
        );

        let (ntx, _nrx, gtx, _grx) = channels();
        let mut cache = PersistCache::default();
        drain_until_empty(
            &storage, &gate, "proximos", &mut cache, &ntx, &gtx, "startup",
        );
        gate.resume_direct();

        let (rows, _) = storage
            .list_audit(&portal_storage::AuditFilter {
                limit: 10,
                ..Default::default()
            })
            .expect("list audit");
        let order: Vec<&str> = rows.iter().map(|r| r.event_type.as_str()).collect();
        assert_eq!(
            order,
            vec!["written_after_the_restart", "accepted_before_the_crash"],
            "acceptance order must survive the restart (list_audit is newest-first)"
        );
        assert_eq!(gate.phase(), portal_storage::SpillPhase::Direct);
    }

    /// A stray replay must not resolve a gate it did not raise.
    ///
    /// The regression: `PersistMsg::ReplaySpill` ended with an unconditional
    /// `resume_direct()`, but `RetentionTask::kick_a_stray_replay` sends that
    /// message one statement before `maintenance_tick` claims a rewrite. With
    /// any backlog on this channel the stray message is dequeued *after* the
    /// gate went up, and lowering it there put every writer back on the
    /// VACUUM-locked database for the rest of the run — four attempts against a
    /// 30 s `busy_timeout` apiece before each finally spilled.
    #[test]
    fn a_stray_replay_leaves_the_gate_where_it_found_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8").to_string();
        let spill = crate::durable::test_gate(&data_dir);
        let (ntx, _nrx, gtx, _grx) = channels();

        let tx = spawn(
            &data_dir,
            None,
            "proximos".to_string(),
            Arc::clone(&spill),
            ntx,
            gtx,
            None,
            None,
            None,
            None,
        )
        .expect("spawn the persist thread");

        // Wait for the thread to be genuinely up before raising anything: its
        // startup drain ends in `resume_direct()`, which would otherwise undo
        // the raise below. A publish that reaches the database proves the drain
        // is behind us.
        let reader = Storage::open(&dir.path().join("portal.db"), None).expect("open reader");
        tx.send(publish_msg("hub-0", 1_700_000_000, 1.0))
            .expect("send the warm-up publish");
        wait_for(
            || reader.count_raw("hub-0", 1).unwrap_or(0) == 1,
            "warm-up publish",
        );

        // A rewrite claims the run and raises the gate, exactly as
        // `Maintenance::start_full_vacuum` does.
        let lease = SpillGate::raise(&spill);
        assert_eq!(spill.phase(), portal_storage::SpillPhase::Spilling);

        // ... and the stray replay the retention sweep queued a moment earlier
        // arrives now.
        tx.send(PersistMsg::ReplaySpill { owns_gate: false })
            .expect("send the stray replay");

        // Round-trip a publish behind it: the thread is a FIFO, so a spilled
        // row here means the stray replay has already been handled.
        tx.send(publish_msg("hub-1", 1_700_000_000, 4.5))
            .expect("send a publish");
        wait_for(
            || spill.depth().unwrap_or(-1) == 1,
            "the publish behind the stray replay to spill",
        );

        assert_eq!(
            spill.phase(),
            portal_storage::SpillPhase::Spilling,
            "a stray replay must not lower a gate the rewrite still owns"
        );

        // The rewrite's own replay does resolve it.
        tx.send(PersistMsg::ReplaySpill { owns_gate: true })
            .expect("send the owning replay");
        wait_for(
            || spill.phase() == portal_storage::SpillPhase::Direct,
            "the owning replay to lower the gate",
        );
        assert_eq!(spill.depth().expect("depth"), 0, "and drain the backlog");
        assert_eq!(
            reader.count_raw("hub-1", 1).expect("count"),
            1,
            "the deferred publish lands on replay"
        );

        lease.handed_to_replay();
    }

    /// One routed publish, ready to hand to the persist thread.
    fn publish_msg(hostname: &str, ts: i64, avg: f64) -> PersistMsg {
        let (topic, payload) = aggregated_publish(hostname, ts, avg);
        let event = match routing::route_publish(&topic, &payload, "proximos") {
            routing::RouteResult::Event(event) => event,
            _ => panic!("the fixture must route to an event"),
        };
        PersistMsg::Publish {
            topic,
            payload,
            event,
        }
    }

    /// Poll `cond` for up to five seconds. The persist thread is a real thread,
    /// so every assertion about what it has done has to wait for it.
    fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
        for _ in 0..500 {
            if cond() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
    }

    fn channels() -> (
        mpsc::UnboundedSender<NotifyJob>,
        mpsc::UnboundedReceiver<NotifyJob>,
        broadcast::Sender<AlarmNudge>,
        broadcast::Receiver<AlarmNudge>,
    ) {
        let (ntx, nrx) = mpsc::unbounded_channel();
        let (gtx, grx) = broadcast::channel(16);
        (ntx, nrx, gtx, grx)
    }

    fn lorawan_sensor(node_key: &str, last_seen: &str, temperature: f64) -> LorawanSensor {
        LorawanSensor {
            dev_eui: node_key.to_string(),
            name: None,
            serial_number: None,
            location: None,
            fields: HashMap::from([("temperature".to_string(), temperature)]),
            field_alarm_states: HashMap::new(),
            field_thresholds: None,
            counters: HashMap::new(),
            events: vec![],
            rssi: None,
            snr: None,
            gateways: vec![],
            fcnt: None,
            dr: None,
            downlink_gateway_id: None,
            last_seen: Some(last_seen.to_string()),
            alarm_state: None,
        }
    }

    /// GitLab #80, the portal's half: a band the operator switched off stops
    /// being evaluated, and an alarm it had already raised is CLEARED rather
    /// than left open.
    ///
    /// The graded state is the device's echo, so unticking the band locally
    /// used to change nothing at all. The field now reads Normal, which is
    /// what walks the engine's clearing branch — dropping the field instead
    /// would have stranded the open alarm with nothing left to resolve it.
    #[test]
    fn a_switched_off_eye_band_clears_its_alarm_and_raises_no_more() {
        let (_dir, storage) = temp_storage();
        let cache = PersistCache::default();
        let (ntx, _nrx, gtx, _grx) = channels();

        let mut tag = eye_tag_at("AA:BB:CC:DD:EE:01", 0);
        tag.humidity_pct = Some(88);
        tag.field_alarm_states = HashMap::from([("humidity".to_string(), "WARNING".to_string())]);

        let band = |enabled: bool| portal_storage::NewFieldThreshold {
            hostname: "h1".to_string(),
            scope: THRESHOLD_SCOPE_EYE.to_string(),
            entity: "AA:BB:CC:DD:EE:01".to_string(),
            field: "humidity".to_string(),
            critical_low: None,
            warning_low: None,
            warning_high: Some(80.0),
            critical_high: Some(90.0),
            enabled,
        };

        storage
            .upsert_field_threshold(&band(true), Some("mgr1"))
            .expect("band on");
        run_eye_alarm_engine(&storage, &cache, "h1", &tag, &ntx, &gtx).expect("engine");
        let sensor_id = "eye:AA:BB:CC:DD:EE:01:Humidity";
        assert!(
            storage
                .active_alarm("h1", sensor_id)
                .expect("read")
                .is_some(),
            "the enabled band raises it"
        );

        // Switched off, with the device still echoing WARNING.
        storage
            .upsert_field_threshold(&band(false), Some("mgr1"))
            .expect("band off");
        run_eye_alarm_engine(&storage, &cache, "h1", &tag, &ntx, &gtx).expect("engine");
        assert!(
            storage
                .active_alarm("h1", sensor_id)
                .expect("read")
                .is_none(),
            "switching the band off clears the alarm it had raised"
        );

        // And it stays cleared while the echo continues.
        run_eye_alarm_engine(&storage, &cache, "h1", &tag, &ntx, &gtx).expect("engine");
        assert!(
            storage
                .active_alarm("h1", sensor_id)
                .expect("read")
                .is_none(),
            "and raises no new one"
        );
    }

    /// A removed Beacon keeps advertising as an unregistered candidate with no
    /// `field_alarm_states`; an alarm it raised while registered must not stay
    /// open for good with nothing left to grade it.
    #[test]
    fn an_alarm_raised_while_registered_clears_once_the_beacon_is_unregistered() {
        let (_dir, storage) = temp_storage();
        let cache = PersistCache::default();
        let (ntx, _nrx, gtx, _grx) = channels();

        let mut tag = eye_tag_at("AA:BB:CC:DD:EE:01", 0);
        tag.humidity_pct = Some(91);
        tag.field_alarm_states = HashMap::from([("humidity".to_string(), "CRITICAL".to_string())]);
        storage
            .upsert_field_threshold(
                &portal_storage::NewFieldThreshold {
                    hostname: "h1".to_string(),
                    scope: THRESHOLD_SCOPE_EYE.to_string(),
                    entity: "AA:BB:CC:DD:EE:01".to_string(),
                    field: "humidity".to_string(),
                    critical_low: Some(10.0),
                    warning_low: Some(20.0),
                    warning_high: Some(80.0),
                    critical_high: Some(90.0),
                    enabled: true,
                },
                Some("mgr1"),
            )
            .expect("band");
        run_eye_alarm_engine(&storage, &cache, "h1", &tag, &ntx, &gtx).expect("engine");
        let sensor_id = "eye:AA:BB:CC:DD:EE:01:Humidity";
        assert!(
            storage
                .active_alarm("h1", sensor_id)
                .expect("read")
                .is_some(),
            "raised while registered"
        );

        // Removed: the Hub now reports it as a candidate, with no alarm states at all.
        let mut candidate = eye_tag_at("AA:BB:CC:DD:EE:01", 30);
        candidate.humidity_pct = Some(91);
        candidate.discovered = Some(true);
        run_eye_alarm_engine(&storage, &cache, "h1", &candidate, &ntx, &gtx).expect("engine");
        assert!(
            storage
                .active_alarm("h1", sensor_id)
                .expect("read")
                .is_none(),
            "an unregistered Beacon has no bounds, so its open alarm clears"
        );

        // A registered Beacon with nothing to report is still left alone.
        let mut quiet = eye_tag_at("AA:BB:CC:DD:EE:02", 60);
        quiet.discovered = Some(false);
        run_eye_alarm_engine(&storage, &cache, "h1", &quiet, &ntx, &gtx).expect("engine");
    }

    /// #110 on a Beacon and a Node: the portal grades the value with the field's hysteresis
    /// (default 0.5 °C, 3 % RH; own value when set), whatever the Hub's echo says.
    #[test]
    fn beacon_and_node_fields_are_graded_with_hysteresis() {
        let (_dir, storage) = temp_storage();
        let cache = PersistCache::default();
        let (ntx, _nrx, gtx, _grx) = channels();
        let band = |scope: &str, entity: &str, field: &str, high: f64| {
            storage
                .upsert_field_threshold(
                    &portal_storage::NewFieldThreshold {
                        hostname: "h1".to_string(),
                        scope: scope.to_string(),
                        entity: entity.to_string(),
                        field: field.to_string(),
                        critical_low: None,
                        warning_low: None,
                        warning_high: Some(high),
                        critical_high: None,
                        enabled: true,
                    },
                    Some("mgr1"),
                )
                .expect("band");
        };

        // Beacon humidity, warning_high 80 %: 3 % RH hysteresis.
        band(THRESHOLD_SCOPE_EYE, "AA:BB:CC:DD:EE:01", "humidity", 80.0);
        for (pct, open) in [(81, true), (79, true), (82, true), (78, true), (77, false)] {
            let mut tag = eye_tag_at("AA:BB:CC:DD:EE:01", 0);
            tag.humidity_pct = Some(pct);
            // The Hub's own grade is ignored for a field with bounds.
            tag.field_alarm_states =
                HashMap::from([("humidity".to_string(), "NORMAL".to_string())]);
            run_eye_alarm_engine(&storage, &cache, "h1", &tag, &ntx, &gtx).expect("engine");
            assert_eq!(
                storage.count_active(Some("h1")).unwrap(),
                usize::from(open) as i64,
                "{pct} %"
            );
        }
        let (_, total) = storage.list_alarms(&Default::default()).unwrap();
        assert_eq!(total, 1, "one alarm for the whole excursion");

        // Node temperature, warning_high 30 °C, own hysteresis 1 °C.
        let node = "58760700c0668fc7";
        band(THRESHOLD_SCOPE_NODE, node, "temperature", 30.0);
        storage
            .set_field_threshold_hysteresis(
                "h1",
                THRESHOLD_SCOPE_NODE,
                node,
                "temperature",
                Some(1.0),
            )
            .unwrap();
        for (i, (temp, open)) in [(30.1, true), (29.5, true), (29.1, true), (28.9, false)]
            .into_iter()
            .enumerate()
        {
            let reading = node_reading(&format!("m-{i}"), "2026-09-28T08:15:00Z", temp);
            run_node_threshold_engine(&storage, &cache, "h1", node, &reading, &ntx, &gtx)
                .expect("engine");
            assert_eq!(
                storage.count_active(Some("h1")).unwrap(),
                usize::from(open) as i64,
                "{temp} °C"
            );
        }
    }

    #[test]
    fn lorawan_sensors_gate_and_history_credit_end_to_end() {
        let (_dir, storage) = temp_storage();
        let mut cache = HashMap::new();

        let sensor = lorawan_sensor("aa11", "2026-01-13T14:30:00Z", 21.5);
        let payload = LorawanSensorsMsg {
            timestamp: None,
            sensors: vec![sensor.clone()],
        };

        persist_lorawan_sensors(&storage, "hub1", payload.clone(), &mut cache)
            .expect("first uplink persists");
        let readings = storage
            .query_field_readings("hub1", "aa11", "temperature", 0, i64::MAX)
            .expect("query");
        assert_eq!(readings.len(), 1, "first last_seen observation is recorded");

        // Same last_seen again: the gate must skip it entirely, not just
        // rely on insert dedup — no new row.
        persist_lorawan_sensors(&storage, "hub1", payload, &mut cache)
            .expect("repeated uplink is a no-op");
        let readings = storage
            .query_field_readings("hub1", "aa11", "temperature", 0, i64::MAX)
            .expect("query");
        assert_eq!(readings.len(), 1, "unchanged last_seen must not re-persist");

        // A backfill job waiting on this Node's history gets credited and
        // completed when the history sink processes the (only) frame.
        let job = storage
            .create_job(&NewBackfillJob {
                hostname: "hub1".to_string(),
                node_key: "aa11".to_string(),
                time_from: 0,
                time_to: 1_000_000,
                trigger: "manual".to_string(),
                samples_expected: None,
            })
            .expect("create job");
        storage
            .update_job_status(job.job_id, JOB_REQUESTED, None, false)
            .expect("mark requested");

        let history = NodeHistoryMsg {
            dev_eui: Some("aa11".to_string()),
            frame_index: Some(0),
            frame_count: Some(1),
            interval_s: None,
            records: vec![history_record(Some(1_736_775_000), Some(22.0))],
        };
        persist_sticker_history(&storage, "hub1", "aa11", history)
            .expect("history frame persists and credits the job");

        let job = storage
            .get_job(job.job_id)
            .expect("get job")
            .expect("job exists");
        // 1 field row + 1 counter row from the single history record.
        assert_eq!(job.samples_received, 2);
        assert_eq!(job.status, JOB_DONE);
    }

    /// P-9 / N32: the grade that raises the alarm is also what the pages read — the engine
    /// hands every reported field's state to the sink, bounded or not.
    #[test]
    fn the_grade_goes_to_the_value_axis_too() {
        let (_dir, storage) = temp_storage();
        type Seen = Vec<(String, String, std::collections::BTreeMap<String, String>)>;
        let seen: Arc<std::sync::Mutex<Seen>> = Arc::default();
        let sink_seen = Arc::clone(&seen);
        let cache = PersistCache {
            field_states: Some(Arc::new(
                move |h: &str, n: &str, s: &std::collections::BTreeMap<String, String>| {
                    sink_seen
                        .lock()
                        .expect("lock")
                        .push((h.to_string(), n.to_string(), s.clone()));
                },
            )),
            ..PersistCache::default()
        };
        let (ntx, _nrx, gtx, _grx) = channels();
        let node = "58760700c0668fc7";
        storage
            .upsert_field_threshold(
                &portal_storage::NewFieldThreshold {
                    hostname: "h1".to_string(),
                    scope: THRESHOLD_SCOPE_NODE.to_string(),
                    entity: node.to_string(),
                    field: "temperature".to_string(),
                    critical_low: Some(5.0),
                    warning_low: Some(10.0),
                    warning_high: Some(28.0),
                    critical_high: Some(30.0),
                    enabled: true,
                },
                Some("mgr1"),
            )
            .expect("band on");
        run_node_threshold_engine(
            &storage,
            &cache,
            "h1",
            node,
            &node_reading("m-1", "2026-09-26T00:00:00Z", 31.0),
            &ntx,
            &gtx,
        )
        .expect("engine");
        let seen = seen.lock().expect("lock");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "h1");
        assert_eq!(seen[0].1, node);
        assert_eq!(
            seen[0].2.get("temperature").map(String::as_str),
            Some("CRITICAL")
        );
    }

    /// nodes_phase2.cs.md P1: the portal grades a Node's telemetry against the bounds it sent,
    /// warning and critical, and that is the sensor's alarm: opened, re-graded, closed.
    #[test]
    fn telemetry_grades_a_nodes_bounds_into_the_sensors_alarm() {
        let (_dir, storage) = temp_storage();
        let cache = PersistCache::default();
        let (ntx, _nrx, gtx, _grx) = channels();
        let node = "58760700c0668fc7";
        let band = |enabled: bool| portal_storage::NewFieldThreshold {
            hostname: "h1".to_string(),
            scope: THRESHOLD_SCOPE_NODE.to_string(),
            entity: node.to_string(),
            field: "temperature".to_string(),
            critical_low: Some(5.0),
            warning_low: Some(10.0),
            warning_high: Some(28.0),
            critical_high: Some(30.0),
            enabled,
        };
        storage
            .upsert_field_threshold(&band(true), Some("mgr1"))
            .expect("band on");
        let sensor_id = format!("lrw:{node}:Temperature");
        let severity = |storage: &Storage| {
            storage
                .active_alarm("h1", &sensor_id)
                .expect("read")
                .map(|a| a.severity)
        };
        let grade = |t: f64, at: &str| {
            run_node_threshold_engine(
                &storage,
                &cache,
                "h1",
                node,
                &node_reading(&format!("m-{at}"), at, t),
                &ntx,
                &gtx,
            )
            .expect("engine")
        };

        grade(20.0, "2026-09-24T17:00:00Z");
        assert_eq!(severity(&storage), None, "inside the bounds");
        grade(29.0, "2026-09-24T17:15:00Z");
        assert_eq!(severity(&storage).as_deref(), Some("warning"));
        grade(32.4, "2026-09-24T17:30:00Z");
        assert_eq!(severity(&storage).as_deref(), Some("critical"), "re-graded");
        grade(20.0, "2026-09-24T17:45:00Z");
        assert_eq!(severity(&storage), None, "back inside closes it");

        grade(32.4, "2026-09-24T18:00:00Z");
        storage
            .upsert_field_threshold(&band(false), Some("mgr1"))
            .expect("band off");
        grade(32.4, "2026-09-24T18:15:00Z");
        assert_eq!(severity(&storage), None, "switched-off bounds close it");
    }

    /// threshold_defaults.md §5: a band left on the battery from an earlier build is never
    /// graded, however low the battery reads — the Node's own alarm is the battery's alarm.
    #[test]
    fn a_leftover_battery_band_raises_nothing() {
        let (_dir, storage) = temp_storage();
        let cache = PersistCache::default();
        let (ntx, _nrx, gtx, _grx) = channels();
        let node = "58760700c0668fc7";
        storage
            .upsert_field_threshold(
                &portal_storage::NewFieldThreshold {
                    hostname: "h1".to_string(),
                    scope: THRESHOLD_SCOPE_NODE.to_string(),
                    entity: node.to_string(),
                    field: "voltage".to_string(),
                    critical_low: Some(3.0),
                    warning_low: Some(3.3),
                    warning_high: None,
                    critical_high: None,
                    enabled: true,
                },
                Some("mgr1"),
            )
            .expect("an old battery band");
        let mut reading = node_reading("m-1", "2026-09-28T08:00:00Z", 20.0);
        reading.fields.insert("voltage".to_string(), 2.1);
        run_node_threshold_engine(&storage, &cache, "h1", node, &reading, &ntx, &gtx)
            .expect("engine");
        assert!(storage
            .active_alarm("h1", &format!("lrw:{node}:Battery"))
            .expect("read")
            .is_none());
    }

    // ---- WP-E: the v2 radio northbound family -----------------------------

    /// A `NodeReading` as `proximos-control` publishes it — built through the producer's own
    /// type, so a change to the envelope is a compile error here rather than a fixture that
    /// quietly stops matching the wire.
    fn node_reading(message_id: &str, received_at: &str, temperature: f64) -> NodeReadingMsg {
        NodeReadingMsg {
            message_id: message_id.to_string(),
            dev_eui: "58760700c0668fc7".to_string(),
            serial: Some("SN-1".to_string()),
            fields: [("temperature".to_string(), temperature)]
                .into_iter()
                .collect(),
            counters: [("hall_left_count".to_string(), 7u64)]
                .into_iter()
                .collect(),
            events: Vec::new(),
            gateways: vec![control_radio::decode::reading::GatewayRx {
                gateway_id: "24e124fffefd3bda".to_string(),
                rssi: Some(-41),
                snr: Some(13.5),
            }],
            dr: Some(5),
            rssi: Some(-41),
            snr: Some(13.5),
            received_at: received_at.to_string(),
            fport: Some(2),
            page_index: None,
            page_count: None,
        }
    }

    /// The end-to-end shape claim: a v2 `reading` lands in the same rows
    /// `persist_lorawan_sensors` writes, and a redelivery of the same `message_id` adds
    /// nothing.
    #[test]
    fn a_node_reading_writes_the_same_rows_as_the_v1_path_and_dedups_by_message_id() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let reading = node_reading("dedup-1", "2026-01-13T14:30:00Z", 21.5);

        persist_node_reading(&storage, &mut cache, "hub1", "aa11", &reading)
            .expect("first uplink persists");

        let rows = storage
            .query_field_readings("hub1", "aa11", "temperature", 0, i64::MAX)
            .expect("query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ts, 1_768_314_600, "received_at is the row ts");
        assert_eq!(rows[0].value, Some(21.5));
        assert_eq!(rows[0].source, SOURCE_LIVE);
        assert_eq!(
            rows[0].alarm_state, None,
            "the v2 wire carries no field alarm state"
        );

        let counters = storage
            .query_counters("hub1", "aa11", "hall_left_count", 0, i64::MAX)
            .expect("query");
        assert_eq!(counters.len(), 1);
        assert_eq!(counters[0].value, Some(7));

        let raw = storage
            .list_raw_messages("hub1", "aa11", None)
            .expect("raw");
        assert_eq!(raw.len(), 1);
        assert!(raw[0].payload.contains("dedup-1"));

        // Exactly the rows the v1 path produces for the same measurement — same table, same
        // key, same shape. Proven by writing one through each and comparing.
        let mut v1_cache = HashMap::new();
        persist_lorawan_sensors(
            &storage,
            "hub1",
            LorawanSensorsMsg {
                timestamp: None,
                sensors: vec![lorawan_sensor("bb22", "2026-01-13T14:30:00Z", 21.5)],
            },
            &mut v1_cache,
        )
        .expect("v1 uplink persists");
        let v1 = storage
            .query_field_readings("hub1", "bb22", "temperature", 0, i64::MAX)
            .expect("query");
        assert_eq!(v1.len(), 1);
        assert_eq!((v1[0].ts, v1[0].value), (rows[0].ts, rows[0].value));
        assert_eq!(v1[0].source, rows[0].source);

        // The redelivery: same message_id, and — to prove the gate is the message_id and not
        // the row-level INSERT OR IGNORE — a different value at the same timestamp.
        let mut again = reading.clone();
        again.fields.insert("temperature".to_string(), 99.9);
        persist_node_reading(&storage, &mut cache, "hub1", "aa11", &again)
            .expect("a duplicate is not an error");
        let rows = storage
            .query_field_readings("hub1", "aa11", "temperature", 0, i64::MAX)
            .expect("query");
        assert_eq!(rows.len(), 1, "one row per message_id");
        assert_eq!(rows[0].value, Some(21.5), "and the first one won");
        assert_eq!(
            storage
                .list_raw_messages("hub1", "aa11", None)
                .expect("raw")
                .len(),
            1,
            "node_raw_messages is a plain INSERT — the gate is what keeps it single"
        );

        // A genuinely new message on the same node still lands.
        let next = node_reading("dedup-2", "2026-01-13T14:45:00Z", 22.0);
        persist_node_reading(&storage, &mut cache, "hub1", "aa11", &next).expect("new message");
        assert_eq!(
            storage
                .query_field_readings("hub1", "aa11", "temperature", 0, i64::MAX)
                .expect("query")
                .len(),
            2
        );
    }

    /// F4 (bench, 2026-09-23): a v2 reading arrives under control-radio's lowercase hostname,
    /// while the store — and so every page that queries storage — holds the agent's spelling.
    /// Persisted under the canonical spelling, the Node's rows are where the pages look.
    #[test]
    fn a_v2_reading_is_stored_under_the_hubs_canonical_spelling() {
        let (_dir, storage) = temp_storage();
        let (ntx, _nrx, gtx, _grx) = channels();
        let mut cache = PersistCache {
            hostnames: Some(Arc::new(|h: &str| {
                (h.eq_ignore_ascii_case("fiber-4e970f4a")).then(|| "FIBER-4E970F4A".to_string())
            })),
            ..PersistCache::default()
        };

        handle(
            &storage,
            IngestEvent::NodeReading {
                hostname: "fiber-4e970f4a".into(),
                node_key: "5876070000000413".into(),
                payload: Box::new(node_reading("canon-1", "2026-09-23T21:53:04Z", 32.47)),
            },
            &mut cache,
            &ntx,
            &gtx,
        )
        .expect("persists");

        let rows = storage
            .query_field_readings(
                "FIBER-4E970F4A",
                "5876070000000413",
                "temperature",
                0,
                i64::MAX,
            )
            .expect("query");
        assert_eq!(rows.len(), 1, "the page's spelling finds the reading");
        assert_eq!(
            storage
                .list_raw_messages("FIBER-4E970F4A", "5876070000000413", None)
                .expect("raw")
                .len(),
            1,
            "and the message ring"
        );
        assert!(
            storage
                .list_raw_messages("fiber-4e970f4a", "5876070000000413", None)
                .expect("raw")
                .is_empty(),
            "nothing is left under the topic's spelling"
        );

        // A Hub the store does not know keeps the spelling it came with.
        handle(
            &storage,
            IngestEvent::NodeReading {
                hostname: "other-hub".into(),
                node_key: "aa11".into(),
                payload: Box::new(node_reading("canon-2", "2026-09-23T21:54:04Z", 20.0)),
            },
            &mut cache,
            &ntx,
            &gtx,
        )
        .expect("persists");
        assert_eq!(
            storage
                .list_raw_messages("other-hub", "aa11", None)
                .expect("raw")
                .len(),
            1
        );
    }

    /// p2p_history_replay.md §14: a v2 replay frame arrives under control-radio's lowercase
    /// hostname, while the backfill job was stored under the Hub's canonical spelling. The rows
    /// land where the chart looks, as `replay`, and the frame credits and closes that job.
    #[test]
    fn a_v2_history_frame_credits_the_job_under_the_canonical_spelling() {
        let (_dir, storage) = temp_storage();
        let (ntx, _nrx, gtx, _grx) = channels();
        let mut cache = PersistCache {
            hostnames: Some(Arc::new(|h: &str| {
                (h.eq_ignore_ascii_case("fiber-4e970f4a")).then(|| "FIBER-4E970F4A".to_string())
            })),
            ..PersistCache::default()
        };
        let node = "5876070000000413";
        let job = storage
            .create_job(&NewBackfillJob {
                hostname: "FIBER-4E970F4A".to_string(),
                node_key: node.to_string(),
                time_from: 1_790_319_900,
                time_to: 1_790_321_040,
                trigger: "manual".to_string(),
                samples_expected: None,
            })
            .expect("create job");
        storage
            .update_job_status(job.job_id, JOB_REQUESTED, None, false)
            .expect("mark requested");

        let frame = |index: i64, t0: i64| NodeHistoryMsg {
            dev_eui: Some(node.to_string()),
            frame_index: Some(index),
            frame_count: Some(2),
            interval_s: None,
            records: vec![
                history_record(Some(t0), Some(27.07)),
                history_record(Some(t0 + 60), Some(27.04)),
            ],
        };
        for (index, t0) in [(0, 1_790_319_991), (1, 1_790_320_111)] {
            handle(
                &storage,
                IngestEvent::NodeHistory {
                    hostname: "fiber-4e970f4a".into(),
                    node_key: node.into(),
                    payload: frame(index, t0),
                },
                &mut cache,
                &ntx,
                &gtx,
            )
            .expect("persists");
            let status = storage
                .get_job(job.job_id)
                .expect("get")
                .expect("job")
                .status;
            if index == 0 {
                assert_eq!(status, JOB_RECEIVING, "the first of two frames is progress");
            } else {
                assert_eq!(status, JOB_DONE, "the last frame closes the job");
            }
        }

        let rows = storage
            .query_field_readings("FIBER-4E970F4A", node, "temperature", 0, i64::MAX)
            .expect("query");
        assert_eq!(rows.len(), 4, "every record is where the chart looks");
        assert!(rows.iter().all(|r| r.source == SOURCE_REPLAY));
        assert!(storage
            .query_field_readings("fiber-4e970f4a", node, "temperature", 0, i64::MAX)
            .expect("query")
            .is_empty());
    }

    /// devices.node.md §3.9, amendment 2026-09-25, item 5: a span request brings back the
    /// minutes the live readings already cover; a record within half an interval of a stored
    /// reading of the same field is left out, the ones in the hole are stored.
    #[test]
    fn a_replayed_record_a_live_reading_covers_is_not_stored_twice() {
        let (_dir, storage) = temp_storage();
        let (ntx, _nrx, gtx, _grx) = channels();
        let mut cache = PersistCache::default();
        let (host, node, t) = ("hub1", "aa11", 1_790_320_231i64);
        storage
            .insert_field_readings(&[
                NodeFieldReading {
                    hostname: host.into(),
                    node_key: node.into(),
                    field: "temperature".into(),
                    ts: t + 12,
                    value: Some(26.9),
                    alarm_state: None,
                    source: SOURCE_LIVE.into(),
                },
                NodeFieldReading {
                    hostname: host.into(),
                    node_key: node.into(),
                    field: "temperature".into(),
                    ts: t + 130,
                    value: Some(26.8),
                    alarm_state: None,
                    source: SOURCE_LIVE.into(),
                },
            ])
            .expect("live readings");
        handle(
            &storage,
            IngestEvent::NodeHistory {
                hostname: host.into(),
                node_key: node.into(),
                payload: NodeHistoryMsg {
                    dev_eui: Some(node.into()),
                    frame_index: Some(0),
                    frame_count: Some(1),
                    interval_s: Some(60),
                    records: [t, t + 60, t + 120]
                        .into_iter()
                        .map(|ts| NodeHistoryRecord {
                            time: Some(ts),
                            fields: HashMap::from([("temperature".to_string(), Some(26.9))]),
                            counters: HashMap::new(),
                        })
                        .collect(),
                },
            },
            &mut cache,
            &ntx,
            &gtx,
        )
        .expect("persists");
        let rows = storage
            .query_field_readings(host, node, "temperature", 0, i64::MAX)
            .expect("query");
        let replayed: Vec<i64> = rows
            .iter()
            .filter(|r| r.source == SOURCE_REPLAY)
            .map(|r| r.ts)
            .collect();
        assert_eq!(
            replayed,
            vec![t + 60],
            "only the minute the live readings missed"
        );
    }

    /// G7: every fPort-85 frame joins the message ring as port 85, under the canonical Hub.
    #[test]
    fn a_command_frame_joins_the_message_ring_as_port_85() {
        let (_dir, storage) = temp_storage();
        let (ntx, _nrx, gtx, _grx) = channels();
        let mut cache = PersistCache {
            hostnames: Some(Arc::new(|h: &str| {
                (h.eq_ignore_ascii_case("fiber-4e970f4a")).then(|| "FIBER-4E970F4A".to_string())
            })),
            ..PersistCache::default()
        };
        handle(
            &storage,
            IngestEvent::NodeCommand {
                hostname: "fiber-4e970f4a".into(),
                node_key: "5876070000000413".into(),
                payload: serde_json::json!({
                    "dev_eui": "5876070000000413",
                    "received_at": "2026-09-23T22:06:59.557Z",
                    "kind": "ack", "seq": 22,
                }),
            },
            &mut cache,
            &ntx,
            &gtx,
        )
        .expect("persists");
        let rows = storage
            .list_raw_messages("FIBER-4E970F4A", "5876070000000413", Some(50))
            .expect("raw");
        assert_eq!(rows.len(), 1);
        let doc: serde_json::Value = serde_json::from_str(&rows[0].payload).expect("json");
        assert_eq!(doc["fport"], 85);
        assert_eq!(doc["kind"], "ack");
        assert_eq!(rows[0].received_at, 1_790_201_219, "the frame's own time");
    }

    /// Q-N1: the `superseded` document for a frame a newer ask replaced is the Hub's, not a
    /// frame the Node sent, so it stays out of the port-85 ring.
    #[test]
    fn a_superseded_command_document_is_not_a_frame() {
        let (_dir, storage) = temp_storage();
        let (ntx, _nrx, gtx, _grx) = channels();
        let mut cache = PersistCache::default();
        handle(
            &storage,
            IngestEvent::NodeCommand {
                hostname: "fiber-4e970f4a".into(),
                node_key: "5876070000000413".into(),
                payload: control_radio::coalesce::superseded_document(
                    "5876070000000413",
                    None,
                    7,
                    9,
                    "2026-09-28T08:00:00Z",
                ),
            },
            &mut cache,
            &ntx,
            &gtx,
        )
        .expect("handled");
        let rows = storage
            .list_raw_messages("fiber-4e970f4a", "5876070000000413", Some(50))
            .expect("raw");
        assert!(rows.is_empty(), "{rows:?}");
    }

    /// §10.5 (amended 2026-10-07): nor is the Hub's `undelivered` note for a downlink it
    /// gave up.
    #[test]
    fn an_undelivered_command_document_is_not_a_frame() {
        let (_dir, storage) = temp_storage();
        let (ntx, _nrx, gtx, _grx) = channels();
        let mut cache = PersistCache::default();
        handle(
            &storage,
            IngestEvent::NodeCommand {
                hostname: "fiber-4e970f4a".into(),
                node_key: "5876070000005722".into(),
                payload: serde_json::json!({
                    "dev_eui": "5876070000005722", "seq": 31, "kind": "undelivered",
                    "status": "undelivered", "attempts": 5, "last_outcome": "NotDelivered",
                    "at": "2026-10-07T11:00:00Z",
                }),
            },
            &mut cache,
            &ntx,
            &gtx,
        )
        .expect("handled");
        let rows = storage
            .list_raw_messages("fiber-4e970f4a", "5876070000005722", Some(50))
            .expect("raw");
        assert!(rows.is_empty(), "{rows:?}");
    }

    #[test]
    fn node_reading_events_become_node_event_rows() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let mut reading = node_reading("ev-1", "2026-01-13T14:30:00Z", 21.5);
        reading
            .events
            .push(control_radio::decode::reading::NodeEvent::new(
                "no_data",
                "2026-01-13T14:30:00Z",
                serde_json::Map::from_iter([(
                    "field".to_string(),
                    serde_json::Value::from("humidity"),
                )]),
            ));
        // An event with an unparseable ts is skipped, not fatal — same rule as the v1 sink.
        reading
            .events
            .push(control_radio::decode::reading::NodeEvent::new(
                "decode_warning",
                "not a timestamp",
                serde_json::Map::new(),
            ));

        persist_node_reading(&storage, &mut cache, "hub1", "aa11", &reading).expect("persists");
        let events = storage
            .query_events("hub1", "aa11", None, 0, i64::MAX, None)
            .expect("query");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "no_data");
    }

    /// One fPort-3 page as the v2 decoder publishes it (control-radio `decode::reading`): the
    /// `alarm_batch` header, then one `alarm` per slot, every event at the message's `ts`.
    /// `page` is `(page_index, page_count)` for a paged batch.
    fn alarm_page(id: &str, at: &str, page: Option<(u32, u32)>, slots: &[i64]) -> NodeReadingMsg {
        use control_radio::decode::reading::NodeEvent as Ev;
        let mut doc = node_reading(id, at, 0.0);
        doc.fields.clear();
        doc.counters.clear();
        doc.fport = Some(3);
        let mut batch = serde_json::Map::new();
        batch.insert("total".into(), serde_json::json!(slots.len()));
        if let Some((index, count)) = page {
            batch.insert("page_index".into(), serde_json::json!(index));
            batch.insert("page_count".into(), serde_json::json!(count));
        }
        doc.events.push(Ev::new("alarm_batch", at, batch));
        for slot in slots {
            doc.events.push(Ev::new(
                "alarm",
                at,
                serde_json::Map::from_iter([
                    ("slot".to_string(), serde_json::json!("mb")),
                    ("rule".to_string(), serde_json::json!(slot)),
                    ("channel".to_string(), serde_json::json!("temperature")),
                    ("event".to_string(), serde_json::json!("activate")),
                    ("side".to_string(), serde_json::json!("low")),
                    ("value".to_string(), serde_json::json!(28.2)),
                ]),
            ));
        }
        doc
    }

    /// `(event_type, seq)` of every stored event of `aa11`, newest first.
    fn stored_events(storage: &Storage) -> Vec<(String, i64)> {
        storage
            .query_events("hub1", "aa11", None, 0, i64::MAX, Some(1000))
            .expect("query")
            .into_iter()
            .map(|e| (e.event_type, e.seq))
            .collect()
    }

    /// devices.backend.md §23 (V27, bench E6-1): a page of six events is six rows. The key
    /// used to be `(event_type, ts)`, and every event of a message carries its `ts`, so only
    /// the header and the first alarm were kept.
    #[test]
    fn every_event_of_a_six_event_page_is_a_row() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let page = alarm_page("m-1", "2026-09-28T01:38:23Z", None, &[0, 1, 2, 3, 4]);
        assert_eq!(page.events.len(), 6);
        persist_node_reading(&storage, &mut cache, "hub1", "aa11", &page).expect("persists");

        let rows = stored_events(&storage);
        assert_eq!(rows.len(), 6, "{rows:?}");
        assert_eq!(
            rows.iter().map(|(_, seq)| *seq).collect::<Vec<_>>(),
            vec![5, 4, 3, 2, 1, 0],
            "the message's order, the last event newest"
        );
        let slots: Vec<i64> = storage
            .query_events("hub1", "aa11", Some("alarm"), 0, i64::MAX, None)
            .expect("query")
            .iter()
            .filter_map(|e| {
                let v: serde_json::Value = serde_json::from_str(e.payload.as_deref()?).ok()?;
                v["rule"].as_i64()
            })
            .collect();
        assert_eq!(slots, vec![4, 3, 2, 1, 0], "every rule's alarm");
    }

    /// The pages of one batch can land in the same second (0413's c1290 and c1291). The page
    /// index keeps them apart: page 1's header is not page 0's.
    #[test]
    fn two_pages_in_the_same_second_are_both_stored() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let at = "2026-09-28T01:38:52Z";
        for page in [
            alarm_page("m-1", at, Some((0, 2)), &[0, 1, 2, 3, 4]),
            alarm_page("m-2", at, Some((1, 2)), &[5]),
        ] {
            persist_node_reading(&storage, &mut cache, "hub1", "aa11", &page).expect("persists");
        }
        let rows = stored_events(&storage);
        assert_eq!(rows.len(), 8, "{rows:?}");
        assert_eq!(
            rows[..2],
            [("alarm".to_string(), 257), ("alarm_batch".to_string(), 256)],
            "page 1's events, its header included, and newest"
        );
        assert_eq!(
            rows.iter().filter(|(t, _)| t == "alarm").count(),
            6,
            "all six slots"
        );
    }

    /// A redelivered message numbers its events the same, so it adds no row — through the
    /// `message_id` gate, and through the key after a restart has emptied the gate.
    #[test]
    fn a_redelivered_page_adds_no_row() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let page = alarm_page(
            "m-1",
            "2026-09-28T01:38:23Z",
            Some((0, 2)),
            &[0, 1, 2, 3, 4],
        );
        persist_node_reading(&storage, &mut cache, "hub1", "aa11", &page).expect("persists");
        persist_node_reading(&storage, &mut cache, "hub1", "aa11", &page).expect("redelivery");
        let mut restarted = PersistCache::default();
        persist_node_reading(&storage, &mut restarted, "hub1", "aa11", &page)
            .expect("redelivery after a restart");
        assert_eq!(stored_events(&storage).len(), 6);
    }

    #[test]
    fn event_seqs_number_the_message_by_page() {
        let unpaged = alarm_page("m-1", "2026-09-28T01:38:23Z", None, &[0, 1]);
        assert_eq!(event_seqs(&unpaged.events).collect::<Vec<_>>(), [0, 1, 2]);
        let second = alarm_page("m-2", "2026-09-28T01:38:23Z", Some((1, 3)), &[5, 6]);
        assert_eq!(
            event_seqs(&second.events).collect::<Vec<_>>(),
            [256, 257, 258]
        );
        let telemetry = node_reading("m-3", "2026-09-28T01:38:23Z", 21.5);
        assert_eq!(event_seqs(&telemetry.events).count(), 0);
    }

    /// One native fPort-3 alarm out of the `alarm` document, through the same engine the v1
    /// path uses: it opens a row, and the matching `deactivate` clears it.
    #[test]
    fn a_node_alarm_opens_and_clears_through_the_alarm_engine() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, mut nrx, gtx, mut grx) = channels();

        let alarm_event = |edge: &str, ts: &str| {
            control_radio::decode::reading::NodeEvent::new(
                "alarm",
                ts,
                serde_json::Map::from_iter([
                    ("slot".to_string(), serde_json::Value::from("mb")),
                    ("rule".to_string(), serde_json::Value::from(0)),
                    (
                        "channel".to_string(),
                        serde_json::Value::from("temperature"),
                    ),
                    ("event".to_string(), serde_json::Value::from(edge)),
                    ("side".to_string(), serde_json::Value::from("high")),
                    ("value".to_string(), serde_json::Value::from(31.5)),
                    (
                        "time".to_string(),
                        serde_json::Value::from(1_787_824_010u32),
                    ),
                ]),
            )
        };

        let mut raise = node_reading("alarm-1", "2026-01-13T14:30:00Z", 31.5);
        raise.fields.clear();
        raise.counters.clear();
        raise
            .events
            .push(alarm_event("activate", "2026-01-13T14:30:00Z"));
        run_node_alarm_engine(&storage, &cache, "hub1", "aa11", &raise, &ntx, &gtx)
            .expect("alarm engine");
        assert_eq!(storage.count_active(Some("hub1")).expect("count"), 1);
        assert!(matches!(nrx.try_recv(), Ok(NotifyJob::Activated(_))));
        assert!(grx.try_recv().is_ok(), "nudge on open");

        // Re-running the engine on a redelivered raise is a no-op — the row is already active.
        run_node_alarm_engine(&storage, &cache, "hub1", "aa11", &raise, &ntx, &gtx)
            .expect("alarm engine");
        assert_eq!(storage.count_active(Some("hub1")).expect("count"), 1);

        let mut clear = node_reading("alarm-2", "2026-01-13T14:45:00Z", 20.0);
        clear.fields.clear();
        clear.counters.clear();
        clear
            .events
            .push(alarm_event("deactivate", "2026-01-13T14:45:00Z"));
        run_node_alarm_engine(&storage, &cache, "hub1", "aa11", &clear, &ntx, &gtx)
            .expect("alarm engine");
        assert_eq!(
            storage.count_active(Some("hub1")).expect("count"),
            0,
            "the device's own recovery edge clears the row"
        );

        // And the document's rows are still persisted alongside the alarm handling.
        persist_node_reading(&storage, &mut cache, "hub1", "aa11", &raise).expect("persists");
        let events = storage
            .query_events("hub1", "aa11", None, 0, i64::MAX, None)
            .expect("query");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "alarm");
    }

    /// The v2 vocabulary is the firmware's own edge (`event: activate|deactivate`) with the
    /// side under `side`, not v1's (`edge: raise|clear`). Decoding one with the other's keys
    /// silently drops every alarm, which is the failure this pins.
    #[test]
    fn the_v2_native_event_vocabulary_is_the_firmwares_own() {
        let ev = control_radio::decode::reading::NodeEvent::new(
            "alarm",
            "2026-01-13T14:30:00Z",
            serde_json::Map::from_iter([
                ("slot".to_string(), serde_json::Value::from("mb")),
                ("rule".to_string(), serde_json::Value::from(2)),
                ("event".to_string(), serde_json::Value::from("activate")),
                ("side".to_string(), serde_json::Value::from("low")),
                ("channel".to_string(), serde_json::Value::from("humidity")),
                ("value".to_string(), serde_json::Value::from(18.0)),
                (
                    "time".to_string(),
                    serde_json::Value::from(1_787_824_020u32),
                ),
            ]),
        );
        let native = decode_v2_native_event("hub1", "aa11", &ev).expect("decodes");
        assert_eq!(native.slot, 2);
        assert_eq!(native.edge, Edge::Raise);
        assert_eq!(native.side.as_deref(), Some("low"));
        assert_eq!(native.quantity.as_deref(), Some("humidity"));
        assert_eq!(native.value, Some(18.0));
        assert_eq!(native.base_time, 1_787_824_020);

        // v1's spelling decodes to nothing rather than to a wrong edge.
        let v1_shaped = control_radio::decode::reading::NodeEvent::new(
            "alarm",
            "2026-01-13T14:30:00Z",
            serde_json::Map::from_iter([
                ("slot".to_string(), serde_json::Value::from(0)),
                ("edge".to_string(), serde_json::Value::from("raise")),
            ]),
        );
        assert!(decode_v2_native_event("hub1", "aa11", &v1_shaped).is_none());
    }

    /// The alarm document as it is published, parsed the way the MQTT router parses it.
    /// Before the Nodes test 2026-09-23 the side won the `type` key on the wire, and every
    /// native alarm was dropped before the engine saw it. Both shapes must raise: the current
    /// one, and the one a Hub built before the fix still publishes.
    #[test]
    fn a_published_alarm_document_raises_in_either_shape() {
        let doc = |event: &str| {
            format!(
                r#"{{"counters":{{"fCnt":3}},"dev_eui":"5876070000000413","dr":5,
                   "events":[{{"base_time":1790204818,"time_synced":true,"total":1,
                               "truncated":false,"ts":"2026-09-23T23:07:11.450752Z",
                               "type":"alarm_batch"}},{event}],
                   "fields":{{}},"fport":3,"gateways":[],"message_id":"m-{len}",
                   "received_at":"2026-09-23T23:07:11.450752Z","serial":"33333"}}"#,
                len = event.len()
            )
        };
        let current = r#"{"event":"activate","channel":"temperature","slot":"mb","rule":0,
                          "time":1790204818,"ts":"2026-09-23T23:07:11.450752Z","type":"alarm",
                          "side":"high","value":27.79}"#;
        let legacy = r#"{"event":"activate","channel":"temperature","slot":"mb","rule":0,
                         "time":1790204818,"ts":"2026-09-23T23:07:11.450752Z","type":"high",
                         "value":27.79}"#;

        for (shape, event) in [("current", current), ("legacy", legacy)] {
            let (_dir, storage) = temp_storage();
            let cache = PersistCache::default();
            let (ntx, mut nrx, gtx, _grx) = channels();
            let reading: NodeReadingMsg =
                serde_json::from_str(&doc(event)).expect("the published shape parses");
            run_node_alarm_engine(&storage, &cache, "hub1", "aa11", &reading, &ntx, &gtx)
                .expect("alarm engine");
            assert_eq!(
                storage.count_active(Some("hub1")).expect("count"),
                1,
                "{shape}: the alarm is raised"
            );
            assert!(
                matches!(nrx.try_recv(), Ok(NotifyJob::Activated(_))),
                "{shape}: and notified"
            );
            let native = reading
                .events
                .iter()
                .find_map(|e| decode_v2_native_event("hub1", "aa11", e))
                .expect("decodes");
            assert_eq!(native.side.as_deref(), Some("high"), "{shape}");
        }
    }

    /// PN-9 (night test 2026-09-26): the removal tombstone closes the removed Node's open
    /// alarms — another Node's stay, and a non-empty `info` closes nothing.
    #[test]
    fn a_removed_node_s_open_alarms_are_closed_by_its_tombstone() {
        let doc = |dev_eui: &str| {
            format!(
                r#"{{"counters":{{}},"dev_eui":"{dev_eui}","events":[
                {{"type":"alarm_batch","ts":"2026-09-26T21:00:00Z","total":1}},
                {{"type":"alarm","ts":"2026-09-26T21:00:00Z","event":"activate","side":"high",
                 "channel":"temperature","slot":"mb","rule":0,"value":31.0,"time":1790456400}}],
                "fields":{{}},"fport":3,"gateways":[],"message_id":"m-{dev_eui}",
                "received_at":"2026-09-26T21:00:00Z"}}"#
            )
        };
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, _nrx, gtx, mut grx) = channels();
        for node in ["aa11", "bb22"] {
            let raise: NodeReadingMsg = serde_json::from_str(&doc(node)).expect("parses");
            run_node_alarm_engine(&storage, &cache, "hub1", node, &raise, &ntx, &gtx)
                .expect("alarm engine");
        }
        // PF-3: the removed Node's "configuration not delivered" warning goes too.
        storage
            .insert_alarm_event(&portal_storage::NewAlarmEvent {
                hostname: "hub1".into(),
                sensor_id: crate::pending::config_warning_sensor_id("aa11"),
                sensor_name: Some("aa11".into()),
                sensor_location: None,
                alarm_type: "CONFIG_NOT_DELIVERED".into(),
                severity: "warning".into(),
                from_state: Some("NORMAL".into()),
                to_state: Some("WARNING".into()),
                message: None,
                value: None,
                threshold: None,
                triggered_at: "2026-09-26T21:00:00Z".into(),
                cleared_at: None,
                acknowledged_at: None,
                acknowledged_by_user_id: None,
                acknowledged_by_username: None,
                native_key: None,
            })
            .expect("warning");
        assert_eq!(storage.count_active(Some("hub1")).expect("count"), 3);
        while grx.try_recv().is_ok() {}

        let inventory = |payload: serde_json::Value| IngestEvent::NodeInventory {
            hostname: "hub1".into(),
            node_key: "aa11".into(),
            payload,
        };
        handle(
            &storage,
            inventory(serde_json::json!({"dev_eui": "aa11", "transport": "p2p"})),
            &mut cache,
            &ntx,
            &gtx,
        )
        .expect("an info document");
        assert_eq!(
            storage.count_active(Some("hub1")).expect("count"),
            3,
            "not a removal"
        );

        handle(
            &storage,
            inventory(serde_json::Value::Null),
            &mut cache,
            &ntx,
            &gtx,
        )
        .expect("the tombstone");
        let open: Vec<String> = storage
            .active_alarms("hub1")
            .expect("list")
            .into_iter()
            .map(|a| a.sensor_id)
            .collect();
        assert_eq!(open.len(), 1, "{open:?}");
        assert!(
            open[0].starts_with("lrw:bb22:"),
            "the other Node's alarm stays: {open:?}"
        );
        assert!(grx.try_recv().is_ok(), "the alarm table is re-rendered");
    }

    /// F24: a reboot clears the Node's latches without a `deactivate`; the next Info's empty
    /// `active_alarms` closes what is still open. An older (replayed) Info and a non-empty list
    /// close nothing.
    #[test]
    fn an_info_with_no_latched_alarm_closes_the_ones_a_reboot_forgot() {
        let raise_doc = r#"{"counters":{},"dev_eui":"aa11","events":[
            {"type":"alarm_batch","ts":"2026-09-23T23:38:05Z","total":1},
            {"type":"alarm","ts":"2026-09-23T23:38:05Z","event":"activate","side":"high",
             "channel":"temperature","slot":"mb","rule":0,"value":27.74,"time":1790206671}],
            "fields":{},"fport":3,"gateways":[],"message_id":"m-raise",
            "received_at":"2026-09-23T23:38:05Z"}"#;
        let info = |received_at: &str, active: serde_json::Value| {
            crate::state::NodeDeviceInfoMsg::new(
                "aa11",
                received_at,
                serde_json::Map::from_iter([("active_alarms".to_string(), active)]),
            )
        };

        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, mut nrx, gtx, _grx) = channels();
        let raise: NodeReadingMsg = serde_json::from_str(raise_doc).expect("parses");
        run_node_alarm_engine(&storage, &cache, "hub1", "aa11", &raise, &ntx, &gtx)
            .expect("alarm engine");
        assert!(matches!(nrx.try_recv(), Ok(NotifyJob::Activated(_))));
        assert_eq!(storage.count_active(Some("hub1")).expect("count"), 1);

        let still_latched = info(
            "2026-09-23T23:40:27Z",
            serde_json::json!([{"slot": 0, "channel": 0, "type": 2}]),
        );
        close_forgotten_native_alarms(
            &storage,
            &mut cache,
            "hub1",
            "aa11",
            &still_latched,
            LATER,
            &ntx,
            &gtx,
        )
        .expect("reconcile");
        assert_eq!(
            storage.count_active(Some("hub1")).expect("count"),
            1,
            "a non-empty list names no slot"
        );

        let replayed_older = info("2026-09-23T23:30:00Z", serde_json::json!([]));
        close_forgotten_native_alarms(
            &storage,
            &mut cache,
            "hub1",
            "aa11",
            &replayed_older,
            LATER,
            &ntx,
            &gtx,
        )
        .expect("reconcile");
        assert_eq!(
            storage.count_active(Some("hub1")).expect("count"),
            1,
            "an Info older than the alarm does not close it"
        );

        let after_reboot = info("2026-09-23T23:40:27Z", serde_json::json!([]));
        close_forgotten_native_alarms(
            &storage,
            &mut cache,
            "hub1",
            "aa11",
            &after_reboot,
            LATER,
            &ntx,
            &gtx,
        )
        .expect("reconcile");
        assert_eq!(
            storage.count_active(Some("hub1")).expect("count"),
            0,
            "the empty list closes it"
        );
        assert!(
            matches!(nrx.try_recv(), Ok(NotifyJob::Cleared { .. })),
            "through the recovery path, notification included"
        );

        // The retained replay can arrive under the Hub's lowercase spelling before the portal
        // knows the display one; the row written under the display spelling still closes.
        run_node_alarm_engine(
            &storage,
            &cache,
            "HUB1",
            "aa11",
            &serde_json::from_str(
                &raise_doc
                    .replace("m-raise", "m-upper")
                    .replace("1790206671", "1790206700"),
            )
            .expect("parses"),
            &ntx,
            &gtx,
        )
        .expect("alarm engine");
        assert_eq!(storage.count_active(Some("HUB1")).expect("count"), 1);
        close_forgotten_native_alarms(
            &storage,
            &mut cache,
            "hub1",
            "aa11",
            &after_reboot,
            LATER,
            &ntx,
            &gtx,
        )
        .expect("reconcile");
        assert_eq!(
            storage.count_active(Some("HUB1")).expect("count"),
            0,
            "closed whatever the spelling"
        );

        // Another Node's alarm is not this Info's business.
        let other: NodeReadingMsg =
            serde_json::from_str(&raise_doc.replace("m-raise", "m-other")).expect("parses");
        run_node_alarm_engine(&storage, &cache, "hub1", "bb22", &other, &ntx, &gtx)
            .expect("alarm engine");
        close_forgotten_native_alarms(
            &storage,
            &mut cache,
            "hub1",
            "aa11",
            &after_reboot,
            LATER,
            &ntx,
            &gtx,
        )
        .expect("reconcile");
        assert_eq!(storage.count_active(Some("hub1")).expect("count"), 1);
    }

    /// nodes_phase2.cs.md P3: a Node restart deactivates all its native alarms. The Info dates
    /// the boot (`received_at − uptime_s`); an alarm raised before it closes even when the list
    /// is not empty, unless the Node re-raised that slot since the boot.
    #[test]
    fn a_node_restart_closes_its_native_alarms_unless_re_raised() {
        let raise = |at: &str, time: i64, id: &str| -> NodeReadingMsg {
            serde_json::from_str(&format!(
                r#"{{"counters":{{}},"dev_eui":"aa11","events":[
                {{"type":"alarm","ts":"{at}","event":"activate","side":"high",
                 "channel":"temperature","slot":"mb","rule":0,"value":32.4,"time":{time}}}],
                "fields":{{}},"fport":3,"gateways":[],"message_id":"{id}","received_at":"{at}"}}"#
            ))
            .expect("parses")
        };
        let info = |received_at: &str, uptime: i64| {
            crate::state::NodeDeviceInfoMsg::new(
                "aa11",
                received_at,
                serde_json::Map::from_iter([
                    ("uptime_s".to_string(), serde_json::json!(uptime)),
                    (
                        "active_alarms".to_string(),
                        serde_json::json!([{"slot": 0, "channel": 0, "type": 2}]),
                    ),
                ]),
            )
        };
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, _nrx, gtx, _grx) = channels();
        let active = |storage: &Storage| storage.count_active(Some("hub1")).expect("count");
        // What `handle` does with an alarm document: the engine, then the event rows the
        // re-raise check reads.
        let alarm = |storage: &Storage, cache: &mut PersistCache, doc: &NodeReadingMsg| {
            run_node_alarm_engine(storage, cache, "hub1", "aa11", doc, &ntx, &gtx).expect("engine");
            persist_node_reading(storage, cache, "hub1", "aa11", doc).expect("persist");
        };

        alarm(
            &storage,
            &mut cache,
            &raise("2026-09-24T16:34:05Z", 1_790_267_645, "m-1"),
        );
        assert_eq!(active(&storage), 1);

        // Rebooted at 16:49:20 and not re-raised yet: the old alarm goes.
        close_forgotten_native_alarms(
            &storage,
            &mut cache,
            "hub1",
            "aa11",
            &info("2026-09-24T16:49:40Z", 20),
            LATER,
            &ntx,
            &gtx,
        )
        .expect("reconcile");
        assert_eq!(active(&storage), 0, "the restart deactivated it");

        // Raised again, then rebooted, and the re-raise after the boot beats the Info in: the
        // engine kept that row, and the Info must not close a live alarm.
        alarm(
            &storage,
            &mut cache,
            &raise("2026-09-24T16:50:00Z", 1_790_268_600, "m-2"),
        );
        alarm(
            &storage,
            &mut cache,
            &raise("2026-09-24T17:05:30Z", 1_790_269_530, "m-3"),
        );
        close_forgotten_native_alarms(
            &storage,
            &mut cache,
            "hub1",
            "aa11",
            &info("2026-09-24T17:05:40Z", 30),
            LATER,
            &ntx,
            &gtx,
        )
        .expect("reconcile");
        assert_eq!(active(&storage), 1, "re-raised since the boot: kept");
    }

    /// Bench 2026-09-28 (E6, 0413 on FW t2d): six rules raised on two pages, the Node rebooted
    /// and raised all six again on two pages. Only the first slot of each page stayed open:
    /// `node_events` keyed a row by `(event_type, ts)`, so it kept the first event of a page, and
    /// the restart rule closed slots 1–4 as never re-raised. V27's `seq` keeps every row, and
    /// the activation cache stays a second guard.
    #[test]
    fn a_restart_keeps_every_slot_a_multi_event_page_re_raised() {
        let page = |at: &str, time: i64, id: &str, slots: &[i64]| -> NodeReadingMsg {
            let events: Vec<String> = slots
                .iter()
                .map(|slot| {
                    format!(
                        r#"{{"type":"alarm","ts":"{at}","event":"activate","side":"low",
                        "channel":"temperature","slot":"mb","rule":{slot},"value":28.2,"time":{time}}}"#
                    )
                })
                .collect();
            serde_json::from_str(&format!(
                r#"{{"counters":{{}},"dev_eui":"aa11","events":[{}],
                "fields":{{}},"fport":3,"gateways":[],"message_id":"{id}","received_at":"{at}"}}"#,
                events.join(",")
            ))
            .expect("parses")
        };
        let boot_info = |received_at: &str, uptime: i64| {
            crate::state::NodeDeviceInfoMsg::new(
                "aa11",
                received_at,
                serde_json::Map::from_iter([
                    ("uptime_s".to_string(), serde_json::json!(uptime)),
                    ("active_alarms".to_string(), serde_json::json!([])),
                ]),
            )
        };
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, _nrx, gtx, _grx) = channels();
        let active = |storage: &Storage| storage.count_active(Some("hub1")).expect("count");
        let alarm = |storage: &Storage, cache: &mut PersistCache, doc: &NodeReadingMsg| {
            run_node_alarm_engine(storage, cache, "hub1", "aa11", doc, &ntx, &gtx).expect("engine");
            persist_node_reading(storage, cache, "hub1", "aa11", doc).expect("persist");
        };

        alarm(
            &storage,
            &mut cache,
            &page(
                "2026-09-28T01:38:23Z",
                1_790_559_491,
                "m-1",
                &[0, 1, 2, 3, 4],
            ),
        );
        alarm(
            &storage,
            &mut cache,
            &page("2026-09-28T01:38:25Z", 1_790_559_497, "m-2", &[5]),
        );
        assert_eq!(active(&storage), 6);

        // Booted 01:38:34, re-raised all six on two pages after the Info.
        alarm(
            &storage,
            &mut cache,
            &page(
                "2026-09-28T01:38:52Z",
                1_790_559_514,
                "m-3",
                &[0, 1, 2, 3, 4],
            ),
        );
        alarm(
            &storage,
            &mut cache,
            &page("2026-09-28T01:38:54Z", 1_790_559_514, "m-4", &[5]),
        );
        close_forgotten_native_alarms(
            &storage,
            &mut cache,
            "hub1",
            "aa11",
            &boot_info("2026-09-28T01:38:48Z", 14),
            LATER,
            &ntx,
            &gtx,
        )
        .expect("reconcile");
        assert_eq!(active(&storage), 6, "every re-raised slot stays open");
    }

    /// E6-1 without the second guard: a portal restarted between the re-raise and the boot
    /// Info has an empty activation cache, so the restart rule reads the re-raises from
    /// `node_events` alone. Before V27 it found slots 0 and 5 and closed the other four.
    #[test]
    fn a_restart_rule_on_an_empty_cache_finds_every_re_raise_in_the_rows() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, _nrx, gtx, _grx) = channels();
        let at_raise = "2026-09-28T01:38:23Z";
        for page in [
            alarm_page("m-1", at_raise, Some((0, 2)), &[0, 1, 2, 3, 4]),
            alarm_page("m-2", at_raise, Some((1, 2)), &[5]),
        ] {
            run_node_alarm_engine(&storage, &cache, "hub1", "aa11", &page, &ntx, &gtx)
                .expect("engine");
            persist_node_reading(&storage, &mut cache, "hub1", "aa11", &page).expect("persist");
        }
        let at_reraise = "2026-09-28T01:38:52Z";
        for page in [
            alarm_page("m-3", at_reraise, Some((0, 2)), &[0, 1, 2, 3, 4]),
            alarm_page("m-4", at_reraise, Some((1, 2)), &[5]),
        ] {
            run_node_alarm_engine(&storage, &cache, "hub1", "aa11", &page, &ntx, &gtx)
                .expect("engine");
            persist_node_reading(&storage, &mut cache, "hub1", "aa11", &page).expect("persist");
        }
        assert_eq!(storage.count_active(Some("hub1")).expect("count"), 6);

        // Booted 01:38:34, before the re-raise; the portal restarted since.
        let mut restarted = PersistCache::default();
        let boot_info = crate::state::NodeDeviceInfoMsg::new(
            "aa11",
            "2026-09-28T01:38:48Z",
            serde_json::Map::from_iter([
                ("uptime_s".to_string(), serde_json::json!(14)),
                ("active_alarms".to_string(), serde_json::json!([])),
            ]),
        );
        close_forgotten_native_alarms(
            &storage,
            &mut restarted,
            "hub1",
            "aa11",
            &boot_info,
            LATER,
            &ntx,
            &gtx,
        )
        .expect("reconcile");
        assert_eq!(
            storage.count_active(Some("hub1")).expect("count"),
            6,
            "every re-raised slot has its row"
        );
    }

    /// GitLab #135: an Info carrying the crash record stores it as a `fatal_error` event, once:
    /// the retained document replayed on a reconnect is the same row. An Info without one
    /// stores nothing.
    #[test]
    fn an_info_with_a_crash_record_stores_it_once() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, _nrx, gtx, _grx) = channels();
        let info = |received_at: &str, fault: bool| {
            let mut m = serde_json::Map::from_iter([
                ("reset_cause".to_string(), serde_json::json!(3)),
                ("active_alarms".to_string(), serde_json::json!([])),
            ]);
            if fault {
                m.insert("fault_reason".into(), serde_json::json!(20));
                m.insert(
                    "fault_reason_name".into(),
                    serde_json::json!("mem_instruction_access"),
                );
                m.insert("fault_pc".into(), serde_json::json!("0xf0000000"));
                m.insert("fault_lr".into(), serde_json::json!("0x080280d5"));
            }
            IngestEvent::NodeDeviceInfo {
                hostname: "hub1".into(),
                node_key: "aa11".into(),
                payload: Some(Box::new(crate::state::NodeDeviceInfoMsg::new(
                    "aa11",
                    received_at,
                    m,
                ))),
            }
        };
        for _ in 0..2 {
            handle(
                &storage,
                info("2026-10-14T08:00:00Z", true),
                &mut cache,
                &ntx,
                &gtx,
            )
            .expect("persist");
        }
        handle(
            &storage,
            info("2026-10-14T09:00:00Z", false),
            &mut cache,
            &ntx,
            &gtx,
        )
        .expect("persist");
        let rows = storage
            .query_events(
                "hub1",
                "aa11",
                Some(crate::render::FATAL_ERROR_EVENT),
                0,
                i64::MAX,
                None,
            )
            .expect("query");
        assert_eq!(rows.len(), 1, "one record, replay deduplicated");
        let payload: serde_json::Value =
            serde_json::from_str(rows[0].payload.as_deref().expect("payload")).expect("json");
        assert_eq!(payload["fault_reason_name"], "mem_instruction_access");
        assert_eq!(payload["fault_pc"], "0xf0000000");
        assert_eq!(payload["reset_cause"], 3);
        assert_eq!(payload["received_at"], "2026-10-14T08:00:00Z");
    }

    /// A `now` past every grace window in these tests' 2026-09 data: the close runs at once.
    const LATER: i64 = 4_000_000_000;

    /// node_reported_state.md §4, amended 2026-09-27 (bench, 0413/5722 08:20Z): the boot Info
    /// arrives ~11 s before the Node re-raises what it still holds. Inside the grace window the
    /// Info closes nothing; the re-raise lands on the open row, and at the end of the window
    /// only an alarm not re-raised since the boot is closed. One row, one notification.
    #[test]
    fn a_restart_closes_native_alarms_only_after_the_grace_window() {
        let raise = |at: &str, time: i64, id: &str, slot: i64| -> NodeReadingMsg {
            serde_json::from_str(&format!(
                r#"{{"counters":{{}},"dev_eui":"aa11","events":[
                {{"type":"alarm","ts":"{at}","event":"activate","side":"high",
                 "channel":"temperature","slot":"mb","rule":{slot},"value":27.0,"time":{time}}}],
                "fields":{{}},"fport":3,"gateways":[],"message_id":"{id}","received_at":"{at}"}}"#
            ))
            .expect("parses")
        };
        let boot_info = |received_at: &str, uptime: i64| {
            crate::state::NodeDeviceInfoMsg::new(
                "aa11",
                received_at,
                serde_json::Map::from_iter([
                    ("uptime_s".to_string(), serde_json::json!(uptime)),
                    ("active_alarms".to_string(), serde_json::json!([])),
                ]),
            )
        };
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, mut nrx, gtx, _grx) = channels();
        let active = |storage: &Storage| storage.count_active(Some("hub1")).expect("count");
        let alarm = |storage: &Storage, cache: &mut PersistCache, doc: &NodeReadingMsg| {
            run_node_alarm_engine(storage, cache, "hub1", "aa11", doc, &ntx, &gtx).expect("engine");
            persist_node_reading(storage, cache, "hub1", "aa11", doc).expect("persist");
        };
        // 2026-09-27T08:20:27Z = 1_790_497_227.
        let t = |s: &str| parse_rfc3339(s).expect("rfc3339");

        // Two native alarms, slots 0 and 1, both open before a reboot at 08:20:40.
        alarm(
            &storage,
            &mut cache,
            &raise("2026-09-27T08:20:27Z", 1_790_497_227, "m-0", 0),
        );
        alarm(
            &storage,
            &mut cache,
            &raise("2026-09-27T08:20:28Z", 1_790_497_228, "m-1", 1),
        );
        assert_eq!(active(&storage), 2);
        while nrx.try_recv().is_ok() {}
        let slot0 = storage
            .active_alarms("hub1")
            .expect("rows")
            .into_iter()
            .find(|r| r.sensor_id.ends_with("native:0"))
            .expect("slot 0")
            .id;

        // The boot Info, 5 s after the boot: nothing closes yet.
        close_forgotten_native_alarms(
            &storage,
            &mut cache,
            "hub1",
            "aa11",
            &boot_info("2026-09-27T08:20:45Z", 5),
            t("2026-09-27T08:20:45Z"),
            &ntx,
            &gtx,
        )
        .expect("reconcile");
        assert_eq!(active(&storage), 2, "inside the grace window");

        // The Node re-raises slot 0 11 s later, onto the open row; slot 1 no longer holds.
        alarm(
            &storage,
            &mut cache,
            &raise("2026-09-27T08:20:56Z", 1_790_497_256, "m-2", 0),
        );
        assert_eq!(active(&storage), 2, "the re-raise opens no second row");
        run_due_restart_closes(&storage, &mut cache, t("2026-09-27T08:22:00Z"), &ntx, &gtx)
            .expect("not yet due");
        assert_eq!(active(&storage), 2, "120 s from the boot have not passed");

        run_due_restart_closes(&storage, &mut cache, t("2026-09-27T08:22:41Z"), &ntx, &gtx)
            .expect("due");
        let open = storage.active_alarms("hub1").expect("rows");
        assert_eq!(
            open.len(),
            1,
            "only the alarm not re-raised since the boot closed: {open:?}"
        );
        assert_eq!(open[0].id, slot0, "slot 0 keeps its original row");
        assert!(
            matches!(nrx.try_recv(), Ok(NotifyJob::Cleared { .. })),
            "slot 1's clear"
        );
        assert!(cache.restart_closes.is_empty(), "run once");
    }

    /// The window is 2 × the Node's report interval from its last config document, at least
    /// 120 s; and an Info long after its boot, or a replay whose window has passed, closes at
    /// once as before.
    #[test]
    fn the_restart_grace_window_follows_the_report_interval() {
        let mut cache = PersistCache::default();
        assert_eq!(restart_grace_s(&cache, "hub1", "aa11"), 120);
        cache
            .node_interval_report
            .insert(("hub1".into(), "aa11".into()), 30);
        assert_eq!(
            restart_grace_s(&cache, "hub1", "aa11"),
            120,
            "at least 120 s"
        );
        cache
            .node_interval_report
            .insert(("hub1".into(), "aa11".into()), 900);
        assert_eq!(restart_grace_s(&cache, "hub1", "aa11"), 1800);

        let raise_doc = r#"{"counters":{},"dev_eui":"aa11","events":[
            {"type":"alarm","ts":"2026-09-27T08:00:00Z","event":"activate","side":"high",
             "channel":"temperature","slot":"mb","rule":0,"value":27.0,"time":1790496000}],
            "fields":{},"fport":3,"gateways":[],"message_id":"m-raise",
            "received_at":"2026-09-27T08:00:00Z"}"#;
        let (_dir, storage) = temp_storage();
        let (ntx, _nrx, gtx, _grx) = channels();
        let raise: NodeReadingMsg = serde_json::from_str(raise_doc).expect("parses");
        run_node_alarm_engine(&storage, &cache, "hub1", "aa11", &raise, &ntx, &gtx)
            .expect("engine");
        persist_node_reading(&storage, &mut cache, "hub1", "aa11", &raise).expect("persist");
        let info = crate::state::NodeDeviceInfoMsg::new(
            "aa11",
            "2026-09-27T08:20:45Z",
            serde_json::Map::from_iter([
                ("uptime_s".to_string(), serde_json::json!(5)),
                ("active_alarms".to_string(), serde_json::json!([])),
            ]),
        );
        let now = parse_rfc3339("2026-09-27T08:24:00Z").expect("rfc3339");
        close_forgotten_native_alarms(&storage, &mut cache, "hub1", "aa11", &info, now, &ntx, &gtx)
            .expect("reconcile");
        assert_eq!(
            storage.count_active(Some("hub1")).expect("count"),
            1,
            "a 900 s Node's window is 30 min"
        );
        close_forgotten_native_alarms(
            &storage, &mut cache, "hub1", "aa11", &info, LATER, &ntx, &gtx,
        )
        .expect("reconcile");
        assert_eq!(
            storage.count_active(Some("hub1")).expect("count"),
            0,
            "the same Info replayed after its window closes at once"
        );
    }

    /// nodes_phase1.cs.md WP2: config info whose active sensors move a banded field to another
    /// alarm slot drafts the table again; a replay that moves nothing drafts nothing, and a row
    /// the operator drafted stays theirs.
    #[test]
    fn config_info_that_moves_the_alarm_slots_drafts_the_table() {
        let (_dir, storage) = temp_storage();
        for (field, lo, hi) in [("temperature", 15.0, 25.0), ("w1_1_temperature", 2.0, 8.0)] {
            storage
                .upsert_field_threshold(
                    &portal_storage::NewFieldThreshold {
                        hostname: "hub1".into(),
                        scope: THRESHOLD_SCOPE_NODE.into(),
                        entity: "aa11".into(),
                        field: field.into(),
                        critical_low: Some(lo),
                        warning_low: None,
                        warning_high: None,
                        critical_high: Some(hi),
                        enabled: true,
                    },
                    Some("admin"),
                )
                .expect("threshold");
        }
        let written: Vec<String> = ["temperature", "humidity", "w1_1_temperature"]
            .map(String::from)
            .to_vec();
        storage
            .set_node_alarm_layout("hub1", "aa11", &written, 1)
            .expect("layout");
        let doc = |barometer: bool| -> crate::state::NodeConfigDumpMsg {
            let mut config = serde_json::Map::new();
            for cap in [
                "cap_hall_left",
                "cap_hall_right",
                "cap_input_a",
                "cap_input_b",
                "cap_light_sensor",
                "cap_pir_detector",
                "cap_accelerometer",
            ] {
                config.insert(format!("sensors.{cap}"), serde_json::json!(false));
            }
            config.insert("sensors.cap_barometer".into(), serde_json::json!(barometer));
            config.insert("sensors.cap_w1_sensors".into(), serde_json::json!(true));
            serde_json::from_value(serde_json::json!({
                "dev_eui": "aa11",
                "received_at": "2026-09-24T10:00:00Z",
                "seq": 0,
                "page_count": 1,
                "config": config,
                "w1_slot_type": ["dallas", "empty", "empty", "empty"],
            }))
            .expect("doc")
        };

        // The same sensors as written: nothing moves, nothing is drafted.
        redraft_alarm_table_on_layout_change(&storage, "hub1", "aa11", &doc(false)).expect("ok");
        assert!(storage
            .list_pending("hub1", "aa11")
            .expect("list")
            .is_empty());

        // MR !129 review: a barometer without a band takes no slot, and a v2.0.0 rule names its
        // own target, so an index moving drafts nothing either.
        redraft_alarm_table_on_layout_change(&storage, "hub1", "aa11", &doc(true)).expect("ok");
        assert!(storage
            .list_pending("hub1", "aa11")
            .expect("list")
            .is_empty());

        // A table written without the Dallas field: every banded field is drafted again, by
        // the portal.
        storage
            .set_node_alarm_layout("hub1", "aa11", &["temperature".to_string()], 2)
            .expect("layout");
        redraft_alarm_table_on_layout_change(&storage, "hub1", "aa11", &doc(true)).expect("ok");
        let rows = storage.list_pending("hub1", "aa11").expect("list");
        let mut keys: Vec<&str> = rows.iter().map(|r| r.param_key.as_str()).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                crate::pending::threshold_enabled_param_key("temperature").as_str(),
                crate::pending::threshold_enabled_param_key("w1_1_temperature").as_str(),
            ]
        );
        assert!(rows
            .iter()
            .all(|r| r.author == crate::pending::ALARM_LAYOUT_AUTHOR
                && r.desired_value.as_deref() == Some("true")));

        // The operator's own draft of one of them is not overwritten on a replay.
        storage
            .upsert_pending(&portal_storage::NewPendingConfig {
                hostname: "hub1".into(),
                node_key: "aa11".into(),
                param_key: crate::pending::threshold_enabled_param_key("temperature"),
                kind: portal_storage::PENDING_KIND_THRESHOLD.into(),
                desired_value: Some("false".into()),
                author: "manager1".into(),
            })
            .expect("draft");
        redraft_alarm_table_on_layout_change(&storage, "hub1", "aa11", &doc(true)).expect("ok");
        let temperature = storage
            .list_pending("hub1", "aa11")
            .expect("list")
            .into_iter()
            .find(|r| r.param_key.ends_with("temperature.enabled") && !r.param_key.contains("w1"))
            .expect("row");
        assert_eq!(temperature.author, "manager1");
        assert_eq!(temperature.desired_value.as_deref(), Some("false"));
    }

    /// HIL 5722 (2026-10-15): a switched-off band the Node was sent earlier is missing from
    /// the written table by design (it carries enabled bands only), so a dump drafts nothing;
    /// and a dump that arrives while a send walk is in flight drafts nothing either.
    #[test]
    fn a_disabled_band_or_a_walk_in_flight_drafts_no_table() {
        let (_dir, storage) = temp_storage();
        for (field, enabled) in [("temperature", true), ("humidity", false)] {
            storage
                .upsert_field_threshold(
                    &portal_storage::NewFieldThreshold {
                        hostname: "h1".into(),
                        scope: THRESHOLD_SCOPE_NODE.into(),
                        entity: "aa11".into(),
                        field: field.into(),
                        critical_low: Some(10.0),
                        warning_low: None,
                        warning_high: None,
                        critical_high: Some(30.0),
                        enabled,
                    },
                    Some("admin"),
                )
                .expect("threshold");
        }
        // Humidity was delivered while it was on, then switched off and sent again.
        storage
            .mark_node_delivery_pending(
                "h1",
                "aa11",
                &[portal_storage::NodeBand {
                    field: "humidity".into(),
                    low: Some(10.0),
                    high: Some(30.0),
                    enabled: true,
                }],
                1,
            )
            .expect("delivery");
        storage
            .set_node_alarm_layout("h1", "aa11", &["temperature".to_string()], 2)
            .expect("layout");
        let mut config = serde_json::Map::new();
        config.insert("sensors.cap_w1_sensors".into(), serde_json::json!(false));
        let doc: crate::state::NodeConfigDumpMsg = serde_json::from_value(serde_json::json!({
            "dev_eui": "aa11",
            "received_at": "2026-10-15T10:00:00Z",
            "seq": 0,
            "page_count": 1,
            "config": config,
        }))
        .expect("doc");
        assert!(storage
            .list_node_delivery("h1", "aa11")
            .expect("delivery")
            .iter()
            .any(|d| d.field == "humidity"));

        redraft_alarm_table_on_layout_change(&storage, "h1", "aa11", &doc).expect("ok");
        assert!(
            storage.list_pending("h1", "aa11").expect("list").is_empty(),
            "a switched-off band absent from the written table is not a move"
        );

        // Now the written table lacks an enabled band, but a walk is sending: nothing drafted.
        storage
            .set_node_alarm_layout("h1", "aa11", &[], 3)
            .expect("layout");
        storage
            .upsert_pending(&portal_storage::NewPendingConfig {
                hostname: "h1".into(),
                node_key: "aa11".into(),
                param_key: "application.interval_report".into(),
                kind: portal_storage::PENDING_KIND_CONFIG.into(),
                desired_value: Some("900".into()),
                author: "admin".into(),
            })
            .expect("draft");
        storage
            .set_pending_state(
                "h1",
                "aa11",
                "application.interval_report",
                None,
                portal_storage::PENDING_STATE_SENDING,
                None,
                false,
            )
            .expect("sending");
        redraft_alarm_table_on_layout_change(&storage, "h1", "aa11", &doc).expect("ok");
        let rows = storage.list_pending("h1", "aa11").expect("list");
        assert_eq!(rows.len(), 1, "only the walk's own row");

        // The walk done, the same dump drafts the missing enabled band.
        storage
            .delete_pending("h1", "aa11", "application.interval_report", None)
            .expect("delete");
        redraft_alarm_table_on_layout_change(&storage, "h1", "aa11", &doc).expect("ok");
        let keys: Vec<String> = storage
            .list_pending("h1", "aa11")
            .expect("list")
            .into_iter()
            .map(|r| r.param_key)
            .collect();
        assert!(keys.contains(&crate::pending::threshold_enabled_param_key("temperature")));
    }

    /// MR !129 review: the table is redrafted when the configured type of a slot that holds a
    /// banded 1-Wire field changed since it was written; its rule names the old type.
    #[test]
    fn a_changed_slot_type_drafts_the_table() {
        let (_dir, storage) = temp_storage();
        storage
            .upsert_field_threshold(
                &portal_storage::NewFieldThreshold {
                    hostname: "h1".into(),
                    scope: THRESHOLD_SCOPE_NODE.into(),
                    entity: "aa11".into(),
                    field: "w1_1_temperature".into(),
                    critical_low: Some(2.0),
                    warning_low: None,
                    warning_high: None,
                    critical_high: Some(8.0),
                    enabled: true,
                },
                Some("admin"),
            )
            .expect("threshold");
        let written = crate::radio::alarm::stored_layout(
            &["w1_1_temperature".to_string()],
            &crate::radio::alarm::SlotTypes([2, 0, 0, 0]),
        );
        storage
            .set_node_alarm_layout("h1", "aa11", &written, 1)
            .expect("layout");
        let with_type = |t: u64| {
            let mut doc = one_dallas_config("aa11");
            doc.config
                .insert("sensors.sensor1_type".into(), serde_json::json!(t));
            doc
        };

        // Written for Dallas (2), configured Dallas: nothing.
        redraft_alarm_table_on_layout_change(&storage, "h1", "aa11", &with_type(2)).expect("ok");
        assert!(storage.list_pending("h1", "aa11").expect("list").is_empty());

        // The slot's configured type is cleared (0): the rule is inert, the table is drafted
        // (and its send is refused until the type is set, `radio::alarm::TableRefusal`).
        redraft_alarm_table_on_layout_change(&storage, "h1", "aa11", &with_type(0)).expect("ok");
        let keys: Vec<String> = storage
            .list_pending("h1", "aa11")
            .expect("list")
            .into_iter()
            .map(|r| r.param_key)
            .collect();
        assert_eq!(
            keys,
            [crate::pending::threshold_enabled_param_key(
                "w1_1_temperature"
            )]
        );

        // A layout recorded before the types were: compared by membership alone.
        storage
            .delete_pending("h1", "aa11", &keys[0], None)
            .expect("discard");
        storage
            .set_node_alarm_layout("h1", "aa11", &["w1_1_temperature".to_string()], 2)
            .expect("layout");
        redraft_alarm_table_on_layout_change(&storage, "h1", "aa11", &with_type(0)).expect("ok");
        assert!(storage.list_pending("h1", "aa11").expect("list").is_empty());
    }

    // ---- default bands (plan/portal/threshold_defaults.md §3, §6) ----------------------

    /// The config info of a Node with one Dallas probe in 1-Wire slot 1.
    fn one_dallas_config(dev_eui: &str) -> crate::state::NodeConfigDumpMsg {
        let mut config = serde_json::Map::new();
        for cap in [
            "cap_barometer",
            "cap_hall_left",
            "cap_hall_right",
            "cap_input_a",
            "cap_input_b",
            "cap_light_sensor",
            "cap_pir_detector",
            "cap_accelerometer",
        ] {
            config.insert(format!("sensors.{cap}"), serde_json::json!(false));
        }
        config.insert("sensors.cap_w1_sensors".into(), serde_json::json!(true));
        serde_json::from_value(serde_json::json!({
            "dev_eui": dev_eui,
            "received_at": "2026-09-28T08:00:00Z",
            "seq": 0,
            "page_count": 1,
            "config": config,
            "w1_slot_type": ["dallas", "empty", "empty", "empty"],
        }))
        .expect("doc")
    }

    fn node_rows(storage: &Storage, node: &str) -> Vec<portal_storage::FieldThreshold> {
        storage
            .list_field_thresholds("h1", THRESHOLD_SCOPE_NODE, node)
            .expect("list")
    }

    /// A Node the portal learns of from the Hub's inventory document gets its board's bands,
    /// switched off; its config info adds the 1-Wire probe it declares, its reading a probe
    /// that reports first. Nothing is drafted — not even when the probe moves the Node's
    /// alarm slots — nothing is graded, and a restart writes nothing more.
    #[test]
    fn a_registered_node_gets_switched_off_default_bands_and_nothing_else() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, _nrx, gtx, _grx) = channels();
        let node = "58760700c0668fc7";
        let feed = |cache: &mut PersistCache, event: IngestEvent| {
            handle(&storage, event, cache, &ntx, &gtx).expect("handled")
        };

        feed(
            &mut cache,
            IngestEvent::NodeInventory {
                hostname: "h1".into(),
                node_key: node.into(),
                payload: serde_json::json!({"dev_eui": node, "transport": "lorawan"}),
            },
        );
        let fields: Vec<String> = node_rows(&storage, node)
            .into_iter()
            .map(|t| t.field)
            .collect();
        assert_eq!(fields, ["humidity", "temperature"]);

        feed(
            &mut cache,
            IngestEvent::NodeDeviceConfig {
                hostname: "h1".into(),
                node_key: node.into(),
                payload: Some(Box::new(one_dallas_config(node))),
            },
        );
        let mut reading = node_reading("m-1", "2026-09-28T08:15:00Z", 61.0);
        reading.fields.insert("voltage".to_string(), 2.2);
        reading.fields.insert("w1_2_humidity".to_string(), 99.0);
        feed(
            &mut cache,
            IngestEvent::NodeReading {
                hostname: "h1".into(),
                node_key: node.into(),
                payload: Box::new(reading),
            },
        );

        let rows = node_rows(&storage, node);
        let fields: Vec<&str> = rows.iter().map(|t| t.field.as_str()).collect();
        assert_eq!(
            fields,
            [
                "humidity",
                "temperature",
                "w1_1_temperature",
                "w1_2_humidity"
            ],
            "no battery"
        );
        assert!(rows.iter().all(|t| !t.enabled), "{rows:?}");
        let dallas = rows
            .iter()
            .find(|t| t.field == "w1_1_temperature")
            .expect("row");
        assert_eq!(
            (dallas.critical_low, dallas.critical_high),
            (Some(-10.0), Some(50.0))
        );

        // 61 °C and 99 % against switched-off bands, and 2.2 V: no alarm, no draft.
        assert_eq!(storage.count_active(Some("h1")).expect("count"), 0);
        assert!(storage.list_pending("h1", node).expect("drafts").is_empty());
        assert!(storage
            .list_node_delivery("h1", node)
            .expect("delivery")
            .is_empty());

        // A restart: a fresh cache, the Hub's replay, nothing more written.
        let history = |storage: &Storage| {
            storage
                .threshold_history("h1", THRESHOLD_SCOPE_NODE, node, None)
                .expect("history")
                .len()
        };
        let before = history(&storage);
        let mut restarted = PersistCache::default();
        feed(
            &mut restarted,
            IngestEvent::NodeInventory {
                hostname: "h1".into(),
                node_key: node.into(),
                payload: serde_json::json!({"dev_eui": node, "transport": "lorawan"}),
            },
        );
        feed(
            &mut restarted,
            IngestEvent::NodeDeviceConfig {
                hostname: "h1".into(),
                node_key: node.into(),
                payload: Some(Box::new(one_dallas_config(node))),
            },
        );
        assert_eq!(history(&storage), before);
        assert_eq!(node_rows(&storage, node).len(), 4);
    }

    /// The first-start backfill fills only what is missing: an operator's band, on or off,
    /// stays exactly as it was.
    #[test]
    fn the_backfill_leaves_an_existing_row_alone() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, _nrx, gtx, _grx) = channels();
        let node = "58760700c0668fc7";
        storage
            .upsert_field_threshold(
                &portal_storage::NewFieldThreshold {
                    hostname: "h1".into(),
                    scope: THRESHOLD_SCOPE_NODE.into(),
                    entity: node.into(),
                    field: "temperature".into(),
                    critical_low: Some(2.0),
                    warning_low: Some(4.0),
                    warning_high: Some(6.0),
                    critical_high: Some(8.0),
                    enabled: false,
                },
                Some("manager1"),
            )
            .expect("an operator's band, switched off");
        handle(
            &storage,
            IngestEvent::NodeInventory {
                hostname: "h1".into(),
                node_key: node.into(),
                payload: serde_json::json!({"dev_eui": node}),
            },
            &mut cache,
            &ntx,
            &gtx,
        )
        .expect("handled");
        let rows = node_rows(&storage, node);
        let t = rows.iter().find(|t| t.field == "temperature").expect("row");
        assert_eq!(
            (
                t.critical_low,
                t.warning_low,
                t.warning_high,
                t.critical_high,
                t.enabled
            ),
            (Some(2.0), Some(4.0), Some(6.0), Some(8.0), false)
        );
        assert!(
            rows.iter().any(|t| t.field == "humidity"),
            "the missing one"
        );
    }

    /// A switched-off default band sitting in a slot that moves drafts nothing: the Node never
    /// had its rule. An enabled band still drafts the table, and the default goes with it.
    #[test]
    fn a_layout_move_drafts_nothing_for_switched_off_default_bands() {
        let (_dir, storage) = temp_storage();
        let d = crate::threshold_defaults::configured();
        crate::threshold_defaults::seed(
            &storage,
            &d,
            "h1",
            THRESHOLD_SCOPE_NODE,
            "aa11",
            ["temperature", "humidity", "w1_1_temperature"],
        )
        .expect("seed");
        // Before any table was written the portal assumes the pre-table layout, where
        // `w1_1_temperature` has no slot: the Dallas probe "moves" it.
        redraft_alarm_table_on_layout_change(&storage, "h1", "aa11", &one_dallas_config("aa11"))
            .expect("ok");
        assert!(storage.list_pending("h1", "aa11").expect("list").is_empty());

        storage
            .set_field_threshold_enabled(
                "h1",
                THRESHOLD_SCOPE_NODE,
                "aa11",
                "w1_1_temperature",
                true,
            )
            .expect("switched on");
        redraft_alarm_table_on_layout_change(&storage, "h1", "aa11", &one_dallas_config("aa11"))
            .expect("ok");
        let keys: Vec<String> = storage
            .list_pending("h1", "aa11")
            .expect("list")
            .into_iter()
            .map(|r| r.param_key)
            .collect();
        assert_eq!(
            keys,
            [crate::pending::threshold_enabled_param_key(
                "w1_1_temperature"
            )]
        );
    }

    /// A Beacon the Hub names registered gets its two bands, switched off; a discovered
    /// candidate gets none.
    #[test]
    fn a_registered_beacon_gets_default_bands_and_a_candidate_none() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, _nrx, gtx, _grx) = channels();
        let mut candidate = eye_tag_at("aa:bb:cc:dd:ee:02", 1_000);
        candidate.discovered = Some(true);
        handle(
            &storage,
            IngestEvent::EyeSensors {
                hostname: "h1".into(),
                payload: EyeSensorsMsg {
                    timestamp: None,
                    discovered_capped: false,
                    tags: vec![eye_tag_at("aa:bb:cc:dd:ee:01", 1_000), candidate],
                },
            },
            &mut cache,
            &ntx,
            &gtx,
        )
        .expect("handled");
        let rows = storage
            .list_field_thresholds("h1", THRESHOLD_SCOPE_EYE, "AA:BB:CC:DD:EE:01")
            .expect("list");
        let fields: Vec<&str> = rows.iter().map(|t| t.field.as_str()).collect();
        assert_eq!(fields, ["humidity", "temperature"]);
        assert!(rows.iter().all(|t| !t.enabled));
        assert!(storage
            .list_field_thresholds("h1", THRESHOLD_SCOPE_EYE, "AA:BB:CC:DD:EE:02")
            .expect("list")
            .is_empty());
    }

    #[test]
    fn a_batch_header_or_a_reading_event_is_not_an_alarm() {
        let empty = serde_json::Map::new();
        assert_eq!(v2_alarm_side("alarm_batch", &empty), None);
        assert_eq!(
            v2_alarm_side("high", &empty),
            None,
            "a side without an edge"
        );
        assert_eq!(
            v2_alarm_side("alarm", &empty),
            Some(None),
            "an alarm naming no side"
        );
    }

    // ---- alarm engine wiring ----------------------------------------------

    fn wired_alarm(line: i64, from: &str, to: &str, temp: Option<f64>) -> AlarmEventMsg {
        AlarmEventMsg {
            timestamp: Some("2026-08-18T00:00:00Z".to_string()),
            line,
            name: Some("Fridge".to_string()),
            from_state: from.to_string(),
            to_state: to.to_string(),
            temperature_celsius: temp,
            event_type: None,
            message: None,
            alarm_type: None,
        }
    }

    /// Line 0 of `h1` with the bounds −5 / 0 / 8 / 12 °C in the portal (decision #41: the
    /// portal grades the line), default hysteresis 0.5 °C.
    fn wired_band(storage: &Storage) {
        storage
            .upsert_field_threshold(
                &NewFieldThreshold {
                    hostname: "h1".into(),
                    scope: THRESHOLD_SCOPE_HUB.into(),
                    entity: "0".into(),
                    field: "temperature".into(),
                    critical_low: Some(-5.0),
                    warning_low: Some(0.0),
                    warning_high: Some(8.0),
                    critical_high: Some(12.0),
                    enabled: true,
                },
                Some("tester"),
            )
            .unwrap();
    }

    /// One `sensors/summary` cycle with line 0 of `h1` at `temp`.
    fn live_line0_event(temp: f64) -> IngestEvent {
        let payload = serde_json::json!({
            "period_start_ts": 1_787_011_200,
            "period_end_ts": 1_787_011_202,
            "sensors": [{
                "line": 0,
                "sample_count": 1,
                "temperature": { "min_celsius": temp, "avg_celsius": temp, "max_celsius": temp },
                "dominant_alarm_state": "NORMAL",
            }],
        });
        IngestEvent::SensorsLive {
            hostname: "h1".to_string(),
            payload: serde_json::from_value(payload).expect("decode"),
        }
    }

    fn live_line0(
        storage: &Storage,
        cache: &mut PersistCache,
        ntx: &mpsc::UnboundedSender<NotifyJob>,
        gtx: &broadcast::Sender<AlarmNudge>,
        temp: f64,
    ) {
        handle(storage, live_line0_event(temp), cache, ntx, gtx).unwrap();
    }

    /// The cleared notification of an acknowledged alarm carries the
    /// acknowledgement's reason (the webhook's `acknowledgement.reason`).
    /// The reason is stored as the note the ack writes in the same
    /// transaction, so it is the acknowledging user's first note from the
    /// ack on: an earlier note or a later one is not it.
    #[test]
    fn the_cleared_notification_carries_the_ack_reason() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, mut nrx, gtx, _grx) = channels();
        wired_band(&storage);
        live_line0(&storage, &mut cache, &ntx, &gtx, 9.0);
        assert!(matches!(nrx.try_recv(), Ok(NotifyJob::Activated(_))));
        let id = storage
            .active_alarm("h1", "line-0")
            .unwrap()
            .expect("open")
            .id;

        storage
            .insert_alarm_note(&id, None, "bob", "someone else's note")
            .unwrap();
        let now = OffsetDateTime::now_utc().format(&Rfc3339).unwrap();
        storage
            .acknowledge_alarm(&id, None, "alice", &now)
            .unwrap()
            .expect("acked");
        storage
            .insert_alarm_note(&id, None, "alice", "door was left open")
            .unwrap();
        storage
            .insert_alarm_note(&id, None, "alice", "closed it, watching")
            .unwrap();

        live_line0(&storage, &mut cache, &ntx, &gtx, 5.0);
        match nrx.try_recv() {
            Ok(NotifyJob::Cleared { ack: Some(ack), .. }) => {
                assert_eq!(ack.user, "alice");
                assert_eq!(ack.reason.as_deref(), Some("door was left open"));
            }
            other => panic!("expected an acknowledged Cleared, got {other:?}"),
        }
    }

    /// decisions.md #28c: an acknowledged warning that escalates waits for a
    /// new acknowledgement, keeps the first one's note, and the escalation
    /// still notifies once (#26).
    #[test]
    fn an_escalation_takes_the_acknowledgement_away() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, mut nrx, gtx, _grx) = channels();
        wired_band(&storage);

        live_line0(&storage, &mut cache, &ntx, &gtx, 9.0);
        assert!(matches!(nrx.try_recv(), Ok(NotifyJob::Activated(_))));
        let id = storage.active_alarm("h1", "line-0").unwrap().unwrap().id;
        storage
            .acknowledge_alarm(&id, Some(1), "alice", "2026-08-18T00:01:00Z")
            .unwrap();
        storage
            .insert_alarm_note(&id, Some(1), "alice", "door was left open")
            .unwrap();

        live_line0(&storage, &mut cache, &ntx, &gtx, 20.0);
        let row = storage.get_alarm(&id).unwrap().unwrap();
        assert_eq!(row.acknowledged_at, None, "the critical state waits again");
        assert_eq!(row.acknowledged_by_username, None);
        assert_eq!(storage.list_alarm_notes(&id).unwrap().len(), 1, "note kept");
        assert!(matches!(nrx.try_recv(), Ok(NotifyJob::Escalated(_))));
        assert!(nrx.try_recv().is_err(), "one escalation message");
    }

    #[test]
    fn wired_lifecycle_opens_updates_and_clears_one_row() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, mut nrx, gtx, mut grx) = channels();
        wired_band(&storage);

        // NORMAL -> WARNING: opens.
        live_line0(&storage, &mut cache, &ntx, &gtx, 9.0);
        assert_eq!(storage.count_active(Some("h1")).unwrap(), 1);
        assert!(matches!(nrx.try_recv(), Ok(NotifyJob::Activated(_))));
        let nudge = grx.try_recv().expect("nudge on open");
        assert_eq!(nudge.activated_ids.len(), 1, "open carries its id");
        assert!(nudge.escalated_ids.is_empty());

        // WARNING -> CRITICAL: severity update in place, no new row, and the
        // first escalation notifies (decisions.md #26) from the updated row.
        live_line0(&storage, &mut cache, &ntx, &gtx, 20.0);
        assert_eq!(storage.count_active(Some("h1")).unwrap(), 1);
        let (rows, _) = storage.list_alarms(&Default::default()).unwrap();
        assert_eq!(rows[0].severity, "critical");
        assert_eq!(rows[0].max_severity, "critical");
        match nrx.try_recv() {
            Ok(NotifyJob::Escalated(data)) => {
                assert_eq!(data.severity, "critical");
                assert_eq!(data.value, Some(20.0));
            }
            other => panic!("expected Escalated, got {other:?}"),
        }
        let nudge = grx.try_recv().expect("still nudges the table");
        assert!(nudge.activated_ids.is_empty(), "no new row opened");
        assert_eq!(
            nudge.escalated_ids.len(),
            1,
            "warning→critical carries the escalated id"
        );

        // CRITICAL -> NORMAL: clears, one Cleared notify.
        live_line0(&storage, &mut cache, &ntx, &gtx, 5.0);
        assert_eq!(storage.count_active(Some("h1")).unwrap(), 0);
        assert!(matches!(nrx.try_recv(), Ok(NotifyJob::Cleared { .. })));
        assert!(grx.try_recv().is_ok());

        let (rows, total) = storage.list_alarms(&Default::default()).unwrap();
        assert_eq!(total, 1, "one row for the whole lifecycle");
        assert!(rows[0].cleared_at.is_some());
    }

    /// One `sensors/aggregated` line 0 window starting at `start`, as the Hub
    /// publishes it.
    fn aggregated_line0(start: i64, state: &str) -> IngestEvent {
        let payload = serde_json::json!({
            "period_start_ts": start,
            "period_end_ts": start + 60,
            "sensors": [{
                "line": 0,
                "sample_count": 30,
                "temperature": { "min_celsius": 4.0, "avg_celsius": 5.0, "max_celsius": 6.0 },
                "dominant_alarm_state": state,
            }],
        });
        IngestEvent::SensorsAggregated {
            hostname: "h1".to_string(),
            payload: serde_json::from_value(payload).expect("decode"),
        }
    }

    /// `sensors/summary` (plan/control/storage_intervals.md §4): the live sample stores no
    /// reading; the portal grades the line from it (decision #41).
    #[test]
    fn a_live_sample_stores_nothing_and_grades_the_line() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, mut nrx, gtx, _grx) = channels();
        wired_band(&storage);

        live_line0(&storage, &mut cache, &ntx, &gtx, 9.0);
        assert!(matches!(nrx.try_recv(), Ok(NotifyJob::Activated(_))));
        live_line0(&storage, &mut cache, &ntx, &gtx, 5.0);

        assert_eq!(
            storage.count_raw("h1", 0).unwrap(),
            0,
            "a live sample is not stored"
        );
        assert_eq!(storage.count_active(Some("h1")).unwrap(), 0);
        assert!(matches!(nrx.try_recv(), Ok(NotifyJob::Cleared { .. })));
    }

    /// #110: a line hovering at a bound opens one alarm, held until the value is the
    /// hysteresis back inside it.
    #[test]
    fn a_line_hovering_at_a_bound_opens_one_alarm() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, mut nrx, gtx, _grx) = channels();
        wired_band(&storage);

        for temp in [8.1, 7.9, 8.2, 7.8, 8.1, 7.6] {
            live_line0(&storage, &mut cache, &ntx, &gtx, temp);
        }
        assert_eq!(storage.count_active(Some("h1")).unwrap(), 1);
        assert!(matches!(nrx.try_recv(), Ok(NotifyJob::Activated(_))));
        assert!(nrx.try_recv().is_err(), "one alarm, no flapping");

        live_line0(&storage, &mut cache, &ntx, &gtx, 7.5);
        assert_eq!(storage.count_active(Some("h1")).unwrap(), 0);
        let (_, total) = storage.list_alarms(&Default::default()).unwrap();
        assert_eq!(total, 1);

        // An own hysteresis of the line is used instead of the default.
        storage
            .set_field_threshold_hysteresis(
                "h1",
                THRESHOLD_SCOPE_HUB,
                "0",
                "temperature",
                Some(0.0),
            )
            .unwrap();
        live_line0(&storage, &mut cache, &ntx, &gtx, 8.1);
        live_line0(&storage, &mut cache, &ntx, &gtx, 7.9);
        assert_eq!(storage.count_active(Some("h1")).unwrap(), 0, "0 = strict");
    }

    /// Decision #41: the Hub's value transitions and its window states no longer open or
    /// close a line's alarm; its connection transitions still do.
    #[test]
    fn the_hub_keeps_the_connection_and_the_portal_the_value() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, mut nrx, gtx, _grx) = channels();
        wired_band(&storage);

        handle_wired_alarm(
            &storage,
            &mut cache,
            "h1",
            wired_alarm(0, "NORMAL", "WARNING", Some(9.0)),
            &ntx,
            &gtx,
        )
        .unwrap();
        assert_eq!(storage.count_active(Some("h1")).unwrap(), 0);

        live_line0(&storage, &mut cache, &ntx, &gtx, 9.0);
        assert_eq!(storage.count_active(Some("h1")).unwrap(), 1);
        let _ = nrx.try_recv();
        handle(
            &storage,
            aggregated_line0(1_787_011_200 + 600, "NORMAL"),
            &mut cache,
            &ntx,
            &gtx,
        )
        .unwrap();
        assert_eq!(
            storage.count_active(Some("h1")).unwrap(),
            1,
            "a window closes nothing"
        );

        // Disconnect: the Hub's.
        handle_wired_alarm(
            &storage,
            &mut cache,
            "h1",
            wired_alarm(0, "WARNING", "DISCONNECTED", None),
            &ntx,
            &gtx,
        )
        .unwrap();
        let active = storage.active_alarm("h1", "line-0").unwrap().expect("open");
        assert_eq!(active.alarm_type, "DISCONNECT");
        // A sample during the disconnect grades nothing.
        live_line0(&storage, &mut cache, &ntx, &gtx, 9.0);
        assert_eq!(
            storage
                .active_alarm("h1", "line-0")
                .unwrap()
                .unwrap()
                .alarm_type,
            "DISCONNECT"
        );
        // The reconnect closes it, whatever value the Hub graded; the next sample grades.
        handle_wired_alarm(
            &storage,
            &mut cache,
            "h1",
            wired_alarm(0, "DISCONNECTED", "WARNING", Some(9.0)),
            &ntx,
            &gtx,
        )
        .unwrap();
        assert_eq!(storage.count_active(Some("h1")).unwrap(), 0);
        live_line0(&storage, &mut cache, &ntx, &gtx, 9.0);
        let active = storage.active_alarm("h1", "line-0").unwrap().expect("open");
        assert_eq!(active.alarm_type, "HIGH");
    }

    #[test]
    fn wired_uses_cached_config_state_thresholds_and_location() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, mut nrx, gtx, _grx) = channels();

        let mut config = ConfigStateMsg::default();
        config.sensors.push(SensorConfigEntry {
            line: 0,
            name: Some("Fridge A".to_string()),
            location: Some("Kitchen".to_string()),
            enabled: None,
            has_override: None,
            thresholds: Some(SensorThresholds {
                critical_low: Some(-5.0),
                critical_high: Some(12.0),
                alarm_low: None,
                alarm_high: None,
                hysteresis: None,
                enabled: None,
                warning_low: Some(0.0),
                warning_high: Some(8.0),
            }),
        });
        handle(
            &storage,
            IngestEvent::ConfigState {
                hostname: "h1".into(),
                payload: config,
            },
            &mut cache,
            &ntx,
            &gtx,
        )
        .unwrap();

        // Decision #41: the Hub's bounds are imported as the line's row.
        let row = storage
            .get_field_threshold("h1", THRESHOLD_SCOPE_HUB, "0", "temperature")
            .unwrap()
            .expect("imported");
        assert_eq!(row.warning_high, Some(8.0));
        assert!(row.enabled);

        live_line0(&storage, &mut cache, &ntx, &gtx, 9.0);

        let (rows, _) = storage.list_alarms(&Default::default()).unwrap();
        assert_eq!(rows[0].sensor_location.as_deref(), Some("Kitchen"));
        assert_eq!(rows[0].threshold, Some(8.0));
        assert_eq!(rows[0].alarm_type, "HIGH");
        assert!(
            matches!(nrx.try_recv(), Ok(NotifyJob::Activated(data)) if data.location.as_deref() == Some("Kitchen"))
        );
    }

    #[test]
    fn connectivity_power_loss_opens_and_recovery_clears() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, _nrx, gtx, _grx) = channels();

        let payload = SystemInfoMsg {
            timestamp: None,
            power: Some(fiber_protocol::payloads::system_info::Power {
                battery: None,
                dc: Some(fiber_protocol::payloads::system_info::Dc {
                    voltage_mv: None,
                    connected: Some(false),
                }),
                last_dc_loss: None,
            }),
            ..Default::default()
        };
        handle_system_info(&storage, &mut cache, "h1", payload, &ntx, &gtx).unwrap();
        assert_eq!(storage.count_active(Some("h1")).unwrap(), 1);

        let payload_up = SystemInfoMsg {
            timestamp: None,
            power: Some(fiber_protocol::payloads::system_info::Power {
                battery: None,
                dc: Some(fiber_protocol::payloads::system_info::Dc {
                    voltage_mv: None,
                    connected: Some(true),
                }),
                last_dc_loss: None,
            }),
            ..Default::default()
        };
        handle_system_info(&storage, &mut cache, "h1", payload_up, &ntx, &gtx).unwrap();
        assert_eq!(storage.count_active(Some("h1")).unwrap(), 0);
    }

    /// UX-058: a Hub on one uplink reports the other one `connected: false`
    /// forever. Only a true→false edge this process saw opens the alarm
    /// (v1 `handlers.py:735,763`); `true` still clears one.
    #[test]
    fn a_link_alarm_opens_on_a_seen_drop_not_on_a_steady_false() {
        use fiber_protocol::payloads::system_info::{Ethernet, Network, Wifi};
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, _nrx, gtx, _grx) = channels();
        let net = |wifi: bool, ethernet: bool| SystemInfoMsg {
            network: Some(Network {
                wifi: Some(Wifi {
                    connected: Some(wifi),
                    ..Default::default()
                }),
                ethernet: Some(Ethernet {
                    connected: Some(ethernet),
                    ..Default::default()
                }),
                has_internet: Some(true),
            }),
            ..Default::default()
        };
        let mut send = |wifi, ethernet| {
            handle_system_info(&storage, &mut cache, "h1", net(wifi, ethernet), &ntx, &gtx)
                .unwrap();
            storage.count_active(Some("h1")).unwrap()
        };

        assert_eq!(send(false, true), 0, "a first-sample false opens nothing");
        assert_eq!(
            send(false, true),
            0,
            "nor does it after, while it stays false"
        );
        assert_eq!(send(true, true), 0, "Wi-Fi comes up");
        assert_eq!(send(false, true), 1, "true→false opens sys:wifi");
        assert_eq!(send(false, true), 1, "one-shot while it stays down");
        assert_eq!(send(true, true), 0, "false→true clears it");
    }

    /// decisions.md #30a: the Hub's retained `status` feeds `sys:online`
    /// under the same edge rule as the links. An offline that was already
    /// retained when this process started opens nothing; an online→offline
    /// edge opens the alarm with its notification, and the return clears it
    /// with one.
    #[test]
    fn hub_status_edges_open_and_clear_sys_online() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, mut nrx, gtx, _grx) = channels();
        let mut send = |status: DeviceStatus| {
            let event = IngestEvent::Status {
                hostname: "h1".into(),
                status,
            };
            handle(&storage, event, &mut cache, &ntx, &gtx).unwrap();
            let jobs: Vec<_> = std::iter::from_fn(|| nrx.try_recv().ok())
                .filter(|j| !matches!(j, NotifyJob::SystemSnapshot { .. }))
                .collect();
            (
                storage.active_alarm("h1", "sys:online").unwrap().is_some(),
                jobs,
            )
        };

        let (open, jobs) = send(DeviceStatus::Offline);
        assert!(
            !open && jobs.is_empty(),
            "a first-sample offline opens nothing"
        );
        let (open, _) = send(DeviceStatus::Online);
        assert!(!open, "online with nothing open is nothing");

        let (open, jobs) = send(DeviceStatus::Offline);
        assert!(open, "online→offline opens sys:online");
        assert!(matches!(&jobs[..], [NotifyJob::Activated(d)] if d.alarm_type == "HUB_OFFLINE"));
        let (open, jobs) = send(DeviceStatus::Offline);
        assert!(open && jobs.is_empty(), "one-shot while it stays offline");

        let (open, jobs) = send(DeviceStatus::Online);
        assert!(!open, "the return clears it");
        assert!(matches!(&jobs[..], [NotifyJob::Cleared { .. }]), "{jobs:?}");
    }

    /// MR !103 review: a refused `sys:online` write is retried through
    /// `handle_status` again. The cache must still hold the old state then,
    /// or the retry reads online→offline as offline→offline and the alarm
    /// never opens.
    #[test]
    fn a_refused_hub_offline_write_still_opens_on_retry() {
        let (dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, _nrx, gtx, _grx) = channels();
        handle_status(&storage, &mut cache, "h1", DeviceStatus::Online, &ntx, &gtx).unwrap();

        storage
            .set_busy_timeout(std::time::Duration::from_millis(1))
            .expect("busy_timeout");
        let blocker =
            rusqlite::Connection::open(dir.path().join("portal.db")).expect("open blocker");
        blocker
            .execute_batch(
                "BEGIN EXCLUSIVE; INSERT INTO readings_raw \
                 (hostname, sensor_line, ts, min_c, avg_c, max_c, sample_count) \
                 VALUES ('blocker', 9, 1, 0, 0, 0, 1);",
            )
            .expect("hold the write lock");
        let refused = handle_status(
            &storage,
            &mut cache,
            "h1",
            DeviceStatus::Offline,
            &ntx,
            &gtx,
        );
        assert!(refused.is_err(), "the held lock refuses the alarm write");
        blocker.execute_batch("ROLLBACK").expect("release the lock");

        handle_status(
            &storage,
            &mut cache,
            "h1",
            DeviceStatus::Offline,
            &ntx,
            &gtx,
        )
        .unwrap();
        assert!(
            storage.active_alarm("h1", "sys:online").unwrap().is_some(),
            "the retry still sees the online→offline edge"
        );
    }

    fn node_sensor_with_alarm(node_key: &str, field_state: &str, temp: f64) -> LorawanSensor {
        let mut sensor = lorawan_sensor(node_key, "2026-08-18T00:00:00Z", temp);
        sensor
            .field_alarm_states
            .insert("temperature".to_string(), field_state.to_string());
        sensor.field_thresholds = Some(vec![fiber_protocol::payloads::lorawan::FieldThreshold {
            field: "temperature".to_string(),
            critical_low: None,
            warning_low: None,
            warning_high: Some(30.0),
            critical_high: Some(40.0),
        }]);
        sensor
    }

    /// §21 review, item 18 (pre-existing). `enabled` is the portal's own
    /// switch over whether a stored threshold is evaluated; the firmware
    /// echo is authoritative about the four bounds and says nothing about
    /// it. Re-upserting `enabled: true` on every aggregate publish — every
    /// ~30 s — meant a manager could silence a noisy threshold and watch it
    /// come back before the page finished reloading.
    #[test]
    fn a_firmware_echo_does_not_re_enable_a_silenced_threshold() {
        let (_dir, storage) = temp_storage();
        let mut cache = HashMap::new();

        let echo = |last_seen: &str, temp: f64| LorawanSensorsMsg {
            timestamp: None,
            sensors: vec![node_sensor_with_alarm_at("aa11", last_seen, temp)],
        };

        // The first echo creates the row, enabled — an unmanaged threshold
        // has always been evaluated.
        persist_lorawan_sensors(
            &storage,
            "hub1",
            echo("2026-08-18T00:00:00Z", 21.0),
            &mut cache,
        )
        .expect("first echo");
        let stored = storage
            .get_field_threshold("hub1", THRESHOLD_SCOPE_NODE, "aa11", "temperature")
            .expect("read")
            .expect("row");
        assert!(stored.enabled, "nothing has silenced it yet");

        // A manager silences it.
        assert!(storage
            .set_field_threshold_enabled("hub1", THRESHOLD_SCOPE_NODE, "aa11", "temperature", false)
            .expect("silence"));

        // The next echo carries the same bounds and must not undo that.
        persist_lorawan_sensors(
            &storage,
            "hub1",
            echo("2026-08-18T00:01:00Z", 22.0),
            &mut cache,
        )
        .expect("second echo");
        let stored = storage
            .get_field_threshold("hub1", THRESHOLD_SCOPE_NODE, "aa11", "temperature")
            .expect("read")
            .expect("row");
        assert!(
            !stored.enabled,
            "the echo owns the bounds; the manager owns whether they are evaluated"
        );
        assert_eq!(
            stored.warning_high,
            Some(30.0),
            "and the bounds it does own still land"
        );
    }

    /// devices.node.md, amendment 2026-10-13 (decision #41): a manager's warning bounds are
    /// stored at once and the Hub learns them only on the field's next send, so the echo's
    /// older warnings must not come back over them. Its critical bounds still land.
    #[test]
    fn a_firmware_echo_keeps_the_warnings_the_portal_stored() {
        let (_dir, storage) = temp_storage();
        let mut cache = HashMap::new();
        let echo = |last_seen: &str| LorawanSensorsMsg {
            timestamp: None,
            sensors: vec![node_sensor_with_alarm_at("aa11", last_seen, 21.0)],
        };
        persist_lorawan_sensors(&storage, "hub1", echo("2026-08-18T00:00:00Z"), &mut cache)
            .expect("first echo");
        storage
            .upsert_field_threshold(
                &NewFieldThreshold {
                    hostname: "hub1".into(),
                    scope: THRESHOLD_SCOPE_NODE.into(),
                    entity: "aa11".into(),
                    field: "temperature".into(),
                    critical_low: None,
                    warning_low: Some(5.0),
                    warning_high: Some(28.0),
                    critical_high: Some(35.0),
                    enabled: true,
                },
                Some("manager1"),
            )
            .expect("save");
        persist_lorawan_sensors(&storage, "hub1", echo("2026-08-18T00:01:00Z"), &mut cache)
            .expect("second echo");
        let stored = storage
            .get_field_threshold("hub1", THRESHOLD_SCOPE_NODE, "aa11", "temperature")
            .expect("read")
            .expect("row");
        assert_eq!(
            (stored.warning_low, stored.warning_high),
            (Some(5.0), Some(28.0))
        );
        assert_eq!(
            stored.critical_high,
            Some(40.0),
            "the echo's critical bound lands"
        );
    }

    /// [`node_sensor_with_alarm`] with a caller-chosen `last_seen`, so a
    /// test can send two echoes past `persist_lorawan_sensors`' advance gate.
    fn node_sensor_with_alarm_at(node_key: &str, last_seen: &str, temp: f64) -> LorawanSensor {
        let mut sensor = node_sensor_with_alarm(node_key, "NORMAL", temp);
        sensor.last_seen = Some(last_seen.to_string());
        sensor
    }

    #[test]
    fn field_state_open_and_clear_for_a_node() {
        let (_dir, storage) = temp_storage();
        let cache = PersistCache::default();
        let (ntx, _nrx, gtx, _grx) = channels();

        let sensor = node_sensor_with_alarm("aa11", "WARNING", 35.0);
        run_lorawan_alarm_engine(&storage, &cache, "h1", "aa11", &sensor, &ntx, &gtx)
            .expect("alarm engine");
        assert_eq!(storage.count_active(Some("h1")).unwrap(), 1);

        let sensor = node_sensor_with_alarm("aa11", "NORMAL", 20.0);
        run_lorawan_alarm_engine(&storage, &cache, "h1", "aa11", &sensor, &ntx, &gtx)
            .expect("alarm engine");
        assert_eq!(storage.count_active(Some("h1")).unwrap(), 0);
    }

    #[test]
    fn native_alarm_event_dedup_skips_quietly() {
        let (_dir, storage) = temp_storage();
        let cache = PersistCache::default();
        let (ntx, mut nrx, gtx, mut grx) = channels();

        let extra: serde_json::Map<String, serde_json::Value> = serde_json::json!({
            "slot": "mb", "rule": 2, "channel": "temperature", "edge": "raise", "side": "high",
            "value": 42.0, "base_time": 1000, "rel_s": 30
        })
        .as_object()
        .unwrap()
        .clone();
        let mut sensor = lorawan_sensor("aa11", "2026-08-18T00:00:00Z", 21.0);
        sensor.events.push(LorawanEvent {
            event_type: "alarm".to_string(),
            ts: Some("2026-08-18T00:00:00Z".to_string()),
            extra: extra.clone(),
        });

        run_lorawan_alarm_engine(&storage, &cache, "h1", "aa11", &sensor, &ntx, &gtx)
            .expect("alarm engine");
        assert_eq!(storage.count_active(Some("h1")).unwrap(), 1);
        assert!(matches!(nrx.try_recv(), Ok(NotifyJob::Activated(_))));
        assert!(grx.try_recv().is_ok());

        // Clear it, then redeliver the exact same raise (same node_key/slot/
        // base_time/rel_s) — the native_key unique index must reject the
        // second insert, and the wiring must skip quietly (no panic, no
        // second row, no notify).
        let (rows, _) = storage.list_alarms(&Default::default()).unwrap();
        storage
            .clear_alarm(&rows[0].id, "2026-08-18T00:01:00Z", None)
            .unwrap();
        // Drain the Cleared notify triggered by the manual clear above —
        // irrelevant to this test, just keeping the channel tidy.
        let _ = nrx.try_recv();
        let _ = grx.try_recv();

        run_lorawan_alarm_engine(&storage, &cache, "h1", "aa11", &sensor, &ntx, &gtx)
            .expect("alarm engine");
        assert_eq!(
            storage.count_active(Some("h1")).unwrap(),
            0,
            "redelivered raise must not reopen"
        );
        assert!(
            nrx.try_recv().is_err(),
            "no notify for a skipped dedup conflict"
        );
    }

    // ---- MB3: the alarm-type table, and its no-longer-silent fallback ----

    /// Every variant, both directions. The pair was one-sided before MB3 —
    /// `alarm_type_str` knew `HIGH` and `parse_alarm_type` did not, because
    /// the catch-all happened to answer it — so `Offline` could have been
    /// added to one half alone and nothing would have failed.
    #[test]
    fn every_alarm_type_round_trips_through_its_string() {
        for t in [
            AlarmType::High,
            AlarmType::Low,
            AlarmType::Disconnect,
            AlarmType::PowerDisconnect,
            AlarmType::WifiDisconnect,
            AlarmType::EthernetDisconnect,
            AlarmType::Offline,
            AlarmType::ConcentratorMissing,
            AlarmType::HubOffline,
            AlarmType::NorthbridgeUnreachable,
            AlarmType::ChirpstackDown,
            AlarmType::NorthbridgeUpdateFailed,
            AlarmType::SouthbridgeUpdateFailed,
            AlarmType::ConfigNotDelivered,
            AlarmType::Calibration,
            AlarmType::ConfigSync,
        ] {
            assert_eq!(parse_alarm_type(alarm_type_str(t)), t, "{t:?}");
        }
        // The two liveness scopes must not collide: `OFFLINE` is the
        // portal's own per-device verdict, `DISCONNECT` is the firmware's
        // echoed wired-probe one (monitoring.backend.md §9.3).
        assert_eq!(alarm_type_str(AlarmType::Offline), "OFFLINE");
        assert_ne!(
            alarm_type_str(AlarmType::Offline),
            alarm_type_str(AlarmType::Disconnect)
        );
    }

    /// The catch-all stays **total** — every caller needs an answer and
    /// there is no error channel on this path — but it is no longer
    /// silent. Its failure mode was to render a liveness outage as a
    /// high-temperature alarm with no trace of the substitution anywhere,
    /// which is exactly the mistake MB3 could have made.
    #[test]
    fn an_unknown_alarm_type_still_parses_but_is_not_silent() {
        assert_eq!(parse_alarm_type("SOMETHING_NEW"), AlarmType::High);
        // Casing is not normalised on the way in, so a lowercase spelling
        // is an unknown string like any other — the writers all use the
        // screaming form `alarm_type_str` produces.
        assert_eq!(parse_alarm_type("offline"), AlarmType::High);
        assert_eq!(parse_alarm_type(""), AlarmType::High);
    }

    fn beacon_history_msg(job: &str, state: &str) -> crate::beacon_history::BeaconHistoryMsg {
        serde_json::from_value(serde_json::json!({
            "job": job,
            "mac": MAC,
            "state": state,
            "from": null,
            "to": null,
            "retry": 0,
            "reason": null,
        }))
        .expect("eye/history message")
    }

    fn feed_beacon_history(
        storage: &Storage,
        cache: &mut PersistCache,
        payload: crate::beacon_history::BeaconHistoryMsg,
        ntx: &mpsc::UnboundedSender<NotifyJob>,
        gtx: &broadcast::Sender<AlarmNudge>,
    ) {
        handle(
            storage,
            IngestEvent::BeaconHistory {
                hostname: "h1".into(),
                payload,
            },
            cache,
            ntx,
            gtx,
        )
        .expect("persists");
    }

    /// beacon_history.md §4.1/§4.4: archive rows are stored as `recording`
    /// readings and **never** grade an alarm — a cold excursion an hour ago
    /// is history, not a page to someone now. A replayed batch adds nothing.
    #[test]
    fn archive_records_land_as_rows_and_raise_no_alarm() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, mut nrx, gtx, mut grx) = channels();
        storage
            .upsert_field_threshold(
                &portal_storage::NewFieldThreshold {
                    hostname: "h1".to_string(),
                    scope: THRESHOLD_SCOPE_EYE.to_string(),
                    entity: MAC.to_string(),
                    field: "temperature".to_string(),
                    critical_low: Some(-30.0),
                    warning_low: Some(-25.0),
                    warning_high: Some(5.0),
                    critical_high: Some(8.0),
                    enabled: true,
                },
                Some("mgr1"),
            )
            .expect("band");

        // A download the Hub started itself (CLI): the unknown id gets a row.
        feed_beacon_history(
            &storage,
            &mut cache,
            beacon_history_msg("hub-1", "requested"),
            &ntx,
            &gtx,
        );
        let mut records = beacon_history_msg("hub-1", "records");
        records.seq = Some(1);
        records.records = vec![(1_000, Some(21.5)), (1_900, Some(22.0)), (2_800, None)];
        feed_beacon_history(&storage, &mut cache, records.clone(), &ntx, &gtx);
        feed_beacon_history(&storage, &mut cache, records, &ntx, &gtx);

        let rows = storage
            .query_eye_readings("h1", MAC, 0, i64::MAX, None)
            .expect("query");
        assert_eq!(rows.len(), 3, "INSERT OR IGNORE on {{MAC}}-rec-{{ts}}");
        assert!(rows
            .iter()
            .all(|r| r.event_type == "recording" && r.humidity_pct.is_none()));
        assert_eq!(
            rows[0].message_id.as_deref(),
            Some("AA:BB:CC:DD:EE:FF-rec-1000")
        );

        let sensor_id = "eye:AA:BB:CC:DD:EE:FF:Temperature";
        assert!(storage
            .active_alarm("h1", sensor_id)
            .expect("read")
            .is_none());
        assert!(nrx.try_recv().is_err(), "nothing notified");
        assert!(grx.try_recv().is_err(), "no alarm nudge");

        // Batches stream during the download; one that then fails keeps its
        // rows and their count (§8, corrected 2026-10-09).
        let mut failed = beacon_history_msg("hub-1", "failed");
        failed.reason = Some("unreachable".into());
        feed_beacon_history(&storage, &mut cache, failed, &ntx, &gtx);
        let job = storage
            .get_beacon_history_job("hub-1")
            .expect("read")
            .expect("row");
        assert_eq!(job.trigger, "manual");
        assert_eq!(job.created_by, None);
        assert_eq!(job.state, "failed");
        assert_eq!(job.received, 3, "the replayed batch credits nothing");
        assert_eq!(
            storage
                .query_eye_readings("h1", MAC, 0, i64::MAX, None)
                .expect("query")
                .len(),
            3
        );
    }

    /// §7 Q6 layer 3: on `done` the archive points are compared with the
    /// live readings within half an interval, the counts land on the job,
    /// and an audit row carries every count. A disagreement does not fail
    /// the job.
    #[test]
    fn a_finished_download_is_cross_checked_against_live_readings() {
        let (_dir, storage) = temp_storage();
        let mut cache = PersistCache::default();
        let (ntx, _nrx, gtx, _grx) = channels();
        for (ts, t) in [(1_010, 4.1), (1_910, 7.0)] {
            storage
                .insert_eye_reading(&EyeReading {
                    hostname: "h1".into(),
                    mac: MAC.into(),
                    ts,
                    temperature_c: Some(t),
                    humidity_pct: None,
                    event_type: "live".into(),
                    message_id: None,
                })
                .expect("live row");
        }

        feed_beacon_history(
            &storage,
            &mut cache,
            beacon_history_msg("bh-1", "requested"),
            &ntx,
            &gtx,
        );
        let mut records = beacon_history_msg("bh-1", "records");
        records.records = vec![(1_000, Some(4.0)), (1_900, Some(4.0)), (2_800, Some(4.0))];
        feed_beacon_history(&storage, &mut cache, records, &ntx, &gtx);
        let mut done = beacon_history_msg("bh-1", "done");
        done.received = Some(3);
        done.sessions = Some(1);
        feed_beacon_history(&storage, &mut cache, done.clone(), &ntx, &gtx);

        let job = storage
            .get_beacon_history_job("bh-1")
            .expect("read")
            .expect("row");
        assert_eq!(job.state, "done", "a disagreement warns, it does not fail");
        assert_eq!(job.compared, Some(2));
        assert_eq!(job.disagreed, Some(1), "7.0 vs 4.0 is past 0.5 °C");

        let audit = |s: &Storage| {
            s.list_audit(&portal_storage::AuditFilter {
                event_type: Some("beacon_history_download".into()),
                limit: 10,
                ..Default::default()
            })
            .expect("audit")
            .0
        };
        let rows = audit(&storage);
        assert_eq!(rows.len(), 1);
        let details = rows[0].details.as_deref().unwrap_or_default();
        assert!(details.contains("\"disagreed\":1"), "{details}");

        // A replayed `done` checks nothing twice.
        feed_beacon_history(&storage, &mut cache, done, &ntx, &gtx);
        assert_eq!(audit(&storage).len(), 1);
    }
}
