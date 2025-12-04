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
}

/// Storage thread worker
pub struct StorageThread;

impl StorageThread {
    /// Spawn the background storage thread
    pub fn spawn(db_path: &str, max_size_gb: i32) -> StorageResult<(StorageHandle, thread::JoinHandle<()>)> {
        let db_path = db_path.to_string();

        // Create bounded channel (buffer 10,000 messages max = ~5MB)
        let (sender, receiver) = bounded::<StorageMessage>(10000);

        let handle = StorageHandle { sender };

        let thread_handle = thread::Builder::new()
            .name("fiber-storage".to_string())
            .spawn(move || {
                Self::run(&db_path, max_size_gb, receiver);
            })
            .expect("Failed to spawn storage thread");

        Ok((handle, thread_handle))
    }

    /// Main storage thread loop
    fn run(db_path: &str, max_size_gb: i32, receiver: Receiver<StorageMessage>) {
        // Initialize database
        let db = match Database::new(db_path, max_size_gb) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("STORAGE THREAD: Failed to initialize database: {}", e);
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
                        if let Ok(conn) = db.connect() {
                            let _ = conn.execute("PRAGMA wal_checkpoint(PASSIVE)", []);
                            eprintln!("STORAGE THREAD: Auto-flushed {} pending writes", pending_writes);
                            pending_writes = 0;
                            last_flush = std::time::Instant::now();
                        }
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
                        if let Ok(conn) = db.connect() {
                            let reading = SensorReading::new(
                                timestamp,
                                sensor_line,
                                temperature,
                                is_connected,
                                alarm_state,
                            );

                            match StorageWriter::write_sensor_reading(&conn, &reading) {
                                Ok(_) => {
                                    pending_writes += 1;
                                    message_count += 1;
                                }
                                Err(e) => {
                                    eprintln!("STORAGE THREAD: Failed to write reading: {}", e);
                                }
                            }
                        }
                    }

                    StorageMessage::WriteSensorReadingsBatch { readings } => {
                        if let Ok(mut conn) = db.connect() {
                            match StorageWriter::write_sensor_readings_batch(&mut conn, &readings) {
                                Ok(count) => {
                                    pending_writes += count as usize;
                                    message_count += count as u64;
                                }
                                Err(e) => {
                                    eprintln!("STORAGE THREAD: Failed to write batch: {}", e);
                                }
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
                        if let Ok(conn) = db.connect() {
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
                    }

                    StorageMessage::Flush => {
                        if let Ok(conn) = db.connect() {
                            let _ = conn.execute("PRAGMA wal_checkpoint(RESTART)", []);
                            eprintln!("STORAGE THREAD: Manual flush of {} pending writes", pending_writes);
                            pending_writes = 0;
                            last_flush = std::time::Instant::now();
                        }
                    }

                    StorageMessage::EnforceRetention => {
                        if let Ok(mut conn) = db.connect() {
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
                    }

                    StorageMessage::GetStorageStats => {
                        if let Ok(conn) = db.connect() {
                            match StorageReader::get_storage_stats(&conn, db_path) {
                                Ok(stats) => {
                                    eprintln!("STORAGE THREAD: {}", stats);
                                }
                                Err(e) => {
                                    eprintln!("STORAGE THREAD: Failed to get stats: {}", e);
                                }
                            }
                        }
                    }

                    StorageMessage::Shutdown => {
                        if pending_writes > 0 {
                            if let Ok(conn) = db.connect() {
                                let _ = conn.execute("PRAGMA wal_checkpoint(RESTART)", []);
                                eprintln!("STORAGE THREAD: Final flush of {} pending writes", pending_writes);
                            }
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
                    if let Ok(conn) = db.connect() {
                        let _ = conn.execute("PRAGMA wal_checkpoint(PASSIVE)", []);
                        if pending_writes > 0 {
                            eprintln!("STORAGE THREAD: Flushed {} pending writes", pending_writes);
                        }
                        pending_writes = 0;
                        last_flush = std::time::Instant::now();
                    }
                }

                // Periodic retention check (every 10,000 messages)
                if message_count % 10000 == 0 {
                    if let Ok(mut conn) = db.connect() {
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
}
