//! SQLite database initialization and schema management
//! Implements WAL mode for crash safety and EU MDR compliance

use rusqlite::{Connection, OptionalExtension};
use std::path::{Path, PathBuf};

use crate::libs::storage::error::{StorageError, StorageResult};

/// Current schema version for migrations
pub const CURRENT_SCHEMA_VERSION: i32 = 2;

/// SQLite database manager
pub struct Database {
    /// Path to the database file
    db_path: PathBuf,

    /// Maximum database size in bytes (5GB for this medical device)
    max_size_bytes: i64,
}

impl Database {
    /// Initialize a new database connection
    /// Creates schema if it doesn't exist
    pub fn new(db_path: impl AsRef<Path>, max_size_gb: i32) -> StorageResult<Self> {
        let db_path = db_path.as_ref().to_path_buf();
        let max_size_bytes = (max_size_gb as i64) * 1024 * 1024 * 1024;

        // Ensure parent directory exists
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                StorageError::IoError(format!("Failed to create DB directory: {}", e))
            })?;
        }

        let db = Self {
            db_path,
            max_size_bytes,
        };

        // Initialize the database with schema
        db.init_connection()?;

        Ok(db)
    }

    /// Create a new connection to the database
    pub fn connect(&self) -> StorageResult<Connection> {
        let conn =
            Connection::open(&self.db_path).map_err(|e| StorageError::DatabaseInitError(
                format!("Failed to open database: {}", e),
            ))?;

        // Configure WAL mode for crash safety
        self.configure_pragmas(&conn)?;

        Ok(conn)
    }

    /// Initialize the database with schema (called once on startup)
    fn init_connection(&self) -> StorageResult<()> {
        let conn = Connection::open(&self.db_path).map_err(|e| {
            StorageError::DatabaseInitError(format!("Failed to open database: {}", e))
        })?;

        self.configure_pragmas(&conn)?;

        // Create schema if not exists
        self.create_schema(&conn)?;

        // Verify schema version
        self.verify_schema(&conn)?;

        Ok(())
    }

    /// Configure SQLite pragmas for medical device compliance
    /// - WAL mode: Write-Ahead Logging for crash recovery
    /// - Foreign keys: Referential integrity
    /// - Synchronous: Balance safety and performance
    fn configure_pragmas(&self, conn: &Connection) -> StorageResult<()> {
        // Enable WAL mode for crash-safe operation (required for medical devices)
        // Note: PRAGMA journal_mode returns the mode, so use query_row
        let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .map_err(|e| StorageError::DatabaseInitError(format!("Failed to enable WAL: {}", e)))?;

        // Enable foreign key constraints for data integrity
        conn.execute("PRAGMA foreign_keys = ON", [])
            .map_err(|e| StorageError::DatabaseInitError(
                format!("Failed to enable foreign keys: {}", e),
            ))?;

        // NORMAL provides good balance: fsync on commit but not between writes
        // This is safer than OFF but faster than FULL
        conn.execute("PRAGMA synchronous = NORMAL", [])
            .map_err(|e| StorageError::DatabaseInitError(
                format!("Failed to set synchronous mode: {}", e),
            ))?;

        // Use in-memory temp storage (don't write to disk)
        conn.execute("PRAGMA temp_store = MEMORY", [])
            .map_err(|e| StorageError::DatabaseInitError(
                format!("Failed to set temp_store: {}", e),
            ))?;

        // Set cache size to 10MB for better performance with many sensors
        conn.execute("PRAGMA cache_size = -10000", [])
            .map_err(|e| StorageError::DatabaseInitError(
                format!("Failed to set cache size: {}", e),
            ))?;

        Ok(())
    }

    /// Create all database tables and indexes
    fn create_schema(&self, conn: &Connection) -> StorageResult<()> {
        // Create schema_version table first (tracks migrations)
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER PRIMARY KEY,
                applied_at INTEGER NOT NULL,
                description TEXT NOT NULL
            )",
        )
        .map_err(|e| StorageError::DatabaseInitError(
            format!("Failed to create schema_version table: {}", e),
        ))?;

        // Create sensor_readings table (main medical data)
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sensor_readings (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL,
                sensor_line INTEGER NOT NULL,
                temperature_c REAL NOT NULL,
                is_connected INTEGER NOT NULL,
                alarm_state TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                data_hmac TEXT
            )",
        )
        .map_err(|e| StorageError::DatabaseInitError(
            format!("Failed to create sensor_readings table: {}", e),
        ))?;

        // Create alarm_events table (alarm history)
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS alarm_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL,
                sensor_line INTEGER NOT NULL,
                from_state TEXT NOT NULL,
                to_state TEXT NOT NULL,
                temperature_c REAL,
                details TEXT
            )",
        )
        .map_err(|e| StorageError::DatabaseInitError(
            format!("Failed to create alarm_events table: {}", e),
        ))?;

        // Create audit_log table (EU MDR compliance - track all operations)
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS audit_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL,
                operation TEXT NOT NULL,
                table_name TEXT,
                record_count INTEGER,
                duration_ms INTEGER,
                thread_id TEXT,
                details TEXT,
                error_msg TEXT,
                record_hash TEXT,
                previous_hash TEXT
            )",
        )
        .map_err(|e| StorageError::DatabaseInitError(
            format!("Failed to create audit_log table: {}", e),
        ))?;

        // Create config_changes table (EU MDR compliance - signed configuration changes)
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS config_changes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL,
                challenge_id TEXT NOT NULL,
                request_id TEXT NOT NULL,
                signer_id TEXT NOT NULL,
                signer_name TEXT NOT NULL,
                command_type TEXT NOT NULL,
                command_json TEXT NOT NULL,
                signature_base64 TEXT NOT NULL,
                nonce TEXT NOT NULL UNIQUE,
                verification_status TEXT NOT NULL,
                applied INTEGER NOT NULL DEFAULT 0,
                error_msg TEXT
            )",
        )
        .map_err(|e| StorageError::DatabaseInitError(
            format!("Failed to create config_changes table: {}", e),
        ))?;

        // Create indexes for fast queries
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_sensor_readings_timestamp
             ON sensor_readings(timestamp DESC);
             CREATE INDEX IF NOT EXISTS idx_sensor_readings_line
             ON sensor_readings(sensor_line);
             CREATE INDEX IF NOT EXISTS idx_sensor_readings_line_timestamp
             ON sensor_readings(sensor_line, timestamp DESC);
             CREATE INDEX IF NOT EXISTS idx_alarm_events_timestamp
             ON alarm_events(timestamp DESC);
             CREATE INDEX IF NOT EXISTS idx_alarm_events_line
             ON alarm_events(sensor_line);
             CREATE INDEX IF NOT EXISTS idx_audit_log_timestamp
             ON audit_log(timestamp DESC);
             CREATE INDEX IF NOT EXISTS idx_audit_log_operation
             ON audit_log(operation);
             CREATE INDEX IF NOT EXISTS idx_audit_log_hash
             ON audit_log(record_hash);
             CREATE INDEX IF NOT EXISTS idx_config_changes_timestamp
             ON config_changes(timestamp DESC);
             CREATE INDEX IF NOT EXISTS idx_config_changes_signer
             ON config_changes(signer_id);
             CREATE INDEX IF NOT EXISTS idx_config_changes_nonce
             ON config_changes(nonce);",
        )
        .map_err(|e| StorageError::DatabaseInitError(
            format!("Failed to create indexes: {}", e),
        ))?;

        Ok(())
    }

    /// Verify schema version and check for migrations
    fn verify_schema(&self, conn: &Connection) -> StorageResult<()> {
        // Check if schema_version table has any entries
        let version: Option<i32> = conn
            .query_row(
                "SELECT MAX(version) FROM schema_version",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| StorageError::DatabaseInitError(
                format!("Failed to query schema version: {}", e),
            ))?
            .flatten();

        match version {
            Some(v) if v == CURRENT_SCHEMA_VERSION => {
                // Schema is up to date
                Ok(())
            }
            Some(v) if v < CURRENT_SCHEMA_VERSION => {
                if v < 2 {
                    self.migrate_v1_to_v2(&conn)?;
                }
                Ok(())
            }
            Some(v) if v > CURRENT_SCHEMA_VERSION => {
                Err(StorageError::MigrationError(
                    format!("Database schema version {} is newer than application version {}",
                        v, CURRENT_SCHEMA_VERSION)
                ))
            }
            Some(_) => {
                // v == CURRENT_SCHEMA_VERSION already handled above; unreachable
                Ok(())
            }
            None => {
                // First time initialization - insert schema version
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;

                conn.execute(
                    "INSERT INTO schema_version (version, applied_at, description)
                     VALUES (?, ?, ?)",
                    rusqlite::params![
                        CURRENT_SCHEMA_VERSION,
                        now,
                        "Initial schema v2: sensor_readings (with HMAC), alarm_events, audit_log (with hash-chain)"
                    ],
                )
                .map_err(|e| StorageError::DatabaseInitError(
                    format!("Failed to insert schema version: {}", e),
                ))?;

                Ok(())
            }
        }
    }

    /// Migrate database schema from v1 to v2
    /// Adds tamper-evidence columns for EU MDR 2017/745 Annex I, 17.1 compliance
    fn migrate_v1_to_v2(&self, conn: &Connection) -> StorageResult<()> {
        use crate::libs::storage::audit::AuditLogger;

        conn.execute_batch(
            "ALTER TABLE audit_log ADD COLUMN record_hash TEXT;
             ALTER TABLE audit_log ADD COLUMN previous_hash TEXT;"
        )
        .map_err(|e| StorageError::MigrationError(
            format!("Failed to add hash columns to audit_log: {}", e),
        ))?;

        conn.execute_batch(
            "ALTER TABLE sensor_readings ADD COLUMN data_hmac TEXT;"
        )
        .map_err(|e| StorageError::MigrationError(
            format!("Failed to add HMAC column to sensor_readings: {}", e),
        ))?;

        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_audit_log_hash ON audit_log(record_hash);"
        )
        .map_err(|e| StorageError::MigrationError(
            format!("Failed to create hash index: {}", e),
        ))?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        conn.execute(
            "INSERT INTO schema_version (version, applied_at, description) VALUES (?, ?, ?)",
            rusqlite::params![
                2,
                now,
                "Add hash-chain (audit_log.record_hash, previous_hash) and HMAC (sensor_readings.data_hmac) for EU MDR tamper-evidence"
            ],
        )
        .map_err(|e| StorageError::MigrationError(
            format!("Failed to record migration: {}", e),
        ))?;

        AuditLogger::log_schema_change(conn, "Migration v1\u{2192}v2: added tamper-evidence columns")?;

        eprintln!("MIGRATION: Schema upgraded from v1 to v2 (tamper-evident audit trail)");
        Ok(())
    }

    /// Get the database file path
    pub fn path(&self) -> &Path {
        &self.db_path
    }

    /// Get the maximum database size in bytes
    pub fn max_size_bytes(&self) -> i64 {
        self.max_size_bytes
    }

    /// Get the maximum database size in GB
    pub fn max_size_gb(&self) -> i32 {
        (self.max_size_bytes / (1024 * 1024 * 1024)) as i32
    }

    /// Check current database file size
    pub fn current_size_bytes(&self) -> StorageResult<i64> {
        std::fs::metadata(&self.db_path)
            .map(|m| m.len() as i64)
            .map_err(|e| StorageError::IoError(format!("Failed to get DB size: {}", e)))
    }

    /// Get database statistics
    pub fn get_current_size_mb(&self) -> StorageResult<f64> {
        let bytes = self.current_size_bytes()?;
        Ok(bytes as f64 / (1024.0 * 1024.0))
    }

    /// Get utilization percentage (0-100)
    pub fn get_utilization_percent(&self) -> StorageResult<f32> {
        let used = self.current_size_bytes()? as f32;
        let max = self.max_size_bytes as f32;
        Ok((used / max) * 100.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_db_path() -> PathBuf {
        PathBuf::from("/tmp/test_fiber_db.db")
    }

    fn cleanup_test_db(path: &Path) {
        let _ = fs::remove_file(path);
        let wal_path = format!("{}-wal", path.display());
        let _ = fs::remove_file(wal_path);
        let shm_path = format!("{}-shm", path.display());
        let _ = fs::remove_file(shm_path);
    }

    #[test]
    fn test_database_creation() {
        let path = test_db_path();
        cleanup_test_db(&path);

        let db = Database::new(&path, 5).expect("Failed to create database");
        assert!(path.exists(), "Database file should exist");
        assert_eq!(db.max_size_gb(), 5);

        cleanup_test_db(&path);
    }

    #[test]
    fn test_schema_creation() {
        let path = test_db_path();
        cleanup_test_db(&path);

        let db = Database::new(&path, 5).expect("Failed to create database");
        let conn = db.connect().expect("Failed to connect");

        // Verify tables exist
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .expect("Failed to prepare statement")
            .query_map([], |row| row.get(0))
            .expect("Failed to query tables")
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"sensor_readings".to_string()));
        assert!(tables.contains(&"alarm_events".to_string()));
        assert!(tables.contains(&"audit_log".to_string()));
        assert!(tables.contains(&"schema_version".to_string()));

        cleanup_test_db(&path);
    }

    #[test]
    fn test_wal_mode_enabled() {
        let path = test_db_path();
        cleanup_test_db(&path);

        let db = Database::new(&path, 5).expect("Failed to create database");
        let conn = db.connect().expect("Failed to connect");

        let journal_mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("Failed to query journal mode");

        assert_eq!(journal_mode.to_uppercase(), "WAL", "WAL mode should be enabled");

        cleanup_test_db(&path);
    }

    #[test]
    fn test_size_calculation() {
        let path = test_db_path();
        cleanup_test_db(&path);

        let db = Database::new(&path, 5).expect("Failed to create database");
        let size = db.current_size_bytes().expect("Failed to get size");
        assert!(size > 0, "Database should have some size");

        let mb = db.get_current_size_mb().expect("Failed to get size in MB");
        assert!(mb < 1.0, "Empty database should be less than 1MB");

        cleanup_test_db(&path);
    }
}
