//! Database maintenance: incremental reclaim, the full VACUUM, and the
//! bookkeeping the operator-facing action reads.
//!
//! The behaviour under test here is easy to get wrong silently, which is why it
//! is pinned rather than reviewed: `auto_vacuum` only takes on an empty file,
//! and `bundled-sqlcipher` defaults temp storage to *memory*, so both are
//! properties of how the database was opened rather than of any statement we
//! write.

use portal_storage::{Landed, SpillGate, SpillKind, Storage};
use rusqlite::Connection;
use std::time::Duration;
use tempfile::tempdir;

/// Fill the database with enough rows to leave a substantial freelist behind
/// after deleting them, so reclaim is measurable rather than theoretical.
///
/// One transaction on a side connection, not `insert_raw` per row: each of
/// those is its own commit and so its own WAL fsync, and at 28 000 rows across
/// the suite that was ~10 minutes of CI disk waits. The pages the deletes free
/// are the same either way; only the commit count changes.
fn churn(path: &std::path::Path, rows: i64) {
    let mut conn = Connection::open(path).expect("churn open");
    let tx = conn.transaction().expect("churn begin");
    {
        let mut insert = tx
            .prepare(
                "INSERT OR IGNORE INTO readings_raw \
                 (hostname, sensor_line, ts, min_c, avg_c, max_c, sample_count) \
                 VALUES ('hub-1', ?1, ?2, 3.0, 4.0, 5.0, 12)",
            )
            .expect("churn prepare");
        for i in 0..rows {
            insert
                .execute(rusqlite::params![i % 8, 1_700_000_000 + i])
                .expect("insert raw");
        }
    }
    tx.commit().expect("churn commit");
}

#[test]
fn fresh_database_is_incremental_auto_vacuum() {
    let dir = tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("portal.db"), None).expect("open");

    // 2 == INCREMENTAL. If this ever reads 0, `Storage::open` set the pragma
    // too late — after a table existed — and every incremental_vacuum call in
    // the retention sweep is a silent no-op.
    assert_eq!(
        storage.auto_vacuum_mode().expect("auto_vacuum"),
        2,
        "fresh database must be created with auto_vacuum=INCREMENTAL"
    );
}

#[test]
fn a_pre_v7_database_is_detected_and_convertible() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");

    // Stand in for a database created before auto_vacuum was requested: make a
    // non-empty file with auto_vacuum=NONE *first*, so Storage::open's pragma
    // is the silent no-op it would be on a real upgrade.
    {
        let conn = Connection::open(&path).expect("raw open");
        conn.execute_batch(
            "PRAGMA auto_vacuum = NONE; \
             CREATE TABLE legacy(x); \
             INSERT INTO legacy VALUES (1);",
        )
        .expect("seed legacy db");
    }

    let storage = Storage::open(&path, None).expect("open legacy");
    assert_eq!(
        storage.auto_vacuum_mode().expect("auto_vacuum"),
        0,
        "auto_vacuum cannot change on a populated file — this is what we detect"
    );

    storage.set_temp_store_file().expect("temp_store");
    storage
        .convert_to_incremental_auto_vacuum()
        .expect("convert");

    assert_eq!(
        storage.auto_vacuum_mode().expect("auto_vacuum"),
        2,
        "the converting VACUUM must leave the database incremental"
    );
}

#[test]
fn incremental_vacuum_returns_pages_and_shrinks_the_file() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    let storage = Storage::open(&path, None).expect("open");

    churn(&path, 4_000);
    let grown = storage.db_size_bytes().expect("size");
    storage
        .sweep_retention(&portal_storage::RetentionPolicy::default(), 2_000_000_000)
        .expect("sweep");

    let reclaimed = storage.incremental_vacuum(u32::MAX).expect("incremental");

    // The `> 1` is the point of this assertion, not `> 0`. `PRAGMA
    // incremental_vacuum` yields one row per reclaimed page and only advances
    // as far as the caller draws rows, so an implementation that executes it
    // without draining reclaims exactly one page per call and still looks like
    // it works. If this ever reads 1, that regressed.
    assert!(
        reclaimed > 1,
        "expected a bulk reclaim, got {reclaimed} page(s) — \
         the incremental_vacuum result rows are not being drained"
    );
    assert!(
        storage.db_size_bytes().expect("size") < grown,
        "incremental_vacuum reported {reclaimed} pages but the file did not shrink"
    );

    // One drained call is enough; there is nothing left to reclaim.
    assert_eq!(storage.incremental_vacuum(u32::MAX).expect("second"), 0);
}

