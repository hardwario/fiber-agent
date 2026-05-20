//! Background storage thread for async writes
//! Non-blocking message-based architecture using crossbeam channels

use crossbeam::channel::{bounded, Receiver, Sender};
use std::thread;
use std::time::Duration;

use crate::libs::alarms::AlarmState;
use crate::libs::storage::db::Database;
use crate::libs::storage::error::StorageResult;
use crate::libs::storage::models::{AlarmEvent, SensorReading};
use crate::libs::storage::retention::RetentionPolicy;
use crate::libs::storage::writer::StorageWriter;
use crate::libs::storage::reader::StorageReader;

/// Message types for communication with storage thread
#[derive(Debug)]
pub enum StorageMessage {
    /// Write a single sensor reading
    WriteSensorReading {
        timestamp: i64,
        sensor_line: u8,
        temperature: f32,
        is_connected: bool,
        alarm_state: AlarmState,
    },

    /// Write a batch of sensor readings (more efficient)
    WriteSensorReadingsBatch { readings: Vec<SensorReading> },

    /// Write an alarm event
    WriteAlarmEvent {
        timestamp: i64,
        sensor_line: u8,
        from_state: AlarmState,
        to_state: AlarmState,
        temperature: Option<f32>,
    },

    /// Query last N readings (blocks on response)
    QueryLastReadings { sensor_line: u8, count: usize },

    /// Get storage statistics
    GetStorageStats,

    /// Manually enforce retention policy
    EnforceRetention,

    /// Flush pending writes to disk
    Flush,

    /// Graceful shutdown
    Shutdown,

    // ===== Save-and-feed (sticker stream + export cursor) =====
    /// Write a sticker uplink or marker. Fire-and-forget — failures are logged.
    WriteStickerReading {
        dev_eui: String,
        provisioning_epoch: i64,
        ts: i64,
        received_at: i64,
        message_id: String,
        event_type: String,
        payload_json: String,
    },
    /// Append a `sticker_removed` marker event (fire-and-forget).
    AppendStickerRemoved {
        dev_eui: String,
        ts: i64,
    },
    /// Bump and return the new provisioning epoch for a dev_eui.
    BumpProvisioningEpoch {
        dev_eui: String,
        reply: Sender<StorageResult<i64>>,
    },
    /// Read the current provisioning epoch for a dev_eui (default 1).
    GetProvisioningEpoch {
        dev_eui: String,
        reply: Sender<StorageResult<i64>>,
    },
    /// Advance the export cursor for a `(broker_id, stream)` pair.
    AdvanceExportCursor {
        broker_id: String,
        stream: String,
        new_id: i64,
    },
    /// Reset the export cursor for a `(broker_id, stream)` pair to 0.
    ResetExportCursor {
        broker_id: String,
        stream: String,
    },
    /// Enforce retention on `sticker_readings` (delete rows older than
    /// `retention_seconds`, log a WARN for un-exported drops).
    EnforceStickerRetention {
        retention_seconds: i64,
    },
}

/// Handle for sending messages to storage thread
#[derive(Clone)]
pub struct StorageHandle {
    sender: Sender<StorageMessage>,
}

impl StorageHandle {
    /// Send a sensor reading to be stored
    pub fn write_sensor_reading(
        &self,
        timestamp: i64,
        sensor_line: u8,
        temperature: f32,
        is_connected: bool,
        alarm_state: AlarmState,
    ) -> StorageResult<()> {
        self.sender
            .send(StorageMessage::WriteSensorReading {
                timestamp,
                sensor_line,
                temperature,
                is_connected,
                alarm_state,
            })
            .map_err(|e| {
                crate::libs::storage::error::StorageError::ChannelError(format!(
                    "Failed to send write message: {}",
                    e
                ))
            })
    }

