//! Data integrity verification for EU MDR 2017/745 compliance
//! Implements SHA-256 hash-chain for audit trail tamper detection
//! and HMAC-SHA256 for sensor reading authenticity

use sha2::{Sha256, Digest};
use hmac::{Hmac, Mac};
use rusqlite::Connection;

use crate::libs::storage::error::{StorageError, StorageResult};

type HmacSha256 = Hmac<Sha256>;

/// Compute SHA-256 hash of an audit log record's content
/// All fields are included in the hash to detect any modification
pub fn compute_audit_record_hash(
    timestamp: i64,
    operation: &str,
    table_name: Option<&str>,
    record_count: Option<i64>,
    duration_ms: Option<i64>,
    thread_id: &str,
    details: Option<&str>,
    error_msg: Option<&str>,
    previous_hash: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(timestamp.to_le_bytes());

    // Length-prefix variable-length fields to prevent boundary ambiguity
    hasher.update(&(operation.len() as u64).to_le_bytes());
    hasher.update(operation.as_bytes());

    let tn = table_name.unwrap_or("");
    hasher.update(&(tn.len() as u64).to_le_bytes());
    hasher.update(tn.as_bytes());

    hasher.update(record_count.unwrap_or(0).to_le_bytes());
    hasher.update(duration_ms.unwrap_or(0).to_le_bytes());

    hasher.update(&(thread_id.len() as u64).to_le_bytes());
    hasher.update(thread_id.as_bytes());

    let det = details.unwrap_or("");
    hasher.update(&(det.len() as u64).to_le_bytes());
    hasher.update(det.as_bytes());

    let err = error_msg.unwrap_or("");
    hasher.update(&(err.len() as u64).to_le_bytes());
    hasher.update(err.as_bytes());

    let prev = previous_hash.unwrap_or("GENESIS");
    hasher.update(&(prev.len() as u64).to_le_bytes());
    hasher.update(prev.as_bytes());

    hex::encode(hasher.finalize())
}

/// Get the hash of the most recent audit log entry (the chain tip)
pub fn get_latest_audit_hash(conn: &Connection) -> StorageResult<Option<String>> {
    use rusqlite::OptionalExtension;
    let result: Option<String> = conn
        .query_row(
            "SELECT record_hash FROM audit_log WHERE record_hash IS NOT NULL ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| StorageError::QueryError(format!("Failed to get latest audit hash: {}", e)))?;

    Ok(result)
}

/// Verify the integrity of the audit trail hash chain
/// Returns Ok(verified_count) if chain is intact, or Err with the first broken link
pub fn verify_audit_chain(conn: &Connection) -> StorageResult<i64> {
    let mut stmt = conn
        .prepare(
            "SELECT id, timestamp, operation, table_name, record_count, duration_ms, thread_id, details, error_msg, record_hash, previous_hash
             FROM audit_log
             WHERE record_hash IS NOT NULL
             ORDER BY id ASC"
        )
        .map_err(|e| StorageError::QueryError(format!("Failed to prepare chain query: {}", e)))?;

    let mut verified = 0i64;
    let mut expected_previous: Option<String> = None;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
            ))
        })
        .map_err(|e| StorageError::QueryError(format!("Failed to query chain: {}", e)))?;

    for row_result in rows {
        let (id, timestamp, operation, table_name, record_count, duration_ms, thread_id, details, error_msg, record_hash, previous_hash)
            = row_result.map_err(|e| StorageError::QueryError(format!("Row error: {}", e)))?;

        // Verify previous_hash matches our expectation
        if expected_previous.is_none() {
            // First record: previous_hash must be "GENESIS"
            let actual_prev = previous_hash.as_deref().unwrap_or("GENESIS");
            if actual_prev != "GENESIS" {
                return Err(StorageError::IntegrityError(
                    format!("First audit record (id={}) has unexpected previous_hash='{}', expected 'GENESIS'", id, actual_prev)
                ));
            }
        }

        if let Some(ref expected) = expected_previous {
            let actual_prev = previous_hash.as_deref().unwrap_or("GENESIS");
            if actual_prev != expected {
                return Err(StorageError::IntegrityError(
                    format!("Hash chain broken at audit_log id={}: expected previous_hash='{}', found='{}'", id, expected, actual_prev)
                ));
            }
        }

        // Recompute and verify record_hash
        let computed = compute_audit_record_hash(
            timestamp,
            &operation,
            table_name.as_deref(),
            record_count,
            duration_ms,
            thread_id.as_deref().unwrap_or("unknown"),
            details.as_deref(),
            error_msg.as_deref(),
            previous_hash.as_deref(),
        );

        if let Some(ref stored_hash) = record_hash {
            if &computed != stored_hash {
                return Err(StorageError::IntegrityError(
                    format!("Record hash mismatch at audit_log id={}: stored='{}', computed='{}'", id, stored_hash, computed)
                ));
            }
        }

        expected_previous = record_hash;
        verified += 1;
    }

    Ok(verified)
}

