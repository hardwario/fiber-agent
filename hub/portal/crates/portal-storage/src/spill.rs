//! The spill journal: a durable side-channel for writes the main database
//! cannot accept right now.
//!
//! A full `VACUUM` holds `portal.db`'s write lock for its entire duration, and
//! this process has eight connections on that file. Before this module, a
//! writer that lost the race was dropped by its caller — `persist.rs` logged
//! "persistence write failed" and moved on — so routine maintenance discarded
//! alarm rows and their notifications (measured 2026-08-24: 1401 of 1542 write
//! attempts refused during one run).
//!
//! The fix is not a bigger RAM queue. This is a medical device, and a power cut
//! during the one window when writes are deliberately deferred is exactly the
//! case the guarantee exists for. So deferred writes go to a **second SQLite
//! database next to the main one** — a different file with its own lock, which
//! a `VACUUM` of `portal.db` cannot touch. A commit here *is* the acceptance
//! point; the row is on disk before anything downstream observes it as
//! accepted.
//!
//! A side SQLite file rather than a hand-rolled append log because atomic,
//! torn-write-safe, fsync-correct appends are precisely the problem SQLite has
//! already solved — and the SQLCipher layer comes with it, which matters
//! because the payloads are patient telemetry.
//!
//! ## Ordering, and why replay is safe to repeat
//!
//! [`SpillJournal::drain`] hands rows to its callback in `id` order and deletes
//! each one only *after* the callback reports it committed. A crash in between
//! therefore replays that row on the next startup. Every row type this journal
//! carries is idempotent under re-application (upsert/dedup shapes on the
//! ingest side, and the audit/report-history inserts are append-only records of
//! events that did happen), so a twice-applied row is harmless and a
//! never-applied one is impossible.
//!
//! ## The gate is an optimisation; the fallback is the guarantee
//!
//! [`SpillGate`] lets writers skip a doomed attempt at the locked database
//! while a rewrite is known to be running. It is deliberately *not* the thing
//! that makes this correct: a writer that misses the phase transition simply
//! blocks on `busy_timeout`, fails, and spills anyway. See
//! [`SpillGate::write_or_spill`].

use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;

use crate::error::{Result, StorageError};
use crate::retry::with_retry;

/// Which writer produced a spilled row, and therefore how to replay it.
///
/// Stored as the `kind` text column rather than an integer so a spilled
/// database is readable with `sqlite3` during an incident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpillKind {
    /// A raw MQTT publish: the ingest path replays it through its own decoder
    /// rather than storing a parsed event, so there is exactly one decode
    /// implementation and no derives on the wire types.
    Mqtt,
    /// A JSON-encoded `NewAuditEntry`.
    Audit,
    /// A JSON-encoded `NewReportHistory`.
    ReportHistory,
    /// A JSON-encoded liveness edge — one device going silent or coming back.
    ///
    /// The *edge* rather than the row it produces, because applying it is a
    /// read-modify-write against the currently open alarm (`portal/src/durable.rs`).
    /// That is what makes a replay idempotent, and it is the reason this kind
    /// exists at all: the edge arrives on a broadcast channel with no
    /// re-announcement, so a refused write is the one place in the portal
    /// where a device could stay marked offline forever.
    Liveness,
}

impl SpillKind {
    /// The `kind` column value.
    pub fn as_str(self) -> &'static str {
        match self {
            SpillKind::Mqtt => "mqtt",
            SpillKind::Audit => "audit",
            SpillKind::ReportHistory => "report_history",
            SpillKind::Liveness => "liveness",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "mqtt" => Some(SpillKind::Mqtt),
            "audit" => Some(SpillKind::Audit),
            "report_history" => Some(SpillKind::ReportHistory),
            "liveness" => Some(SpillKind::Liveness),
            _ => None,
        }
    }
}

/// A row as stored, before its `kind` is recognised.
struct RawSpilledRow {
    id: i64,
    ts: i64,
    kind: String,
    payload: Vec<u8>,
}

