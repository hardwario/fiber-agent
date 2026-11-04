//! The spill journal: a durable side-channel for writes `fiber_medical.db`
//! cannot accept right now.
//!
//! A full `VACUUM` rewrites the whole database and holds its write lock for the
//! duration. Today [`RetentionPolicy::enforce`](super::retention::RetentionPolicy)
//! runs that rewrite *inline on the storage thread*, so for however long a
//! 2.5 GB repack takes, every incoming reading waits in the bounded crossbeam
//! channel — 10 000 slots of RAM that a power cut takes with it. This is a
//! medical device; that window is exactly the case the no-data-loss guarantee
//! exists for.
//!
//! So a maintenance run hands the VACUUM to a helper thread on its own
//! connection and the storage thread keeps draining its channel, writing each
//! message *here* instead: a second SQLite database next to the main one, with
//! its own lock, which a VACUUM of `fiber_medical.db` cannot touch. A commit
//! here is the point at which the write counts as accepted. When the rewrite
//! finishes the thread replays this journal in `id` order and resumes direct
//! writes; because one thread drains one FIFO channel, "replay before any newer
//! work" is structural rather than something the code has to arrange.
//!
//! A side SQLite file rather than a hand-rolled append log because atomic,
//! torn-write-safe, fsync-correct appends are precisely the problem SQLite has
//! already solved — and the SQLCipher layer comes with it, which matters
//! because the payloads are patient telemetry.
//!
//! ## Why a separate record type
//!
//! [`StorageMessage`](super::thread::StorageMessage) cannot be serialised: it
//! derives only `Debug`, and several variants carry a `crossbeam::Sender` reply
//! channel. [`SpillRecord`] mirrors only the variants that represent a
//! *durable write*. Query and control messages are served from the live
//! connection as usual — reads are not blocked by a VACUUM until its final
//! moment, and a reply channel is meaningless after a restart anyway.

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::libs::alarms::AlarmState;
use crate::libs::storage::error::{StorageError, StorageResult};
use crate::libs::storage::models::{AlarmEvent, SensorReading};
use crate::libs::storage::writer::StorageWriter;

/// A write deferred while a rewrite holds the main database's lock.
///
/// One variant per durable-write `StorageMessage`. Kept as its own type rather
/// than a `#[derive(Serialize)]` on the message enum so that adding a message
/// with a reply channel cannot silently become unspillable at runtime — it
/// simply has no variant here, and [`spillable`] hands it back to be served
/// normally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SpillRecord {
    SensorReading {
        timestamp: i64,
        sensor_line: u8,
        temperature: f32,
        is_connected: bool,
        alarm_state: AlarmState,
    },
    SensorReadingsBatch {
        readings: Vec<SensorReading>,
    },
    SensorBuckets {
        rows: Vec<super::models::MinuteAggregateRow>,
    },
    AlarmEvent {
        timestamp: i64,
        sensor_line: u8,
        from_state: AlarmState,
        to_state: AlarmState,
        temperature: Option<f32>,
    },
    NodeReading {
        dev_eui: String,
        provisioning_epoch: i64,
        ts: i64,
        received_at: i64,
        message_id: String,
        event_type: String,
        payload_json: String,
    },
    BeaconReading {
        mac: String,
        ts: i64,
        received_at: i64,
        message_id: String,
        event_type: String,
        payload_json: String,
    },
    NodeRemoved {
        dev_eui: String,
        ts: i64,
    },
    AdvanceExportCursor {
        broker_id: String,
        stream: String,
        new_id: i64,
    },
    ResetExportCursor {
        broker_id: String,
        stream: String,
    },
    AuditEvent {
        operation: String,
        table_name: Option<String>,
        details: Option<String>,
    },
}

