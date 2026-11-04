//! `export_ledger` repository — dedup ingest by `message_id`, 7-day retention.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;

use crate::error::Result;
use crate::Storage;

/// v1 default retention window for the export ledger.
pub const DEFAULT_RETENTION_DAYS: i64 = 7;

impl Storage {
    /// Record an exported message; returns `false` if `message_id` was
    /// already recorded (duplicate, caller should skip re-processing).
    pub fn record_export(
        &self,
        message_id: &str,
        stream: &str,
        exported_at: Option<i64>,
    ) -> Result<bool> {
        let received_at = now_unix();
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO export_ledger (message_id, stream, exported_at, received_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![message_id, stream, exported_at, received_at],
        )?;
        Ok(changed > 0)
    }

    /// Delete ledger rows older than `days` (by `received_at`); returns the
    /// number of rows removed.
    pub fn prune_ledger_older_than(&self, days: i64) -> Result<usize> {
        let cutoff = now_unix() - days * 86_400;
        let deleted = self.conn.execute(
            "DELETE FROM export_ledger WHERE received_at < ?1",
            params![cutoff],
        )?;
        Ok(deleted)
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
