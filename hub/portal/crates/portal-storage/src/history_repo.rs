//! Time-range queries over `readings_minute` / `readings_raw` for the
//! history/zoom/export UI (M3-A). Reads only — no writes live here.
//!
//! All ranges are `[from_ts, to_ts)`: inclusive lower bound, exclusive upper
//! bound, matching the bucket convention used by `query_hourly`.

use rusqlite::params;

use crate::error::Result;
use crate::models::{HourlyBucket, MinuteReading, RawReading};
use crate::Storage;

impl Storage {
    /// Minute rows for `[from_ts, to_ts)`, ordered by `minute_ts` ASC.
    ///
    /// The composite primary key `(hostname, sensor_line, minute_ts)` is a
    /// covering left-prefix index for this predicate + ordering — see
    /// `tests/history.rs::query_minutes_uses_primary_key_index`.
    pub fn query_minutes(
        &self,
        hostname: &str,
        sensor_line: i64,
        from_ts: i64,
        to_ts: i64,
    ) -> Result<Vec<MinuteReading>> {
        let mut stmt = self.conn.prepare(
            "SELECT hostname, sensor_line, minute_ts, min_c, avg_c, max_c,
                    sample_count, disconnect_count, worst_alarm, source, data_hmac
             FROM readings_minute
             WHERE hostname = ?1 AND sensor_line = ?2
               AND minute_ts >= ?3 AND minute_ts < ?4
             ORDER BY minute_ts ASC",
        )?;
        let rows = stmt.query_map(params![hostname, sensor_line, from_ts, to_ts], |r| {
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
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Raw rows for `[from_ts, to_ts)`, ordered by `ts` ASC. Same
    /// covering-index reasoning as [`Storage::query_minutes`] applies to
    /// `readings_raw`'s `(hostname, sensor_line, ts)` primary key.
    pub fn query_raw(
        &self,
        hostname: &str,
        sensor_line: i64,
        from_ts: i64,
        to_ts: i64,
    ) -> Result<Vec<RawReading>> {
        let mut stmt = self.conn.prepare(
            "SELECT hostname, sensor_line, ts, min_c, avg_c, max_c, sample_count
             FROM readings_raw
             WHERE hostname = ?1 AND sensor_line = ?2
               AND ts >= ?3 AND ts < ?4
             ORDER BY ts ASC",
        )?;
        let rows = stmt.query_map(params![hostname, sensor_line, from_ts, to_ts], |r| {
            Ok(RawReading {
                hostname: r.get(0)?,
                sensor_line: r.get(1)?,
                ts: r.get(2)?,
                min_c: r.get(3)?,
                avg_c: r.get(4)?,
                max_c: r.get(5)?,
                sample_count: r.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Hour-aligned buckets over `readings_minute` for `[from_ts, to_ts)`,
    /// ordered by `hour_ts` ASC. `hour_ts` is the floor of `minute_ts` to the
    /// enclosing hour: `(minute_ts / 3600) * 3600`.
    ///
    /// `avg_c` is the sample-count-weighted mean of the bucket's minute
    /// averages — `SUM(avg_c * sample_count) / SUM(sample_count)` — which
    /// falls back to a plain `AVG(avg_c)` when every minute in the bucket has
    /// a `NULL sample_count` (so `SUM(sample_count)` is `NULL` and the
    /// weighted term divides by `NULLIF(..., 0)` -> `NULL`, and `COALESCE`
    /// picks the plain average instead). `min_c`/`max_c` use `MIN`/`MAX`,
    /// which already skip `NULL`s.
    pub fn query_hourly(
        &self,
        hostname: &str,
        sensor_line: i64,
        from_ts: i64,
        to_ts: i64,
    ) -> Result<Vec<HourlyBucket>> {
        let mut stmt = self.conn.prepare(
            "SELECT (minute_ts / 3600) * 3600 AS hour_ts,
                    MIN(min_c) AS min_c,
                    COALESCE(
                        SUM(avg_c * sample_count) / NULLIF(SUM(sample_count), 0),
                        AVG(avg_c)
                    ) AS avg_c,
                    MAX(max_c) AS max_c,
                    SUM(sample_count) AS sample_count
             FROM readings_minute
             WHERE hostname = ?1 AND sensor_line = ?2
               AND minute_ts >= ?3 AND minute_ts < ?4
             GROUP BY hour_ts
             ORDER BY hour_ts ASC",
        )?;
        let rows = stmt.query_map(params![hostname, sensor_line, from_ts, to_ts], |r| {
            Ok(HourlyBucket {
                hour_ts: r.get(0)?,
                min_c: r.get(1)?,
                avg_c: r.get(2)?,
                max_c: r.get(3)?,
                sample_count: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// `(MIN(minute_ts), MAX(minute_ts))` for a sensor line, or `None` if it
    /// has no minute rows — used to bound zoom/export range pickers.
    pub fn minute_extent(&self, hostname: &str, sensor_line: i64) -> Result<Option<(i64, i64)>> {
        let extent = self.conn.query_row(
            "SELECT MIN(minute_ts), MAX(minute_ts)
             FROM readings_minute
             WHERE hostname = ?1 AND sensor_line = ?2",
            params![hostname, sensor_line],
            |r| {
                let min_ts: Option<i64> = r.get(0)?;
                let max_ts: Option<i64> = r.get(1)?;
                Ok(min_ts.zip(max_ts))
            },
        )?;
        Ok(extent)
    }
}
