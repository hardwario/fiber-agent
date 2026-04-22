//! Audit trail logging for EU MDR 2017/745 compliance
//! Tracks all database operations, errors, and system events for regulatory compliance
//! All entries are SHA-256 hash-chained for tamper detection

use rusqlite::Connection;

use crate::libs::storage::error::{StorageError, StorageResult};
use crate::libs::storage::integrity;
use crate::libs::storage::models::AuditLogEntry;

/// Audit logger for tracking all storage operations
pub struct AuditLogger;

impl AuditLogger {
    /// Log a successful operation to the audit trail
    /// Computes SHA-256 hash-chain linking this entry to the previous one
    pub fn log_operation(
        conn: &Connection,
        operation: &str,
        table_name: Option<&str>,
        record_count: Option<i64>,
        duration_ms: Option<i64>,
    ) -> StorageResult<()> {
        let thread_id = std::thread::current()
            .name()
            .unwrap_or("unknown")
            .to_string();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let previous_hash = integrity::get_latest_audit_hash(conn)?;
        let record_hash = integrity::compute_audit_record_hash(
            now,
            operation,
            table_name,
            record_count,
            duration_ms,
            &thread_id,
            None,  // details
            None,  // error_msg
            previous_hash.as_deref(),
        );

        conn.execute(
            "INSERT INTO audit_log (timestamp, operation, table_name, record_count, duration_ms, thread_id, details, error_msg, record_hash, previous_hash)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                now,
                operation,
                table_name,
                record_count,
                duration_ms,
                thread_id,
                None::<String>,  // details
                None::<String>,  // error_msg (None = success)
                record_hash,
                previous_hash,
            ],
        )
        .map_err(|e| StorageError::AuditError(format!("Failed to log operation: {}", e)))?;

