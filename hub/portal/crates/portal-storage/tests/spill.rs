//! The spill journal: durability and replay ordering.
//!
//! These are pinned rather than reviewed because the properties are invisible
//! in normal operation — the journal is empty except during a maintenance run,
//! so a regression here surfaces only as missing data after a rewrite, which is
//! exactly the failure this module exists to prevent.

use portal_storage::{Landed, SpillGate, SpillJournal, SpillKind, SpillPhase};
use tempfile::tempdir;

fn journal(dir: &std::path::Path) -> SpillJournal {
    SpillJournal::open(&dir.join("portal.spill.db"), None).expect("open spill")
}

#[test]
fn rows_replay_in_the_order_they_were_accepted() {
    let dir = tempdir().expect("tempdir");
    let spill = journal(dir.path());

    for i in 0..50i64 {
        spill
            .append(
                SpillKind::Mqtt,
                1_700_000_000 + i,
                format!("event-{i}").as_bytes(),
            )
            .expect("append");
    }

    let mut spill = spill;
    let mut seen = Vec::new();
    let replayed = spill
        .drain(7, |row| {
            seen.push(String::from_utf8(row.payload.clone()).expect("utf8"));
            Ok(())
        })
        .expect("drain");

    assert_eq!(replayed, 50);
    let expected: Vec<String> = (0..50).map(|i| format!("event-{i}")).collect();
    assert_eq!(
        seen, expected,
        "replay must follow id order across batch boundaries, not per-batch order"
    );
    assert_eq!(
        spill.depth().expect("depth"),
        0,
        "a drained journal is empty"
    );
}

#[test]
fn a_journal_survives_reopening_because_appends_are_committed() {
    let dir = tempdir().expect("tempdir");

    {
        let spill = journal(dir.path());
        spill.append(SpillKind::Audit, 1, b"first").expect("append");
        spill
            .append(SpillKind::Audit, 2, b"second")
            .expect("append");
        assert_eq!(spill.depth().expect("depth"), 2);
    }

    // Reopening stands in for the process dying between the append and the
    // replay — the case the whole design exists for.
    let mut spill = journal(dir.path());
    assert_eq!(
        spill.depth().expect("depth"),
        2,
        "an append that returned Ok must be on disk"
    );

    let mut seen = Vec::new();
    spill
        .drain(16, |row| {
            seen.push(row.payload.clone());
            Ok(())
        })
        .expect("drain");
    assert_eq!(seen, vec![b"first".to_vec(), b"second".to_vec()]);
}

#[test]
fn a_failed_replay_leaves_that_row_and_everything_after_it_in_place() {
    let dir = tempdir().expect("tempdir");
    let spill = journal(dir.path());

    for i in 0..5i64 {
        spill
            .append(SpillKind::Mqtt, i, format!("{i}").as_bytes())
            .expect("append");
    }

    let mut spill = spill;
    let mut applied = 0;
    let result = spill.drain(16, |row| {
        if row.payload == b"2" {
            return Err(portal_storage::StorageError::Conflict("boom".into()));
        }
        applied += 1;
        Ok(())
    });

    assert!(result.is_err(), "the drain reports the failure");
    assert_eq!(applied, 2, "rows before the failure were applied");
    assert_eq!(
        spill.depth().expect("depth"),
        3,
        "the failing row and its successors stay queued — order is part of the contract"
    );

    // The retry replays exactly the remainder, still in order.
    let mut seen = Vec::new();
    spill
        .drain(16, |row| {
            seen.push(String::from_utf8(row.payload.clone()).expect("utf8"));
            Ok(())
        })
        .expect("second drain");
    assert_eq!(seen, vec!["2", "3", "4"]);
}

#[test]
fn a_row_is_only_deleted_after_its_callback_commits() {
    let dir = tempdir().expect("tempdir");
    let spill = journal(dir.path());
    spill.append(SpillKind::Mqtt, 1, b"only").expect("append");

    let mut spill = spill;
    // Simulate a crash *inside* apply: the callback reports failure, so the row
    // must survive. A twice-applied row is harmless; a lost one is not.
    let _ = spill.drain(16, |_| {
        Err(portal_storage::StorageError::Conflict("crash".into()))
    });

    assert_eq!(
        spill.depth().expect("depth"),
        1,
        "a row whose apply did not report success must not be deleted"
    );
}

