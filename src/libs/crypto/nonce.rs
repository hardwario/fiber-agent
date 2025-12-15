//! Nonce tracking for replay attack prevention

use super::error::CryptoError;
use lru::LruCache;
use rusqlite::Connection;
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Nonce tracker with SQLite persistence and LRU cache
pub struct NonceTracker {
    /// Database connection
    db_path: String,

    /// LRU cache for fast lookups (nonce -> timestamp)
    cache: LruCache<String, i64>,

    /// Nonce validity window in seconds
    validity_sec: i64,
}

impl NonceTracker {
    /// Create a new nonce tracker
    pub fn new(db_path: &Path, validity_sec: i64, cache_size: usize) -> Result<Self, CryptoError> {
        let db_path_str = db_path.to_string_lossy().to_string();

        // Create database and table if needed
        {
            let conn = Connection::open(&db_path_str).map_err(|e| {
                CryptoError::NonceDatabaseError(format!("Failed to open database: {}", e))
            })?;

            conn.execute(
                "CREATE TABLE IF NOT EXISTS used_nonces (
                    nonce TEXT PRIMARY KEY,
                    timestamp INTEGER NOT NULL,
                    signer_id TEXT NOT NULL
                )",
                [],
            )
            .map_err(|e| {
                CryptoError::NonceDatabaseError(format!(
                    "Failed to create used_nonces table: {}",
                    e
                ))
            })?;

            // Create index for cleanup queries
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_nonce_timestamp ON used_nonces(timestamp)",
                [],
            )
            .map_err(|e| {
                CryptoError::NonceDatabaseError(format!("Failed to create index: {}", e))
            })?;
        }

        let cache = LruCache::new(NonZeroUsize::new(cache_size).unwrap());

        eprintln!(
            "[NonceTracker] Initialized with {}s validity window, cache size {}",
            validity_sec, cache_size
        );

        Ok(Self {
            db_path: db_path_str,
            cache,
            validity_sec,
        })
    }

    /// Check if nonce has been used recently
    pub fn is_nonce_used(&mut self, nonce: &str) -> Result<bool, CryptoError> {
        // Check cache first
        if self.cache.contains(nonce) {
            return Ok(true);
        }

        // Check database
        let conn = Connection::open(&self.db_path).map_err(|e| {
            CryptoError::NonceDatabaseError(format!("Failed to open database: {}", e))
        })?;

        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM used_nonces WHERE nonce = ?",
                [nonce],
                |row| row.get(0),
            )
            .map_err(|e| {
                CryptoError::NonceDatabaseError(format!("Failed to query nonce: {}", e))
            })?;

        Ok(count > 0)
    }

    /// Record nonce usage
    pub fn record_nonce(&mut self, nonce: &str, signer_id: &str, timestamp: i64) -> Result<(), CryptoError> {
        // Add to cache
        self.cache.put(nonce.to_string(), timestamp);

        // Add to database
        let conn = Connection::open(&self.db_path).map_err(|e| {
            CryptoError::NonceDatabaseError(format!("Failed to open database: {}", e))
        })?;

        conn.execute(
            "INSERT INTO used_nonces (nonce, timestamp, signer_id) VALUES (?, ?, ?)",
            rusqlite::params![nonce, timestamp, signer_id],
        )
        .map_err(|e| {
            CryptoError::NonceDatabaseError(format!("Failed to insert nonce: {}", e))
        })?;

        Ok(())
    }

    /// Cleanup old nonces (older than validity window)
    pub fn cleanup_old_nonces(&mut self) -> Result<usize, CryptoError> {
        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let cutoff_time = current_time - self.validity_sec;

        let conn = Connection::open(&self.db_path).map_err(|e| {
            CryptoError::NonceDatabaseError(format!("Failed to open database: {}", e))
        })?;

        let deleted = conn
            .execute("DELETE FROM used_nonces WHERE timestamp < ?", [cutoff_time])
            .map_err(|e| {
                CryptoError::NonceDatabaseError(format!("Failed to cleanup nonces: {}", e))
            })?;

        if deleted > 0 {
            eprintln!("[NonceTracker] Cleaned up {} old nonces", deleted);
        }

        Ok(deleted)
    }

    /// Get current nonce count in database
    pub fn nonce_count(&self) -> Result<usize, CryptoError> {
        let conn = Connection::open(&self.db_path).map_err(|e| {
            CryptoError::NonceDatabaseError(format!("Failed to open database: {}", e))
        })?;

        let count: i32 = conn
            .query_row("SELECT COUNT(*) FROM used_nonces", [], |row| row.get(0))
            .map_err(|e| {
                CryptoError::NonceDatabaseError(format!("Failed to count nonces: {}", e))
            })?;

        Ok(count as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_nonce_tracking() {
        let temp_db = "/tmp/test_nonces.db";
        let _ = fs::remove_file(temp_db); // Clean up if exists

        let mut tracker = NonceTracker::new(Path::new(temp_db), 600, 100).unwrap();

        let nonce1 = "abc123def456";
        let nonce2 = "xyz789uvw012";

        // First use should be allowed
        assert!(!tracker.is_nonce_used(nonce1).unwrap());

        // Record nonce
        tracker
            .record_nonce(nonce1, "test@example.com", 1702483200)
            .unwrap();

        // Second use should be detected
        assert!(tracker.is_nonce_used(nonce1).unwrap());

        // Different nonce should be allowed
        assert!(!tracker.is_nonce_used(nonce2).unwrap());

        // Cleanup
        let _ = fs::remove_file(temp_db);
    }

    #[test]
    fn test_nonce_cleanup() {
        let temp_db = "/tmp/test_nonces_cleanup.db";
        let _ = fs::remove_file(temp_db);

        let mut tracker = NonceTracker::new(Path::new(temp_db), 600, 100).unwrap();

        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Add old nonce (expired)
        tracker
            .record_nonce("old_nonce", "test@example.com", current_time - 700)
            .unwrap();

        // Add recent nonce (valid)
        tracker
            .record_nonce("new_nonce", "test@example.com", current_time - 100)
            .unwrap();

        assert_eq!(tracker.nonce_count().unwrap(), 2);

        // Cleanup should remove old nonce
        let deleted = tracker.cleanup_old_nonces().unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(tracker.nonce_count().unwrap(), 1);

        // New nonce should still exist
        assert!(tracker.is_nonce_used("new_nonce").unwrap());
        assert!(!tracker.is_nonce_used("old_nonce").unwrap());

        // Cleanup
        let _ = fs::remove_file(temp_db);
    }
}
