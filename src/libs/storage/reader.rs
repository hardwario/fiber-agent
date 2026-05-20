//! Read operations for querying stored data
//! Retrieve sensor readings, alarm events, and statistics

use rusqlite::{Connection, OptionalExtension};

use crate::libs::storage::error::{StorageError, StorageResult};
use crate::libs::storage::models::{AlarmEvent, SensorReading, StickerReadingRow, StorageStats};

/// Reader for querying sensor data
pub struct StorageReader;

impl StorageReader {
    /// Get the most recent N readings for a specific sensor
    pub fn get_last_readings(
        conn: &Connection,
        sensor_line: u8,
        count: usize,
    ) -> StorageResult<Vec<SensorReading>> {
        let mut stmt = conn
            .prepare(
                "SELECT id, timestamp, sensor_line, temperature_c, is_connected, alarm_state, created_at, data_hmac
                 FROM sensor_readings
                 WHERE sensor_line = ?
                 ORDER BY timestamp DESC
                 LIMIT ?",
            )
            .map_err(|e| StorageError::QueryError(format!("Failed to prepare query: {}", e)))?;

        let readings = stmt
            .query_map(rusqlite::params![sensor_line, count], |row| {
                Ok(SensorReading {
                    id: row.get(0)?,
                    timestamp: row.get(1)?,
                    sensor_line: row.get(2)?,
                    temperature_c: row.get(3)?,
                    is_connected: row.get::<_, i32>(4)? != 0,
                    alarm_state: row.get(5)?,
                    created_at: row.get(6)?,
                    data_hmac: row.get(7)?,
                })
            })
            .map_err(|e| StorageError::QueryError(format!("Failed to query: {}", e)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::QueryError(format!("Failed to collect results: {}", e)))?;

        Ok(readings)
    }

    /// Get readings in a time range for a specific sensor
    pub fn get_readings_in_range(
        conn: &Connection,
        sensor_line: u8,
        from_timestamp: i64,
        to_timestamp: i64,
    ) -> StorageResult<Vec<SensorReading>> {
        let mut stmt = conn
            .prepare(
                "SELECT id, timestamp, sensor_line, temperature_c, is_connected, alarm_state, created_at, data_hmac
                 FROM sensor_readings
                 WHERE sensor_line = ? AND timestamp >= ? AND timestamp <= ?
                 ORDER BY timestamp DESC",
            )
            .map_err(|e| StorageError::QueryError(format!("Failed to prepare query: {}", e)))?;

        let readings = stmt
            .query_map(
                rusqlite::params![sensor_line, from_timestamp, to_timestamp],
                |row| {
                    Ok(SensorReading {
                        id: row.get(0)?,
                        timestamp: row.get(1)?,
                        sensor_line: row.get(2)?,
                        temperature_c: row.get(3)?,
                        is_connected: row.get::<_, i32>(4)? != 0,
                        alarm_state: row.get(5)?,
                        created_at: row.get(6)?,
                        data_hmac: row.get(7)?,
                    })
                },
            )
            .map_err(|e| StorageError::QueryError(format!("Failed to query: {}", e)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::QueryError(format!("Failed to collect results: {}", e)))?;

        Ok(readings)
    }

    /// Get overall storage statistics
    pub fn get_storage_stats(
        conn: &Connection,
        db_path: &str,
    ) -> StorageResult<StorageStats> {
        let total_readings: i64 = conn
            .query_row("SELECT COUNT(*) FROM sensor_readings", [], |row| row.get(0))
            .map_err(|e| StorageError::QueryError(
                format!("Failed to count readings: {}", e),
            ))?;

        let total_alarm_events: i64 = conn
            .query_row("SELECT COUNT(*) FROM alarm_events", [], |row| row.get(0))
            .map_err(|e| StorageError::QueryError(
                format!("Failed to count alarm events: {}", e),
            ))?;

        let total_audit_entries: i64 = conn
            .query_row("SELECT COUNT(*) FROM audit_log", [], |row| row.get(0))
            .unwrap_or(0); // Audit table might not exist in all cases

        let db_size_bytes = std::fs::metadata(db_path)
            .map(|m| m.len() as i64)
            .unwrap_or(0);

        let (oldest, newest): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT MIN(timestamp), MAX(timestamp) FROM sensor_readings",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|e| StorageError::QueryError(
                format!("Failed to query timestamp range: {}", e),
            ))?;

        Ok(StorageStats {
            total_readings,
            total_alarm_events,
            total_audit_entries,
            db_size_bytes,
            oldest_reading_timestamp: oldest,
            newest_reading_timestamp: newest,
            db_path: db_path.to_string(),
        })
    }