#[test]
fn every_kind_round_trips_through_the_kind_column() {
    let dir = tempdir().expect("tempdir");
    let spill = journal(dir.path());

    for kind in [
        SpillKind::Mqtt,
        SpillKind::Audit,
        SpillKind::ReportHistory,
        SpillKind::Liveness,
    ] {
        spill.append(kind, 1, b"x").expect("append");
    }

    let mut spill = spill;
    let mut seen = Vec::new();
    spill
        .drain(16, |row| {
            seen.push(row.kind);
            Ok(())
        })
        .expect("drain");

    assert_eq!(
        seen,
        vec![
            SpillKind::Mqtt,
            SpillKind::Audit,
            SpillKind::ReportHistory,
            SpillKind::Liveness
        ],
        "an unrecognised kind would be refused rather than replayed as the wrong type"
    );
}

#[test]
fn an_unknown_kind_is_refused_rather_than_dropped() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.spill.db");
    {
        let spill = journal(dir.path());
        spill
            .append(SpillKind::Mqtt, 1, b"readable")
            .expect("append");
    }

    // A row a future version wrote, read back after a downgrade.
    let raw = rusqlite::Connection::open(&path).expect("raw open");
    raw.execute(
        "INSERT INTO spill (ts, kind, payload) VALUES (2, 'from_the_future', X'00')",
        [],
    )
    .expect("insert");
    drop(raw);

    let mut spill = SpillJournal::open(&path, None).expect("reopen");
    let mut replayed = 0;
    let result = spill.drain(16, |_| {
        replayed += 1;
        Ok(())
    });

    assert!(
        result.is_err(),
        "an unreplayable row must stop the drain, not be silently discarded"
    );
    assert_eq!(
        replayed, 1,
        "the readable row queued ahead of it must still be replayed — decoding \
         happens per row, not per batch"
    );
    assert_eq!(
        spill.depth().expect("depth"),
        1,
        "and the row that could not be read must still be there"
    );
}

// ---- write_or_spill: the delayed-versus-lost decision ---------------------

fn busy() -> portal_storage::StorageError {
    portal_storage::StorageError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
        Some("database is locked".into()),
    ))
}

fn gate(dir: &std::path::Path) -> SpillGate {
    SpillGate::open(dir, None).expect("open gate")
}

#[test]
fn a_write_that_succeeds_is_not_journalled() {
    let dir = tempdir().expect("tempdir");
    let gate = gate(dir.path());

    let landed = gate
        .write_or_spill(SpillKind::Mqtt, b"payload", || Ok(()))
        .expect("write");

    assert_eq!(landed, Landed::Committed);
    assert_eq!(
        gate.depth().expect("depth"),
        0,
        "the journal is for deferred writes only — it must stay empty in normal operation"
    );
}

#[test]
fn contention_is_retried_before_it_is_journalled() {
    let dir = tempdir().expect("tempdir");
    let gate = gate(dir.path());

    let mut attempts = 0;
    let landed = gate
        .write_or_spill(SpillKind::Mqtt, b"payload", || {
            attempts += 1;
            if attempts < 3 {
                Err(busy())
            } else {
                Ok(())
            }
        })
        .expect("write");

    assert_eq!(
        landed,
        Landed::Committed,
        "the retry cleared the contention"
    );
    assert_eq!(gate.depth().expect("depth"), 0);
}

#[test]
fn contention_that_outlasts_the_retry_budget_is_journalled_not_dropped() {
    let dir = tempdir().expect("tempdir");
    let gate = gate(dir.path());

    let landed = gate
        .write_or_spill(SpillKind::Mqtt, b"an alarm row", || Err(busy()))
        .expect("the journal must accept it");

    assert!(matches!(landed, Landed::Spilled(_)));
    assert_eq!(gate.depth().expect("depth"), 1);
}

