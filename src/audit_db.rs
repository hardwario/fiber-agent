// src/audit_db.rs
// SQLite-based audit log with HMAC signatures

use crate::audit::{AuditEntry, AuditSink};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use rusqlite::{params, Connection, OptionalExtension, Result as SqlResult};
use sha2::Sha256;
use std::path::PathBuf;

type HmacSha256 = Hmac<Sha256>;

pub struct SqliteAuditSink {
    conn: Connection,
    hmac_key: [u8; 32],
}

impl SqliteAuditSink {
    /// Open audit database from persistent /data partition
    /// Path: /data/fiber/audit.db (survives RAUC updates)
    pub fn open_file(path: &str, hmac_key: [u8; 32]) -> SqlResult<Self> {
        // Create directory if it doesn't exist
        if let Some(parent) = PathBuf::from(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let conn = Connection::open(path)?;

        // Set pragmas for durability and performance
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "wal_autocheckpoint", "10000")?;
        conn.pragma_update(None, "synchronous", "FULL")?; // Audit requires durability
        conn.pragma_update(None, "foreign_keys", "ON")?;

        // Create audit_log table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS audit_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts_utc DATETIME NOT NULL,
                event_type TEXT NOT NULL,
                sensor_id INTEGER NOT NULL,
                severity TEXT NOT NULL,
                value REAL,
                details TEXT,
                hash TEXT NOT NULL,
                signature TEXT NOT NULL,
                signer_id TEXT NOT NULL DEFAULT 'system',
                sequence INTEGER NOT NULL UNIQUE,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP
            )",
            [],
        )?;

        // Create index for queries
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_log(ts_utc)",
            [],
        )?;

        Ok(Self { conn, hmac_key })
    }

    pub fn open_in_memory(hmac_key: [u8; 32]) -> SqlResult<Self> {
        let conn = Connection::open_in_memory()?;

        // Set pragmas
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        conn.execute(
            "CREATE TABLE audit_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts_utc DATETIME NOT NULL,
                event_type TEXT NOT NULL,
                sensor_id INTEGER NOT NULL,
                severity TEXT NOT NULL,
                value REAL,
                details TEXT,
                hash TEXT NOT NULL,
                signature TEXT NOT NULL,
                signer_id TEXT NOT NULL DEFAULT 'system',
                sequence INTEGER NOT NULL UNIQUE,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP
            )",
            [],
        )?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_log(ts_utc)",
            [],
        )?;

        Ok(Self { conn, hmac_key })
    }

    fn compute_signature(&self, hash: &str, prev_hash: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.hmac_key)
            .expect("HMAC can take key of any size");
        mac.update(format!("{}:{}", hash, prev_hash).as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    pub fn entry_count(&self) -> SqlResult<u64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM audit_log", [], |row| row.get(0))
    }
}

