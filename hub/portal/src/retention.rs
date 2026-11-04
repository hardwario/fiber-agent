//! Retention task (M7): runs `Storage::sweep_retention` on a slow timer.
//!
//! What gets pruned and what is kept as evidence is documented on
//! `portal_storage::retention_repo` — that decision lives next to the SQL, not
//! here. This module only owns *when* the sweep runs and how it reports.
//!
//! It gets its own `Storage` connection (the last one) and does its work on the
//! blocking pool: a sweep is a long `DELETE` plus an occasional `VACUUM`, and it
//! must never sit in the async path where a browser request or an MQTT ingest
//! batch is waiting on the same runtime worker.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use portal_storage::{PageStats, RetentionPolicy, SpillGate, Storage, StorageError};
use tokio::sync::mpsc;

use crate::config::MaintenanceWindow;
use crate::persist::PersistMsg;

/// How often the sweep runs. Retention windows are measured in days, so a
/// 6-hour cadence is four chances a day to notice — frequent enough that
/// `/data` never surprises anyone, rare enough to be invisible.
const SWEEP_INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// First sweep is delayed so it never competes with startup (migrations,
/// retained-MQTT replay, the first render).
const STARTUP_DELAY: Duration = Duration::from_secs(120);
/// Pages handed back per sweep by the incremental reclaim.
///
/// This is a lock-duration budget, not a throughput target. `incremental_vacuum`
/// takes the write lock while it moves pages, and the writer it would block is
/// the persist thread — which is where the alarm engine runs. 2 048 pages is
/// 8 MB at the default 4 KiB page size, or ~32 MB/day across four sweeps, which
/// comfortably outpaces what retention frees on this workload while keeping any
/// single pause short.
///
/// A full `VACUUM` is deliberately not run *here*: it locks writers out for its
/// entire duration (measured: ~3 s for 1.2 GB on a tmpfs, and this device writes
/// to eMMC), so it belongs in the maintenance window rather than in a sweep that
/// runs four times a day. See [`RetentionTask::maintenance_tick`] and
/// `routes::settings_routes`.
const INCREMENTAL_VACUUM_PAGE_CAP: u32 = 2_048;

/// How often to re-evaluate while inside a maintenance window.
///
/// The window bounds when a rewrite may *start*, so a tick that decides "not
/// yet" (no space, a run already going) should get another chance before the
/// window closes rather than waiting for tomorrow.
const MAINTENANCE_RECHECK_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Minutes in a day, for the window's wrap arithmetic.
const MINUTES_PER_DAY: u32 = 24 * 60;

/// Minutes past midnight back to `HH:MM`, for logs and the settings page.
pub fn format_hh_mm(minute_of_day: u32) -> String {
    let m = minute_of_day % MINUTES_PER_DAY;
    format!("{:02}:{:02}", m / 60, m % 60)
}

#[derive(Clone)]
pub struct RetentionTask {
    storage: Arc<Mutex<Storage>>,
    policy: RetentionPolicy,
    /// Kept so the maintenance preflight can measure free space on the
    /// filesystem the database actually lives on.
    data_dir: String,
    /// Raised for the duration of a rewrite so the other seven connections
    /// defer their writes to disk instead of losing them — see `crate::durable`.
    spill: Arc<SpillGate>,
    /// How the rewrite tells the persist thread to drain the journal. `None`
    /// only in tests, where nothing is ingesting.
    persist_tx: Option<mpsc::UnboundedSender<PersistMsg>>,
    /// When a scheduled rewrite may start. `None` = no scheduled maintenance,
    /// which is the default and what every existing deployment gets until its
    /// config says otherwise.
    window: Option<MaintenanceWindow>,
}

impl RetentionTask {
    pub fn open(
        data_dir: &str,
        db_key: Option<&str>,
        policy: RetentionPolicy,
        spill: Arc<SpillGate>,
        persist_tx: Option<mpsc::UnboundedSender<PersistMsg>>,
    ) -> Result<Self, StorageError> {
        std::fs::create_dir_all(data_dir)?;
        let path = std::path::Path::new(data_dir).join("portal.db");
        let storage = Storage::open(&path, db_key)?;

        // This is the only connection that ever runs a `VACUUM`, so it is the
        // only one that wants temp files on disk; the rest keep in-memory temp
        // storage for fast sorts. See `Storage::set_temp_store_file`.
        if let Err(err) = storage.set_temp_store_file() {
            tracing::warn!(%err, "could not route SQLite temp files to disk; a full VACUUM would run in RAM");
        }

        // A `full_vacuum_running` flag can only be left set by a process that
        // died mid-rewrite. Nothing is running yet, so clear it here.
        match storage.clear_stale_full_vacuum_flag() {
            Ok(true) => {
                tracing::warn!(
                    "cleared a full-VACUUM flag left over from a previous run; \
                     the database is intact (VACUUM is transactional) but the rewrite did not finish"
                );
                // A power cut or a kill mid-rewrite must be readable from the
                // audit trail itself, not inferred from a `started` row with no
                // `finished` plus a journald line nobody kept. The database is
                // unharmed — VACUUM rolls back atomically — and the spill
                // journal replays at this same startup; the row records that
                // the repack was interrupted rather than completed.
                //
                // The actor is copied from the `_started` row rather than left
                // `None`. `/audit` renders an actor-less maintenance row as
                // "scheduled", so a hard-coded `None` here claimed the timer had
                // started a rewrite an administrator actually clicked — and the
                // `_started` row two lines above it in the same table still
                // named them.
                let actor = interrupted_run_actor(&storage);
                let trigger = if actor.is_some() {
                    "manual"
                } else {
                    "schedule"
                };
                write_maintenance_audit(
                    &spill,
                    &storage,
                    actor.as_ref(),
                    "database_maintenance_interrupted",
                    false,
                    Some(format!(
                        r#"{{"reason":"process_died_mid_rewrite","trigger":"{trigger}"}}"#
                    )),
                );
            }
            Ok(false) => {}
            Err(err) => tracing::warn!(%err, "could not reconcile the full-VACUUM flag"),
        }

        // A database created before `auto_vacuum=INCREMENTAL` was requested
        // keeps no pointer map, so the incremental reclaim below is a silent
        // no-op on it and `/data` grows without bound. Loud, because the
        // symptom otherwise appears months later as a full partition.
        match storage.auto_vacuum_mode() {
            Ok(2) => {}
            Ok(mode) => tracing::error!(
                auto_vacuum = mode,
                "this database predates auto_vacuum=INCREMENTAL, so pages are never reclaimed \
                 automatically; an administrator must run database maintenance once from \
                 /settings to convert it"
            ),
            Err(err) => tracing::warn!(%err, "could not read auto_vacuum mode"),
        }

        Ok(Self {
            storage: Arc::new(Mutex::new(storage)),
            policy,
            data_dir: data_dir.to_string(),
            spill,
            persist_tx,
            window: None,
        })
    }

