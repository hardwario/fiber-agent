//! Node history backfill jobs: `backfill_jobs` (ws.md `backfill_job`).
//!
//! Lifecycle: `pending → requested → receiving → done`, terminal branches
//! `failed` / `cancelled` (+ `device_refused` flag). Active =
//! pending/requested/receiving; only requested/receiving can be credited
//! with history frames; `pending` is the only freely-cancellable state.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;

use crate::error::Result;
use crate::models::{BackfillJob, NewBackfillJob, JOB_PENDING, JOB_RECEIVING, JOB_REQUESTED};
use crate::Storage;

const JOB_COLUMNS: &str = "job_id, hostname, node_key, status, time_from, time_to, \"trigger\", \
     samples_expected, samples_received, retry_count, error_message, \
     device_refused, requested_at, last_sample_at, not_before, created_at, updated_at";

impl Storage {
    /// Create a job in `pending`; returns the stored row.
    pub fn create_job(&self, job: &NewBackfillJob) -> Result<BackfillJob> {
        let now = now_unix();
        self.conn.execute(
            "INSERT INTO backfill_jobs
                (hostname, node_key, status, time_from, time_to, \"trigger\",
                 samples_expected, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            params![
                job.hostname,
                job.node_key,
                JOB_PENDING,
                job.time_from,
                job.time_to,
                job.trigger,
                job.samples_expected,
                now,
            ],
        )?;
        let job_id = self.conn.last_insert_rowid();
        match self.get_job(job_id)? {
            Some(row) => Ok(row),
            // Unreachable: we just inserted it on the same connection.
            None => Err(rusqlite::Error::QueryReturnedNoRows.into()),
        }
    }

    /// One job by id, or `None`.
    pub fn get_job(&self, job_id: i64) -> Result<Option<BackfillJob>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM backfill_jobs WHERE job_id = ?1"
        ))?;
        let mut rows = stmt.query_map(params![job_id], job_from_row)?;
        rows.next().transpose().map_err(Into::into)
    }

    /// Jobs for a Node, newest first. `limit` is clamped to `1..=200`
    /// (default 20 when `None`).
    pub fn list_jobs(&self, node_key: &str, limit: Option<i64>) -> Result<Vec<BackfillJob>> {
        let limit = limit.unwrap_or(20).clamp(1, 200);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM backfill_jobs
             WHERE node_key = ?1
             ORDER BY job_id DESC
             LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![node_key, limit], job_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Every Node on this Hub whose **most recent** backfill job ended
    /// `failed` — the terminal state §6.3's "cannot download history" alert
    /// is raised from, made visible on the fault axis by §19 (F12).
    ///
    /// Latest-job-only rather than "has ever failed" so the fault clears by
    /// itself: the next job for that Node — an operator's, or the automatic
    /// one the next reconnect creates — supersedes the failure, exactly as an
    /// operator would expect after retrying. `job_id` is the autoincrement
    /// key, so it orders by creation.
    ///
    /// A window the Node refused (`device_refused`) is not a fault: the Node
    /// answered that it holds nothing there, which devices.backend.md §18.8
    /// says raises no §6.3 alert, and the gap list already reads "not on the
    /// Node". Bench 2026-09-28: a reflashed Node refused the window of its own
    /// flash and showed "History could not be downloaded".
    pub fn nodes_with_failed_backfill(&self, hostname: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT node_key FROM backfill_jobs b
             WHERE b.hostname = ?1
               AND b.status = 'failed'
               AND b.device_refused = 0
               AND b.job_id = (SELECT job_id FROM backfill_jobs
                                WHERE hostname = b.hostname AND node_key = b.node_key
                                ORDER BY time_to DESC, job_id DESC LIMIT 1)",
        )?;
        let rows = stmt.query_map(params![hostname], |r| r.get(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Active (pending/requested/receiving) jobs on one device, oldest
    /// first — the reconciler's work queue.
    pub fn active_jobs(&self, hostname: &str) -> Result<Vec<BackfillJob>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM backfill_jobs
             WHERE hostname = ?1 AND status IN ('pending', 'requested', 'receiving')
             ORDER BY job_id ASC"
        ))?;
        let rows = stmt.query_map(params![hostname], job_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Set a job's status (+ `error_message`, `device_refused`), stamping
    /// `updated_at` and — on a transition into `requested` — `requested_at`.
    /// Returns the updated row, or `None` if the job does not exist. An
    /// invalid status fails the DB `CHECK`.
    pub fn update_job_status(
        &self,
        job_id: i64,
        status: &str,
        error_message: Option<&str>,
        device_refused: bool,
    ) -> Result<Option<BackfillJob>> {
        let now = now_unix();
        let changed = self.conn.execute(
            "UPDATE backfill_jobs
             SET status = ?2,
                 error_message = ?3,
                 device_refused = ?4,
                 requested_at = CASE WHEN ?2 = ?5 THEN ?6 ELSE requested_at END,
                 updated_at = ?6
             WHERE job_id = ?1",
            params![
                job_id,
                status,
                error_message,
                device_refused,
                JOB_REQUESTED,
                now
            ],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_job(job_id)
    }

    /// Credit a received history frame: bump `samples_received` by
    /// `points_inserted` (dedup-safe — a replayed frame that inserted no
    /// new rows credits nothing), stamp `last_sample_at = ts` when the
    /// frame actually inserted rows, and promote `requested → receiving`.
    ///
    /// **`points_inserted`, not `points_seen`** (plan/portal/mqtt/design.md
    /// §6.2). rumqttc replays unacked publishes on reconnect regardless of
    /// `clean_session`, so a frame can arrive twice; the second carries the
    /// same records (`points_seen > 0`) and inserts none. Stamping on
    /// `points_seen` moved the column forward for a frame that added
    /// nothing, which would read as progress that never happened. Nothing
    /// reads the column today, so this is a correctness fix made before
    /// something does — `points_seen` stays a parameter because it is what
    /// distinguishes "an empty frame" from "a replayed one"; the persist sink
    /// logs that distinction at debug before crediting (the second review
    /// found this comment promising a log line nobody had written).
    ///
    /// Gated on status ∈ requested|receiving: returns `Some(updated row)`
    /// only when the credit applied, `None` for a missing job or any other
    /// status (pending/terminal frames are stray and must not mutate).
    pub fn credit_frames(
        &self,
        job_id: i64,
        _points_seen: i64,
        points_inserted: i64,
        ts: i64,
    ) -> Result<Option<BackfillJob>> {
        let now = now_unix();
        let changed = self.conn.execute(
            "UPDATE backfill_jobs
             SET samples_received = samples_received + ?2,
                 last_sample_at = CASE WHEN ?2 > 0 THEN ?3 ELSE last_sample_at END,
                 status = ?4,
                 updated_at = ?5
             WHERE job_id = ?1 AND status IN (?6, ?4)",
            params![
                job_id,
                points_inserted,
                ts,
                JOB_RECEIVING,
                now,
                JOB_REQUESTED,
            ],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_job(job_id)
    }

    /// Bump `retry_count` (+ `updated_at`); returns the updated row, or
    /// `None` if the job does not exist.
    pub fn increment_retry(&self, job_id: i64) -> Result<Option<BackfillJob>> {
        let changed = self.conn.execute(
            "UPDATE backfill_jobs
             SET retry_count = retry_count + 1, updated_at = ?2
             WHERE job_id = ?1",
            params![job_id, now_unix()],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_job(job_id)
    }

    /// One reconciler re-send, recorded as a single statement: bump
    /// `retry_count` and put the job back into `requested` at the same time.
    ///
    /// **One statement, not two, because the caller retries it.** The service
    /// used to run [`Self::increment_retry`] and
    /// [`Self::update_job_status`] back to back inside a
    /// [`crate::with_retry`] closure, and `retry_count = retry_count + 1` is
    /// not idempotent: a `SQLITE_BUSY` on the *second* statement — exactly what
    /// a maintenance rewrite produces — re-ran the first, so one contended
    /// re-send could burn the whole `max_retries` budget and fail the job as
    /// "retries exhausted" after a single downlink. A statement that returns
    /// `SQLITE_BUSY` has not committed, so retrying this one cannot
    /// double-count.
    ///
    /// The status half is [`Self::update_job_status`]'s `(JOB_REQUESTED, None,
    /// false)` case spelled out: `requested_at` restamps because the status
    /// *is* `requested`.
    ///
    /// Returns the updated row, or `None` if the job does not exist.
    pub fn record_retry_sent(&self, job_id: i64) -> Result<Option<BackfillJob>> {
        let now = now_unix();
        let changed = self.conn.execute(
            "UPDATE backfill_jobs
             SET retry_count = retry_count + 1,
                 status = ?2,
                 error_message = NULL,
                 device_refused = 0,
                 requested_at = ?3,
                 updated_at = ?3
             WHERE job_id = ?1",
            params![job_id, JOB_REQUESTED, now],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_job(job_id)
    }

    /// Arm this job's cadence-aware retry deadline: the reconciler will not
    /// re-send before `not_before` (V11, devices.backend.md §6.2/§16.1).
    /// Written by the service right after a downlink actually went out, so
    /// the clock starts at the send, not at the last credited frame.
    /// Returns `false` when the job does not exist.
    pub fn set_not_before(&self, job_id: i64, not_before: i64) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE backfill_jobs SET not_before = ?2 WHERE job_id = ?1",
            params![job_id, not_before],
        )?;
        Ok(changed > 0)
    }

    /// Cancel a job. Only `pending` is freely cancellable (ws.md);
    /// returns `false` when the job is missing or in any other state —
    /// the caller maps that to its conflict/error response.
    pub fn cancel_job(&self, job_id: i64) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE backfill_jobs
             SET status = ?2, updated_at = ?3
             WHERE job_id = ?1 AND status = ?4",
            params![job_id, "cancelled", now_unix(), JOB_PENDING],
        )?;
        Ok(changed > 0)
    }

    /// Whether the device already refused a job for `node_key` whose window covers
    /// `[from_ts, to_ts]` whole (`device_refused`, a terminal answer such as
    /// `HISTORY_UNAVAILABLE`). The automatic path does not ask for such a window again (N37).
    pub fn refused_covering(&self, node_key: &str, from_ts: i64, to_ts: i64) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM backfill_jobs
             WHERE node_key = ?1 AND status = 'failed' AND device_refused = 1
               AND time_from <= ?2 AND time_to >= ?3",
            params![node_key, from_ts, to_ts],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// The end of the newest window an automatic job of `trigger` served for `node_key`: the
    /// holes before it were asked for already, filled or not (devices.node.md §3.9, amendment
    /// 2026-09-25, item 5). `None` before the first one.
    pub fn served_horizon(&self, node_key: &str, trigger: &str) -> Result<Option<i64>> {
        Ok(self.conn.query_row(
            "SELECT MAX(time_to) FROM backfill_jobs
             WHERE node_key = ?1 AND \"trigger\" = ?2 AND status = 'done'",
            params![node_key, trigger],
            |r| r.get(0),
        )?)
    }

    /// When the newest request of `trigger` for `node_key` went out, the spacing between
    /// automatic requests counts from it. `None` before the first one.
    pub fn last_requested_at(&self, node_key: &str, trigger: &str) -> Result<Option<i64>> {
        Ok(self.conn.query_row(
            "SELECT MAX(requested_at) FROM backfill_jobs
             WHERE node_key = ?1 AND \"trigger\" = ?2",
            params![node_key, trigger],
            |r| r.get(0),
        )?)
    }

    /// Whether the Node already answered for `[from_ts, to_ts]` whole: a finished job, or one
    /// it refused, covers the window. A hole still open under such an answer is one the Node
    /// does not hold, so the gap list offers no download for it (P-8).
    pub fn answered_covering(&self, node_key: &str, from_ts: i64, to_ts: i64) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM backfill_jobs
             WHERE node_key = ?1
               AND (status = 'done' OR (status = 'failed' AND device_refused = 1))
               AND time_from <= ?2 AND time_to >= ?3",
            params![node_key, from_ts, to_ts],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Active jobs for a Node whose `[time_from, time_to]` window
    /// intersects `[from_ts, to_ts]` — a non-empty result is the REST
    /// layer's 409 "active job overlaps" conflict. Newest first.
    pub fn find_overlapping(
        &self,
        node_key: &str,
        from_ts: i64,
        to_ts: i64,
    ) -> Result<Vec<BackfillJob>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM backfill_jobs
             WHERE node_key = ?1
               AND status IN ('pending', 'requested', 'receiving')
               AND time_from <= ?3 AND time_to >= ?2
             ORDER BY job_id DESC"
        ))?;
        let rows = stmt.query_map(params![node_key, from_ts, to_ts], job_from_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

fn job_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<BackfillJob> {
    Ok(BackfillJob {
        job_id: r.get(0)?,
        hostname: r.get(1)?,
        node_key: r.get(2)?,
        status: r.get(3)?,
        time_from: r.get(4)?,
        time_to: r.get(5)?,
        trigger: r.get(6)?,
        samples_expected: r.get(7)?,
        samples_received: r.get(8)?,
        retry_count: r.get(9)?,
        error_message: r.get(10)?,
        device_refused: r.get(11)?,
        requested_at: r.get(12)?,
        last_sample_at: r.get(13)?,
        not_before: r.get(14)?,
        created_at: r.get(15)?,
        updated_at: r.get(16)?,
    })
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
