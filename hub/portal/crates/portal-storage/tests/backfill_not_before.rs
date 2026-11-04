//! B3/§6.2: `backfill_jobs.not_before` (V11) — the per-job, cadence-aware
//! retry deadline that replaced the fleet-wide `stall_timeout_s`.
//!
//! Three things can only go wrong here, and all three are silent:
//! `not_before` reading back as something other than `None` on a row
//! nothing has armed (which would make the reconciler's NULL fallback
//! unreachable), `updated_at` dragging the deadline with it (the whole
//! reason a column was chosen over `updated_at` arithmetic), and the
//! column landing mid-`JOB_COLUMNS` shifting every index after it in
//! `job_from_row`. Precedent for the file: `tests/device_meta.rs` (V9).

use portal_storage::{
    NewBackfillJob, Storage, JOB_DONE, JOB_PENDING, JOB_RECEIVING, JOB_REQUESTED,
};
use tempfile::tempdir;

fn open() -> (tempfile::TempDir, Storage) {
    let dir = tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("portal.db"), None).expect("open+migrate");
    (dir, storage)
}

fn new_job() -> NewBackfillJob {
    NewBackfillJob {
        hostname: "hub-1".into(),
        node_key: "70b3d5e050011234".into(),
        time_from: 1_000,
        time_to: 2_000,
        trigger: "auto-on-reconnect".into(),
        samples_expected: Some(120),
    }
}

/// A job nothing has published has no deadline — `create_job`'s INSERT does
/// not mention the column, which is the same value path V11 gives every row
/// that predates it. The reconciler's NULL fallback exists for exactly this
/// state and would be dead code if a default ever crept in.
#[test]
fn a_fresh_job_carries_no_deadline() {
    let (_dir, s) = open();
    let job = s.create_job(&new_job()).expect("create");
    assert_eq!(job.status, JOB_PENDING);
    assert_eq!(job.not_before, None);
    assert_eq!(
        s.get_job(job.job_id).expect("get").expect("row").not_before,
        None,
        "and it reads back the same way, not as 0"
    );
}

#[test]
fn set_not_before_round_trips_and_is_overwritable() {
    let (_dir, s) = open();
    let job = s.create_job(&new_job()).expect("create");

    assert!(s.set_not_before(job.job_id, 1_768_314_600).expect("arm"));
    assert_eq!(
        s.get_job(job.job_id).expect("get").expect("row").not_before,
        Some(1_768_314_600)
    );

    // A retry re-arms it; the second publish's deadline replaces the first.
    assert!(s.set_not_before(job.job_id, 1_768_315_500).expect("re-arm"));
    assert_eq!(
        s.get_job(job.job_id).expect("get").expect("row").not_before,
        Some(1_768_315_500)
    );
}

#[test]
fn set_not_before_reports_a_missing_job() {
    let (_dir, s) = open();
    assert!(
        !s.set_not_before(999_999, 1_768_314_600)
            .expect("no such job"),
        "a vanished job is not an error, it is a false"
    );
}

/// The deadline must not move when a credited frame or a status change
/// stamps `updated_at` — that is the defect the column was introduced to
/// fix (V11's header: `updated_at` moves on every credited frame, which is
/// exactly the event that should NOT postpone a retry).
#[test]
fn crediting_frames_moves_updated_at_and_not_the_deadline() {
    let (_dir, s) = open();
    let job = s.create_job(&new_job()).expect("create");
    let job = s
        .update_job_status(job.job_id, JOB_REQUESTED, None, false)
        .expect("to requested")
        .expect("row");
    s.set_not_before(job.job_id, 1_768_314_600).expect("arm");

    let credited = s
        .credit_frames(job.job_id, 60, 60, 1_600)
        .expect("credit")
        .expect("credited");
    assert_eq!(credited.status, JOB_RECEIVING);
    assert_eq!(credited.samples_received, 60);
    assert_eq!(
        credited.not_before,
        Some(1_768_314_600),
        "a frame arriving is progress, not a reason to wait longer"
    );

    let bumped = s.increment_retry(job.job_id).expect("retry").expect("row");
    assert_eq!(bumped.retry_count, 1);
    assert_eq!(bumped.not_before, Some(1_768_314_600));

    let done = s
        .update_job_status(job.job_id, JOB_DONE, None, false)
        .expect("to done")
        .expect("row");
    assert_eq!(done.not_before, Some(1_768_314_600));
}