impl RawSpilledRow {
    /// Refused, not skipped: a row whose `kind` this build does not know (a
    /// downgrade after a newer one wrote it) stays on disk for an operator to
    /// look at. Dropping it is the one outcome this module exists to prevent.
    fn decode(&self) -> Result<SpilledRow> {
        let kind = SpillKind::from_str(&self.kind).ok_or_else(|| {
            StorageError::Conflict(format!(
                "spill row {} has unknown kind {:?}; refusing to drop it",
                self.id, self.kind
            ))
        })?;
        Ok(SpilledRow {
            id: self.id,
            ts: self.ts,
            kind,
            payload: self.payload.clone(),
        })
    }
}

/// One row awaiting replay.
#[derive(Debug, Clone)]
pub struct SpilledRow {
    pub id: i64,
    pub ts: i64,
    pub kind: SpillKind,
    pub payload: Vec<u8>,
}

/// The side database itself.
///
/// Owns its own `rusqlite::Connection` (`!Sync`), so it lives behind the
/// [`SpillGate`]'s mutex rather than being cloned per writer. Contention is a
/// non-issue: outside a maintenance run nothing writes here at all.
pub struct SpillJournal {
    conn: Connection,
}

impl SpillJournal {
    /// Open (creating if needed) the journal at `path`.
    ///
    /// `key` follows [`crate::Storage::open`]'s contract — `Some` for
    /// SQLCipher, `None` for dev-mode plain SQLite. The schema is created
    /// inline rather than by a refinery migration because this is a *different
    /// database file*; the migration runner is bound to `portal.db`.
    ///
    /// `synchronous = FULL`, unlike the main database's WAL default: a commit
    /// here is the point at which a write counts as accepted, so it has to
    /// survive a power cut on its own rather than within a checkpoint window.
    pub fn open(path: &Path, key: Option<&str>) -> Result<Self> {
        let conn = Connection::open(path)?;

        if let Some(key) = key {
            conn.pragma_update(None, "key", key)?;
        }

        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.busy_timeout(Duration::from_secs(30))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS spill (
                 id      INTEGER PRIMARY KEY AUTOINCREMENT,
                 ts      INTEGER NOT NULL,
                 kind    TEXT    NOT NULL,
                 payload BLOB    NOT NULL
             );",
        )?;