    /// Attach the configured maintenance window.
    ///
    /// A builder step rather than a sixth positional argument to
    /// [`RetentionTask::open`], which already takes five: the six call sites in
    /// tests all want `None`, and a scheduled rewrite is exactly the thing that
    /// should have to be asked for explicitly.
    pub fn with_maintenance_window(mut self, window: Option<MaintenanceWindow>) -> Self {
        match window {
            Some(w) => tracing::info!(
                start_utc = %format_hh_mm(w.start_minute_utc),
                duration_minutes = w.duration_minutes,
                "scheduled database maintenance is enabled"
            ),
            None => tracing::info!(
                "no maintenance window configured; full VACUUM stays operator-initiated"
            ),
        }
        self.window = window;
        self
    }

    /// A narrow handle on the same connection for the operator-initiated
    /// maintenance action.
    ///
    /// Deliberately the *same* connection as the sweep rather than a ninth of
    /// its own: the mutex is what keeps a full rewrite and a sweep from
    /// overlapping, and this connection is the one already configured to keep
    /// its temp files on disk.
    pub fn maintenance(&self) -> Maintenance {
        Maintenance {
            storage: Arc::clone(&self.storage),
            window: self.window,
            data_dir: self.data_dir.clone(),
            spill: Arc::clone(&self.spill),
            persist_tx: self.persist_tx.clone(),
        }
    }

    /// Spawns the timer loop. Never returns; a failed sweep is logged and
    /// retried on the next tick rather than taking the process down — falling
    /// behind on housekeeping is not a monitoring fault.
    pub fn spawn(self) {
        tokio::spawn(async move {
            tokio::time::sleep(STARTUP_DELAY).await;
            loop {
                match self.sweep_once().await {
                    Ok(report) if report.total() > 0 => tracing::info!(
                        minute = report.minute_rows,
                        raw = report.raw_rows,
                        node = report.node_rows,
                        eye = report.eye_rows,
                        ledger = report.ledger_rows,
                        sessions = report.session_rows,
                        jobs = report.job_rows,
                        notifications = report.notification_rows,
                        "retention sweep pruned rows"
                    ),
                    Ok(_) => tracing::debug!("retention sweep: nothing to prune"),
                    Err(err) => tracing::warn!(%err, "retention sweep failed; retrying next tick"),
                }
                self.kick_a_stray_replay();
                self.maintenance_tick().await;
                tokio::time::sleep(self.sleep_until_next_tick()).await;
            }
        });
    }

    /// How long to wait before the next pass.
    ///
    /// With no window this is the plain sweep cadence, exactly as before. With
    /// one, the wait shortens so a tick actually lands inside the window rather
    /// than stepping over it: six hours of sleep would miss a 90-minute window
    /// most nights.
    fn sleep_until_next_tick(&self) -> Duration {
        let Some(window) = self.window else {
            return SWEEP_INTERVAL;
        };
        let minute_of_day = (now_unix().rem_euclid(86_400) / 60) as u32;
        if window.contains(minute_of_day) {
            // Inside the window: re-check often enough that a run refused for
            // space (or one that finishes early) gets another chance tonight.
            return SWEEP_INTERVAL.min(MAINTENANCE_RECHECK_INTERVAL);
        }
        let minutes_until_open =
            (window.start_minute_utc + MINUTES_PER_DAY - minute_of_day) % MINUTES_PER_DAY;
        SWEEP_INTERVAL.min(Duration::from_secs(
            u64::from(minutes_until_open.max(1)) * 60,
        ))
    }

    /// Evaluate the window and the trigger, and either claim a rewrite or
    /// record why not.
    ///
    /// Goes through the same [`Maintenance::start_full_vacuum`] the settings
    /// button uses, so scheduled and manual runs serialise on the one
    /// `begin_full_vacuum` claim for free: an operator clicking during a
    /// scheduled run gets the existing `already_running` refusal, and vice
    /// versa.
    async fn maintenance_tick(&self) {
        let Some(window) = self.window else {
            return;
        };
        let maintenance = self.maintenance();
        let status = match maintenance.status().await {
            Ok(status) => status,
            Err(err) => {
                tracing::warn!(%err, "could not read maintenance state; skipping this window tick");
                return;
            }
        };
        if status.state.full_vacuum_running {
            return;
        }

        let stats = match self.page_stats().await {
            Ok(stats) => stats,
            Err(err) => {
                tracing::warn!(%err, "could not measure database fragmentation");
                return;
            }
        };

        let decision = schedule_decision(
            now_unix(),
            window,
            stats,
            status.auto_vacuum_mode,
            status.state.last_full_vacuum_at,
        );

        if !decision.should_run() {
            // Only a decision made *inside* the window is worth an audit row;
            // "it is 14:00" is not an event.
            if decision != ScheduleDecision::OutsideWindow {
                tracing::debug!(reason = decision.slug(), "scheduled maintenance not needed");
            }
            return;
        }

        tracing::info!(
            reason = decision.slug(),
            freelist_pages = stats.freelist_count,
            freelist_bytes = stats.freelist_bytes(),
            trigger = "schedule",
            "starting scheduled database maintenance"
        );
        if let Err(refusal) = maintenance.start_full_vacuum(None).await {
            maintenance.audit_skipped(refusal.slug()).await;
        }
    }

    async fn page_stats(&self) -> Result<PageStats, StorageError> {
        let storage = Arc::clone(&self.storage);
        blocking(move || {
            let guard = storage.lock().unwrap_or_else(|p| p.into_inner());
            guard.page_stats()
        })
        .await
    }

