//! Async write operations for storage
//! Handles insertion of sensor readings and alarm events

use rusqlite::Connection;

use crate::libs::storage::audit::AuditLogger;
use crate::libs::storage::error::{StorageError, StorageResult};
use crate::libs::storage::models::{AlarmEvent, SensorReading};

/// Writer for sensor readings and alarm events
pub struct StorageWriter;

impl StorageWriter {
    /// Write a sensor reading to the database
    /// Non-blocking: returns immediately
    pub fn write_sensor_reading(
        conn: &Connection,
        reading: &SensorReading,
    ) -> StorageResult<i64> {
        let start = std::time::Instant::now();

        let result = conn.execute(
            "INSERT INTO sensor_readings (timestamp, sensor_line, temperature_c, is_connected, alarm_state, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                reading.timestamp,
                reading.sensor_line,
                reading.temperature_c,
                if reading.is_connected { 1 } else { 0 },
                reading.alarm_state,
                reading.created_at,
            ],
        );

        match result {
            Ok(_) => {
                let duration_ms = start.elapsed().as_millis() as i64;
                let _ = AuditLogger::log_operation(conn, "INSERT", Some("sensor_readings"), Some(1), Some(duration_ms));
                Ok(conn.last_insert_rowid())
            }
            Err(e) => {
                let duration_ms = start.elapsed().as_millis() as i64;
                let _ = AuditLogger::log_error(
                    conn,
                    "INSERT",
                    Some("sensor_readings"),
                    &e.to_string(),
                    Some(duration_ms),
                );
                Err(StorageError::InsertError(format!("Failed to insert reading: {}", e)))
            }
        }
    }

    /// Write a batch of sensor readings
    /// More efficient than individual writes (wrapped in single transaction)
    pub fn write_sensor_readings_batch(
        conn: &mut Connection,
        readings: &[SensorReading],
    ) -> StorageResult<i64> {
        if readings.is_empty() {
            return Ok(0);
        }

        let start = std::time::Instant::now();

        let tx = conn
            .transaction()
            .map_err(|e| StorageError::InsertError(format!("Failed to start transaction: {}", e)))?;

        let mut inserted_count = 0i64;

        for reading in readings {
            tx.execute(
                "INSERT INTO sensor_readings (timestamp, sensor_line, temperature_c, is_connected, alarm_state, created_at)
                 VALUES (?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    reading.timestamp,
                    reading.sensor_line,
                    reading.temperature_c,
                    if reading.is_connected { 1 } else { 0 },
                    reading.alarm_state,
                    reading.created_at,
                ],
            )
            .map_err(|e| StorageError::InsertError(format!("Failed to insert batch reading: {}", e)))?;

            inserted_count += 1;
        }

        tx.commit()
            .map_err(|e| StorageError::InsertError(format!("Failed to commit batch: {}", e)))?;

        let duration_ms = start.elapsed().as_millis() as i64;
        let _ = AuditLogger::log_operation(
            conn,
            "INSERT",
            Some("sensor_readings"),
            Some(inserted_count),
            Some(duration_ms),
        );

        Ok(inserted_count)
    }

    /// Write an alarm event
    pub fn write_alarm_event(
        conn: &Connection,
        event: &AlarmEvent,
    ) -> StorageResult<i64> {
        let start = std::time::Instant::now();

        let result = conn.execute(
            "INSERT INTO alarm_events (timestamp, sensor_line, from_state, to_state, temperature_c, details)
             VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                event.timestamp,
                event.sensor_line,
                event.from_state,
                event.to_state,
                event.temperature_c,
                event.details,
            ],
        );

        match result {
            Ok(_) => {
                let duration_ms = start.elapsed().as_millis() as i64;
                let _ = AuditLogger::log_operation(conn, "INSERT", Some("alarm_events"), Some(1), Some(duration_ms));
                Ok(conn.last_insert_rowid())
            }
            Err(e) => {
                let duration_ms = start.elapsed().as_millis() as i64;
                let _ = AuditLogger::log_error(
                    conn,
                    "INSERT",
                    Some("alarm_events"),
                    &e.to_string(),
                    Some(duration_ms),
                );
                Err(StorageError::InsertError(format!("Failed to insert alarm event: {}", e)))
            }
        }
    }

    /// Write a batch of alarm events
    pub fn write_alarm_events_batch(
        conn: &mut Connection,
        events: &[AlarmEvent],
    ) -> StorageResult<i64> {
        if events.is_empty() {
            return Ok(0);
        }

        let start = std::time::Instant::now();

        let tx = conn
            .transaction()
            .map_err(|e| StorageError::InsertError(format!("Failed to start transaction: {}", e)))?;

        let mut inserted_count = 0i64;

        for event in events {
            tx.execute(
                "INSERT INTO alarm_events (timestamp, sensor_line, from_state, to_state, temperature_c, details)
                 VALUES (?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    event.timestamp,
                    event.sensor_line,
                    event.from_state,
                    event.to_state,
                    event.temperature_c,
                    event.details,
                ],
            )
            .map_err(|e| StorageError::InsertError(
                format!("Failed to insert batch alarm event: {}", e),
            ))?;

            inserted_count += 1;
        }

        tx.commit()
            .map_err(|e| StorageError::InsertError(format!("Failed to commit batch: {}", e)))?;

        let duration_ms = start.elapsed().as_millis() as i64;
        let _ = AuditLogger::log_operation(
            conn,
            "INSERT",
            Some("alarm_events"),
            Some(inserted_count),
            Some(duration_ms),
        );

        Ok(inserted_count)
    }

    /// Get count of sensor readings in database
    pub fn get_sensor_reading_count(conn: &Connection) -> StorageResult<i64> {
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sensor_readings", [], |row| row.get(0))
            .map_err(|e| StorageError::QueryError(format!("Failed to count readings: {}", e)))?;

        Ok(count)
    }

    /// Get count of alarm events in database
    pub fn get_alarm_event_count(conn: &Connection) -> StorageResult<i64> {
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM alarm_events", [], |row| row.get(0))
            .map_err(|e| StorageError::QueryError(format!("Failed to count events: {}", e)))?;

        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::alarms::AlarmState;
    use crate::libs::storage::db::Database;

    #[test]
    fn test_write_sensor_reading() {
        let db = Database::new("/tmp/test_write.db", 5).expect("Failed to create test DB");
        let conn = db.connect().expect("Failed to connect");

        let reading = SensorReading::new(1000, 0, 36.5, true, AlarmState::Normal);
        let result = StorageWriter::write_sensor_reading(&conn, &reading);

        assert!(result.is_ok());
        let count = StorageWriter::get_sensor_reading_count(&conn).expect("Failed to count");
        assert_eq!(count, 1);

        let _ = std::fs::remove_file("/tmp/test_write.db");
    }

    #[test]
    fn test_write_alarm_event() {
        let db = Database::new("/tmp/test_alarm_write.db", 5).expect("Failed to create test DB");
        let conn = db.connect().expect("Failed to connect");

        let event = AlarmEvent::new(1000, 0, AlarmState::Normal, AlarmState::Warning, Some(37.0));
        let result = StorageWriter::write_alarm_event(&conn, &event);

        assert!(result.is_ok());
        let count = StorageWriter::get_alarm_event_count(&conn).expect("Failed to count");
        assert_eq!(count, 1);

        let _ = std::fs::remove_file("/tmp/test_alarm_write.db");
    }

    #[test]
    fn test_batch_write_sensor_readings() {
        let db = Database::new("/tmp/test_batch.db", 5).expect("Failed to create test DB");
        let conn = db.connect().expect("Failed to connect");

        let readings = vec![
            SensorReading::new(1000, 0, 36.5, true, AlarmState::Normal),
            SensorReading::new(1001, 1, 36.6, true, AlarmState::Normal),
            SensorReading::new(1002, 2, 36.7, true, AlarmState::Normal),
        ];

        let result = StorageWriter::write_sensor_readings_batch(&conn, &readings);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 3);

        let count = StorageWriter::get_sensor_reading_count(&conn).expect("Failed to count");
        assert_eq!(count, 3);

        let _ = std::fs::remove_file("/tmp/test_batch.db");
    }
}
