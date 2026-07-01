//! Data retention policy enforcement
//! Implements FIFO auto-purge when 5GB capacity is reached
//! Medical device compliance: maintains full data integrity while respecting storage constraints

use rusqlite::Connection;

use crate::libs::storage::audit::AuditLogger;
use crate::libs::storage::db::Database;
use crate::libs::storage::error::{StorageError, StorageResult};

/// Retention policy for medical data storage
pub struct RetentionPolicy {
    /// Maximum database size in bytes (5GB)
    max_size_bytes: i64,

    /// Percentage threshold for triggering cleanup (e.g., 90 = cleanup at 90%)
    cleanup_threshold_percent: f32,

    /// Minimum age of records to delete (in seconds) - delete oldest first
    min_age_seconds: i64,
}

impl Default for RetentionPolicy {
    /// Default 5GB capacity policy. Convenience for sticker-stream sweeping
    /// where caller does not have a configured `max_size_gb` to pass in.
    fn default() -> Self {
        Self::new(5)
    }
}

/// Result of a `sweep_sticker_readings` pass.
#[derive(Debug, Clone, Default)]
pub struct StickerRetentionResult {
    /// Total rows deleted from `sticker_readings`.
    pub purged: i64,
    /// Of those deleted, how many had `id > min(cursor)` — i.e. were dropped
    /// before any destination had a chance to export them. Emits a WARN log.
    pub unexported_dropped: i64,
}

impl RetentionPolicy {
    /// Create a new retention policy (5GB with 90% cleanup threshold)
    pub fn new(max_size_gb: i32) -> Self {
        Self::with_max_bytes((max_size_gb.max(1) as i64) * 1024 * 1024 * 1024)
    }

    /// Create a retention policy from a precomputed byte cap. Use this
    /// when the operator config specifies sub-GB precision (via
    /// `StorageConfig::max_size_mb`); the legacy `new(i32)` constructor
    /// is fine for whole-GB defaults.
    pub fn with_max_bytes(max_size_bytes: i64) -> Self {
        Self {
            max_size_bytes,
            cleanup_threshold_percent: 90.0,
            min_age_seconds: 60, // Don't delete records less than 1 minute old
        }
    }

    /// Check if cleanup is needed based on current database size
    pub fn needs_cleanup(&self, db: &Database) -> StorageResult<bool> {
        let current_size = db.current_size_bytes()?;
        let threshold = (self.max_size_bytes as f32 * (self.cleanup_threshold_percent / 100.0))
            as i64;

        Ok(current_size > threshold)
    }

    /// Get the percentage of storage currently used
    pub fn get_usage_percent(&self, db: &Database) -> StorageResult<f32> {
        db.get_utilization_percent()
    }

