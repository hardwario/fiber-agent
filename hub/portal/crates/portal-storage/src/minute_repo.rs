//! `readings_minute` repository — the 30-day hot tier.

use rusqlite::{params, OptionalExtension};

use crate::error::Result;
use crate::models::MinuteReading;
use crate::Storage;

impl Storage {
    /// Upsert a minute row. On conflict the existing row's values are
    /// replaced, *except* a `source = 'replay'` row is never downgraded by
    /// an incoming non-replay (`live`) row for the same slot — replay
    /// backfills must survive later live sweeps.
    pub fn upsert_minute(&self, row: &MinuteReading) -> Result<()> {
        self.conn.execute(
            "INSERT INTO readings_minute
                (hostname, sensor_line, minute_ts, min_c, avg_c, max_c,
                 sample_count, disconnect_count, worst_alarm, source, data_hmac)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(hostname, sensor_line, minute_ts) DO UPDATE SET
                min_c = excluded.min_c,
                avg_c = excluded.avg_c,
                max_c = excluded.max_c,
                sample_count = excluded.sample_count,
                disconnect_count = excluded.disconnect_count,
                worst_alarm = excluded.worst_alarm,
                source = excluded.source,
                data_hmac = excluded.data_hmac
             WHERE NOT (readings_minute.source = 'replay' AND excluded.source != 'replay')",
            params![
                row.hostname,
                row.sensor_line,
                row.minute_ts,
                row.min_c,
                row.avg_c,
                row.max_c,
                row.sample_count,
                row.disconnect_count,
                row.worst_alarm,
                row.source,
                row.data_hmac,
            ],
        )?;
        Ok(())
    }

    /// Most recent minute row for a sensor line, if any.
    pub fn latest_minute(&self, hostname: &str, sensor_line: i64) -> Result<Option<MinuteReading>> {
        let row = self
            .conn
            .query_row(
                "SELECT hostname, sensor_line, minute_ts, min_c, avg_c, max_c,
                        sample_count, disconnect_count, worst_alarm, source, data_hmac
                 FROM readings_minute
                 WHERE hostname = ?1 AND sensor_line = ?2
                 ORDER BY minute_ts DESC
                 LIMIT 1",
                params![hostname, sensor_line],
                |r| {
                    Ok(MinuteReading {
                        hostname: r.get(0)?,
                        sensor_line: r.get(1)?,
                        minute_ts: r.get(2)?,
                        min_c: r.get(3)?,
                        avg_c: r.get(4)?,
                        max_c: r.get(5)?,
                        sample_count: r.get(6)?,
                        disconnect_count: r.get(7)?,
                        worst_alarm: r.get(8)?,
                        source: r.get(9)?,
                        data_hmac: r.get(10)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }
}
