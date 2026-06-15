//! SQLite database initialization and schema management
//! Implements WAL mode for crash safety and EU MDR compliance

use rusqlite::{Connection, OptionalExtension};
use std::path::{Path, PathBuf};

use crate::libs::storage::error::{StorageError, StorageResult};

/// Current schema version for migrations
pub const CURRENT_SCHEMA_VERSION: i32 = 4;

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

        // Generate DB encryption key if it doesn't exist.
        //
        // Production semantics: the systemd/yocto setup pre-creates
        // /data/fiber/config — so if it exists but we then fail to write
        // the key (disk full, EROFS, partial mount), that's a real prod
        // problem and we MUST refuse to continue: otherwise the next
        // configure_pragmas pass opens the DB without PRAGMA key, the
        // sqlcipher file is created unencrypted, and the next boot (once
        // the key lands) can never decrypt it.
        //
        // Test/dev semantics: if the parent dir doesn't exist and we can't
        // create it (sandboxed test env), there's no place to put a key —
        // fall back to "no encryption" with a loud warning so the test
        // suite continues to work without becoming a vector for prod
        // mis-configuration.
        let key_path = std::path::Path::new("/data/fiber/config/db_encryption.key");
        if !key_path.exists() {
            let parent = key_path.parent();
            let parent_ok = parent
                .map(|p| std::fs::create_dir_all(p).is_ok() && p.exists())
                .unwrap_or(false);
            if !parent_ok {
                eprintln!(
                    "[storage] WARN: encryption key dir {:?} is unavailable — \
                     database will be opened WITHOUT SQLCipher encryption \
                     (acceptable in tests / dev; misconfigured in production)",
                    parent,
                );
            } else {
                use rand::Rng;
                let key: Vec<u8> = (0..32).map(|_| rand::thread_rng().gen()).collect();
                let hex_key = hex::encode(&key);
                std::fs::write(key_path, &hex_key).map_err(|e| {
                    StorageError::IoError(format!(
                        "Failed to write DB encryption key at {:?}: {} \
                         (refusing to open DB without a key would leave it unencrypted)",
                        key_path, e,
                    ))
                })?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(
                        key_path,
                        std::fs::Permissions::from_mode(0o600),
                    )
                    .map_err(|e| {
                        StorageError::IoError(format!(
                            "Failed to chmod 0600 the DB encryption key at {:?}: {}",
                            key_path, e
                        ))
                    })?;
                }
                eprintln!("Generated new database encryption key");
            }
        }

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
    /// - Encryption key (SQLCipher) — must be first pragma
    /// - WAL mode: Write-Ahead Logging for crash recovery
    /// - Foreign keys: Referential integrity
    /// - Synchronous: Balance safety and performance
    fn configure_pragmas(&self, conn: &Connection) -> StorageResult<()> {
        // Encryption key (SQLCipher) — must be first pragma. If the key file
        // exists but is unreadable or empty/non-hex, we refuse to continue —
        // the alternative is silently opening the DB un-keyed, which then
        // either creates an unencrypted file (first boot) or fails to read
        // back any encrypted rows (existing install). Both are worse than a
        // hard error at startup.
        let key_path = std::path::Path::new("/data/fiber/config/db_encryption.key");
        if key_path.exists() {
            let key = std::fs::read_to_string(key_path).map_err(|e| {
                StorageError::DatabaseInitError(format!(
                    "Encryption key at {:?} is unreadable: {}",
                    key_path, e
                ))
            })?;
            let key = key.trim();
            if key.is_empty() || !key.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(StorageError::DatabaseInitError(format!(
                    "Encryption key at {:?} is empty or not hex; refusing to \
                     open DB un-keyed",
                    key_path
                )));
            }
            conn.execute_batch(&format!("PRAGMA key = '{}';", key))
                .map_err(|e| StorageError::DatabaseInitError(
                    format!("Failed to set encryption key: {}", e),
                ))?;
        }

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

        // Cap WAL growth so a stuck/slow checkpoint cannot eat the partition.
        // 64MB ceiling on the -wal file; auto-checkpoint every 1000 pages
        // (~4MB at the default page size). These pragmas return the new
        // value as a row, so use execute_batch which discards rows.
        conn.execute_batch(
            "PRAGMA journal_size_limit = 67108864;
             PRAGMA wal_autocheckpoint = 1000;",
        )
        .map_err(|e| StorageError::DatabaseInitError(
            format!("Failed to set WAL size pragmas: {}", e),
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
                if v < 3 {
                    self.migrate_v2_to_v3(&conn)?;
                }
                if v < 4 {
                    self.migrate_v3_to_v4(&conn)?;
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

                // Create v3+v4 tables on fresh initialization so DB starts at current version
                self.create_v3_tables(&conn)?;
                self.create_v4_tables(&conn)?;

                conn.execute(
                    "INSERT INTO schema_version (version, applied_at, description)
                     VALUES (?, ?, ?)",
                    rusqlite::params![
                        CURRENT_SCHEMA_VERSION,
                        now,
                        "Initial schema v4 (save-and-feed + tamper-evidence + minute aggregates)"
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

    /// Create v3 tables (sticker_readings, sticker_provisioning_epoch, export_cursor)
    /// Used both by migration and by fresh initialization.
    fn create_v3_tables(&self, conn: &Connection) -> StorageResult<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sticker_readings (
                 id                 INTEGER PRIMARY KEY AUTOINCREMENT,
                 dev_eui            TEXT    NOT NULL,
                 provisioning_epoch INTEGER NOT NULL,
                 ts                 INTEGER NOT NULL,
                 received_at        INTEGER NOT NULL,
                 message_id         TEXT    NOT NULL UNIQUE,
                 event_type         TEXT    NOT NULL,
                 payload_json       TEXT    NOT NULL,
                 created_at         INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_sticker_readings_dev_eui_ts
                 ON sticker_readings(dev_eui, ts);
             CREATE INDEX IF NOT EXISTS idx_sticker_readings_id
                 ON sticker_readings(id);

             CREATE TABLE IF NOT EXISTS sticker_provisioning_epoch (
                 dev_eui    TEXT    PRIMARY KEY,
                 epoch      INTEGER NOT NULL DEFAULT 1,
                 updated_at INTEGER NOT NULL
             );

             CREATE TABLE IF NOT EXISTS export_cursor (
                 broker_id        TEXT    NOT NULL,
                 stream           TEXT    NOT NULL,
                 last_exported_id INTEGER NOT NULL DEFAULT 0,
                 updated_at       INTEGER NOT NULL,
                 PRIMARY KEY (broker_id, stream)
             );"
        )
        .map_err(|e| StorageError::MigrationError(
            format!("Failed to create v3 tables: {}", e),
        ))?;
        Ok(())
    }

    /// Migrate database schema from v2 to v3
    /// Adds sticker_readings, sticker_provisioning_epoch, export_cursor
    /// for the save-and-feed firmware DB / store-and-forward export pipeline.
    fn migrate_v2_to_v3(&self, conn: &Connection) -> StorageResult<()> {
        use crate::libs::storage::audit::AuditLogger;

        self.create_v3_tables(conn)?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        conn.execute(
            "INSERT INTO schema_version (version, applied_at, description) VALUES (?, ?, ?)",
            rusqlite::params![
                3,
                now,
                "Save-and-feed: sticker_readings, sticker_provisioning_epoch, export_cursor"
            ],
        )
        .map_err(|e| StorageError::MigrationError(
            format!("Failed to record v3 migration: {}", e),
        ))?;

        AuditLogger::log_schema_change(conn, "Migration v2\u{2192}v3: save-and-feed tables")?;

        eprintln!("MIGRATION: Schema upgraded from v2 to v3 (save-and-feed)");
        Ok(())
    }

    /// Create v4 tables (sensor_readings_minute).
    ///
    /// Holds per-minute aggregates (min/avg/max + sample/disconnect counts +
    /// worst alarm state) so the raw `sensor_readings` table can be retained
    /// for ~30 days while history queries spanning months/years are answered
    /// from the aggregate table instead. WITHOUT ROWID + composite PK keeps
    /// the row physical size small.
    fn create_v4_tables(&self, conn: &Connection) -> StorageResult<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sensor_readings_minute (
                 minute_ts        INTEGER NOT NULL,
                 sensor_line      INTEGER NOT NULL,
                 min_c            REAL    NOT NULL,
                 avg_c            REAL    NOT NULL,
                 max_c            REAL    NOT NULL,
                 sample_count     INTEGER NOT NULL,
                 disconnect_count INTEGER NOT NULL DEFAULT 0,
                 worst_alarm      TEXT    NOT NULL,
                 created_at       INTEGER NOT NULL,
                 data_hmac        BLOB,
                 PRIMARY KEY (minute_ts, sensor_line)
             ) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS idx_srm_minute_ts
                 ON sensor_readings_minute(minute_ts DESC);
             CREATE INDEX IF NOT EXISTS idx_srm_line_minute
                 ON sensor_readings_minute(sensor_line, minute_ts DESC);"
        )
        .map_err(|e| StorageError::MigrationError(
            format!("Failed to create v4 tables: {}", e),
        ))?;
        Ok(())
    }

    /// Migrate database schema from v3 to v4
    /// Adds sensor_readings_minute for per-minute aggregates.
    fn migrate_v3_to_v4(&self, conn: &Connection) -> StorageResult<()> {
        use crate::libs::storage::audit::AuditLogger;

        self.create_v4_tables(conn)?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        conn.execute(
            "INSERT INTO schema_version (version, applied_at, description) VALUES (?, ?, ?)",
            rusqlite::params![
                4,
                now,
                "Minute-aggregate table for long-term retention"
            ],
        )
        .map_err(|e| StorageError::MigrationError(
            format!("Failed to record v4 migration: {}", e),
        ))?;

        AuditLogger::log_schema_change(conn, "Migration v3\u{2192}v4: sensor_readings_minute")?;

        eprintln!("MIGRATION: Schema upgraded from v3 to v4 (minute aggregates)");
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

    #[test]
    fn schema_v3_creates_new_tables() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 1).unwrap();
        let conn = db.connect().unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert!(tables.contains(&"sticker_readings".to_string()));
        assert!(tables.contains(&"sticker_provisioning_epoch".to_string()));
        assert!(tables.contains(&"export_cursor".to_string()));
        assert!(tables.contains(&"sensor_readings_minute".to_string()));

        let version: i32 = conn
            .query_row(
                "SELECT MAX(version) FROM schema_version",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);
    }
}
