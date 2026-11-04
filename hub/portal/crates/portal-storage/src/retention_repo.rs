//! Retention sweeps (M7).
//!
//! What is pruned and what is deliberately kept forever is a compliance
//! decision, not a tidiness one, so it is stated here rather than spread
//! across call sites:
//!
//! **Pruned — telemetry.** Minute/raw probe readings, Node field/counter/event
//! rows, Beacon readings, the firmware-export ledger, expired sessions, and
//! terminal backfill jobs. These are reproducible measurements with a hot-tier
//! contract (`plan/v1/viewer/mqtt.md` calls `readings_minute` "the
//! authoritative 30-day hot tier"), and unbounded growth on a CM4's `/data`
//! partition is its own hazard.
//!
//! **Never pruned here — the evidence trail.** `alarm_events`, `alarm_notes`,
//! `audit_log`, `field_threshold_history`. For an EU MDR Class IIa product
//! these are the record of what the device reported and who acknowledged it;
//! deleting them on a timer is a regulatory risk, not housekeeping. Per-subject
//! erasure (GDPR Art. 17) is a targeted operation on `users`/`audit_log`
//! anonymisation — a separate feature, deliberately not a sweep.
//!
//! Notification delivery history sits in between: operational noise, not
//! evidence of a measurement, so it is pruned on a long window.

use crate::error::Result;
use crate::models::MaintenanceState;

/// Physical layout of the database file, for the maintenance trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageStats {
    pub page_count: i64,
    pub freelist_count: i64,
    pub page_size: i64,
}

impl PageStats {
    /// Bytes sitting on the freelist — allocated to the file, holding nothing.
    pub fn freelist_bytes(&self) -> u64 {
        (self.freelist_count.max(0) as u64).saturating_mul(self.page_size.max(0) as u64)
    }

    /// Freelist as a fraction of the whole file, `0.0` for an empty database.
    pub fn freelist_fraction(&self) -> f64 {
        if self.page_count <= 0 {
            return 0.0;
        }
        self.freelist_count.max(0) as f64 / self.page_count as f64
    }
}

use crate::Storage;

/// Windows, in days, for each pruned class. Defaults mirror v1's hot tier
/// (30 days) and ledger retention (7 days).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// `readings_minute` — the authoritative hot tier.
    pub minute_days: i64,
    /// `readings_raw` — the fine-grained tier behind ≤2 h chart spans.
    pub raw_days: i64,
    /// Node field/counter/event rows and Beacon readings.
    pub radio_days: i64,
    /// `export_ledger` dedup entries.
    pub ledger_days: i64,
    /// Terminal (`done`/`failed`/`cancelled`) backfill jobs.
    pub job_days: i64,
    /// `report_history` — notification delivery log.
    pub notification_days: i64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            minute_days: 30,
            raw_days: 7,
            radio_days: 30,
            ledger_days: 7,
            job_days: 30,
            notification_days: 90,
        }
    }
}

/// Rows removed per class by one [`Storage::sweep_retention`] pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionReport {
    pub minute_rows: usize,
    pub raw_rows: usize,
    pub node_rows: usize,
    pub eye_rows: usize,
    pub ledger_rows: usize,
    pub session_rows: usize,
    pub job_rows: usize,
    pub notification_rows: usize,
}

impl RetentionReport {
    pub fn total(&self) -> usize {
        self.minute_rows
            + self.raw_rows
            + self.node_rows
            + self.eye_rows
            + self.ledger_rows
            + self.session_rows
            + self.job_rows
            + self.notification_rows
    }
}