    /// Enforce retention policy - delete oldest records to stay under limit
    /// Uses FIFO approach: deletes oldest sensor_readings first.
    ///
    /// Two correctness invariants live here, both of which were broken before:
    /// 1. We walk the time axis FORWARD (cutoff_lower advances by 3600s each
    ///    iteration). The earlier version reassigned `cutoff_timestamp = oldest_ts`
    ///    and then ran `DELETE WHERE timestamp < cutoff_timestamp`, so the next
    ///    iteration looked for rows older than what we'd just deleted — found
    ///    none, and exited after a single 1-hour batch no matter how much
    ///    we'd actually exceeded the cap.
    /// 2. `VACUUM` runs OUTSIDE the surrounding transaction. SQLite's
    ///    `VACUUM` is forbidden inside a transaction; running it there made
    ///    the whole DELETE rollback, so retention literally never freed any
    ///    space before this fix.
    pub fn enforce(
        &self,
        db: &Database,
        conn: &mut Connection,
    ) -> StorageResult<RetentionStats> {
        let start = std::time::Instant::now();
        let current_size = db.current_size_bytes()?;

        // If under max size, nothing to do
        if current_size <= self.max_size_bytes {
            return Ok(RetentionStats {
                deleted_count: 0,
                freed_bytes: 0,
                oldest_deleted_timestamp: None,
                duration_ms: 0,
            });
        }

        // Calculate how much we need to free (target 85% of max → 10% headroom)
        let target_size = (self.max_size_bytes as f32 * 0.85) as i64;
        let bytes_to_free = current_size - target_size;

        // Estimate row → bytes ratio so we can decide when "enough" is freed
        // without re-statting the DB file every iteration (the file size
        // doesn't actually shrink until VACUUM runs anyway). Falls back to a
        // conservative 128 B/row if the table is empty for some reason.
        let row_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sensor_readings", [], |r| r.get(0))
            .unwrap_or(0);
        let avg_bytes_per_row = if row_count > 0 {
            (current_size / row_count).max(1)
        } else {
            128
        };
        let target_rows_to_delete =
            ((bytes_to_free / avg_bytes_per_row) + 1).min(100_000);

        eprintln!(
            "RETENTION: DB size {}MB exceeds limit, freeing ~{}MB \
             (target: {}MB, ~{} rows)",
            current_size / (1024 * 1024),
            bytes_to_free / (1024 * 1024),
            target_size / (1024 * 1024),
            target_rows_to_delete,
        );

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let max_age_ts = now - self.min_age_seconds;

        let mut oldest_deleted_timestamp = None;
        let mut deleted_count = 0i64;
        let mut cutoff_lower = 0i64;

        // Wrap the deletes in a transaction so a mid-loop error rolls back
        // cleanly and the partition doesn't get a half-deleted window. VACUUM
        // is intentionally outside this transaction (see fn doc comment).
        let tx = conn
            .transaction()
            .map_err(|e| StorageError::DeleteError(format!("Failed to start transaction: {}", e)))?;

        while deleted_count < target_rows_to_delete {
            // Oldest remaining row strictly newer than the previous batch's
            // upper bound. Forward-walking the time axis avoids the dead loop
            // the previous implementation had.
            let oldest: Option<i64> = tx
                .query_row(
                    "SELECT MIN(timestamp) FROM sensor_readings
                     WHERE timestamp >= ?1 AND timestamp < ?2",
                    rusqlite::params![cutoff_lower, max_age_ts],
                    |row| row.get(0),
                )
                .map_err(|e| StorageError::DeleteError(
                    format!("Failed to query oldest timestamp: {}", e),
                ))?;

            let Some(oldest_ts) = oldest else { break; };
            let batch_cutoff = (oldest_ts + 3600).min(max_age_ts);

            let rows_affected = tx
                .execute(
                    "DELETE FROM sensor_readings WHERE timestamp >= ?1 AND timestamp < ?2",
                    rusqlite::params![oldest_ts, batch_cutoff],
                )
                .map_err(|e| StorageError::DeleteError(format!("Failed to delete records: {}", e)))?;

            deleted_count += rows_affected as i64;
            if oldest_deleted_timestamp.is_none() {
                oldest_deleted_timestamp = Some(oldest_ts);
            }
            cutoff_lower = batch_cutoff;

            eprintln!(
                "RETENTION: Deleted {} records in [{}, {})",
                rows_affected, oldest_ts, batch_cutoff,
            );

            // Safety net: a 1-hour window that yields zero deletes means we
            // walked past the data — break so we don't spin.
            if rows_affected == 0 {
                break;
            }
        }

        let duration_ms = start.elapsed().as_millis() as i64;

        tx.commit()
            .map_err(|e| StorageError::DeleteError(
                format!("Failed to commit retention cleanup: {}", e),
            ))?;

        // VACUUM after the transaction commits so SQLite can actually reclaim
        // free pages. SQLite forbids VACUUM inside an open transaction —
        // running it there silently aborted everything above on the previous
        // implementation. Soft-fail VACUUM since the DELETEs are already
        // committed; we'd rather log than lose the deletion progress.
        if deleted_count > 0 {
            if let Err(e) = conn.execute_batch("VACUUM") {
                eprintln!(
                    "RETENTION: VACUUM after delete failed: {} (deletes already committed)",
                    e,
                );
            }
        }

        // Log the retention cleanup in audit trail
        if deleted_count > 0 {
            if let Some(ts) = oldest_deleted_timestamp {
                let _ = AuditLogger::log_retention_cleanup(conn, deleted_count, ts, duration_ms);
            }

            eprintln!(
                "RETENTION: Cleanup complete - deleted {} records in {}ms",
                deleted_count, duration_ms
            );
        }

        Ok(RetentionStats {
            deleted_count,
            freed_bytes: deleted_count * avg_bytes_per_row,
            oldest_deleted_timestamp,
            duration_ms,
        })
    }