impl SpillRecord {
    /// Apply this record to the main database.
    ///
    /// Deliberately the *same* `StorageWriter` / `AuditLogger` calls the live
    /// match arms make, so a replayed row and a directly written one cannot
    /// diverge.
    ///
    /// The audit row is written from the storage thread like every other one:
    /// `AuditLogger` hash-chains through `get_latest_audit_hash` with no
    /// locking, so a second thread writing audit rows concurrently would fork
    /// the chain. Serialisation here rests entirely on "one connection, one
    /// thread".
    pub fn apply(&self, conn: &mut Connection, hmac_secret: Option<&[u8]>) -> StorageResult<()> {
        match self {
            SpillRecord::SensorReading {
                timestamp,
                sensor_line,
                temperature,
                is_connected,
                alarm_state,
            } => {
                let reading = SensorReading::new(
                    *timestamp,
                    *sensor_line,
                    *temperature,
                    *is_connected,
                    *alarm_state,
                );
                StorageWriter::write_sensor_reading(conn, &reading, hmac_secret).map(|_| ())
            }
            SpillRecord::SensorReadingsBatch { readings } => {
                StorageWriter::write_sensor_readings_batch(conn, readings, hmac_secret).map(|_| ())
            }
            SpillRecord::SensorBuckets { rows } => rows.iter().try_for_each(|row| {
                StorageWriter::write_sensor_bucket(conn, row, hmac_secret).map(|_| ())
            }),
            SpillRecord::AlarmEvent {
                timestamp,
                sensor_line,
                from_state,
                to_state,
                temperature,
            } => {
                let event = AlarmEvent::new(
                    *timestamp,
                    *sensor_line,
                    *from_state,
                    *to_state,
                    *temperature,
                );
                StorageWriter::write_alarm_event(conn, &event).map(|_| ())
            }
            SpillRecord::NodeReading {
                dev_eui,
                provisioning_epoch,
                ts,
                received_at,
                message_id,
                event_type,
                payload_json,
            } => StorageWriter::write_node_reading(
                conn,
                dev_eui,
                *provisioning_epoch,
                *ts,
                *received_at,
                message_id,
                event_type,
                payload_json,
            )
            .map(|_| ()),
            SpillRecord::BeaconReading {
                mac,
                ts,
                received_at,
                message_id,
                event_type,
                payload_json,
            } => StorageWriter::write_beacon_reading(
                conn,
                mac,
                *ts,
                *received_at,
                message_id,
                event_type,
                payload_json,
            )
            .map(|_| ()),
            SpillRecord::NodeRemoved { dev_eui, ts } => {
                StorageWriter::append_node_removed_event(conn, dev_eui, *ts).map(|_| ())
            }
            SpillRecord::AdvanceExportCursor {
                broker_id,
                stream,
                new_id,
            } => StorageWriter::advance_export_cursor(conn, broker_id, stream, *new_id),
            SpillRecord::ResetExportCursor { broker_id, stream } => {
                StorageWriter::reset_export_cursor(conn, broker_id, stream)
            }
            SpillRecord::AuditEvent {
                operation,
                table_name,
                details,
            } => match details.as_deref() {
                Some(d) => crate::libs::storage::audit::AuditLogger::log_operation_with_details(
                    conn,
                    operation,
                    table_name.as_deref(),
                    d,
                ),
                None => crate::libs::storage::audit::AuditLogger::log_operation(
                    conn,
                    operation,
                    table_name.as_deref(),
                    None,
                    None,
                ),
            },
        }
    }
}

/// The side database.
pub struct SpillJournal {
    conn: Connection,
    path: PathBuf,
}

