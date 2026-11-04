//! History-backfill service (M5-F): the job book-keeper and **the only
//! background MQTT publisher** in this binary (plan/v1/viewer/mqtt.md
//! §"Backfill"). Owns the `backfill_jobs` lifecycle *around* the persist
//! thread's part of it — frame crediting (`samples_received`,
//! `requested → receiving`) and done-marking happen in `src/persist.rs` on
//! the writer connection, NEVER here; this service creates jobs, drains the
//! pending queue onto the wire, learns refusals from command results,
//! retries stalls, and nudges the SSE layer to re-render the jobs table.
//!
//! Status machine (ws.md §`backfill_job`): `pending → requested →
//! receiving → done`, terminal branches `failed` / `cancelled`
//! (+ `device_refused` flag). Active = pending/requested/receiving;
//! `pending` is the only freely-cancellable state; `retry_count` is
//! incremented only after a successful publish.
//!
//! Four cooperating pieces, all sharing one `BackfillService`:
//!
//! - **Drain task** (every [`BackfillConfig::drain_period_s`]): per
//!   `(hostname, node_key)` with no job already in requested/receiving,
//!   pick the OLDEST pending job and publish one
//!   `commands/get_sticker_history` downlink — one per node per pass,
//!   honoring a cool-off of [`BackfillConfig::min_request_interval_s`]
//!   since the last downlink *actually sent* to that node (mqtt.md).
//! - **Watcher task**: subscribes the store's `ChangeEvent` broadcast.
//!   `NodeCommandResult` for `get_sticker_history` with a terminal refusal
//!   (`history_unavailable`/`out_of_range`/`not_supported`/
//!   `unsupported_field`/`bad_request`) fails the node's in-flight job with
//!   `device_refused`; OK answers (`ok`/`requested`/`accepted`) are no-ops.
//!   `NodeHistoryProgress` just nudges the SSE table (crediting already
//!   happened in persist).
//! - **Transition task** (B3, devices.backend.md §6.1): subscribes
//!   [`crate::offline::OfflineService::subscribe_transitions`]. *This Node*
//!   coming back is what arms an automatic backfill — not
//!   `DeviceStatus{online:true}`, which is the **Hub** reconnecting and says
//!   nothing about the Node that was actually silent. This Node going
//!   offline again cancels the auto jobs the last recovery queued, so the
//!   next recovery re-detects the gaps instead of draining a stale window.
//! - **Reconciler task** (every [`BackfillConfig::reconciler_period_s`]):
//!   requested/receiving jobs past their `not_before` deadline (V11) are
//!   republished (+ `increment_retry`, publish-first so the count only
//!   reflects real downlinks) or failed with "retries exhausted" once
//!   `retry_count` hits [`BackfillConfig::max_retries`] — which also raises
//!   a `NodeHistoryUnavailable` system alert (§6.3), instead of the job
//!   dying silently in the table.
//!
//! # Retry cadence (§6.2 as decided in §16.1)
//!
//! A retry deadline is `3 x` the Node's own `application.interval_report`,
//! not a fleet-wide stall timeout: the old 120 s rule re-sent nine downlinks
//! to a Node on a 20-minute cadence before its first scheduled uplink could
//! answer. The deadline is stored per job (`backfill_jobs.not_before`)
//! rather than kept in watcher memory so it survives a restart, and it is
//! armed at the moment a downlink actually goes out — `updated_at` moves on
//! every credited frame, which is exactly the event that must not postpone
//! a retry. `DEFAULT_INTERVAL_REPORT_S` (300 s) is the fallback for an
//! unknown cadence; deliberately **not**
//! [`crate::notify::offline::UNKNOWN_INTERVAL_FALLBACK_S`] (900 s) — see
//! that constant's doc for why the two want opposite errors.
//!
//! It is not the Node's answer budget
//! ([`crate::pending::downlink_answer_budget`]), which the send walk and the
//! force-send watch share (lrw_p2p_unification.cs.md §3 P2), because it
//! measures more: the answer, then the replay stream the answer starts. It
//! is never the shorter of the two for a Node that reports at least every
//! minute (three ticks against two and a minute). They part only for an
//! unknown cadence, where §16.1 keeps 300 s here and the answer budget
//! assumes the firmware's 900 s.
//!
//! Both periodic passes also sync §6.3's dead-history fault into the store
//! ([`BackfillService::sync_history_faults`]), which is where the Node row
//! and the overview strip read it from (§19 F12, #64).
//!
//! - **SSE nudges**: every job mutation (create/cancel/status/refusal) and
//!   every `NodeHistoryProgress` frame broadcasts a [`BackfillNudge`];
//!   `sse.rs` re-fetches `jobs_for` and re-renders
//!   `partials/backfill_jobs.html` as SSE event `backfill-jobs-{node_key}`.
//!
//! # Storage
//!
//! Owns SQLite connection #5 on the shared `data_dir`/`db_key` (after
//! persist, auth, read-db, keys) — another connection under WAL is fine,
//! same reasoning as `auth::AuthService::open`. Access goes through
//! `crate::db::Db`: the sync `Storage` call runs on a blocking-pool thread,
//! holding the mutex only for the closure's duration.
//!
//! # Testability
//!
//! [`crate::relay::Publisher`] is the seam, same as `CommandRelay`:
//! production wires `MqttHandle`, tests wire a capturing double so the
//! drain/cool-off/reconciler assertions below need no live broker.
//!
//! The route handlers that call `request`/`cancel`/`jobs_for` are a
//! parallel work package — suppressed `dead_code` like `relay.rs` until
//! they land.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use portal_storage::{
    BackfillJob, NewAuditEntry, NewBackfillJob, SpillGate, Storage, StorageError, JOB_PENDING,
    JOB_RECEIVING, JOB_REQUESTED,
};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;

use crate::alarms::notifier::NotifyJob;
use crate::keys::KeyService;
use crate::notify::offline::{DeviceScope, Transition};
use crate::notify::{SystemAlertEvent, SystemAlertKind};
use crate::offline::HubTransition;
use crate::relay::CommandRelay;
use crate::state::{ChangeEvent, StateStore};

/// Terminal device answers on `…/{dev_eui}/command` for
/// `get_sticker_history` — the window is permanently unserveable
/// (ws.md §`backfill_job`).
const TERMINAL_RESULTS: [&str; 5] = [
    "history_unavailable",
    "out_of_range",
    "not_supported",
    "unsupported_field",
    "bad_request",
];

/// OK device answers — the request was accepted, frames will follow.
const OK_RESULTS: [&str; 3] = ["ok", "requested", "accepted"];

/// The firmware's `Response.Error.Code`, by number, for a refusal that arrives on the v2
/// `command` family: that carries the code, not v1's name (p2p_history_replay.md §14).
/// Only the names [`TERMINAL_RESULTS`] acts on matter; the rest are named for the log.
fn error_code_name(code: u64) -> &'static str {
    match code {
        1 => "bad_request",
        2 => "out_of_range",
        3 => "not_ready",
        4 => "history_unavailable",
        5 => "unsupported_field",
        6 => "persist_failed",
        7 => "not_supported",
        8 => "not_writable",
        _ => "unknown",
    }
}

/// Where a v2 Hub's history request goes: the in-process `control.radio` seam's
/// `node-send cmd history-replay` (p2p_history_replay.md §14). A trait so the drain can be
/// tested without a ChirpStack; production wires [`crate::radio::RadioControl`].
pub trait HistorySeam: Send + Sync {
    fn history_replay<'a>(
        &'a self,
        dev_eui: &'a str,
        from: i64,
        to: i64,
    ) -> BoxFuture<'a, Result<Value, crate::radio::RadioError>>;

    /// Takes requests that are still waiting, undelivered, out of the Hub's downlink queue, and
    /// says how many went. A seam that cannot do that drops nothing.
    fn drop_queued<'a>(
        &'a self,
        _dev_eui: &'a str,
        _seqs: &'a [u64],
    ) -> BoxFuture<'a, Result<usize, crate::radio::RadioError>> {
        Box::pin(async { Ok(0) })
    }
}

impl HistorySeam for crate::radio::RadioControl {
    fn drop_queued<'a>(
        &'a self,
        dev_eui: &'a str,
        seqs: &'a [u64],
    ) -> BoxFuture<'a, Result<usize, crate::radio::RadioError>> {
        Box::pin(crate::radio::RadioControl::drop_queued(self, dev_eui, seqs))
    }

    fn history_replay<'a>(
        &'a self,
        dev_eui: &'a str,
        from: i64,
        to: i64,
    ) -> BoxFuture<'a, Result<Value, crate::radio::RadioError>> {
        Box::pin(crate::radio::RadioControl::history_replay(
            self, dev_eui, from, to,
        ))
    }
}

/// Upper bound on auto jobs created per node per reconnect — keeps a
/// pathological gap-riddled node from flooding the queue ("bounded" per the
/// work package). The most recent gaps win.
const MAX_AUTO_JOBS_PER_NODE: usize = 16;

/// The periodic gap scan skips a Node silent for longer than this many `interval_report`s,
/// the offline threshold's K (#131).
const OFFLINE_TICKS: i64 = 4;

/// Nudge channel capacity — same reasoning as `StateStore`'s ChangeEvent
/// channel: a lagging SSE subscriber just re-renders on the next nudge.
const NUDGE_CHANNEL_CAPACITY: usize = 256;

/// Whether `delta` seconds between two consecutive reports is a gap: more than 1.5 x the
/// report interval (mqtt/design.md §6, amendment 2026-10-28, #115). One lost report leaves
/// a delta of 2 x (+- jitter), which the earlier strict `> 2 x` rule missed; 1.5 x still
/// absorbs a report's normal jitter.
fn is_gap(delta: i64, interval: i64) -> bool {
    2 * delta > 3 * interval.max(1)
}

/// Fallback `application.interval_report` when the Node's cached config
/// doesn't carry one (fiber-protocol `sticker_config` default).
const DEFAULT_INTERVAL_REPORT_S: i64 = 300;

/// `device_meta.backfill_auto = 0` blocks the automatic path only; a manual
/// job from the detail page always runs (§6.4, V9's header).
const AUTO_TRIGGER: &str = "auto-on-reconnect";

/// `audit_log.username` for rows this service writes on its own initiative,
/// with `user_id` NULL beside it (devices.backend.md §19, F11). The
/// `system:{subsystem}` shape is the convention, not the string: MB3's
/// liveness writer takes `system:offline`.
const SYSTEM_ACTOR: &str = "system:backfill";

/// Upper bound on gap rows rendered under the jobs table — the same bound
/// [`MAX_AUTO_JOBS_PER_NODE`] puts on the jobs those gaps would produce, so
/// the list never promises more than the queue can act on.
const MAX_RENDERED_GAPS: usize = MAX_AUTO_JOBS_PER_NODE;

/// Tuning knobs, all defaulted per the M5-F work package. A single struct
/// (not `PortalConfig` fields) because nothing outside this service reads
/// them, and tests override them wholesale.
#[derive(Debug, Clone)]
pub struct BackfillConfig {
    /// Cool-off between downlinks to one `(hostname, node_key)`, measured
    /// from the last *actually sent* publish.
    pub min_request_interval_s: u64,
    /// Drain-pass cadence.
    pub drain_period_s: u64,
    /// Reconciler cadence.
    pub reconciler_period_s: u64,
    /// How many of the Node's own `interval_report` ticks must pass after a
    /// downlink before the reconciler re-sends it (§6.2: "wait for 3
    /// incoming `interval_report` ticks, then re-send"). A knob rather than
    /// a constant so a test can collapse the wait without pretending a Node
    /// reports every second.
    pub retry_cadence_ticks: i64,
    /// Stalled jobs already retried this many times fail with
    /// "retries exhausted". **2, not 3**: §6.2 and devices.node.md §3.9
    /// both budget "3 attempts total", and the drain's own publish is
    /// attempt one — so two re-sends, three downlinks, then the §6.3
    /// alert. M5-F shipped 3 here, which was four downlinks against a
    /// spec that asks for three.
    pub max_retries: i64,
    /// Each auto (gap) job's window is clamped to at most this many hours,
    /// keeping the newest part of the gap.
    pub auto_backfill_max_window_h: i64,
    /// Manual request windows may span at most this many days (rest.md
    /// `backfill_max_window_days`); doubles as the gap detector's default
    /// look-back for auto backfill.
    pub backfill_max_window_days: i64,
    /// The least time between two automatic requests to one Node, so a Node that flaps
    /// offline/online on a weak link asks at most this often, one span per request
    /// (devices.node.md §3.9, amendment 2026-09-25, item 5).
    pub auto_min_interval_s: i64,
    /// How many of the Node's `interval_report` ticks a recovery waits before its automatic
    /// download is due, so the link shows it holds first (item 5). A knob for the same reason
    /// `retry_cadence_ticks` is one.
    pub auto_settle_ticks: i64,
    /// `nodes.history_auto_requests_per_10min` (item 6): the most automatic requests to one
    /// Hub in any 10 minutes. Manual ones count toward it and are never held back.
    pub auto_max_per_10min: u32,
    /// Every this many of a Node's `interval_report` ticks the stored-sample gap search runs
    /// for it even without a recovery: a Node that rejoins before the offline sweep fires
    /// never produces one (#131).
    pub gap_scan_ticks: i64,
    /// Automatic backfill only considers holes that end within this many hours (#131).
    pub gap_scan_horizon_h: i64,
}

impl Default for BackfillConfig {
    fn default() -> Self {
        Self {
            min_request_interval_s: 30,
            drain_period_s: 5,
            reconciler_period_s: 60,
            retry_cadence_ticks: 3,
            max_retries: 2,
            auto_backfill_max_window_h: 24,
            backfill_max_window_days: 30,
            auto_min_interval_s: 3_600,
            auto_settle_ticks: 2,
            auto_max_per_10min: 6,
            gap_scan_ticks: 5,
            gap_scan_horizon_h: 24,
        }
    }
}

/// Why a backfill request was rejected — the route layer maps these onto
/// 422 (window) / 409 (overlap) responses (rest.md `request-history`).
#[derive(Debug, thiserror::Error)]
pub enum BackfillError {
    #[error("time_to must be after time_from")]
    EmptyWindow,
    #[error("window must lie entirely in the past")]
    FutureWindow,
    #[error("window exceeds the {0}-day maximum")]
    WindowTooWide(i64),
    #[error("an active backfill job already overlaps this window")]
    Overlap,
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// "This node's jobs table is stale — re-render it." Broadcast on every job
/// mutation and every history-progress frame; carries no job data because
/// the SSE layer re-reads `jobs_for` (same coarse-signal philosophy as
/// [`ChangeEvent`]).
#[derive(Debug, Clone)]
pub struct BackfillNudge {
    pub hostname: String,
    pub node_key: String,
}

/// The backfill service. Cheap to clone (`Arc`-backed throughout) — the
/// same pattern `AuthService`/`CommandRelay` use for cloning into
/// `AppState` per request.
#[derive(Clone)]
pub struct BackfillService {
    db: crate::db::Db,
    /// The one chokepoint every command leaves the portal through
    /// (plan/portal/mqtt/design.md §7). This service used to hold a raw
    /// `Publisher` and format its own topic — one of the two registry
    /// bypasses §5.4 closes.
    relay: CommandRelay,
    /// The signing identity for a request with no user behind it: the
    /// `portal-service` key, minted per hostname on first use
    /// (`KeyService::ensure_service_key`, design.md §5.4). `None` only in a
    /// bare test service, where the request is refused rather than sent.
    keys: Option<KeyService>,
    cfg: Arc<BackfillConfig>,
    /// `(hostname, node_key)` -> when the last downlink was actually sent —
    /// the drain cool-off. In-memory only (a restart just allows one
    /// immediate request, harmless); never pruned, but one entry per Node
    /// ever seen is noise at this fleet size.
    last_publish: Arc<parking_lot::Mutex<HashMap<(String, String), Instant>>>,
    nudge_tx: broadcast::Sender<BackfillNudge>,
    /// Where the "cannot download history" alert goes (§6.3). `None` in
    /// harnesses that wire no notifier — the same fail-quiet stance
    /// `OfflineService::announce` takes on a dropped receiver.
    notify_tx: Option<mpsc::UnboundedSender<NotifyJob>>,
    /// The spill journal, for [`Self::audit_system`]. The job bookkeeping on
    /// this connection is idempotent housekeeping and only retries; audit
    /// rows are evidence and are never pruned, so a write refused during a
    /// rewrite is a lost record rather than a delayed one.
    spill: Arc<SpillGate>,
    /// The v2 radio, when this Hub has one: history requests go through it instead of the
    /// relay, whose `get_sticker_history` the v2 agent does not implement (N35). `None` in
    /// harnesses; a Hub without a rendered radio config answers `NotConfigured` and the
    /// request falls back to the relay.
    radio: Option<Arc<dyn HistorySeam>>,
    /// `(hostname, node_key)` -> the `seq` the last history request went out under on the
    /// radio, so a refusal for that `seq` fails the job it answers. In-memory: after a restart
    /// a refusal goes unmatched and the reconciler's retry covers it, as for a missed event.
    radio_seqs: Arc<parking_lot::Mutex<HashMap<(String, String), u64>>>,
    /// Job id -> the `seq` of that job's last request on the radio, so a retry can take the
    /// earlier one out of the Hub's queue first. In-memory, one number per job: after a restart
    /// a retry simply queues, as before.
    job_seqs: Arc<parking_lot::Mutex<HashMap<i64, u64>>>,
    /// `(hostname, node_key)` -> the unix second a recovery's automatic download is due at
    /// (item 5's deferral). In-memory: after a restart the next recovery schedules it again,
    /// and the spacing is read from the jobs table, so it survives.
    auto_due: Arc<parking_lot::Mutex<HashMap<(String, String), i64>>>,
    /// (hostname, node_key) -> when the periodic gap search last ran for the Node (#131).
    last_scan: Arc<parking_lot::Mutex<HashMap<(String, String), i64>>>,
    /// (hostname, node_key) -> the Node's `last_seen` (unix s) at the previous live uplink,
    /// so a reading that follows a gap schedules the automatic download (#115). In-memory:
    /// the first uplink after a restart only starts the record.
    last_uplink: Arc<parking_lot::Mutex<HashMap<(String, String), i64>>>,
    /// Every Hub spelling the store knows, refreshed each drain tick. A Node's readings land
    /// under the Hub that heard it, so a roaming Node's samples are split across Hubs and the
    /// gap search has to read them all (#131, HIL !174).
    known_hosts: Arc<parking_lot::Mutex<Vec<String>>>,
    /// Lowercase hostname -> when each history request of the last 10 minutes went out, for
    /// item 6's per-Hub limit. In-memory: a restart only forgets up to ten minutes of it.
    sent_window: Arc<parking_lot::Mutex<HashMap<String, std::collections::VecDeque<Instant>>>>,
    /// Job id -> (failed radio-seam attempts in a row, when the next one may go). A seam that
    /// refuses (ChirpStack down, a transport the Hub does not run) was asked again on every
    /// drain tick, every 5 s for as long as it lasted (P2P-N12, bench 2026-09-26). In-memory:
    /// a restart retries at once, which is what a restart is for.
    seam_backoff: Arc<parking_lot::Mutex<HashMap<i64, (u32, Instant)>>>,
}

/// The first wait after a refused radio-seam request; it doubles per refusal up to
/// [`SEAM_BACKOFF_MAX`].
const SEAM_BACKOFF_FIRST: Duration = Duration::from_secs(30);
const SEAM_BACKOFF_MAX: Duration = Duration::from_secs(600);

fn seam_backoff_delay(failures: u32) -> Duration {
    SEAM_BACKOFF_FIRST
        .saturating_mul(1u32 << failures.saturating_sub(1).min(8))
        .min(SEAM_BACKOFF_MAX)
}

/// The window item 6's limit counts over.
const AUTO_LIMIT_WINDOW: Duration = Duration::from_secs(600);

impl BackfillService {
    /// Opens (creating if needed) `<data_dir>/portal.db` — the same path
    /// every other service struct uses — as this service's own connection.
    pub fn open(
        data_dir: &str,
        db_key: Option<&str>,
        relay: CommandRelay,
        cfg: BackfillConfig,
        spill: Arc<SpillGate>,
    ) -> Result<Self, StorageError> {
        std::fs::create_dir_all(data_dir)?;
        let path = Path::new(data_dir).join("portal.db");
        let storage = Storage::open(&path, db_key)?;
        let (nudge_tx, _rx) = broadcast::channel(NUDGE_CHANNEL_CAPACITY);
        Ok(Self {
            db: crate::db::Db::from_storage(storage, "backfill"),
            relay,
            keys: None,
            cfg: Arc::new(cfg),
            last_publish: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            nudge_tx,
            notify_tx: None,
            spill,
            radio: None,
            radio_seqs: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            job_seqs: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            auto_due: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            last_scan: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            last_uplink: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            known_hosts: Arc::new(parking_lot::Mutex::new(Vec::new())),
            sent_window: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            seam_backoff: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        })
    }