#[test]
fn an_unretryable_error_is_journalled_too_rather_than_discarded() {
    let dir = tempdir().expect("tempdir");
    let gate = gate(dir.path());

    // A constraint violation or a corrupt file: retrying is useless and this is
    // a real defect, but even the pathological case is preserved for inspection
    // rather than thrown away.
    let landed = gate
        .write_or_spill(SpillKind::Audit, b"audit row", || {
            Err(portal_storage::StorageError::Conflict("bad".into()))
        })
        .expect("the journal must accept it");

    assert!(matches!(landed, Landed::Spilled(_)));
    assert_eq!(gate.depth().expect("depth"), 1);
}

#[test]
fn a_raised_gate_skips_the_attempt_entirely() {
    let dir = tempdir().expect("tempdir");
    let gate = gate(dir.path());
    gate.begin_spilling();

    let mut attempted = false;
    let landed = gate
        .write_or_spill(SpillKind::Mqtt, b"payload", || {
            attempted = true;
            Ok(())
        })
        .expect("write");

    assert!(matches!(landed, Landed::Spilled(_)));
    assert!(
        !attempted,
        "while a rewrite holds the lock the attempt is known to be doomed; \
         the gate exists to skip it, not to make the write correct"
    );

    // Replay drains it, and only then does direct writing resume.
    gate.begin_replaying();
    let mut spill = SpillJournal::open(&dir.path().join("portal.spill.db"), None).expect("reopen");
    assert_eq!(spill.depth().expect("depth"), 1);
    spill.drain(16, |_| Ok(())).expect("drain");

    gate.resume_direct();
    let landed = gate
        .write_or_spill(SpillKind::Mqtt, b"payload", || Ok(()))
        .expect("write");
    assert_eq!(landed, Landed::Committed);
}

/// The gate is a two-step protocol, and the second step used to live at the end
/// of a detached blocking task. A panic before it left every subsequent write
/// in the process spilling forever, with no drain to end it — a wedged portal
/// that only a restart fixed. `Drop` runs while a panic unwinds, so the lease
/// closes that by construction.
#[test]
fn a_lease_dropped_by_a_panicking_rewrite_lowers_the_gate() {
    let dir = tempdir().expect("tempdir");
    let gate = std::sync::Arc::new(SpillGate::open(dir.path(), None).expect("open gate"));

    let raised = std::sync::Arc::clone(&gate);
    let died = std::thread::spawn(move || {
        let _lease = SpillGate::raise(&raised);
        assert_eq!(raised.phase(), SpillPhase::Spilling);
        panic!("the rewrite fell over");
    })
    .join();

    assert!(died.is_err(), "the test is only meaningful if it panicked");
    assert_eq!(
        gate.phase(),
        SpillPhase::Direct,
        "a panicking rewrite must not leave every later write spilling"
    );
}

/// The other half: a lease handed to a replayer must *not* lower the gate, or a
/// direct write could overtake the backlog the replayer is still draining.
#[test]
fn a_lease_handed_to_replay_leaves_the_gate_raised() {
    let dir = tempdir().expect("tempdir");
    let gate = std::sync::Arc::new(SpillGate::open(dir.path(), None).expect("open gate"));

    SpillGate::raise(&gate).handed_to_replay();

    assert_eq!(gate.phase(), SpillPhase::Spilling);
}

/// Rows in the journal at open were accepted by a process that died mid-rewrite
/// and have not been replayed. Opening `Direct` let auth, the notifier and
/// `db_read` — which hold their own connections — commit fresh rows ahead of
/// them, so "replay before any newer work" held only for the ingest path.
#[test]
fn a_journal_with_rows_opens_with_the_gate_already_raised() {
    let dir = tempdir().expect("tempdir");

    {
        let spill = journal(dir.path());
        spill
            .append(SpillKind::Audit, 1, b"accepted before the crash")
            .expect("append");
    }

    let gate = SpillGate::open(dir.path(), None).expect("reopen gate");
    assert_eq!(
        gate.phase(),
        SpillPhase::Replaying,
        "every writer must defer until the backlog is drained"
    );
}