#[test]
fn incremental_vacuum_respects_the_page_cap() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    let storage = Storage::open(&path, None).expect("open");

    churn(&path, 4_000);
    storage
        .sweep_retention(&portal_storage::RetentionPolicy::default(), 2_000_000_000)
        .expect("sweep");

    // The cap is what keeps the write lock short on a device that is
    // monitoring 24/7, so it has to actually bound the work.
    let reclaimed = storage.incremental_vacuum(4).expect("capped");
    assert!(
        reclaimed <= 4,
        "cap of 4 pages reclaimed {reclaimed} instead"
    );
    assert!(
        storage.incremental_vacuum(u32::MAX).expect("rest") > 0,
        "the capped call should have left pages behind"
    );
}

#[test]
fn full_vacuum_claim_is_exclusive_and_records_its_outcome() {
    let dir = tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("portal.db"), None).expect("open");

    let state = storage.maintenance_state().expect("state");
    assert!(!state.full_vacuum_running);
    assert_eq!(state.last_full_vacuum_at, None);

    assert!(storage.begin_full_vacuum().expect("first claim"));
    assert!(
        !storage.begin_full_vacuum().expect("second claim"),
        "a second concurrent request must be refused, not queued"
    );

    storage.set_temp_store_file().expect("temp_store");
    storage.full_vacuum().expect("vacuum");
    storage
        .finish_full_vacuum(1_700_000_000, 1_234, None, false)
        .expect("finish");

    let state = storage.maintenance_state().expect("state");
    assert!(!state.full_vacuum_running);
    assert_eq!(state.last_full_vacuum_at, Some(1_700_000_000));
    assert_eq!(state.last_full_vacuum_ms, Some(1_234));
    assert_eq!(state.last_full_vacuum_error, None);

    // The claim is releasable and re-takeable afterwards.
    assert!(storage.begin_full_vacuum().expect("re-claim"));
}

#[test]
fn a_failed_full_vacuum_keeps_the_previous_success_timestamp() {
    let dir = tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("portal.db"), None).expect("open");

    storage.begin_full_vacuum().expect("claim");
    storage
        .finish_full_vacuum(1_700_000_000, 10, None, false)
        .expect("succeed");

    storage.begin_full_vacuum().expect("claim again");
    storage
        .finish_full_vacuum(1_700_009_999, 20, Some("disk full"), false)
        .expect("fail");

    let state = storage.maintenance_state().expect("state");
    assert_eq!(
        state.last_full_vacuum_at,
        Some(1_700_000_000),
        "a failure must not advance the last-success timestamp"
    );
    assert_eq!(state.last_full_vacuum_error.as_deref(), Some("disk full"));
    assert!(!state.full_vacuum_running);
}

#[test]
fn a_stale_running_flag_is_cleared_at_startup() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");

    {
        let storage = Storage::open(&path, None).expect("open");
        storage.begin_full_vacuum().expect("claim");
        // Process dies here: the flag survives, the VACUUM does not.
    }

    let storage = Storage::open(&path, None).expect("reopen");
    assert!(
        storage
            .maintenance_state()
            .expect("state")
            .full_vacuum_running
    );
    assert!(storage.clear_stale_full_vacuum_flag().expect("clear"));

    let state = storage.maintenance_state().expect("state");
    assert!(!state.full_vacuum_running);
    assert_eq!(
        state.last_full_vacuum_error.as_deref(),
        Some("interrupted by shutdown")
    );
    // Idempotent — a clean startup clears nothing.
    assert!(!storage.clear_stale_full_vacuum_flag().expect("clear again"));
}

