//! Count/min/max/avg/median over one probe's stored readings — the
//! probe-detail window's aggregate line (plan/portal/monitoring.backend.md
//! §6 WP3.1, spec plan/portal/monitoring.md §6.2).
//!
//! Reads only. Three methods, one per source table, because the three
//! sources disagree about which column *is* the value:
//!
//! | probe | table | value |
//! |---|---|---|
//! | wired | `readings_minute` | `avg_c` |
//! | Node | `node_field_readings` | `value` |
//! | Beacon | `eye_readings` | `temperature_c` / `humidity_pct` |
//!
//! **The wired probe summarises `avg_c` alone** — not `min_c` for the
//! minimum and `max_c` for the maximum (decision 2026-09-08). A minute row
//! already carries the extremes of the samples inside it, so mixing columns
//! would answer a different question per statistic: "the coldest instant"
//! for min against "the mean of the minute means" for avg. All five
//! statistics are therefore over the same series the chart draws, and
//! `count` is the number of **stored points**, not the number of samples
//! the Hub took.
//!
//! Every window is half-open `[from_ts, to_ts)`, matching `query_minutes`
//! and every other reader in this crate.
//!
//! ## The median
//!
//! SQLite has no `median()`. It is computed here as an ordered offset:
//! `ROW_NUMBER() OVER (ORDER BY val)` numbers the non-NULL values, and the
//! rows whose number is in `((n+1)/2, (n+2)/2)` — integer division — are
//! the middle one for an odd `n` and the two middles for an even one, so
//! their average is the median in both cases. One CTE computes the count
//! and the other four statistics, and the same CTE feeds the offset, so the
//! window is scanned once and `count` cannot disagree with the median it
//! sized.
//!
//! Window functions need SQLite 3.25+; the bundled SQLCipher 4.5 is on
//! 3.4x, well past that (`tests/probe_summary.rs` is the proof that runs).
//!
//! NULL values are excluded from every statistic, `count` included — a
//! stored row whose reading is NULL is a row with no value, and counting it
//! would make `avg` disagree with `count * mean`.

use rusqlite::{params, OptionalExtension, Row};

use crate::error::Result;
use crate::models::FieldSummary;
use crate::Storage;

/// The shared tail of all three queries: `vals` is a one-column CTE of
/// non-NULL values, and everything below is source-independent.
///
/// `total` is `COUNT(*)` over `vals` (already NULL-free), and the median
/// subquery re-reads `vals` with a row number. Written as one statement so
/// the two halves see the same snapshot without a transaction.
const SUMMARY_TAIL: &str = "
    ranked AS (
        SELECT val, ROW_NUMBER() OVER (ORDER BY val) AS rn, COUNT(*) OVER () AS n
        FROM vals
    )
    SELECT
        (SELECT COUNT(*) FROM vals),
        (SELECT MIN(val)  FROM vals),
        (SELECT MAX(val)  FROM vals),
        (SELECT AVG(val)  FROM vals),
        (SELECT AVG(val) FROM ranked WHERE rn IN ((n + 1) / 2, (n + 2) / 2))
";

/// `(count, min, max, avg, median)` in the column order [`SUMMARY_TAIL`]
/// selects. An empty window answers `count = 0` and four NULLs, which maps
/// straight onto [`FieldSummary::EMPTY`].
fn summary_from_row(r: &Row<'_>) -> rusqlite::Result<FieldSummary> {
    Ok(FieldSummary {
        count: r.get(0)?,
        min: r.get(1)?,
        max: r.get(2)?,
        avg: r.get(3)?,
        median: r.get(4)?,
    })
}