#[test]
fn an_empty_journal_opens_ready_for_direct_writes() {
    let dir = tempdir().expect("tempdir");
    let gate = SpillGate::open(dir.path(), None).expect("open gate");
    assert_eq!(
        gate.phase(),
        SpillPhase::Direct,
        "a clean boot must not defer anything"
    );
}

/// `SpillGate::drain` replays the whole backlog in acceptance order across
/// batch boundaries, even though it now hands the journal's mutex back between
/// them.
///
/// The lock used to be held for the entire replay, so `BATCH` bounded nothing
/// but memory and every other writer blocked in `append` for the length of the
/// backlog — each one on a `spawn_blocking` thread. Releasing it per batch is
/// only safe if ordering survives, which is what this pins.
#[test]
fn a_gate_drain_crosses_batch_boundaries_in_order() {
    let dir = tempdir().expect("tempdir");
    let gate = SpillGate::open(dir.path(), None).expect("open gate");

    for i in 0..50i64 {
        gate.append(
            SpillKind::Mqtt,
            1_700_000_000 + i,
            format!("event-{i}").as_bytes(),
        )
        .expect("append");
    }

    let mut seen = Vec::new();
    let replayed = gate
        .drain(7, |row| {
            seen.push(String::from_utf8(row.payload.clone()).expect("utf8"));
            Ok(())
        })
        .expect("drain");

    assert_eq!(replayed, 50);
    let expected: Vec<String> = (0..50).map(|i| format!("event-{i}")).collect();
    assert_eq!(seen, expected, "order must survive the per-batch relocking");
    assert_eq!(gate.depth().expect("depth"), 0);
}

/// A writer that spills *during* a replay is not swallowed by it, and not
/// replayed by it either.
///
/// The pass is bounded by the `MAX(id)` it snapshots when it starts, so a row
/// appended above that ceiling stays queued for the next pass — which is
/// precisely what `persist::drain_until_empty` loops for, and why it must not
/// lower the gate on the strength of one pass.
#[test]
fn a_row_appended_during_a_replay_waits_for_the_next_pass() {
    use std::sync::Arc;

    let dir = tempdir().expect("tempdir");
    let gate = Arc::new(SpillGate::open(dir.path(), None).expect("open gate"));

    for i in 0..8i64 {
        gate.append(
            SpillKind::Mqtt,
            1_700_000_000 + i,
            format!("old-{i}").as_bytes(),
        )
        .expect("append");
    }

    let latecomer = {
        let gate = Arc::clone(&gate);
        // Blocks on the journal's mutex until a batch boundary, then lands
        // above the ceiling this drain snapshotted.
        move || {
            gate.append(SpillKind::Audit, 1_700_000_099, b"late")
                .expect("append during the replay");
        }
    };

    let mut spawned: Option<std::thread::JoinHandle<()>> = None;
    let mut seen = Vec::new();
    let replayed = gate
        .drain(2, |row| {
            seen.push(String::from_utf8(row.payload.clone()).expect("utf8"));
            if spawned.is_none() {
                spawned = Some(std::thread::spawn(latecomer.clone()));
            }
            Ok(())
        })
        .expect("drain");
    spawned.expect("the writer thread").join().expect("join");

    assert_eq!(replayed, 8, "the pass replays exactly the backlog it found");
    assert!(
        seen.iter().all(|p| p.starts_with("old-")),
        "and nothing that arrived after it started: {seen:?}"
    );
    assert_eq!(
        gate.depth().expect("depth"),
        1,
        "the latecomer is durable and queued, not lost and not replayed early"
    );

    // The next pass takes it, exactly as `drain_until_empty` arranges.
    let mut late = Vec::new();
    assert_eq!(
        gate.drain(2, |row| {
            late.push(String::from_utf8(row.payload.clone()).expect("utf8"));
            Ok(())
        })
        .expect("second pass"),
        1
    );
    assert_eq!(late, vec!["late".to_string()]);
    assert_eq!(gate.depth().expect("depth"), 0);
}