    /// Send an alarm event to be stored
    pub fn write_alarm_event(
        &self,
        timestamp: i64,
        sensor_line: u8,
        from_state: AlarmState,
        to_state: AlarmState,
        temperature: Option<f32>,
    ) -> StorageResult<()> {
        self.sender
            .send(StorageMessage::WriteAlarmEvent {
                timestamp,
                sensor_line,
                from_state,
                to_state,
                temperature,
            })
            .map_err(|e| {
                crate::libs::storage::error::StorageError::ChannelError(format!(
                    "Failed to send alarm message: {}",
                    e
                ))
            })
    }

    /// Flush pending writes to disk
    pub fn flush(&self) -> StorageResult<()> {
        self.sender
            .send(StorageMessage::Flush)
            .map_err(|e| {
                crate::libs::storage::error::StorageError::ChannelError(format!(
                    "Failed to send flush message: {}",
                    e
                ))
            })
    }

    /// Signal shutdown
    pub fn shutdown(&self) -> StorageResult<()> {
        self.sender
            .send(StorageMessage::Shutdown)
            .map_err(|e| {
                crate::libs::storage::error::StorageError::ChannelError(format!(
                    "Failed to send shutdown message: {}",
                    e
                ))
            })
    }

    // ===== Save-and-feed (sticker stream + export cursor) =====

    /// Send a sticker reading to be persisted (fire-and-forget).
    pub fn write_sticker_reading(
        &self,
        dev_eui: String,
        provisioning_epoch: i64,
        ts: i64,
        received_at: i64,
        message_id: String,
        event_type: String,
        payload_json: String,
    ) -> StorageResult<()> {
        self.sender
            .send(StorageMessage::WriteStickerReading {
                dev_eui,
                provisioning_epoch,
                ts,
                received_at,
                message_id,
                event_type,
                payload_json,
            })
            .map_err(|e| {
                crate::libs::storage::error::StorageError::ChannelError(format!(
                    "Failed to send sticker reading: {}",
                    e
                ))
            })
    }

    /// Append a `sticker_removed` marker event (fire-and-forget).
    pub fn append_sticker_removed(&self, dev_eui: String, ts: i64) -> StorageResult<()> {
        self.sender
            .send(StorageMessage::AppendStickerRemoved { dev_eui, ts })
            .map_err(|e| {
                crate::libs::storage::error::StorageError::ChannelError(format!(
                    "Failed to send sticker_removed: {}",
                    e
                ))
            })
    }

    /// Bump and return the new provisioning epoch for `dev_eui`.
    pub fn bump_provisioning_epoch(&self, dev_eui: String) -> StorageResult<i64> {
        let (tx, rx) = bounded(1);
        self.sender
            .send(StorageMessage::BumpProvisioningEpoch { dev_eui, reply: tx })
            .map_err(|e| {
                crate::libs::storage::error::StorageError::ChannelError(format!(
                    "Failed to send bump_provisioning_epoch: {}",
                    e
                ))
            })?;
        rx.recv().map_err(|e| {
            crate::libs::storage::error::StorageError::ChannelError(format!(
                "Failed to receive bump_provisioning_epoch reply: {}",
                e
            ))
        })?
    }

    /// Read the current provisioning epoch for `dev_eui` (default 1).
    pub fn get_provisioning_epoch(&self, dev_eui: String) -> StorageResult<i64> {
        let (tx, rx) = bounded(1);
        self.sender
            .send(StorageMessage::GetProvisioningEpoch { dev_eui, reply: tx })
            .map_err(|e| {
                crate::libs::storage::error::StorageError::ChannelError(format!(
                    "Failed to send get_provisioning_epoch: {}",
                    e
                ))
            })?;
        rx.recv().map_err(|e| {
            crate::libs::storage::error::StorageError::ChannelError(format!(
                "Failed to receive get_provisioning_epoch reply: {}",
                e
            ))
        })?
    }