        Ok(())
    }

    /// Log a failed operation to the audit trail (with error details)
    /// Computes SHA-256 hash-chain linking this entry to the previous one
    pub fn log_error(
        conn: &Connection,
        operation: &str,
        table_name: Option<&str>,
        error_msg: &str,
        duration_ms: Option<i64>,
    ) -> StorageResult<()> {
        let thread_id = std::thread::current()
            .name()
            .unwrap_or("unknown")
            .to_string();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let previous_hash = integrity::get_latest_audit_hash(conn)?;
        let record_hash = integrity::compute_audit_record_hash(
            now,
            operation,
            table_name,
            None,  // record_count
            duration_ms,
            &thread_id,
            None,  // details
            Some(error_msg),
            previous_hash.as_deref(),
        );

        conn.execute(
            "INSERT INTO audit_log (timestamp, operation, table_name, duration_ms, thread_id, error_msg, record_hash, previous_hash)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![now, operation, table_name, duration_ms, thread_id, error_msg, record_hash, previous_hash],
        )
        .map_err(|e| StorageError::AuditError(format!("Failed to log error: {}", e)))?;

        Ok(())
    }

    /// Log a schema change (critical for MDR compliance)
    pub fn log_schema_change(
        conn: &Connection,
        change_description: &str,
    ) -> StorageResult<()> {
        Self::log_operation(conn, "SCHEMA_CHANGE", None, None, None).map_err(|e| {
            eprintln!("CRITICAL: Failed to log schema change: {}. Change: {}", e, change_description);
            e
        })?;

        eprintln!("AUDIT: Schema change: {}", change_description);
        Ok(())
    }

    /// Log retention policy enforcement (deletion of old data)
    /// Computes SHA-256 hash-chain linking this entry to the previous one
    pub fn log_retention_cleanup(
        conn: &Connection,
        deleted_count: i64,
        oldest_deleted_timestamp: i64,
        duration_ms: i64,
    ) -> StorageResult<()> {
        let details = format!(
            r#"{{"oldest_deleted_ts": {}, "reason": "FIFO_retention_policy"}}"#,
            oldest_deleted_timestamp
        );

        let thread_id = std::thread::current()
            .name()
            .unwrap_or("unknown")
            .to_string();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let previous_hash = integrity::get_latest_audit_hash(conn)?;
        let record_hash = integrity::compute_audit_record_hash(
            now,
            "DELETE",
            Some("sensor_readings"),
            Some(deleted_count),
            Some(duration_ms),
            &thread_id,
            Some(&details),
            None,  // error_msg
            previous_hash.as_deref(),
        );

        conn.execute(
            "INSERT INTO audit_log (timestamp, operation, table_name, record_count, duration_ms, thread_id, details, record_hash, previous_hash)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                now,
                "DELETE",
                "sensor_readings",
                deleted_count,
                duration_ms,
                thread_id,
                details,
                record_hash,
                previous_hash,
            ],
        )
        .map_err(|e| StorageError::AuditError(
            format!("Failed to log retention cleanup: {}", e),
        ))?;

        Ok(())
    }

    /// Get audit log entries for a specific time range
    pub fn query_audit_logs(
        conn: &Connection,
        from_timestamp: i64,
        to_timestamp: i64,
        limit: i64,
    ) -> StorageResult<Vec<AuditLogEntry>> {
        let mut stmt = conn
            .prepare(
                "SELECT id, timestamp, operation, table_name, record_count, duration_ms, thread_id, details, error_msg, record_hash, previous_hash
                 FROM audit_log
                 WHERE timestamp >= ? AND timestamp <= ?
                 ORDER BY timestamp DESC
                 LIMIT ?",
            )
            .map_err(|e| StorageError::QueryError(format!("Failed to prepare query: {}", e)))?;

        let entries = stmt
            .query_map(rusqlite::params![from_timestamp, to_timestamp, limit], |row| {
                Ok(AuditLogEntry {
                    id: row.get(0)?,
                    timestamp: row.get(1)?,
                    operation: row.get(2)?,
                    table_name: row.get(3)?,
                    record_count: row.get(4)?,
                    duration_ms: row.get(5)?,
                    thread_id: row.get(6)?,
                    details: row.get(7)?,
                    error_msg: row.get(8)?,
                    record_hash: row.get(9)?,
                    previous_hash: row.get(10)?,
                })
            })
            .map_err(|e| StorageError::QueryError(format!("Failed to query logs: {}", e)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::QueryError(format!("Failed to map logs: {}", e)))?;

        Ok(entries)
    }

    /// Get all error entries from audit log
    pub fn query_errors(
        conn: &Connection,
        from_timestamp: i64,
        to_timestamp: i64,
        limit: i64,
    ) -> StorageResult<Vec<AuditLogEntry>> {
        let mut stmt = conn
            .prepare(
                "SELECT id, timestamp, operation, table_name, record_count, duration_ms, thread_id, details, error_msg, record_hash, previous_hash
                 FROM audit_log
                 WHERE timestamp >= ? AND timestamp <= ? AND error_msg IS NOT NULL
                 ORDER BY timestamp DESC
                 LIMIT ?",
            )
            .map_err(|e| StorageError::QueryError(format!("Failed to prepare query: {}", e)))?;

        let entries = stmt
            .query_map(rusqlite::params![from_timestamp, to_timestamp, limit], |row| {
                Ok(AuditLogEntry {
                    id: row.get(0)?,
                    timestamp: row.get(1)?,
                    operation: row.get(2)?,
                    table_name: row.get(3)?,
                    record_count: row.get(4)?,
                    duration_ms: row.get(5)?,
                    thread_id: row.get(6)?,
                    details: row.get(7)?,
                    error_msg: row.get(8)?,
                    record_hash: row.get(9)?,
                    previous_hash: row.get(10)?,
                })
            })
            .map_err(|e| StorageError::QueryError(format!("Failed to query errors: {}", e)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::QueryError(format!("Failed to map errors: {}", e)))?;

        Ok(entries)
    }

    /// Get total number of audit log entries
    pub fn audit_log_count(conn: &Connection) -> StorageResult<i64> {
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM audit_log", [], |row| row.get(0))
            .map_err(|e| StorageError::QueryError(format!("Failed to count audit logs: {}", e)))?;

        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::storage::db::Database;

    #[test]
    fn test_log_operation() {
        let db = Database::new("/tmp/test_audit.db", 5).expect("Failed to create test DB");
        let conn = db.connect().expect("Failed to connect");

        let result = AuditLogger::log_operation(&conn, "TEST_OP", Some("test_table"), Some(10), Some(5));
        assert!(result.is_ok());

        // Verify it was logged
        let count = AuditLogger::audit_log_count(&conn).expect("Failed to count");
        assert!(count > 0);

        let _ = std::fs::remove_file("/tmp/test_audit.db");
    }

    #[test]
    fn test_log_error() {
        let db = Database::new("/tmp/test_audit_error.db", 5).expect("Failed to create test DB");
        let conn = db.connect().expect("Failed to connect");

        let result =
            AuditLogger::log_error(&conn, "TEST_ERROR", Some("test_table"), "Test error message", Some(5));
        assert!(result.is_ok());

        // Query the error
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let errors = AuditLogger::query_errors(&conn, now - 100, now + 100, 10).expect("Failed to query");
        assert!(!errors.is_empty());
        assert!(errors[0].error_msg.is_some());

        let _ = std::fs::remove_file("/tmp/test_audit_error.db");
    }
}