    /// Wires the notifier used for the §6.3 terminal alert. Separate from
    /// [`Self::open`] because the notifier channel is created later in
    /// `main` than the storage opens, and because every test but the two
    /// that assert on the alert wants a service without one.
    pub fn with_notifier(mut self, notify_tx: mpsc::UnboundedSender<NotifyJob>) -> Self {
        self.notify_tx = Some(notify_tx);
        self
    }

    /// Wires the key service the `portal-service` signer is minted from
    /// (design.md §5.4). Separate from [`Self::open`] for the same reason
    /// [`Self::with_notifier`] is: `main` opens the key store after this
    /// one, and every test but the signing ones wants a service without it.
    pub fn with_keys(mut self, keys: KeyService) -> Self {
        self.keys = Some(keys);
        self
    }

    /// Wires the v2 radio seam history requests go through (p2p_history_replay.md §14).
    pub fn with_radio(mut self, radio: Arc<dyn HistorySeam>) -> Self {
        self.radio = Some(radio);
        self
    }

    /// Spawns the drain, watcher, transition and reconciler background
    /// tasks. Call once from `main` after the store exists; the tasks run
    /// for the process lifetime.
    ///
    /// `transitions` is [`crate::offline::OfflineService::subscribe_transitions`]
    /// — B3's auto-backfill trigger (§6.1). `None` leaves the automatic
    /// path unarmed; manual jobs are unaffected, which is what makes it
    /// safe for harnesses that build no offline sweep.
    pub fn spawn_tasks(
        &self,
        store: Arc<StateStore>,
        transitions: Option<broadcast::Receiver<HubTransition>>,
    ) {
        let drain = self.clone();
        let drain_store = Arc::clone(&store);
        tokio::spawn(async move { drain.run_drain(drain_store).await });

        let watcher = self.clone();
        let watcher_store = Arc::clone(&store);
        tokio::spawn(async move { watcher.run_watcher(watcher_store).await });

        if let Some(rx) = transitions {
            let liveness = self.clone();
            let liveness_store = Arc::clone(&store);
            tokio::spawn(async move { liveness.run_transitions(liveness_store, rx).await });
        }

        let reconciler = self.clone();
        tokio::spawn(async move { reconciler.run_reconciler(store).await });
    }

    /// SSE layer subscription point for [`BackfillNudge`]s.
    pub fn subscribe_nudges(&self) -> broadcast::Receiver<BackfillNudge> {
        self.nudge_tx.subscribe()
    }

    // ---- Public API for the route handlers ---------------------------

    /// Validates and creates a `pending` job (rest.md `request-history`
    /// semantics): `to > from`, entirely in the past, span ≤
    /// `backfill_max_window_days`, and no active job overlapping the
    /// window (→ [`BackfillError::Overlap`], the 409). `trigger` is
    /// `"manual"` from the routes; `backfill_gaps` passes
    /// `"auto-on-reconnect"`.
    pub async fn request(
        &self,
        hostname: &str,
        node_key: &str,
        from_unix: i64,
        to_unix: i64,
        trigger: &str,
    ) -> Result<BackfillJob, BackfillError> {
        if to_unix <= from_unix {
            return Err(BackfillError::EmptyWindow);
        }
        if to_unix > now_unix() {
            return Err(BackfillError::FutureWindow);
        }
        let max_span = self.cfg.backfill_max_window_days * 86_400;
        if to_unix - from_unix > max_span {
            return Err(BackfillError::WindowTooWide(
                self.cfg.backfill_max_window_days,
            ));
        }

        let hostname = hostname.to_string();
        let node_key = node_key.to_lowercase();
        let trigger = trigger.to_string();
        // Overlap check + insert under one lock hold: every job creator
        // goes through this service's connection, so two racing requests
        // serialize here rather than both passing the check.
        let created = self
            .db
            .run(move |s| {
                // The whole check-then-insert is the retried unit, not just
                // the insert: retrying the insert alone could re-run it
                // against an overlap that appeared in between.
                portal_storage::with_retry(|| {
                    if !s
                        .find_overlapping(&node_key, from_unix, to_unix)?
                        .is_empty()
                    {
                        return Ok(None);
                    }
                    s.create_job(&NewBackfillJob {
                        hostname: hostname.clone(),
                        node_key: node_key.clone(),
                        time_from: from_unix,
                        time_to: to_unix,
                        trigger: trigger.clone(),
                        samples_expected: None,
                    })
                    .map(Some)
                })
            })
            .await?;

        let job = created.ok_or(BackfillError::Overlap)?;
        self.nudge(&job.hostname, &job.node_key);
        Ok(job)
    }

    /// Cancels a job — only `pending` is freely cancellable (ws.md);
    /// returns `false` when the job is missing or in any other state (the
    /// route maps that to its 404/409).
    /// `node_key` is not decoration: job ids are sequential and global, so
    /// without it a manager on `/nodes/A` can cancel `/nodes/B`'s job by
    /// pasting its id into the path — and the route's audit row would record
    /// the cancellation against A. The same ownership rule
    /// `probe_routes::delete_event` applies to a note, applied to a job.
    /// A job that exists but belongs to someone else answers `Ok(false)`,
    /// exactly like one that does not exist: the caller renders "no such
    /// job" either way and learns nothing about B's job ids.
    pub async fn cancel(&self, node_key: &str, job_id: i64) -> Result<bool, BackfillError> {
        let node_key = node_key.to_lowercase();
        let (cancelled, job) = self
            .db
            .run(move |s| {
                portal_storage::with_retry(|| {
                    let job = s.get_job(job_id)?;
                    if job.as_ref().is_none_or(|j| j.node_key != node_key) {
                        return Ok((false, None));
                    }
                    let cancelled = s.cancel_job(job_id)?;
                    Ok((cancelled, job))
                })
            })
            .await?;
        if cancelled {
            if let Some(job) = job {
                self.nudge(&job.hostname, &job.node_key);
            }
        }
        Ok(cancelled)
    }

    /// Jobs for one Node, newest first (`limit` clamped by the repo,
    /// default 20).
    pub async fn jobs_for(
        &self,
        node_key: &str,
        limit: Option<i64>,
    ) -> Result<Vec<BackfillJob>, BackfillError> {
        let node_key = node_key.to_lowercase();
        Ok(self.db.run(move |s| s.list_jobs(&node_key, limit)).await?)
    }

    /// Scans `node_field_readings` over the last `max_age_days` and returns
    /// `(from, to)` windows where the delta between consecutive reports
    /// exceeds 1.5×`interval_report_s` ([`is_gap`], #115; v1's gap detector and the
    /// original rule used 2×; rest.md `/sensors/{node_key}/gaps`). Read-only.
    ///
    /// "A report" is a distinct timestamp across **every field this Node has
    /// stored**, not `temperature` alone. B3 hardcoded `temperature` on the
    /// reasoning that every reporting Node carries it; a Node reporting only
    /// humidity (or CO₂, or anything else) therefore returned no rows, the
    /// pairwise loop never ran, and the answer was permanently "no gaps" —
    /// so the detail page's gap list stayed empty and automatic backfill
    /// never had a reason to fire (§19, F5).
    pub async fn detect_gaps(
        &self,
        hostname: &str,
        node_key: &str,
        interval_report_s: i64,
        max_age_days: i64,
    ) -> Result<Vec<(i64, i64)>, BackfillError> {
        let interval = if interval_report_s > 0 {
            interval_report_s
        } else {
            DEFAULT_INTERVAL_REPORT_S
        };
        let now = now_unix();
        let min_ts = now - max_age_days.max(0) * 86_400;
        let mut hosts = self.known_hosts.lock().clone();
        hosts.push(hostname.to_string());
        hosts.sort();
        hosts.dedup();
        let node_key = node_key.to_lowercase();
        let mut reported_at = self
            .db
            .run(move |s| {
                let mut all = Vec::new();
                for host in &hosts {
                    all.extend(s.node_reading_timestamps(host, &node_key, min_ts, now + 1)?);
                }
                Ok(all)
            })
            .await?;
        reported_at.sort_unstable();
        reported_at.dedup();

        let mut gaps = Vec::new();
        for pair in reported_at.windows(2) {
            if is_gap(pair[1] - pair[0], interval) {
                gaps.push((pair[0], pair[1]));
            }
        }
        Ok(gaps)
    }

    /// #115: a live uplink that arrives after a gap schedules the Node's automatic download,
    /// so a Node that never went offline (a broker restart, a few lost reports) still gets
    /// its hole filled. Compares each Node's `last_seen` with the one seen at its previous
    /// uplink; the auto path dedups (one due time per Node) and rate-limits it.
    pub async fn observe_uplinks(&self, store: &StateStore, hostname: &str) {
        let Some(device) = store.snapshot_device(hostname) else {
            return;
        };
        let mut due = Vec::new();
        for (node_key, node) in &device.nodes {
            let Some(seen) = node
                .last_seen
                .as_deref()
                .and_then(|t| t.parse::<jiff::Timestamp>().ok())
                .map(|t| t.as_second())
            else {
                continue;
            };
            let key = (hostname.to_string(), node_key.to_lowercase());
            let previous = self.last_uplink.lock().insert(key, seen);
            let Some(previous) = previous else { continue };
            if is_gap(seen - previous, node_interval_s(store, hostname, node_key)) {
                due.push(node_key.to_lowercase());
            }
        }
        for node_key in due {
            tracing::info!(hostname, node_key, "uplink after a gap: auto backfill");
            self.auto_backfill_node(store, hostname, &node_key).await;
        }
    }

    /// [`Self::detect_gaps`] rendered for `partials/backfill_jobs.html`,
    /// newest first and capped at [`MAX_RENDERED_GAPS`] (§6's "missing *x*
    /// for *from–to*").
    ///
    /// Computed at render time rather than stored: a gap is a property of
    /// the readings table, and the whole point of showing it next to the
    /// jobs is that a completed backfill makes it disappear. It therefore
    /// rides the existing `backfill-jobs-{node_key}` SSE event — the same
    /// re-render that already refetches `jobs_for` — instead of adding a
    /// second event the page would have to reconcile against the first.
    ///
    /// Degrades to an empty list: a detail page without its gap list is
    /// still the page an operator came for, exactly as `jobs_for` degrades.
    pub async fn gaps_ctx(
        &self,
        store: &StateStore,
        hostname: &str,
        node_key: &str,
    ) -> Vec<GapCtx> {
        let interval = node_interval_s(store, hostname, node_key);
        let gaps = match self
            .detect_gaps(
                hostname,
                node_key,
                interval,
                self.cfg.backfill_max_window_days,
            )
            .await
        {
            Ok(gaps) => gaps,
            Err(err) => {
                tracing::warn!(%err, hostname, node_key, "gap detection failed; rendering none");
                return Vec::new();
            }
        };
        // P-8: a hole the Node already answered for, or any hole while it keeps no history, has
        // nothing to download; the row says so instead of offering a futile downlink.
        let history_off = node_keeps_no_history(store, hostname, node_key);
        let mut out = Vec::new();
        for (from, to) in gaps.into_iter().rev().take(MAX_RENDERED_GAPS) {
            let unavailable = if history_off {
                Some("history_off")
            } else {
                let node = node_key.to_string();
                let answered = self
                    .db
                    .run(move |s| s.answered_covering(&node, from, to))
                    .await
                    .unwrap_or(false);
                answered.then_some("answered")
            };
            out.push(GapCtx {
                time_from: format_ts_minutes(from),
                time_to: format_ts_minutes(to),
                from_unix: from,
                to_unix: to,
                duration: format_duration(to - from),
                samples_expected: missing_samples(from, to, interval),
                unavailable,
            });
        }
        out
    }

    /// Detects gaps and creates **one** `auto-on-reconnect` job for the span they cover:
    /// from the oldest hole after the last served automatic window to the newest one's end,
    /// skipping windows the Node refused, clamped to
    /// `auto_backfill_max_window_h` (keeping the newest part). One downlink rather than one per
    /// hole, because the gateway's duty cycle is what a Node's commands share (devices.node.md
    /// §3.9, amendment 2026-09-25, item 5); the Node sends back what it holds for the span and
    /// the history sink leaves out what the live readings already cover. Returns the number of
    /// jobs created, 0 or 1. Looks at the [`MAX_AUTO_JOBS_PER_NODE`] newest holes.
    pub async fn backfill_gaps(
        &self,
        hostname: &str,
        node_key: &str,
        interval_report_s: i64,
    ) -> Result<usize, BackfillError> {
        let gaps = self
            .detect_gaps(
                hostname,
                node_key,
                interval_report_s,
                self.cfg.backfill_max_window_days,
            )
            .await?;

        // The holes before the end of the last window an automatic job served were asked for
        // already; what is still a hole there, the Node does not hold.
        let node = node_key.to_lowercase();
        let horizon = self
            .db
            .run(move |s| s.served_horizon(&node, AUTO_TRIGGER))
            .await
            .unwrap_or(None);

        // #131: only holes inside the recent horizon; an old one (before the Node's history
        // began, say) must not win over the fresh gap.
        let recent = now_unix() - self.cfg.gap_scan_horizon_h * 3_600;
        let mut span: Option<(i64, i64)> = None;
        for (from, to) in gaps.into_iter().rev().take(MAX_AUTO_JOBS_PER_NODE) {
            if to <= recent {
                continue;
            }
            if horizon.is_some_and(|h| to <= h) {
                continue;
            }
            // A window the Node refused is one it holds nothing for (from before its history
            // began, say). Asking again after every recovery cost ten downlinks and ended on a
            // refusal (N37, bench 2026-09-25). The form still asks.
            let node = node_key.to_lowercase();
            if self
                .db
                .run(move |s| s.refused_covering(&node, from, to))
                .await
                .unwrap_or(false)
            {
                tracing::info!(
                    hostname,
                    node_key,
                    from,
                    to,
                    "auto backfill skips a window the Node refused before"
                );
                continue;
            }
            span = Some(match span {
                None => (from, to),
                Some((f, t)) => (f.min(from), t.max(to)),
            });
        }
        let Some((from, to)) = span else {
            return Ok(0);
        };
        let from = from.max(to - self.cfg.auto_backfill_max_window_h * 3_600);

        match self
            .request(hostname, node_key, from, to, "auto-on-reconnect")
            .await
        {
            Ok(job) => {
                tracing::info!(
                    hostname,
                    node_key,
                    job_id = job.job_id,
                    from,
                    to,
                    "auto backfill job created"
                );
                Ok(1)
            }
            // Already queued/being served — exactly what the overlap check is for on repeated
            // reconnects.
            Err(BackfillError::Overlap) => Ok(0),
            // Validation failures on a detected span mean clock skew or a degenerate clamp.
            Err(BackfillError::EmptyWindow)
            | Err(BackfillError::FutureWindow)
            | Err(BackfillError::WindowTooWide(_)) => Ok(0),
            Err(err @ BackfillError::Storage(_)) => Err(err),
        }
    }

    // ---- Drain --------------------------------------------------------

    /// One drain pass over `hostnames`: per `(hostname, node_key)` with no
    /// job already in requested/receiving, publish the oldest pending job
    /// (one downlink per node per pass), honoring the cool-off since this
    /// service's last successful publish to that node.
    pub async fn drain_pass(&self, store: &StateStore, hostnames: &[String]) {
        self.run_due_auto(store, now_unix()).await;
        for hostname in hostnames {
            self.sync_history_faults(store, hostname).await;
            let host = hostname.clone();
            let jobs = match self.db.run(move |s| s.active_jobs(&host)).await {
                Ok(jobs) => jobs,
                Err(err) => {
                    tracing::warn!(%err, hostname, "drain: active_jobs failed");
                    continue;
                }
            };

            // active_jobs is job_id ASC, so per-node vectors stay oldest
            // first.
            let mut by_node: BTreeMap<String, Vec<BackfillJob>> = BTreeMap::new();
            for job in jobs {
                by_node.entry(job.node_key.clone()).or_default().push(job);
            }

            for (node_key, node_jobs) in by_node {
                let in_flight = node_jobs
                    .iter()
                    .any(|j| j.status == JOB_REQUESTED || j.status == JOB_RECEIVING);
                if in_flight {
                    continue;
                }
                let Some(job) = node_jobs.into_iter().find(|j| j.status == JOB_PENDING) else {
                    continue;
                };
                if !self.cooloff_elapsed(hostname, &node_key) {
                    continue;
                }
                if self.publish_history_request(store, &job).await {
                    self.set_status(job.job_id, JOB_REQUESTED, None, false)
                        .await;
                    self.nudge(&job.hostname, &job.node_key);
                }
            }
        }
    }

    async fn run_drain(self, store: Arc<StateStore>) {
        let mut ticker = tokio::time::interval(Duration::from_secs(self.cfg.drain_period_s));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            // Hostnames come off the live store: a job whose device hasn't
            // (re)announced itself yet just waits for a later pass —
            // publishing to a host the broker has never seen buys nothing.
            let hostnames = pass_hostnames(&store);
            self.periodic_scan(&store, now_unix()).await;
            self.drain_pass(&store, &hostnames).await;
        }
    }

    /// #131: runs the gap search for every known Node of an online Hub once per
    /// `gap_scan_ticks` x `interval_report`, not only on a recovery. The first sight of a
    /// Node only starts its clock, so a portal start does not ask every Node at once.
    pub async fn periodic_scan(&self, store: &StateStore, now: i64) {
        *self.known_hosts.lock() = pass_hostnames(store);
        let mut due = Vec::new();
        for hostname in pass_hostnames(store) {
            let Some(device) = store.snapshot_device(&hostname) else {
                continue;
            };
            if !device.online {
                continue;
            }
            for (node_key, node) in &device.nodes {
                let interval = node_interval_s(store, &hostname, node_key);
                // A Node not heard within the offline threshold has nothing new to fill and
                // would be asked again every period (HIL !174: a Node off air for days).
                let heard = node
                    .last_seen
                    .as_deref()
                    .and_then(|t| t.parse::<jiff::Timestamp>().ok())
                    .map(|t| t.as_second());
                if heard.is_none_or(|t| now - t > OFFLINE_TICKS * interval) {
                    continue;
                }
                let key = (hostname.clone(), node_key.to_lowercase());
                let mut map = self.last_scan.lock();
                let last = *map.entry(key.clone()).or_insert(now);
                if now - last >= self.cfg.gap_scan_ticks * interval {
                    map.insert(key, now);
                    due.push((hostname.clone(), node_key.to_lowercase()));
                }
            }
        }
        for (hostname, node_key) in due {
            self.auto_backfill_node(store, &hostname, &node_key).await;
        }
    }

    // ---- Watcher -------------------------------------------------------