    /// Check if record is old enough to be eligible for deletion
    pub fn is_eligible_for_deletion(&self, record_timestamp: i64) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let record_age_seconds = now - record_timestamp;
        record_age_seconds > self.min_age_seconds
    }

    /// Sweep `sensor_readings` — delete raw rows older than
    /// `retention_seconds` whose minute already exists in
    /// `sensor_readings_minute`. The aggregator-presence guard means we
    /// never drop raw data before it has been folded into the long-term
    /// aggregate; if the aggregator lags, retention slides with it.
    ///
    /// Returns the number of rows deleted.
    pub fn sweep_raw_sensor_readings(
        conn: &mut Connection,
        retention_seconds: i64,
    ) -> StorageResult<i64> {
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
            - retention_seconds;

        let purged = conn
            .execute(
                "DELETE FROM sensor_readings
                 WHERE timestamp < ?1
                   AND EXISTS (
                       SELECT 1 FROM sensor_readings_minute m
                       WHERE m.minute_ts = (sensor_readings.timestamp / 60) * 60
                         AND m.sensor_line = sensor_readings.sensor_line
                   )",
                rusqlite::params![cutoff],
            )
            .map_err(|e| StorageError::DeleteError(
                format!("sweep_raw_sensor_readings: {}", e),
            ))? as i64;

        if purged > 0 {
            let _ = AuditLogger::log_operation(
                conn,
                "DELETE",
                Some("sensor_readings"),
                Some(purged),
                None,
            );
        }

        Ok(purged)
    }

    /// Sweep the `sticker_readings` table — delete rows older than
    /// `retention_seconds` (by `ts`). Reports how many of the deleted rows
    /// had `id > min(last_exported_id across all destinations for the
    /// "sticker" stream)`; those are data losses for at least one
    /// destination and are logged at WARN level.
    pub fn sweep_sticker_readings(
        &self,
        conn: &mut Connection,
        retention_seconds: i64,
    ) -> StorageResult<StickerRetentionResult> {
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
            - retention_seconds;

        // Lowest cursor across all destinations on the "sticker" stream.
        // If no cursor row exists yet (no destinations have ever exported),
        // treat as 0 so every soon-to-be-deleted row counts as un-exported.
        let min_cursor: i64 = conn
            .query_row(
                "SELECT COALESCE(MIN(last_exported_id), 0) FROM export_cursor
                 WHERE stream = 'sticker'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        let unexported_dropped: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sticker_readings WHERE ts < ? AND id > ?",
                rusqlite::params![cutoff, min_cursor],
                |r| r.get(0),
            )
            .unwrap_or(0);

        let purged = conn
            .execute(
                "DELETE FROM sticker_readings WHERE ts < ?",
                rusqlite::params![cutoff],
            )
            .map_err(|e| StorageError::DeleteError(format!("sweep_sticker_readings: {}", e)))?
            as i64;

        if unexported_dropped > 0 {
            eprintln!(
                "WARN [retention] dropped {} un-exported sticker rows (min_cursor={}, cutoff={})",
                unexported_dropped, min_cursor, cutoff,
            );
        }

        if purged > 0 {
            let _ = AuditLogger::log_operation(
                conn,
                "DELETE",
                Some("sticker_readings"),
                Some(purged),
                None,
            );
        }

        Ok(StickerRetentionResult {
            purged,
            unexported_dropped,
        })
    }

    /// Sweep the `eye_readings` table — delete rows older than
    /// `retention_seconds` (by `ts`). Reports how many of the deleted rows
    /// had `id > min(last_exported_id across all destinations for the "eye"
    /// stream)`; those are data losses for at least one destination and are
    /// logged at WARN level. Mirrors `sweep_sticker_readings`.
    pub fn sweep_eye_readings(
        &self,
        conn: &mut Connection,
        retention_seconds: i64,
    ) -> StorageResult<EyeRetentionResult> {
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
            - retention_seconds;

        let min_cursor: i64 = conn
            .query_row(
                "SELECT COALESCE(MIN(last_exported_id), 0) FROM export_cursor
                 WHERE stream = 'eye'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        let unexported_dropped: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM eye_readings WHERE ts < ? AND id > ?",
                rusqlite::params![cutoff, min_cursor],
                |r| r.get(0),
            )
            .unwrap_or(0);

        let purged = conn
            .execute(
                "DELETE FROM eye_readings WHERE ts < ?",
                rusqlite::params![cutoff],
            )
            .map_err(|e| StorageError::DeleteError(format!("sweep_eye_readings: {}", e)))?
            as i64;

        if unexported_dropped > 0 {
            eprintln!(
                "WARN [retention] dropped {} un-exported eye_readings rows (min_cursor={}, cutoff={})",
                unexported_dropped, min_cursor, cutoff,
            );
        }

        if purged > 0 {
            let _ = AuditLogger::log_operation(
                conn,
                "DELETE",
                Some("eye_readings"),
                Some(purged),
                None,
            );
        }

        Ok(EyeRetentionResult {
            purged,
            unexported_dropped,
        })
    }
}