impl SpillJournal {
    /// Open (creating if needed) the journal beside `main_db_path`.
    ///
    /// `key` is the same SQLCipher key the main database uses — see
    /// [`crate::libs::storage::db::Database::encryption_key`]; `None` is the
    /// unencrypted test/dev fallback, matching `configure_pragmas`.
    ///
    /// `synchronous = FULL`, unlike the main database's `NORMAL`: a commit here
    /// is the point at which a write counts as accepted, so it has to survive a
    /// power cut on its own rather than within a checkpoint window.
    pub fn open(main_db_path: &Path, key: Option<&str>) -> StorageResult<Self> {
        let path = spill_path(main_db_path);
        let conn = Connection::open(&path).map_err(|e| {
            StorageError::DatabaseInitError(format!("Failed to open spill journal: {}", e))
        })?;

        if let Some(key) = key {
            conn.execute_batch(&format!("PRAGMA key = '{}';", key))
                .map_err(|e| {
                    StorageError::DatabaseInitError(format!(
                        "Failed to key the spill journal: {}",
                        e
                    ))
                })?;
        }

        let _mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .map_err(|e| {
                StorageError::DatabaseInitError(format!("spill journal WAL failed: {}", e))
            })?;
        conn.execute("PRAGMA synchronous = FULL", []).map_err(|e| {
            StorageError::DatabaseInitError(format!("spill journal synchronous failed: {}", e))
        })?;
        conn.busy_timeout(Duration::from_secs(30)).map_err(|e| {
            StorageError::DatabaseInitError(format!("spill journal busy_timeout failed: {}", e))
        })?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS spill (
                 id      INTEGER PRIMARY KEY AUTOINCREMENT,
                 ts      INTEGER NOT NULL,
                 payload BLOB    NOT NULL
             );",
        )
        .map_err(|e| {
            StorageError::DatabaseInitError(format!("spill journal schema failed: {}", e))
        })?;

        Ok(Self { conn, path })
    }

    /// Where this journal lives, for logging.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one deferred write. The commit is the acceptance point.
    pub fn append(&self, ts: i64, record: &SpillRecord) -> StorageResult<i64> {
        let payload = serde_json::to_vec(record).map_err(|e| {
            StorageError::QueryError(format!("failed to encode a spill record: {}", e))
        })?;
        self.conn
            .execute(
                "INSERT INTO spill (ts, payload) VALUES (?1, ?2)",
                rusqlite::params![ts, payload],
            )
            .map_err(|e| StorageError::QueryError(format!("spill append failed: {}", e)))?;
        Ok(self.conn.last_insert_rowid())
    }

    /// How many writes are waiting. Surfaced so a stuck run reads as
    /// "N events waiting" rather than silence.
    pub fn depth(&self) -> StorageResult<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM spill", [], |row| row.get(0))
            .map_err(|e| StorageError::QueryError(format!("spill depth failed: {}", e)))
    }

    /// Replay every row into `conn`, in acceptance order, deleting each only
    /// once it has committed. Returns how many were replayed.
    ///
    /// A row that fails to apply stops the drain and stays queued, along with
    /// everything after it: replay order is part of the contract, and dropping
    /// a row we could not apply is the one outcome this module exists to
    /// prevent. The next drain — the next maintenance run, or the next startup
    /// — retries from the same place. A crash between applying a row and
    /// deleting it replays that row, which is harmless: every table this writes
    /// to dedups on a natural key or is an append-only record of an event that
    /// did happen.
    pub fn drain(
        &mut self,
        conn: &mut Connection,
        hmac_secret: Option<&[u8]>,
    ) -> StorageResult<usize> {
        /// Rows per SELECT — enough that a long backlog is not thousands of
        /// round-trips, small enough to bound the memory a drain holds.
        const BATCH: usize = 256;

        let mut replayed = 0usize;
        loop {
            let rows = self.take(BATCH)?;
            if rows.is_empty() {
                return Ok(replayed);
            }
            for (id, payload) in rows {
                // Decoded one row at a time, not a batch at a time: a row this
                // build cannot read must not block the perfectly replayable
                // rows queued *ahead* of it.
                let record = decode(id, &payload)?;
                record.apply(conn, hmac_secret)?;
                self.conn
                    .execute("DELETE FROM spill WHERE id = ?1", rusqlite::params![id])
                    .map_err(|e| StorageError::QueryError(format!("spill delete failed: {}", e)))?;
                replayed += 1;
            }
        }
    }

    fn take(&self, limit: usize) -> StorageResult<Vec<(i64, Vec<u8>)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, payload FROM spill ORDER BY id ASC LIMIT ?1")
            .map_err(|e| StorageError::QueryError(format!("spill select failed: {}", e)))?;

        stmt.query_map(rusqlite::params![limit as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
        .map_err(|e| StorageError::QueryError(format!("spill select failed: {}", e)))
    }
}