    /// Ask the persist thread to drain a journal that nothing else will.
    ///
    /// A row can reach the journal outside a maintenance replay: a writer that
    /// raced the end of one (the phase check is deliberately not locked to the
    /// write — the gate is an optimisation, the fallback is the guarantee), or
    /// an ordinary contention error that outlasted the retry budget. Such a row
    /// is on disk the instant it is appended and cannot be lost, but only a
    /// drain makes it *visible*. Without this it would wait for the next
    /// maintenance run or the next restart, either of which is unbounded.
    ///
    /// Only when the gate is down: a raised gate means a replay already owns
    /// the journal.
    fn kick_a_stray_replay(&self) {
        if self.spill.phase() != portal_storage::SpillPhase::Direct {
            return;
        }
        match self.spill.depth() {
            Ok(0) => {}
            Ok(depth) => {
                tracing::info!(
                    depth,
                    "spill journal holds rows outside a maintenance run; replaying them"
                );
                if let Some(tx) = &self.persist_tx {
                    // `owns_gate: false` — this task raised nothing. The
                    // statement after this one is `maintenance_tick`, which may
                    // raise the gate before the persist thread gets to this
                    // message; a stray replay that lowered it would put every
                    // writer back on the locked database for the rest of the
                    // rewrite.
                    let _ = tx.send(PersistMsg::ReplaySpill { owns_gate: false });
                }
            }
            Err(err) => tracing::warn!(%err, "could not measure the spill journal"),
        }
    }

    async fn sweep_once(&self) -> Result<portal_storage::RetentionReport, StorageError> {
        let storage = Arc::clone(&self.storage);
        let policy = self.policy.clone();
        let now = now_unix();
        let joined = tokio::task::spawn_blocking(move || {
            let guard = storage.lock().unwrap_or_else(|poison| poison.into_inner());
            let report = guard.sweep_retention(&policy, now)?;

            // Deleting rows only moves pages to the freelist; handing them back
            // to `/data` is the point of the sweep on a CM4. A failure here is
            // not worth failing the sweep over — the rows are already gone and
            // the next pass reclaims them.
            match guard.incremental_vacuum(INCREMENTAL_VACUUM_PAGE_CAP) {
                Ok(0) => {}
                Ok(pages) => tracing::debug!(pages, "reclaimed pages after retention sweep"),
                Err(err) => tracing::warn!(%err, "incremental vacuum after retention sweep failed"),
            }
            Ok(report)
        })
        .await;

        match joined {
            Ok(result) => result,
            Err(join_err) => match join_err.try_into_panic() {
                Ok(payload) => std::panic::resume_unwind(payload),
                Err(_) => Err(StorageError::Io(std::io::Error::other(
                    "retention task was cancelled",
                ))),
            },
        }
    }
}

/// Database maintenance: the full `VACUUM`, and the one-time conversion of a
/// pre-V7 database to `auto_vacuum=INCREMENTAL`.
///
/// Reached two ways, through the same claim: an admin pressing the button on
/// `/settings`, and [`RetentionTask`]'s window tick. A full rewrite still locks
/// out every writer for its whole duration, and the blocked writer is still the
/// persist thread where the alarm engine runs — what changed is that a blocked
/// write is now *deferred to disk* rather than dropped, so the cost of a rewrite
/// is latency instead of lost readings, and a timer can be trusted with it. See
/// plan/portal/sqlite-vacuum.md §3 for the guarantee that buys this, and §4–§5
/// for when the timer fires.
#[derive(Clone)]
pub struct Maintenance {
    storage: Arc<Mutex<Storage>>,
    /// Shown read-only on the settings page. The window is deployment YAML, not
    /// a UI setting — see plan/portal/sqlite-vacuum.md §1.
    window: Option<MaintenanceWindow>,
    data_dir: String,
    spill: Arc<SpillGate>,
    persist_tx: Option<mpsc::UnboundedSender<PersistMsg>>,
}

/// Headroom left free after the rewrite, so maintenance can never be the reason
/// `/data` fills up. `/data` is grown to the whole eMMC on first boot, so this
/// is cheap insurance rather than a real constraint.
const FREE_SPACE_RESERVE: u64 = 512 * 1024 * 1024;

/// Free space a full `VACUUM` needs before it may start.
///
/// Twice the database, because VACUUM writes a complete temporary copy *and*, in
/// WAL mode, pushes every page through the WAL as well — plus
/// [`FREE_SPACE_RESERVE`] so the rewrite cannot be what fills the partition.
pub fn vacuum_space_required(db_size_bytes: u64) -> u64 {
    db_size_bytes
        .saturating_mul(2)
        .saturating_add(FREE_SPACE_RESERVE)
}

/// The preflight decision, as a pure function so both the button's enabled
/// state and the handler's refusal come from the *same* rule. They disagreed
/// once: the UI omitted the reserve and offered a click the preflight refused.
///
/// `available == None` means "could not measure", and that permits the run — a
/// platform we cannot measure should not lose maintenance entirely. The UI
/// follows the same reading.
pub fn space_permits_vacuum(
    db_size_bytes: u64,
    available: Option<u64>,
) -> Result<(), VacuumRefusal> {
    let required = vacuum_space_required(db_size_bytes);
    match available {
        Some(available) if available < required => Err(VacuumRefusal::InsufficientSpace {
            required,
            available,
        }),
        _ => Ok(()),
    }
}

/// Fraction of the file that must be dead space before a scheduled rewrite is
/// worth its wear on eMMC.
///
/// `incremental_vacuum` hands pages back to the freelist on every sweep but
/// never defragments and never shrinks the file below its high-water mark. A
/// fifth of the file being dead space is where repacking starts paying for
/// itself. Not YAML: nobody should tune a freelist fraction per deployment —
/// the window is the part that varies by site.
const FREELIST_FRACTION_TRIGGER: f64 = 0.20;

/// …and it must be this much dead space in absolute terms too. A fraction alone
/// flags tiny databases where the incremental reclaim is already sufficient.
const FREELIST_BYTES_TRIGGER: u64 = 64 * 1024 * 1024;

/// A successful rewrite this recent means the next window skips. The damper is
/// on *successes*, so a window missed because the device was off does not push
/// the schedule out.
const MIN_INTERVAL_BETWEEN_RUNS: Duration = Duration::from_secs(7 * 24 * 3600);

/// Why the scheduler did or did not start a run, as data rather than a
/// sentence — the audit row and the log line both render it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleDecision {
    /// Dead space has grown past both thresholds.
    Fragmented,
    /// A pre-V7 database still owes its one-time conversion to
    /// `auto_vacuum=INCREMENTAL`. Until it lands, the incremental reclaim is a
    /// silent no-op and `/data` grows without bound — and today that conversion
    /// waits for an administrator to notice a `tracing::error!` and click a
    /// button. The window is exactly the right place for it.
    ConversionOwed,
    /// Outside the configured window.
    OutsideWindow,
    /// Inside the window, but a successful rewrite is younger than the damper.
    RanRecently,
    /// Inside the window, nothing worth rewriting.
    NotFragmented,
}

