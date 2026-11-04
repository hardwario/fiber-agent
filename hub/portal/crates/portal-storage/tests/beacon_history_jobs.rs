//! V29 `beacon_history_jobs` (plan/portal/beacon_history.md §4.1, §7).
//!
//! The rules that matter: a Hub report on an unknown id creates the row
//! (a CLI-started download), `done` and `cancelled` are final, `failed` is
//! not (the Hub retries under the same id), a replayed message changes
//! nothing, and only `pending` can be cancelled.

use portal_storage::{
    BeaconHistoryReport, EyeReading, NewBeaconHistoryJob, Storage, JOB_CANCELLED, JOB_DONE,
    JOB_FAILED, JOB_PENDING, JOB_RECEIVING, JOB_REQUESTED,
};
use tempfile::tempdir;

const MAC: &str = "7C:D9:F4:10:6E:30";

fn open() -> (tempfile::TempDir, Storage) {
    let dir = tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("portal.db"), None).expect("open+migrate");
    (dir, storage)
}

fn new_job(id: &str) -> NewBeaconHistoryJob {
    NewBeaconHistoryJob {
        id: id.into(),
        hostname: "hub-1".into(),
        mac: MAC.into(),
        from_ts: Some(1_000),
        to_ts: Some(2_000),
        trigger: "manual".into(),
        created_by: Some("manager1".into()),
    }
}

fn report(id: &str, state: &str) -> BeaconHistoryReport {
    BeaconHistoryReport {
        id: id.into(),
        hostname: "hub-1".into(),
        mac: MAC.into(),
        state: state.into(),
        ..Default::default()
    }
}

#[test]
fn a_new_job_is_pending_and_the_one_in_flight() {
    let (_dir, s) = open();
    let job = s
        .create_beacon_history_job(&new_job("bh-1"))
        .expect("create");
    assert_eq!(job.state, JOB_PENDING);
    assert_eq!(job.received, 0);
    assert_eq!(job.crc_failed, None);
    assert_eq!(
        s.active_beacon_history_job(MAC)
            .expect("active")
            .map(|j| j.id),
        Some("bh-1".to_string())
    );
}

#[test]
fn reports_move_the_job_and_keep_what_they_do_not_carry() {
    let (_dir, s) = open();
    s.create_beacon_history_job(&new_job("bh-1"))
        .expect("create");
    s.apply_beacon_history_report(&BeaconHistoryReport {
        received: Some(240),
        expected: Some(1344),
        ..report("bh-1", JOB_RECEIVING)
    })
    .expect("receiving");
    let done = s
        .apply_beacon_history_report(&BeaconHistoryReport {
            received: Some(1344),
            sessions: Some(2),
            rejected_chunks: Some(0),
            future_dropped: Some(1),
            ..report("bh-1", JOB_DONE)
        })
        .expect("done")
        .expect("applied");
    assert_eq!(done.state, JOB_DONE);
    assert_eq!(done.received, 1344);
    assert_eq!(done.expected, Some(1344), "kept from the receiving message");
    assert_eq!(done.sessions, Some(2));
    assert_eq!(done.crc_failed, None, "unknown stays unknown, never 0");
    assert_eq!(done.created_by.as_deref(), Some("manager1"));

    // Final: a late or replayed message changes nothing.
    assert!(s
        .apply_beacon_history_report(&report("bh-1", JOB_RECEIVING))
        .expect("late")
        .is_none());
    assert_eq!(
        s.get_beacon_history_job("bh-1")
            .expect("get")
            .expect("row")
            .state,
        JOB_DONE
    );
}

#[test]
fn an_unknown_job_from_the_hub_gets_a_manual_row_with_nobody_behind_it() {
    let (_dir, s) = open();
    let job = s
        .apply_beacon_history_report(&BeaconHistoryReport {
            from_ts: Some(5_000),
            ..report("hub-made-1", JOB_REQUESTED)
        })
        .expect("apply")
        .expect("created");
    assert_eq!(job.trigger, "manual");
    assert_eq!(job.created_by, None);
    assert_eq!(job.state, JOB_REQUESTED);
    assert_eq!(job.from_ts, Some(5_000));
}

