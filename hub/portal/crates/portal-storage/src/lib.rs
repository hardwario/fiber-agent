//! Storage layer: rusqlite+SQLCipher repositories, refinery migrations (M1+).
//!
//! [`Storage`] owns a single `rusqlite::Connection` and is meant to live on
//! one dedicated (blocking) thread — `rusqlite::Connection` is `!Sync`, so
//! this crate exposes plain synchronous methods and leaves the async/mpsc
//! worker wiring to the caller (a later work package).

mod alarm_layout_repo;
mod alarm_repo;
mod audit_repo;
mod backfill_repo;
mod beacon_history_repo;
mod calibration_repo;
mod cluster_repo;
mod delivery_repo;
mod device_meta_repo;
mod error;
mod eye_repo;
mod group_repo;
mod history_repo;
mod keys_repo;
mod ledger_repo;
mod minute_repo;
mod models;
mod node_deadline_repo;
mod node_repo;
mod notify_repo;
mod pending_repo;
mod raw_repo;
mod retention_repo;
mod retry;
mod sensor_event_repo;
mod sensor_repo;
mod session_repo;
mod settings_repo;
mod spill;
mod summary_repo;
mod sync_repo;
mod threshold_repo;
mod user_repo;

use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

/// A `LIKE` pattern matching `value` anywhere, with the operator's own `%`,
/// `_` and `~` escaped so they are searched for literally.
///
/// `~` as the ESCAPE character rather than a backslash: the clause is written
/// inline in a Rust string literal, and `'\\'` there needs four backslashes
/// to reach SQLite as one — which is how the first version of this was wrong.
/// Every caller must pair the pattern with `ESCAPE '~'`.
pub(crate) fn like_contains(value: &str) -> String {
    let escaped = value
        .replace('~', "~~")
        .replace('%', "~%")
        .replace('_', "~_");
    format!("%{escaped}%")
}

pub use alarm_repo::{AlarmCounts, AlarmSeverityChange};
pub use audit_repo::AuditCounts;
pub use beacon_history_repo::DRAIN_JOB_TRIGGER;
pub use cluster_repo::{
    ClusterClient, ClusterInsert, NewClusterClient, CLUSTER_STATE_COMPLETE, CLUSTER_STATE_FAILED,
    CLUSTER_STATE_PENDING,
};
pub use error::{Result, StorageError};
pub use group_repo::MoveDirection;
pub use ledger_repo::DEFAULT_RETENTION_DAYS;
pub use models::{
    AlarmEvent, AlarmListFilter, AlarmNote, AlarmSort, AuditEntry, AuditFilter, AuditSort,
    BackfillJob, BeaconHistoryJob, BeaconHistoryReport, DeviceCalibration, DeviceIdentityRow,
    DeviceMeta, DeviceReportConfig, EyeReading, EyeWindowRange, FieldSummary, FieldThreshold,
    FieldThresholdChange, HourlyBucket, MaintenanceState, MinuteReading, NewAlarmEvent,
    NewAuditEntry, NewBackfillJob, NewBeaconHistoryJob, NewDeviceCalibration, NewDeviceIdentity,
    NewDeviceMeta, NewFieldThreshold, NewPendingConfig, NewReportHistory, NewSensorEvent,
    NewSensorGroupMember, NewServiceDeviceKey, NewUser, NewUserDeviceKey, NodeBand,
    NodeCounterReading, NodeEvent, NodeFieldReading, NodeRawMessage, NodeThresholdDelivery,
    NotifyGlobalConfig, PendingConfigRow, ProbeIdentity, RawReading, ReportHistoryEntry,
    ReportHistoryFilter, SensorEvent, SensorGroup, SensorGroupMember, ServiceDeviceKeyRow,
    SessionRow, SystemAlertConfig, User, UserDeviceKeyRow, DELIVERY_STATE_CONFIRMED,
    DELIVERY_STATE_DIFFERS, DELIVERY_STATE_PENDING, DELIVERY_STATE_UNDELIVERED,
    DELIVERY_STATE_UNKNOWN, HISTORY_SOURCE_REPORT, HISTORY_SOURCE_SYSTEM_ALERT,
    HISTORY_STATUS_FAILED, HISTORY_STATUS_SENT, JOB_CANCELLED, JOB_DONE, JOB_FAILED, JOB_PENDING,
    JOB_RECEIVING, JOB_REQUESTED, PENDING_KIND_CONFIG, PENDING_KIND_THRESHOLD, PENDING_STATES,
    PENDING_STATE_CONFIRMED, PENDING_STATE_ERROR, PENDING_STATE_PENDING, PENDING_STATE_SENDING,
    PENDING_STATE_UNDELIVERED, SOURCE_LIVE, SOURCE_REPLAY, THRESHOLD_SCOPE_EYE,
    THRESHOLD_SCOPE_HUB, THRESHOLD_SCOPE_NODE,
};
pub use node_deadline_repo::NodeConfigDeadline;
pub use node_repo::raw_message_port;
pub use pending_repo::{HeldRevert, HubLanding};
pub use retention_repo::{PageStats, RetentionPolicy, RetentionReport};
pub use retry::with_retry;
pub use sensor_repo::{DisabledSensor, SensorPurge};
pub use spill::{Landed, SpillGate, SpillJournal, SpillKind, SpillLease, SpillPhase, SpilledRow};
pub use summary_repo::{EyeField, PeakSource};
pub use sync_repo::{ConfigGeneration, ConfigSyncResult};