impl ScheduleDecision {
    pub fn should_run(&self) -> bool {
        matches!(self, Self::Fragmented | Self::ConversionOwed)
    }

    /// Slug for the audit row's `details`, matching `VacuumRefusal::slug`'s
    /// shape — Rust never hands the audit trail a sentence.
    pub fn slug(&self) -> &'static str {
        match self {
            Self::Fragmented => "fragmented",
            Self::ConversionOwed => "conversion_owed",
            Self::OutsideWindow => "outside_window",
            Self::RanRecently => "ran_recently",
            Self::NotFragmented => "not_fragmented",
        }
    }
}

/// Should a scheduled rewrite start right now?
///
/// Pure arithmetic over `(now, window, stats, state)` so it tests without a
/// database, exactly like [`space_permits_vacuum`]. The window says when a
/// rewrite *may* run; this says whether it *should*. An unconditional nightly
/// rewrite of a healthy database is pointless wear.
pub fn schedule_decision(
    now_unix: i64,
    window: MaintenanceWindow,
    stats: PageStats,
    auto_vacuum_mode: i64,
    last_success_unix: Option<i64>,
) -> ScheduleDecision {
    let minute_of_day = now_unix.rem_euclid(86_400) as u32 / 60;
    if !window.contains(minute_of_day) {
        return ScheduleDecision::OutsideWindow;
    }

    // Checked before the damper: a database that never got its conversion has
    // been reclaiming nothing at all, and waiting another week is the outcome
    // this whole feature exists to end.
    if auto_vacuum_mode != 2 {
        return ScheduleDecision::ConversionOwed;
    }

    if let Some(last) = last_success_unix {
        let age = now_unix.saturating_sub(last);
        if age >= 0 && (age as u64) < MIN_INTERVAL_BETWEEN_RUNS.as_secs() {
            return ScheduleDecision::RanRecently;
        }
    }

    if stats.freelist_fraction() > FREELIST_FRACTION_TRIGGER
        && stats.freelist_bytes() > FREELIST_BYTES_TRIGGER
    {
        ScheduleDecision::Fragmented
    } else {
        ScheduleDecision::NotFragmented
    }
}

/// Why a requested full VACUUM was refused. Each variant is a slug the
/// templates translate — Rust never hands the UI a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VacuumRefusal {
    /// One is already running.
    AlreadyRunning,
    /// Not enough free space for the temporary copy plus headroom.
    InsufficientSpace { required: u64, available: u64 },
    /// The database could not be inspected.
    Unavailable,
}

impl VacuumRefusal {
    /// Slug for the `?err=` flash parameter, matching the i18n catalogue.
    pub fn slug(&self) -> &'static str {
        match self {
            Self::AlreadyRunning => "already_running",
            Self::InsufficientSpace { .. } => "insufficient_space",
            Self::Unavailable => "unavailable",
        }
    }
}

impl Maintenance {
    /// Current bookkeeping plus the two figures the settings page shows
    /// alongside it.
    pub async fn status(&self) -> Result<MaintenanceStatus, StorageError> {
        let storage = Arc::clone(&self.storage);
        let data_dir = self.data_dir.clone();
        let window = self.window;
        let spill = Arc::clone(&self.spill);
        blocking(move || {
            let guard = storage.lock().unwrap_or_else(|p| p.into_inner());
            Ok(MaintenanceStatus {
                state: guard.maintenance_state()?,
                db_size_bytes: guard.db_size_bytes().unwrap_or(0),
                auto_vacuum_mode: guard.auto_vacuum_mode().unwrap_or(-1),
                available_bytes: crate::diskspace::available_bytes(std::path::Path::new(&data_dir)),
                window,
                spill_depth: spill.depth().unwrap_or(0),
            })
        })
        .await
    }

