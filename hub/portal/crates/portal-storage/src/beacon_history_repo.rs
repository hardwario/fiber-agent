//! Beacon archive history jobs: `beacon_history_jobs` (V29,
//! plan/portal/beacon_history.md §4.1).
//!
//! Unlike `backfill_jobs`, the Hub runs the job (`control/ble` owns the
//! retries, §2.4) and the portal records what the Hub reports on
//! `eye/history`. States are the `JOB_*` constants; `pending` is the
//! portal's own "asked, the Hub has not said anything yet" and the only one
//! an operator can cancel. A `done` row is final; a `failed` one is not,
//! because the Hub re-queues an unreachable/busy/deadline failure under the
//! same id. A `cancelled` row stays cancelled whatever the Hub says later —
//! the operator's decision is the portal's record.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, OptionalExtension};

use crate::error::Result;
use crate::models::{BeaconHistoryJob, BeaconHistoryReport, NewBeaconHistoryJob, JOB_PENDING};
use crate::Storage;

const COLUMNS: &str = "id, hostname, mac, from_ts, to_ts, \"trigger\", state, received, \
     expected, retry, reason, created_by, created_at, updated_at, sessions, rejected_chunks, \
     future_dropped, crc_failed, compared, disagreed";

/// The trigger a job the Hub started on its own (CLI, an older portal) is
/// recorded under (§4.1).
const UNKNOWN_JOB_TRIGGER: &str = "manual";

/// Id prefix of the download the Hub runs before it changes a Beacon's
/// recording, because such a change erases the archive (§8, amended
/// 2026-10-09).
const DRAIN_JOB_PREFIX: &str = "bh-hub-drain-";

/// Trigger of that download: nobody asked for it, so "ručně" would mislead.
pub const DRAIN_JOB_TRIGGER: &str = "auto-before-recording-change";

/// The trigger a job first seen in a Hub report is stored under.
fn unknown_job_trigger(id: &str) -> &'static str {
    if id.starts_with(DRAIN_JOB_PREFIX) {
        DRAIN_JOB_TRIGGER
    } else {
        UNKNOWN_JOB_TRIGGER
    }
}