#[test]
fn a_failed_job_comes_back_on_a_hub_retry_but_a_cancelled_one_does_not() {
    let (_dir, s) = open();
    s.create_beacon_history_job(&new_job("bh-1"))
        .expect("create");
    s.apply_beacon_history_report(&BeaconHistoryReport {
        reason: Some("unreachable".into()),
        ..report("bh-1", JOB_FAILED)
    })
    .expect("failed");
    let again = s
        .apply_beacon_history_report(&BeaconHistoryReport {
            retry: Some(1),
            ..report("bh-1", JOB_REQUESTED)
        })
        .expect("retry")
        .expect("applied");
    assert_eq!(again.state, JOB_REQUESTED);
    assert_eq!(again.retry, 1);
    assert_eq!(
        again.reason.as_deref(),
        Some("unreachable"),
        "why it is retried"
    );

    s.create_beacon_history_job(&new_job("bh-2"))
        .expect("create");
    assert!(s.cancel_beacon_history_job(MAC, "bh-2").expect("cancel"));
    assert!(s
        .apply_beacon_history_report(&report("bh-2", JOB_RECEIVING))
        .expect("late")
        .is_none());
    assert_eq!(
        s.get_beacon_history_job("bh-2")
            .expect("get")
            .expect("row")
            .state,
        JOB_CANCELLED
    );
}

#[test]
fn only_a_pending_job_of_that_beacon_can_be_cancelled() {
    let (_dir, s) = open();
    s.create_beacon_history_job(&new_job("bh-1"))
        .expect("create");
    assert!(!s
        .cancel_beacon_history_job("AA:BB:CC:DD:EE:FF", "bh-1")
        .expect("other mac"));
    s.apply_beacon_history_report(&report("bh-1", JOB_REQUESTED))
        .expect("requested");
    assert!(!s.cancel_beacon_history_job(MAC, "bh-1").expect("requested"));
}

#[test]
fn a_silent_job_fails_and_a_talking_one_does_not() {
    let (_dir, s) = open();
    s.create_beacon_history_job(&new_job("bh-1"))
        .expect("create");
    // Nothing is older than a cutoff in the past.
    assert!(s
        .fail_silent_beacon_history_jobs(0, 0, "silent")
        .expect("sweep")
        .is_empty());
    let failed = s
        .fail_silent_beacon_history_jobs(i64::MAX, 0, "silent")
        .expect("sweep");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].state, JOB_FAILED);
    assert_eq!(failed[0].reason.as_deref(), Some("silent"));
    assert!(s.active_beacon_history_job(MAC).expect("active").is_none());
}

#[test]
fn readings_feed_the_gap_detector_and_the_cross_check_across_event_types() {
    let (_dir, s) = open();
    for (ts, event_type) in [(100, "live"), (100, "recording"), (900, "recording")] {
        s.insert_eye_reading(&EyeReading {
            hostname: "hub-1".into(),
            mac: MAC.into(),
            ts,
            temperature_c: Some(4.0),
            humidity_pct: None,
            event_type: event_type.into(),
            message_id: Some(format!("{MAC}-{event_type}-{ts}")),
        })
        .expect("insert");
    }
    assert_eq!(
        s.eye_reading_timestamps(MAC, 0, 1_000).expect("ts"),
        vec![100, 900]
    );
    let points = s.eye_points_for_mac(MAC, 0, 1_000).expect("points");
    assert_eq!(points.len(), 3);
}

#[test]
fn a_job_the_hub_is_retrying_gets_the_longer_silence() {
    let (_dir, s) = open();
    s.create_beacon_history_job(&new_job("bh-1"))
        .expect("create");
    s.apply_beacon_history_report(&BeaconHistoryReport {
        retry: Some(1),
        reason: Some("busy".into()),
        ..report("bh-1", JOB_REQUESTED)
    })
    .expect("retrying");
    // Past the plain cutoff, not past the retry one: still waiting.
    assert!(s
        .fail_silent_beacon_history_jobs(i64::MAX, 0, "silent")
        .expect("sweep")
        .is_empty());
    assert_eq!(
        s.fail_silent_beacon_history_jobs(i64::MAX, i64::MAX, "silent")
            .expect("sweep")
            .len(),
        1
    );
}

#[test]
fn the_hubs_download_before_a_recording_change_is_not_manual() {
    let (_dir, s) = open();
    let job = s
        .apply_beacon_history_report(&report("bh-hub-drain-1791556248-0a1b2c3d", JOB_RECEIVING))
        .expect("report")
        .expect("row");
    assert_eq!(job.trigger, portal_storage::DRAIN_JOB_TRIGGER);
    let job = s
        .apply_beacon_history_report(&report("bh-cli-1", JOB_RECEIVING))
        .expect("report")
        .expect("row");
    assert_eq!(job.trigger, "manual", "a CLI download stays manual");
}
