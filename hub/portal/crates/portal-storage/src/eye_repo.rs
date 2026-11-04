//! EYE/BLE beacon persistence: `eye_readings`.
//!
//! One row per Beacon per Hub storage window, from `eye/aggregated`
//! (storage_intervals.md §5): event_type `advertising`, the window average in
//! the value columns and its min/max beside them (V31), message_id
//! `{hostname}/{MAC}-adv-{window_start}` (the Hub's id under its hostname). Rows written by older Portals (`live`, `{mac}-live-{ts//30}`) stay.
//! Dedup happens on the natural key `(hostname, mac, ts, event_type)` *and* the partial unique index on
//! `message_id` — `INSERT OR IGNORE` covers both.

use rusqlite::params;

use crate::error::Result;
use crate::models::{EyeReading, EyeWindowRange};
use crate::Storage;

impl Storage {
    /// Insert a beacon reading; returns `false` when it was a duplicate
    /// (same natural key, or same non-NULL `message_id` — the caller
    /// should skip re-processing, mirroring `record_export`).
    pub fn insert_eye_reading(&self, row: &EyeReading) -> Result<bool> {
        let row = &self.without_disabled_eye_fields(row)?;
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO eye_readings
                (hostname, mac, ts, temperature_c, humidity_pct, event_type, message_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                row.hostname,
                row.mac,
                row.ts,
                row.temperature_c,
                row.humidity_pct,
                row.event_type,
                row.message_id,
            ],
        )?;
        Ok(changed > 0)
    }

    /// Insert one Beacon storage window: the average in the value columns,
    /// as [`Storage::insert_eye_reading`], plus the window's min/max (V31).
    /// Same dedup, so a redelivered window is a no-op.
    pub fn insert_eye_window(&self, row: &EyeReading, range: &EyeWindowRange) -> Result<bool> {
        let row = &self.without_disabled_eye_fields(row)?;
        let mut range = *range;
        if row.temperature_c.is_none() {
            range.temperature_min_c = None;
            range.temperature_max_c = None;
        }
        if row.humidity_pct.is_none() {
            range.humidity_min_pct = None;
            range.humidity_max_pct = None;
        }
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO eye_readings
                (hostname, mac, ts, temperature_c, humidity_pct, event_type, message_id,
                 temperature_min_c, temperature_max_c, humidity_min_pct, humidity_max_pct)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                row.hostname,
                row.mac,
                row.ts,
                row.temperature_c,
                row.humidity_pct,
                row.event_type,
                row.message_id,
                range.temperature_min_c,
                range.temperature_max_c,
                range.humidity_min_pct,
                range.humidity_max_pct,
            ],
        )?;
        Ok(changed > 0)
    }

    /// `row` with the columns of the Beacon's switched-off sensors emptied: a switched-off
    /// sensor is not stored (sensor_disabled, V33, GitLab #120). The row itself is kept, so
    /// the other sensor and the window's dedup key still land.
    fn without_disabled_eye_fields(&self, row: &EyeReading) -> Result<EyeReading> {
        let disabled = self.disabled_sensor_fields(
            &row.hostname,
            crate::models::THRESHOLD_SCOPE_EYE,
            &row.mac,
        )?;
        let mut row = row.clone();
        if disabled.contains("temperature") {
            row.temperature_c = None;
        }
        if disabled.contains("humidity") {
            row.humidity_pct = None;
        }
        Ok(row)
    }

    /// [`Self::insert_eye_window`] for a cluster Beacon (decision #42): the row is keyed by the
    /// registry, and `source_hub` (V34) records the home Hub that heard the window.
    pub fn insert_eye_window_from(
        &self,
        row: &EyeReading,
        range: &EyeWindowRange,
        source_hub: &str,
    ) -> Result<bool> {
        let row = &self.without_disabled_eye_fields(row)?;
        let mut range = *range;
        if row.temperature_c.is_none() {
            range.temperature_min_c = None;
            range.temperature_max_c = None;
        }
        if row.humidity_pct.is_none() {
            range.humidity_min_pct = None;
            range.humidity_max_pct = None;
        }
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO eye_readings
                (hostname, mac, ts, temperature_c, humidity_pct, event_type, message_id,
                 temperature_min_c, temperature_max_c, humidity_min_pct, humidity_max_pct,
                 source_hub)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                row.hostname,
                row.mac,
                row.ts,
                row.temperature_c,
                row.humidity_pct,
                row.event_type,
                row.message_id,
                range.temperature_min_c,
                range.temperature_max_c,
                range.humidity_min_pct,
                range.humidity_max_pct,
                source_hub,
            ],
        )?;
        Ok(changed > 0)
    }

    /// The Hub a stored window came from (V34), by `message_id`. `None` when there is no such
    /// row or it predates V34.
    pub fn eye_window_source_hub(&self, message_id: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension as _;
        Ok(self
            .conn
            .query_row(
                "SELECT source_hub FROM eye_readings WHERE message_id = ?1",
                params![message_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// The min/max stored beside one `eye_readings` row, by `message_id`.
    /// `None` when no such row exists.
    pub fn eye_window_range(&self, message_id: &str) -> Result<Option<EyeWindowRange>> {
        use rusqlite::OptionalExtension as _;
        Ok(self
            .conn
            .query_row(
                "SELECT temperature_min_c, temperature_max_c, humidity_min_pct, humidity_max_pct
                 FROM eye_readings WHERE message_id = ?1",
                params![message_id],
                |r| {
                    Ok(EyeWindowRange {
                        temperature_min_c: r.get(0)?,
                        temperature_max_c: r.get(1)?,
                        humidity_min_pct: r.get(2)?,
                        humidity_max_pct: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Beacon readings for `[from_ts, to_ts)`, ordered by `ts` ASC.
    /// `limit` is clamped to `1..=50000` (default 5000 when `None`),
    /// per rest.md's eye readings endpoint.
    pub fn query_eye_readings(
        &self,
        hostname: &str,
        mac: &str,
        from_ts: i64,
        to_ts: i64,
        limit: Option<i64>,
    ) -> Result<Vec<EyeReading>> {
        let limit = limit.unwrap_or(5000).clamp(1, 50_000);
        let mut stmt = self.conn.prepare(
            "SELECT hostname, mac, ts, temperature_c, humidity_pct, event_type, message_id
             FROM eye_readings
             WHERE hostname = ?1 AND mac = ?2
               AND ts >= ?3 AND ts < ?4
             ORDER BY ts ASC
             LIMIT ?5",
        )?;
        let rows = stmt.query_map(params![hostname, mac, from_ts, to_ts, limit], |r| {
            Ok(EyeReading {
                hostname: r.get(0)?,
                mac: r.get(1)?,
                ts: r.get(2)?,
                temperature_c: r.get(3)?,
                humidity_pct: r.get(4)?,
                event_type: r.get(5)?,
                message_id: r.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}