    async fn run_watcher(self, store: Arc<StateStore>) {
        // Subscribed here, then dropped: the watcher only ever reads the
        // broadcast, and holding an `Arc<StateStore>` it never queries would
        // just outlive the store for no reason.
        let mut changes = store.subscribe();
        loop {
            match changes.recv().await {
                Ok(change) => self.handle_change(&store, change).await,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // A missed NodeCommandResult refusal is recovered by
                    // the reconciler (the job stalls, retries, and either
                    // succeeds or exhausts) — log and keep consuming.
                    tracing::warn!(missed = n, "backfill watcher lagged on ChangeEvents");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }

    async fn handle_change(&self, store: &StateStore, change: ChangeEvent) {
        match change {
            // #131: a Hello (join/boot) follows a silence the offline sweep may never have
            // called an outage; look for the hole it left.
            ChangeEvent::NodeJoined {
                hostname, node_key, ..
            } => {
                self.auto_backfill_node(store, &hostname, &node_key.to_lowercase())
                    .await;
            }
            ChangeEvent::NodeBatch { hostname } => {
                self.observe_uplinks(store, &hostname).await;
            }
            ChangeEvent::NodeCommandResult {
                hostname,
                node_key,
                payload,
            } => {
                self.handle_command_result(&hostname, &node_key, &payload)
                    .await;
            }
            ChangeEvent::NodeHistoryProgress {
                hostname, node_key, ..
            } => {
                // Crediting/done-marking already happened on the persist
                // thread — this is purely the "re-render the table" signal.
                self.nudge(&hostname, &node_key);
            }
            _ => {}
        }
    }

    /// Learns refusals from `…/{dev_eui}/command` results (mqtt.md: "the
    /// only way a refused history request is learned"). Terminal answers
    /// fail the node's in-flight (requested/receiving) job with
    /// `device_refused`; OK answers are no-ops (frames will follow);
    /// anything else is logged and ignored.
    pub async fn handle_command_result(&self, hostname: &str, node_key: &str, payload: &Value) {
        if payload.get("transport").and_then(Value::as_str) == Some("control.radio") {
            self.handle_radio_result(hostname, node_key, payload).await;
            return;
        }
        if payload.get("command").and_then(Value::as_str) != Some("get_sticker_history") {
            return;
        }
        let Some(result) = payload.get("result").and_then(Value::as_str) else {
            return;
        };
        if OK_RESULTS.contains(&result) {
            return;
        }
        if !TERMINAL_RESULTS.contains(&result) {
            tracing::debug!(
                hostname,
                node_key,
                result,
                "unrecognized get_sticker_history result — ignoring"
            );
            return;
        }

        self.refuse_in_flight(
            hostname,
            node_key,
            result,
            payload.get("detail").and_then(Value::as_str),
        )
        .await;
    }

    /// A v2 answer on the `command` family (p2p_history_replay.md §14). It names no command,
    /// only the `seq` it answers, so it is a history refusal when that is the `seq` this
    /// service's last request to the Node went out under; the code says whether it is final.
    async fn handle_radio_result(&self, hostname: &str, node_key: &str, payload: &Value) {
        if payload.get("result").and_then(Value::as_str) != Some("error") {
            return;
        }
        let seq = payload.get("seq").and_then(Value::as_u64);
        let key = (hostname.to_string(), node_key.to_lowercase());
        if seq.is_none() || self.radio_seqs.lock().get(&key).copied() != seq {
            return;
        }
        let result = error_code_name(payload.get("code").and_then(Value::as_u64).unwrap_or(0));
        if !TERMINAL_RESULTS.contains(&result) {
            tracing::debug!(
                hostname,
                node_key,
                result,
                "history request answered with a non-final error — the reconciler retries"
            );
            return;
        }
        self.radio_seqs.lock().remove(&key);
        self.refuse_in_flight(
            hostname,
            node_key,
            result,
            payload.get("detail").and_then(Value::as_str),
        )
        .await;
    }

    /// Fails the Node's in-flight (requested/receiving) job as refused by the device.
    async fn refuse_in_flight(
        &self,
        hostname: &str,
        node_key: &str,
        result: &str,
        detail: Option<&str>,
    ) {
        let host = hostname.to_string();
        let node = node_key.to_lowercase();
        let in_flight = match self.db.run(move |s| s.active_jobs(&host)).await {
            Ok(jobs) => jobs.into_iter().find(|j| {
                j.node_key == node && (j.status == JOB_REQUESTED || j.status == JOB_RECEIVING)
            }),
            Err(err) => {
                tracing::warn!(%err, hostname, "refusal: active_jobs failed");
                return;
            }
        };
        let Some(job) = in_flight else {
            tracing::debug!(
                hostname,
                node_key,
                result,
                "refusal without an in-flight job — stray"
            );
            return;
        };

        let mut message = format!("device refused: {result}");
        if let Some(detail) = detail {
            message.push_str(" — ");
            message.push_str(detail);
        }
        tracing::info!(
            job_id = job.job_id,
            hostname,
            node_key,
            from = job.time_from,
            to = job.time_to,
            result,
            "history backfill refused by the device"
        );
        self.set_status(job.job_id, "failed", Some(message), true)
            .await;
        self.nudge(hostname, node_key);
    }

    // ---- Liveness transitions (B3, §6.1) --------------------------------

    async fn run_transitions(
        self,
        store: Arc<StateStore>,
        mut transitions: broadcast::Receiver<HubTransition>,
    ) {
        loop {
            match transitions.recv().await {
                Ok(t) => self.handle_transition(&store, t).await,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    // A missed recovery costs one deferred auto-backfill:
                    // the gaps are still there on the next transition, and
                    // the operator can always request the window by hand.
                    // Nothing here is recoverable by replay, so log and
                    // keep consuming.
                    tracing::warn!(missed = n, "backfill missed offline transitions");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }

    /// One Node's liveness edge (§6.1). A recovery arms the automatic
    /// backfill for **that** Node; a fresh onset cancels whatever the last
    /// recovery queued, so the procedure restarts from freshly detected
    /// gaps rather than draining a window that grew while it waited.
    ///
    /// Beacons are ignored: they have no `get_sticker_history` and their
    /// archive backfill is `control/ble`'s (§12.1).
    pub async fn handle_transition(&self, store: &StateStore, t: HubTransition) {
        tracing::debug!(
            hostname = %t.hostname,
            entity = %t.transition.entity,
            scope = ?t.transition.scope,
            transition = ?t.transition.transition,
            "backfill saw a liveness transition"
        );
        if t.transition.scope != DeviceScope::Node {
            return;
        }
        let node_key = t.transition.entity.to_lowercase();
        match t.transition.transition {
            Transition::CameBack => self.auto_backfill_node(store, &t.hostname, &node_key).await,
            Transition::WentOffline => self.cancel_auto_jobs(&t.hostname, &node_key).await,
        }
    }

    /// Gap-backfill one Node, honouring its `device_meta.backfill_auto`
    /// (§6.4). The toggle blocks this path only — [`Self::request`], which
    /// the detail page's form calls, never consults it.
    /// Decision #18 (2026-09-26): history comes only from a paired Hub, under any of the
    /// spellings [`KeyService::is_paired`] tries (NT-4). Without a key service (a portal or a
    /// test wired without one) there is nothing to ask and the check passes.
    pub async fn hub_paired(&self, hostname: &str) -> bool {
        match &self.keys {
            Some(keys) => keys.is_paired(hostname).await,
            None => true,
        }
    }

    async fn auto_backfill_node(&self, store: &StateStore, hostname: &str, node_key: &str) {
        if !self.hub_paired(hostname).await {
            tracing::debug!(
                hostname,
                node_key,
                "auto backfill skipped: the Hub is not paired"
            );
            return;
        }
        if !self.auto_backfill_enabled(hostname, node_key).await {
            tracing::debug!(hostname, node_key, "auto backfill disabled for this Node");
            return;
        }
        // A Node that reports it keeps no history would answer HISTORY_UNAVAILABLE to every
        // window, and each refusal would count toward the "cannot download history" fault
        // (devices.node.md §3.9, amendment 2026-09-25; P-8).
        if node_keeps_no_history(store, hostname, node_key) {
            tracing::debug!(
                hostname,
                node_key,
                "auto backfill skipped: the Node keeps no history"
            );
            return;
        }
        self.schedule_auto(store, hostname, node_key, now_unix())
            .await;
    }

    /// Item 5's deferral: a recovery only schedules the download, for 2 x `interval_report`
    /// after the return (the link shows it holds) and no sooner than `auto_min_interval_s`
    /// after the Node's previous automatic request. A due time already set stays: a Node that
    /// flaps does not push its download further out with every return.
    async fn schedule_auto(&self, store: &StateStore, hostname: &str, node_key: &str, now: i64) {
        let interval = node_interval_s(store, hostname, node_key);
        let node = node_key.to_lowercase();
        let last = self
            .db
            .run(move |s| s.last_requested_at(&node, AUTO_TRIGGER))
            .await
            .unwrap_or(None);
        let due = (now + self.cfg.auto_settle_ticks * interval)
            .max(last.map_or(0, |l| l + self.cfg.auto_min_interval_s));
        let key = (hostname.to_string(), node_key.to_lowercase());
        let due = *self.auto_due.lock().entry(key).or_insert(due);
        tracing::info!(hostname, node_key, due, "auto backfill scheduled");
    }

    /// Runs every scheduled automatic download that is due at `now`: one job for the span of
    /// the Node's holes. Called from each drain pass.
    pub async fn run_due_auto(&self, store: &StateStore, now: i64) {
        let due: Vec<(String, String)> = {
            let mut map = self.auto_due.lock();
            let keys: Vec<_> = map
                .iter()
                .filter(|(_, at)| **at <= now)
                .map(|(k, _)| k.clone())
                .collect();
            for k in &keys {
                map.remove(k);
            }
            keys
        };
        for (hostname, node_key) in due {
            // Both checked again: the switch or the Node's config can have changed since.
            if !self.auto_backfill_enabled(&hostname, &node_key).await
                || node_keeps_no_history(store, &hostname, &node_key)
            {
                continue;
            }
            self.run_auto(store, &hostname, &node_key).await;
        }
    }

    async fn run_auto(&self, store: &StateStore, hostname: &str, node_key: &str) {
        let interval = node_interval_s(store, hostname, node_key);
        match self.backfill_gaps(hostname, node_key, interval).await {
            // Nothing was created, so no privileged action happened and
            // there is nothing to record: an audit trail of "the service
            // looked and found no gap" is noise an operator would learn to
            // scroll past.
            Ok(0) => {}
            Ok(created) => {
                tracing::info!(
                    hostname,
                    node_key,
                    created,
                    "auto backfill jobs created on Node recovery"
                );
                self.audit_system(
                    "node_backfill_auto",
                    node_key,
                    serde_json::json!({
                        "hostname": hostname,
                        "jobs": created,
                        "trigger": AUTO_TRIGGER,
                    }),
                    true,
                )
                .await;
            }
            Err(err) => {
                tracing::warn!(%err, hostname, node_key, "auto backfill on Node recovery failed");
                self.audit_system(
                    "node_backfill_auto",
                    node_key,
                    serde_json::json!({
                        "hostname": hostname,
                        "trigger": AUTO_TRIGGER,
                        "error": err.to_string(),
                    }),
                    false,
                )
                .await;
            }
        }
    }

    /// One `audit_log` row for something **the portal did by itself**
    /// (devices.node.md §3.9; devices.backend.md §19, F11).
    ///
    /// Not [`crate::routes::audit_action`]: that one takes a `CurrentUser`,
    /// and there is no user here — a Node reconnecting is what triggered
    /// this. The convention this establishes, and which MB3's liveness
    /// writer (`system:offline`) reuses: `username` carries a
    /// `system:{subsystem}` string and `user_id` is NULL, so an operator
    /// reading the audit page can see at a glance that no human is behind
    /// the row and no schema column had to be invented for it. B4's send
    /// walk is *not* one of these — it keeps the human who pressed Send.
    ///
    /// Fire-and-forget like every other audit write: a failure is logged and
    /// never propagated, so it cannot undo the jobs it describes.
    async fn audit_system(
        &self,
        event_type: &'static str,
        target: &str,
        details: serde_json::Value,
        success: bool,
    ) {
        let entry = NewAuditEntry {
            event_type: event_type.to_string(),
            username: Some(SYSTEM_ACTOR.to_string()),
            user_id: None,
            ip: None,
            target: Some(target.to_string()),
            details: serde_json::to_string(&details).ok(),
            success,
        };
        let spill = Arc::clone(&self.spill);
        if let Err(err) = self
            .db
            .run(move |s| crate::durable::insert_audit(&spill, s, &entry))
            .await
        {
            tracing::error!(%err, event_type, "system audit entry could not be written or spilled");
        }
    }

    /// `device_meta.backfill_auto` for one Node. Missing row = enabled
    /// (V9's `NOT NULL DEFAULT 1`, and "no row" means "every default"); an
    /// unreadable table also means enabled, because failing *open* here
    /// costs one downlink while failing closed silently loses history.
    async fn auto_backfill_enabled(&self, hostname: &str, node_key: &str) -> bool {
        let host = hostname.to_string();
        let node = node_key.to_string();
        match self
            .db
            .run(move |s| s.get_device_meta(&host, portal_storage::THRESHOLD_SCOPE_NODE, &node))
            .await
        {
            Ok(Some(meta)) => meta.backfill_auto,
            Ok(None) => true,
            Err(err) => {
                tracing::warn!(%err, hostname, node_key, "device_meta unreadable; assuming backfill_auto");
                true
            }
        }
    }

    /// Cancels this Node's active **auto** jobs — the "a new offline during
    /// a running procedure cancels and restarts it" half of §6.1.
    ///
    /// Manual jobs are left alone deliberately: an operator who asked for a
    /// window asked for it, and the drain will still be holding it when the
    /// Node returns. This goes through `update_job_status` rather than
    /// [`Self::cancel`] because that one is pending-only by contract (ws.md)
    /// and the job being abandoned here is usually mid-flight.
    async fn cancel_auto_jobs(&self, hostname: &str, node_key: &str) {
        let host = hostname.to_string();
        let node = node_key.to_string();
        let jobs = match self.db.run(move |s| s.active_jobs(&host)).await {
            Ok(jobs) => jobs,
            Err(err) => {
                tracing::warn!(%err, hostname, "offline cancel: active_jobs failed");
                return;
            }
        };
        let mut cancelled = 0usize;
        for job in jobs {
            // Only a job not sent yet: one whose request went out rides the downlink already
            // spent, and its frames may still come (item 5).
            if job.node_key != node || job.trigger != AUTO_TRIGGER || job.status != JOB_PENDING {
                continue;
            }
            self.set_status(
                job.job_id,
                "cancelled",
                Some("node went offline again".into()),
                false,
            )
            .await;
            cancelled += 1;
        }
        if cancelled > 0 {
            tracing::info!(
                hostname,
                node_key,
                cancelled,
                "auto backfill jobs cancelled — Node went offline again"
            );
            self.nudge(hostname, node_key);
        }
    }

    // ---- Reconciler ----------------------------------------------------

    /// One reconciler pass: requested/receiving jobs past their
    /// `not_before` deadline get republished (publish first, then
    /// `increment_retry` — the count only reflects downlinks actually
    /// sent), or fail with "retries exhausted" at `max_retries`, which also
    /// raises the §6.3 alert.
    ///
    /// `not_before` is `NULL` on rows written before V11 and on any job
    /// whose downlink predates this code; those fall back to
    /// `updated_at + retry_cadence_ticks x cadence`, the same rule computed
    /// from what the old rows do carry.
    pub async fn reconcile_pass(&self, store: &StateStore, hostnames: &[String]) {
        let now = now_unix();
        for hostname in hostnames {
            let host = hostname.clone();
            let jobs = match self.db.run(move |s| s.active_jobs(&host)).await {
                Ok(jobs) => jobs,
                Err(err) => {
                    tracing::warn!(%err, hostname, "reconciler: active_jobs failed");
                    continue;
                }
            };

            for job in jobs {
                if job.status != JOB_REQUESTED && job.status != JOB_RECEIVING {
                    continue;
                }
                let interval = node_interval_s(store, hostname, &job.node_key);
                let deadline = job
                    .not_before
                    .unwrap_or_else(|| job.updated_at + self.cfg.retry_cadence_ticks * interval);
                if now < deadline {
                    continue;
                }

                if job.retry_count >= self.cfg.max_retries {
                    tracing::warn!(
                        hostname,
                        node_key = %job.node_key,
                        job_id = job.job_id,
                        retries = job.retry_count,
                        "backfill job stalled with retries exhausted — failing"
                    );
                    self.set_status(
                        job.job_id,
                        "failed",
                        Some("retries exhausted".into()),
                        false,
                    )
                    .await;
                    self.alert_history_unavailable(&job);
                    self.nudge(&job.hostname, &job.node_key);
                } else if self.publish_history_request(store, &job).await {
                    let job_id = job.job_id;
                    // One statement, so the retry cannot double-count.
                    // Bumping `retry_count` and restamping the status used to
                    // be two calls inside this closure, and `with_retry`
                    // re-runs the whole closure: a `SQLITE_BUSY` on the second
                    // — which is what a maintenance rewrite produces — re-ran
                    // the non-idempotent `retry_count + 1`, so one contended
                    // pass could exhaust `max_retries` and fail the job after
                    // a single downlink.
                    let bumped = self
                        .db
                        .run(move |s| portal_storage::with_retry(|| s.record_retry_sent(job_id)))
                        .await;
                    if let Err(err) = bumped {
                        tracing::warn!(%err, job_id = job.job_id, "reconciler: retry bookkeeping failed");
                    }
                    self.nudge(&job.hostname, &job.node_key);
                }
            }
            // After the failures this pass wrote, so an exhausted job shows
            // its fault without waiting for the next drain.
            self.sync_history_faults(store, hostname).await;
        }
    }

    /// Pushes §6.3's dead-history fault into the store for one Hub, under the
    /// same latest-job rule the query defines (`nodes_with_failed_backfill`):
    /// set where the newest job for that Node ended `failed`, cleared as soon
    /// as a later job supersedes it.
    ///
    /// Called from both periodic passes rather than from each job mutation,
    /// which is what makes it a sync and not an event: the store starts empty
    /// after a restart, and a Node only appears in it once the Hub announces
    /// it, so the flag has to be (re)seeded from the table whenever a Node
    /// shows up. An unreadable jobs table leaves the last known flags
    /// standing (devices.backend.md §19 F12, #64).
    async fn sync_history_faults(&self, store: &StateStore, hostname: &str) {
        let host = hostname.to_string();
        match self
            .db
            .run(move |s| s.nodes_with_failed_backfill(&host))
            .await
        {
            Ok(failed) => store.set_history_faults(hostname, &failed.into_iter().collect()),
            Err(err) => {
                tracing::warn!(%err, hostname, "history faults: nodes_with_failed_backfill failed")
            }
        }
    }

    async fn run_reconciler(self, store: Arc<StateStore>) {
        let mut ticker = tokio::time::interval(Duration::from_secs(self.cfg.reconciler_period_s));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let hostnames = pass_hostnames(&store);
            self.reconcile_pass(&store, &hostnames).await;
        }
    }

    // ---- Internals -----------------------------------------------------

    /// Sends `get_sticker_history` for `job` through the relay and records
    /// the cool-off stamp on success.
    ///
    /// It used to be a flat-dialect publish this service formatted the
    /// topic for itself — one of the two registry bypasses design.md §5.4
    /// closes. It is a signed `read` now, so it is single-shot on the
    /// device (verified, executed, one audit row, no challenge) and the
    /// Hub's trail carries `signer_id: portal-service`.
    ///
    /// The signer is the portal's own service key, not a user's: this runs
    /// on a timer with nobody in the loop, and the key it signs with can do
    /// exactly `get_status` at `level:read` and nothing else.
    ///
    /// **`dispatch`, not `execute`.** "Sent" is all this path has ever
    /// meant (see below), so waiting for `config/response` would buy
    /// nothing and cost `CommandRelay::COMMAND_TIMEOUT` per node:
    /// `drain_pass` walks `(hostname, node)` one at a time, so an offline
    /// Hub with a handful of pending jobs would spend minutes in a single
    /// pass — and `run_drain`'s interval, which bursts the ticks it missed,
    /// would come straight back for another one.
    /// Whether an automatic request may go to `hostname` now under item 6's limit.
    fn auto_budget_left(&self, hostname: &str) -> bool {
        let mut map = self.sent_window.lock();
        let sent = map.entry(hostname.to_ascii_lowercase()).or_default();
        while sent
            .front()
            .is_some_and(|at| at.elapsed() >= AUTO_LIMIT_WINDOW)
        {
            sent.pop_front();
        }
        sent.len() < self.cfg.auto_max_per_10min as usize
    }

    fn note_sent(&self, hostname: &str) {
        self.sent_window
            .lock()
            .entry(hostname.to_ascii_lowercase())
            .or_default()
            .push_back(Instant::now());
    }

    async fn publish_history_request(&self, store: &StateStore, job: &BackfillJob) -> bool {
        if self
            .seam_backoff
            .lock()
            .get(&job.job_id)
            .is_some_and(|(_, next)| Instant::now() < *next)
        {
            return false;
        }
        // Item 6: an automatic request waits while the Hub's 10-minute budget is spent; it
        // stays where it is (pending, or requested for a retry) and goes on a later pass.
        if job.trigger == AUTO_TRIGGER && !self.auto_budget_left(&job.hostname) {
            tracing::debug!(
                job_id = job.job_id,
                hostname = %job.hostname,
                "automatic history request held back: the Hub's 10-minute budget is spent"
            );
            return false;
        }
        // A v2 Hub's radio first (p2p_history_replay.md §14): the relay's `get_sticker_history`
        // lands in an agent that has no LoRaWAN stack. The pending walk's fallback applies: a
        // Hub with no rendered radio config (v1) takes the relay below.
        if let Some(radio) = &self.radio {
            // A retry replaces its own earlier request while that one still waits in the Hub's
            // queue. On P2P a request refused `Busy` and re-announced was still queued when the
            // retry went out, and the Node then got the same window two or three times back to
            // back (bench 2026-09-27, job 138). A request already delivered stays: the Node
            // holds it, and its answer is still this job's.
            let earlier = self.job_seqs.lock().get(&job.job_id).copied();
            if let Some(earlier) = earlier {
                match radio.drop_queued(&job.node_key, &[earlier]).await {
                    Ok(0) => {}
                    Ok(dropped) => tracing::info!(
                        job_id = job.job_id,
                        node_key = %job.node_key,
                        seq = earlier,
                        dropped,
                        "history retry: the earlier request was still queued, replaced"
                    ),
                    Err(err) => tracing::debug!(
                        %err,
                        job_id = job.job_id,
                        "history retry: could not check the Hub's queue, queueing anyway"
                    ),
                }
            }
            match radio
                .history_replay(&job.node_key, job.time_from, job.time_to)
                .await
            {
                Ok(accepted) => {
                    let seq = accepted.get("seq").and_then(Value::as_u64);
                    // One line per request: the window and the seq its answer comes back
                    // under, so a refusal on the `command` topic can be read against it.
                    tracing::info!(
                        job_id = job.job_id,
                        hostname = %job.hostname,
                        node_key = %job.node_key,
                        from = job.time_from,
                        to = job.time_to,
                        seq,
                        "history requested through the radio seam"
                    );
                    if let Some(seq) = seq {
                        self.radio_seqs
                            .lock()
                            .insert((job.hostname.clone(), job.node_key.clone()), seq);
                        self.job_seqs.lock().insert(job.job_id, seq);
                    }
                    self.seam_backoff.lock().remove(&job.job_id);
                    self.mark_sent(store, job).await;
                    return true;
                }
                Err(crate::radio::RadioError::NotConfigured(_)) => {}
                Err(err) => {
                    let failures = {
                        let mut map = self.seam_backoff.lock();
                        let entry = map.entry(job.job_id).or_insert((0, Instant::now()));
                        entry.0 += 1;
                        entry.1 = Instant::now() + seam_backoff_delay(entry.0);
                        entry.0
                    };
                    tracing::warn!(
                        %err,
                        job_id = job.job_id,
                        hostname = %job.hostname,
                        failures,
                        retry_in_s = seam_backoff_delay(failures).as_secs(),
                        "history-replay did not reach the radio"
                    );
                    return false;
                }
            }
        }

        let Some(keys) = &self.keys else {
            tracing::debug!(
                job_id = job.job_id,
                "backfill: no key service wired — skipping get_sticker_history"
            );
            return false;
        };
        let key = match keys.ensure_service_key(&job.hostname).await {
            Ok(key) => key,
            Err(err) => {
                tracing::warn!(
                    %err,
                    job_id = job.job_id,
                    hostname = %job.hostname,
                    "backfill: could not mint the portal-service signing key"
                );
                return false;
            }
        };

        let outcome = self
            .relay
            .dispatch(
                crate::keys::PORTAL_SERVICE_SIGNER,
                &key,
                &job.hostname,
                "get_sticker_history",
                serde_json::json!({
                    "dev_eui": job.node_key,
                    "from": job.time_from,
                    "to": job.time_to,
                }),
                Some(format!("history backfill job {}", job.job_id)),
            )
            .await;

        // "Sent" is what this path has always meant, and now what the call
        // literally returns: the cool-off stamp and the retry deadline exist
        // because the *downlink* went out, not because the Node answered —
        // the answer arrives later as history frames. A refusal before
        // signing (`request_id: None`) or a publish failure is the only
        // thing that means nothing left the portal.
        if outcome.request_id.is_none() {
            tracing::warn!(
                job_id = job.job_id,
                hostname = %job.hostname,
                message = ?outcome.message,
                "get_sticker_history refused before signing"
            );
            return false;
        }
        if outcome.status == "error" {
            tracing::warn!(
                job_id = job.job_id,
                hostname = %job.hostname,
                message = ?outcome.message,
                "get_sticker_history did not reach the device"
            );
            return false;
        }

        self.mark_sent(store, job).await;
        true
    }

    /// A request left the portal: stamp the cool-off and arm the retry deadline.
    async fn mark_sent(&self, store: &StateStore, job: &BackfillJob) {
        self.note_sent(&job.hostname);
        self.last_publish
            .lock()
            .insert((job.hostname.clone(), job.node_key.clone()), Instant::now());
        self.arm_retry_deadline(store, job).await;
    }

    /// Arms this job's `not_before` (V11): `now + retry_cadence_ticks x` the
    /// Node's own `application.interval_report`. Called from the publish
    /// path so the wait starts at the send.
    async fn arm_retry_deadline(&self, store: &StateStore, job: &BackfillJob) {
        let interval = node_interval_s(store, &job.hostname, &job.node_key);
        let deadline = now_unix() + self.cfg.retry_cadence_ticks * interval;
        let job_id = job.job_id;
        if let Err(err) = self
            .db
            .run(move |s| portal_storage::with_retry(|| s.set_not_before(job_id, deadline)))
            .await
        {
            // The reconciler's NULL fallback covers this: the job is still
            // retried, just off `updated_at` instead of the send.
            tracing::warn!(%err, job_id, "failed to arm backfill retry deadline");
        }
    }

    /// The §6.3 terminal alert: three cadence-spaced downlinks went out and
    /// the Node answered none of them, so the window is not coming. Rides
    /// the same pre-evaluated `SystemAlerts` path B2's offline sweep uses —
    /// this detector, too, owns its own debounce (a job reaches
    /// "retries exhausted" exactly once).
    fn alert_history_unavailable(&self, job: &BackfillJob) {
        let Some(tx) = self.notify_tx.as_ref() else {
            return;
        };
        let _ = tx.send(NotifyJob::SystemAlerts {
            hostname: job.hostname.clone(),
            events: vec![SystemAlertEvent::new(
                SystemAlertKind::NodeHistoryUnavailable,
                format!(
                    "Node {} did not answer {} history requests for {} - {}",
                    job.node_key,
                    job.retry_count + 1,
                    format_ts_minutes(job.time_from),
                    format_ts_minutes(job.time_to),
                ),
                "history_unavailable",
                vec![
                    ("node", job.node_key.clone()),
                    ("count", (job.retry_count + 1).to_string()),
                    ("from", crate::display_tz::format_minutes(job.time_from)),
                    ("to", crate::display_tz::format_minutes(job.time_to)),
                ],
            )],
        });
    }

    /// True when no downlink was sent to `(hostname, node_key)` within the
    /// last `min_request_interval_s`.
    fn cooloff_elapsed(&self, hostname: &str, node_key: &str) -> bool {
        let map = self.last_publish.lock();
        match map.get(&(hostname.to_string(), node_key.to_string())) {
            Some(last) => last.elapsed() >= Duration::from_secs(self.cfg.min_request_interval_s),
            None => true,
        }
    }

    /// `update_job_status` with logging — status transitions here are
    /// best-effort (the reconciler re-detects anything that slipped).
    async fn set_status(
        &self,
        job_id: i64,
        status: &'static str,
        error_message: Option<String>,
        device_refused: bool,
    ) {
        let result = self
            .db
            .run(move |s| {
                portal_storage::with_retry(|| {
                    s.update_job_status(job_id, status, error_message.as_deref(), device_refused)
                })
            })
            .await;
        if let Err(err) = result {
            tracing::warn!(%err, job_id, status, "backfill status update failed");
        }
    }

    fn nudge(&self, hostname: &str, node_key: &str) {
        // No subscribers is normal (no browser tab open) — same reasoning
        // as StateStore's ChangeEvent sends.
        let _ = self.nudge_tx.send(BackfillNudge {
            hostname: hostname.to_string(),
            node_key: node_key.to_string(),
        });
    }
}

/// `templates/partials/backfill_jobs.html`'s per-row context — field names
/// and shapes are that template's binding contract (see its doc comment).
/// Shared by `sse.rs` and the route handlers so both render identical rows.
#[derive(Debug, Clone, Serialize)]
pub struct JobCtx {
    pub job_id: i64,
    pub status: String,
    pub time_from: String,
    pub time_to: String,
    pub trigger: String,
    pub samples_received: i64,
    pub samples_expected: Option<i64>,
    pub retry_count: i64,
    pub device_refused: bool,
    pub error_message: Option<String>,
    /// A terminal refusal's name ([`TERMINAL_RESULTS`]) parsed out of `error_message`, so the
    /// template shows a translated reason instead of "device refused: history_unavailable —
    /// code 4" (HS-13). The raw message stays in the row's tooltip.
    pub refusal: Option<&'static str>,
    pub created_at: String,
    /// The window as unix seconds, so the History tab's coverage bar can paint the holes a
    /// running job is downloading (ui_components.md, amendment 2026-10-15).
    pub from_unix: i64,
    pub to_unix: i64,
}

/// One detected hole in a Node's history — `partials/backfill_jobs.html`'s
/// "missing *x* for *from–to*" row (§6, closing paragraph).
///
/// `duration` is pre-formatted rather than i18n-keyed because `h`/`min` are
/// the same two abbreviations in both shipped languages; the label around
/// it is a translated key.
///
/// `samples_expected` is the *x* the Czech spec asks for by name —
/// devices.node.md §3.9 item 1 wants "od–do, očekávaný počet záznamů", and
/// §6's terser "missing *x* for *from–to*" reads as either. It is an
/// estimate from the cadence, not a promise from the device: `BackfillJob`'s
/// own `samples_expected` stays `None` until the Node answers, and a hole
/// that has not been requested yet has nobody to ask.
#[derive(Debug, Clone, Serialize)]
pub struct GapCtx {
    pub time_from: String,
    pub time_to: String,
    /// The window again as unix seconds, for the row's "Download" button to ask for exactly
    /// this hole (devices.node.md §3.9, amendment 2026-09-25).
    pub from_unix: i64,
    pub to_unix: i64,
    pub duration: String,
    pub samples_expected: i64,
    /// Why this hole has nothing to download, when it has not (P-8): `answered` (a finished or
    /// refused job covers it) or `history_off` (the Node keeps no history). `None` = the row
    /// offers Download.
    pub unavailable: Option<&'static str>,
}

/// Whether the Node's config info says `application.history_enable = false`. Unknown (no
/// config info yet) is not "no": the request is then the way to find out.
pub(crate) fn node_keeps_no_history(store: &StateStore, hostname: &str, node_key: &str) -> bool {
    store
        .snapshot_device(hostname)
        .and_then(|d| {
            d.node_configs
                .get(node_key)
                .and_then(|e| e.config.get(crate::render::HISTORY_ENABLE_KEY).cloned())
        })
        .and_then(|v| v.as_bool())
        == Some(false)
}

/// How many readings a hole between two consecutive readings swallowed:
/// the span holds `span / interval` cadence slots, and the two readings
/// bounding it occupy one of them, so one fewer is missing. Never negative
/// — [`BackfillService::detect_gaps`] only reports spans over
/// `2 x interval`, but a
/// cadence that changed mid-window can still make the arithmetic degenerate.
pub(crate) fn missing_samples(from: i64, to: i64, interval_s: i64) -> i64 {
    if interval_s <= 0 {
        return 0;
    }
    ((to - from) / interval_s - 1).max(0)
}

/// `h`/`min` from a span in seconds, minute-resolution — the same precision
/// [`format_ts_minutes`] gives the window it describes.
pub(crate) fn format_duration(seconds: i64) -> String {
    let minutes = (seconds.max(0) + 59) / 60;
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// Maps a stored job onto the template row, formatting timestamps as
/// `YYYY-MM-DD HH:MM` in the display zone.
pub fn job_ctx(job: &BackfillJob) -> JobCtx {
    JobCtx {
        job_id: job.job_id,
        status: job.status.clone(),
        time_from: format_ts_minutes(job.time_from),
        time_to: format_ts_minutes(job.time_to),
        trigger: job.trigger.clone(),
        samples_received: job.samples_received,
        samples_expected: job.samples_expected,
        retry_count: job.retry_count,
        device_refused: job.device_refused,
        error_message: job.error_message.clone(),
        refusal: refusal_name(job),
        created_at: format_ts_minutes(job.created_at),
        from_unix: job.time_from,
        to_unix: job.time_to,
    }
}

/// The terminal result a refused job's message names: `"device refused: <name>[ — detail]"`
/// as [`BackfillService`] writes it, or the bare name.
fn refusal_name(job: &BackfillJob) -> Option<&'static str> {
    if !job.device_refused {
        return None;
    }
    let message = job.error_message.as_deref()?;
    let rest = message.strip_prefix("device refused: ").unwrap_or(message);
    let name = rest.split(" — ").next().unwrap_or(rest).trim();
    TERMINAL_RESULTS.iter().copied().find(|r| *r == name)
}

/// One Node's `application.interval_report` from the store's cached config,
/// falling back to [`DEFAULT_INTERVAL_REPORT_S`]. The single place the
/// cadence is resolved — the retry deadline, the gap detector and the auto
/// trigger all have to agree on it or a Node gets retried against one
/// number and gap-scanned against another.
/// The Hubs a drain or reconciler pass walks, in the spelling jobs are stored under (N34,
/// history tests 2026-09-25). The store keys a Hub lowercase and keeps its display spelling in
/// `DeviceState::hostname`; jobs are created with the display one (the offline transition, the
/// route), and storage matches hostnames exactly. Walking the map keys found no job on a Hub
/// named `FIBER-4E970F4A`, so nothing was ever requested there.
fn pass_hostnames(store: &StateStore) -> Vec<String> {
    let mut hostnames: Vec<String> = store
        .snapshot()
        .devices
        .values()
        .map(|d| d.hostname.clone())
        .collect();
    hostnames.sort();
    hostnames.dedup();
    hostnames
}

fn node_interval_s(store: &StateStore, hostname: &str, node_key: &str) -> i64 {
    store
        .snapshot_device(hostname)
        .and_then(|device| {
            device
                .node_configs
                .get(node_key)
                .and_then(|entry| entry.config.get("application.interval_report"))
                .and_then(Value::as_i64)
        })
        .filter(|interval| *interval > 0)
        .unwrap_or(DEFAULT_INTERVAL_REPORT_S)
}

/// `YYYY-MM-DD HH:MM` in the display zone, the zone the backfill form is
/// typed in (decisions.md #25, #32). Minute precision is what the jobs table
/// needs; falls back to the raw number for out-of-range values.
pub(crate) fn format_ts_minutes(unix_secs: i64) -> String {
    format_ts_minutes_in(unix_secs, &crate::display_tz::current())
}

fn format_ts_minutes_in(unix_secs: i64, tz: &jiff::tz::TimeZone) -> String {
    match jiff::Timestamp::from_second(unix_secs) {
        Ok(ts) => ts
            .to_zoned(tz.clone())
            .strftime("%Y-%m-%d %H:%M")
            .to_string(),
        Err(_) => unix_secs.to_string(),
    }
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::offline::OfflineTransition;
    use crate::relay::Publisher;
    use futures_util::future::BoxFuture;
    use portal_storage::{NodeFieldReading, SOURCE_LIVE};
    use serde_json::json;
    use std::sync::Mutex as TestMutex;

    /// Captured `(topic, payload)` pairs shared between a `Publisher`
    /// double and the test asserting against it (same shape as relay.rs's
    /// test doubles — duplicated because that module's doubles are
    /// `cfg(test)`-private to it).
    type Captured = Arc<TestMutex<Vec<(String, Vec<u8>)>>>;

    /// Records every publish, and that is all it has to do.
    ///
    /// `get_sticker_history` is a signed `read` through the relay now
    /// (design.md §5.4), but it goes out through `CommandRelay::dispatch`,
    /// which returns once the frame is on the wire. So no Hub double is
    /// needed to keep these tests off the relay's 20 s timeout, and the
    /// publish counts below mean exactly what they meant when this was a
    /// fire-and-forget publish.
    #[derive(Default, Clone)]
    struct RecordingPublisher {
        sent: Captured,
    }

    impl Publisher for RecordingPublisher {
        fn publish(
            &self,
            topic: String,
            payload: Vec<u8>,
        ) -> BoxFuture<'static, anyhow::Result<()>> {
            self.sent.lock().unwrap().push((topic, payload));
            Box::pin(async { Ok(()) })
        }
    }

    /// A publisher whose every publish fails — for asserting that nothing
    /// is state-transitioned or retry-counted on an unsent downlink.
    struct FailingPublisher;
    impl Publisher for FailingPublisher {
        fn publish(
            &self,
            _topic: String,
            _payload: Vec<u8>,
        ) -> BoxFuture<'static, anyhow::Result<()>> {
            Box::pin(async { Err(anyhow::anyhow!("broker unreachable")) })
        }
    }

    struct NoopPublisher;
    impl Publisher for NoopPublisher {
        fn publish(
            &self,
            _topic: String,
            _payload: Vec<u8>,
        ) -> BoxFuture<'static, anyhow::Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    impl BackfillService {
        /// Test-only convenience for other modules' tests (`http.rs`'s
        /// `test_app`) — a service on a fresh db that never really
        /// publishes, mirroring `CommandRelay::test_instance`.
        pub fn test_instance(data_dir: &str) -> Self {
            Self::open(
                data_dir,
                None,
                CommandRelay::new(NoopPublisher, "proximos"),
                BackfillConfig::default(),
                crate::durable::test_gate(data_dir),
            )
            .expect("open backfill service")
        }
    }

    const HOST: &str = "fiber-001";
    const NODE_A: &str = "aabbccdd00000001";
    const NODE_B: &str = "aabbccdd00000002";

    /// A service whose downlinks go out through a real relay, signed by a
    /// real `portal-service` key minted under a paired CA — the production
    /// path, with only the broker replaced by a double.
    async fn service_with(
        publisher: RecordingPublisher,
        cfg: BackfillConfig,
    ) -> (BackfillService, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8 path");

        let relay = CommandRelay::new(publisher, "proximos");
        let keys = KeyService::open(data_dir, None).expect("open keys");
        keys.store_pairing(
            HOST,
            Some("test-ca".into()),
            "00".repeat(32),
            [7u8; 32],
            None,
            None,
        )
        .await
        .expect("pair the hub");

        let service = BackfillService::open(
            data_dir,
            None,
            relay,
            cfg,
            crate::durable::test_gate(data_dir),
        )
        .expect("open service")
        .with_keys(keys);
        (service, dir)
    }

    /// Decision #18: a paired Hub is found under either case of its name, an unpaired one is
    /// not, and the auto path creates no job for it.
    /// P2P-N12: a refused seam waits 30 s, then twice as long each time, at most 10 min.
    #[test]
    fn a_refused_radio_seam_backs_off_exponentially_to_ten_minutes() {
        let secs: Vec<u64> = [1, 2, 3, 4, 5, 6, 40]
            .into_iter()
            .map(|n| seam_backoff_delay(n).as_secs())
            .collect();
        assert_eq!(secs, [30, 60, 120, 240, 480, 600, 600]);
    }

    #[tokio::test]
    async fn history_comes_only_from_a_paired_hub() {
        let (service, _dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        assert!(service.hub_paired(HOST).await);
        assert!(service.hub_paired(&HOST.to_ascii_lowercase()).await);
        assert!(!service.hub_paired("FIBER-999").await);

        let store = StateStore::new();
        service
            .auto_backfill_node(&store, "FIBER-999", NODE_A)
            .await;
        assert!(service
            .jobs_for(NODE_A, None)
            .await
            .expect("list")
            .is_empty());
    }

    /// The same, for the two tests that need every publish to fail.
    async fn failing_service(cfg: BackfillConfig) -> (BackfillService, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8 path");
        let keys = KeyService::open(data_dir, None).expect("open keys");
        keys.store_pairing(
            HOST,
            Some("test-ca".into()),
            "00".repeat(32),
            [7u8; 32],
            None,
            None,
        )
        .await
        .expect("pair the hub");
        let service = BackfillService::open(
            data_dir,
            None,
            CommandRelay::new(FailingPublisher, "proximos"),
            cfg,
            crate::durable::test_gate(data_dir),
        )
        .expect("open service")
        .with_keys(keys);
        (service, dir)
    }

    /// Second connection onto the service's db (WAL) — lets tests seed
    /// readings and flip job states the public API deliberately refuses to.
    fn side_door(dir: &tempfile::TempDir) -> Storage {
        Storage::open(&dir.path().join("portal.db"), None).expect("open side door")
    }

    fn parsed(body: &[u8]) -> Value {
        serde_json::from_slice(body).expect("valid json")
    }

    /// A store holding no devices: every Node then resolves to
    /// [`DEFAULT_INTERVAL_REPORT_S`], which is what every test that is not
    /// about the cadence itself wants.
    fn unconfigured_store() -> StateStore {
        StateStore::new()
    }

    /// A store where `NODE_A` on `HOST` carries a cached
    /// `application.interval_report` of `interval_s`.
    fn store_reporting_every(interval_s: i64) -> StateStore {
        use crate::state::events::IngestEvent;
        use fiber_protocol::payloads::lorawan::NodeConfigMsg;
        let store = StateStore::new();
        store.apply(IngestEvent::StickerConfig {
            hostname: HOST.into(),
            node_key: NODE_A.into(),
            payload: NodeConfigMsg {
                dev_eui: Some(NODE_A.into()),
                config: [("application.interval_report".to_string(), json!(interval_s))]
                    .into_iter()
                    .collect(),
                page_index: None,
                page_count: None,
                synced_at: None,
                last_ack: None,
                read_status: Some("complete".into()),
                missing: None,
                error: None,
                chunks_done: None,
                chunks_total: None,
            },
        });
        store
    }

    // ---- request() validation matrix ---------------------------------

    #[tokio::test]
    async fn request_rejects_bad_windows() {
        let (service, _dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let now = now_unix();

        // to <= from
        let err = service
            .request(HOST, NODE_A, now - 100, now - 100, "manual")
            .await
            .unwrap_err();
        assert!(matches!(err, BackfillError::EmptyWindow));

        // window reaching into the future
        let err = service
            .request(HOST, NODE_A, now - 100, now + 3_600, "manual")
            .await
            .unwrap_err();
        assert!(matches!(err, BackfillError::FutureWindow));

        // wider than backfill_max_window_days
        let err = service
            .request(HOST, NODE_A, now - 31 * 86_400, now - 10, "manual")
            .await
            .unwrap_err();
        assert!(matches!(err, BackfillError::WindowTooWide(30)));
    }

    #[tokio::test]
    async fn request_creates_pending_and_conflicts_on_overlap() {
        let (service, _dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let mut nudges = service.subscribe_nudges();
        let now = now_unix();

        let job = service
            .request(HOST, NODE_A, now - 7_200, now - 3_600, "manual")
            .await
            .expect("create job");
        assert_eq!(job.status, JOB_PENDING);
        assert_eq!(job.trigger, "manual");
        assert_eq!(job.node_key, NODE_A);

        let nudge = nudges.try_recv().expect("nudge on create");
        assert_eq!(nudge.node_key, NODE_A);

        // Overlapping window on the same node → 409-shaped error.
        let err = service
            .request(HOST, NODE_A, now - 5_400, now - 1_800, "manual")
            .await
            .unwrap_err();
        assert!(matches!(err, BackfillError::Overlap));

        // Same window on a different node is fine.
        service
            .request(HOST, NODE_B, now - 5_400, now - 1_800, "manual")
            .await
            .expect("other node unaffected");
    }

    #[tokio::test]
    async fn request_lowercases_node_key() {
        let (service, _dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let now = now_unix();
        let job = service
            .request(HOST, "AABBCCDD00000001", now - 7_200, now - 3_600, "manual")
            .await
            .expect("create job");
        assert_eq!(job.node_key, NODE_A);
    }

    // ---- drain ---------------------------------------------------------

    #[tokio::test]
    async fn drain_publishes_oldest_pending_once_per_node() {
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let (service, _dir) = service_with(publisher, BackfillConfig::default()).await;
        let now = now_unix();

        // Two non-overlapping pendings on node A (oldest first), one on B.
        let oldest = service
            .request(HOST, NODE_A, now - 40_000, now - 30_000, "manual")
            .await
            .expect("job 1");
        service
            .request(HOST, NODE_A, now - 20_000, now - 10_000, "manual")
            .await
            .expect("job 2");
        let b_job = service
            .request(HOST, NODE_B, now - 20_000, now - 10_000, "manual")
            .await
            .expect("job 3");

        service
            .drain_pass(&unconfigured_store(), &[HOST.to_string()])
            .await;

        let captured = sent.lock().unwrap().clone();
        assert_eq!(captured.len(), 2, "one downlink per node per pass");
        // One dialect, one topic (design.md §7): the downlink used to be a
        // flat `{"command": "get_sticker_history", …}` on its own topic,
        // formatted by this service. It is a signed envelope through the
        // relay now.
        assert!(captured
            .iter()
            .all(|(topic, _)| topic == "proximos/fiber-001/commands/config/request"));

        let a_body = parsed(&captured[0].1);
        assert_eq!(a_body["command"], "config_request");
        assert_eq!(a_body["command_type"], "get_sticker_history");
        assert_eq!(a_body["v"], 2);
        assert_eq!(a_body["level"], "read", "single-shot on the device");
        assert_eq!(
            a_body["signer_id"], "portal-service",
            "a background request has no user; it signs as the portal (design.md §5.4)"
        );
        assert_eq!(
            a_body["certificate"]["permissions"],
            json!(["get_status", "level:read"]),
            "exactly what that signer may do, and nothing above read"
        );
        assert_eq!(
            a_body["params"],
            json!({
                "dev_eui": NODE_A,
                "from": oldest.time_from,
                "to": oldest.time_to,
            }),
            "node A publishes its OLDEST pending job"
        );
        let b_body = parsed(&captured[1].1);
        assert_eq!(b_body["params"]["dev_eui"], NODE_B);

        let jobs = service.jobs_for(NODE_A, None).await.expect("list");
        let oldest_row = jobs
            .iter()
            .find(|j| j.job_id == oldest.job_id)
            .expect("row");
        assert_eq!(oldest_row.status, JOB_REQUESTED);
        assert!(oldest_row.requested_at.is_some());

        // Node A now has an in-flight job AND node B is inside its
        // cool-off — a second pass publishes nothing (b's pending queue is
        // empty anyway).
        service
            .drain_pass(&unconfigured_store(), &[HOST.to_string()])
            .await;
        assert_eq!(
            sent.lock().unwrap().len(),
            2,
            "no publish while a job is in flight"
        );
        assert_eq!(
            service
                .jobs_for(NODE_B, None)
                .await
                .expect("list b")
                .first()
                .map(|j| j.status.clone())
                .as_deref(),
            Some(JOB_REQUESTED)
        );
        let _ = b_job;
    }

    #[tokio::test(start_paused = true)]
    async fn drain_honors_cooloff_between_downlinks_to_one_node() {
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let (service, dir) = service_with(publisher, BackfillConfig::default()).await;
        let now = now_unix();

        let first = service
            .request(HOST, NODE_A, now - 40_000, now - 30_000, "manual")
            .await
            .expect("job 1");
        service
            .request(HOST, NODE_A, now - 20_000, now - 10_000, "manual")
            .await
            .expect("job 2");

        service
            .drain_pass(&unconfigured_store(), &[HOST.to_string()])
            .await;
        assert_eq!(sent.lock().unwrap().len(), 1);

        // Resolve job 1 out-of-band (the persist thread's role) so only
        // the cool-off can be what's blocking job 2.
        side_door(&dir)
            .update_job_status(first.job_id, "done", None, false)
            .expect("mark done");

        service
            .drain_pass(&unconfigured_store(), &[HOST.to_string()])
            .await;
        assert_eq!(
            sent.lock().unwrap().len(),
            1,
            "cool-off since the last actually-sent downlink blocks the next request"
        );

        tokio::time::advance(Duration::from_secs(31)).await;
        service
            .drain_pass(&unconfigured_store(), &[HOST.to_string()])
            .await;
        assert_eq!(
            sent.lock().unwrap().len(),
            2,
            "cool-off elapsed — job 2 goes out"
        );
    }

    /// N34 (history tests 2026-09-25): a pass walks the Hubs in their display spelling, the one
    /// jobs are created under, so a Hub named in capitals gets its jobs requested at all.
    #[tokio::test]
    async fn a_pass_walks_hubs_in_the_spelling_jobs_are_stored_under() {
        let store = StateStore::new();
        store.apply(crate::state::IngestEvent::Status {
            hostname: "FIBER-4E970F4A".into(),
            status: fiber_protocol::DeviceStatus::Online,
        });
        // Decision #21: the display spelling is the lowercase one too.
        assert_eq!(pass_hostnames(&store), ["fiber-4e970f4a"]);
        assert_eq!(
            store.snapshot().devices.keys().cloned().collect::<Vec<_>>(),
            ["fiber-4e970f4a"],
            "the map key is what the passes used to walk"
        );
    }

    #[tokio::test]
    async fn drain_leaves_job_pending_when_publish_fails() {
        let (service, _dir) = failing_service(BackfillConfig::default()).await;
        let now = now_unix();
        let job = service
            .request(HOST, NODE_A, now - 7_200, now - 3_600, "manual")
            .await
            .expect("create job");

        service
            .drain_pass(&unconfigured_store(), &[HOST.to_string()])
            .await;

        let jobs = service.jobs_for(NODE_A, None).await.expect("list");
        assert_eq!(jobs[0].job_id, job.job_id);
        assert_eq!(
            jobs[0].status, JOB_PENDING,
            "unsent request must not flip to requested"
        );
    }

    // ---- refusal handling ----------------------------------------------

    #[tokio::test]
    async fn terminal_command_results_fail_the_inflight_job() {
        for refusal in TERMINAL_RESULTS {
            let (service, _dir) =
                service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
            let now = now_unix();
            service
                .request(HOST, NODE_A, now - 7_200, now - 3_600, "manual")
                .await
                .expect("create job");
            service
                .drain_pass(&unconfigured_store(), &[HOST.to_string()])
                .await;

            service
                .handle_command_result(
                    HOST,
                    NODE_A,
                    &json!({
                        "command": "get_sticker_history",
                        "result": refusal,
                        "detail": "window not in ring buffer",
                    }),
                )
                .await;

            let job = &service.jobs_for(NODE_A, None).await.expect("list")[0];
            assert_eq!(job.status, "failed", "refusal {refusal}");
            assert!(job.device_refused, "refusal {refusal}");
            let message = job.error_message.as_deref().unwrap_or_default();
            assert!(message.contains(refusal), "refusal {refusal}: {message}");
            assert!(message.contains("window not in ring buffer"));
        }
    }

    /// The v2 radio seam, as the drain sees it: records every request, answers with `seq`, or
    /// `NotConfigured` for a Hub with no rendered radio config.
    struct FakeSeam {
        configured: bool,
        seq: u64,
        calls: parking_lot::Mutex<Vec<(String, i64, i64)>>,
        drops: parking_lot::Mutex<Vec<(String, Vec<u64>)>>,
    }

    impl FakeSeam {
        fn new(configured: bool, seq: u64) -> Arc<Self> {
            Arc::new(Self {
                configured,
                seq,
                calls: parking_lot::Mutex::new(Vec::new()),
                drops: parking_lot::Mutex::new(Vec::new()),
            })
        }
    }

    impl HistorySeam for FakeSeam {
        fn history_replay<'a>(
            &'a self,
            dev_eui: &'a str,
            from: i64,
            to: i64,
        ) -> BoxFuture<'a, Result<Value, crate::radio::RadioError>> {
            self.calls.lock().push((dev_eui.to_string(), from, to));
            let answer = if self.configured {
                Ok(json!({"command": "node-send", "queued": true, "seq": self.seq}))
            } else {
                Err(crate::radio::RadioError::NotConfigured(
                    "no rendered radio config".into(),
                ))
            };
            Box::pin(async move { answer })
        }

        fn drop_queued<'a>(
            &'a self,
            dev_eui: &'a str,
            seqs: &'a [u64],
        ) -> BoxFuture<'a, Result<usize, crate::radio::RadioError>> {
            self.drops.lock().push((dev_eui.to_string(), seqs.to_vec()));
            Box::pin(async { Ok(1) })
        }
    }

    /// Bench 2026-09-27 (job 138): a retry takes the job's earlier request out of the Hub's
    /// queue before it queues the new one, so a request still waiting there is replaced rather
    /// than delivered twice. The first request drops nothing.
    #[tokio::test]
    async fn a_history_retry_replaces_its_earlier_queued_request() {
        let (service, _dir) =
            service_with(RecordingPublisher::default(), instant_retry_cfg(2)).await;
        let seam = FakeSeam::new(true, 42);
        let service = service.with_radio(seam.clone());
        let now = now_unix();
        service
            .request(HOST, NODE_A, now - 7_200, now - 3_600, "manual")
            .await
            .expect("create job");
        let store = unconfigured_store();
        service.drain_pass(&store, &[HOST.to_string()]).await;
        assert!(
            seam.drops.lock().is_empty(),
            "the first request drops nothing"
        );

        service.reconcile_pass(&store, &[HOST.to_string()]).await;
        assert_eq!(seam.calls.lock().len(), 2, "the retry queued again");
        assert_eq!(
            *seam.drops.lock(),
            vec![(NODE_A.to_string(), vec![42])],
            "after taking the earlier request out"
        );
    }

    /// N35 (bench, 2026-09-25): on a v2 Hub the relay's `get_sticker_history` lands in an
    /// agent with no LoRaWAN stack. The request goes through the radio seam instead, for the
    /// job's own window, and nothing is published on the relay.
    #[tokio::test]
    async fn a_v2_hub_requests_history_through_the_radio_not_the_relay() {
        let publisher = RecordingPublisher::default();
        let (service, _dir) = service_with(publisher.clone(), BackfillConfig::default()).await;
        let seam = FakeSeam::new(true, 42);
        let service = service.with_radio(seam.clone());
        let now = now_unix();
        let job = service
            .request(HOST, NODE_A, now - 7_200, now - 3_600, "manual")
            .await
            .expect("create job");

        service
            .drain_pass(&unconfigured_store(), &[HOST.to_string()])
            .await;

        assert_eq!(
            *seam.calls.lock(),
            vec![(NODE_A.to_string(), job.time_from, job.time_to)]
        );
        assert!(
            publisher.sent.lock().unwrap().is_empty(),
            "no relay publish"
        );
        let jobs = service.jobs_for(NODE_A, None).await.expect("list");
        assert_eq!(jobs[0].status, JOB_REQUESTED);
    }

    /// A Hub without a rendered radio config is a v1 Hub: the relay path, as before.
    #[tokio::test]
    async fn a_hub_without_a_radio_config_falls_back_to_the_relay() {
        let publisher = RecordingPublisher::default();
        let (service, _dir) = service_with(publisher.clone(), BackfillConfig::default()).await;
        let service = service.with_radio(FakeSeam::new(false, 0));
        let now = now_unix();
        service
            .request(HOST, NODE_A, now - 7_200, now - 3_600, "manual")
            .await
            .expect("create job");

        service
            .drain_pass(&unconfigured_store(), &[HOST.to_string()])
            .await;

        assert_eq!(
            publisher.sent.lock().unwrap().len(),
            1,
            "one relay downlink"
        );
        let jobs = service.jobs_for(NODE_A, None).await.expect("list");
        assert_eq!(jobs[0].status, JOB_REQUESTED);
    }

    /// A v2 refusal names no command, only the `seq` it answers. It fails the job when that is
    /// the request's `seq` and the code is final; another `seq`, or `NOT_READY`, leaves the job
    /// to the reconciler.
    #[tokio::test]
    async fn a_radio_refusal_for_the_requests_seq_fails_the_job() {
        let (service, _dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let service = service.with_radio(FakeSeam::new(true, 42));
        let now = now_unix();
        service
            .request(HOST, NODE_A, now - 7_200, now - 3_600, "manual")
            .await
            .expect("create job");
        service
            .drain_pass(&unconfigured_store(), &[HOST.to_string()])
            .await;

        let error = |seq: u64, code: u64| {
            json!({"transport": "control.radio", "command": "error", "result": "error",
                   "seq": seq, "code": code, "detail": format!("code {code}")})
        };
        for (seq, code) in [(41, 4), (42, 3)] {
            service
                .handle_command_result(HOST, NODE_A, &error(seq, code))
                .await;
            let job = &service.jobs_for(NODE_A, None).await.expect("list")[0];
            assert_eq!(job.status, JOB_REQUESTED, "seq {seq} code {code}");
        }
        service
            .handle_command_result(
                HOST,
                NODE_A,
                &json!({"transport": "control.radio", "command": "ack", "result": "ok", "seq": 42}),
            )
            .await;
        assert_eq!(
            service.jobs_for(NODE_A, None).await.expect("list")[0].status,
            JOB_REQUESTED
        );

        service
            .handle_command_result(HOST, NODE_A, &error(42, 4))
            .await;
        let job = &service.jobs_for(NODE_A, None).await.expect("list")[0];
        assert_eq!(job.status, "failed");
        assert!(job.device_refused);
        let message = job.error_message.as_deref().unwrap_or_default();
        assert!(message.contains("history_unavailable"), "{message}");

        // HS-13: the jobs table names the reason in the operator's language.
        let ctx = job_ctx(job);
        assert_eq!(ctx.refusal, Some("history_unavailable"));
        let templates = crate::render::Templates::new("templates", "static");
        let html = templates
            .env()
            .get_template("partials/backfill_jobs.html")
            .expect("template")
            .render(minijinja::context! {
                lang => "cs", node_key => NODE_A, can_edit => false,
                jobs => vec![ctx], gaps => Vec::<GapCtx>::new(),
            })
            .expect("render");
        assert!(html.contains("odmítnuto · Node tato data nemá"), "{html}");
    }

    #[tokio::test]
    async fn ok_and_foreign_command_results_are_noops() {
        let (service, _dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let now = now_unix();
        service
            .request(HOST, NODE_A, now - 7_200, now - 3_600, "manual")
            .await
            .expect("create job");
        service
            .drain_pass(&unconfigured_store(), &[HOST.to_string()])
            .await;

        for result in OK_RESULTS {
            service
                .handle_command_result(
                    HOST,
                    NODE_A,
                    &json!({"command": "get_sticker_history", "result": result}),
                )
                .await;
        }
        // A refusal-shaped result for some OTHER command must not touch
        // the history job either.
        service
            .handle_command_result(
                HOST,
                NODE_A,
                &json!({"command": "set_sticker_config", "result": "bad_request"}),
            )
            .await;

        let job = &service.jobs_for(NODE_A, None).await.expect("list")[0];
        assert_eq!(job.status, JOB_REQUESTED);
        assert!(!job.device_refused);
    }

    // ---- reconciler ------------------------------------------------------

    /// `retry_cadence_ticks = 0` puts every in-flight job's `not_before` at
    /// the instant it was published, so it is due on the very next pass —
    /// the alternative is pretending a Node reports every second, which
    /// would then also drive the gap detector.
    fn instant_retry_cfg(max_retries: i64) -> BackfillConfig {
        BackfillConfig {
            retry_cadence_ticks: 0,
            max_retries,
            ..BackfillConfig::default()
        }
    }

    #[tokio::test]
    async fn reconciler_republishes_stalled_job_and_counts_the_retry() {
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let (service, _dir) = service_with(publisher, instant_retry_cfg(1)).await;
        let now = now_unix();
        service
            .request(HOST, NODE_A, now - 7_200, now - 3_600, "manual")
            .await
            .expect("create job");
        service
            .drain_pass(&unconfigured_store(), &[HOST.to_string()])
            .await;
        assert_eq!(sent.lock().unwrap().len(), 1);

        service
            .reconcile_pass(&unconfigured_store(), &[HOST.to_string()])
            .await;
        assert_eq!(sent.lock().unwrap().len(), 2, "stalled job republished");
        let job = &service.jobs_for(NODE_A, None).await.expect("list")[0];
        assert_eq!(job.status, JOB_REQUESTED);
        assert_eq!(job.retry_count, 1);

        // retry_count (1) has reached max_retries (1): the next pass gives
        // up instead of publishing a third downlink.
        service
            .reconcile_pass(&unconfigured_store(), &[HOST.to_string()])
            .await;
        assert_eq!(sent.lock().unwrap().len(), 2, "no publish after exhaustion");
        let job = &service.jobs_for(NODE_A, None).await.expect("list")[0];
        assert_eq!(job.status, "failed");
        assert_eq!(job.error_message.as_deref(), Some("retries exhausted"));
        assert!(
            !job.device_refused,
            "exhaustion is distinct from device refusal"
        );
    }

    #[tokio::test]
    async fn reconciler_does_not_count_retries_for_failed_publishes() {
        let (service, dir) = failing_service(instant_retry_cfg(3)).await;
        let now = now_unix();
        let job = service
            .request(HOST, NODE_A, now - 7_200, now - 3_600, "manual")
            .await
            .expect("create job");
        // Force in-flight out-of-band (drain can't — its publish fails too).
        side_door(&dir)
            .update_job_status(job.job_id, JOB_REQUESTED, None, false)
            .expect("mark requested");

        service
            .reconcile_pass(&unconfigured_store(), &[HOST.to_string()])
            .await;

        let row = &service.jobs_for(NODE_A, None).await.expect("list")[0];
        assert_eq!(
            row.retry_count, 0,
            "retry_count increments only after a successful publish"
        );
        assert_eq!(row.status, JOB_REQUESTED);
    }

    // ---- gap detection ---------------------------------------------------

    fn seed_reading(storage: &Storage, node_key: &str, ts: i64) {
        seed_field_reading(storage, node_key, "temperature", ts);
    }

    fn seed_field_reading(storage: &Storage, node_key: &str, field: &str, ts: i64) {
        storage
            .insert_field_reading(&NodeFieldReading {
                hostname: HOST.to_string(),
                node_key: node_key.to_string(),
                field: field.to_string(),
                ts,
                value: Some(4.2),
                alarm_state: None,
                source: SOURCE_LIVE.to_string(),
            })
            .expect("seed reading");
    }

    #[tokio::test]
    async fn detect_gaps_flags_deltas_over_one_and_a_half_report_intervals() {
        let (service, dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let storage = side_door(&dir);
        let now = now_unix();

        // interval 300 → threshold 450 (#115). Deltas: 300 (ok), 1700 (gap),
        // 450 (NOT a gap, boundary), 451 (gap, just over), 600 (gap: one lost report).
        let t0 = now - 10_000;
        let ts = [t0, t0 + 300, t0 + 2_000, t0 + 2_450, t0 + 2_901, t0 + 3_501];
        for t in ts {
            seed_reading(&storage, NODE_A, t);
        }

        let gaps = service
            .detect_gaps(HOST, NODE_A, 300, 30)
            .await
            .expect("detect");
        assert_eq!(
            gaps,
            vec![
                (t0 + 300, t0 + 2_000),
                (t0 + 2_450, t0 + 2_901),
                (t0 + 2_901, t0 + 3_501)
            ]
        );
    }

    #[test]
    fn one_lost_report_is_a_gap_and_jitter_is_not() {
        assert!(is_gap(120, 60), "exactly 2 x: one report given up (#115)");
        assert!(is_gap(183, 60), "the HIL run's gap");
        assert!(!is_gap(90, 60), "1.5 x is the boundary");
        assert!(!is_gap(70, 60), "jitter");
    }

    /// #115: a live uplink after a gap schedules the automatic download even though the
    /// Node never went offline; an on-time uplink does not.
    #[tokio::test]
    async fn an_uplink_after_a_gap_schedules_the_auto_backfill() {
        let (service, _dir, _t0) = service_with_two_gaps().await;
        let now = now_unix();
        let key = (HOST.to_string(), NODE_A.to_string());
        let interval = DEFAULT_INTERVAL_REPORT_S;

        // First sight only records.
        service
            .observe_uplinks(&store_heard_at(now - 3 * interval), HOST)
            .await;
        assert!(service.auto_due.lock().is_empty());
        // On time: no gap.
        let on_time = now - 2 * interval;
        service
            .observe_uplinks(&store_heard_at(on_time), HOST)
            .await;
        assert!(service.auto_due.lock().is_empty(), "an on-time uplink");
        // The same reading seen again (another batch) is not a gap either.
        service
            .observe_uplinks(&store_heard_at(on_time), HOST)
            .await;
        assert!(service.auto_due.lock().is_empty(), "a repeated last_seen");
        // One lost report: 2 x interval.
        service.observe_uplinks(&store_heard_at(now), HOST).await;
        assert!(service.auto_due.lock().contains_key(&key), "scheduled");

        service
            .run_due_auto(&store_heard_at(now), now + 86_400)
            .await;
        let jobs = service.jobs_for(NODE_A, None).await.expect("list");
        assert_eq!(auto_jobs(jobs), 1, "one auto job");
    }

    /// #115: the uplink path goes through the auto path's dedup and spacing: a burst of
    /// gapped uplinks keeps one due time, and a later gapped uplink does not re-ask the same holes.
    #[tokio::test]
    async fn gapped_uplinks_are_deduplicated() {
        let (service, _dir, _t0) = service_with_two_gaps().await;
        let now = now_unix();
        let interval = DEFAULT_INTERVAL_REPORT_S;
        for i in 0..4 {
            let at = now - (8 - 2 * i) * interval;
            service.observe_uplinks(&store_heard_at(at), HOST).await;
        }
        assert_eq!(service.auto_due.lock().len(), 1, "one due time");
        service
            .run_due_auto(&store_heard_at(now), now + 86_400)
            .await;
        service
            .observe_uplinks(&store_heard_at(now + 2 * interval), HOST)
            .await;
        service
            .run_due_auto(&store_heard_at(now), now + 86_400)
            .await;
        let jobs = service.jobs_for(NODE_A, None).await.expect("list");
        assert_eq!(auto_jobs(jobs), 1, "no second job for the same holes");
    }

    /// F5. B3 scanned `temperature` alone, so a Node reporting only
    /// humidity returned no rows at all: the pairwise loop never ran, the
    /// answer was permanently "no gaps", the detail page's gap list stayed
    /// empty and automatic backfill never had a reason to fire.
    #[tokio::test]
    async fn detect_gaps_sees_a_node_that_reports_no_temperature() {
        let (service, dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let storage = side_door(&dir);
        let now = now_unix();
        let t0 = now - 10_000;

        for t in [t0, t0 + 300, t0 + 2_000] {
            seed_field_reading(&storage, NODE_A, "humidity", t);
        }

        let gaps = service
            .detect_gaps(HOST, NODE_A, 300, 30)
            .await
            .expect("detect");
        assert_eq!(gaps, vec![(t0 + 300, t0 + 2_000)]);
    }

    /// "The Node reported" is the union across its fields, not any one of
    /// them: a second field on a slower cadence must not invent a hole where
    /// the first one was reporting all along.
    #[tokio::test]
    async fn a_slow_second_field_does_not_invent_a_gap() {
        let (service, dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let storage = side_door(&dir);
        let now = now_unix();
        let t0 = now - 10_000;

        // Temperature every 300 s across the whole window...
        for step in 0..10 {
            seed_field_reading(&storage, NODE_A, "temperature", t0 + step * 300);
        }
        // ...and a battery voltage only at the ends, 2700 s apart.
        seed_field_reading(&storage, NODE_A, "voltage", t0);
        seed_field_reading(&storage, NODE_A, "voltage", t0 + 2_700);

        let gaps = service
            .detect_gaps(HOST, NODE_A, 300, 30)
            .await
            .expect("detect");
        assert!(gaps.is_empty(), "got {gaps:?}");
    }

    #[tokio::test]
    async fn detect_gaps_ignores_readings_older_than_max_age() {
        let (service, dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let storage = side_door(&dir);
        let now = now_unix();

        // A huge gap entirely older than max_age_days=1, plus a fresh pair
        // with no gap: nothing must be reported.
        seed_reading(&storage, NODE_A, now - 10 * 86_400);
        seed_reading(&storage, NODE_A, now - 5 * 86_400);
        seed_reading(&storage, NODE_A, now - 600);
        seed_reading(&storage, NODE_A, now - 300);

        let gaps = service
            .detect_gaps(HOST, NODE_A, 300, 1)
            .await
            .expect("detect");
        assert!(gaps.is_empty(), "got {gaps:?}");
    }

    #[tokio::test]
    async fn backfill_gaps_creates_auto_jobs_and_skips_overlaps_on_rerun() {
        let (service, dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let storage = side_door(&dir);
        let now = now_unix();
        let t0 = now - 10_000;
        for t in [t0, t0 + 2_000, t0 + 2_300, t0 + 4_000] {
            seed_reading(&storage, NODE_A, t);
        }

        let created = service
            .backfill_gaps(HOST, NODE_A, 300)
            .await
            .expect("backfill gaps");
        assert_eq!(created, 1, "one request for the span, not one per hole");

        let jobs = service.jobs_for(NODE_A, None).await.expect("list");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].trigger, "auto-on-reconnect");
        assert_eq!(jobs[0].status, JOB_PENDING);
        assert_eq!((jobs[0].time_from, jobs[0].time_to), (t0, t0 + 4_000));

        // Same gaps on a reconnect re-run: the span overlaps the active
        // job → nothing new.
        let created = service
            .backfill_gaps(HOST, NODE_A, 300)
            .await
            .expect("re-run");
        assert_eq!(created, 0);
    }

    #[tokio::test]
    async fn backfill_gaps_clamps_windows_to_the_auto_max() {
        let cfg = BackfillConfig {
            auto_backfill_max_window_h: 1,
            ..BackfillConfig::default()
        };
        let (service, dir) = service_with(RecordingPublisher::default(), cfg).await;
        let storage = side_door(&dir);
        let now = now_unix();

        // One 10 000 s gap; the auto job must keep only its newest hour.
        let gap_start = now - 20_000;
        let gap_end = now - 10_000;
        seed_reading(&storage, NODE_A, gap_start);
        seed_reading(&storage, NODE_A, gap_end);

        let created = service
            .backfill_gaps(HOST, NODE_A, 300)
            .await
            .expect("backfill gaps");
        assert_eq!(created, 1);
        let job = &service.jobs_for(NODE_A, None).await.expect("list")[0];
        assert_eq!(job.time_to, gap_end);
        assert_eq!(job.time_from, gap_end - 3_600);
    }

    // ---- cancel ----------------------------------------------------------

    #[tokio::test]
    async fn cancel_is_pending_only() {
        let (service, _dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let mut nudges = service.subscribe_nudges();
        let now = now_unix();

        let pending = service
            .request(HOST, NODE_A, now - 40_000, now - 30_000, "manual")
            .await
            .expect("job 1");
        let requested = service
            .request(HOST, NODE_B, now - 20_000, now - 10_000, "manual")
            .await
            .expect("job 2");
        service
            .drain_pass(&unconfigured_store(), &[HOST.to_string()])
            .await; // both go requested? no — per node
                    // NODE_A's job went requested; NODE_B's too (different nodes, one
                    // downlink each). Re-create a fresh pending on NODE_A to cancel.
        while nudges.try_recv().is_ok() {}

        assert!(
            !service
                .cancel(&requested.node_key, requested.job_id)
                .await
                .expect("cancel requested"),
            "requested is not freely cancellable"
        );
        assert!(
            !service
                .cancel("70b3d5e075000001", 999_999)
                .await
                .expect("cancel missing"),
            "missing job cancels nothing"
        );

        let fresh = service
            .request(HOST, NODE_A, now - 9_000, now - 8_000, "manual")
            .await
            .expect("fresh pending");
        while nudges.try_recv().is_ok() {}
        assert!(service
            .cancel(&fresh.node_key, fresh.job_id)
            .await
            .expect("cancel pending"));
        let nudge = nudges.try_recv().expect("nudge on cancel");
        assert_eq!(nudge.node_key, NODE_A);

        let row = service
            .jobs_for(NODE_A, None)
            .await
            .expect("list")
            .into_iter()
            .find(|j| j.job_id == fresh.job_id)
            .expect("row");
        assert_eq!(row.status, "cancelled");
        assert!(
            !service
                .cancel(&fresh.node_key, fresh.job_id)
                .await
                .expect("re-cancel"),
            "cancelled is terminal"
        );
        let _ = pending;
    }

    /// Job ids are sequential and global, so the id alone is not authority
    /// to cancel: the `node_key` in the route's path has to own the job.
    /// Before this, a manager on NODE_A could cancel NODE_B's job and the
    /// audit row would record NODE_A as the target.
    #[tokio::test]
    async fn cancel_will_not_reach_across_nodes() {
        let (service, _dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let now = now_unix();

        let victim = service
            .request(HOST, NODE_B, now - 20_000, now - 10_000, "manual")
            .await
            .expect("NODE_B job");

        assert!(
            !service
                .cancel(NODE_A, victim.job_id)
                .await
                .expect("cross-node cancel"),
            "NODE_A must not cancel NODE_B's job"
        );

        let row = service
            .jobs_for(NODE_B, None)
            .await
            .expect("list")
            .into_iter()
            .find(|j| j.job_id == victim.job_id)
            .expect("row");
        assert_eq!(row.status, "pending", "NODE_B's job survives untouched");

        // ...and the owner still can.
        assert!(service
            .cancel(NODE_B, victim.job_id)
            .await
            .expect("owner cancel"));
    }

    // ---- B3: cadence retry, terminal alarm (§6.2/§6.3) --------------------

    /// P2: a history retry keeps §6.2's three ticks, since it waits out the replay stream as
    /// well as the answer, but it never re-sends before the Node's answer to the first request
    /// is overdue, on either carrier, for any known cadence of a minute or more.
    #[test]
    fn a_history_retry_never_comes_before_the_answer_is_overdue() {
        let ticks = BackfillConfig::default().retry_cadence_ticks;
        for report in [60_i64, 300, 900, 3_600] {
            let retry = Duration::from_secs(u64::try_from(ticks * report).expect("positive"));
            let answer = crate::pending::downlink_answer_budget(u64::try_from(report).ok());
            assert!(retry >= answer, "{report} s: {retry:?} < {answer:?}");
        }
    }

    /// Collects the `SystemAlerts` jobs a service sends, so a test can
    /// assert on the §6.3 alert rather than on its absence.
    async fn service_with_notifier(
        publisher: RecordingPublisher,
        cfg: BackfillConfig,
    ) -> (
        BackfillService,
        tempfile::TempDir,
        mpsc::UnboundedReceiver<NotifyJob>,
    ) {
        let (service, dir) = service_with(publisher, cfg).await;
        let (tx, rx) = mpsc::unbounded_channel();
        (service.with_notifier(tx), dir, rx)
    }

    /// §6.2's budget, end to end: the drain's downlink is attempt one, two
    /// cadence-spaced re-sends follow, and the third failure is terminal —
    /// three downlinks, not four (that is what `max_retries: 2` buys).
    /// §6.3 then fires exactly one `NodeHistoryUnavailable`.
    ///
    /// Runs on `instant_retry_cfg`, so every deadline lands in the past the
    /// moment it is armed: `now_unix()` is wall-clock, so `tokio::time`
    /// cannot move it, and the arithmetic is asserted off the stored row in
    /// `arming_a_retry_deadline_waits_three_of_the_nodes_own_ticks`.
    #[tokio::test]
    async fn three_downlinks_then_a_history_unavailable_alert() {
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let (service, _dir, mut alerts) =
            service_with_notifier(publisher, instant_retry_cfg(2)).await;
        let store = unconfigured_store();
        let now = now_unix();
        service
            .request(HOST, NODE_A, now - 7_200, now - 3_600, "manual")
            .await
            .expect("create job");

        service.drain_pass(&store, &[HOST.to_string()]).await;
        assert_eq!(sent.lock().unwrap().len(), 1, "attempt 1 is the drain's");

        for expected in [2usize, 3] {
            service.reconcile_pass(&store, &[HOST.to_string()]).await;
            assert_eq!(sent.lock().unwrap().len(), expected, "cadence re-send");
        }
        let job = &service.jobs_for(NODE_A, None).await.expect("list")[0];
        assert_eq!(job.retry_count, 2);
        assert_eq!(job.status, JOB_REQUESTED);
        assert!(
            alerts.try_recv().is_err(),
            "no alert while retries remain — the window may still arrive"
        );

        // Third failure: terminal. No fourth downlink.
        service.reconcile_pass(&store, &[HOST.to_string()]).await;
        assert_eq!(sent.lock().unwrap().len(), 3, "three attempts, not four");
        let job = &service.jobs_for(NODE_A, None).await.expect("list")[0];
        assert_eq!(job.status, "failed");
        assert_eq!(job.error_message.as_deref(), Some("retries exhausted"));
        assert!(
            !job.device_refused,
            "exhaustion is silence, not a refusal — B1's refused badge must not light"
        );

        let alert = alerts.try_recv().expect("§6.3 alert");
        match alert {
            NotifyJob::SystemAlerts { hostname, events } => {
                assert_eq!(hostname, HOST);
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].kind, SystemAlertKind::NodeHistoryUnavailable);
                assert!(
                    events[0].message.contains(NODE_A),
                    "the operator has to learn which Node: {}",
                    events[0].message
                );
            }
            other => panic!("expected SystemAlerts, got {other:?}"),
        }

        // A job reaches "retries exhausted" once, so the alert needs no
        // debounce of its own — the next pass skips a failed job entirely.
        service.reconcile_pass(&store, &[HOST.to_string()]).await;
        assert!(alerts.try_recv().is_err(), "alert fires once per job");
    }

    /// The deadline arithmetic §6.2 actually specifies, read off the row:
    /// `now + retry_cadence_ticks x` **this Node's** cadence, not the
    /// detector's 900 s and not a fleet-wide timeout (§16.1).
    #[tokio::test]
    async fn arming_a_retry_deadline_waits_three_of_the_nodes_own_ticks() {
        let (service, _dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let store = store_reporting_every(600);
        let now = now_unix();
        service
            .request(HOST, NODE_A, now - 7_200, now - 3_600, "manual")
            .await
            .expect("create job");

        let before = now_unix();
        service.drain_pass(&store, &[HOST.to_string()]).await;
        let after = now_unix();

        let job = &service.jobs_for(NODE_A, None).await.expect("list")[0];
        let armed = job.not_before.expect("deadline armed on publish");
        assert!(
            (before + 3 * 600..=after + 3 * 600).contains(&armed),
            "expected now + 3 x 600, got {armed} against [{before}, {after}]"
        );

        // Before the deadline nothing is re-sent, whatever `updated_at`
        // says — that is the whole point of the column.
        let sent_before = service.jobs_for(NODE_A, None).await.expect("list")[0].retry_count;
        service.reconcile_pass(&store, &[HOST.to_string()]).await;
        let job = &service.jobs_for(NODE_A, None).await.expect("list")[0];
        assert_eq!(
            job.retry_count, sent_before,
            "the deadline is 30 minutes out; this pass must do nothing"
        );
        assert_eq!(job.status, JOB_REQUESTED);
    }

    /// A Node whose cadence the store does not know falls back to
    /// [`DEFAULT_INTERVAL_REPORT_S`] (300 s), *not* the detector's 900 s —
    /// §16.1's "the two want opposite errors".
    #[tokio::test]
    async fn an_unknown_cadence_arms_off_the_backfill_default_not_the_detectors() {
        let (service, _dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let now = now_unix();
        service
            .request(HOST, NODE_A, now - 7_200, now - 3_600, "manual")
            .await
            .expect("create job");

        let before = now_unix();
        service
            .drain_pass(&unconfigured_store(), &[HOST.to_string()])
            .await;
        let after = now_unix();

        let armed = service.jobs_for(NODE_A, None).await.expect("list")[0]
            .not_before
            .expect("deadline armed");
        assert!(
            (before + 3 * DEFAULT_INTERVAL_REPORT_S..=after + 3 * DEFAULT_INTERVAL_REPORT_S)
                .contains(&armed),
            "expected now + 3 x 300, got {armed}"
        );
    }

    /// V11's NULL rule: a row whose downlink predates the column (or whose
    /// `set_not_before` failed) is retried off `updated_at + ticks x
    /// cadence`, the same rule computed from what the old row does carry.
    ///
    /// A job forced in-flight without ever being published is exactly the
    /// pre-V11 shape — `not_before` is NULL and `updated_at` is the
    /// transition — so the two cadences below read the fallback formula off
    /// the same row rather than off raw SQL portal has no connection for.
    #[tokio::test]
    async fn a_null_deadline_falls_back_to_updated_at() {
        // ticks = 3, cadence 300 s: `updated_at + 900` is in the future,
        // so a NULL row is NOT "due now".
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let (patient, dir) = service_with(publisher, BackfillConfig::default()).await;
        let job = patient
            .request(
                HOST,
                NODE_A,
                now_unix() - 7_200,
                now_unix() - 3_600,
                "manual",
            )
            .await
            .expect("create job");
        side_door(&dir)
            .update_job_status(job.job_id, JOB_REQUESTED, None, false)
            .expect("force in flight without publishing");
        let row = &patient.jobs_for(NODE_A, None).await.expect("list")[0];
        assert!(row.not_before.is_none(), "the pre-V11 shape");

        patient
            .reconcile_pass(&unconfigured_store(), &[HOST.to_string()])
            .await;
        assert!(
            sent.lock().unwrap().is_empty(),
            "NULL is not 'due now' — it falls back to updated_at + 3 x cadence"
        );

        // ticks = 0 collapses that fallback onto `updated_at` itself, which
        // has already passed: the same NULL row is now due.
        let publisher = RecordingPublisher::default();
        let sent = publisher.sent.clone();
        let (prompt, dir) = service_with(publisher, instant_retry_cfg(2)).await;
        let job = prompt
            .request(
                HOST,
                NODE_A,
                now_unix() - 7_200,
                now_unix() - 3_600,
                "manual",
            )
            .await
            .expect("create job");
        side_door(&dir)
            .update_job_status(job.job_id, JOB_REQUESTED, None, false)
            .expect("force in flight without publishing");
        prompt
            .reconcile_pass(&unconfigured_store(), &[HOST.to_string()])
            .await;
        assert_eq!(
            sent.lock().unwrap().len(),
            1,
            "a NULL row past its updated_at fallback is republished"
        );
        let row = &prompt.jobs_for(NODE_A, None).await.expect("list")[0];
        assert_eq!(row.retry_count, 1);
        assert!(
            row.not_before.is_some(),
            "the republish arms the column the old row lacked"
        );
    }

    // ---- B3: the liveness trigger (§6.1/§6.4) -----------------------------

    /// A recovery and its scheduled download run to the end: the transition, then the drain
    /// step that fires what is due, a day on (past the settle and the spacing).
    async fn recover(service: &BackfillService, store: &StateStore, node_key: &str) {
        service.handle_transition(store, came_back(node_key)).await;
        service.run_due_auto(store, now_unix() + 86_400).await;
    }

    fn auto_jobs(jobs: Vec<BackfillJob>) -> usize {
        jobs.into_iter()
            .filter(|j| j.trigger == AUTO_TRIGGER)
            .count()
    }

    /// #131: a Node that rejoins with Hello before the offline sweep fires has its gap
    /// searched anyway, on the join.
    #[tokio::test]
    async fn a_hello_schedules_the_gap_search() {
        let (service, _dir, _t0) = service_with_two_gaps().await;
        let store = store_with_node();
        service
            .handle_change(
                &store,
                ChangeEvent::NodeJoined {
                    hostname: HOST.into(),
                    node_key: NODE_A.to_uppercase(),
                    at: now_unix(),
                },
            )
            .await;
        service.run_due_auto(&store, now_unix() + 86_400).await;
        let jobs = service.jobs_for(NODE_A, None).await.expect("list");
        assert_eq!(auto_jobs(jobs), 1, "the join queued one auto job");
    }

    /// #131: the gap search also runs every `gap_scan_ticks` x `interval_report`; the first
    /// sight of a Node only starts its clock.
    #[tokio::test]
    async fn the_gap_search_runs_every_five_intervals() {
        let (service, _dir, _t0) = service_with_two_gaps().await;
        let now = now_unix();
        let period = 5 * DEFAULT_INTERVAL_REPORT_S;
        let store = store_heard_at(now + period - 60);

        service.periodic_scan(&store, now).await;
        service.periodic_scan(&store, now + period - 1).await;
        service.run_due_auto(&store, now + 86_400).await;
        let jobs = service.jobs_for(NODE_A, None).await.expect("list");
        assert_eq!(auto_jobs(jobs), 0, "not before five intervals");

        service.periodic_scan(&store, now + period).await;
        service.run_due_auto(&store, now + 86_400).await;
        let jobs = service.jobs_for(NODE_A, None).await.expect("list");
        assert_eq!(auto_jobs(jobs), 1, "five intervals on, the search ran");
    }

    /// HIL !174: a Node silent past the offline threshold is not scanned every period.
    #[tokio::test]
    async fn the_periodic_scan_skips_a_silent_node() {
        let (service, _dir, _t0) = service_with_two_gaps().await;
        let now = now_unix();
        let store = store_heard_at(now - 86_400);
        service.periodic_scan(&store, now).await;
        service.periodic_scan(&store, now + 86_400).await;
        assert!(service.auto_due.lock().is_empty(), "nothing scheduled");
    }

    /// HIL !174: a roaming Node's readings are stored under each Hub that heard it; the gap
    /// search reads them all, so the other Hub's samples are not taken for a hole and the
    /// real hole is still found.
    #[tokio::test]
    async fn the_gap_search_reads_every_hub_a_node_reported_through() {
        let (service, dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let storage = side_door(&dir);
        let t0 = now_unix() - 20_000;
        // Hub A heard 0..3000, Hub B 3300..6000 every 300 s, then a hole to 9000 on A.
        for t in (0..=3_000).step_by(300) {
            seed_reading(&storage, NODE_A, t0 + t);
        }
        for t in (3_300..=6_000).step_by(300) {
            storage
                .insert_field_reading(&NodeFieldReading {
                    hostname: "other-hub".into(),
                    node_key: NODE_A.into(),
                    field: "temperature".into(),
                    ts: t0 + t,
                    value: Some(1.0),
                    alarm_state: None,
                    source: SOURCE_LIVE.into(),
                })
                .expect("seed");
        }
        seed_reading(&storage, NODE_A, t0 + 9_000);
        *service.known_hosts.lock() = vec![HOST.into(), "other-hub".into()];
        let gaps = service
            .detect_gaps(HOST, NODE_A, 300, 30)
            .await
            .expect("gaps");
        assert_eq!(gaps, vec![(t0 + 6_000, t0 + 9_000)]);
    }

    fn store_heard_at(ts: i64) -> StateStore {
        use crate::state::events::IngestEvent;
        let store = StateStore::new();
        let at = jiff::Timestamp::from_second(ts).expect("ts").to_string();
        store.apply(IngestEvent::NodeInventory {
            hostname: HOST.into(),
            node_key: NODE_A.into(),
            payload: json!({ "dev_eui": NODE_A, "transport": "lorawan", "last_seen": at }),
        });
        store
    }

    /// #131: a hole older than the auto window is not chosen over nothing.
    #[tokio::test]
    async fn an_old_hole_outside_the_recent_horizon_is_skipped() {
        let (service, dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let storage = side_door(&dir);
        let t0 = now_unix() - 3 * 86_400;
        for t in [t0, t0 + 5_000] {
            seed_reading(&storage, NODE_A, t);
        }
        let created = service
            .backfill_gaps(HOST, NODE_A, DEFAULT_INTERVAL_REPORT_S)
            .await
            .expect("gaps");
        assert_eq!(created, 0);
    }

    fn came_back(node_key: &str) -> HubTransition {
        HubTransition {
            hostname: HOST.to_string(),
            transition: OfflineTransition {
                scope: DeviceScope::Node,
                entity: node_key.to_string(),
                transition: Transition::CameBack,
                silent_for_s: 3_600,
            },
        }
    }

    fn went_offline(node_key: &str) -> HubTransition {
        HubTransition {
            hostname: HOST.to_string(),
            transition: OfflineTransition {
                scope: DeviceScope::Node,
                entity: node_key.to_string(),
                transition: Transition::WentOffline,
                silent_for_s: 3_600,
            },
        }
    }

    /// Seeds two holes in `NODE_A`'s history and returns the service.
    async fn service_with_two_gaps() -> (BackfillService, tempfile::TempDir, i64) {
        let (service, dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let storage = side_door(&dir);
        let t0 = now_unix() - 10_000;
        for t in [t0, t0 + 2_000, t0 + 2_300, t0 + 4_000] {
            seed_reading(&storage, NODE_A, t);
        }
        (service, dir, t0)
    }

    /// §6.1's cancel-and-restart cycle: a recovery queues the gaps, a fresh
    /// onset cancels **only** what that recovery queued, and the next
    /// recovery re-detects from the readings rather than draining a stale
    /// window.
    #[tokio::test]
    async fn a_new_offline_cancels_the_auto_jobs_and_the_next_recovery_requeues() {
        let (service, _dir, t0) = service_with_two_gaps().await;
        let store = unconfigured_store();

        // A manual job an operator asked for, inside the same outage.
        let manual = service
            .request(HOST, NODE_A, t0 + 4_100, t0 + 4_500, "manual")
            .await
            .expect("manual job");

        recover(&service, &store, NODE_A).await;
        let auto: Vec<_> = service
            .jobs_for(NODE_A, None)
            .await
            .expect("list")
            .into_iter()
            .filter(|j| j.trigger == AUTO_TRIGGER)
            .collect();
        assert_eq!(auto.len(), 1, "one auto job for the span of the holes");
        assert!(auto.iter().all(|j| j.status == JOB_PENDING));

        service
            .handle_transition(&store, went_offline(NODE_A))
            .await;
        let jobs = service.jobs_for(NODE_A, None).await.expect("list");
        for job in jobs.iter().filter(|j| j.trigger == AUTO_TRIGGER) {
            assert_eq!(job.status, "cancelled", "job {}", job.job_id);
            assert_eq!(
                job.error_message.as_deref(),
                Some("node went offline again")
            );
        }
        let manual_row = jobs
            .iter()
            .find(|j| j.job_id == manual.job_id)
            .expect("manual row");
        assert_eq!(
            manual_row.status, JOB_PENDING,
            "an operator who asked for a window asked for it — §6.1"
        );

        // Nothing active overlaps the gaps any more, so the next recovery
        // re-detects them instead of finding its own cancelled rows.
        recover(&service, &store, NODE_A).await;
        let fresh: Vec<_> = service
            .jobs_for(NODE_A, None)
            .await
            .expect("list")
            .into_iter()
            .filter(|j| j.trigger == AUTO_TRIGGER && j.status == JOB_PENDING)
            .collect();
        assert_eq!(fresh.len(), 1, "the second recovery re-queues the span");
    }

    /// Item 5: once an automatic job served a span, the next recovery asks only for the holes
    /// after it; the older ones were asked for already, and what is still a hole there the Node
    /// does not hold.
    #[tokio::test]
    async fn the_next_recovery_asks_only_after_the_last_served_span() {
        let (service, dir, t0) = service_with_two_gaps().await;
        let store = unconfigured_store();
        let storage = side_door(&dir);
        recover(&service, &store, NODE_A).await;
        let first = service.jobs_for(NODE_A, None).await.expect("list");
        storage
            .update_job_status(first[0].job_id, portal_storage::JOB_DONE, None, false)
            .expect("served");

        // A newer hole after the served span.
        for t in [t0 + 6_000, t0 + 8_000] {
            seed_reading(&storage, NODE_A, t);
        }
        recover(&service, &store, NODE_A).await;
        let jobs = service.jobs_for(NODE_A, None).await.expect("list");
        let newest = jobs.iter().max_by_key(|j| j.job_id).expect("a second job");
        assert_eq!(jobs.len(), 2);
        assert_eq!(
            (newest.time_from, newest.time_to),
            (t0 + 4_000, t0 + 8_000),
            "only the holes after the served span"
        );
    }

    /// Item 6: at most `auto_max_per_10min` automatic requests go to one Hub in 10 minutes;
    /// the next automatic job waits pending, while a manual one is never held back (and counts).
    #[tokio::test]
    async fn the_hub_limit_holds_back_automatic_requests_but_not_manual_ones() {
        let cfg = BackfillConfig {
            auto_max_per_10min: 1,
            min_request_interval_s: 0,
            ..BackfillConfig::default()
        };
        let (service, _dir) = service_with(RecordingPublisher::default(), cfg).await;
        let store = unconfigured_store();
        let now = now_unix();
        for node in [NODE_A, NODE_B] {
            service
                .request(HOST, node, now - 7_200, now - 3_600, AUTO_TRIGGER)
                .await
                .expect("auto job");
        }
        service.drain_pass(&store, &[HOST.to_string()]).await;
        let status =
            |jobs: Vec<BackfillJob>| jobs.into_iter().map(|j| j.status).collect::<Vec<_>>();
        let a = status(service.jobs_for(NODE_A, None).await.expect("a"));
        let b = status(service.jobs_for(NODE_B, None).await.expect("b"));
        assert_eq!(
            [a, b]
                .concat()
                .iter()
                .filter(|s| *s == JOB_REQUESTED)
                .count(),
            1,
            "one automatic request for the Hub's 10 minutes"
        );

        const NODE_C: &str = "aabbccdd00000003";
        service
            .request(HOST, NODE_C, now - 7_200, now - 3_600, "manual")
            .await
            .expect("manual job");
        service.drain_pass(&store, &[HOST.to_string()]).await;
        let c = service.jobs_for(NODE_C, None).await.expect("c");
        assert_eq!(
            c[0].status, JOB_REQUESTED,
            "a manual request is never held back"
        );

        let zero = BackfillConfig {
            auto_max_per_10min: 0,
            ..BackfillConfig::default()
        };
        let (service, _dir) = service_with(RecordingPublisher::default(), zero).await;
        service
            .request(HOST, NODE_A, now - 7_200, now - 3_600, AUTO_TRIGGER)
            .await
            .expect("auto job");
        service.drain_pass(&store, &[HOST.to_string()]).await;
        assert_eq!(
            service.jobs_for(NODE_A, None).await.expect("a")[0].status,
            JOB_PENDING,
            "0 sends no automatic request"
        );
    }

    /// Item 5 (amendment 2026-09-25): a new offline cancels only an automatic job not sent
    /// yet. One whose request went out keeps going, because its downlink is spent and its
    /// frames may still come.
    #[tokio::test]
    async fn going_offline_leaves_an_auto_job_already_in_flight() {
        let (service, _dir, _t0) = service_with_two_gaps().await;
        let store = unconfigured_store();
        recover(&service, &store, NODE_A).await;
        service.drain_pass(&store, &[HOST.to_string()]).await;
        let in_flight = service
            .jobs_for(NODE_A, None)
            .await
            .expect("list")
            .into_iter()
            .find(|j| j.status == JOB_REQUESTED)
            .expect("one job in flight after the drain");

        service
            .handle_transition(&store, went_offline(NODE_A))
            .await;
        let row = service
            .jobs_for(NODE_A, None)
            .await
            .expect("list")
            .into_iter()
            .find(|j| j.job_id == in_flight.job_id)
            .expect("row");
        assert_eq!(row.status, JOB_REQUESTED);
    }

    /// Item 5: a recovery only schedules. The download is due 2 x `interval_report` after it,
    /// and no sooner than an hour after the Node's previous automatic request, so a Node that
    /// flaps on a weak link asks once an hour for everything since.
    #[tokio::test]
    async fn a_flapping_node_asks_at_most_once_an_hour_for_the_whole_span() {
        let (service, _dir, _t0) = service_with_two_gaps().await;
        let store = unconfigured_store();
        let now = now_unix();

        service.handle_transition(&store, came_back(NODE_A)).await;
        service.run_due_auto(&store, now + 10).await;
        assert!(
            service
                .jobs_for(NODE_A, None)
                .await
                .expect("list")
                .is_empty(),
            "not at once: the link has to hold for two report intervals"
        );
        service
            .run_due_auto(&store, now + 2 * DEFAULT_INTERVAL_REPORT_S + 1)
            .await;
        let jobs = service.jobs_for(NODE_A, None).await.expect("list");
        assert_eq!(jobs.len(), 1);
        service.drain_pass(&store, &[HOST.to_string()]).await;

        // Offline and back within the hour: scheduled for an hour after that request.
        service
            .handle_transition(&store, went_offline(NODE_A))
            .await;
        service.handle_transition(&store, came_back(NODE_A)).await;
        service.run_due_auto(&store, now + 1_800).await;
        assert_eq!(service.jobs_for(NODE_A, None).await.expect("list").len(), 1);
        let due = service
            .auto_due
            .lock()
            .get(&(HOST.to_string(), NODE_A.to_string()))
            .copied();
        assert!(
            due >= Some(now + 3_600),
            "due an hour after the last automatic request: {due:?}"
        );
    }

    /// §6.4: `device_meta.backfill_auto = 0` blocks the automatic path and
    /// nothing else.
    #[tokio::test]
    async fn the_per_node_toggle_blocks_auto_and_not_manual() {
        let (service, dir, t0) = service_with_two_gaps().await;
        let store = unconfigured_store();
        side_door(&dir)
            .upsert_device_meta(&portal_storage::NewDeviceMeta {
                backfill_auto: false,
                ..portal_storage::NewDeviceMeta::new(
                    HOST,
                    portal_storage::THRESHOLD_SCOPE_NODE,
                    NODE_A,
                )
            })
            .expect("disable auto backfill");

        recover(&service, &store, NODE_A).await;
        assert!(
            service
                .jobs_for(NODE_A, None)
                .await
                .expect("list")
                .is_empty(),
            "the toggle blocks the automatic path"
        );

        let manual = service
            .request(HOST, NODE_A, t0, t0 + 2_000, "manual")
            .await
            .expect("manual job runs regardless of the toggle");
        assert_eq!(manual.trigger, "manual");
    }

    /// A store where `NODE_A`'s config info says `application.history_enable = enabled`.
    fn store_with_history(enabled: bool) -> StateStore {
        use crate::state::events::IngestEvent;
        let store = StateStore::new();
        store.apply(IngestEvent::NodeDeviceConfig {
            hostname: HOST.into(),
            node_key: NODE_A.into(),
            payload: Some(Box::new(
                serde_json::from_value(json!({
                    "dev_eui": NODE_A,
                    "received_at": "2026-09-25T09:00:00Z",
                    "seq": 0,
                    "page_count": 1,
                    "config": {"application.history_enable": enabled},
                }))
                .expect("config document"),
            )),
        });
        store
    }

    /// devices.node.md §3.9, amendment 2026-09-25 (P-8): a Node that reports it keeps no
    /// history is not asked on its return, because every window would be refused and count
    /// toward the "cannot download history" fault. One that keeps it is.
    #[tokio::test]
    async fn a_node_that_keeps_no_history_is_not_asked_on_its_return() {
        let (service, _dir, _t0) = service_with_two_gaps().await;
        recover(&service, &store_with_history(false), NODE_A).await;
        assert!(service
            .jobs_for(NODE_A, None)
            .await
            .expect("list")
            .is_empty());

        recover(&service, &store_with_history(true), NODE_A).await;
        assert!(!service
            .jobs_for(NODE_A, None)
            .await
            .expect("list")
            .is_empty());
    }

    /// A row for a *different* Node must not silence this one — the lookup
    /// is keyed, and a missing row means "every default", i.e. enabled.
    #[tokio::test]
    async fn a_missing_or_foreign_meta_row_leaves_auto_backfill_enabled() {
        let (service, dir, _t0) = service_with_two_gaps().await;
        let store = unconfigured_store();
        side_door(&dir)
            .upsert_device_meta(&portal_storage::NewDeviceMeta {
                backfill_auto: false,
                ..portal_storage::NewDeviceMeta::new(
                    HOST,
                    portal_storage::THRESHOLD_SCOPE_NODE,
                    NODE_B,
                )
            })
            .expect("disable auto backfill for the other Node");

        recover(&service, &store, NODE_A).await;
        assert_eq!(
            service.jobs_for(NODE_A, None).await.expect("list").len(),
            1,
            "NODE_B's toggle says nothing about NODE_A"
        );
    }

    // ---- §19 (F11): the automatic path leaves a trail ---------------------

    /// A Node reconnecting makes this service send `get_sticker_history`
    /// downlinks to hardware. Before §19 `backfill.rs` contained no `audit`
    /// call at all, so an operator reconciling device traffic against the
    /// audit page saw downlinks with no originating action
    /// (devices.node.md §3.9).
    #[tokio::test]
    async fn an_automatic_backfill_writes_one_system_actor_audit_row() {
        let (service, dir, _t0) = service_with_two_gaps().await;
        let store = unconfigured_store();
        let storage = side_door(&dir);

        recover(&service, &store, NODE_A).await;

        let (rows, total) = storage
            .list_audit(&portal_storage::AuditFilter {
                event_type: Some("node_backfill_auto".to_string()),
                ..Default::default()
            })
            .expect("audit page");
        assert_eq!(total, 1, "one row for the whole recovery, not one per job");
        let row = &rows[0];
        assert_eq!(row.username.as_deref(), Some("system:backfill"));
        assert_eq!(row.user_id, None, "no human is behind this");
        assert_eq!(row.target.as_deref(), Some(NODE_A));
        assert!(row.success);
        let details: Value =
            serde_json::from_str(row.details.as_deref().expect("details")).expect("json");
        assert_eq!(details["jobs"], 1, "one job for the span");
        assert_eq!(details["trigger"], AUTO_TRIGGER);
    }

    /// The toggle blocks the path, so there is nothing to audit — the row
    /// records a downlink actually going out, not the service looking.
    #[tokio::test]
    async fn a_recovery_that_creates_no_job_writes_no_audit_row() {
        let (service, dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let store = unconfigured_store();
        let storage = side_door(&dir);

        // No readings seeded, so no gaps are detected and no job is made.
        recover(&service, &store, NODE_A).await;

        let (_, total) = storage
            .list_audit(&portal_storage::AuditFilter {
                event_type: Some("node_backfill_auto".to_string()),
                ..Default::default()
            })
            .expect("audit page");
        assert_eq!(total, 0);
    }

    // ---- §19 (F12): the terminal job state reaches the fault axis ---------

    /// The read behind [`crate::render::NodeFault::HistoryUnavailable`].
    /// Latest-window-only, so the fault clears by itself when the next job —
    /// an operator's or the next recovery's — supersedes the failure.
    #[tokio::test]
    async fn only_a_nodes_latest_job_decides_the_history_fault() {
        let (service, dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let storage = side_door(&dir);
        let now = now_unix();

        let failed = service
            .request(HOST, NODE_A, now - 7_200, now - 3_600, "manual")
            .await
            .expect("first job");
        storage
            .update_job_status(failed.job_id, "failed", Some("retries exhausted"), false)
            .expect("fail it");

        assert_eq!(
            storage.nodes_with_failed_backfill(HOST).expect("query"),
            vec![NODE_A.to_string()]
        );

        // A newer job for the same Node supersedes the failure.
        service
            .request(HOST, NODE_A, now - 1_800, now - 900, "manual")
            .await
            .expect("second job");
        assert!(
            storage
                .nodes_with_failed_backfill(HOST)
                .expect("query")
                .is_empty(),
            "a retry clears the fault without anyone deleting a row"
        );
    }

    /// N37 (bench, 2026-09-25): a recovery creates its jobs newest hole first, so the job
    /// created last is the oldest hole, typically one from before the Node's history began,
    /// which the Node refuses. Deciding the fault by the newest *window* rather than the newest
    /// job keeps a recovery whose outage came down whole from flagging the Node.
    #[tokio::test]
    async fn the_newest_window_decides_the_history_fault_not_the_last_job() {
        let (service, dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let storage = side_door(&dir);
        let now = now_unix();
        let recent = service
            .request(HOST, NODE_A, now - 1_800, now - 900, "auto-on-reconnect")
            .await
            .expect("the outage");
        let old = service
            .request(
                HOST,
                NODE_A,
                now - 90_000,
                now - 86_400,
                "auto-on-reconnect",
            )
            .await
            .expect("a hole from before the history began");
        storage
            .update_job_status(recent.job_id, "done", None, false)
            .expect("done");
        storage
            .update_job_status(old.job_id, "failed", Some("device refused"), true)
            .expect("refused");
        assert!(storage
            .nodes_with_failed_backfill(HOST)
            .expect("query")
            .is_empty());

        // The same window asked again and failed again is a newer attempt: it decides.
        let retry = service
            .request(HOST, NODE_A, now - 1_800, now - 900, "manual")
            .await
            .expect("a retry of the outage");
        storage
            .update_job_status(retry.job_id, "failed", Some("retries exhausted"), false)
            .expect("fail");
        assert_eq!(
            storage.nodes_with_failed_backfill(HOST).expect("query"),
            vec![NODE_A.to_string()]
        );
    }

    /// A refused newest window is the Node's answer, not a history fault: no §6.3 alert, the
    /// row names the reason, and it supersedes an older exhaustion (bench 2026-09-27, 0413).
    #[tokio::test]
    async fn a_refused_newest_window_is_not_a_history_fault() {
        let (service, dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let storage = side_door(&dir);
        let now = now_unix();
        let exhausted = service
            .request(HOST, NODE_A, now - 7_200, now - 3_600, "manual")
            .await
            .expect("an older window");
        storage
            .update_job_status(exhausted.job_id, "failed", Some("retries exhausted"), false)
            .expect("exhaust");
        assert_eq!(
            storage.nodes_with_failed_backfill(HOST).expect("query"),
            vec![NODE_A.to_string()]
        );

        let refused = service
            .request(HOST, NODE_A, now - 1_800, now - 900, "auto-on-reconnect")
            .await
            .expect("the newest window");
        storage
            .update_job_status(
                refused.job_id,
                "failed",
                Some("device refused: out_of_range"),
                true,
            )
            .expect("refuse");
        assert!(storage
            .nodes_with_failed_backfill(HOST)
            .expect("query")
            .is_empty());
    }

    /// N37: a window the Node refused is not asked for again by the next recovery.
    #[tokio::test]
    async fn a_recovery_does_not_ask_again_for_a_window_the_node_refused() {
        let (service, dir, _t0) = service_with_two_gaps().await;
        let store = unconfigured_store();
        recover(&service, &store, NODE_A).await;
        let first = service.jobs_for(NODE_A, None).await.expect("list");
        assert_eq!(first.len(), 1, "one span for the two holes: {first:?}");
        let storage = side_door(&dir);
        for job in &first {
            storage
                .update_job_status(
                    job.job_id,
                    "failed",
                    Some("device refused: history_unavailable"),
                    true,
                )
                .expect("refuse");
        }

        recover(&service, &store, NODE_A).await;
        assert_eq!(
            service.jobs_for(NODE_A, None).await.expect("list").len(),
            1,
            "no new job for either refused hole"
        );
        service
            .request(HOST, NODE_A, first[0].time_from, first[0].time_to, "manual")
            .await
            .expect("the form still asks");
    }

    /// A store holding `NODE_A` on `HOST` as a real Node — one telemetry
    /// publish is what creates one, and the fault sync only writes Nodes the
    /// store already knows.
    fn store_with_node() -> StateStore {
        use crate::state::events::IngestEvent;
        let store = StateStore::new();
        store.apply(IngestEvent::LorawanSensors {
            hostname: HOST.into(),
            // A publish with no `fields` is dropped (v1 parity), so the
            // Node needs one reading to exist at all.
            payload: serde_json::from_value(
                json!({ "sensors": [{ "dev_eui": NODE_A, "fields": { "temperature": 21.0 } }] }),
            )
            .expect("a minimal sensors publish"),
        });
        store
    }

    fn history_fault(store: &StateStore) -> bool {
        store
            .snapshot_device(HOST)
            .expect("device")
            .nodes
            .get(NODE_A)
            .expect("node")
            .history_unavailable
    }

    /// How many `ChangeEvent::Node` for `NODE_A` are waiting — the repaint
    /// the row needs, same event every other Node fault rides.
    fn node_repaints(rx: &mut broadcast::Receiver<ChangeEvent>) -> usize {
        let mut seen = 0;
        while let Ok(change) = rx.try_recv() {
            if matches!(change, ChangeEvent::Node { ref node_key, .. } if node_key == NODE_A) {
                seen += 1;
            }
        }
        seen
    }

    /// #64: the overview strip counts faults synchronously off the store, so
    /// exhaustion has to land on `NodeState::history_unavailable` — and a
    /// later job has to take it off again, each with a repaint.
    #[tokio::test]
    async fn exhaustion_flags_the_node_in_the_store_and_a_later_job_clears_it() {
        let (service, dir) =
            service_with(RecordingPublisher::default(), instant_retry_cfg(0)).await;
        let store = store_with_node();
        let mut changes = store.subscribe();
        let now = now_unix();

        service
            .request(HOST, NODE_A, now - 7_200, now - 3_600, "manual")
            .await
            .expect("first job");
        service.drain_pass(&store, &[HOST.to_string()]).await;
        assert!(!history_fault(&store), "a job in flight is not a fault");

        // max_retries 0: the first reconciler pass gives up on it.
        service.reconcile_pass(&store, &[HOST.to_string()]).await;
        assert!(history_fault(&store), "retries exhausted is the fault");
        assert_eq!(node_repaints(&mut changes), 1, "the row repaints on it");

        // A later job for the same Node supersedes the failure, and the
        // fault goes with it (the latest-job rule `nodes_with_failed_backfill`
        // defines).
        let next = service
            .request(HOST, NODE_A, now - 1_800, now - 900, "manual")
            .await
            .expect("second job");
        side_door(&dir)
            .update_job_status(next.job_id, "done", None, false)
            .expect("finish it");
        service.drain_pass(&store, &[HOST.to_string()]).await;
        assert!(!history_fault(&store), "a finished job clears the fault");
        assert_eq!(node_repaints(&mut changes), 1, "and repaints the row again");
    }

    /// Beacons have no `get_sticker_history`; their archive backfill is
    /// `control/ble`'s (§12.1). Their transitions ride the same broadcast,
    /// so the scope guard is load-bearing.
    #[tokio::test]
    async fn beacon_transitions_are_ignored() {
        let (service, _dir, _t0) = service_with_two_gaps().await;
        let store = unconfigured_store();
        let beacon = HubTransition {
            hostname: HOST.to_string(),
            transition: OfflineTransition {
                scope: DeviceScope::Beacon,
                entity: "AA:BB:CC:DD:EE:FF".to_string(),
                transition: Transition::CameBack,
                silent_for_s: 3_600,
            },
        };
        service.handle_transition(&store, beacon).await;
        assert!(
            service
                .jobs_for(NODE_A, None)
                .await
                .expect("list")
                .is_empty(),
            "a Beacon recovery must not queue Node history"
        );
    }

    /// The transition carries whatever casing the detector held; job rows
    /// and `jobs_for` are lowercase (V6's `node_key`).
    #[tokio::test]
    async fn an_uppercase_transition_entity_still_matches_its_jobs() {
        let (service, _dir, _t0) = service_with_two_gaps().await;
        let store = unconfigured_store();
        service
            .handle_transition(&store, came_back(&NODE_A.to_uppercase()))
            .await;
        service.run_due_auto(&store, now_unix() + 86_400).await;
        assert_eq!(
            service.jobs_for(NODE_A, None).await.expect("list").len(),
            1,
            "the trigger lowercases before it touches storage"
        );
    }

    /// The wiring itself, which no other test in this file covers: a real
    /// [`crate::offline::OfflineService`] sweep, a real `broadcast`, and
    /// the task [`BackfillService::spawn_tasks`] actually spawns — the
    /// three pieces `main.rs`'s ordering comment is about. Every test
    /// above calls `handle_transition` directly and would pass with
    /// `run_transitions` never spawned at all.
    ///
    /// Multi-threaded and real-time on purpose: the path crosses
    /// `spawn_blocking` twice (the sweep's `device_meta` reads, the
    /// service's storage), and those are not ordered by
    /// `tokio::time::advance` — the same reason `offline.rs`'s
    /// `the_spawned_sweep_ticks_on_its_own` refuses a paused clock.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_real_sweep_recovery_reaches_the_spawned_trigger() {
        use crate::state::events::{IngestEvent, NodeReadingMsg};

        // 2026-01-13T14:30:00Z, the timestamp `offline.rs`'s tests pin.
        const SEEN_AT: i64 = 1_768_314_600;

        // Due at once and drained every second, so the spawned drain fires it within the poll.
        let cfg = BackfillConfig {
            auto_settle_ticks: 0,
            auto_min_interval_s: 0,
            drain_period_s: 1,
            ..BackfillConfig::default()
        };
        let (service, dir) = service_with(RecordingPublisher::default(), cfg).await;
        let storage = side_door(&dir);
        let t0 = now_unix() - 10_000;
        for t in [t0, t0 + 2_000, t0 + 2_300, t0 + 4_000] {
            seed_reading(&storage, NODE_A, t);
        }

        // One Node on HOST, last heard from at SEEN_AT. The service's own
        // db is reused for the sweep's `device_meta` reads, so both halves
        // see one database, exactly as they do in `main`.
        let store = Arc::new(StateStore::new());
        store.apply(IngestEvent::NodeReading {
            hostname: HOST.into(),
            node_key: NODE_A.into(),
            payload: Box::new(NodeReadingMsg {
                message_id: "m-1".into(),
                dev_eui: NODE_A.into(),
                serial: None,
                fields: [("temperature".to_string(), 4.2)].into_iter().collect(),
                counters: Default::default(),
                events: Vec::new(),
                gateways: Vec::new(),
                rssi: None,
                snr: None,
                fport: None,
                page_index: None,
                page_count: None,
                dr: None,
                received_at: "2026-01-13T14:30:00Z".to_string(),
            }),
        });

        let db =
            crate::db::Db::open(dir.path().to_str().expect("utf8"), None, "test").expect("open db");
        let offline = crate::offline::OfflineService::test_instance(db);

        // The ordering `main.rs` documents: subscribe BEFORE the sweep can
        // run. Reversed, this test is what fails.
        service.spawn_tasks(Arc::clone(&store), Some(offline.subscribe_transitions()));

        // No config for this Node, so the detector's 900 s fallback and the
        // default K = 4 give a 3 600 s budget.
        let went = offline.pass(&store, SEEN_AT + 3_601).await;
        assert_eq!(went.len(), 1, "the sweep must produce the onset");
        assert_eq!(went[0].transition, Transition::WentOffline);

        // One fresh uplink, then a sweep inside the new budget → recovery.
        store.apply(IngestEvent::NodeReading {
            hostname: HOST.into(),
            node_key: NODE_A.into(),
            payload: Box::new(NodeReadingMsg {
                message_id: "m-2".into(),
                dev_eui: NODE_A.into(),
                serial: None,
                fields: [("temperature".to_string(), 4.3)].into_iter().collect(),
                counters: Default::default(),
                events: Vec::new(),
                gateways: Vec::new(),
                rssi: None,
                snr: None,
                fport: None,
                page_index: None,
                page_count: None,
                dr: None,
                received_at: "2026-01-13T15:30:00Z".to_string(),
            }),
        });
        let back = offline.pass(&store, SEEN_AT + 3_700).await;
        assert_eq!(back.len(), 1, "the sweep must produce the recovery");
        assert_eq!(back[0].transition, Transition::CameBack);

        // The spawned tasks are on another thread; poll rather than sleep a fixed amount.
        let mut jobs = Vec::new();
        for _ in 0..200 {
            jobs = service.jobs_for(NODE_A, None).await.expect("list");
            if !jobs.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(
            jobs.len(),
            1,
            "the recovery must reach the spawned trigger and queue the span"
        );
        assert!(jobs.iter().all(|j| j.trigger == AUTO_TRIGGER));
    }

    // ---- B3: the gap list (§6, closing paragraph) -------------------------

    /// What `partials/backfill_jobs.html` renders: newest hole first, the
    /// window pre-formatted, the span in h/min, and the count the Czech
    /// spec asks for (devices.node.md §3.9 item 1).
    #[tokio::test]
    async fn gaps_ctx_renders_the_newest_hole_first_with_its_missing_count() {
        let (service, dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let storage = side_door(&dir);
        let now = now_unix();
        let t0 = now - 40_000;
        // Two holes at a 600 s cadence: 3 600 s (5 missing) then
        // 12 000 s (19 missing).
        for t in [t0, t0 + 3_600, t0 + 4_200, t0 + 16_200] {
            seed_reading(&storage, NODE_A, t);
        }

        let gaps = service
            .gaps_ctx(&store_reporting_every(600), HOST, NODE_A)
            .await;
        assert_eq!(gaps.len(), 2);
        assert_eq!(gaps[0].duration, "3 h 20 min", "newest hole first");
        assert_eq!(gaps[0].samples_expected, 19);
        assert_eq!(gaps[0].time_from, format_ts_minutes(t0 + 4_200));
        assert_eq!(gaps[0].time_to, format_ts_minutes(t0 + 16_200));
        assert_eq!(gaps[1].duration, "1 h");
        assert_eq!(gaps[1].samples_expected, 5);
    }

    /// P-8: a hole the Node already answered for (a finished or refused job covers it) offers
    /// no download, and while the Node keeps no history no hole does.
    #[tokio::test]
    async fn a_hole_with_nothing_to_download_says_why() {
        let (service, dir, t0) = service_with_two_gaps().await;
        let storage = side_door(&dir);
        let job = storage
            .create_job(&portal_storage::NewBackfillJob {
                hostname: HOST.into(),
                node_key: NODE_A.into(),
                time_from: t0,
                time_to: t0 + 2_000,
                trigger: "manual".into(),
                samples_expected: None,
            })
            .expect("job");
        storage
            .update_job_status(job.job_id, portal_storage::JOB_DONE, None, false)
            .expect("served");

        let gaps = service.gaps_ctx(&unconfigured_store(), HOST, NODE_A).await;
        assert_eq!(gaps.len(), 2);
        assert_eq!(
            gaps[0].unavailable, None,
            "the newer hole is still asked for"
        );
        assert_eq!(gaps[1].unavailable, Some("answered"));

        let gaps = service
            .gaps_ctx(&store_with_history(false), HOST, NODE_A)
            .await;
        assert!(gaps.iter().all(|g| g.unavailable == Some("history_off")));
    }

    /// The list is capped at what the queue can act on, so it never
    /// promises more than [`MAX_AUTO_JOBS_PER_NODE`] jobs' worth of work.
    #[tokio::test]
    async fn gaps_ctx_caps_at_the_auto_job_bound() {
        let (service, dir) =
            service_with(RecordingPublisher::default(), BackfillConfig::default()).await;
        let storage = side_door(&dir);
        let mut t = now_unix() - 30 * (MAX_RENDERED_GAPS as i64 + 4) * 700;
        for _ in 0..MAX_RENDERED_GAPS + 4 {
            seed_reading(&storage, NODE_A, t);
            t += 700; // > 2 x 300 → every delta is a gap
        }
        let gaps = service.gaps_ctx(&unconfigured_store(), HOST, NODE_A).await;
        assert_eq!(gaps.len(), MAX_RENDERED_GAPS);
    }

    #[test]
    fn missing_samples_never_goes_negative() {
        assert_eq!(missing_samples(0, 3_600, 600), 5);
        assert_eq!(missing_samples(0, 600, 600), 0, "one slot, none missing");
        assert_eq!(missing_samples(0, 100, 600), 0, "shorter than one slot");
        assert_eq!(missing_samples(0, 3_600, 0), 0, "no cadence, no estimate");
    }

    #[test]
    fn format_duration_rounds_up_to_the_minute() {
        assert_eq!(format_duration(0), "0 min");
        assert_eq!(format_duration(1), "1 min");
        assert_eq!(format_duration(3_600), "1 h");
        assert_eq!(format_duration(3_660), "1 h 1 min");
        assert_eq!(format_duration(-5), "0 min");
    }

    // ---- ctx mapping -------------------------------------------------------

    #[test]
    fn job_ctx_formats_timestamps_for_the_template() {
        let job = BackfillJob {
            job_id: 7,
            hostname: HOST.to_string(),
            node_key: NODE_A.to_string(),
            status: JOB_PENDING.to_string(),
            time_from: 1_755_500_400, // 2025-08-18 07:00 UTC
            time_to: 1_755_504_000,   // 2025-08-18 08:00 UTC
            trigger: "manual".to_string(),
            samples_expected: None,
            samples_received: 0,
            retry_count: 0,
            error_message: None,
            device_refused: false,
            requested_at: None,
            not_before: None,
            last_sample_at: None,
            created_at: 1_755_504_000,
            updated_at: 1_755_504_000,
        };
        let ctx = job_ctx(&job);
        assert_eq!(ctx.time_from, "2025-08-18 07:00");
        assert_eq!(ctx.time_to, "2025-08-18 08:00");
        assert_eq!(ctx.created_at, "2025-08-18 08:00");
        assert_eq!(ctx.job_id, 7);
        assert_eq!(ctx.status, "pending");
    }

    /// decisions.md #25/#32: the jobs table shows a window in the display
    /// zone the form beside it is typed in.
    #[test]
    fn job_times_are_shown_in_the_display_zone() {
        let prague = jiff::tz::TimeZone::get("Europe/Prague").unwrap();
        // 2025-08-18 07:00 UTC is 09:00 in Prague (CEST).
        assert_eq!(
            format_ts_minutes_in(1_755_500_400, &prague),
            "2025-08-18 09:00"
        );
    }
}
