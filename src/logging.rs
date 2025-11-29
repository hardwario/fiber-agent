// src/logging.rs
use chrono::{DateTime, Utc};
use rusqlite::Connection;

/// Log severity levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Critical,
}

/// Default log level = INFO.
impl Default for LogLevel {
    fn default() -> Self {
        LogLevel::Info
    }
}

/// One log entry. Shape matches what will be persisted.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub ts_utc: DateTime<Utc>,
    pub level: LogLevel,
    pub source: String,
    pub message: String,
}

impl LogEntry {
    pub fn new(
        ts_utc: DateTime<Utc>,
        level: LogLevel,
        source: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            ts_utc,
            level,
            source: source.into(),
            message: message.into(),
        }
    }
}

/// Abstract log sink. Later we plug different implementations here.
pub trait LogSink {
    fn log(&mut self, entry: LogEntry);

    fn log_msg(
        &mut self,
        ts: DateTime<Utc>,
        level: LogLevel,
        source: &'static str,
        msg: impl Into<String>,
    ) {
        self.log(LogEntry::new(ts, level, source, msg));
    }

    fn info(&mut self, ts: DateTime<Utc>, source: &'static str, msg: impl Into<String>) {
        self.log_msg(ts, LogLevel::Info, source, msg);
    }

    fn warn(&mut self, ts: DateTime<Utc>, source: &'static str, msg: impl Into<String>) {
        self.log_msg(ts, LogLevel::Warn, source, msg);
    }

    fn error(&mut self, ts: DateTime<Utc>, source: &'static str, msg: impl Into<String>) {
        self.log_msg(ts, LogLevel::Error, source, msg);
    }
}

/// Simple in-memory logger for tests and debugging.
#[derive(Default)]
pub struct InMemoryLogger {
    entries: Vec<LogEntry>,
    min_level: LogLevel,
}

impl InMemoryLogger {
    pub fn new(min_level: LogLevel) -> Self {
        Self {
            entries: Vec::new(),
            min_level,
        }
    }

    pub fn entries(&self) -> &[LogEntry] {
        &self.entries
    }

    pub fn into_entries(self) -> Vec<LogEntry> {
        self.entries
    }
}

impl LogSink for InMemoryLogger {
    fn log(&mut self, entry: LogEntry) {
        if entry.level >= self.min_level {
            self.entries.push(entry);
        }
    }
}

/// SQLite-backed log sink.
///
/// Schema:
///   logs(
///     id          INTEGER PRIMARY KEY AUTOINCREMENT,
///     ts_utc_ms   INTEGER NOT NULL,
///     level       INTEGER NOT NULL,
///     source      TEXT NOT NULL,
///     message     TEXT NOT NULL
///   )
pub struct SqliteLogSink {
    conn: Connection,
}

impl SqliteLogSink {
    /// Open a file-backed SQLite DB and initialize schema + WAL.
    pub fn open_file(path: impl AsRef<std::path::Path>) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        Self::init_conn(&conn)?;
        Ok(Self { conn })
    }

    /// Open an in-memory DB (used mostly for tests).
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init_conn(&conn)?;
        Ok(Self { conn })
    }

    fn init_conn(conn: &Connection) -> rusqlite::Result<()> {
        // For file-backed DBs, this will switch to WAL.
        // For in-memory DBs, SQLite may effectively ignore WAL, which is fine for tests.
        conn.pragma_update(None, "journal_mode", &"WAL")?;
        conn.pragma_update(None, "synchronous", &"NORMAL")?;

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS logs (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                ts_utc_ms   INTEGER NOT NULL,
                level       INTEGER NOT NULL,
                source      TEXT NOT NULL,
                message     TEXT NOT NULL
            );
            "#,
        )?;

        Ok(())
    }

    /// Helper for tests: count log rows.
    #[cfg(test)]
    pub fn count_rows(&self) -> usize {
        self.conn
            .query_row("SELECT COUNT(*) FROM logs", [], |row| row.get::<_, i64>(0))
            .unwrap_or(0) as usize
    }
}

fn level_to_i32(level: LogLevel) -> i32 {
    match level {
        LogLevel::Trace => 0,
        LogLevel::Debug => 1,
        LogLevel::Info => 2,
        LogLevel::Warn => 3,
        LogLevel::Error => 4,
        LogLevel::Critical => 5,
    }
}

impl LogSink for SqliteLogSink {
    fn log(&mut self, entry: LogEntry) {
        let ts_ms = entry.ts_utc.timestamp_millis();
        let level_i = level_to_i32(entry.level);

        self.conn
            .execute(
                "INSERT INTO logs (ts_utc_ms, level, source, message) VALUES (?1, ?2, ?3, ?4)",
                (&ts_ms, &level_i, &entry.source, &entry.message),
            )
            .expect("failed to insert log entry into SQLite");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn t(ms: i64) -> DateTime<Utc> {
        Utc.timestamp_millis_opt(ms).unwrap()
    }

    #[test]
    fn logger_respects_min_level_and_orders_entries() {
        let mut logger = InMemoryLogger::new(LogLevel::Info);

        logger.log(LogEntry::new(t(0), LogLevel::Debug, "test", "debug msg"));
        logger.log(LogEntry::new(t(10), LogLevel::Info, "test", "info msg"));
        logger.log(LogEntry::new(t(20), LogLevel::Warn, "other", "warn msg"));

        let entries = logger.entries();

        // Debug should be dropped (below min_level)
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].level, LogLevel::Info);
        assert_eq!(entries[0].message, "info msg");
        assert_eq!(entries[1].level, LogLevel::Warn);
        assert_eq!(entries[1].message, "warn msg");
    }

    #[test]
    fn convenience_methods_work() {
        let mut logger = InMemoryLogger::new(LogLevel::Trace);
        let now = t(0);

        logger.info(now, "acq", "acquisition started");
        logger.warn(now, "alarm", "threshold near limit");
        logger.error(now, "system", "something bad happened");

        let entries = logger.entries();
        assert_eq!(entries.len(), 3);
        assert!(entries
            .iter()
            .any(|e| e.source == "acq" && e.level == LogLevel::Info));
        assert!(entries
            .iter()
            .any(|e| e.source == "alarm" && e.level == LogLevel::Warn));
        assert!(entries
            .iter()
            .any(|e| e.source == "system" && e.level == LogLevel::Error));
    }

    #[test]
    fn sqlite_log_sink_persists_entries() {
        let mut sink = SqliteLogSink::open_in_memory().expect("open in-memory sqlite");
        let now = t(0);

        sink.info(now, "system", "hello");
        sink.warn(now, "alarm", "warn");
        sink.error(now, "system", "boom");

        assert_eq!(sink.count_rows(), 3);
    }
}
