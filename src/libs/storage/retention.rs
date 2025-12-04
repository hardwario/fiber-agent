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

impl RetentionPolicy {
    /// Create a new retention policy (5GB with 90% cleanup threshold)
    pub fn new(max_size_gb: i32) -> Self {
        Self {
            max_size_bytes: (max_size_gb as i64) * 1024 * 1024 * 1024,
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
    /// Uses FIFO approach: deletes oldest sensor_readings first
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

        // Calculate how much we need to free (10% margin)
        let target_size = (self.max_size_bytes as f32 * 0.85) as i64;
        let bytes_to_free = current_size - target_size;

        eprintln!(
            "RETENTION: DB size {}MB exceeds limit, freeing {}MB (target: {}MB)",
            current_size / (1024 * 1024),
            bytes_to_free / (1024 * 1024),
            target_size / (1024 * 1024)
        );

        // Get the oldest timestamp we need to delete up to
        // We need to estimate how many records to delete
        let mut oldest_deleted_timestamp = None;
        let mut deleted_count = 0i64;

        // Start a transaction for deletion
        let tx = conn
            .transaction()
            .map_err(|e| StorageError::DeleteError(format!("Failed to start transaction: {}", e)))?;

        // Delete oldest sensor_readings in batches (most data is here)
        // Strategy: delete records older than N seconds until we've freed enough space
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let mut cutoff_timestamp = now - self.min_age_seconds;

        loop {
            // Get oldest timestamp in the database
            let oldest: Option<i64> = tx
                .query_row(
                    "SELECT MIN(timestamp) FROM sensor_readings WHERE timestamp < ?",
                    rusqlite::params![cutoff_timestamp],
                    |row| row.get(0),
                )
                .map_err(|e| StorageError::DeleteError(
                    format!("Failed to query oldest timestamp: {}", e),
                ))?;

            if oldest.is_none() {
                // No more old records to delete
                break;
            }

            let oldest_ts = oldest.unwrap();
            cutoff_timestamp = oldest_ts;

            // Delete a batch of records (delete one hour at a time to be gradual)
            let batch_cutoff = oldest_ts + 3600; // 1 hour of records

            let rows_affected = tx
                .execute(
                    "DELETE FROM sensor_readings WHERE timestamp >= ? AND timestamp < ?",
                    rusqlite::params![oldest_ts, batch_cutoff],
                )
                .map_err(|e| StorageError::DeleteError(format!("Failed to delete records: {}", e)))?;

            deleted_count += rows_affected as i64;
            oldest_deleted_timestamp = Some(oldest_ts);

            eprintln!(
                "RETENTION: Deleted {} records from {} (oldest: {})",
                rows_affected, oldest_ts, oldest_ts
            );

            // Check if we've freed enough space
            if bytes_to_free < 0 {
                break; // We've deleted enough
            }

            // Check if we still need to delete more
            if deleted_count > 100000 {
                // Prevent deleting too much in one operation
                break;
            }
        }

        // Vacuum to reclaim space (compacts the database)
        tx.execute("VACUUM", [])
            .map_err(|e| StorageError::DeleteError(
                format!("Failed to vacuum database: {}", e),
            ))?;

        let duration_ms = start.elapsed().as_millis() as i64;

        // Commit transaction
        tx.commit()
            .map_err(|e| StorageError::DeleteError(
                format!("Failed to commit retention cleanup: {}", e),
            ))?;

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
            freed_bytes: bytes_to_free,
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
}
