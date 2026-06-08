//! Async write operations for storage
//! Handles insertion of sensor readings and alarm events

use rusqlite::{Connection, OptionalExtension};

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
            Ok(_) => Ok(conn.last_insert_rowid()),
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

    /// Advance the export cursor for `(broker_id, stream)` to `new_id`.
    /// Monotonic: if `new_id <= current_cursor`, the cursor is left unchanged.
    /// This makes the drain loop safe against out-of-order PUBACKs or retries.
    pub fn advance_export_cursor(
        conn: &mut Connection,
        broker_id: &str,
        stream: &str,
        new_id: i64,
    ) -> StorageResult<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO export_cursor (broker_id, stream, last_exported_id, updated_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(broker_id, stream) DO UPDATE SET
                 last_exported_id = MAX(last_exported_id, excluded.last_exported_id),
                 updated_at       = excluded.updated_at",
            rusqlite::params![broker_id, stream, new_id, now],
        )
        .map_err(|e| StorageError::InsertError(format!("advance cursor: {}", e)))?;
        Ok(())
    }

    /// Reset the export cursor for `(broker_id, stream)` to 0 so the next
    /// drain pass replays every row in the underlying stream. Idempotent.
    pub fn reset_export_cursor(
        conn: &mut Connection,
        broker_id: &str,
        stream: &str,
    ) -> StorageResult<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO export_cursor (broker_id, stream, last_exported_id, updated_at)
             VALUES (?, ?, 0, ?)
             ON CONFLICT(broker_id, stream) DO UPDATE SET
                 last_exported_id = 0,
                 updated_at       = excluded.updated_at",
            rusqlite::params![broker_id, stream, now],
        )
        .map_err(|e| StorageError::InsertError(format!("reset cursor: {}", e)))?;
        Ok(())
    }

    /// Look up the current provisioning epoch for a `dev_eui`. Returns 1 if
    /// the dev_eui has never been provisioned (default epoch). Used by the
    /// LoRaWAN monitor to stamp every uplink with the active epoch so a
    /// sticker that was removed and re-provisioned does not look identical
    /// to its previous incarnation in downstream replays.
    pub fn get_provisioning_epoch(conn: &Connection, dev_eui: &str) -> StorageResult<i64> {
        let mut stmt = conn
            .prepare("SELECT epoch FROM sticker_provisioning_epoch WHERE dev_eui = ?")
            .map_err(|e| StorageError::QueryError(format!("prepare provisioning epoch: {}", e)))?;
        let epoch: Option<i64> = stmt
            .query_row(rusqlite::params![dev_eui], |r| r.get(0))
            .optional()
            .map_err(|e| StorageError::QueryError(format!("query provisioning epoch: {}", e)))?;
        Ok(epoch.unwrap_or(1))
    }

    /// Atomically bump the provisioning epoch for a `dev_eui`. If no row
    /// exists for the dev_eui, inserts at epoch=2 (a brand-new sticker that
    /// has never sent traffic yet is implicitly at epoch 1; the first bump
    /// signals re-provisioning and lands at 2). Otherwise increments by one.
    pub fn bump_provisioning_epoch(conn: &mut Connection, dev_eui: &str) -> StorageResult<i64> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let tx = conn
            .transaction()
            .map_err(|e| StorageError::QueryError(format!("tx: {}", e)))?;
        tx.execute(
            "INSERT INTO sticker_provisioning_epoch (dev_eui, epoch, updated_at)
             VALUES (?, 2, ?)
             ON CONFLICT(dev_eui) DO UPDATE SET epoch = epoch + 1, updated_at = ?",
            rusqlite::params![dev_eui, now, now],
        )
        .map_err(|e| StorageError::InsertError(format!("bump_provisioning_epoch: {}", e)))?;
        let epoch: i64 = tx
            .query_row(
                "SELECT epoch FROM sticker_provisioning_epoch WHERE dev_eui = ?",
                rusqlite::params![dev_eui],
                |r| r.get(0),
            )
            .map_err(|e| StorageError::QueryError(format!("read epoch: {}", e)))?;
        tx.commit()
            .map_err(|e| StorageError::QueryError(format!("commit: {}", e)))?;
        Ok(epoch)
    }

    /// Append a `sticker_removed` marker event for a sticker. The marker is
    /// stored under the current provisioning epoch so that downstream readers
    /// can distinguish "this sticker was deprovisioned" from a subsequent
    /// re-provisioned incarnation. The `message_id` is derived from the
    /// `dev_eui` and `ts`, so calling this twice with the same timestamp is
    /// a no-op (same idempotency story as `write_sticker_reading`).
    pub fn append_sticker_removed_event(
        conn: &mut Connection,
        dev_eui: &str,
        ts: i64,
    ) -> StorageResult<Option<i64>> {
        let epoch = Self::get_provisioning_epoch(conn, dev_eui)?;
        let message_id = format!("{}-{}-removed", dev_eui, ts);
        Self::write_sticker_reading(
            conn,
            dev_eui,
            epoch,
            ts,
            ts,
            &message_id,
            "sticker_removed",
            "{}",
        )
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

    #[test]
    fn provisioning_epoch_starts_at_one_and_bumps() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 1).unwrap();
        let mut conn = db.connect().unwrap();

        assert_eq!(StorageWriter::get_provisioning_epoch(&conn, "abc").unwrap(), 1);

        let v = StorageWriter::bump_provisioning_epoch(&mut conn, "abc").unwrap();
        assert_eq!(v, 2);

        let v = StorageWriter::bump_provisioning_epoch(&mut conn, "abc").unwrap();
        assert_eq!(v, 3);

        // Different dev_eui starts fresh
        assert_eq!(StorageWriter::get_provisioning_epoch(&conn, "def").unwrap(), 1);
    }

    #[test]
    fn cursor_load_default_is_zero_then_advance_updates() {
        use crate::libs::storage::reader::StorageReader;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 1).unwrap();
        let mut conn = db.connect().unwrap();

        assert_eq!(StorageReader::load_export_cursor(&conn, "remote", "sticker").unwrap(), 0);

        StorageWriter::advance_export_cursor(&mut conn, "remote", "sticker", 42).unwrap();
        assert_eq!(StorageReader::load_export_cursor(&conn, "remote", "sticker").unwrap(), 42);

        // Going backwards is rejected (no-op). Monotonic guarantee.
        StorageWriter::advance_export_cursor(&mut conn, "remote", "sticker", 41).unwrap();
        assert_eq!(StorageReader::load_export_cursor(&conn, "remote", "sticker").unwrap(), 42);

        // Independent across (broker_id, stream)
        StorageWriter::advance_export_cursor(&mut conn, "local", "sticker", 7).unwrap();
        assert_eq!(StorageReader::load_export_cursor(&conn, "local", "sticker").unwrap(), 7);
        assert_eq!(StorageReader::load_export_cursor(&conn, "remote", "sticker").unwrap(), 42);

        // Reset clears just the targeted pair
        StorageWriter::reset_export_cursor(&mut conn, "remote", "sticker").unwrap();
        assert_eq!(StorageReader::load_export_cursor(&conn, "remote", "sticker").unwrap(), 0);
        assert_eq!(StorageReader::load_export_cursor(&conn, "local", "sticker").unwrap(), 7);
    }

    #[test]
    fn append_sticker_removed_event_inserts_marker_row() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 1).unwrap();
        let mut conn = db.connect().unwrap();

        let id = StorageWriter::append_sticker_removed_event(&mut conn, "70b3d5", 1716120100).unwrap();
        assert!(id.is_some());

        let (event_type, dev_eui): (String, String) = conn
            .query_row(
                "SELECT event_type, dev_eui FROM sticker_readings WHERE id = ?",
                rusqlite::params![id.unwrap()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(event_type, "sticker_removed");
        assert_eq!(dev_eui, "70b3d5");
    }
}
