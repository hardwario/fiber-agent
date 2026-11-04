//! SQLite database initialization and schema management
//! Implements WAL mode for crash safety and EU MDR compliance

use rusqlite::{Connection, OptionalExtension};
use std::path::{Path, PathBuf};

use crate::libs::storage::error::{StorageError, StorageResult};

/// Current schema version for migrations
pub const CURRENT_SCHEMA_VERSION: i32 = 5;

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
        Self::with_max_bytes(db_path, (max_size_gb.max(1) as i64) * 1024 * 1024 * 1024)
    }

    /// Build a Database with a precomputed byte cap. Used when the
    /// operator config specifies sub-GB granularity via
    /// `StorageConfig::max_size_mb`; `new(path, gb)` stays as the
    /// integer-GB convenience constructor.
    pub fn with_max_bytes(db_path: impl AsRef<Path>, max_size_bytes: i64) -> StorageResult<Self> {
        let db_path = db_path.as_ref().to_path_buf();
        let max_size_bytes = max_size_bytes.max(1024 * 1024); // sanity floor: 1 MB

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
                    std::fs::set_permissions(key_path, std::fs::Permissions::from_mode(0o600))
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

    /// The SQLCipher key every connection to this database — and to its spill
    /// journal — must present, or `None` in the test/dev case where no key file
    /// exists.
    ///
    /// Factored out of `configure_pragmas` so the spill journal
    /// ([`crate::libs::storage::spill`]) can key itself the same way without a
    /// third copy of the hardcoded path. A key file that exists but is
    /// unreadable, empty, or non-hex is a hard error: the alternative is
    /// silently opening un-keyed, which either creates an unencrypted file
    /// (first boot) or fails to read back any encrypted row (existing install).
    pub fn encryption_key() -> StorageResult<Option<String>> {
        let key_path = std::path::Path::new("/data/fiber/config/db_encryption.key");
        if !key_path.exists() {
            return Ok(None);
        }
        let key = std::fs::read_to_string(key_path).map_err(|e| {
            StorageError::DatabaseInitError(format!(
                "Encryption key at {:?} is unreadable: {}",
                key_path, e
            ))
        })?;
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(StorageError::DatabaseInitError(format!(
                "Encryption key at {:?} is empty or not hex; refusing to open DB un-keyed",
                key_path
            )));
        }
        Ok(Some(key.to_string()))
    }

    /// Create a new connection to the database
    pub fn connect(&self) -> StorageResult<Connection> {
        let conn = Connection::open(&self.db_path).map_err(|e| {
            StorageError::DatabaseInitError(format!("Failed to open database: {}", e))
        })?;

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

        Self::ensure_bucket_window_column(&conn)?;

        Ok(())
    }

    /// Add `sensor_readings_minute.window_s`, the length in seconds a bucket
    /// covers from `minute_ts`, so the raw retention sweep can tell which
    /// bucket contains a raw row. NULL is a pre-2026-10-12 one-minute roll-up
    /// bucket. A nullable column added in place, with no schema version
    /// bump: an older image booted from the other slot still opens the file
    /// and simply leaves the column NULL.
    fn ensure_bucket_window_column(conn: &Connection) -> StorageResult<()> {
        if !Self::has_bucket_window_column(conn)? {
            Self::add_bucket_window_column(conn)?;
        }
        Ok(())
    }

    fn has_bucket_window_column(conn: &Connection) -> StorageResult<bool> {
        conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('sensor_readings_minute')
             WHERE name = 'window_s'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .map_err(|e| {
            StorageError::MigrationError(format!("Failed to inspect bucket columns: {}", e))
        })
    }

    /// Several threads open the database at start-up, so two of them can both
    /// see the column missing: the one that loses the race gets "duplicate
    /// column name". The column being there afterwards is what matters.
    fn add_bucket_window_column(conn: &Connection) -> StorageResult<()> {
        match conn.execute_batch("ALTER TABLE sensor_readings_minute ADD COLUMN window_s INTEGER;")
        {
            Ok(()) => Ok(()),
            Err(_) if Self::has_bucket_window_column(conn)? => Ok(()),
            Err(e) => Err(StorageError::MigrationError(format!(
                "Failed to add window_s column: {}",
                e
            ))),
        }
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
        if let Some(key) = Self::encryption_key()? {
            conn.execute_batch(&format!("PRAGMA key = '{}';", key))
                .map_err(|e| {
                    StorageError::DatabaseInitError(format!("Failed to set encryption key: {}", e))
                })?;
        }

        // Wait out a lock rather than giving up on it.
        //
        // rusqlite's default is 5 s and it is applied silently, which is fine
        // while every writer holds the lock for microseconds. It stops being
        // fine once a full `VACUUM` can hold it for the length of a rewrite:
        // every other writer on this connection then fails after 5 s with
        // SQLITE_BUSY, and a failed write on this device is a lost reading.
        // 30 s matches the rewrite connection in `retention::full_vacuum` and
        // the portal's own `BUSY_TIMEOUT`, so all three halves of the system
        // agree on how long "busy" is allowed to last.
        conn.busy_timeout(std::time::Duration::from_secs(30))
            .map_err(|e| {
                StorageError::DatabaseInitError(format!("Failed to set busy_timeout: {}", e))
            })?;

        // Enable WAL mode for crash-safe operation (required for medical devices)
        // Note: PRAGMA journal_mode returns the mode, so use query_row
        let _mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .map_err(|e| StorageError::DatabaseInitError(format!("Failed to enable WAL: {}", e)))?;

        // Enable foreign key constraints for data integrity
        conn.execute("PRAGMA foreign_keys = ON", []).map_err(|e| {
            StorageError::DatabaseInitError(format!("Failed to enable foreign keys: {}", e))
        })?;

        // NORMAL provides good balance: fsync on commit but not between writes
        // This is safer than OFF but faster than FULL
        conn.execute("PRAGMA synchronous = NORMAL", [])
            .map_err(|e| {
                StorageError::DatabaseInitError(format!("Failed to set synchronous mode: {}", e))
            })?;

        // Use in-memory temp storage (don't write to disk)
        conn.execute("PRAGMA temp_store = MEMORY", [])
            .map_err(|e| {
                StorageError::DatabaseInitError(format!("Failed to set temp_store: {}", e))
            })?;

        // Set cache size to 10MB for better performance with many sensors
        conn.execute("PRAGMA cache_size = -10000", [])
            .map_err(|e| {
                StorageError::DatabaseInitError(format!("Failed to set cache size: {}", e))
            })?;

        // Cap WAL growth so a stuck/slow checkpoint cannot eat the partition.
        // 64MB ceiling on the -wal file; auto-checkpoint every 1000 pages
        // (~4MB at the default page size). These pragmas return the new
        // value as a row, so use execute_batch which discards rows.
        conn.execute_batch(
            "PRAGMA journal_size_limit = 67108864;
             PRAGMA wal_autocheckpoint = 1000;",
        )
        .map_err(|e| {
            StorageError::DatabaseInitError(format!("Failed to set WAL size pragmas: {}", e))
        })?;

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
        .map_err(|e| {
            StorageError::DatabaseInitError(format!("Failed to create schema_version table: {}", e))
        })?;

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
        .map_err(|e| {
            StorageError::DatabaseInitError(format!(
                "Failed to create sensor_readings table: {}",
                e
            ))
        })?;

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
        .map_err(|e| {
            StorageError::DatabaseInitError(format!("Failed to create alarm_events table: {}", e))
        })?;

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
        .map_err(|e| {
            StorageError::DatabaseInitError(format!("Failed to create audit_log table: {}", e))
        })?;

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
        .map_err(|e| {
            StorageError::DatabaseInitError(format!("Failed to create config_changes table: {}", e))
        })?;

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
        .map_err(|e| StorageError::DatabaseInitError(format!("Failed to create indexes: {}", e)))?;

        Ok(())
    }

    /// Verify schema version and check for migrations
    fn verify_schema(&self, conn: &Connection) -> StorageResult<()> {
        // Check if schema_version table has any entries
        let version: Option<i32> = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get(0)
            })
            .optional()
            .map_err(|e| {
                StorageError::DatabaseInitError(format!("Failed to query schema version: {}", e))
            })?
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
                if v < 5 {
                    self.migrate_v4_to_v5(&conn)?;
                }
                Ok(())
            }
            Some(v) if v > CURRENT_SCHEMA_VERSION => Err(StorageError::MigrationError(format!(
                "Database schema version {} is newer than application version {}",
                v, CURRENT_SCHEMA_VERSION
            ))),
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

                // Create v3+v4+v5 tables on fresh initialization so DB starts at current version
                self.create_v3_tables(&conn)?;
                self.create_v4_tables(&conn)?;
                self.create_v5_tables(&conn)?;

                conn.execute(
                    "INSERT INTO schema_version (version, applied_at, description)
                     VALUES (?, ?, ?)",
                    rusqlite::params![
                        CURRENT_SCHEMA_VERSION,
                        now,
                        "Initial schema v5 (save-and-feed + tamper-evidence + minute aggregates + eye readings)"
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
             ALTER TABLE audit_log ADD COLUMN previous_hash TEXT;",
        )
        .map_err(|e| {
            StorageError::MigrationError(format!("Failed to add hash columns to audit_log: {}", e))
        })?;

        conn.execute_batch("ALTER TABLE sensor_readings ADD COLUMN data_hmac TEXT;")
            .map_err(|e| {
                StorageError::MigrationError(format!(
                    "Failed to add HMAC column to sensor_readings: {}",
                    e
                ))
            })?;

        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_audit_log_hash ON audit_log(record_hash);",
        )
        .map_err(|e| StorageError::MigrationError(format!("Failed to create hash index: {}", e)))?;

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

        AuditLogger::log_schema_change(
            conn,
            "Migration v1\u{2192}v2: added tamper-evidence columns",
        )?;

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
             );",
        )
        .map_err(|e| StorageError::MigrationError(format!("Failed to create v3 tables: {}", e)))?;
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
        .map_err(|e| {
            StorageError::MigrationError(format!("Failed to record v3 migration: {}", e))
        })?;

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
                 ON sensor_readings_minute(sensor_line, minute_ts DESC);",
        )
        .map_err(|e| StorageError::MigrationError(format!("Failed to create v4 tables: {}", e)))?;
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
            rusqlite::params![4, now, "Minute-aggregate table for long-term retention"],
        )
        .map_err(|e| {
            StorageError::MigrationError(format!("Failed to record v4 migration: {}", e))
        })?;

        AuditLogger::log_schema_change(conn, "Migration v3\u{2192}v4: sensor_readings_minute")?;

        eprintln!("MIGRATION: Schema upgraded from v3 to v4 (minute aggregates)");
        Ok(())
    }

    /// Create v5 tables: EYE BLE tag readings (save-and-feed stream, like
    /// sticker_readings but without the OTAA provisioning epoch).
    fn create_v5_tables(&self, conn: &Connection) -> StorageResult<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS eye_readings (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 mac          TEXT    NOT NULL,
                 ts           INTEGER NOT NULL,
                 received_at  INTEGER NOT NULL,
                 message_id   TEXT    NOT NULL UNIQUE,
                 event_type   TEXT    NOT NULL,
                 payload_json TEXT    NOT NULL,
                 created_at   INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_eye_readings_mac_ts
                 ON eye_readings(mac, ts);
             CREATE INDEX IF NOT EXISTS idx_eye_readings_id
                 ON eye_readings(id);",
        )
        .map_err(|e| StorageError::MigrationError(format!("Failed to create v5 tables: {}", e)))?;
        Ok(())
    }

    /// Migrate database schema from v4 to v5. Adds eye_readings.
    fn migrate_v4_to_v5(&self, conn: &Connection) -> StorageResult<()> {
        use crate::libs::storage::audit::AuditLogger;

        self.create_v5_tables(conn)?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        conn.execute(
            "INSERT INTO schema_version (version, applied_at, description) VALUES (?, ?, ?)",
            rusqlite::params![5, now, "EYE BLE tag readings (save-and-feed stream)"],
        )
        .map_err(|e| {
            StorageError::MigrationError(format!("Failed to record v5 migration: {}", e))
        })?;

        AuditLogger::log_schema_change(conn, "Migration v4\u{2192}v5: eye_readings")?;

        eprintln!("MIGRATION: Schema upgraded from v4 to v5 (eye readings)");
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

    /// A database path nobody else in this binary can touch.
    ///
    /// These tests used to share one hardcoded `/tmp/test_fiber_db.db` and
    /// delete it on entry and exit, so under cargo's default parallelism they
    /// removed each other's files mid-run — `test_wal_mode_enabled` failed in
    /// roughly two runs in five. The returned `TempDir` is the cleanup: hold it
    /// for the length of the test and dropping it takes the database, its
    /// `-wal` and its `-shm` with it.
    fn test_db(name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("create a temp dir for the test database");
        let path = dir.path().join(format!("{name}.db"));
        (dir, path)
    }

    #[test]
    fn test_database_creation() {
        let (_dir, path) = test_db("test_database_creation");

        let db = Database::new(&path, 5).expect("Failed to create database");
        assert!(path.exists(), "Database file should exist");
        assert_eq!(db.max_size_gb(), 5);
    }

    #[test]
    fn test_schema_creation() {
        let (_dir, path) = test_db("test_schema_creation");

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
    }

    #[test]
    fn test_wal_mode_enabled() {
        let (_dir, path) = test_db("test_wal_mode_enabled");

        let db = Database::new(&path, 5).expect("Failed to create database");
        let conn = db.connect().expect("Failed to connect");

        let journal_mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("Failed to query journal mode");

        assert_eq!(
            journal_mode.to_uppercase(),
            "WAL",
            "WAL mode should be enabled"
        );
    }

    /// A write must wait out a `VACUUM`, not give up on it.
    ///
    /// rusqlite applies a 5 s `busy_timeout` by default and does it silently,
    /// so before this pragma existed every writer on this connection failed
    /// five seconds into a rewrite — and on this device a failed write is a
    /// lost reading. Asserted as a value rather than by holding a lock for six
    /// seconds: the waiting is SQLite's behaviour, the number is ours, and the
    /// number is the part that regressed.
    #[test]
    fn a_connection_waits_out_a_lock_far_longer_than_rusqlites_default() {
        let (_dir, path) = test_db("busy_timeout");
        let db = Database::new(&path, 5).expect("Failed to create database");
        let conn = db.connect().expect("Failed to connect");

        let timeout_ms: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .expect("Failed to query busy_timeout");

        assert_eq!(
            timeout_ms, 30_000,
            "must match retention::full_vacuum's rewrite connection and the portal's BUSY_TIMEOUT"
        );
    }

    #[test]
    fn test_size_calculation() {
        let (_dir, path) = test_db("test_size_calculation");

        let db = Database::new(&path, 5).expect("Failed to create database");
        let size = db.current_size_bytes().expect("Failed to get size");
        assert!(size > 0, "Database should have some size");

        let mb = db.get_current_size_mb().expect("Failed to get size in MB");
        assert!(mb < 1.0, "Empty database should be less than 1MB");
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
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);
    }

    /// A database from before `window_s` gets the column on open, and its
    /// existing buckets keep NULL (one-minute roll-ups).
    #[test]
    fn an_existing_database_gains_the_bucket_window_column() {
        let (_dir, path) = test_db("bucket_window_column");
        {
            let db = Database::new(&path, 1).unwrap();
            let conn = db.connect().unwrap();
            conn.execute_batch(
                "ALTER TABLE sensor_readings_minute DROP COLUMN window_s;
                 INSERT INTO sensor_readings_minute
                     (minute_ts, sensor_line, min_c, avg_c, max_c,
                      sample_count, created_at, worst_alarm)
                 VALUES (60, 0, 1.0, 2.0, 3.0, 30, 120, 'NORMAL');",
            )
            .unwrap();
        }
        let db = Database::new(&path, 1).unwrap();
        let conn = db.connect().unwrap();
        let window: Option<i64> = conn
            .query_row(
                "SELECT window_s FROM sensor_readings_minute WHERE minute_ts = 60",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(window, None);
    }

    /// Two openers racing at start-up: the loser's ALTER finds the column
    /// already there, which must not fail the storage thread.
    #[test]
    fn adding_the_bucket_window_column_twice_is_not_an_error() {
        let (_dir, path) = test_db("bucket_window_column_race");
        let db = Database::new(&path, 1).unwrap();
        let conn = db.connect().unwrap();
        assert!(Database::has_bucket_window_column(&conn).unwrap());
        Database::add_bucket_window_column(&conn).unwrap();
    }
}