    /// Preflight and start a full `VACUUM` in the background.
    ///
    /// Returns once the work is *claimed*, not once it is finished — a rewrite
    /// outlasts any sane request timeout, so the settings page polls
    /// [`Self::status`] instead. `Err` is a refusal the caller renders; the
    /// outcome of an accepted run lands in `maintenance_state`.
    pub async fn start_full_vacuum(&self, actor: Option<AuditActor>) -> Result<(), VacuumRefusal> {
        let storage = Arc::clone(&self.storage);
        let data_dir = self.data_dir.clone();

        // Preflight and claim under one lock acquisition, so two requests
        // cannot both pass the check.
        let claim = {
            let storage = Arc::clone(&storage);
            let data_dir = data_dir.clone();
            let spill = Arc::clone(&self.spill);
            blocking(move || {
                let guard = storage.lock().unwrap_or_else(|p| p.into_inner());
                let db_size = guard.db_size_bytes()? as u64;
                let available = crate::diskspace::available_bytes(std::path::Path::new(&data_dir));
                if let Err(refusal) = space_permits_vacuum(db_size, available) {
                    return Ok(Err(refusal));
                }
                if !guard.begin_full_vacuum()? {
                    return Ok(Err(VacuumRefusal::AlreadyRunning));
                }
                // Raised under the same lock acquisition as the claim, so the
                // reroute and the rewrite cannot disagree about whether a run
                // is in progress. A writer that races this check still spills:
                // its write fails on the lock and `durable::write_or_spill`
                // falls back. The gate only saves it the doomed attempt.
                //
                // The lease, not a bare `begin_spilling`, is what guarantees
                // the gate comes back down even if the rewrite below panics.
                Ok(Ok(SpillGate::raise(&spill)))
            })
            .await
        };

        let lease = match claim {
            Ok(Ok(lease)) => lease,
            Ok(Err(refusal)) => {
                if let VacuumRefusal::InsufficientSpace {
                    required,
                    available,
                } = &refusal
                {
                    tracing::error!(
                        required_bytes = required,
                        available_bytes = available,
                        "refusing a full VACUUM: not enough free space for the temporary copy; \
                         pages will not be repacked until space is freed"
                    );
                }
                return Err(refusal);
            }
            Err(err) => {
                tracing::error!(%err, "could not preflight a full VACUUM");
                return Err(VacuumRefusal::Unavailable);
            }
        };

        self.audit(
            actor.as_ref(),
            "database_maintenance_started",
            true,
            Some(format!(
                r#"{{"trigger":"{}"}}"#,
                if actor.is_some() {
                    "manual"
                } else {
                    "schedule"
                }
            )),
        )
        .await;

        // Detached on purpose: the handler has already returned by the time
        // this finishes. `lease` moves in with it, so however this task ends —
        // returning, panicking, or never running because the runtime shut down
        // — the gate is resolved.
        let persist_tx = self.persist_tx.clone();
        let spill = Arc::clone(&self.spill);
        let trigger = if actor.is_some() {
            "manual"
        } else {
            "schedule"
        };
        tokio::task::spawn_blocking(move || {
            let guard = storage.lock().unwrap_or_else(|p| p.into_inner());
            let started = std::time::Instant::now();
            let size_before = guard.db_size_bytes().unwrap_or(0);

            // Converting a pre-V7 database is itself a full rewrite, so it
            // replaces rather than follows the plain VACUUM.
            let needs_conversion = !matches!(guard.auto_vacuum_mode(), Ok(2));
            let outcome = if needs_conversion {
                tracing::info!("converting the database to auto_vacuum=INCREMENTAL");
                guard.convert_to_incremental_auto_vacuum()
            } else {
                guard.full_vacuum()
            };

            let elapsed_ms = started.elapsed().as_millis() as i64;
            let error = outcome.as_ref().err().map(|e| e.to_string());
            match &outcome {
                Ok(()) => tracing::info!(
                    elapsed_ms,
                    converted = needs_conversion,
                    trigger,
                    "full VACUUM finished"
                ),
                Err(err) => tracing::error!(%err, elapsed_ms, trigger, "full VACUUM failed"),
            }
            if let Err(err) = guard.finish_full_vacuum(
                now_unix(),
                elapsed_ms,
                error.as_deref(),
                needs_conversion && outcome.is_ok(),
            ) {
                // The flag stays set; startup reconciliation clears it.
                tracing::error!(%err, "could not record the full-VACUUM outcome");
            }

            // The sibling of `database_maintenance_started`. Without it the
            // audit trail shows a rewrite beginning and nothing else, and an
            // auditor cannot tell a finished run from a killed one.
            let reclaimed_bytes = size_before.saturating_sub(guard.db_size_bytes().unwrap_or(0));
            let details = format!(
                r#"{{"elapsed_ms":{elapsed_ms},"reclaimed_bytes":{reclaimed_bytes},"converted":{needs_conversion},"trigger":"{trigger}"}}"#
            );
            write_maintenance_audit(
                &spill,
                &guard,
                actor.as_ref(),
                "database_maintenance_finished",
                outcome.is_ok(),
                Some(details),
            );

            // The lock is free again. Drop it before handing the backlog to the
            // persist thread, which needs the database to replay into.
            drop(guard);

            // The persist thread owns replay: it holds the ingest cache and the
            // notify/nudge senders, and being a single FIFO consumer is what
            // makes "replay before any newer work" structural rather than
            // something this task has to coordinate.
            match &persist_tx {
                Some(tx) if tx.send(PersistMsg::ReplaySpill { owns_gate: true }).is_ok() => {
                    lease.handed_to_replay()
                }
                // No persist thread (tests, or it died): nothing will drain the
                // journal, so the gate must come back down or every subsequent
                // write spills with no drain to end it. Dropping the lease does
                // that and says so. Whatever is queued stays queued and durable
                // for the next startup, which replays it.
                _ => drop(lease),
            }
        });

        Ok(())
    }

    /// Bare instance for handler tests, mirroring `AlarmService`/`CommandRelay`.
    #[cfg(test)]
    pub fn test_instance(data_dir: &str) -> Self {
        let spill = SpillGate::open(std::path::Path::new(data_dir), None)
            .expect("open spill journal for maintenance test instance");
        RetentionTask::open(
            data_dir,
            None,
            RetentionPolicy::default(),
            Arc::new(spill),
            None,
        )
        .expect("open retention for maintenance test instance")
        .maintenance()
    }