/// Compute HMAC-SHA256 for a sensor reading
pub fn compute_reading_hmac(
    secret: &[u8],
    timestamp: i64,
    sensor_line: u8,
    temperature_c: f32,
    is_connected: bool,
    alarm_state: &str,
) -> String {
    let mut mac = HmacSha256::new_from_slice(secret)
        .expect("HMAC accepts any key length");
    mac.update(&timestamp.to_le_bytes());
    mac.update(&[sensor_line]);
    mac.update(&temperature_c.to_le_bytes());
    mac.update(&[if is_connected { 1u8 } else { 0u8 }]);
    mac.update(&(alarm_state.len() as u64).to_le_bytes());
    mac.update(alarm_state.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Verify HMAC of a sensor reading using constant-time comparison
pub fn verify_reading_hmac(
    secret: &[u8],
    timestamp: i64,
    sensor_line: u8,
    temperature_c: f32,
    is_connected: bool,
    alarm_state: &str,
    expected_hmac: &str,
) -> bool {
    let mut mac = HmacSha256::new_from_slice(secret)
        .expect("HMAC accepts any key length");
    mac.update(&timestamp.to_le_bytes());
    mac.update(&[sensor_line]);
    mac.update(&temperature_c.to_le_bytes());
    mac.update(&[if is_connected { 1u8 } else { 0u8 }]);
    mac.update(&(alarm_state.len() as u64).to_le_bytes());
    mac.update(alarm_state.as_bytes());
    let expected_bytes = hex::decode(expected_hmac).unwrap_or_default();
    mac.verify_slice(&expected_bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::storage::db::Database;

    #[test]
    fn test_compute_audit_record_hash_deterministic() {
        let hash1 = compute_audit_record_hash(
            1000, "INSERT", Some("sensor_readings"), Some(10), Some(5),
            "main", None, None, None,
        );
        let hash2 = compute_audit_record_hash(
            1000, "INSERT", Some("sensor_readings"), Some(10), Some(5),
            "main", None, None, None,
        );
        assert_eq!(hash1, hash2, "Same inputs must produce same hash");
        assert_eq!(hash1.len(), 64, "SHA-256 hex digest should be 64 chars");
    }

    #[test]
    fn test_compute_audit_record_hash_changes_with_input() {
        let hash1 = compute_audit_record_hash(
            1000, "INSERT", Some("sensor_readings"), Some(10), Some(5),
            "main", None, None, None,
        );
        let hash2 = compute_audit_record_hash(
            1001, "INSERT", Some("sensor_readings"), Some(10), Some(5),
            "main", None, None, None,
        );
        assert_ne!(hash1, hash2, "Different timestamp must produce different hash");
    }

    #[test]
    fn test_genesis_hash() {
        // First record in chain uses GENESIS as previous
        let hash = compute_audit_record_hash(
            1000, "INSERT", Some("test"), None, None,
            "main", None, None, None,
        );
        assert!(!hash.is_empty());
    }

    #[test]
    fn test_chain_links_previous_hash() {
        let hash1 = compute_audit_record_hash(
            1000, "INSERT", Some("test"), None, None,
            "main", None, None, None,
        );
        let hash2 = compute_audit_record_hash(
            1001, "INSERT", Some("test"), None, None,
            "main", None, None, Some(&hash1),
        );
        // hash2 includes hash1 as previous, so changing hash1 would change hash2
        let hash2_alt = compute_audit_record_hash(
            1001, "INSERT", Some("test"), None, None,
            "main", None, None, Some("tampered"),
        );
        assert_ne!(hash2, hash2_alt, "Different previous_hash must produce different record_hash");
    }

    #[test]
    fn test_get_latest_audit_hash_empty_db() {
        let db = Database::new("/tmp/test_integrity_empty.db", 5).expect("Failed to create test DB");
        let conn = db.connect().expect("Failed to connect");

        let result = get_latest_audit_hash(&conn).expect("Should not error on empty DB");
        assert!(result.is_none(), "Empty DB should have no latest hash");

        let _ = std::fs::remove_file("/tmp/test_integrity_empty.db");
    }

    #[test]
    fn test_verify_audit_chain_empty() {
        let db = Database::new("/tmp/test_integrity_chain_empty.db", 5).expect("Failed to create test DB");
        let conn = db.connect().expect("Failed to connect");

        let count = verify_audit_chain(&conn).expect("Should verify empty chain");
        assert_eq!(count, 0);

        let _ = std::fs::remove_file("/tmp/test_integrity_chain_empty.db");
    }

    #[test]
    fn test_verify_audit_chain_with_entries() {
        let db = Database::new("/tmp/test_integrity_chain_entries.db", 5).expect("Failed to create test DB");
        let conn = db.connect().expect("Failed to connect");

        // Insert a chain of 3 records
        let prev_hash: Option<String> = None;
        let hash1 = compute_audit_record_hash(
            1000, "INSERT", Some("test"), Some(1), Some(5),
            "main", None, None, prev_hash.as_deref(),
        );

        conn.execute(
            "INSERT INTO audit_log (timestamp, operation, table_name, record_count, duration_ms, thread_id, record_hash, previous_hash)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![1000, "INSERT", "test", 1, 5, "main", hash1, Option::<String>::None],
        ).expect("Insert 1");

        let hash2 = compute_audit_record_hash(
            1001, "DELETE", Some("test"), Some(2), Some(10),
            "main", None, None, Some(hash1.as_str()),
        );

        conn.execute(
            "INSERT INTO audit_log (timestamp, operation, table_name, record_count, duration_ms, thread_id, record_hash, previous_hash)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![1001, "DELETE", "test", 2, 10, "main", hash2, hash1],
        ).expect("Insert 2");

        let hash3 = compute_audit_record_hash(
            1002, "EXPORT", None, Some(100), Some(50),
            "main", None, None, Some(hash2.as_str()),
        );

        conn.execute(
            "INSERT INTO audit_log (timestamp, operation, table_name, record_count, duration_ms, thread_id, record_hash, previous_hash)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![1002, "EXPORT", Option::<String>::None, 100, 50, "main", hash3, hash2],
        ).expect("Insert 3");

        let count = verify_audit_chain(&conn).expect("Chain should verify");
        assert_eq!(count, 3);

        let _ = std::fs::remove_file("/tmp/test_integrity_chain_entries.db");
    }

    #[test]
    fn test_verify_audit_chain_detects_tamper() {
        let db = Database::new("/tmp/test_integrity_tamper.db", 5).expect("Failed to create test DB");
        let conn = db.connect().expect("Failed to connect");

        let hash1 = compute_audit_record_hash(
            1000, "INSERT", Some("test"), Some(1), Some(5),
            "main", None, None, None,
        );

        conn.execute(
            "INSERT INTO audit_log (timestamp, operation, table_name, record_count, duration_ms, thread_id, record_hash, previous_hash)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![1000, "INSERT", "test", 1, 5, "main", hash1, Option::<String>::None],
        ).expect("Insert 1");

        // Insert a tampered second record (wrong record_hash)
        conn.execute(
            "INSERT INTO audit_log (timestamp, operation, table_name, record_count, duration_ms, thread_id, record_hash, previous_hash)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![1001, "DELETE", "test", 2, 10, "main", "tampered_hash_value", hash1],
        ).expect("Insert tampered");

        let result = verify_audit_chain(&conn);
        assert!(result.is_err(), "Should detect tampered record");
        if let Err(StorageError::IntegrityError(msg)) = result {
            assert!(msg.contains("Record hash mismatch"), "Error should mention hash mismatch");
        }

        let _ = std::fs::remove_file("/tmp/test_integrity_tamper.db");
    }

    #[test]
    fn test_compute_reading_hmac_deterministic() {
        let secret = b"test_secret_key";
        let hmac1 = compute_reading_hmac(secret, 1000, 0, 36.5, true, "NORMAL");
        let hmac2 = compute_reading_hmac(secret, 1000, 0, 36.5, true, "NORMAL");
        assert_eq!(hmac1, hmac2, "Same inputs must produce same HMAC");
        assert_eq!(hmac1.len(), 64, "HMAC-SHA256 hex digest should be 64 chars");
    }

    #[test]
    fn test_compute_reading_hmac_different_secret() {
        let hmac1 = compute_reading_hmac(b"key1", 1000, 0, 36.5, true, "NORMAL");
        let hmac2 = compute_reading_hmac(b"key2", 1000, 0, 36.5, true, "NORMAL");
        assert_ne!(hmac1, hmac2, "Different keys must produce different HMACs");
    }

    #[test]
    fn test_verify_reading_hmac() {
        let secret = b"test_secret_key";
        let hmac = compute_reading_hmac(secret, 1000, 0, 36.5, true, "NORMAL");
        assert!(verify_reading_hmac(secret, 1000, 0, 36.5, true, "NORMAL", &hmac));
        assert!(!verify_reading_hmac(secret, 1001, 0, 36.5, true, "NORMAL", &hmac));
        assert!(!verify_reading_hmac(b"wrong_key", 1000, 0, 36.5, true, "NORMAL", &hmac));
    }
}