    /// Fetch sticker readings with `id > last_id`, ordered by id ascending,
    /// up to `limit` rows. Used by the export drain loop to consume rows
    /// past the per-(broker, stream) cursor.
    pub fn fetch_sticker_readings_after(
        conn: &Connection,
        last_id: i64,
        limit: usize,
    ) -> StorageResult<Vec<StickerReadingRow>> {
        let mut stmt = conn
            .prepare(
                "SELECT id, dev_eui, provisioning_epoch, ts, received_at, message_id,
                        event_type, payload_json, created_at
                 FROM sticker_readings
                 WHERE id > ?
                 ORDER BY id ASC
                 LIMIT ?",
            )
            .map_err(|e| StorageError::QueryError(format!("prepare sticker_readings_after: {}", e)))?;

        let rows = stmt
            .query_map(rusqlite::params![last_id, limit as i64], |r| {
                Ok(StickerReadingRow {
                    id: r.get(0)?,
                    dev_eui: r.get(1)?,
                    provisioning_epoch: r.get(2)?,
                    ts: r.get(3)?,
                    received_at: r.get(4)?,
                    message_id: r.get(5)?,
                    event_type: r.get(6)?,
                    payload_json: r.get(7)?,
                    created_at: r.get(8)?,
                })
            })
            .map_err(|e| StorageError::QueryError(format!("query sticker_readings_after: {}", e)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::QueryError(format!("collect sticker_readings_after: {}", e)))?;

        Ok(rows)
    }

    /// Fetch sensor readings with `id > last_id`, ordered by id ascending,
    /// up to `limit` rows. Companion to `fetch_sticker_readings_after`,
    /// consumed by the probe-stream export drain.
    pub fn fetch_sensor_readings_after(
        conn: &Connection,
        last_id: i64,
        limit: usize,
    ) -> StorageResult<Vec<SensorReading>> {
        let mut stmt = conn
            .prepare(
                "SELECT id, timestamp, sensor_line, temperature_c, is_connected,
                        alarm_state, created_at, data_hmac
                 FROM sensor_readings
                 WHERE id > ?
                 ORDER BY id ASC
                 LIMIT ?",
            )
            .map_err(|e| StorageError::QueryError(format!("prepare sensor_readings_after: {}", e)))?;

        let rows = stmt
            .query_map(rusqlite::params![last_id, limit as i64], |r| {
                Ok(SensorReading {
                    id: r.get(0)?,
                    timestamp: r.get(1)?,
                    sensor_line: r.get::<_, i64>(2)? as u8,
                    temperature_c: r.get(3)?,
                    is_connected: r.get(4)?,
                    alarm_state: r.get(5)?,
                    created_at: r.get(6)?,
                    data_hmac: r.get(7)?,
                })
            })
            .map_err(|e| StorageError::QueryError(format!("query sensor_readings_after: {}", e)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::QueryError(format!("collect sensor_readings_after: {}", e)))?;

        Ok(rows)
    }

    /// Look up the current export cursor for a `(broker_id, stream)` pair.
    /// Returns 0 if no cursor row exists yet — semantically "haven't exported
    /// anything yet", since `sticker_readings.id`/`sensor_readings.id`/
    /// `alarm_events.id` are all `AUTOINCREMENT` starting at 1.
    pub fn load_export_cursor(
        conn: &Connection,
        broker_id: &str,
        stream: &str,
    ) -> StorageResult<i64> {
        let mut stmt = conn
            .prepare("SELECT last_exported_id FROM export_cursor WHERE broker_id = ? AND stream = ?")
            .map_err(|e| StorageError::QueryError(format!("prepare cursor: {}", e)))?;
        let v: Option<i64> = stmt
            .query_row(rusqlite::params![broker_id, stream], |r| r.get(0))
            .optional()
            .map_err(|e| StorageError::QueryError(format!("query cursor: {}", e)))?;
        Ok(v.unwrap_or(0))
    }

    /// Fetch alarm events with `id > last_id`, ordered by id ascending,
    /// up to `limit` rows. Used by the alarm-stream export drain.
    pub fn fetch_alarm_events_after(
        conn: &Connection,
        last_id: i64,
        limit: usize,
    ) -> StorageResult<Vec<AlarmEvent>> {
        let mut stmt = conn
            .prepare(
                "SELECT id, timestamp, sensor_line, from_state, to_state, temperature_c, details
                 FROM alarm_events
                 WHERE id > ?
                 ORDER BY id ASC
                 LIMIT ?",
            )
            .map_err(|e| StorageError::QueryError(format!("prepare alarm_events_after: {}", e)))?;

        let rows = stmt
            .query_map(rusqlite::params![last_id, limit as i64], |r| {
                Ok(AlarmEvent {
                    id: r.get(0)?,
                    timestamp: r.get(1)?,
                    sensor_line: r.get::<_, i64>(2)? as u8,
                    from_state: r.get(3)?,
                    to_state: r.get(4)?,
                    temperature_c: r.get(5)?,
                    details: r.get(6)?,
                })
            })
            .map_err(|e| StorageError::QueryError(format!("query alarm_events_after: {}", e)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::QueryError(format!("collect alarm_events_after: {}", e)))?;

        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::alarms::AlarmState;
    use crate::libs::storage::db::Database;
    use crate::libs::storage::writer::StorageWriter;

    #[test]
    fn test_get_last_readings() {
        let db = Database::new("/tmp/test_read.db", 5).expect("Failed to create test DB");
        let conn = db.connect().expect("Failed to connect");

        let reading1 = SensorReading::new(1000, 0, 36.5, true, AlarmState::Normal);
        let reading2 = SensorReading::new(1001, 0, 36.6, true, AlarmState::Normal);

        StorageWriter::write_sensor_reading(&conn, &reading1, None).expect("Failed to write");
        StorageWriter::write_sensor_reading(&conn, &reading2, None).expect("Failed to write");

        let results = StorageReader::get_last_readings(&conn, 0, 10).expect("Failed to read");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].temperature_c, 36.6); // Most recent first

        let _ = std::fs::remove_file("/tmp/test_read.db");
    }

    #[test]
    fn test_get_readings_in_range() {
        let db = Database::new("/tmp/test_range.db", 5).expect("Failed to create test DB");
        let conn = db.connect().expect("Failed to connect");

        let reading1 = SensorReading::new(1000, 0, 36.5, true, AlarmState::Normal);
        let reading2 = SensorReading::new(1500, 0, 36.6, true, AlarmState::Normal);
        let reading3 = SensorReading::new(2000, 0, 36.7, true, AlarmState::Normal);

        StorageWriter::write_sensor_reading(&conn, &reading1, None).expect("Failed to write");
        StorageWriter::write_sensor_reading(&conn, &reading2, None).expect("Failed to write");
        StorageWriter::write_sensor_reading(&conn, &reading3, None).expect("Failed to write");

        let results = StorageReader::get_readings_in_range(&conn, 0, 1200, 1800).expect("Failed to read");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].timestamp, 1500);

        let _ = std::fs::remove_file("/tmp/test_range.db");
    }

