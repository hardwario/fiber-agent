//! Per-minute aggregator for `sensor_readings`.
//!
//! Rolls closed minutes (i.e. minutes strictly older than `now`) of raw
//! `sensor_readings` rows into `sensor_readings_minute` so the raw table
//! can be retention-trimmed to ~30 days while the aggregate table provides
//! ~3 years of history within ~1GB of disk on the shared `/data` partition.
//!
//! Idempotent: `INSERT OR IGNORE` keyed on `(minute_ts, sensor_line)` means
//! re-running across the same window is a no-op. Safe to call as often as
//! desired; the storage thread calls it once per minute.

use rusqlite::Connection;

use crate::libs::storage::error::{StorageError, StorageResult};
use crate::libs::storage::integrity::compute_minute_aggregate_hmac;

/// Result of one aggregator pass.
#[derive(Debug, Clone, Default)]
pub struct AggregationStats {
    /// Number of `(minute_ts, sensor_line)` rows inserted by this pass.
    pub rows_inserted: i64,
    /// Earliest minute we touched on this pass (UNIX seconds), if any.
    pub from_minute: Option<i64>,
    /// Latest minute we touched on this pass (UNIX seconds), if any.
    pub to_minute: Option<i64>,
}

/// Aggregate all closed minutes that aren't yet in `sensor_readings_minute`.
///
/// A minute `m` is "closed" once `now_ts >= m + 60`. We only aggregate up to
/// `floor((now_ts - 60) / 60) * 60` to avoid double-counting an in-progress
/// minute. Returns stats so the storage thread can log a single line per
/// pass instead of one per minute.
pub fn aggregate_closed_minutes(
    conn: &mut Connection,
    now_ts: i64,
    hmac_secret: Option<&[u8]>,
) -> StorageResult<AggregationStats> {
    let cutoff_minute = ((now_ts - 60) / 60) * 60;

    let tx = conn.transaction().map_err(|e| {
        StorageError::InsertError(format!("aggregator: begin tx: {}", e))
    })?;

    // Insert per-minute aggregates for any closed minute not already present.
    // `worst_alarm` uses a severity ranking (CRITICAL > DISCONNECTED >
    // WARNING > RECONNECTING > NEVER_CONNECTED > NORMAL).
    let rows_inserted = tx
        .execute(
            "INSERT OR IGNORE INTO sensor_readings_minute
                 (minute_ts, sensor_line, min_c, avg_c, max_c,
                  sample_count, disconnect_count, worst_alarm,
                  created_at, data_hmac)
             SELECT
                 (timestamp / 60) * 60               AS minute_ts,
                 sensor_line                         AS sensor_line,
                 MIN(temperature_c)                  AS min_c,
                 AVG(temperature_c)                  AS avg_c,
                 MAX(temperature_c)                  AS max_c,
                 COUNT(*)                            AS sample_count,
                 SUM(CASE WHEN is_connected = 0 THEN 1 ELSE 0 END)
                                                     AS disconnect_count,
                 CASE MAX(CASE alarm_state
                              WHEN 'CRITICAL'        THEN 5
                              WHEN 'DISCONNECTED'    THEN 4
                              WHEN 'WARNING'         THEN 3
                              WHEN 'RECONNECTING'    THEN 2
                              WHEN 'NEVER_CONNECTED' THEN 1
                              WHEN 'NORMAL'          THEN 0
                              ELSE 0
                          END)
                     WHEN 5 THEN 'CRITICAL'
                     WHEN 4 THEN 'DISCONNECTED'
                     WHEN 3 THEN 'WARNING'
                     WHEN 2 THEN 'RECONNECTING'
                     WHEN 1 THEN 'NEVER_CONNECTED'
                     ELSE        'NORMAL'
                 END                                 AS worst_alarm,
                 ?1                                  AS created_at,
                 NULL                                AS data_hmac
             FROM sensor_readings
             WHERE timestamp < ?2
               AND timestamp >= COALESCE(
                   (SELECT MAX(minute_ts) + 60 FROM sensor_readings_minute),
                   0)
             GROUP BY (timestamp / 60), sensor_line",
            rusqlite::params![now_ts, cutoff_minute],
        )
        .map_err(|e| {
            StorageError::InsertError(format!("aggregator: insert: {}", e))
        })? as i64;

    // If we have an HMAC secret, fill in data_hmac for the rows we just
    // inserted (those with NULL hmac, bounded by the cutoff). Splitting it
    // out keeps the aggregate INSERT simple SQL; the HMAC needs the same
    // canonical byte layout we use elsewhere for tamper-evidence.
    if rows_inserted > 0 {
        if let Some(secret) = hmac_secret {
            let mut stmt = tx
                .prepare(
                    "SELECT minute_ts, sensor_line, min_c, avg_c, max_c,
                            sample_count, disconnect_count, worst_alarm
                     FROM sensor_readings_minute
                     WHERE data_hmac IS NULL AND minute_ts < ?1",
                )
                .map_err(|e| StorageError::QueryError(
                    format!("aggregator: prepare hmac scan: {}", e),
                ))?;
            let rows = stmt
                .query_map(rusqlite::params![cutoff_minute], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i32>(1)?,
                        row.get::<_, f64>(2)?,
                        row.get::<_, f64>(3)?,
                        row.get::<_, f64>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                })
                .map_err(|e| StorageError::QueryError(
                    format!("aggregator: query hmac scan: {}", e),
                ))?;
            let pending: Vec<(i64, i32, f64, f64, f64, i64, i64, String)> = rows
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StorageError::QueryError(
                    format!("aggregator: collect hmac scan: {}", e),
                ))?;
            drop(stmt);

            for (minute_ts, sensor_line, min_c, avg_c, max_c,
                 sample_count, disconnect_count, worst_alarm) in pending
            {
                let hmac = compute_minute_aggregate_hmac(
                    secret,
                    minute_ts,
                    sensor_line as u8,
                    min_c,
                    avg_c,
                    max_c,
                    sample_count,
                    disconnect_count,
                    &worst_alarm,
                );
                tx.execute(
                    "UPDATE sensor_readings_minute
                     SET data_hmac = ?1
                     WHERE minute_ts = ?2 AND sensor_line = ?3",
                    rusqlite::params![hmac, minute_ts, sensor_line],
                )
                .map_err(|e| StorageError::InsertError(
                    format!("aggregator: update hmac: {}", e),
                ))?;
            }
        }
    }

    // Capture the window we just covered for reporting.
    let (from_minute, to_minute): (Option<i64>, Option<i64>) = tx
        .query_row(
            "SELECT MIN(minute_ts), MAX(minute_ts)
             FROM sensor_readings_minute
             WHERE created_at = ?1",
            rusqlite::params![now_ts],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap_or((None, None));

    tx.commit().map_err(|e| {
        StorageError::InsertError(format!("aggregator: commit: {}", e))
    })?;

    Ok(AggregationStats {
        rows_inserted,
        from_minute,
        to_minute,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::storage::db::Database;
    use crate::libs::storage::models::SensorReading;
    use crate::libs::storage::writer::StorageWriter;
    use crate::libs::alarms::AlarmState;

    fn fresh_db() -> (Database, rusqlite::Connection) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::mem::forget(tmp); // keep file alive past NamedTempFile drop
        let db = Database::new(&path, 1).unwrap();
        let conn = db.connect().unwrap();
        (db, conn)
    }

    #[test]
    fn aggregates_one_minute_per_sensor() {
        let (_db, mut conn) = fresh_db();

        // Three readings inside minute 600 (i.e. timestamps 600..659), one sensor.
        for (offset, temp) in [(0, 20.0f32), (30, 22.0), (59, 24.0)] {
            let r = SensorReading::new(600 + offset, 0, temp, true, AlarmState::Normal);
            StorageWriter::write_sensor_reading(&conn, &r, None).unwrap();
        }
        // Force a reading in minute 660 so cutoff includes minute 600.
        let r = SensorReading::new(660, 0, 25.0, true, AlarmState::Normal);
        StorageWriter::write_sensor_reading(&conn, &r, None).unwrap();

        // now_ts = 720 -> cutoff = 660 -> only minute 600 is closed.
        let stats = aggregate_closed_minutes(&mut conn, 720, None).unwrap();
        assert_eq!(stats.rows_inserted, 1);

        let (min_c, avg_c, max_c, count): (f64, f64, f64, i64) = conn
            .query_row(
                "SELECT min_c, avg_c, max_c, sample_count
                 FROM sensor_readings_minute
                 WHERE minute_ts = 600 AND sensor_line = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert!((min_c - 20.0).abs() < 1e-3);
        assert!((avg_c - 22.0).abs() < 1e-3);
        assert!((max_c - 24.0).abs() < 1e-3);
        assert_eq!(count, 3);
    }

    #[test]
    fn rerunning_is_a_noop() {
        let (_db, mut conn) = fresh_db();
        for offset in [0, 30, 59] {
            let r = SensorReading::new(600 + offset, 0, 20.0, true, AlarmState::Normal);
            StorageWriter::write_sensor_reading(&conn, &r, None).unwrap();
        }
        let first = aggregate_closed_minutes(&mut conn, 720, None).unwrap();
        assert_eq!(first.rows_inserted, 1);
        let second = aggregate_closed_minutes(&mut conn, 720, None).unwrap();
        assert_eq!(second.rows_inserted, 0);
    }

    #[test]
    fn worst_alarm_picks_highest_severity() {
        let (_db, mut conn) = fresh_db();
        let states = [
            AlarmState::Normal,
            AlarmState::Warning,
            AlarmState::Critical,
            AlarmState::Normal,
        ];
        for (i, s) in states.iter().enumerate() {
            let r = SensorReading::new(600 + i as i64, 0, 20.0, true, *s);
            StorageWriter::write_sensor_reading(&conn, &r, None).unwrap();
        }
        aggregate_closed_minutes(&mut conn, 720, None).unwrap();

        let worst: String = conn
            .query_row(
                "SELECT worst_alarm FROM sensor_readings_minute WHERE minute_ts = 600",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(worst, "CRITICAL");
    }

    #[test]
    fn disconnect_count_counts_only_disconnects() {
        let (_db, mut conn) = fresh_db();
        for (offset, connected) in [(0, true), (10, false), (20, false), (30, true)] {
            let r = SensorReading::new(600 + offset, 0, 20.0, connected, AlarmState::Normal);
            StorageWriter::write_sensor_reading(&conn, &r, None).unwrap();
        }
        aggregate_closed_minutes(&mut conn, 720, None).unwrap();

        let (total, dc): (i64, i64) = conn
            .query_row(
                "SELECT sample_count, disconnect_count
                 FROM sensor_readings_minute WHERE minute_ts = 600",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(total, 4);
        assert_eq!(dc, 2);
    }

    #[test]
    fn writes_hmac_when_secret_provided() {
        let (_db, mut conn) = fresh_db();
        for offset in [0, 30] {
            let r = SensorReading::new(600 + offset, 0, 20.0, true, AlarmState::Normal);
            StorageWriter::write_sensor_reading(&conn, &r, None).unwrap();
        }
        let secret = b"test-secret-key";
        aggregate_closed_minutes(&mut conn, 720, Some(secret)).unwrap();

        let hmac: Option<Vec<u8>> = conn
            .query_row(
                "SELECT data_hmac FROM sensor_readings_minute WHERE minute_ts = 600",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let hmac = hmac.expect("HMAC should be populated");
        assert_eq!(hmac.len(), 32, "HMAC-SHA256 output is 32 bytes");
    }
}