/// Refused, not skipped: a row this build cannot decode (a downgrade after a
/// newer one wrote it) stays on disk for an operator to look at.
fn decode(id: i64, payload: &[u8]) -> StorageResult<SpillRecord> {
    serde_json::from_slice(payload).map_err(|e| {
        StorageError::QueryError(format!(
            "spill row {} is undecodable ({}); refusing to drop it",
            id, e
        ))
    })
}

/// `<main>.db` → `<main>_spill.db`, so the journal always travels with the
/// database it belongs to (including in tests, which use a temp path).
pub fn spill_path(main_db_path: &Path) -> PathBuf {
    let stem = main_db_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "fiber".to_string());
    main_db_path.with_file_name(format!("{stem}_spill.db"))
}

/// What to do with a message while a rewrite holds the write lock.
///
/// Exhaustive on purpose: there is no `_` arm, so a new [`StorageMessage`]
/// variant does not compile until somebody has decided which of these three it
/// is. The predecessor of this function returned
/// `Result<SpillRecord, StorageMessage>` with a catch-all `other => Err(other)`,
/// and two housekeeping variants fell through it into direct writes against a
/// database a `VACUUM` had locked — the failure this whole module exists to
/// prevent, reintroduced by a wildcard.
///
/// [`StorageMessage`]: super::thread::StorageMessage
pub enum Routed {
    /// Carries producer data nobody else holds. Append it to the journal; that
    /// commit *is* the acceptance point.
    Spill(SpillRecord),
    /// Housekeeping. It derives its work from rows that are already durable, so
    /// postponing it loses nothing and re-running it later is the same
    /// operation. The `&'static str` names the job for the log line.
    Postpone(&'static str),
    /// A query, a reply-carrying write, or a control message: served live.
    /// A reply channel cannot be replayed across a restart, and WAL readers are
    /// not blocked by a `VACUUM` anyway.
    Serve(super::thread::StorageMessage),
}

/// Classify one message for the duration of a rewrite. See [`Routed`].
pub fn route(msg: super::thread::StorageMessage) -> Routed {
    use super::thread::StorageMessage as M;
    match msg {
        M::WriteSensorReading {
            timestamp,
            sensor_line,
            temperature,
            is_connected,
            alarm_state,
        } => Routed::Spill(SpillRecord::SensorReading {
            timestamp,
            sensor_line,
            temperature,
            is_connected,
            alarm_state,
        }),
        M::WriteSensorReadingsBatch { readings } => {
            Routed::Spill(SpillRecord::SensorReadingsBatch { readings })
        }
        M::WriteSensorBuckets { rows } => Routed::Spill(SpillRecord::SensorBuckets { rows }),
        M::WriteAlarmEvent {
            timestamp,
            sensor_line,
            from_state,
            to_state,
            temperature,
        } => Routed::Spill(SpillRecord::AlarmEvent {
            timestamp,
            sensor_line,
            from_state,
            to_state,
            temperature,
        }),
        M::WriteNodeReading {
            dev_eui,
            provisioning_epoch,
            ts,
            received_at,
            message_id,
            event_type,
            payload_json,
        } => Routed::Spill(SpillRecord::NodeReading {
            dev_eui,
            provisioning_epoch,
            ts,
            received_at,
            message_id,
            event_type,
            payload_json,
        }),
        M::WriteBeaconReading {
            mac,
            ts,
            received_at,
            message_id,
            event_type,
            payload_json,
        } => Routed::Spill(SpillRecord::BeaconReading {
            mac,
            ts,
            received_at,
            message_id,
            event_type,
            payload_json,
        }),
        M::AppendNodeRemoved { dev_eui, ts } => {
            Routed::Spill(SpillRecord::NodeRemoved { dev_eui, ts })
        }
        M::AdvanceExportCursor {
            broker_id,
            stream,
            new_id,
        } => Routed::Spill(SpillRecord::AdvanceExportCursor {
            broker_id,
            stream,
            new_id,
        }),
        M::ResetExportCursor { broker_id, stream } => {
            Routed::Spill(SpillRecord::ResetExportCursor { broker_id, stream })
        }
        M::WriteAuditEvent {
            operation,
            table_name,
            details,
        } => Routed::Spill(SpillRecord::AuditEvent {
            operation,
            table_name,
            details,
        }),

        // Housekeeping: no payload of its own, and a sweep re-derives
        // everything it needs from the table on the next tick. Postponing is
        // strictly better than blocking on the rewrite's lock and then failing.
        M::EnforceNodeRetention { .. } => Routed::Postpone("node_retention"),

        // Served live. Two of these deserve a note.
        //
        // `BumpProvisioningEpoch` is a *write* that answers on a reply channel,
        // so it cannot be spilled: there would be nobody left to answer. During
        // a rewrite it waits out `busy_timeout` and may fail, and the LoRaWAN
        // provisioning caller retries — the request-scoped side of the line
        // `portal/src/durable.rs` draws between "spill, because no caller is
        // listening" and "retry, because one is".
        //
        // `FlushSync`'s contract ("everything queued before this is committed")
        // still holds while spilling: the journal is `synchronous = FULL`, so
        // every append ordered before this message is already fsynced.
        served @ (M::QueryLastReadings { .. }
        | M::GetStorageStats
        | M::BumpProvisioningEpoch { .. }
        | M::GetProvisioningEpoch { .. }
        | M::MaxBeaconReadingTs { .. }
        | M::GetRecentNodeUplinks { .. }
        | M::DevEuiLastEventWasRemovalOrAbsent { .. }
        | M::Flush
        | M::FlushSync { .. }
        | M::Shutdown) => Routed::Serve(served),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::storage::db::Database;
    use crate::libs::storage::thread::StorageMessage;

    fn temp_db() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::new(dir.path().join("fiber_medical.db"), 5).expect("open db");
        (dir, db)
    }

    fn reading(ts: i64) -> SpillRecord {
        SpillRecord::SensorReading {
            timestamp: ts,
            sensor_line: 0,
            temperature: 36.5,
            is_connected: true,
            alarm_state: AlarmState::Normal,
        }
    }

    #[test]
    fn the_journal_lives_beside_the_database_it_belongs_to() {
        assert_eq!(
            spill_path(Path::new("/data/fiber/fiber_medical.db")),
            Path::new("/data/fiber/fiber_medical_spill.db"),
        );
    }

    /// The crash case: a process dies mid-rewrite with accepted writes in the
    /// journal. Reopening replays every one of them and empties the journal.
    #[test]
    fn writes_left_by_a_crash_are_replayed_on_the_next_open() {
        let (dir, db) = temp_db();
        let mut conn = db.connect().expect("connect");

        {
            let journal = SpillJournal::open(db.path(), None).expect("open journal");
            for i in 0..25 {
                journal
                    .append(1_700_000_000 + i, &reading(1_700_000_000 + i))
                    .expect("append");
            }
            assert_eq!(journal.depth().expect("depth"), 25);
            // ... the process dies here, without replaying ...
        }

        assert_eq!(
            StorageWriter::get_sensor_reading_count(&conn).expect("count"),
            0,
            "the writes are on disk, but not yet in the main database"
        );

        let mut journal = SpillJournal::open(db.path(), None).expect("reopen journal");
        assert_eq!(
            journal.depth().expect("depth"),
            25,
            "an append that returned Ok must survive the crash"
        );

        let replayed = journal.drain(&mut conn, None).expect("drain");
        assert_eq!(replayed, 25);
        assert_eq!(
            StorageWriter::get_sensor_reading_count(&conn).expect("count"),
            25,
            "every accepted write must reach the database"
        );
        assert_eq!(journal.depth().expect("depth"), 0);

        // A second drain is a no-op — which is what makes a crash *during*
        // replay survivable too.
        assert_eq!(journal.drain(&mut conn, None).expect("drain"), 0);
        assert_eq!(
            StorageWriter::get_sensor_reading_count(&conn).expect("count"),
            25
        );
        drop(dir);
    }

    #[test]
    fn a_row_that_cannot_be_applied_stays_queued_rather_than_being_dropped() {
        let (dir, db) = temp_db();
        let mut conn = db.connect().expect("connect");
        let path = spill_path(db.path());

        {
            let journal = SpillJournal::open(db.path(), None).expect("open journal");
            journal.append(1, &reading(1)).expect("append");
        }

        // A record a future build wrote, read back after a downgrade.
        let raw = rusqlite::Connection::open(&path).expect("raw open");
        raw.execute(
            "INSERT INTO spill (ts, payload) VALUES (2, ?1)",
            rusqlite::params![br#"{"FromTheFuture":{}}"#.to_vec()],
        )
        .expect("insert");
        drop(raw);

        let mut journal = SpillJournal::open(db.path(), None).expect("reopen");
        let result = journal.drain(&mut conn, None);

        assert!(
            result.is_err(),
            "an unreplayable row must stop the drain, not be silently discarded"
        );
        assert_eq!(
            journal.depth().expect("depth"),
            1,
            "the decodable row ahead of it was applied and removed; the bad one stays"
        );
        assert_eq!(
            StorageWriter::get_sensor_reading_count(&conn).expect("count"),
            1
        );
        drop(dir);
    }

    /// Every message lands in exactly one of the three buckets.
    ///
    /// The two `Postpone` assertions are the regression: `EnforceRetention` and
    /// `EnforceNodeRetention` used to fall through the old `spillable`'s
    /// catch-all into a direct write against a database the rewrite had locked.
    #[test]
    fn every_message_is_routed_to_a_journal_a_postponement_or_the_live_connection() {
        assert!(matches!(
            route(StorageMessage::WriteSensorReading {
                timestamp: 1,
                sensor_line: 0,
                temperature: 1.0,
                is_connected: true,
                alarm_state: AlarmState::Normal,
            }),
            Routed::Spill(_)
        ));
        assert!(matches!(
            route(StorageMessage::WriteAuditEvent {
                operation: "TEST".into(),
                table_name: None,
                details: None,
            }),
            Routed::Spill(_)
        ));

        assert!(
            matches!(
                route(StorageMessage::EnforceNodeRetention {
                    retention_seconds: 1
                }),
                Routed::Postpone(_)
            ),
            "a retention sweep waits for the rewrite instead of fighting it for the lock"
        );
        assert!(
            matches!(route(StorageMessage::Shutdown), Routed::Serve(_)),
            "control messages are served live"
        );
        assert!(
            matches!(route(StorageMessage::GetStorageStats), Routed::Serve(_)),
            "queries are served live"
        );
    }

    /// A record round-trips through the journal unchanged — the replayed row
    /// must be the row that was accepted, not an approximation of it.
    #[test]
    fn a_record_round_trips_through_the_journal() {
        let (dir, db) = temp_db();
        let journal = SpillJournal::open(db.path(), None).expect("open journal");

        journal
            .append(
                7,
                &SpillRecord::NodeReading {
                    dev_eui: "aa11".into(),
                    provisioning_epoch: 3,
                    ts: 100,
                    received_at: 101,
                    message_id: "msg-1".into(),
                    event_type: "reading".into(),
                    payload_json: r#"{"t":1.5}"#.into(),
                },
            )
            .expect("append");

        let rows = journal.take(16).expect("take");
        assert_eq!(rows.len(), 1);
        match &decode(rows[0].0, &rows[0].1).expect("decode") {
            SpillRecord::NodeReading {
                dev_eui,
                provisioning_epoch,
                message_id,
                payload_json,
                ..
            } => {
                assert_eq!(dev_eui, "aa11");
                assert_eq!(*provisioning_epoch, 3);
                assert_eq!(message_id, "msg-1");
                assert_eq!(payload_json, r#"{"t":1.5}"#);
            }
            other => panic!("wrong variant: {other:?}"),
        }
        drop(dir);
    }
}