    /// Advance the export cursor for `(broker_id, stream)` to `new_id`.
    pub fn advance_export_cursor(
        &self,
        broker_id: String,
        stream: String,
        new_id: i64,
    ) -> StorageResult<()> {
        self.sender
            .send(StorageMessage::AdvanceExportCursor {
                broker_id,
                stream,
                new_id,
            })
            .map_err(|e| {
                crate::libs::storage::error::StorageError::ChannelError(format!(
                    "Failed to send advance_export_cursor: {}",
                    e
                ))
            })
    }

    /// Reset the export cursor for `(broker_id, stream)` to 0.
    pub fn reset_export_cursor(&self, broker_id: String, stream: String) -> StorageResult<()> {
        self.sender
            .send(StorageMessage::ResetExportCursor { broker_id, stream })
            .map_err(|e| {
                crate::libs::storage::error::StorageError::ChannelError(format!(
                    "Failed to send reset_export_cursor: {}",
                    e
                ))
            })
    }

    /// Enforce retention on `sticker_readings`.
    pub fn enforce_sticker_retention(&self, retention_seconds: i64) -> StorageResult<()> {
        self.sender
            .send(StorageMessage::EnforceStickerRetention { retention_seconds })
            .map_err(|e| {
                crate::libs::storage::error::StorageError::ChannelError(format!(
                    "Failed to send enforce_sticker_retention: {}",
                    e
                ))
            })
    }
}

/// Storage thread worker
pub struct StorageThread;

impl StorageThread {
    /// Spawn the background storage thread
    pub fn spawn(db_path: &str, max_size_gb: i32) -> StorageResult<(StorageHandle, thread::JoinHandle<()>)> {
        Self::spawn_with_hmac(db_path, max_size_gb, None)
    }

    /// Spawn the background storage thread with optional HMAC secret path
    /// If hmac_secret_path is provided, loads the HMAC key for sensor reading integrity (EU MDR)
    pub fn spawn_with_hmac(db_path: &str, max_size_gb: i32, hmac_secret_path: Option<&str>) -> StorageResult<(StorageHandle, thread::JoinHandle<()>)> {
        let db_path = db_path.to_string();

        // Load HMAC secret at startup; auto-generate if missing (EU MDR requires integrity tags)
        let hmac_secret: Option<Vec<u8>> = hmac_secret_path.and_then(|path| {
            match std::fs::read(path) {
                Ok(key) => {
                    if key.is_empty() {
                        eprintln!("STORAGE THREAD: HMAC key file {} is empty, integrity tags disabled", path);
                        None
                    } else {
                        eprintln!("STORAGE THREAD: HMAC secret loaded from {} ({} bytes)", path, key.len());
                        Some(key)
                    }
                }
                Err(_) => {
                    // Auto-generate a 32-byte random HMAC key on first boot
                    eprintln!("STORAGE THREAD: HMAC key file {} not found, generating new 32-byte key", path);
                    use rand::Rng;
                    let key: Vec<u8> = {
                        let mut rng = rand::thread_rng();
                        let mut buf = vec![0u8; 32];
                        rng.fill(&mut buf[..]);
                        buf
                    };

                    // Ensure parent directory exists
                    if let Some(parent) = std::path::Path::new(path).parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }

                    match std::fs::write(path, &key) {
                        Ok(()) => {
                            // Set file permissions to owner-only read/write (0o600)
                            #[cfg(unix)]
                            {
                                use std::os::unix::fs::PermissionsExt;
                                let perms = std::fs::Permissions::from_mode(0o600);
                                if let Err(e) = std::fs::set_permissions(path, perms) {
                                    eprintln!("STORAGE THREAD: WARNING - Could not set permissions on {}: {}", path, e);
                                }
                            }
                            eprintln!("STORAGE THREAD: Generated new HMAC key and saved to {} (32 bytes, mode 0600)", path);
                            Some(key)
                        }
                        Err(e) => {
                            eprintln!("STORAGE THREAD: ERROR - Failed to write generated HMAC key to {}: {} — sensor reading integrity tags disabled", path, e);
                            None
                        }
                    }
                }
            }
        });

