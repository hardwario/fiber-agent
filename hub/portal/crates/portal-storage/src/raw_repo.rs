//! `readings_raw` repository — dedup-safe raw samples.

use rusqlite::params;

use crate::error::Result;
use crate::models::RawReading;
use crate::Storage;

impl Storage {
    /// Insert a raw reading, ignoring duplicates on the natural key
    /// `(hostname, sensor_line, ts)`.
    pub fn insert_raw(&self, row: &RawReading) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO readings_raw
                (hostname, sensor_line, ts, min_c, avg_c, max_c, sample_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                row.hostname,
                row.sensor_line,
                row.ts,
                row.min_c,
                row.avg_c,
                row.max_c,
                row.sample_count,
            ],
        )?;
        Ok(())
    }

    /// Number of raw rows stored for a sensor line.
    pub fn count_raw(&self, hostname: &str, sensor_line: i64) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM readings_raw WHERE hostname = ?1 AND sensor_line = ?2",
            params![hostname, sensor_line],
            |r| r.get(0),
        )?;
        Ok(count as usize)
    }
}