/// Result of a `sweep_eye_readings` pass.
#[derive(Debug, Clone, Default)]
pub struct EyeRetentionResult {
    /// Total rows deleted from `eye_readings`.
    pub purged: i64,
    /// Of those deleted, how many had `id > min(cursor)` — i.e. were dropped
    /// before any destination had a chance to export them. Emits a WARN log.
    pub unexported_dropped: i64,
}

/// Statistics from a retention cleanup operation
#[derive(Debug, Clone)]
pub struct RetentionStats {
    /// Number of records deleted
    pub deleted_count: i64,

    /// Approximate bytes freed
    pub freed_bytes: i64,

    /// Oldest timestamp of deleted records
    pub oldest_deleted_timestamp: Option<i64>,

    /// Duration of cleanup operation in milliseconds
    pub duration_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::storage::db::Database;

    #[test]
    fn test_retention_policy_creation() {
        let policy = RetentionPolicy::new(5);
        assert_eq!(policy.max_size_bytes, 5 * 1024 * 1024 * 1024);
        assert_eq!(policy.cleanup_threshold_percent, 90.0);
    }

    #[test]
    fn test_eligibility_for_deletion() {
        let policy = RetentionPolicy::new(5);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // Very old record should be eligible
        assert!(policy.is_eligible_for_deletion(now - 10000));

        // Very new record should not be eligible
        assert!(!policy.is_eligible_for_deletion(now));
    }

    #[test]
    fn test_needs_cleanup_initially_false() {
        let db = Database::new("/tmp/test_retention.db", 5).expect("Failed to create test DB");
        let policy = RetentionPolicy::new(5);

        let needs = policy.needs_cleanup(&db).expect("Failed to check cleanup need");
        assert!(!needs, "Empty database should not need cleanup");

        let _ = std::fs::remove_file("/tmp/test_retention.db");
    }

    #[test]
    fn sticker_retention_purges_old_rows_and_warns_when_unexported() {
        use crate::libs::storage::writer::StorageWriter;
        use std::time::{SystemTime, UNIX_EPOCH};

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 1).unwrap();
        let mut conn = db.connect().unwrap();

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        let day = 86400i64;

        // Old row (35 days ago)
        StorageWriter::write_sticker_reading(
            &mut conn,
            "abc",
            1,
            now - 35 * day,
            now - 35 * day,
            "abc-old",
            "uplink",
            "{}",
        )
        .unwrap();
        // Fresh row (1 day ago)
        StorageWriter::write_sticker_reading(
            &mut conn,
            "abc",
            1,
            now - day,
            now - day,
            "abc-fresh",
            "uplink",
            "{}",
        )
        .unwrap();

        // Cursor is at 0 (no destinations have exported anything) → both
        // un-exported. Sweep should drop the old row and warn.
        let policy = RetentionPolicy::default();
        let dropped = policy.sweep_sticker_readings(&mut conn, 30 * day).unwrap();
        assert_eq!(dropped.purged, 1);
        assert_eq!(dropped.unexported_dropped, 1);

        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM sticker_readings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[test]
    fn eye_retention_purges_old_rows_and_warns_when_unexported() {
        use crate::libs::storage::writer::StorageWriter;
        use std::time::{SystemTime, UNIX_EPOCH};

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 1).unwrap();
        let mut conn = db.connect().unwrap();

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        let day = 86400i64;