        // Create bounded channel (buffer 10,000 messages max = ~5MB)
        let (sender, receiver) = bounded::<StorageMessage>(10000);

        let handle = StorageHandle { sender };

        let thread_handle = thread::Builder::new()
            .name("fiber-storage".to_string())
            .spawn(move || {
                Self::run(&db_path, max_size_gb, receiver, hmac_secret.as_deref());
            })
            .expect("Failed to spawn storage thread");

        Ok((handle, thread_handle))
    }

    /// Main storage thread loop
    fn run(db_path: &str, max_size_gb: i32, receiver: Receiver<StorageMessage>, hmac_secret: Option<&[u8]>) {
        // Initialize database
        let db = match Database::new(db_path, max_size_gb) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("STORAGE THREAD: Failed to initialize database: {}", e);
                return;
            }
        };

        // Open ONE connection for the lifetime of this thread.
        // Re-opening per message would re-run SQLCipher's PBKDF2 KDF (256k iterations)
        // and burn an entire CPU core on low-power ARM devices.
        let mut conn = match db.connect() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("STORAGE THREAD: Failed to open initial connection: {}", e);
                return;
            }
        };

        let retention_policy = RetentionPolicy::new(max_size_gb);
        let mut message_count = 0u64;
        let mut pending_writes = 0usize;
        let mut last_flush = std::time::Instant::now();
        let flush_interval = Duration::from_millis(100); // Auto-flush every 100ms
        let flush_threshold = 1000; // Auto-flush every 1000 messages

        eprintln!(
            "STORAGE THREAD: Started, database: {}, max size: {}GB",
            db_path, max_size_gb
        );

        loop {
            // Try to get a message with timeout for periodic flushing
            let msg = match receiver.recv_timeout(flush_interval) {
                Ok(m) => Some(m),
                Err(crossbeam::channel::RecvTimeoutError::Timeout) => {
                    // Timeout - do periodic flush if needed
                    if pending_writes > 0 && last_flush.elapsed() > flush_interval {
                        let _ = conn.execute("PRAGMA wal_checkpoint(PASSIVE)", []);
                        pending_writes = 0;
                        last_flush = std::time::Instant::now();
                    }
                    None
                }
                Err(crossbeam::channel::RecvTimeoutError::Disconnected) => {
                    eprintln!("STORAGE THREAD: Receiver disconnected, shutting down");
                    break;
                }
            };

            if let Some(msg) = msg {
                match msg {
                    StorageMessage::WriteSensorReading {
                        timestamp,
                        sensor_line,
                        temperature,
                        is_connected,
                        alarm_state,
                    } => {
                        let reading = SensorReading::new(
                            timestamp,
                            sensor_line,
                            temperature,
                            is_connected,
                            alarm_state,
                        );

                        match StorageWriter::write_sensor_reading(&conn, &reading, hmac_secret) {
                            Ok(_) => {
                                pending_writes += 1;
                                message_count += 1;
                            }
                            Err(e) => {
                                eprintln!("STORAGE THREAD: Failed to write reading: {}", e);
                            }
                        }
                    }

                    StorageMessage::WriteSensorReadingsBatch { readings } => {
                        match StorageWriter::write_sensor_readings_batch(&mut conn, &readings, hmac_secret) {
                            Ok(count) => {
                                pending_writes += count as usize;
                                message_count += count as u64;
                            }
                            Err(e) => {
                                eprintln!("STORAGE THREAD: Failed to write batch: {}", e);
                            }
                        }
                    }

                    StorageMessage::WriteAlarmEvent {
                        timestamp,
                        sensor_line,
                        from_state,
                        to_state,
                        temperature,
                    } => {
                        let event =
                            AlarmEvent::new(timestamp, sensor_line, from_state, to_state, temperature);

                        match StorageWriter::write_alarm_event(&conn, &event) {
                            Ok(_) => {
                                pending_writes += 1;
                                message_count += 1;
                            }
                            Err(e) => {
                                eprintln!("STORAGE THREAD: Failed to write alarm: {}", e);
                            }
                        }
                    }

                    StorageMessage::Flush => {
                        let _ = conn.execute("PRAGMA wal_checkpoint(RESTART)", []);
                        pending_writes = 0;
                        last_flush = std::time::Instant::now();
                    }

                    StorageMessage::EnforceRetention => {
                        match retention_policy.enforce(&db, &mut conn) {
                            Ok(stats) => {
                                eprintln!(
                                    "STORAGE THREAD: Retention enforced - deleted {}, freed {}MB",
                                    stats.deleted_count,
                                    stats.freed_bytes / (1024 * 1024)
                                );
                            }
                            Err(e) => {
                                eprintln!("STORAGE THREAD: Retention enforcement failed: {}", e);
                            }
                        }
                    }

                    StorageMessage::GetStorageStats => {
                        match StorageReader::get_storage_stats(&conn, db_path) {
                            Ok(stats) => {
                                eprintln!("STORAGE THREAD: {}", stats);
                            }
                            Err(e) => {
                                eprintln!("STORAGE THREAD: Failed to get stats: {}", e);
                            }
                        }
                    }

                    StorageMessage::WriteStickerReading {
                        dev_eui,
                        provisioning_epoch,
                        ts,
                        received_at,
                        message_id,
                        event_type,
                        payload_json,
                    } => {
                        match StorageWriter::write_sticker_reading(
                            &mut conn,
                            &dev_eui,
                            provisioning_epoch,
                            ts,
                            received_at,
                            &message_id,
                            &event_type,
                            &payload_json,
                        ) {
                            Ok(_) => {
                                pending_writes += 1;
                                message_count += 1;
                            }
                            Err(e) => {
                                eprintln!("STORAGE THREAD: write_sticker_reading failed: {}", e);
                            }
                        }
                    }

                    StorageMessage::AppendStickerRemoved { dev_eui, ts } => {
                        match StorageWriter::append_sticker_removed_event(&mut conn, &dev_eui, ts) {
                            Ok(_) => {
                                pending_writes += 1;
                                message_count += 1;
                            }
                            Err(e) => {
                                eprintln!(
                                    "STORAGE THREAD: append_sticker_removed_event failed: {}",
                                    e
                                );
                            }
                        }
                    }

                    StorageMessage::BumpProvisioningEpoch { dev_eui, reply } => {
                        let _ = reply
                            .send(StorageWriter::bump_provisioning_epoch(&mut conn, &dev_eui));
                    }

                    StorageMessage::GetProvisioningEpoch { dev_eui, reply } => {
                        let _ = reply.send(StorageWriter::get_provisioning_epoch(&conn, &dev_eui));
                    }

                    StorageMessage::AdvanceExportCursor {
                        broker_id,
                        stream,
                        new_id,
                    } => {
                        if let Err(e) = StorageWriter::advance_export_cursor(
                            &mut conn,
                            &broker_id,
                            &stream,
                            new_id,
                        ) {
                            eprintln!("STORAGE THREAD: advance_export_cursor failed: {}", e);
                        }
                    }

                    StorageMessage::ResetExportCursor { broker_id, stream } => {
                        if let Err(e) =
                            StorageWriter::reset_export_cursor(&mut conn, &broker_id, &stream)
                        {
                            eprintln!("STORAGE THREAD: reset_export_cursor failed: {}", e);
                        }
                    }

                    StorageMessage::EnforceStickerRetention { retention_seconds } => {
                        match RetentionPolicy::default()
                            .sweep_sticker_readings(&mut conn, retention_seconds)
                        {
                            Ok(r) => {
                                if r.purged > 0 {
                                    eprintln!(
                                        "STORAGE THREAD: sticker retention purged {} rows ({} un-exported)",
                                        r.purged, r.unexported_dropped
                                    );
                                }
                            }
                            Err(e) => {
                                eprintln!(
                                    "STORAGE THREAD: sweep_sticker_readings failed: {}",
                                    e
                                );
                            }
                        }
                    }

                    StorageMessage::Shutdown => {
                        if pending_writes > 0 {
                            let _ = conn.execute("PRAGMA wal_checkpoint(RESTART)", []);
                            eprintln!("STORAGE THREAD: Final flush of {} pending writes", pending_writes);
                        }
                        eprintln!(
                            "STORAGE THREAD: Shutting down after processing {} messages",
                            message_count
                        );
                        break;
                    }

                    _ => {}
                }

                // Periodic flush if we have pending writes
                if pending_writes > flush_threshold || last_flush.elapsed() > flush_interval {
                    let _ = conn.execute("PRAGMA wal_checkpoint(PASSIVE)", []);
                    pending_writes = 0;
                    last_flush = std::time::Instant::now();
                }

                // Periodic retention check (every 10,000 messages, skipping 0)
                if message_count > 0 && message_count % 10000 == 0 {
                    if let Ok(should_clean) = retention_policy.needs_cleanup(&db) {
                        if should_clean {
                            if let Ok(usage) = retention_policy.get_usage_percent(&db) {
                                eprintln!("STORAGE THREAD: Storage at {:.1}%, enforcing retention", usage);
                                let _ = retention_policy.enforce(&db, &mut conn);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_storage_handle_creation() {
        let (handle, _) =
            StorageThread::spawn("/tmp/test_thread.db", 5).expect("Failed to spawn thread");

        // Send a write message
        let result = handle.write_sensor_reading(
            1000,
            0,
            36.5,
            true,
            AlarmState::Normal,
        );
        assert!(result.is_ok());

        let _ = handle.shutdown();
        let _ = std::fs::remove_file("/tmp/test_thread.db");
    }

    #[test]
    fn storage_handle_write_sticker_reading_persists_via_thread() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        let (handle, join) = StorageThread::spawn(&path, 1).unwrap();
        handle
            .write_sticker_reading(
                "abc".into(),
                1,
                1716120000,
                1716120001,
                "abc-1716120000-0".into(),
                "uplink".into(),
                r#"{"fields":{"temp":21}}"#.into(),
            )
            .unwrap();
        handle.flush().unwrap();
        handle.shutdown().unwrap();
        join.join().unwrap();

        let db = Database::new(&path, 1).unwrap();
        let conn = db.connect().unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM sticker_readings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn storage_handle_bump_and_get_provisioning_epoch_via_thread() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        let (handle, join) = StorageThread::spawn(&path, 1).unwrap();
        assert_eq!(handle.get_provisioning_epoch("xyz".into()).unwrap(), 1);
        assert_eq!(handle.bump_provisioning_epoch("xyz".into()).unwrap(), 2);
        assert_eq!(handle.bump_provisioning_epoch("xyz".into()).unwrap(), 3);
        assert_eq!(handle.get_provisioning_epoch("xyz".into()).unwrap(), 3);

        handle.shutdown().unwrap();
        join.join().unwrap();
    }

    #[test]
    fn storage_handle_export_cursor_ops_via_thread() {
        use crate::libs::storage::reader::StorageReader;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        let (handle, join) = StorageThread::spawn(&path, 1).unwrap();
        handle
            .advance_export_cursor("local".into(), "sticker".into(), 17)
            .unwrap();
        handle.flush().unwrap();
        handle.shutdown().unwrap();
        join.join().unwrap();

        let db = Database::new(&path, 1).unwrap();
        let conn = db.connect().unwrap();
        assert_eq!(
            StorageReader::load_export_cursor(&conn, "local", "sticker").unwrap(),
            17
        );
    }
}
