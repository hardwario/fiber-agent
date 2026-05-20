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
    /// If hmac_secret is provided, computes HMAC-SHA256 over reading data (EU MDR IEC 62304 §5.5.3)
    pub fn write_sensor_reading(
        conn: &Connection,
        reading: &SensorReading,
        hmac_secret: Option<&[u8]>,
    ) -> StorageResult<i64> {
        let start = std::time::Instant::now();

        let data_hmac = hmac_secret.map(|secret| {
            crate::libs::storage::integrity::compute_reading_hmac(
                secret,
                reading.timestamp,
                reading.sensor_line,
                reading.temperature_c,
                reading.is_connected,
                &reading.alarm_state,
            )
        });

        let result = conn.execute(
            "INSERT INTO sensor_readings (timestamp, sensor_line, temperature_c, is_connected, alarm_state, created_at, data_hmac)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                reading.timestamp,
                reading.sensor_line,
                reading.temperature_c,
                if reading.is_connected { 1 } else { 0 },
                reading.alarm_state,
                reading.created_at,
                data_hmac,
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
    /// If hmac_secret is provided, computes HMAC-SHA256 per reading (EU MDR IEC 62304 §5.5.3)
    pub fn write_sensor_readings_batch(
        conn: &mut Connection,
        readings: &[SensorReading],
        hmac_secret: Option<&[u8]>,
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
            let data_hmac = hmac_secret.map(|secret| {
                crate::libs::storage::integrity::compute_reading_hmac(
                    secret,
                    reading.timestamp,
                    reading.sensor_line,
                    reading.temperature_c,
                    reading.is_connected,
                    &reading.alarm_state,
                )
            });

            tx.execute(
                "INSERT INTO sensor_readings (timestamp, sensor_line, temperature_c, is_connected, alarm_state, created_at, data_hmac)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    reading.timestamp,
                    reading.sensor_line,
                    reading.temperature_c,
                    if reading.is_connected { 1 } else { 0 },
                    reading.alarm_state,
                    reading.created_at,
                    data_hmac,
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

    /// Insert a sticker reading (LoRaWAN uplink or sticker_removed marker).
    ///
    /// Idempotent on `message_id`: returns `Ok(Some(rowid))` on insert and
    /// `Ok(None)` if a row with the same `message_id` already exists. This
    /// lets the save-and-feed write path be retried freely (e.g. by the
    /// LoRaWAN monitor on transient failures) without creating duplicates.
    pub fn write_sticker_reading(
        conn: &mut Connection,
        dev_eui: &str,
        provisioning_epoch: i64,
        ts: i64,
        received_at: i64,
        message_id: &str,
        event_type: &str,
        payload_json: &str,
    ) -> StorageResult<Option<i64>> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let res = conn.execute(
            "INSERT OR IGNORE INTO sticker_readings
             (dev_eui, provisioning_epoch, ts, received_at, message_id, event_type, payload_json, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                dev_eui, provisioning_epoch, ts, received_at,
                message_id, event_type, payload_json, now,
            ],
        )
        .map_err(|e| StorageError::InsertError(format!("Failed to insert sticker reading: {}", e)))?;

        if res == 0 {
            Ok(None)
        } else {
            Ok(Some(conn.last_insert_rowid()))
        }
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
        let result = StorageWriter::write_sensor_reading(&conn, &reading, None);

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
        let mut conn = db.connect().expect("Failed to connect");

        let readings = vec![
            SensorReading::new(1000, 0, 36.5, true, AlarmState::Normal),
            SensorReading::new(1001, 1, 36.6, true, AlarmState::Normal),
            SensorReading::new(1002, 2, 36.7, true, AlarmState::Normal),
        ];

        let result = StorageWriter::write_sensor_readings_batch(&mut conn, &readings, None);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 3);

        let count = StorageWriter::get_sensor_reading_count(&conn).expect("Failed to count");
        assert_eq!(count, 3);

        let _ = std::fs::remove_file("/tmp/test_batch.db");
    }

    #[test]
    fn test_write_sensor_reading_with_hmac() {
        let db = Database::new("/tmp/test_write_hmac.db", 5).expect("Failed to create test DB");
        let conn = db.connect().expect("Failed to connect");

        let secret = b"test_hmac_secret_key_for_eu_mdr";
        let reading = SensorReading::new(1000, 0, 36.5, true, AlarmState::Normal);
        let result = StorageWriter::write_sensor_reading(&conn, &reading, Some(secret));
        assert!(result.is_ok());

        // Verify HMAC was stored in the database
        let stored_hmac: Option<String> = conn
            .query_row(
                "SELECT data_hmac FROM sensor_readings WHERE id = ?",
                [result.unwrap()],
                |row| row.get(0),
            )
            .expect("Failed to query HMAC");

        assert!(stored_hmac.is_some(), "HMAC should be stored when secret is provided");
        let hmac_value = stored_hmac.unwrap();
        assert_eq!(hmac_value.len(), 64, "HMAC-SHA256 hex digest should be 64 chars");

        // Verify the stored HMAC matches what we'd compute independently
        let expected_hmac = crate::libs::storage::integrity::compute_reading_hmac(
            secret,
            reading.timestamp,
            reading.sensor_line,
            reading.temperature_c,
            reading.is_connected,
            &reading.alarm_state,
        );
        assert_eq!(hmac_value, expected_hmac, "Stored HMAC should match computed HMAC");

        // Verify using the verify function for constant-time comparison
        assert!(crate::libs::storage::integrity::verify_reading_hmac(
            secret,
            reading.timestamp,
            reading.sensor_line,
            reading.temperature_c,
            reading.is_connected,
            &reading.alarm_state,
            &hmac_value,
        ));

        let _ = std::fs::remove_file("/tmp/test_write_hmac.db");
    }

    #[test]
    fn test_write_sensor_reading_without_hmac() {
        let db = Database::new("/tmp/test_write_no_hmac.db", 5).expect("Failed to create test DB");
        let conn = db.connect().expect("Failed to connect");

        let reading = SensorReading::new(1000, 0, 36.5, true, AlarmState::Normal);
        let result = StorageWriter::write_sensor_reading(&conn, &reading, None);
        assert!(result.is_ok());

        // Verify no HMAC was stored
        let stored_hmac: Option<String> = conn
            .query_row(
                "SELECT data_hmac FROM sensor_readings WHERE id = ?",
                [result.unwrap()],
                |row| row.get(0),
            )
            .expect("Failed to query HMAC");

        assert!(stored_hmac.is_none(), "HMAC should be None when no secret is provided");

        let _ = std::fs::remove_file("/tmp/test_write_no_hmac.db");
    }

    #[test]
    fn test_batch_write_with_hmac() {
        let db = Database::new("/tmp/test_batch_hmac.db", 5).expect("Failed to create test DB");
        let mut conn = db.connect().expect("Failed to connect");

        let secret = b"batch_test_secret";
        let readings = vec![
            SensorReading::new(1000, 0, 36.5, true, AlarmState::Normal),
            SensorReading::new(1001, 1, 36.6, true, AlarmState::Warning),
            SensorReading::new(1002, 2, 36.7, false, AlarmState::Disconnected),
        ];

        let result = StorageWriter::write_sensor_readings_batch(&mut conn, &readings, Some(secret));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 3);

        // Verify all readings have HMACs
        let hmac_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sensor_readings WHERE data_hmac IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .expect("Failed to count HMACs");
        assert_eq!(hmac_count, 3, "All batch readings should have HMACs");

        // Verify each HMAC is unique (different reading data = different HMAC)
        let hmacs: Vec<String> = conn
            .prepare("SELECT data_hmac FROM sensor_readings ORDER BY id")
            .expect("Failed to prepare")
            .query_map([], |row| row.get(0))
            .expect("Failed to query")
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(hmacs.len(), 3);
        assert_ne!(hmacs[0], hmacs[1], "Different readings should have different HMACs");
        assert_ne!(hmacs[1], hmacs[2], "Different readings should have different HMACs");

        let _ = std::fs::remove_file("/tmp/test_batch_hmac.db");
    }

    #[test]
    fn write_sticker_reading_inserts_and_is_idempotent_on_message_id() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 1).unwrap();
        let mut conn = db.connect().unwrap();

        let id1 = StorageWriter::write_sticker_reading(
            &mut conn,
            "70b3d5",
            2,
            1716120000,
            1716120001,
            "70b3d5-1716120000-7",
            "uplink",
            r#"{"fields":{"temp":21.0}}"#,
        )
        .unwrap();
        assert!(id1.is_some());

        // Same message_id again: must be a no-op, returns None.
        let id2 = StorageWriter::write_sticker_reading(
            &mut conn,
            "70b3d5",
            2,
            1716120000,
            1716120001,
            "70b3d5-1716120000-7",
            "uplink",
            r#"{"fields":{"temp":21.0}}"#,
        )
        .unwrap();
        assert!(id2.is_none(), "duplicate message_id must not insert a new row");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sticker_readings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