mod embedded {
    refinery::embed_migrations!("../../migrations");
}

/// How long a blocked writer waits before giving up.
///
/// SQLite's default is 0 — an immediate `SQLITE_BUSY`. With eight connections
/// on one file that meant any writer losing a race was simply dropped by its
/// caller (`persist.rs` logs "persistence write failed" and moves on), and a
/// full `VACUUM` holds the write lock for its entire duration — so routine
/// maintenance was discarding alarm rows and their notifications.
///
/// Waiting is unambiguously better than dropping here: the ingest channel is
/// unbounded, so a stalled writer queues in memory rather than blocking the
/// MQTT event loop.
const BUSY_TIMEOUT: Duration = Duration::from_secs(30);

/// Owns the SQLite/SQLCipher connection and exposes repository methods.
pub struct Storage {
    conn: Connection,
}

impl Storage {
    /// Open (creating if needed) the database at `path`, apply `PRAGMA key`
    /// when `key` is `Some` (SQLCipher), enable WAL + foreign keys, and run
    /// pending migrations.
    ///
    /// `key = None` is dev mode only (plain, unencrypted sqlite) and is
    /// logged as a warning.
    pub fn open(path: &Path, key: Option<&str>) -> Result<Self> {
        Self::open_inner(path, key, None)
    }

    /// **Test fixture only**: open with the migration set stopped after
    /// `version`, so a test can build the *older* schema a later migration
    /// will actually land on.
    ///
    /// This exists for `tests/pending_config.rs`'s
    /// `migrations_apply_over_a_populated_database`, which otherwise asserts
    /// nothing: opening a fresh file twice runs the whole set the first time
    /// and re-verifies it the second, so the migration under test never sees
    /// a database it did not itself create. The real hazard — V13 landing on
    /// a file that has been running since V1 — needs a fixture built at an
    /// earlier version, which is what refinery's `Target::Version` is for.
    ///
    /// Not for production code: a portal that stopped short of the newest
    /// migration would be running against a schema its queries do not match.
    #[doc(hidden)]
    pub fn open_at_version(path: &Path, key: Option<&str>, version: u32) -> Result<Self> {
        Self::open_inner(path, key, Some(version))
    }

    fn open_inner(path: &Path, key: Option<&str>, target_version: Option<u32>) -> Result<Self> {
        let mut conn = Connection::open(path)?;

        match key {
            Some(key) => conn.pragma_update(None, "key", key)?,
            None => tracing::warn!(
                path = %path.display(),
                "opening portal-storage without an encryption key (dev mode, plain sqlite)"
            ),
        }

        // `auto_vacuum` can only be *changed* while the database is still
        // empty — on a populated file SQLite silently keeps the existing
        // setting, which is exactly what `Storage::auto_vacuum_mode` reports so
        // the operator can be told a one-time conversion is needed. It
        // therefore has to run before the migrations create the first table,
        // and after `PRAGMA key` because SQLCipher requires that to come first.
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;

        let runner = embedded::migrations::runner();
        match target_version {
            None => runner.run(&mut conn)?,
            Some(version) => runner
                .set_target(refinery::Target::Version(version))
                .run(&mut conn)?,
        };

        // Decision #21's safeguard: every stored hostname in its one spelling, once per process
        // (the first connection opened does it for the rest).
        if target_version.is_none() {
            static NORMALIZED: std::sync::Once = std::sync::Once::new();
            let mut result = Ok(0);
            NORMALIZED.call_once(|| result = lowercase_hostnames(&mut conn));
            match result {
                Ok(0) => {}
                Ok(n) => tracing::info!(rows = n, "hostnames lowercased (decision #21)"),
                Err(e) => tracing::warn!(error = %e, "could not lowercase stored hostnames"),
            }
        }

        Ok(Self { conn })
    }