impl Storage {
    /// One wired probe's `readings_minute.avg_c` over `[from_ts, to_ts)`.
    ///
    /// See the module doc for why `min_c`/`max_c` are deliberately not
    /// consulted: this is a summary of the series the window charts.
    pub fn summarize_minutes(
        &self,
        hostname: &str,
        sensor_line: i64,
        from_ts: i64,
        to_ts: i64,
    ) -> Result<FieldSummary> {
        self.conn
            .query_row(
                &format!(
                    "WITH vals AS (
                        SELECT avg_c AS val FROM readings_minute
                        WHERE hostname = ?1 AND sensor_line = ?2
                          AND minute_ts >= ?3 AND minute_ts < ?4
                          AND avg_c IS NOT NULL
                     ),{SUMMARY_TAIL}"
                ),
                params![hostname, sensor_line, from_ts, to_ts],
                summary_from_row,
            )
            .map_err(Into::into)
    }

    /// One Node field's `node_field_readings.value` over `[from_ts, to_ts)`.
    pub fn summarize_node_field(
        &self,
        hostname: &str,
        node_key: &str,
        field: &str,
        from_ts: i64,
        to_ts: i64,
    ) -> Result<FieldSummary> {
        self.conn
            .query_row(
                &format!(
                    "WITH vals AS (
                        SELECT value AS val FROM node_field_readings
                        WHERE hostname = ?1 AND node_key = ?2 AND field = ?3
                          AND ts >= ?4 AND ts < ?5
                          AND value IS NOT NULL
                     ),{SUMMARY_TAIL}"
                ),
                params![hostname, node_key, field, from_ts, to_ts],
                summary_from_row,
            )
            .map_err(Into::into)
    }

    /// One Beacon field over `[from_ts, to_ts)`.
    ///
    /// `column` picks the `eye_readings` column by [`EyeField`], not by a
    /// bound parameter — SQLite cannot parameterise an identifier, and a
    /// caller-supplied string here would be an injection seam. A closed
    /// two-variant enum makes the identifier unreachable from a request.
    ///
    /// `humidity_pct` is stored as an INTEGER and cast to REAL, so an
    /// even-count median lands between two integers rather than truncating.
    pub fn summarize_eye_field(
        &self,
        hostname: &str,
        mac: &str,
        column: EyeField,
        from_ts: i64,
        to_ts: i64,
    ) -> Result<FieldSummary> {
        let expr = match column {
            EyeField::Temperature => "temperature_c",
            EyeField::Humidity => "CAST(humidity_pct AS REAL)",
        };
        self.conn
            .query_row(
                &format!(
                    "WITH vals AS (
                        SELECT {expr} AS val FROM eye_readings
                        WHERE hostname = ?1 AND mac = ?2
                          AND ts >= ?3 AND ts < ?4
                          AND {expr} IS NOT NULL
                     ),{SUMMARY_TAIL}"
                ),
                params![hostname, mac, from_ts, to_ts],
                summary_from_row,
            )
            .map_err(Into::into)
    }
}

/// Which of `eye_readings`' two numeric columns to summarise. A closed enum
/// rather than a string, so the SQL identifier it selects can never come
/// from a request.
/// Which stored series an alarm's peak is read from — the same three
/// sources, and the same value column per source, as the summaries above.
#[derive(Debug, Clone, Copy)]
pub enum PeakSource<'a> {
    Wired { line: i64 },
    Node { node_key: &'a str, field: &'a str },
    Eye { mac: &'a str, field: EyeField },
}

impl Storage {
    /// The highest (or, for a LOW alarm, lowest) stored value in
    /// `[from_ts, to_ts)` and when it was stored — an alarm's peak
    /// (decisions.md #24). `None` when the window holds no reading. The
    /// alarm row cannot answer this itself: it only sees the values its
    /// transitions carried, not the readings in between.
    pub fn peak_in_window(
        &self,
        hostname: &str,
        source: PeakSource<'_>,
        from_ts: i64,
        to_ts: i64,
        highest: bool,
    ) -> Result<Option<(f64, i64)>> {
        let order = if highest { "DESC" } else { "ASC" };
        let row = |r: &Row<'_>| Ok((r.get(0)?, r.get(1)?));
        match source {
            PeakSource::Wired { line } => self.conn.query_row(
                &format!(
                    "SELECT avg_c, minute_ts FROM readings_minute
                     WHERE hostname = ?1 AND sensor_line = ?2
                       AND minute_ts >= ?3 AND minute_ts < ?4 AND avg_c IS NOT NULL
                     ORDER BY avg_c {order}, minute_ts LIMIT 1"
                ),
                params![hostname, line, from_ts, to_ts],
                row,
            ),
            PeakSource::Node { node_key, field } => self.conn.query_row(
                &format!(
                    "SELECT value, ts FROM node_field_readings
                     WHERE hostname = ?1 AND node_key = ?2 AND field = ?3
                       AND ts >= ?4 AND ts < ?5 AND value IS NOT NULL
                     ORDER BY value {order}, ts LIMIT 1"
                ),
                params![hostname, node_key, field, from_ts, to_ts],
                row,
            ),
            PeakSource::Eye { mac, field } => {
                let expr = match field {
                    EyeField::Temperature => "temperature_c",
                    EyeField::Humidity => "CAST(humidity_pct AS REAL)",
                };
                self.conn.query_row(
                    &format!(
                        "SELECT {expr}, ts FROM eye_readings
                         WHERE hostname = ?1 AND mac = ?2
                           AND ts >= ?3 AND ts < ?4 AND {expr} IS NOT NULL
                         ORDER BY {expr} {order}, ts LIMIT 1"
                    ),
                    params![hostname, mac, from_ts, to_ts],
                    row,
                )
            }
        }
        .optional()
        .map_err(Into::into)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EyeField {
    Temperature,
    Humidity,
}

impl EyeField {
    /// The registry field name this column backs — `"temperature"` /
    /// `"humidity"` — so a caller iterating `eye_registry()` can map a
    /// field onto a column without a second match arm of its own.
    pub fn from_field_name(name: &str) -> Option<EyeField> {
        match name {
            "temperature" => Some(EyeField::Temperature),
            "humidity" => Some(EyeField::Humidity),
            _ => None,
        }
    }
}