/// The regression test for the defect this change exists to fix.
///
/// rusqlite already sets a 5 s `busy_timeout` on every connection, so a writer
/// losing the lock briefly was never a problem. The problem is a lock held
/// *longer* than that: `persist.rs` answers a storage error by logging and
/// moving on, so anything the ingest path tried to write — alarm rows included
/// — was discarded rather than retried. A full VACUUM holds the write lock for
/// its entire duration, and 5 s is not much: 1.2 GB took ~3 s on a tmpfs, and
/// this device writes to eMMC.
///
/// **This test is deliberately slow.** It has to outlast rusqlite's default to
/// prove anything, so it holds the lock for [`HELD`] and would pass trivially
/// with any shorter hold.
#[test]
fn a_blocked_writer_waits_out_a_lock_longer_than_rusqlites_default() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Comfortably past rusqlite's 5 s default, short enough to keep the suite
    /// bearable.
    const HELD: Duration = Duration::from_millis(5_500);

    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");
    let writer = Storage::open(&path, None).expect("open writer");

    // Hold an exclusive write transaction from another connection, the way a
    // full VACUUM does, and let the writer race it.
    let blocker_path = path.clone();
    let (locked_tx, locked_rx) = mpsc::channel();
    let blocker = std::thread::spawn(move || {
        let conn = Connection::open(&blocker_path).expect("blocker open");
        conn.execute_batch("BEGIN EXCLUSIVE").expect("take lock");
        locked_tx.send(()).expect("signal");
        std::thread::sleep(HELD);
        conn.execute_batch("COMMIT").expect("release lock");
    });
    locked_rx.recv().expect("lock held");

    let started = Instant::now();
    let row = portal_storage::RawReading {
        hostname: "hub-1".into(),
        sensor_line: 3,
        ts: 1_700_000_000,
        min_c: Some(3.0),
        avg_c: Some(4.0),
        max_c: Some(5.0),
        sample_count: Some(12),
    };
    writer
        .insert_raw(&row)
        .expect("a write blocked past rusqlite's 5 s default must still wait, not be dropped");
    let waited = started.elapsed();

    blocker.join().expect("blocker");

    assert!(
        waited >= Duration::from_secs(5),
        "insert returned after {waited:?}; it cannot have outlasted the default timeout"
    );
}

/// End-to-end: nothing written during a full VACUUM is lost, whichever path
/// each row takes.
///
/// The writer runs through `write_or_spill` with its `busy_timeout` dropped to
/// 1 ms, so a refusal *would* be caught — but do not read a pass here as proof
/// that the spill path ran. Measured on a dev host: with 20 000 rows (and again
/// with 200 000) not one write was refused, because VACUUM only holds the
/// exclusive lock for a moment at the end and a local disk gets through the
/// rewrite far too quickly. The 2026-08-24 field measurement — 1401 of 1542
/// writes refused — was a large database on eMMC, which is not reproducible
/// here.
///
/// So this stays what it always was: an end-to-end no-loss check. The guard for
/// the refusal itself is `a_write_refused_by_a_held_lock_spills_and_replays`
/// below, which forces the lock instead of racing for it.
///
/// Counts rows rather than trusting the absence of an error: the 2026-08-24
/// defect was silent, and an error that nobody returns is exactly what it
/// looked like.
#[test]
fn writes_during_a_full_vacuum_are_not_lost() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");

    let seed = Storage::open(&path, None).expect("open seed");
    churn(&path, 20_000);
    seed.sweep_retention(&portal_storage::RetentionPolicy::default(), 2_000_000_000)
        .expect("sweep");

    let vacuum_path = path.clone();
    let done = Arc::new(AtomicBool::new(false));
    let vacuum_done = Arc::clone(&done);
    let vacuum = std::thread::spawn(move || {
        let storage = Storage::open(&vacuum_path, None).expect("open vacuum conn");
        storage.set_temp_store_file().expect("temp_store");
        storage.full_vacuum().expect("full vacuum");
        vacuum_done.store(true, Ordering::SeqCst);
    });

    let writer = Storage::open(&path, None).expect("open writer");
    // Impatient on purpose: with the production 30 s the rewrite finishes
    // first and the spill path is never taken, which is what made this a
    // characterisation test rather than a guard.
    writer
        .set_busy_timeout(Duration::from_millis(1))
        .expect("busy_timeout");

    let gate = SpillGate::open(dir.path(), None).expect("open spill");

    let mut attempted = 0i64;
    let mut spilled = 0i64;
    while !done.load(Ordering::SeqCst) && attempted < 200 {
        let row = raw_row(attempted);
        match gate
            .write_or_spill(SpillKind::Mqtt, &attempted.to_le_bytes(), || {
                writer.insert_raw(&row)
            })
            .expect("neither the database nor the journal accepted the row")
        {
            Landed::Committed => {}
            Landed::Spilled(_) => spilled += 1,
        }
        attempted += 1;
    }
    vacuum.join().expect("vacuum thread");

    // The lock is free again: replay, exactly as the persist thread does.
    let mut replayed = 0i64;
    gate.drain(64, |spill_row| {
        replayed += 1;
        writer.insert_raw(&raw_row(decode_index(&spill_row.payload)))
    })
    .expect("replay");

    assert_eq!(replayed, spilled, "every spilled row must be replayed");

    let stored = writer
        .count_raw("hub-2", 1)
        .expect("count rows written during the vacuum") as i64;
    assert_eq!(
        stored, attempted,
        "{} of {attempted} rows written during the VACUUM were lost ({spilled} took the spill path)",
        attempted - stored
    );
}