    /// Route SQLite's temporary files — the `VACUUM` copy above all — to disk
    /// instead of memory.
    ///
    /// `bundled-sqlcipher` compiles with `-DSQLITE_TEMP_STORE=2`, which makes a
    /// temp database default to **memory**; a full `VACUUM` would then allocate
    /// roughly the whole database in RAM, and this database is telemetry-bearing
    /// and uncapped. Only the retention connection calls this. The other seven
    /// keep in-memory temp storage, which is what keeps the history and chart
    /// queries fast.
    ///
    /// Where the file actually lands is `SQLITE_TMPDIR`, set by the binary's
    /// `main()` before the Landlock ruleset is built — inside the data
    /// directory, so the existing ruleset already covers it.
    pub fn set_temp_store_file(&self) -> Result<()> {
        self.conn.pragma_update(None, "temp_store", "FILE")?;
        Ok(())
    }

    /// Override [`BUSY_TIMEOUT`] on this connection.
    ///
    /// Exists for the guard tests: the only way to observe what a writer does
    /// when a lock outlasts its patience is to make it impatient, because a
    /// test database vacuums far too quickly to out-wait 30 seconds. Not used
    /// in production — every connection wants the full timeout.
    pub fn set_busy_timeout(&self, timeout: Duration) -> Result<()> {
        self.conn.busy_timeout(timeout)?;
        Ok(())
    }
}

/// Decision #21: a Hub's hostname has one spelling, lowercase. Lowercase the `hostname`
/// column of every table that has one — found from the schema, so a table a later migration
/// adds is covered without a change here. Idempotent: a database already in one spelling
/// touches no row. On a clash (a unique key that already holds the lowercase spelling) the
/// row under the old spelling replaces it: that is the spelling the portal has been storing
/// and reading under (#94), the lowercase one a stray. Returns the rows changed.
pub fn lowercase_hostnames(conn: &mut Connection) -> Result<usize> {
    let tables: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
             AND name <> 'refinery_schema_history'",
        )?;
        let names = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        names
    };
    let tx = conn.transaction()?;
    let mut changed = 0;
    for table in tables {
        let has_hostname = {
            let mut stmt = tx.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
            let cols = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            cols.iter().any(|c| c == "hostname")
        };
        if has_hostname {
            changed += tx.execute(
                &format!(
                    "UPDATE OR REPLACE \"{table}\" SET hostname = lower(hostname) \
                     WHERE hostname <> lower(hostname)"
                ),
                [],
            )?;
        }
    }
    tx.commit()?;
    Ok(changed)
}

#[cfg(test)]
mod hostname_tests {
    use super::*;

    fn rows(conn: &Connection, sql: &str) -> Vec<(String, i64)> {
        let mut stmt = conn.prepare(sql).unwrap();
        let out = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        out
    }

    #[test]
    fn every_hostname_column_is_lowercased_and_the_old_spelling_wins_a_clash() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE keyed (hostname TEXT PRIMARY KEY, v INTEGER);
             CREATE TABLE plain (hostname TEXT, v INTEGER);
             CREATE TABLE other (name TEXT, v INTEGER);
             INSERT INTO keyed VALUES ('FIBER-4E970F4A', 1), ('fiber-4e970f4a', 2), ('hub-1', 3);
             INSERT INTO plain VALUES ('FIBER-4E970F4A', 1), ('fiber-4e970f4a', 2);
             INSERT INTO other VALUES ('FIBER-4E970F4A', 1);",
        )
        .unwrap();

        assert_eq!(lowercase_hostnames(&mut conn).unwrap(), 2);

        assert_eq!(
            rows(&conn, "SELECT hostname, v FROM keyed ORDER BY hostname"),
            vec![("fiber-4e970f4a".into(), 1), ("hub-1".into(), 3)],
            "the row under the old spelling replaces the stray lowercase one"
        );
        assert_eq!(
            rows(&conn, "SELECT hostname, v FROM plain ORDER BY v"),
            vec![("fiber-4e970f4a".into(), 1), ("fiber-4e970f4a".into(), 2)]
        );
        assert_eq!(
            rows(&conn, "SELECT name, v FROM other"),
            vec![("FIBER-4E970F4A".into(), 1)],
            "a column that is not `hostname` is left alone"
        );
        assert_eq!(lowercase_hostnames(&mut conn).unwrap(), 0, "idempotent");
    }
}