impl Storage {
    /// Create a job in `pending`; returns the stored row.
    pub fn create_beacon_history_job(&self, job: &NewBeaconHistoryJob) -> Result<BeaconHistoryJob> {
        let now = now_unix();
        self.conn.execute(
            "INSERT INTO beacon_history_jobs
                (id, hostname, mac, from_ts, to_ts, \"trigger\", state, created_by,
                 created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            params![
                job.id,
                job.hostname,
                job.mac,
                job.from_ts,
                job.to_ts,
                job.trigger,
                JOB_PENDING,
                job.created_by,
                now,
            ],
        )?;
        match self.get_beacon_history_job(&job.id)? {
            Some(row) => Ok(row),
            None => Err(rusqlite::Error::QueryReturnedNoRows.into()),
        }
    }

    /// One job by id, or `None`.
    pub fn get_beacon_history_job(&self, id: &str) -> Result<Option<BeaconHistoryJob>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM beacon_history_jobs WHERE id = ?1"),
                params![id],
                job_from_row,
            )
            .optional()?)
    }

    /// One Beacon's jobs, newest first. `limit` is clamped to `1..=200`
    /// (default 20), as `list_jobs`.
    pub fn list_beacon_history_jobs(
        &self,
        mac: &str,
        limit: Option<i64>,
    ) -> Result<Vec<BeaconHistoryJob>> {
        let limit = limit.unwrap_or(20).clamp(1, 200);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM beacon_history_jobs
             WHERE mac = ?1
             ORDER BY created_at DESC, rowid DESC
             LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![mac, limit], job_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The Beacon's job in flight (pending/requested/receiving), if any —
    /// §4.1's "one job per Beacon in flight".
    pub fn active_beacon_history_job(&self, mac: &str) -> Result<Option<BeaconHistoryJob>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM beacon_history_jobs
                     WHERE mac = ?1 AND state IN ('pending', 'requested', 'receiving')
                     ORDER BY created_at DESC, rowid DESC LIMIT 1"
                ),
                params![mac],
                job_from_row,
            )
            .optional()?)
    }

    /// Applies one `eye/history` state message (§2.2). A job id the portal
    /// does not know — a download started from the CLI — gets a row of its
    /// own (`trigger = manual`, `created_by` NULL). Returns the row after
    /// the update, or `None` when the message was ignored (the row is
    /// `done` or `cancelled`).
    ///
    /// Idempotent: the persist writer may replay a message, and applying
    /// the same state twice leaves the same row.
    pub fn apply_beacon_history_report(
        &self,
        report: &BeaconHistoryReport,
    ) -> Result<Option<BeaconHistoryJob>> {
        let now = now_unix();
        self.conn.execute(
            "INSERT OR IGNORE INTO beacon_history_jobs
                (id, hostname, mac, from_ts, to_ts, \"trigger\", state, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            params![
                report.id,
                report.hostname,
                report.mac,
                report.from_ts,
                report.to_ts,
                unknown_job_trigger(&report.id),
                JOB_PENDING,
                now,
            ],
        )?;
        // `received` never goes down: the Hub's count restarts with a retried
        // attempt, while the rows earlier batches stored stay credited.
        let changed = self.conn.execute(
            "UPDATE beacon_history_jobs
             SET state = ?2,
                 received = MAX(received, COALESCE(?3, received)),
                 expected = COALESCE(?4, expected),
                 from_ts = COALESCE(?5, from_ts),
                 to_ts = COALESCE(?6, to_ts),
                 retry = COALESCE(?7, retry),
                 reason = CASE WHEN ?8 IS NOT NULL THEN ?8
                               WHEN ?2 = 'done' THEN NULL
                               ELSE reason END,
                 sessions = COALESCE(?9, sessions),
                 rejected_chunks = COALESCE(?10, rejected_chunks),
                 future_dropped = COALESCE(?11, future_dropped),
                 crc_failed = COALESCE(?12, crc_failed),
                 updated_at = ?13
             WHERE id = ?1 AND state NOT IN ('done', 'cancelled')",
            params![
                report.id,
                report.state,
                report.received,
                report.expected,
                report.from_ts,
                report.to_ts,
                report.retry,
                report.reason,
                report.sessions,
                report.rejected_chunks,
                report.future_dropped,
                report.crc_failed,
                now,
            ],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_beacon_history_job(&report.id)
    }

    /// Credits a `records` batch (§2.3) to its job without changing its
    /// state: `stored` rows the batch added are counted into `received`, and
    /// a job in flight gets its silence clock reset, since the batch is the
    /// Hub talking about it. Batches stream during the download (§8,
    /// corrected 2026-10-09), so a job that later fails keeps both its rows
    /// and their count. Returns whether a row moved.
    pub fn credit_beacon_history_records(&self, id: &str, stored: i64) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE beacon_history_jobs
             SET received = received + ?2,
                 updated_at = CASE WHEN state IN ('pending', 'requested', 'receiving')
                                   THEN ?3 ELSE updated_at END
             WHERE id = ?1",
            params![id, stored, now_unix()],
        )?;
        Ok(changed > 0)
    }

    /// Moves a job that is still `pending` to `state` (with `reason`) — the
    /// portal's own edges: the Hub acknowledged the request, or the request
    /// never left. A job the Hub has already reported on is left alone,
    /// since its own message may overtake the acknowledgement.
    pub fn settle_pending_beacon_history_job(
        &self,
        id: &str,
        state: &str,
        reason: Option<&str>,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE beacon_history_jobs SET state = ?2, reason = ?3, updated_at = ?4
             WHERE id = ?1 AND state = 'pending'",
            params![id, state, reason, now_unix()],
        )?;
        Ok(changed > 0)
    }

    /// Cancels one Beacon's job, only while it is `pending`.
    pub fn cancel_beacon_history_job(&self, mac: &str, id: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE beacon_history_jobs SET state = 'cancelled', updated_at = ?3
             WHERE id = ?1 AND mac = ?2 AND state = 'pending'",
            params![id, mac, now_unix()],
        )?;
        Ok(changed > 0)
    }

    /// Removes a job that was never sent (refused before signing): nothing
    /// happened, so nothing is left in the table either.
    pub fn delete_beacon_history_job(&self, id: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "DELETE FROM beacon_history_jobs WHERE id = ?1 AND state = 'pending'",
            params![id],
        )?;
        Ok(changed > 0)
    }

    /// Fails every job in flight the Hub has said nothing about since
    /// `cutoff` — `retry_cutoff` for one it is retrying — (§4.1: a Hub
    /// restart lost its queue) and returns them.
    pub fn fail_silent_beacon_history_jobs(
        &self,
        cutoff: i64,
        retry_cutoff: i64,
        reason: &str,
    ) -> Result<Vec<BeaconHistoryJob>> {
        // A job the Hub is retrying (`retry >= 1`) may sit silent through the
        // retry ladder's longest wait (beacon_history.md §8), so it gets the
        // older cutoff.
        let mut stmt = self.conn.prepare(
            "SELECT id FROM beacon_history_jobs
             WHERE state IN ('pending', 'requested', 'receiving')
               AND updated_at < CASE WHEN retry >= 1 THEN ?2 ELSE ?1 END",
        )?;
        let ids = stmt
            .query_map(params![cutoff, retry_cutoff], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let now = now_unix();
        let mut out = Vec::new();
        for id in ids {
            let changed = self.conn.execute(
                "UPDATE beacon_history_jobs SET state = 'failed', reason = ?2, updated_at = ?3
                 WHERE id = ?1 AND state IN ('pending', 'requested', 'receiving')",
                params![id, reason, now],
            )?;
            if changed > 0 {
                if let Some(job) = self.get_beacon_history_job(&id)? {
                    out.push(job);
                }
            }
        }
        Ok(out)
    }

    /// Records the cross-check of §7 Q6 layer 3 on a finished job.
    pub fn set_beacon_history_crosscheck(
        &self,
        id: &str,
        compared: i64,
        disagreed: i64,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE beacon_history_jobs SET compared = ?2, disagreed = ?3 WHERE id = ?1",
            params![id, compared, disagreed],
        )?;
        Ok(changed > 0)
    }

    /// One Beacon's readings in `[from_ts, to_ts]` as `(ts, temperature,
    /// event_type)`, ordered by `ts`, across every Hub that heard it — the
    /// cross-check's input. Keyed by MAC alone for the reason
    /// `last_eye_reading_ts` gives.
    pub fn eye_points_for_mac(
        &self,
        mac: &str,
        from_ts: i64,
        to_ts: i64,
    ) -> Result<Vec<(i64, Option<f64>, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT ts, temperature_c, event_type FROM eye_readings
             WHERE mac = ?1 AND ts >= ?2 AND ts <= ?3
             ORDER BY ts ASC",
        )?;
        let rows = stmt.query_map(params![mac, from_ts, to_ts], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The distinct timestamps of one Beacon's stored readings, every event
    /// type, in `[from_ts, to_ts)`, ascending — the gap detector's input. An
    /// archive row fills a hole exactly as a live one would.
    pub fn eye_reading_timestamps(&self, mac: &str, from_ts: i64, to_ts: i64) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT ts FROM eye_readings
             WHERE mac = ?1 AND ts >= ?2 AND ts < ?3
             ORDER BY ts ASC",
        )?;
        let rows = stmt.query_map(params![mac, from_ts, to_ts], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

fn job_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<BeaconHistoryJob> {
    Ok(BeaconHistoryJob {
        id: r.get(0)?,
        hostname: r.get(1)?,
        mac: r.get(2)?,
        from_ts: r.get(3)?,
        to_ts: r.get(4)?,
        trigger: r.get(5)?,
        state: r.get(6)?,
        received: r.get(7)?,
        expected: r.get(8)?,
        retry: r.get(9)?,
        reason: r.get(10)?,
        created_by: r.get(11)?,
        created_at: r.get(12)?,
        updated_at: r.get(13)?,
        sessions: r.get(14)?,
        rejected_chunks: r.get(15)?,
        future_dropped: r.get(16)?,
        crc_failed: r.get(17)?,
        compared: r.get(18)?,
        disagreed: r.get(19)?,
    })
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