/// The same guarantee, made deterministic.
///
/// The test above races a real VACUUM, so how many writes are refused depends
/// on machine speed — it proves nothing is lost, but not that the spill path
/// ran. Here an explicit `BEGIN EXCLUSIVE` holds the write lock for certain, so
/// the refusal, the spill and the replay are all forced.
#[test]
fn a_write_refused_by_a_held_lock_spills_and_replays() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("portal.db");

    let writer = Storage::open(&path, None).expect("open writer");
    writer
        .set_busy_timeout(Duration::from_millis(1))
        .expect("busy_timeout");
    let gate = SpillGate::open(dir.path(), None).expect("open spill");

    let blocker = Connection::open(&path).expect("open blocker");
    blocker
        .execute_batch(
            "BEGIN EXCLUSIVE; INSERT INTO readings_raw \
             (hostname, sensor_line, ts, min_c, avg_c, max_c, sample_count) \
             VALUES ('blocker', 9, 1, 0, 0, 0, 1);",
        )
        .expect("hold the write lock");

    let row = raw_row(0);
    let landed = gate
        .write_or_spill(SpillKind::Mqtt, &0i64.to_le_bytes(), || {
            writer.insert_raw(&row)
        })
        .expect("the journal must accept what the database refused");

    assert!(
        matches!(landed, Landed::Spilled(_)),
        "a write refused by a held lock must be preserved, not dropped"
    );
    assert_eq!(
        writer.count_raw("hub-2", 1).expect("count"),
        0,
        "and it must not be in the main database yet"
    );

    blocker.execute_batch("ROLLBACK").expect("release the lock");

    gate.drain(64, |spill_row| {
        writer.insert_raw(&raw_row(decode_index(&spill_row.payload)))
    })
    .expect("replay");

    assert_eq!(
        writer.count_raw("hub-2", 1).expect("count"),
        1,
        "replay must land the deferred row once the lock is free"
    );
    assert_eq!(gate.depth().expect("depth"), 0, "and drain the journal");
}

fn decode_index(payload: &[u8]) -> i64 {
    i64::from_le_bytes(payload.try_into().expect("an 8-byte index"))
}

/// A row the writer is about to spill, keyed so `count_raw("hub-2", 1)` counts
/// exactly the rows these two tests wrote.
fn raw_row(n: i64) -> portal_storage::RawReading {
    portal_storage::RawReading {
        hostname: "hub-2".into(),
        sensor_line: 1,
        ts: 1_900_000_000 + n,
        min_c: Some(1.0),
        avg_c: Some(2.0),
        max_c: Some(3.0),
        sample_count: Some(1),
    }
}