    /// A `database_maintenance_skipped` row naming the gate that refused.
    pub async fn audit_skipped(&self, reason: &str) {
        self.audit(
            None,
            "database_maintenance_skipped",
            false,
            Some(format!(r#"{{"reason":"{reason}"}}"#)),
        )
        .await;
    }

    async fn audit(
        &self,
        actor: Option<&AuditActor>,
        event_type: &str,
        success: bool,
        details: Option<String>,
    ) {
        let storage = Arc::clone(&self.storage);
        // A scheduled run has no actor, and the row says so by leaving both
        // fields `None` — the schema already allows it, and `/audit` renders an
        // absent actor as "scheduled". Writing no row at all (what a `None`
        // actor used to mean) would make the schedule invisible in the one
        // place an auditor looks.
        let entry = portal_storage::NewAuditEntry {
            event_type: event_type.to_string(),
            username: actor.map(|a| a.username.clone()),
            user_id: actor.map(|a| a.user_id),
            ip: actor.and_then(|a| a.ip.clone()),
            target: Some("portal.db".to_string()),
            details,
            success,
        };
        // Through the spill: this row is written *while* a rewrite is being
        // claimed, so it is the single most likely audit entry in the system to
        // meet a locked database.
        let spill = Arc::clone(&self.spill);
        let written = blocking(move || {
            let guard = storage.lock().unwrap_or_else(|p| p.into_inner());
            crate::durable::insert_audit(&spill, &guard, &entry)
        })
        .await;
        if let Err(err) = written {
            tracing::warn!(%err, "failed to write audit entry for database maintenance");
        }
    }
}

/// Who started the rewrite that did not finish, from the most recent
/// `database_maintenance_started` row.
///
/// `None` for a scheduled run (which writes no actor by design) and for a
/// database with no `_started` row at all — both render as "scheduled" on
/// `/audit`, which is the honest reading in either case.
fn interrupted_run_actor(storage: &Storage) -> Option<AuditActor> {
    let (rows, _) = storage
        .list_audit(&portal_storage::AuditFilter {
            event_type: Some("database_maintenance_started".to_string()),
            limit: 1,
            ..Default::default()
        })
        .map_err(|err| {
            tracing::warn!(%err, "could not read the interrupted run's actor; recording it unattributed");
        })
        .ok()?;
    let row = rows.into_iter().next()?;
    Some(AuditActor {
        user_id: row.user_id?,
        username: row.username?,
        ip: row.ip,
    })
}

/// Write a maintenance audit row from a blocking context.
///
/// The async [`Maintenance::audit`] cannot be used from inside the detached
/// rewrite — it is already holding the storage mutex on a blocking thread — so
/// the row shape lives here and both paths call it. Through the spill, because
/// these rows are written while a rewrite holds the write lock and are the most
/// likely in the system to meet a locked database.
fn write_maintenance_audit(
    spill: &SpillGate,
    storage: &Storage,
    actor: Option<&AuditActor>,
    event_type: &str,
    success: bool,
    details: Option<String>,
) {
    let entry = portal_storage::NewAuditEntry {
        event_type: event_type.to_string(),
        username: actor.map(|a| a.username.clone()),
        user_id: actor.map(|a| a.user_id),
        ip: actor.and_then(|a| a.ip.clone()),
        target: Some("portal.db".to_string()),
        details,
        success,
    };
    if let Err(err) = crate::durable::insert_audit(spill, storage, &entry) {
        tracing::warn!(%err, event_type, "failed to write audit entry for database maintenance");
    }
}

/// Who asked for maintenance, for the audit row.
#[derive(Debug, Clone)]
pub struct AuditActor {
    pub user_id: i64,
    pub username: String,
    pub ip: Option<String>,
}

/// What the settings page renders.
#[derive(Debug, Clone)]
pub struct MaintenanceStatus {
    pub state: portal_storage::MaintenanceState,
    pub db_size_bytes: i64,
    /// `2` is incremental; anything else means a one-time conversion is owed.
    pub auto_vacuum_mode: i64,
    /// `None` when free space could not be determined.
    pub available_bytes: Option<u64>,
    /// The configured window, or `None` for no scheduled maintenance. Read-only
    /// on the page — it is deployment YAML, not a setting.
    pub window: Option<MaintenanceWindow>,
    /// Writes waiting in the spill journal. Non-zero outside a run means a
    /// replay is owed; a stuck run reads as "N events waiting" rather than
    /// silence (plan/portal/sqlite-vacuum.md §3.1.3).
    pub spill_depth: i64,
}

/// Run a blocking storage closure, turning a panic into a propagated panic and
/// a cancellation into an error — same contract as `sweep_once`.
async fn blocking<T, F>(f: F) -> Result<T, StorageError>
where
    F: FnOnce() -> Result<T, StorageError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(join_err) => match join_err.try_into_panic() {
            Ok(payload) => std::panic::resume_unwind(payload),
            Err(_) => Err(StorageError::Io(std::io::Error::other(
                "maintenance task was cancelled",
            ))),
        },
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use portal_storage::{MinuteReading, SOURCE_LIVE};

    fn minute_row(hostname: &str, ts: i64) -> MinuteReading {
        MinuteReading {
            hostname: hostname.to_string(),
            sensor_line: 0,
            minute_ts: ts,
            min_c: Some(1.0),
            avg_c: Some(2.0),
            max_c: Some(3.0),
            sample_count: Some(60),
            disconnect_count: Some(0),
            worst_alarm: None,
            source: SOURCE_LIVE.to_string(),
            data_hmac: None,
        }
    }

    fn window(start: &str, minutes: u32) -> MaintenanceWindow {
        MaintenanceWindow {
            start_minute_utc: crate::config::parse_hh_mm_for_tests(start).expect("HH:MM"),
            duration_minutes: minutes,
        }
    }

    /// `now` at `hh:mm` UTC on an arbitrary day, for the arithmetic below.
    fn at(hh_mm: &str) -> i64 {
        let day = 20_000 * 86_400;
        day + i64::from(crate::config::parse_hh_mm_for_tests(hh_mm).expect("HH:MM")) * 60
    }

    fn stats(page_count: i64, freelist_count: i64) -> PageStats {
        PageStats {
            page_count,
            freelist_count,
            page_size: 4096,
        }
    }

    /// 1 GiB with 30 % of it dead — the case a full rewrite exists for, and the
    /// one the incremental reclaim cannot fix because it never shrinks the file
    /// below its high-water mark.
    fn fragmented() -> PageStats {
        stats(262_144, 78_643)
    }

    #[test]
    fn a_window_only_opens_at_its_own_hour() {
        let w = window("02:30", 90);
        assert!(!w.contains(149), "02:29");
        assert!(w.contains(150), "02:30");
        assert!(w.contains(239), "03:59");
        assert!(!w.contains(240), "04:00 — the window has closed");
    }

    #[test]
    fn a_window_may_straddle_midnight() {
        let w = window("23:30", 90);
        assert!(w.contains(23 * 60 + 30));
        assert!(
            w.contains(0),
            "00:00 is inside a window that opened at 23:30"
        );
        assert!(w.contains(59), "00:59");
        assert!(!w.contains(60), "01:00 — closed");
        assert!(!w.contains(12 * 60));
    }

    #[test]
    fn a_healthy_database_is_not_rewritten_just_because_the_window_opened() {
        // 1 GiB, 5 % dead: over neither threshold.
        assert_eq!(
            schedule_decision(
                at("02:45"),
                window("02:30", 90),
                stats(262_144, 13_107),
                2,
                None
            ),
            ScheduleDecision::NotFragmented,
            "an unconditional nightly rewrite is pointless wear on eMMC"
        );
    }

    #[test]
    fn a_small_database_is_not_rewritten_however_fragmented_it_looks() {
        // 8 MiB with half of it on the freelist: 50 % but only 4 MiB, which the
        // incremental reclaim already handles. The absolute floor is what stops
        // the fraction alone from flagging every tiny database.
        assert_eq!(
            schedule_decision(
                at("02:45"),
                window("02:30", 90),
                stats(2_048, 1_024),
                2,
                None
            ),
            ScheduleDecision::NotFragmented
        );
    }

    #[test]
    fn a_fragmented_database_is_rewritten_inside_the_window_and_only_there() {
        let w = window("02:30", 90);
        assert_eq!(
            schedule_decision(at("02:45"), w, fragmented(), 2, None),
            ScheduleDecision::Fragmented
        );
        assert_eq!(
            schedule_decision(at("14:00"), w, fragmented(), 2, None),
            ScheduleDecision::OutsideWindow,
            "the window is what makes this safe on a 24/7 device"
        );
    }

    #[test]
    fn a_rewrite_last_week_dampens_the_next_one() {
        let now = at("02:45");
        let w = window("02:30", 90);
        assert_eq!(
            schedule_decision(now, w, fragmented(), 2, Some(now - 6 * 86_400)),
            ScheduleDecision::RanRecently
        );
        assert_eq!(
            schedule_decision(now, w, fragmented(), 2, Some(now - 8 * 86_400)),
            ScheduleDecision::Fragmented
        );
    }

    /// The damper is on *successes*, so a window missed because the device was
    /// off does not push the schedule out — there is nothing here that records
    /// attempts.
    #[test]
    fn a_pre_v7_database_is_converted_even_though_it_looks_unfragmented() {
        assert_eq!(
            schedule_decision(at("02:45"), window("02:30", 90), stats(262_144, 0), 0, None),
            ScheduleDecision::ConversionOwed,
            "until it converts, the incremental reclaim is a silent no-op"
        );
    }

    /// The conversion is owed regardless of the damper: a database that has been
    /// reclaiming nothing at all must not wait another week for it.
    #[test]
    fn the_conversion_outranks_the_damper() {
        let now = at("02:45");
        assert_eq!(
            schedule_decision(
                now,
                window("02:30", 90),
                stats(262_144, 0),
                0,
                Some(now - 3_600)
            ),
            ScheduleDecision::ConversionOwed
        );
    }

    /// The opt-in default, and the safety property every existing deployment
    /// depends on: no `maintenance_window` in the config means no rewrite ever
    /// starts on its own. The recorded 2026-08-24 stance survives for anyone who
    /// does not ask for the schedule.
    #[tokio::test]
    async fn a_deployment_with_no_window_never_schedules_anything() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8 path");
        let task = RetentionTask::open(
            data_dir,
            None,
            RetentionPolicy::default(),
            crate::durable::test_gate(data_dir),
            None,
        )
        .expect("open retention")
        .with_maintenance_window(None);

        task.maintenance_tick().await;

        let state = task
            .maintenance()
            .status()
            .await
            .expect("read maintenance state");
        assert!(
            !state.state.full_vacuum_running && state.state.last_full_vacuum_at.is_none(),
            "an unconfigured deployment must never rewrite its database unasked"
        );
        // And the sweep cadence is untouched, so nothing else changed either.
        assert_eq!(task.sleep_until_next_tick(), SWEEP_INTERVAL);
    }

