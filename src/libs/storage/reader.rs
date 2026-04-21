//! Read operations for querying stored data
//! Retrieve sensor readings, alarm events, and statistics

use rusqlite::Connection;

use crate::libs::storage::error::{StorageError, StorageResult};
use crate::libs::storage::models::{AlarmEvent, SensorReading, StorageStats};

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
                "SELECT id, timestamp, sensor_line, temperature_c, is_connected, alarm_state, created_at
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
                    data_hmac: None,
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
                "SELECT id, timestamp, sensor_line, temperature_c, is_connected, alarm_state, created_at
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
                        data_hmac: None,
                    })
                },
            )
            .map_err(|e| StorageError::QueryError(format!("Failed to query: {}", e)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::QueryError(format!("Failed to collect results: {}", e)))?;

        Ok(readings)
    }

    /// Get latest reading for all sensors
    pub fn get_latest_readings_all_sensors(conn: &Connection) -> StorageResult<Vec<SensorReading>> {
        let mut stmt = conn
            .prepare(
                "SELECT id, timestamp, sensor_line, temperature_c, is_connected, alarm_state, created_at
                 FROM sensor_readings
                 WHERE (sensor_line, timestamp) IN (
                     SELECT sensor_line, MAX(timestamp) FROM sensor_readings GROUP BY sensor_line
                 )
                 ORDER BY sensor_line",
            )
            .map_err(|e| StorageError::QueryError(format!("Failed to prepare query: {}", e)))?;

        let readings = stmt
            .query_map([], |row| {
                Ok(SensorReading {
                    id: row.get(0)?,
                    timestamp: row.get(1)?,
                    sensor_line: row.get(2)?,
                    temperature_c: row.get(3)?,
                    is_connected: row.get::<_, i32>(4)? != 0,
                    alarm_state: row.get(5)?,
                    created_at: row.get(6)?,
                    data_hmac: None,
                })
            })
            .map_err(|e| StorageError::QueryError(format!("Failed to query: {}", e)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::QueryError(format!("Failed to collect results: {}", e)))?;

        Ok(readings)
    }

    /// Get alarm events for a specific sensor in time range
    pub fn get_alarm_events(
        conn: &Connection,
        sensor_line: u8,
        from_timestamp: i64,
        to_timestamp: i64,
    ) -> StorageResult<Vec<AlarmEvent>> {
        let mut stmt = conn
            .prepare(
                "SELECT id, timestamp, sensor_line, from_state, to_state, temperature_c, details
                 FROM alarm_events
                 WHERE sensor_line = ? AND timestamp >= ? AND timestamp <= ?
                 ORDER BY timestamp DESC",
            )
            .map_err(|e| StorageError::QueryError(format!("Failed to prepare query: {}", e)))?;

        let events = stmt
            .query_map(
                rusqlite::params![sensor_line, from_timestamp, to_timestamp],
                |row| {
                    Ok(AlarmEvent {
                        id: row.get(0)?,
                        timestamp: row.get(1)?,
                        sensor_line: row.get(2)?,
                        from_state: row.get(3)?,
                        to_state: row.get(4)?,
                        temperature_c: row.get(5)?,
                        details: row.get(6)?,
                    })
                },
            )
            .map_err(|e| StorageError::QueryError(format!("Failed to query: {}", e)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::QueryError(format!("Failed to collect results: {}", e)))?;

        Ok(events)
    }

    /// Get all alarm events across all sensors
    pub fn get_all_alarm_events(
        conn: &Connection,
        from_timestamp: i64,
        to_timestamp: i64,
    ) -> StorageResult<Vec<AlarmEvent>> {
        let mut stmt = conn
            .prepare(
                "SELECT id, timestamp, sensor_line, from_state, to_state, temperature_c, details
                 FROM alarm_events
                 WHERE timestamp >= ? AND timestamp <= ?
                 ORDER BY timestamp DESC",
            )
            .map_err(|e| StorageError::QueryError(format!("Failed to prepare query: {}", e)))?;

        let events = stmt
            .query_map(rusqlite::params![from_timestamp, to_timestamp], |row| {
                Ok(AlarmEvent {
                    id: row.get(0)?,
                    timestamp: row.get(1)?,
                    sensor_line: row.get(2)?,
                    from_state: row.get(3)?,
                    to_state: row.get(4)?,
                    temperature_c: row.get(5)?,
                    details: row.get(6)?,
                })
            })
            .map_err(|e| StorageError::QueryError(format!("Failed to query: {}", e)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StorageError::QueryError(format!("Failed to collect results: {}", e)))?;

        Ok(events)
    }

    /// Get temperature statistics for a sensor
    pub fn get_sensor_stats(
        conn: &Connection,
        sensor_line: u8,
        from_timestamp: i64,
        to_timestamp: i64,
    ) -> StorageResult<SensorStats> {
        let row = conn
            .query_row(
                "SELECT
                    COUNT(*) as count,
                    AVG(temperature_c) as avg_temp,
                    MIN(temperature_c) as min_temp,
                    MAX(temperature_c) as max_temp
                 FROM sensor_readings
                 WHERE sensor_line = ? AND timestamp >= ? AND timestamp <= ?",
                rusqlite::params![sensor_line, from_timestamp, to_timestamp],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, f32>(1)?,
                        row.get::<_, f32>(2)?,
                        row.get::<_, f32>(3)?,
                    ))
                },
            )
            .map_err(|e| StorageError::QueryError(
                format!("Failed to query sensor stats: {}", e),
            ))?;

        Ok(SensorStats {
            count: row.0,
            avg_temp: row.1,
            min_temp: row.2,
            max_temp: row.3,
        })
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
}

/// Temperature sensor statistics
#[derive(Debug, Clone)]
pub struct SensorStats {
    /// Number of readings in the period
    pub count: i64,

    /// Average temperature in Celsius
    pub avg_temp: f32,

    /// Minimum temperature in Celsius
    pub min_temp: f32,

    /// Maximum temperature in Celsius
    pub max_temp: f32,
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

        StorageWriter::write_sensor_reading(&conn, &reading1).expect("Failed to write");
        StorageWriter::write_sensor_reading(&conn, &reading2).expect("Failed to write");

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

        StorageWriter::write_sensor_reading(&conn, &reading1).expect("Failed to write");
        StorageWriter::write_sensor_reading(&conn, &reading2).expect("Failed to write");
        StorageWriter::write_sensor_reading(&conn, &reading3).expect("Failed to write");

        let results = StorageReader::get_readings_in_range(&conn, 0, 1200, 1800).expect("Failed to read");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].timestamp, 1500);

        let _ = std::fs::remove_file("/tmp/test_range.db");
    }
}