    #[test]
    fn fetch_sensor_and_alarm_after_returns_in_id_order() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 1).unwrap();
        let conn = db.connect().unwrap();

        for i in 0..3 {
            let reading = SensorReading::new(1000 + i, 0, 21.0, true, AlarmState::Normal);
            StorageWriter::write_sensor_reading(&conn, &reading, None).unwrap();

            let event = AlarmEvent::new(
                1000 + i,
                0,
                AlarmState::Normal,
                AlarmState::Warning,
                Some(21.0),
            );
            StorageWriter::write_alarm_event(&conn, &event).unwrap();
        }

        let s = StorageReader::fetch_sensor_readings_after(&conn, 1, 10).unwrap();
        assert_eq!(s.iter().map(|r| r.id).collect::<Vec<_>>(), vec![2, 3]);

        let a = StorageReader::fetch_alarm_events_after(&conn, 0, 10).unwrap();
        assert_eq!(a.iter().map(|r| r.id).collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn fetch_sticker_readings_after_returns_only_above_cursor_in_order() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 1).unwrap();
        let mut conn = db.connect().unwrap();

        for i in 0..5 {
            StorageWriter::write_sticker_reading(
                &mut conn,
                "abc",
                1,
                1000 + i,
                1000 + i,
                &format!("abc-{}-0", 1000 + i),
                "uplink",
                "{}",
            )
            .unwrap();
        }

        let rows = StorageReader::fetch_sticker_readings_after(&conn, 2, 10).unwrap();
        assert_eq!(rows.len(), 3, "expected ids 3,4,5 (cursor=2)");
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![3, 4, 5]);

        let limited = StorageReader::fetch_sticker_readings_after(&conn, 0, 2).unwrap();
        assert_eq!(limited.len(), 2);
    }
}