        Ok(Self { conn })
    }

    /// Append one deferred write. Returns its `id`.
    ///
    /// The `INSERT` committing is the acceptance point — callers may report
    /// success to their producer once this returns `Ok`.
    pub fn append(&self, kind: SpillKind, ts: i64, payload: &[u8]) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO spill (ts, kind, payload) VALUES (?1, ?2, ?3)",
            rusqlite::params![ts, kind.as_str(), payload],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// How many rows are waiting. Surfaced so a stuck run reads as
    /// "N events waiting" rather than silence.
    pub fn depth(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM spill", [], |row| row.get(0))?)
    }

    /// Replay every row in `id` order, deleting each only once `apply` reports
    /// it committed. Returns the number replayed.
    ///
    /// A row whose `kind` is unrecognised (a downgrade after a future version
    /// wrote it) stops the drain and stays in place rather than being silently
    /// dropped — losing it is the one outcome this module exists to prevent.
    /// Rows queued ahead of it are still replayed first.
    ///
    /// `apply` returning `Err` aborts the drain and leaves that row and every
    /// row after it in place; the caller retries on the next pass. Stopping
    /// rather than skipping is deliberate: replay order is part of the
    /// contract.
    pub fn drain<F>(&mut self, batch: usize, mut apply: F) -> Result<usize>
    where
        F: FnMut(&SpilledRow) -> Result<()>,
    {
        let mut replayed = 0usize;
        loop {
            match self.drain_batch(batch, i64::MAX, &mut apply)? {
                0 => return Ok(replayed),
                moved => replayed += moved,
            }
        }
    }

    /// The highest `id` currently queued, or `None` for an empty journal.
    ///
    /// The ceiling [`SpillGate::drain`] snapshots before it starts, so a replay
    /// is bounded by the backlog it found rather than chasing rows appended
    /// while it runs.
    pub fn max_id(&self) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT MAX(id) FROM spill", [], |row| {
                row.get::<_, Option<i64>>(0)
            })?)
    }

    /// One pass: replay at most `batch` rows with `id <= up_to`, in `id` order,
    /// deleting each only once `apply` reports it committed. Returns how many
    /// moved — `0` means there is nothing left at or below the ceiling.
    ///
    /// Split out of [`Self::drain`] so [`SpillGate::drain`] can hand the
    /// journal's mutex back between passes. Holding it across the whole replay
    /// blocked every other writer — each on a `spawn_blocking` thread — for the
    /// length of the backlog, which is the opposite of what a batch size is
    /// for.
    fn drain_batch<F>(&mut self, batch: usize, up_to: i64, apply: &mut F) -> Result<usize>
    where
        F: FnMut(&SpilledRow) -> Result<()>,
    {
        let rows = self.take(batch, up_to)?;
        let mut replayed = 0usize;

        for row in &rows {
            // Decoded one row at a time, not a batch at a time: a row this
            // build cannot read must not block the perfectly replayable
            // rows queued ahead of it.
            let row = row.decode()?;
            apply(&row)?;
            // Only now is the row redundant. A crash above replays it.
            self.conn
                .execute("DELETE FROM spill WHERE id = ?1", rusqlite::params![row.id])?;
            replayed += 1;
        }

        Ok(replayed)
    }

    /// The next `limit` rows at or below `up_to`, in `id` order, still
    /// undecoded.
    fn take(&self, limit: usize, up_to: i64) -> Result<Vec<RawSpilledRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, kind, payload FROM spill WHERE id <= ?2 ORDER BY id ASC LIMIT ?1",
        )?;

        let rows = stmt
            .query_map(rusqlite::params![limit as i64, up_to], |row| {
                Ok(RawSpilledRow {
                    id: row.get(0)?,
                    ts: row.get(1)?,
                    kind: row.get(2)?,
                    payload: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

/// Ownership of a raised gate, for the length of one rewrite.
///
/// Created by [`SpillGate::raise`]. Exactly one of two things happens to it:
/// a replayer accepts the backlog ([`SpillLease::handed_to_replay`]) and
/// becomes responsible for lowering the gate, or the lease is dropped and
/// lowers the gate itself. There is no third path, and `Drop` runs while a
/// panic unwinds, so no failure inside the rewrite can wedge the process into
/// spilling every write forever.
pub struct SpillLease {
    gate: Arc<SpillGate>,
    handed_off: bool,
}

impl SpillLease {
    /// A replayer has accepted the backlog and now owns lowering the gate.
    /// Consumes the lease so it cannot also be dropped into the fallback.
    pub fn handed_to_replay(mut self) {
        self.handed_off = true;
    }
}

impl Drop for SpillLease {
    fn drop(&mut self) {
        if !self.handed_off {
            // Nothing is going to replay the journal, so leaving the gate up
            // would spill every subsequent write with no drain to end it.
            // Whatever is already queued stays queued and is durable; the next
            // startup replays it.
            tracing::warn!(
                "a database rewrite ended without handing off its spill journal; \
                 resuming direct writes (queued rows replay at the next drain)"
            );
            self.gate.resume_direct();
        }
    }
}

/// Which way writes are currently routed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpillPhase {
    /// Normal operation: writes go to the main database.
    Direct = 0,
    /// A rewrite holds the write lock; writes go to the journal.
    Spilling = 1,
    /// The rewrite is done and the journal is being replayed. Still spilling,
    /// because new work must not overtake the backlog.
    Replaying = 2,
}

impl SpillPhase {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => SpillPhase::Spilling,
            2 => SpillPhase::Replaying,
            _ => SpillPhase::Direct,
        }
    }
}

/// Shared routing decision plus the journal every writer appends to.
///
/// Held as `Arc<SpillGate>` by the persist thread, the auth service, the
/// notifier and the maintenance task.
pub struct SpillGate {
    phase: AtomicU8,
    journal: Mutex<SpillJournal>,
}

impl SpillGate {
    pub fn new(journal: SpillJournal) -> Self {
        Self {
            phase: AtomicU8::new(SpillPhase::Direct as u8),
            journal: Mutex::new(journal),
        }
    }

    /// Open the journal beside the main database and wrap it in a gate.
    ///
    /// `data_dir`/`key` are the same pair every `Storage::open` call site
    /// already threads through.
    ///
    /// **A non-empty journal opens the gate already raised.** Rows in it were
    /// accepted by a process that died mid-rewrite and have not been replayed,
    /// so the gate starts in [`SpillPhase::Replaying`]: every writer that opens
    /// after this — auth, the notifier, the request path, the audit writer — spills
    /// rather than writing directly, and a fresh row cannot commit ahead of an
    /// older accepted one. The persist thread's startup drain lowers it.
    ///
    /// Self-limiting by construction: on a clean boot the journal is empty, the
    /// phase is `Direct` before `main` opens a single service, and nothing
    /// stalls.
    pub fn open(data_dir: &Path, key: Option<&str>) -> Result<Self> {
        let journal = SpillJournal::open(&data_dir.join("portal.spill.db"), key)?;
        let backlog = journal.depth().unwrap_or(0);
        let gate = Self::new(journal);
        if backlog > 0 {
            tracing::warn!(
                backlog,
                "the spill journal holds writes a previous run did not replay; \
                 deferring every writer until they are drained"
            );
            gate.begin_replaying();
        }
        Ok(gate)
    }

    pub fn phase(&self) -> SpillPhase {
        SpillPhase::from_u8(self.phase.load(Ordering::Acquire))
    }

    /// Raise the gate. Called as part of claiming a full vacuum, under the same
    /// lock as `begin_full_vacuum`, so the claim and the reroute are atomic
    /// with respect to each other.
    ///
    /// Prefer [`SpillGate::raise`], which cannot be left raised by a panic.
    pub fn begin_spilling(&self) {
        self.phase
            .store(SpillPhase::Spilling as u8, Ordering::Release);
    }

    /// Raise the gate and take a [`SpillLease`] for the length of one rewrite.
    ///
    /// The gate is a two-step protocol — raise, then either hand the backlog to
    /// a replayer or lower it again — and the second step lives at the end of a
    /// *detached* blocking task. A panic there (or a runtime shutting down
    /// before the task runs) used to leave the gate raised, so every subsequent
    /// write in the process spilled forever, until a restart. The lease closes
    /// that by construction: dropping it resolves the phase, including while a
    /// panic unwinds.
    pub fn raise(gate: &Arc<SpillGate>) -> SpillLease {
        gate.begin_spilling();
        SpillLease {
            gate: Arc::clone(gate),
            handed_off: false,
        }
    }

    /// The rewrite is finished; the backlog is now draining.
    pub fn begin_replaying(&self) {
        self.phase
            .store(SpillPhase::Replaying as u8, Ordering::Release);
    }

    /// Replay is complete. Ordered *after* the last replayed row commits, so no
    /// direct write can overtake the backlog.
    pub fn resume_direct(&self) {
        self.phase
            .store(SpillPhase::Direct as u8, Ordering::Release);
    }

    /// Append a deferred write. `ts` is the acceptance timestamp, preserved so
    /// a replayed reading appears with its original time rather than the
    /// replay's.
    pub fn append(&self, kind: SpillKind, ts: i64, payload: &[u8]) -> Result<i64> {
        let journal = self.journal.lock().unwrap_or_else(|p| p.into_inner());
        journal.append(kind, ts, payload)
    }

    pub fn depth(&self) -> Result<i64> {
        let journal = self.journal.lock().unwrap_or_else(|p| p.into_inner());
        journal.depth()
    }

    /// Drain the backlog that was queued when this call started.
    ///
    /// **The lock is taken per batch, not for the whole drain.** Replay stays
    /// single-threaded and ordered — the mutex is still what serialises it —
    /// but a writer that has to spill during a long replay waits one batch
    /// rather than the whole backlog. Holding it throughout meant every other
    /// writer (`AuthService`'s audit rows, the request path's report history,
    /// `Maintenance`'s own audit) blocked in `append` for the length of the
    /// replay, each occupying a `spawn_blocking` thread.
    ///
    /// Bounded by the `MAX(id)` snapshot taken up front, so releasing the lock
    /// cannot turn this into a chase: rows appended *during* the replay are the
    /// next pass's problem, which is exactly what `persist::drain_until_empty`
    /// loops for.
    pub fn drain<F>(&self, batch: usize, mut apply: F) -> Result<usize>
    where
        F: FnMut(&SpilledRow) -> Result<()>,
    {
        let ceiling = {
            let journal = self.journal.lock().unwrap_or_else(|p| p.into_inner());
            match journal.max_id()? {
                Some(id) => id,
                None => return Ok(0),
            }
        };

        let mut replayed = 0usize;
        loop {
            let moved = {
                let mut journal = self.journal.lock().unwrap_or_else(|p| p.into_inner());
                journal.drain_batch(batch, ceiling, &mut apply)?
            };
            if moved == 0 {
                return Ok(replayed);
            }
            replayed += moved;
        }
    }

    /// Attempt `op` against the main database; on failure, preserve `payload`
    /// here instead of dropping it.
    ///
    /// This is the whole guarantee in one function. Callers that have somebody
    /// to report a failure to (an HTTP handler answering a person) want
    /// [`with_retry`] on its own instead — see the portal's `durable` module
    /// for where that line is drawn.
    ///
    /// Returns `Err` only when the journal itself refused the row: the one
    /// outcome where data really is at risk, logged at `error!` accordingly.
    /// `op` is retried internally, so callers must not wrap it in a retry loop
    /// of their own.
    pub fn write_or_spill<F>(&self, kind: SpillKind, payload: &[u8], op: F) -> Result<Landed>
    where
        F: FnMut() -> Result<()>,
    {
        let mut op = op;

        // A rewrite is known to hold the lock: skip the doomed attempt. Racing
        // this check is harmless — the write below just fails and spills. The
        // phase is an optimisation, never the thing that makes this correct.
        if self.phase() != SpillPhase::Direct {
            return self.preserve(kind, payload, None);
        }

        match with_retry(&mut op) {
            Ok(()) => Ok(Landed::Committed),
            Err(err) if err.is_retryable() => {
                // Contention outlasting both `BUSY_TIMEOUT` and the retry
                // budget — in practice a full VACUUM of a large database.
                self.preserve(kind, payload, Some(&err))
            }
            Err(err) => {
                // A constraint violation or a corrupt file: retrying is
                // useless, so this is a real defect. Say so loudly, but still
                // keep the row — even the pathological case is preserved for
                // inspection rather than discarded.
                tracing::error!(
                    %err,
                    kind = kind.as_str(),
                    "storage write failed unretryably; spilling for inspection"
                );
                self.preserve(kind, payload, Some(&err))
            }
        }
    }

    fn preserve(
        &self,
        kind: SpillKind,
        payload: &[u8],
        cause: Option<&StorageError>,
    ) -> Result<Landed> {
        match self.append(kind, now_unix(), payload) {
            Ok(id) => {
                if let Some(err) = cause {
                    tracing::warn!(
                        %err,
                        kind = kind.as_str(),
                        id,
                        "write deferred to the spill journal"
                    );
                }
                Ok(Landed::Spilled(id))
            }
            Err(err) => {
                tracing::error!(
                    %err,
                    kind = kind.as_str(),
                    bytes = payload.len(),
                    "SPILL JOURNAL REFUSED A WRITE — this row is at risk of being lost"
                );
                Err(err)
            }
        }
    }
}

/// Where a write actually landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landed {
    /// Committed to the main database.
    Committed,
    /// Committed to the spill journal under this id, awaiting replay.
    Spilled(i64),
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