impl AuditSink for SqliteAuditSink {
    fn record_event(&mut self, mut entry: AuditEntry) -> Result<(), Box<dyn std::error::Error>> {
        // Compute SHA256 hash of entry content
        use sha2::Digest;
        let mut hasher = Sha256::new();
        hasher.update(format!(
            "{}:{}:{}:{}:{}:{}",
            entry.ts_utc, entry.event_type, entry.sensor_id, entry.severity, entry.value, entry.details
        ));
        entry.hash = hex::encode(hasher.finalize());

        // Get previous hash for signature chain
        let prev_hash: String = self
            .conn
            .query_row(
                "SELECT COALESCE((SELECT hash FROM audit_log ORDER BY sequence DESC LIMIT 1), '')",
                [],
                |row| row.get(0),
            )
            .unwrap_or_default();

        entry.signature = self.compute_signature(&entry.hash, &prev_hash);

        // Get next sequence number
        let next_seq: u64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(sequence), 0) + 1 FROM audit_log",
                [],
                |row| row.get(0),
            )
            .unwrap_or(1);

        entry.sequence = next_seq;

        // Insert into database
        self.conn.execute(
            "INSERT INTO audit_log (ts_utc, event_type, sensor_id, severity, value, details, hash, signature, signer_id, sequence)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                entry.ts_utc,
                entry.event_type,
                entry.sensor_id,
                entry.severity,
                entry.value,
                entry.details,
                entry.hash,
                entry.signature,
                entry.signer_id,
                entry.sequence
            ],
        )?;

        Ok(())
    }

    fn verify_entry(&self, id: u64) -> Result<bool, Box<dyn std::error::Error>> {
        // Query the entry
        let entry: (String, String, u64) = self
            .conn
            .query_row(
                "SELECT hash, signature, sequence FROM audit_log WHERE id = ?",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|e| format!("Entry not found: {}", e))?;

        let (hash, signature, sequence) = entry;

        // Get previous hash for verification
        let prev_hash: String = if sequence > 1 {
            self.conn
                .query_row(
                    "SELECT COALESCE((SELECT hash FROM audit_log WHERE sequence = ?), '')",
                    params![sequence - 1],
                    |row| row.get(0),
                )
                .unwrap_or_default()
        } else {
            String::new()
        };

        let expected_sig = self.compute_signature(&hash, &prev_hash);
        Ok(signature == expected_sig)
    }

    fn get_entry(&self, id: u64) -> Result<Option<AuditEntry>, Box<dyn std::error::Error>> {
        let entry: Option<AuditEntry> = self
            .conn
            .query_row(
                "SELECT id, ts_utc, event_type, sensor_id, severity, value, details, hash, signature, signer_id, sequence
                 FROM audit_log WHERE id = ?",
                params![id],
                |row| {
                    Ok(AuditEntry {
                        id: row.get(0)?,
                        ts_utc: row.get(1)?,
                        event_type: row.get(2)?,
                        sensor_id: row.get(3)?,
                        severity: row.get(4)?,
                        value: row.get(5)?,
                        details: row.get(6)?,
                        hash: row.get(7)?,
                        signature: row.get(8)?,
                        signer_id: row.get(9)?,
                        sequence: row.get(10)?,
                    })
                },
            )
            .optional()?;

        Ok(entry)
    }

    fn query_range(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<AuditEntry>, Box<dyn std::error::Error>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts_utc, event_type, sensor_id, severity, value, details, hash, signature, signer_id, sequence
             FROM audit_log WHERE ts_utc >= ? AND ts_utc <= ? ORDER BY sequence ASC",
        )?;

        let entries = stmt
            .query_map(params![from, to], |row| {
                Ok(AuditEntry {
                    id: row.get(0)?,
                    ts_utc: row.get(1)?,
                    event_type: row.get(2)?,
                    sensor_id: row.get(3)?,
                    severity: row.get(4)?,
                    value: row.get(5)?,
                    details: row.get(6)?,
                    hash: row.get(7)?,
                    signature: row.get(8)?,
                    signer_id: row.get(9)?,
                    sequence: row.get(10)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(entries)
    }

    fn get_last_hash(&self) -> Result<String, Box<dyn std::error::Error>> {
        let hash: String = self
            .conn
            .query_row(
                "SELECT COALESCE((SELECT hash FROM audit_log ORDER BY sequence DESC LIMIT 1), '')",
                [],
                |row| row.get(0),
            )?;

        Ok(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sqlite_audit_basic_insert() {
        let mut sink = SqliteAuditSink::open_in_memory([42u8; 32]).unwrap();

        let entry = AuditEntry {
            id: 0,
            ts_utc: Utc::now(),
            event_type: "alarm_triggered".to_string(),
            sensor_id: 1,
            severity: "Critical".to_string(),
            value: 39.5,
            details: "Temperature too high".to_string(),
            hash: String::new(),
            signature: String::new(),
            signer_id: "system".to_string(),
            sequence: 0,
        };

        sink.record_event(entry).unwrap();

        let count = sink.entry_count().unwrap();
        assert_eq!(count, 1, "Should have 1 entry");
    }

    #[test]
    fn test_sqlite_audit_chain_verification() {
        let mut sink = SqliteAuditSink::open_in_memory([42u8; 32]).unwrap();

        for i in 1..=5 {
            let entry = AuditEntry {
                id: i as u64,
                ts_utc: Utc::now(),
                event_type: "alarm_triggered".to_string(),
                sensor_id: i as u32,
                severity: "Warning".to_string(),
                value: 37.0 + i as f32,
                details: format!("Event {}", i),
                hash: String::new(),
                signature: String::new(),
                signer_id: "system".to_string(),
                sequence: 0,
            };
            sink.record_event(entry).unwrap();
        }

        let count = sink.entry_count().unwrap();
        assert_eq!(count, 5, "Should have 5 entries");

        // Verify each entry
        for i in 1..=5 {
            let is_valid = sink.verify_entry(i).unwrap();
            assert!(is_valid, "Entry {} should verify", i);
        }
    }

    #[test]
    fn test_sqlite_audit_range_query() {
        let mut sink = SqliteAuditSink::open_in_memory([42u8; 32]).unwrap();

        let now = Utc::now();
        let one_hour_ago = now - chrono::Duration::hours(1);

        let entry1 = AuditEntry {
            id: 1,
            ts_utc: one_hour_ago,
            event_type: "alarm_triggered".to_string(),
            sensor_id: 1,
            severity: "Info".to_string(),
            value: 36.5,
            details: "Old".to_string(),
            hash: String::new(),
            signature: String::new(),
            signer_id: "system".to_string(),
            sequence: 0,
        };

        let entry2 = AuditEntry {
            id: 2,
            ts_utc: now,
            event_type: "alarm_triggered".to_string(),
            sensor_id: 1,
            severity: "Info".to_string(),
            value: 37.0,
            details: "Recent".to_string(),
            hash: String::new(),
            signature: String::new(),
            signer_id: "system".to_string(),
            sequence: 0,
        };

        sink.record_event(entry1).unwrap();
        sink.record_event(entry2).unwrap();

        let range = sink
            .query_range(now - chrono::Duration::minutes(10), now)
            .unwrap();
        assert_eq!(range.len(), 1, "Should find only recent entry");
    }

    #[test]
    fn test_sqlite_audit_sequence_integrity() {
        let mut sink = SqliteAuditSink::open_in_memory([42u8; 32]).unwrap();

        for i in 1..=3 {
            let entry = AuditEntry {
                id: i as u64,
                ts_utc: Utc::now(),
                event_type: "alarm_triggered".to_string(),
                sensor_id: 1,
                severity: "Info".to_string(),
                value: 36.5,
                details: "Test".to_string(),
                hash: String::new(),
                signature: String::new(),
                signer_id: "system".to_string(),
                sequence: 0,
            };
            sink.record_event(entry).unwrap();
        }

        let range = sink.query_range(Utc::now() - chrono::Duration::hours(1), Utc::now()).unwrap();
        assert_eq!(range.len(), 3);
        for (idx, entry) in range.iter().enumerate() {
            assert_eq!(entry.sequence, (idx + 1) as u64);
        }
    }

    #[test]
    fn test_sqlite_audit_get_entry() {
        let mut sink = SqliteAuditSink::open_in_memory([42u8; 32]).unwrap();

        let entry = AuditEntry {
            id: 1,
            ts_utc: Utc::now(),
            event_type: "alarm_triggered".to_string(),
            sensor_id: 42,
            severity: "Critical".to_string(),
            value: 39.5,
            details: "Test event".to_string(),
            hash: String::new(),
            signature: String::new(),
            signer_id: "system".to_string(),
            sequence: 0,
        };

        sink.record_event(entry).unwrap();

        let retrieved = sink.get_entry(1).unwrap();
        assert!(retrieved.is_some());
        let retrieved_entry = retrieved.unwrap();
        assert_eq!(retrieved_entry.sensor_id, 42);
        assert_eq!(retrieved_entry.value, 39.5);
        assert_eq!(retrieved_entry.severity, "Critical");
    }
}