    /// With a window configured, the loop stops sleeping past it. Six hours of
    /// sleep would step over a 90-minute window most nights.
    #[test]
    fn a_configured_window_shortens_the_sleep_enough_to_land_inside_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8 path");
        let task = RetentionTask::open(
            data_dir,
            None,
            RetentionPolicy::default(),
            crate::durable::test_gate(data_dir),
            None,
        )
        .expect("open retention")
        // Open right now, whenever "now" is, so this does not depend on the
        // wall clock at test time.
        .with_maintenance_window(Some(MaintenanceWindow {
            start_minute_utc: 0,
            duration_minutes: MINUTES_PER_DAY,
        }));

        assert_eq!(
            task.sleep_until_next_tick(),
            MAINTENANCE_RECHECK_INTERVAL,
            "inside the window, re-check often enough to get another chance tonight"
        );
    }

    /// An interrupted rewrite is attributed to whoever started it.
    ///
    /// The regression: the `_interrupted` row was written with a hard-coded
    /// `None` actor, and `/audit` renders an actor-less maintenance row as
    /// "scheduled" — so a crashed *manual* rewrite claimed the timer had
    /// started it, contradicting the `_started` row still sitting above it in
    /// the same table.
    #[tokio::test]
    async fn an_interrupted_rewrite_names_whoever_started_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8 path").to_string();

        // A manual run that claimed the flag and then died.
        {
            let storage = Storage::open(&std::path::Path::new(&data_dir).join("portal.db"), None)
                .expect("open");
            storage
                .insert_audit(&portal_storage::NewAuditEntry {
                    event_type: "database_maintenance_started".to_string(),
                    username: Some("admin".to_string()),
                    user_id: Some(7),
                    ip: Some("10.0.0.2".to_string()),
                    target: Some("portal.db".to_string()),
                    details: Some(r#"{"trigger":"manual"}"#.to_string()),
                    success: true,
                })
                .expect("started row");
            assert!(storage.begin_full_vacuum().expect("claim"));
        }

        // ... and the restart that reconciles it.
        RetentionTask::open(
            &data_dir,
            None,
            RetentionPolicy::default(),
            crate::durable::test_gate(&data_dir),
            None,
        )
        .expect("open retention");

        let storage = Storage::open(&std::path::Path::new(&data_dir).join("portal.db"), None)
            .expect("reopen");
        let (rows, _) = storage
            .list_audit(&portal_storage::AuditFilter {
                event_type: Some("database_maintenance_interrupted".to_string()),
                limit: 10,
                ..Default::default()
            })
            .expect("list audit");

        let row = rows
            .first()
            .expect("the restart must record the interruption");
        assert_eq!(
            row.username.as_deref(),
            Some("admin"),
            "an actor-less row renders as \"scheduled\", which this run was not"
        );
        assert_eq!(row.user_id, Some(7));
        assert_eq!(row.ip.as_deref(), Some("10.0.0.2"));
        assert!(
            row.details
                .as_deref()
                .is_some_and(|d| d.contains(r#""trigger":"manual""#)),
            "and the row says so on its own: {:?}",
            row.details
        );
    }

    /// A scheduled run really does have no actor, and must keep reading as
    /// "scheduled" after a crash.
    #[tokio::test]
    async fn an_interrupted_scheduled_rewrite_stays_unattributed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8 path").to_string();

        {
            let storage = Storage::open(&std::path::Path::new(&data_dir).join("portal.db"), None)
                .expect("open");
            storage
                .insert_audit(&portal_storage::NewAuditEntry {
                    event_type: "database_maintenance_started".to_string(),
                    username: None,
                    user_id: None,
                    ip: None,
                    target: Some("portal.db".to_string()),
                    details: Some(r#"{"trigger":"schedule"}"#.to_string()),
                    success: true,
                })
                .expect("started row");
            assert!(storage.begin_full_vacuum().expect("claim"));
        }

        RetentionTask::open(
            &data_dir,
            None,
            RetentionPolicy::default(),
            crate::durable::test_gate(&data_dir),
            None,
        )
        .expect("open retention");

        let storage = Storage::open(&std::path::Path::new(&data_dir).join("portal.db"), None)
            .expect("reopen");
        let (rows, _) = storage
            .list_audit(&portal_storage::AuditFilter {
                event_type: Some("database_maintenance_interrupted".to_string()),
                limit: 10,
                ..Default::default()
            })
            .expect("list audit");
        let row = rows.first().expect("interrupted row");
        assert_eq!(row.username, None);
        assert!(row
            .details
            .as_deref()
            .is_some_and(|d| d.contains(r#""trigger":"schedule""#)));
    }

    /// The gate must come back down even when nothing will replay the journal.
    ///
    /// `Maintenance::test_instance` has no persist thread, which is the same
    /// shape as the real failure: the detached rewrite finishes (or panics) with
    /// nobody to hand the backlog to. Before the lease, that left the gate
    /// raised and every subsequent write in the process spilled forever.
    #[tokio::test]
    async fn a_vacuum_with_nobody_to_replay_lowers_the_gate_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_str().expect("utf8 path");
        let spill = crate::durable::test_gate(data_dir);
        let maintenance = RetentionTask::open(
            data_dir,
            None,
            RetentionPolicy::default(),
            Arc::clone(&spill),
            None,
        )
        .expect("open retention")
        .maintenance();

        maintenance
            .start_full_vacuum(None)
            .await
            .expect("the rewrite is claimed");

        // The rewrite is detached, so poll rather than assume it has finished.
        for _ in 0..200 {
            if spill.phase() == portal_storage::SpillPhase::Direct {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "the gate is still {:?}; every write after this one would spill with no drain to end it",
            spill.phase()
        );
    }

    #[tokio::test]
    async fn sweep_prunes_past_the_window_and_keeps_the_rest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let task = RetentionTask::open(
            dir.path().to_str().expect("utf8 path"),
            None,
            RetentionPolicy {
                minute_days: 30,
                ..RetentionPolicy::default()
            },
            crate::durable::test_gate(dir.path().to_str().expect("utf8 path")),
            None,
        )
        .expect("open");

        let now = now_unix();
        {
            let guard = task.storage.lock().expect("lock");
            guard
                .upsert_minute(&minute_row("hub", now - 40 * 86_400))
                .expect("old row");
            guard
                .upsert_minute(&minute_row("hub", now - 60))
                .expect("fresh row");
        }

        let report = task.sweep_once().await.expect("sweep");
        assert_eq!(
            report.minute_rows, 1,
            "only the out-of-window row is pruned"
        );

        let guard = task.storage.lock().expect("lock");
        let kept = guard
            .query_minutes("hub", 0, now - 86_400, now + 60)
            .expect("query");
        assert_eq!(kept.len(), 1, "the in-window row survives");
    }

    #[tokio::test]
    async fn sweep_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let task = RetentionTask::open(
            dir.path().to_str().expect("utf8 path"),
            None,
            RetentionPolicy::default(),
            crate::durable::test_gate(dir.path().to_str().expect("utf8 path")),
            None,
        )
        .expect("open");
        let now = now_unix();
        {
            let guard = task.storage.lock().expect("lock");
            guard
                .upsert_minute(&minute_row("hub", now - 40 * 86_400))
                .expect("old row");
        }
        assert_eq!(task.sweep_once().await.expect("first").minute_rows, 1);
        assert_eq!(
            task.sweep_once().await.expect("second").minute_rows,
            0,
            "a second pass finds nothing left to do"
        );
    }

    /// The preflight is arithmetic on two numbers, so it is tested as such —
    /// filling a real disk to exercise a refusal would be absurd, and stubbing
    /// statvfs would test the stub.
    #[test]
    fn space_preflight_needs_double_the_database_plus_the_reserve() {
        const MB: u64 = 1024 * 1024;
        let db = 100 * MB;
        assert_eq!(vacuum_space_required(db), 200 * MB + FREE_SPACE_RESERVE);

        // Exactly enough passes; one byte short does not.
        let required = vacuum_space_required(db);
        assert!(space_permits_vacuum(db, Some(required)).is_ok());
        assert_eq!(
            space_permits_vacuum(db, Some(required - 1)),
            Err(VacuumRefusal::InsufficientSpace {
                required,
                available: required - 1
            })
        );

        // Enough for the copy but not the headroom is still a refusal: the
        // reserve is the point, not a rounding allowance.
        assert!(space_permits_vacuum(db, Some(200 * MB)).is_err());
    }

    /// Unmeasurable free space must not disable maintenance — and the UI has to
    /// read it the same way, which is why both call this one function.
    #[test]
    fn unknown_free_space_permits_the_run() {
        assert!(space_permits_vacuum(500 * 1024 * 1024, None).is_ok());
    }

    /// A saturating overflow must not wrap into a tiny requirement and wave a
    /// hopeless rewrite through.
    #[test]
    fn an_absurd_database_size_does_not_wrap() {
        assert_eq!(vacuum_space_required(u64::MAX), u64::MAX);
        assert!(space_permits_vacuum(u64::MAX, Some(u64::MAX - 1)).is_err());
    }

    #[tokio::test]
    async fn maintenance_status_reports_a_fresh_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let task = RetentionTask::open(
            dir.path().to_str().expect("utf8 path"),
            None,
            RetentionPolicy::default(),
            crate::durable::test_gate(dir.path().to_str().expect("utf8 path")),
            None,
        )
        .expect("open");

        let status = task.maintenance().status().await.expect("status");
        assert!(!status.state.full_vacuum_running);
        assert_eq!(status.state.last_full_vacuum_at, None);
        assert_eq!(
            status.auto_vacuum_mode, 2,
            "a fresh database must already be incremental, or the reclaim is a no-op"
        );
        assert!(status.db_size_bytes > 0);
    }

    /// The sweep must hand pages back, not just delete rows — that is the whole
    /// reason the automatic full VACUUM could be removed.
    #[tokio::test]
    async fn a_sweep_reclaims_pages_incrementally() {
        let dir = tempfile::tempdir().expect("tempdir");
        let task = RetentionTask::open(
            dir.path().to_str().expect("utf8 path"),
            None,
            RetentionPolicy::default(),
            crate::durable::test_gate(dir.path().to_str().expect("utf8 path")),
            None,
        )
        .expect("open");

        let now = now_unix();
        {
            let guard = task.storage.lock().expect("lock");
            for i in 0..4_000 {
                guard
                    .upsert_minute(&minute_row("hub", now - 90 * 86_400 + i))
                    .expect("old row");
            }
        }
        let before = {
            let guard = task.storage.lock().expect("lock");
            guard.db_size_bytes().expect("size")
        };

        task.sweep_once().await.expect("sweep");

        let after = {
            let guard = task.storage.lock().expect("lock");
            guard.db_size_bytes().expect("size")
        };
        assert!(
            after < before,
            "sweep pruned rows but returned no space: {before} -> {after}"
        );
    }
}
