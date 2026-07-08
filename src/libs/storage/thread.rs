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
    /// Write an EYE BLE tag reading (fire-and-forget).
    WriteEyeReading {
        mac: String,
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
    /// Returns true if `dev_eui` has no sticker_readings rows OR the
    /// most-recent one is a `sticker_removed` marker. Used by provisioning
    /// to decide whether to bump the epoch.
    DevEuiLastEventWasRemovalOrAbsent {
        dev_eui: String,
        reply: Sender<StorageResult<bool>>,
    },
    /// Record a free-form audit log entry (fire-and-forget). Used by
    /// config-applier paths that don't otherwise touch the database but
    /// still need an EU-MDR audit trail (e.g. device label changes).
    WriteAuditEvent {
        operation: String,
        table_name: Option<String>,
        details: Option<String>,
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

    /// Send an EYE BLE tag reading to be persisted (fire-and-forget).
    pub fn write_eye_reading(
        &self,
        mac: String,
        ts: i64,
        received_at: i64,
        message_id: String,
        event_type: String,
        payload_json: String,
    ) -> StorageResult<()> {
        self.sender
            .send(StorageMessage::WriteEyeReading {
                mac,
                ts,
                received_at,
                message_id,
                event_type,
                payload_json,
            })
            .map_err(|e| {
                crate::libs::storage::error::StorageError::ChannelError(format!(
                    "Failed to send eye reading: {}",
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

    /// Returns true if `dev_eui` is absent from `sticker_readings` OR the
    /// most-recent row for it is a `sticker_removed` marker. Used by the
    /// LoRaWAN provisioning path to decide whether bumping the epoch is
    /// the right thing to do (vs. an idempotent re-provision of an already-
    /// active sticker).
    pub fn dev_eui_last_event_was_removal_or_absent(
        &self,
        dev_eui: String,
    ) -> StorageResult<bool> {
        let (tx, rx) = bounded(1);
        self.sender
            .send(StorageMessage::DevEuiLastEventWasRemovalOrAbsent { dev_eui, reply: tx })
            .map_err(|e| {
                crate::libs::storage::error::StorageError::ChannelError(format!(
                    "Failed to send dev_eui_last_event_was_removal_or_absent: {}",
                    e
                ))
            })?;
        rx.recv().map_err(|e| {
            crate::libs::storage::error::StorageError::ChannelError(format!(
                "Failed to receive dev_eui_last_event_was_removal_or_absent reply: {}",
                e
            ))
        })?
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

    /// Record an audit-log entry from outside the storage thread (e.g. the
    /// config-applier path that updates `device_label`). Fire-and-forget;
    /// failures show up only in the channel-error log line and never block
    /// or fail the calling code path.
    pub fn log_audit_event(
        &self,
        operation: String,
        table_name: Option<String>,
        details: Option<String>,
    ) -> StorageResult<()> {
        self.sender
            .send(StorageMessage::WriteAuditEvent {
                operation,
                table_name,
                details,
            })
            .map_err(|e| {
                crate::libs::storage::error::StorageError::ChannelError(format!(
                    "Failed to send audit event: {}",
                    e
                ))
            })
    }
}

/// Storage thread worker
pub struct StorageThread;

impl StorageThread {
    /// Spawn the background storage thread.
    ///
    /// `max_size_gb` is the legacy integer-GB cap. Most call sites use
    /// this directly; `spawn_with_max_bytes` is the precision variant
    /// for configs that set `storage.max_size_mb` (e.g. 2500 MB).
    pub fn spawn(db_path: &str, max_size_gb: i32) -> StorageResult<(StorageHandle, thread::JoinHandle<()>)> {
        Self::spawn_with_hmac(db_path, max_size_gb, None)
    }

    /// Spawn with a precomputed byte cap (honors sub-GB granularity).
    pub fn spawn_with_max_bytes(
        db_path: &str,
        max_size_bytes: i64,
        hmac_secret_path: Option<&str>,
    ) -> StorageResult<(StorageHandle, thread::JoinHandle<()>)> {
        Self::spawn_with_hmac_and_max_bytes(db_path, max_size_bytes, hmac_secret_path)
    }

    /// Spawn the background storage thread with optional HMAC secret path
    /// If hmac_secret_path is provided, loads the HMAC key for sensor reading integrity (EU MDR)
    pub fn spawn_with_hmac(db_path: &str, max_size_gb: i32, hmac_secret_path: Option<&str>) -> StorageResult<(StorageHandle, thread::JoinHandle<()>)> {
        Self::spawn_with_hmac_and_max_bytes(
            db_path,
            (max_size_gb.max(1) as i64) * 1024 * 1024 * 1024,
            hmac_secret_path,
        )
    }

    fn spawn_with_hmac_and_max_bytes(
        db_path: &str,
        max_size_bytes: i64,
        hmac_secret_path: Option<&str>,
    ) -> StorageResult<(StorageHandle, thread::JoinHandle<()>)> {
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
                Self::run(&db_path, max_size_bytes, receiver, hmac_secret.as_deref());
            })
            .expect("Failed to spawn storage thread");

        Ok((handle, thread_handle))
    }

    /// Main storage thread loop
    fn run(db_path: &str, max_size_bytes: i64, receiver: Receiver<StorageMessage>, hmac_secret: Option<&[u8]>) {
        // Initialize database
        let db = match Database::with_max_bytes(db_path, max_size_bytes) {
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

        let retention_policy = RetentionPolicy::with_max_bytes(max_size_bytes);
        let mut message_count = 0u64;
        let mut pending_writes = 0usize;
        let mut last_flush = std::time::Instant::now();
        let flush_interval = Duration::from_millis(100); // Auto-flush every 100ms
        let flush_threshold = 1000; // Auto-flush every 1000 messages

        // Batched audit summary: instead of an audit_log row per sensor INSERT
        // (which doubled disk churn and got us into the 100% /data incident),
        // accumulate inserts and emit one INSERT_BATCH row per minute.
        let audit_summary_interval = Duration::from_secs(60);
        let mut audit_summary_count = 0i64;
        let mut last_audit_summary = std::time::Instant::now();

        // Connection self-healing: if N consecutive writes fail (e.g. the
        // /data filesystem went read-only or was remounted), drop the long-
        // lived connection and reopen with exponential backoff. Without this
        // the storage thread loops forever logging "disk I/O error" until
        // a restart.
        const RECONNECT_FAILURE_THRESHOLD: u32 = 3;
        const RECONNECT_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
        const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);
        let mut consecutive_write_failures: u32 = 0;
        let mut reconnect_backoff = RECONNECT_BACKOFF_INITIAL;
        let mut next_reconnect_attempt: Option<std::time::Instant> = None;

        // Per-minute aggregator: roll closed minutes of raw sensor_readings
        // into sensor_readings_minute so the raw table can be retention-
        // trimmed to ~30 days while still answering multi-year queries.
        let aggregator_interval = Duration::from_secs(60);
        let mut last_aggregator_run = std::time::Instant::now();

        // Raw retention sweep: every hour, drop raw rows older than 30 days
        // whose minute has already been folded into the aggregate. Without
        // this the raw table grows ~90MB/day and fills /data in ~5 weeks.
        const RAW_RETENTION_SECONDS: i64 = 30 * 24 * 3600;
        let raw_retention_interval = Duration::from_secs(3600);
        let mut last_raw_retention_run = std::time::Instant::now();

        // Sticker retention sweep: every hour, drop sticker_readings older
        // than 30 days. The matching aggregate (probe_1m / minute aggregates)
        // is shipped via the export pipeline and replayed on demand by
        // viewers, so raw sticker rows past the live window are safe to drop.
        // Without this the table grew unbounded — the StorageHandle method
        // exists but had no scheduler hooked up.
        const STICKER_RETENTION_SECONDS: i64 = 30 * 24 * 3600;
        let sticker_retention_interval = Duration::from_secs(3600);
        let mut last_sticker_retention_run = std::time::Instant::now();

        // EYE retention sweep: every hour, drop eye_readings older than 30
        // days, mirroring the sticker_readings policy above. Without this
        // eye_readings grows unbounded for as long as configured tags keep
        // advertising, eventually filling /data.
        const EYE_RETENTION_SECONDS: i64 = 30 * 24 * 3600;
        let eye_retention_interval = Duration::from_secs(3600);
        let mut last_eye_retention_run = std::time::Instant::now();

        eprintln!(
            "STORAGE THREAD: Started, database: {}, max size: {} MB",
            db_path,
            max_size_bytes / (1024 * 1024),
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
                    if audit_summary_count > 0
                        && last_audit_summary.elapsed() >= audit_summary_interval
                    {
                        let elapsed_ms = last_audit_summary.elapsed().as_millis() as i64;
                        let _ = crate::libs::storage::audit::AuditLogger::log_operation(
                            &conn,
                            "INSERT_BATCH",
                            Some("sensor_readings"),
                            Some(audit_summary_count),
                            Some(elapsed_ms),
                        );
                        audit_summary_count = 0;
                        last_audit_summary = std::time::Instant::now();
                    }
                    if last_aggregator_run.elapsed() >= aggregator_interval
                        && consecutive_write_failures < RECONNECT_FAILURE_THRESHOLD
                    {
                        let now_ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs() as i64;
                        match crate::libs::storage::aggregator::aggregate_closed_minutes(
                            &mut conn,
                            now_ts,
                            hmac_secret,
                        ) {
                            Ok(stats) if stats.rows_inserted > 0 => {
                                eprintln!(
                                    "STORAGE THREAD: aggregator rolled {} minute(s) into sensor_readings_minute",
                                    stats.rows_inserted
                                );
                            }
                            Ok(_) => {}
                            Err(e) => {
                                eprintln!("STORAGE THREAD: aggregator failed: {}", e);
                                consecutive_write_failures =
                                    consecutive_write_failures.saturating_add(1);
                            }
                        }
                        last_aggregator_run = std::time::Instant::now();
                    }
                    if last_raw_retention_run.elapsed() >= raw_retention_interval
                        && consecutive_write_failures < RECONNECT_FAILURE_THRESHOLD
                    {
                        match RetentionPolicy::sweep_raw_sensor_readings(
                            &mut conn,
                            RAW_RETENTION_SECONDS,
                        ) {
                            Ok(purged) if purged > 0 => {
                                eprintln!(
                                    "STORAGE THREAD: raw retention swept {} sensor_readings rows older than {} days",
                                    purged, RAW_RETENTION_SECONDS / 86400
                                );
                            }
                            Ok(_) => {}
                            Err(e) => {
                                eprintln!("STORAGE THREAD: raw retention sweep failed: {}", e);
                            }
                        }
                        last_raw_retention_run = std::time::Instant::now();
                    }
                    if last_sticker_retention_run.elapsed() >= sticker_retention_interval
                        && consecutive_write_failures < RECONNECT_FAILURE_THRESHOLD
                    {
                        match RetentionPolicy::default()
                            .sweep_sticker_readings(&mut conn, STICKER_RETENTION_SECONDS)
                        {
                            Ok(stats) if stats.purged > 0 => {
                                eprintln!(
                                    "STORAGE THREAD: sticker retention swept {} sticker_readings rows older than {} days (unexported_dropped={})",
                                    stats.purged,
                                    STICKER_RETENTION_SECONDS / 86400,
                                    stats.unexported_dropped,
                                );
                            }
                            Ok(_) => {}
                            Err(e) => {
                                eprintln!("STORAGE THREAD: sticker retention sweep failed: {}", e);
                            }
                        }
                        last_sticker_retention_run = std::time::Instant::now();
                    }
                    if last_eye_retention_run.elapsed() >= eye_retention_interval
                        && consecutive_write_failures < RECONNECT_FAILURE_THRESHOLD
                    {
                        match RetentionPolicy::default()
                            .sweep_eye_readings(&mut conn, EYE_RETENTION_SECONDS)
                        {
                            Ok(stats) if stats.purged > 0 => {
                                eprintln!(
                                    "STORAGE THREAD: eye retention swept {} eye_readings rows older than {} days (unexported_dropped={})",
                                    stats.purged,
                                    EYE_RETENTION_SECONDS / 86400,
                                    stats.unexported_dropped,
                                );
                            }
                            Ok(_) => {}
                            Err(e) => {
                                eprintln!("STORAGE THREAD: eye retention sweep failed: {}", e);
                            }
                        }
                        last_eye_retention_run = std::time::Instant::now();
                    }
                    if consecutive_write_failures >= RECONNECT_FAILURE_THRESHOLD
                        && next_reconnect_attempt
                            .map(|t| std::time::Instant::now() >= t)
                            .unwrap_or(true)
                    {
                        eprintln!(
                            "STORAGE THREAD: {} consecutive write failures, reopening DB connection (backoff {:?})",
                            consecutive_write_failures, reconnect_backoff
                        );
                        match db.connect() {
                            Ok(new_conn) => {
                                eprintln!("STORAGE THREAD: Reconnected to database");
                                conn = new_conn;
                                consecutive_write_failures = 0;
                                reconnect_backoff = RECONNECT_BACKOFF_INITIAL;
                                next_reconnect_attempt = None;
                            }
                            Err(e) => {
                                eprintln!(
                                    "STORAGE THREAD: Reconnect failed: {} — retrying in {:?}",
                                    e, reconnect_backoff
                                );
                                next_reconnect_attempt =
                                    Some(std::time::Instant::now() + reconnect_backoff);
                                reconnect_backoff = std::cmp::min(
                                    reconnect_backoff * 2,
                                    RECONNECT_BACKOFF_MAX,
                                );
                            }
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
                                audit_summary_count += 1;
                                consecutive_write_failures = 0;
                                reconnect_backoff = RECONNECT_BACKOFF_INITIAL;
                                next_reconnect_attempt = None;
                            }
                            Err(e) => {
                                eprintln!("STORAGE THREAD: Failed to write reading: {}", e);
                                consecutive_write_failures = consecutive_write_failures.saturating_add(1);
                            }
                        }
                    }

                    StorageMessage::WriteSensorReadingsBatch { readings } => {
                        match StorageWriter::write_sensor_readings_batch(&mut conn, &readings, hmac_secret) {
                            Ok(count) => {
                                pending_writes += count as usize;
                                message_count += count as u64;
                                audit_summary_count += count;
                                consecutive_write_failures = 0;
                                reconnect_backoff = RECONNECT_BACKOFF_INITIAL;
                                next_reconnect_attempt = None;
                            }
                            Err(e) => {
                                eprintln!("STORAGE THREAD: Failed to write batch: {}", e);
                                consecutive_write_failures = consecutive_write_failures.saturating_add(1);
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
                        // PASSIVE on the hot path: never blocks new writers
                        // and never waits for active readers. RESTART (the
                        // mode we used to issue here) is appropriate at
                        // shutdown when you want everyone to drop the WAL,
                        // not for periodic app-driven flushes — under load
                        // it stalled the storage thread for tens to
                        // hundreds of ms while reader-side connections
                        // (export drain, replay) released pages. RESTART is
                        // still used by the Shutdown handler below.
                        let _ = conn.execute("PRAGMA wal_checkpoint(PASSIVE)", []);
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

                    StorageMessage::WriteEyeReading {
                        mac,
                        ts,
                        received_at,
                        message_id,
                        event_type,
                        payload_json,
                    } => {
                        match StorageWriter::write_eye_reading(
                            &mut conn,
                            &mac,
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
                                eprintln!("STORAGE THREAD: write_eye_reading failed: {}", e);
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

                    StorageMessage::DevEuiLastEventWasRemovalOrAbsent { dev_eui, reply } => {
                        use rusqlite::OptionalExtension;
                        let result = conn
                            .query_row(
                                "SELECT event_type FROM sticker_readings
                                 WHERE dev_eui = ?
                                 ORDER BY id DESC LIMIT 1",
                                rusqlite::params![dev_eui],
                                |r| r.get::<_, String>(0),
                            )
                            .optional()
                            .map(|opt| match opt {
                                None => true, // absent
                                Some(e) => e == "sticker_removed",
                            })
                            .map_err(|e| {
                                crate::libs::storage::error::StorageError::QueryError(format!(
                                    "dev_eui_last_event: {}",
                                    e
                                ))
                            });
                        let _ = reply.send(result);
                    }

                    StorageMessage::WriteAuditEvent { operation, table_name, details } => {
                        // Fire-and-forget audit row. Used by config-applier
                        // paths (e.g. device label changes) that don't
                        // otherwise touch the database. record_count and
                        // duration_ms aren't meaningful for these events.
                        let result = match details.as_deref() {
                            Some(d) => crate::libs::storage::audit::AuditLogger::log_operation_with_details(
                                &conn,
                                &operation,
                                table_name.as_deref(),
                                d,
                            ),
                            None => crate::libs::storage::audit::AuditLogger::log_operation(
                                &conn,
                                &operation,
                                table_name.as_deref(),
                                None,
                                None,
                            ),
                        };
                        if let Err(e) = result {
                            eprintln!(
                                "STORAGE THREAD: audit '{}' failed: {}",
                                operation, e
                            );
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
    fn dev_eui_last_event_logic_works() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();
        let (handle, join) = StorageThread::spawn(&path, 1).unwrap();

        // Absent → true
        assert!(handle.dev_eui_last_event_was_removal_or_absent("abc".into()).unwrap());

        handle
            .write_sticker_reading(
                "abc".into(),
                1,
                1000,
                1000,
                "abc-1000-0".into(),
                "uplink".into(),
                "{}".into(),
            )
            .unwrap();
        handle.flush().unwrap();
        // Last event is uplink → false
        assert!(!handle.dev_eui_last_event_was_removal_or_absent("abc".into()).unwrap());

        handle.append_sticker_removed("abc".into(), 1100).unwrap();
        handle.flush().unwrap();
        // Last event is sticker_removed → true
        assert!(handle.dev_eui_last_event_was_removal_or_absent("abc".into()).unwrap());

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