/// `not_before` was inserted into `JOB_COLUMNS` between `last_sample_at`
/// and `created_at`, so every index after it in `job_from_row` shifted. A
/// mis-mapping there is invisible until a timestamp turns up in the wrong
/// field, so read the whole row back through all three of its query paths.
#[test]
fn every_column_still_maps_after_the_new_one() {
    let (_dir, s) = open();
    let job = s.create_job(&new_job()).expect("create");
    s.update_job_status(job.job_id, JOB_REQUESTED, Some("first try"), true)
        .expect("to requested");
    s.credit_frames(job.job_id, 7, 7, 1_555).expect("credit");
    s.increment_retry(job.job_id).expect("retry");
    s.set_not_before(job.job_id, 1_768_314_600).expect("arm");

    let by_id = s.get_job(job.job_id).expect("get").expect("row");
    let listed = s
        .list_jobs("70b3d5e050011234", None)
        .expect("list")
        .into_iter()
        .find(|j| j.job_id == job.job_id)
        .expect("listed");
    let active = s
        .active_jobs("hub-1")
        .expect("active")
        .into_iter()
        .find(|j| j.job_id == job.job_id)
        .expect("active");
    assert_eq!(by_id, listed, "list_jobs maps the same columns as get_job");
    assert_eq!(by_id, active, "active_jobs too");

    assert_eq!(by_id.hostname, "hub-1");
    assert_eq!(by_id.node_key, "70b3d5e050011234");
    assert_eq!(by_id.status, JOB_RECEIVING);
    assert_eq!(by_id.time_from, 1_000);
    assert_eq!(by_id.time_to, 2_000);
    assert_eq!(by_id.trigger, "auto-on-reconnect");
    assert_eq!(by_id.samples_expected, Some(120));
    assert_eq!(by_id.samples_received, 7);
    assert_eq!(by_id.retry_count, 1);
    assert_eq!(by_id.error_message.as_deref(), Some("first try"));
    assert!(by_id.device_refused);
    assert!(by_id.requested_at.is_some());
    assert_eq!(by_id.last_sample_at, Some(1_555));
    assert_eq!(by_id.not_before, Some(1_768_314_600));
    assert!(
        by_id.created_at > 0,
        "created_at is a unix second, not a NULL"
    );
    assert!(by_id.updated_at >= by_id.created_at);
}

/// V11 has to apply to a file that already carries jobs, not just to a
/// fresh one — that is the upgrade path on a device. The reopen is the
/// no-op-migration half; `PRAGMA table_info` is the ALTER's actual shape,
/// and "nullable, no default" is what makes the row above read back `None`.
#[test]
fn v11_applies_over_a_populated_database() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    let job_id = {
        let s = Storage::open(&path, None).expect("open+migrate");
        let job = s.create_job(&new_job()).expect("create");
        s.update_job_status(job.job_id, JOB_REQUESTED, None, false)
            .expect("to requested");
        job.job_id
    };

    let reopened = Storage::open(&path, None).expect("second open is a no-op migration");
    let job = reopened
        .get_job(job_id)
        .expect("get")
        .expect("row survived");
    assert_eq!(job.not_before, None, "a row written before an arm has none");
    assert_eq!(job.status, JOB_REQUESTED);
    assert!(reopened
        .set_not_before(job_id, 42)
        .expect("arm after upgrade"));

    let conn = rusqlite::Connection::open(&path).expect("raw open");
    let (notnull, dflt) = conn
        .query_row(
            "SELECT \"notnull\", dflt_value FROM pragma_table_info('backfill_jobs') \
             WHERE name = 'not_before'",
            [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .expect("the column exists");
    assert_eq!(notnull, 0, "nullable — NULL is the 'no deadline' state");
    assert_eq!(
        dflt, None,
        "no default, or the NULL fallback is unreachable"
    );
}

/// One re-send bumps `retry_count` exactly once, and a refused attempt bumps it
/// not at all.
///
/// The regression this pins: the reconciler ran `increment_retry` and
/// `update_job_status` back to back inside a `with_retry` closure, and
/// `retry_count = retry_count + 1` is not idempotent. A `SQLITE_BUSY` on the
/// second statement — which is exactly what a maintenance rewrite produces —
/// re-ran the first, so a single contended re-send could burn the whole
/// `max_retries` budget (2 by default) and the next reconciler pass would fail
/// the job as "retries exhausted" after one downlink. One statement cannot
/// half-apply: a refused `execute` has not committed.
#[test]
fn a_re_send_bumps_the_retry_count_once_and_a_refused_one_not_at_all() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    let s = Storage::open(&path, None).expect("open+migrate");
    let job = s.create_job(&new_job()).expect("create");

    let bumped = s
        .record_retry_sent(job.job_id)
        .expect("record")
        .expect("row");
    assert_eq!(bumped.retry_count, 1);
    assert_eq!(
        bumped.status, JOB_REQUESTED,
        "and the status half of the old pair still applies"
    );
    assert_eq!(bumped.error_message, None);
    assert!(!bumped.device_refused);

    // Impatient, so the held lock below is a refusal rather than a wait.
    s.set_busy_timeout(std::time::Duration::from_millis(1))
        .expect("busy_timeout");
    let blocker = rusqlite::Connection::open(&path).expect("open blocker");
    blocker
        .execute_batch(
            "BEGIN EXCLUSIVE; INSERT INTO readings_raw \
             (hostname, sensor_line, ts, min_c, avg_c, max_c, sample_count) \
             VALUES ('blocker', 9, 1, 0, 0, 0, 1);",
        )
        .expect("hold the write lock");

    let refused = s
        .record_retry_sent(job.job_id)
        .expect_err("a held lock must refuse the write");
    assert!(
        refused.is_retryable(),
        "and be classified as contention so the caller retries: {refused:?}"
    );

    blocker.execute_batch("ROLLBACK").expect("release the lock");

    assert_eq!(
        s.get_job(job.job_id)
            .expect("get")
            .expect("row")
            .retry_count,
        1,
        "a refused re-send must not have spent a retry"
    );

    // And the retry the caller now makes spends exactly one more.
    portal_storage::with_retry(|| s.record_retry_sent(job.job_id)).expect("retry");
    assert_eq!(
        s.get_job(job.job_id)
            .expect("get")
            .expect("row")
            .retry_count,
        2
    );
}

/// A job that is not there is `None`, not a silent no-op — same contract as
/// `increment_retry` and `update_job_status`.
#[test]
fn recording_a_re_send_for_a_missing_job_is_none() {
    let (_dir, s) = open();
    assert!(s.record_retry_sent(9_999).expect("record").is_none());
}