        // Old row (35 days ago)
        StorageWriter::write_eye_reading(
            &mut conn,
            "AA:BB:CC:DD:EE:FF",
            now - 35 * day,
            now - 35 * day,
            "mac-old",
            "advertising",
            "{}",
        )
        .unwrap();
        // Fresh row (1 day ago)
        StorageWriter::write_eye_reading(
            &mut conn,
            "AA:BB:CC:DD:EE:FF",
            now - day,
            now - day,
            "mac-fresh",
            "advertising",
            "{}",
        )
        .unwrap();

        // Cursor is at 0 (no destinations have exported anything) → both
        // un-exported. Sweep should drop the old row and warn.
        let policy = RetentionPolicy::default();
        let dropped = policy.sweep_eye_readings(&mut conn, 30 * day).unwrap();
        assert_eq!(dropped.purged, 1);
        assert_eq!(dropped.unexported_dropped, 1);

        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM eye_readings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[test]
    fn enforce_walks_forward_and_completes_without_rollback() {
        // Regression: the previous implementation (a) ran VACUUM inside an
        // open transaction (SQLite rejects this) and (b) advanced the cutoff
        // by reassigning it to the oldest timestamp found, so the next loop
        // iteration's WHERE clause matched nothing. The net effect was a
        // forced rollback that deleted zero rows even when far over capacity.
        use crate::libs::alarms::AlarmState;
        use crate::libs::storage::models::SensorReading;
        use crate::libs::storage::writer::StorageWriter;
        use std::time::{SystemTime, UNIX_EPOCH};

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 1).unwrap();
        let mut conn = db.connect().unwrap();

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        // Three rows spread over a 4-hour window, all well past min_age.
        for offset_h in [4, 3, 2] {
            let ts = now - offset_h * 3600 - 120;
            let r = SensorReading::new(ts, 0, 20.0, true, AlarmState::Normal);
            StorageWriter::write_sensor_reading(&conn, &r, None).unwrap();
        }

        let mut policy = RetentionPolicy::new(1);
        // Force enforce() into the deletion path by pinning the cap below
        // current size, regardless of the actual file size.
        policy.max_size_bytes = 1;
        let stats = policy.enforce(&db, &mut conn).unwrap();

        // We should have deleted more than a single 1-hour batch — the old
        // bug capped this at one row's worth.
        assert!(
            stats.deleted_count >= 2,
            "enforce should delete multiple batches, deleted={}",
            stats.deleted_count,
        );

        // And the transaction must commit (the rows really go away — the
        // earlier VACUUM-in-transaction bug rolled back the deletes).
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM sensor_readings", [], |r| r.get(0))
            .unwrap();
        assert!(remaining < 3, "expected rows to be persisted-deleted, found {}", remaining);
    }

    #[test]
    fn raw_sweep_only_deletes_aggregated_rows() {
        use crate::libs::alarms::AlarmState;
        use crate::libs::storage::aggregator::aggregate_closed_minutes;
        use crate::libs::storage::models::SensorReading;
        use crate::libs::storage::writer::StorageWriter;
        use std::time::{SystemTime, UNIX_EPOCH};

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 1).unwrap();
        let mut conn = db.connect().unwrap();

        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        let day = 86400i64;

        // Two readings 35 days ago: should be aggregatable + sweepable.
        for offset in [0, 30] {
            let r = SensorReading::new(
                now - 35 * day + offset,
                0,
                20.0,
                true,
                AlarmState::Normal,
            );
            StorageWriter::write_sensor_reading(&conn, &r, None).unwrap();
        }
        // One reading 35 days ago for a different sensor that we WON'T aggregate.
        // It must survive the sweep because no aggregate covers it.
        let r = SensorReading::new(now - 35 * day, 1, 21.0, true, AlarmState::Normal);
        StorageWriter::write_sensor_reading(&conn, &r, None).unwrap();

        // Aggregate only the sensor_line=0 readings by deleting any sensor_line=1
        // rows from the aggregate after running the aggregator.
        aggregate_closed_minutes(&mut conn, now, None).unwrap();
        conn.execute(
            "DELETE FROM sensor_readings_minute WHERE sensor_line = 1",
            [],
        )
        .unwrap();

        let purged =
            RetentionPolicy::sweep_raw_sensor_readings(&mut conn, 30 * day).unwrap();
        assert_eq!(purged, 2, "only the aggregated sensor_line=0 rows should drop");

        let surviving_line_1: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sensor_readings WHERE sensor_line = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            surviving_line_1, 1,
            "unaggregated raw row must not be deleted even if past retention"
        );
    }
}