impl Storage {
    /// One retention pass. `now_unix` is passed in rather than read from the
    /// clock so the sweep is testable and so a caller can reason about the
    /// cutoffs it is about to apply.
    ///
    /// Each class is deleted in its own statement: a failure part-way through
    /// leaves earlier classes pruned rather than rolling everything back —
    /// retention is idempotent, so the next pass finishes the job.
    pub fn sweep_retention(
        &self,
        policy: &RetentionPolicy,
        now_unix: i64,
    ) -> Result<RetentionReport> {
        let day = 86_400;
        let mut report = RetentionReport::default();

        let cutoff = now_unix - policy.minute_days * day;
        report.minute_rows = self
            .conn
            .execute("DELETE FROM readings_minute WHERE minute_ts < ?1", [cutoff])?;

        let cutoff = now_unix - policy.raw_days * day;
        report.raw_rows = self
            .conn
            .execute("DELETE FROM readings_raw WHERE ts < ?1", [cutoff])?;

        let cutoff = now_unix - policy.radio_days * day;
        let mut node_rows = self
            .conn
            .execute("DELETE FROM node_field_readings WHERE ts < ?1", [cutoff])?;
        node_rows += self
            .conn
            .execute("DELETE FROM node_counter_readings WHERE ts < ?1", [cutoff])?;
        node_rows += self
            .conn
            .execute("DELETE FROM node_events WHERE ts < ?1", [cutoff])?;
        node_rows += self.conn.execute(
            "DELETE FROM node_raw_messages WHERE received_at < ?1",
            [cutoff],
        )?;
        report.node_rows = node_rows;
        report.eye_rows = self
            .conn
            .execute("DELETE FROM eye_readings WHERE ts < ?1", [cutoff])?;

        report.ledger_rows = self.prune_ledger_older_than(policy.ledger_days)?;
        report.session_rows = self.prune_expired_sessions(now_unix)?;

        // Only terminal jobs: an active job older than the window is a stalled
        // job the reconciler still owns, and deleting it would strand the
        // device's history replay with no record of why.
        let cutoff = now_unix - policy.job_days * day;
        report.job_rows = self.conn.execute(
            "DELETE FROM backfill_jobs \
             WHERE status IN ('done','failed','cancelled') AND updated_at < ?1",
            [cutoff],
        )?;
        // The Beacon archive jobs (V29) age out on the same window.
        report.job_rows += self.conn.execute(
            "DELETE FROM beacon_history_jobs \
             WHERE state IN ('done','failed','cancelled') AND updated_at < ?1",
            [cutoff],
        )?;

        // `sent_at` is a `datetime('now')` string, so the cutoff is compared as
        // text — SQLite's ISO-8601 ordering is lexicographic, which is exactly
        // why the column stores that format.
        report.notification_rows = self.conn.execute(
            "DELETE FROM report_history \
             WHERE sent_at < datetime(?1, 'unixepoch')",
            [now_unix - policy.notification_days * day],
        )?;

        Ok(report)
    }

    /// Reclaim up to `max_pages` free pages, returning how many were actually
    /// handed back to the filesystem.
    ///
    /// This is the routine mechanism: `PRAGMA incremental_vacuum` takes the
    /// write lock in short bursts and needs no temporary copy of the database,
    /// so it can run on every sweep of a device that is monitoring 24/7. It
    /// only truncates from the end of the file — it does not repack pages —
    /// which is why [`Self::full_vacuum`] still exists.
    ///
    /// Returns `Ok(0)` when the database predates `auto_vacuum=INCREMENTAL`
    /// (see [`Self::auto_vacuum_mode`]): SQLite keeps no pointer map for such a
    /// file, so the pragma is a silent no-op rather than an error.
    ///
    /// # This pragma has to be *stepped*, not executed
    ///
    /// `PRAGMA incremental_vacuum` is a loop in the VDBE that yields **one
    /// result row per reclaimed page**, so it only makes as much progress as
    /// the caller draws rows. `execute_batch` stops at the first row and
    /// therefore reclaims exactly one page per call, however large a limit it
    /// is given — measured: 122 pages with 118 on the freelist went to 121, and
    /// draining the rows instead took the same file to 4 pages. Hence the
    /// explicit `query` + drain below; it looks redundant and is not.
    pub fn incremental_vacuum(&self, max_pages: u32) -> Result<usize> {
        if max_pages == 0 {
            return Ok(0);
        }
        // Measured in `page_count` — the size of the file, which is the
        // resource being reclaimed. `freelist_count` is not a usable measure
        // here: it under-reports on an auto_vacuum database.
        let before = self.page_count()?;
        {
            let mut stmt = self
                .conn
                .prepare(&format!("PRAGMA incremental_vacuum({max_pages})"))?;
            let mut rows = stmt.query([])?;
            while rows.next()?.is_some() {}
        }
        let after = self.page_count()?;
        Ok((before - after).max(0) as usize)
    }

    /// Rewrite the whole database, repacking pages and shrinking the file to
    /// its minimum.
    ///
    /// **This locks out every writer for its entire duration** (readers are
    /// unaffected — they keep reading the pre-VACUUM WAL snapshot). On this
    /// device the blocked writer is the persist thread, which is also where the
    /// alarm engine runs, so an unattended call delays alarm rows and their
    /// notifications by however long the rewrite takes. That is why nothing
    /// schedules this: it is reachable only from the operator-initiated
    /// maintenance action, after a free-space preflight.
    ///
    /// The caller is responsible for having called
    /// [`Storage::set_temp_store_file`](crate::Storage::set_temp_store_file)
    /// first, or the temporary copy lands in RAM.
    pub fn full_vacuum(&self) -> Result<()> {
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }

    /// `PRAGMA auto_vacuum` — 0 none, 1 full, 2 incremental.
    ///
    /// `Storage::open` requests `INCREMENTAL`, but SQLite honours that only
    /// while the file is still empty, so a database created before that change
    /// still reports 0 and cannot reclaim pages incrementally. Detecting it is
    /// the whole point of this getter; converting requires one full VACUUM.
    pub fn auto_vacuum_mode(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))?)
    }

    /// Switch an existing database to `auto_vacuum=INCREMENTAL`.
    ///
    /// The pragma alone does nothing on a populated file — SQLite needs a full
    /// rewrite to build the pointer map — so this pays [`Self::full_vacuum`]'s
    /// cost, write stall included, and must be treated as such by the caller.
    pub fn convert_to_incremental_auto_vacuum(&self) -> Result<()> {
        self.conn
            .execute_batch("PRAGMA auto_vacuum = INCREMENTAL; VACUUM;")?;
        Ok(())
    }

    /// On-disk size of the main database file, excluding `-wal` and `-shm`.
    /// Derived from SQLite's own page accounting rather than `stat`, so it is
    /// consistent with what a VACUUM is about to rewrite.
    pub fn db_size_bytes(&self) -> Result<i64> {
        let page_count = self.page_count()?;
        let page_size: i64 = self
            .conn
            .query_row("PRAGMA page_size", [], |row| row.get(0))?;
        Ok(page_count * page_size)
    }

    /// Read the `maintenance_state` singleton.
    pub fn maintenance_state(&self) -> Result<MaintenanceState> {
        Ok(self.conn.query_row(
            "SELECT last_full_vacuum_at, last_full_vacuum_ms, last_full_vacuum_error, \
                    full_vacuum_running, auto_vacuum_converted \
             FROM maintenance_state WHERE id = 1",
            [],
            |row| {
                Ok(MaintenanceState {
                    last_full_vacuum_at: row.get(0)?,
                    last_full_vacuum_ms: row.get(1)?,
                    last_full_vacuum_error: row.get(2)?,
                    full_vacuum_running: row.get::<_, i64>(3)? != 0,
                    auto_vacuum_converted: row.get::<_, i64>(4)? != 0,
                })
            },
        )?)
    }

    /// Claim the right to run a full VACUUM, returning `false` when one is
    /// already in progress.
    ///
    /// The claim is the `WHERE full_vacuum_running = 0` clause, not a
    /// read-then-write in the caller: two operators pressing the button at the
    /// same moment must not both start a rewrite of the same file.
    pub fn begin_full_vacuum(&self) -> Result<bool> {
        let claimed = self.conn.execute(
            "UPDATE maintenance_state SET full_vacuum_running = 1 \
             WHERE id = 1 AND full_vacuum_running = 0",
            [],
        )?;
        Ok(claimed == 1)
    }

    /// Record the outcome and release the claim. `error` is `None` on success.
    pub fn finish_full_vacuum(
        &self,
        now_unix: i64,
        duration_ms: i64,
        error: Option<&str>,
        converted_auto_vacuum: bool,
    ) -> Result<()> {
        // `last_full_vacuum_at` marks the last *successful* run, so a failure
        // leaves the previous timestamp alone and only updates the error.
        self.conn.execute(
            "UPDATE maintenance_state \
             SET full_vacuum_running    = 0, \
                 last_full_vacuum_ms    = ?1, \
                 last_full_vacuum_error = ?2, \
                 last_full_vacuum_at    = CASE WHEN ?2 IS NULL THEN ?3 \
                                               ELSE last_full_vacuum_at END, \
                 auto_vacuum_converted  = CASE WHEN ?4 = 1 THEN 1 \
                                               ELSE auto_vacuum_converted END \
             WHERE id = 1",
            rusqlite::params![
                duration_ms,
                error,
                now_unix,
                i64::from(converted_auto_vacuum)
            ],
        )?;
        Ok(())
    }

    /// Clear a `full_vacuum_running` flag left set by a process that died
    /// mid-rewrite. Called once at startup: the flag is in-process state that
    /// happens to be persisted, and no run can be in progress before the
    /// retention task starts.
    ///
    /// VACUUM itself is transactional, so an interrupted one leaves the
    /// database intact — only the flag is stale.
    pub fn clear_stale_full_vacuum_flag(&self) -> Result<bool> {
        let cleared = self.conn.execute(
            "UPDATE maintenance_state SET full_vacuum_running = 0, \
                    last_full_vacuum_error = 'interrupted by shutdown' \
             WHERE id = 1 AND full_vacuum_running = 1",
            [],
        )?;
        Ok(cleared == 1)
    }

    /// Pages, and how many of them are on the freelist.
    ///
    /// The pair the scheduled-maintenance trigger reads: `incremental_vacuum`
    /// hands pages back to the freelist but never defragments and never shrinks
    /// the file below its high-water mark, so a large freelist fraction is
    /// precisely the condition a full rewrite fixes and nothing else does.
    /// Returned together because judging either one alone is misleading — a
    /// fifth of a 4 MB database is not worth a rewrite.
    pub fn page_stats(&self) -> Result<PageStats> {
        Ok(PageStats {
            page_count: self.page_count()?,
            freelist_count: self
                .conn
                .query_row("PRAGMA freelist_count", [], |row| row.get(0))?,
            page_size: self
                .conn
                .query_row("PRAGMA page_size", [], |row| row.get(0))?,
        })
    }

    fn page_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA page_count", [], |row| row.get(0))?)
    }
}
