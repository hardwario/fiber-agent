//! Background storage thread for async writes
//! Non-blocking message-based architecture using crossbeam channels

use crossbeam::channel::{bounded, Receiver, Sender};
use rusqlite::Connection;
use std::thread;
use std::time::{Duration, Instant};

use crate::libs::alarms::AlarmState;
use crate::libs::config::MaintenanceWindow;
use crate::libs::storage::db::Database;
use crate::libs::storage::error::StorageResult;
use crate::libs::storage::models::{AlarmEvent, SensorReading};
use crate::libs::storage::reader::StorageReader;
use crate::libs::storage::retention::{self, RetentionPolicy};
use crate::libs::storage::spill::{self, Routed, SpillJournal, SpillRecord};
use crate::libs::storage::writer::StorageWriter;

/// Fold the WAL back into the main database.
///
/// `PRAGMA wal_checkpoint` returns a `(busy, log, checkpointed)` row, and
/// rusqlite's `execute` rejects any statement that yields rows — so every
/// `let _ = conn.execute("PRAGMA wal_checkpoint(...)", [])` in this file was a
/// no-op that silently discarded the error. `FlushSync` spotted the trap and
/// documented it ("do not copy them"); the copies it warned about were never
/// fixed, so in practice the only checkpoints this process performed came from
/// `FlushSync` and `wal_autocheckpoint`.
///
/// One helper, so there is nowhere left to copy the mistake from. A `busy`
/// result is not an error — PASSIVE declining to block is the point of PASSIVE
/// — so only a real failure is reported.
fn checkpoint(conn: &Connection, mode: &str) -> rusqlite::Result<()> {
    conn.query_row(&format!("PRAGMA wal_checkpoint({mode})"), [], |_| Ok(()))
}

/// Unix seconds, for spill-row timestamps.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Why a rewrite is starting, for the audit row and the log line.
///
/// Explicit rather than derived from [`retention::VacuumUrgency`]: the same
/// `Deferred` urgency now reaches `start_vacuum` down two different paths — the
/// window tick, and the size trigger on a Hub with no window configured — and
/// an auditor reading `/audit` needs to tell those apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VacuumTrigger {
    /// The maintenance window opened and a rewrite was owed.
    Schedule,
    /// The size trigger, on a deployment with no window to defer to.
    Retention,
    /// Utilisation crossed the emergency threshold. Ignores the window:
    /// reclaiming space beats waiting for 02:30.
    Emergency,
}

impl VacuumTrigger {
    fn as_str(self) -> &'static str {
        match self {
            Self::Schedule => "schedule",
            Self::Retention => "retention",
            Self::Emergency => "emergency",
        }
    }
}

/// How often to ask whether the maintenance window has opened.
///
/// The window is measured in tens of minutes, so a five-minute check is
/// precise enough and costs one `SystemTime::now` per tick.
const MAINTENANCE_CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How often the capacity pass checks the cap and the free space on `/data`.
const CAPACITY_CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Is the wall clock inside the configured window right now?
///
/// `false` for no window at all — the opt-in default, where a rewrite happens
/// only on the emergency threshold. UTC, deliberately: see
/// [`MaintenanceWindow`].
fn in_maintenance_window(window: Option<&MaintenanceWindow>) -> bool {
    let Some(window) = window else {
        return false;
    };
    let minute_of_day = (now_unix().rem_euclid(86_400) / 60) as u32;
    window.contains(minute_of_day)
}

/// Where a deferred write actually landed.
///
/// There is no third outcome, and that is the point: [`accept_durably`] does
/// not return until the record is on disk somewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Accepted {
    /// Appended to the spill journal — the normal path during a rewrite.
    Spilled,
    /// Committed straight to the main database, because the journal would not
    /// take it.
    WrittenDirectly,
}

/// How long to wait between attempts when neither the journal nor the database
/// will accept a write. Short at first because the usual cause is transient,
/// then a flat cadence so a genuinely wedged filesystem does not spin a core.
const ACCEPT_RETRY_BACKOFF: [Duration; 3] = [
    Duration::from_millis(50),
    Duration::from_millis(250),
    Duration::from_secs(1),
];
const ACCEPT_RETRY_CADENCE: Duration = Duration::from_secs(2);
/// How often to repeat the "still stalled" line once a write is stuck.
const ACCEPT_STALL_LOG_EVERY: Duration = Duration::from_secs(30);

/// Accept `record` durably, or stall until it can be accepted.
///
/// The ladder, in order of preference:
///
/// 1. the spill journal — the normal path during a rewrite, one fsynced INSERT;
/// 2. the main database via [`SpillRecord::apply`] — during a rewrite this
///    waits out `busy_timeout` and usually fails, but a landed write is a
///    landed write;
/// 3. sleep and repeat, forever.
///
/// This replaces a `continue` that logged "at risk of being lost" and dropped
/// the message. That is the one outcome plan/portal/sqlite-vacuum.md §3 forbids
/// outright, so there is no error return here to tempt a caller into it.
///
/// Step 3 stops this thread draining its channel, so producers block in `send`.
/// That is exactly the contract §3.2.2 states — every `StorageHandle` method
/// uses a blocking `send`, and a stall is a delay, not a loss. Step 2 is also
/// what makes the ladder self-terminating: once the rewrite releases the lock,
/// the direct write succeeds on the next pass even if the journal never
/// recovers.
///
/// **Step 2 trades acceptance order for durability, deliberately.** A direct
/// write while the journal still holds a backlog commits out of order relative
/// to those queued rows. That is safe for every record type here: replayed rows
/// are inserted fresh and take *higher* rowids than anything already committed,
/// so an `AdvanceExportCursor` that lands early cannot skip them — its cursor
/// value was computed from pre-run rows and is strictly lower. Losing the row
/// would not be safe, and durability is the guarantee we actually made.
fn accept_durably(
    journal: &SpillJournal,
    conn: &mut Connection,
    record: &SpillRecord,
    hmac_secret: Option<&[u8]>,
) -> Accepted {
    let mut attempt = 0usize;
    let started = Instant::now();
    let mut last_logged: Option<Instant> = None;

    loop {
        match journal.append(now_unix(), record) {
            Ok(_) => return Accepted::Spilled,
            Err(journal_err) => {
                // Not contention: the journal connection already carries a 30 s
                // busy_timeout, so an error here is a real fault — a read-only
                // /data, an I/O error, a deleted file. Try the database.
                match record.apply(conn, hmac_secret) {
                    Ok(()) => return Accepted::WrittenDirectly,
                    Err(db_err) => {
                        let due = last_logged
                            .map(|t| t.elapsed() >= ACCEPT_STALL_LOG_EVERY)
                            .unwrap_or(true);
                        if due {
                            eprintln!(
                                "STORAGE THREAD: cannot accept a write — journal: {}; \
                                 database: {}. Stalling for {:?} rather than \
                                 dropping it; producers will block until one of \
                                 the two recovers.",
                                journal_err,
                                db_err,
                                started.elapsed()
                            );
                            last_logged = Some(Instant::now());
                        }
                    }
                }
            }
        }

        thread::sleep(
            ACCEPT_RETRY_BACKOFF
                .get(attempt)
                .copied()
                .unwrap_or(ACCEPT_RETRY_CADENCE),
        );
        attempt = attempt.saturating_add(1);
    }
}

/// Has the rewrite reported back, and what did it say?
///
/// `None` — still running — is the only answer that leaves the rewrite in
/// flight, and that is the whole point of spelling the match out.
///
/// `Disconnected` means the helper dropped its sender without reporting: it
/// panicked, and nothing in this workspace sets `panic = "abort"`, so the
/// process carries on around it. That case used to fall into the same arm as
/// `Empty` (`if let Ok(outcome) = rx.try_recv()`), which left
/// `vacuum_in_flight` set for the life of the process — every durable write
/// spilling forever into an uncapped journal, the whole housekeeping block
/// suppressed, and the emergency retention path gated off. This is the
/// agent-side counterpart of the portal's `SpillLease::drop`: however the
/// rewrite ended, it ended, and exactly one path leads out.
fn vacuum_report(
    rx: &Receiver<StorageResult<retention::VacuumOutcome>>,
) -> Option<StorageResult<retention::VacuumOutcome>> {
    match rx.try_recv() {
        Ok(outcome) => Some(outcome),
        Err(crossbeam::channel::TryRecvError::Empty) => None,
        Err(crossbeam::channel::TryRecvError::Disconnected) => Some(Err(
            crate::libs::storage::error::StorageError::ChannelError(
                "the VACUUM helper thread ended without reporting an outcome (it panicked); \
                 the rewrite is over either way"
                    .to_string(),
            ),
        )),
    }
}

/// Hand a full `VACUUM` to a helper thread, after a free-space preflight and a
/// real checkpoint.
///
/// Returns the channel the helper reports back on, or `None` when the run was
/// refused — a refusal is a logged skip, never an error loop.
///
/// The rewrite runs on the helper's *own* connection so this thread can keep
/// draining its channel into the spill journal. What it must not do is write a
/// row of any kind — which is why it needs no HMAC secret. In particular it
/// must not write an audit row: `AuditLogger` hash-chains without locking, so a
/// second writer would fork the chain. The storage thread records the outcome
/// when the helper reports back.
///
/// The caller decides *whether* to run; this only decides whether it *can*.
/// A `Deferred` urgency reaches here either because the window opened
/// ([`VacuumTrigger::Schedule`]) or because no window is configured and there is
/// nothing to defer to ([`VacuumTrigger::Retention`]). The capacity limits
/// count live pages, so a skipped rewrite only leaves the file at its high-water
/// mark; it no longer makes the next pass delete more (#124).
/// [`VacuumTrigger::Emergency`] ignores the window.
fn start_vacuum(
    db: &Database,
    conn: &mut Connection,
    trigger: VacuumTrigger,
) -> Option<Receiver<StorageResult<retention::VacuumOutcome>>> {
    let db_size = db.current_size_bytes().unwrap_or(0).max(0) as u64;
    let mount = db
        .path()
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "/".to_string());
    let usage = crate::libs::storage::disk::get_partition_usage(&mount);
    let available = (usage.total_bytes > 0).then_some(usage.available_bytes);

    if !retention::space_permits_vacuum(db_size, available) {
        let required = retention::vacuum_space_required(db_size);
        eprintln!(
            "STORAGE THREAD: skipping VACUUM — needs {} bytes free on {}, has {:?}; \
             pages stay unreclaimed until space is freed",
            required, mount, available
        );
        let _ = crate::libs::storage::audit::AuditLogger::log_operation_with_details(
            conn,
            "DATABASE_MAINTENANCE_SKIPPED",
            Some("sensor_readings"),
            &format!(r#"{{"reason":"insufficient_space","required_bytes":{required}}}"#),
        );
        return None;
    }

    // Fold the WAL back before handing the file over, so the rewrite is not
    // also republishing a large WAL. This is the checkpoint that used to be a
    // no-op — see `checkpoint`.
    if let Err(e) = checkpoint(conn, "TRUNCATE") {
        eprintln!("STORAGE THREAD: pre-VACUUM checkpoint failed: {}", e);
    }

    let _ = crate::libs::storage::audit::AuditLogger::log_operation_with_details(
        conn,
        "DATABASE_MAINTENANCE_STARTED",
        Some("sensor_readings"),
        &format!(
            r#"{{"trigger":"{}","bytes_before":{db_size}}}"#,
            trigger.as_str()
        ),
    );

    let (tx, rx) = bounded(1);
    let db_path = db.path().to_path_buf();
    match thread::Builder::new()
        .name("fiber-vacuum".into())
        .spawn(move || {
            let _ = tx.send(retention::full_vacuum(&db_path));
        }) {
        Ok(_) => Some(rx),
        Err(e) => {
            eprintln!("STORAGE THREAD: could not spawn the VACUUM helper: {}", e);
            None
        }
    }
}

/// How long a caller waits for a reply from the storage thread.
///
/// The five request/reply methods below used a bare `recv()` with no timeout.
/// That was fine while every message was serviced in microseconds; it stops
/// being fine once the thread can be busy for the length of a database
/// rewrite, because then the LoRaWAN uplink path, node provisioning and the
/// BLE reading sink block *indefinitely*. A bounded wait degrades to "we
/// tried", which is what [`StorageHandle::flush_sync`] already does.
///
/// Generous rather than tight: the thread must be allowed to finish a queued
/// batch first, and a spurious timeout on a provisioning epoch is worse than a
/// slow one. Note this bounds a *stall*, never a loss — the write messages are
/// fire-and-forget and are not affected.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

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

    /// Write storage-window buckets into `sensor_readings_minute`
    WriteSensorBuckets {
        rows: Vec<crate::libs::storage::models::MinuteAggregateRow>,
    },

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

    /// Flush pending writes to disk
    Flush,

    /// Flush pending writes and *answer when it is actually done*.
    ///
    /// Every other write message here is fire-and-forget, which is fine while
    /// the process keeps running — the worker drains the channel eventually.
    /// It is not fine when the caller is about to cut power: `Flush` returning
    /// only means "queued". Because the worker is a single thread draining one
    /// FIFO channel, a reply to this also proves every message queued *before*
    /// it (e.g. the `WriteAuditEvent` for a power-off) has been committed.
    FlushSync { reply: Sender<StorageResult<()>> },

    /// Graceful shutdown
    Shutdown,

    // ===== Save-and-feed (Node stream + export cursor) =====
    /// Write a Node uplink or marker. Fire-and-forget — failures are logged.
    WriteNodeReading {
        dev_eui: String,
        provisioning_epoch: i64,
        ts: i64,
        received_at: i64,
        message_id: String,
        event_type: String,
        payload_json: String,
    },
    /// Write an EYE BLE tag reading (fire-and-forget).
    WriteBeaconReading {
        mac: String,
        ts: i64,
        received_at: i64,
        message_id: String,
        event_type: String,
        payload_json: String,
    },
    /// Append a `sticker_removed` marker event (fire-and-forget).
    AppendNodeRemoved { dev_eui: String, ts: i64 },
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
    /// Newest stored `ts` for `(mac, event_type)` — the archive resume point.
    ///
    /// EDIT (control-ble integration): the reader function already existed but took a
    /// `&Connection`, which only code running on the storage thread has.
    MaxBeaconReadingTs {
        mac: String,
        event_type: String,
        reply: Sender<StorageResult<Option<i64>>>,
    },
    /// Recent stored uplink timestamps for a Node, for deriving its cadence
    /// across a restart.
    GetRecentNodeUplinks {
        dev_eui: String,
        limit: usize,
        reply: Sender<StorageResult<Vec<i64>>>,
    },
    /// Advance the export cursor for a `(broker_id, stream)` pair.
    AdvanceExportCursor {
        broker_id: String,
        stream: String,
        new_id: i64,
    },
    /// Reset the export cursor for a `(broker_id, stream)` pair to 0.
    ResetExportCursor { broker_id: String, stream: String },
    /// Enforce retention on `sticker_readings` (delete rows older than
    /// `retention_seconds`, log a WARN for un-exported drops).
    EnforceNodeRetention { retention_seconds: i64 },
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

    /// Send the buckets of one closed storage window to be stored
    pub fn write_sensor_buckets(
        &self,
        rows: Vec<crate::libs::storage::models::MinuteAggregateRow>,
    ) -> StorageResult<()> {
        self.sender
            .send(StorageMessage::WriteSensorBuckets { rows })
            .map_err(|e| {
                crate::libs::storage::error::StorageError::ChannelError(format!(
                    "Failed to send bucket message: {}",
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
        self.sender.send(StorageMessage::Flush).map_err(|e| {
            crate::libs::storage::error::StorageError::ChannelError(format!(
                "Failed to send flush message: {}",
                e
            ))
        })
    }

    /// Flush pending writes and block until the storage thread confirms it.
    ///
    /// Use this instead of [`Self::flush`] when the process is about to stop
    /// existing — a power-off or reboot — and the caller needs the rows it just
    /// queued to be on disk rather than merely in the channel. The wait is
    /// bounded so a wedged storage thread degrades to "we tried" instead of
    /// hanging the caller forever.
    pub fn flush_sync(&self, timeout: Duration) -> StorageResult<()> {
        let (tx, rx) = bounded(1);
        self.sender
            .send(StorageMessage::FlushSync { reply: tx })
            .map_err(|e| {
                crate::libs::storage::error::StorageError::ChannelError(format!(
                    "Failed to send flush_sync: {}",
                    e
                ))
            })?;
        rx.recv_timeout(timeout).map_err(|e| {
            crate::libs::storage::error::StorageError::ChannelError(format!(
                "Timed out waiting for flush_sync reply: {}",
                e
            ))
        })?
    }

    /// Signal shutdown
    pub fn shutdown(&self) -> StorageResult<()> {
        self.sender.send(StorageMessage::Shutdown).map_err(|e| {
            crate::libs::storage::error::StorageError::ChannelError(format!(
                "Failed to send shutdown message: {}",
                e
            ))
        })
    }

    // ===== Save-and-feed (Node stream + export cursor) =====

    /// Send a Node reading to be persisted (fire-and-forget).
    pub fn write_node_reading(
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
            .send(StorageMessage::WriteNodeReading {
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
                    "Failed to send Node reading: {}",
                    e
                ))
            })
    }

    /// Send an EYE BLE tag reading to be persisted (fire-and-forget).
    pub fn write_beacon_reading(
        &self,
        mac: String,
        ts: i64,
        received_at: i64,
        message_id: String,
        event_type: String,
        payload_json: String,
    ) -> StorageResult<()> {
        self.sender
            .send(StorageMessage::WriteBeaconReading {
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
    pub fn append_node_removed(&self, dev_eui: String, ts: i64) -> StorageResult<()> {
        self.sender
            .send(StorageMessage::AppendNodeRemoved { dev_eui, ts })
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
        rx.recv_timeout(REPLY_TIMEOUT).map_err(|e| {
            crate::libs::storage::error::StorageError::ChannelError(format!(
                "Failed to receive bump_provisioning_epoch reply: {}",
                e
            ))
        })?
    }

    /// The most recent stored uplink timestamps for `dev_eui`, newest first.
    pub fn recent_node_uplinks(&self, dev_eui: String, limit: usize) -> StorageResult<Vec<i64>> {
        let (tx, rx) = bounded(1);
        self.sender
            .send(StorageMessage::GetRecentNodeUplinks {
                dev_eui,
                limit,
                reply: tx,
            })
            .map_err(|e| {
                crate::libs::storage::error::StorageError::ChannelError(format!(
                    "Failed to send get_recent_node_uplinks: {}",
                    e
                ))
            })?;
        rx.recv_timeout(REPLY_TIMEOUT).map_err(|e| {
            crate::libs::storage::error::StorageError::ChannelError(format!(
                "Failed to receive get_recent_node_uplinks reply: {}",
                e
            ))
        })?
    }

    /// Newest stored `ts` for `(mac, event_type)`, or `None` when nothing is stored yet.
    ///
    /// EDIT (control-ble integration): `ReadingSink::max_reading_ts`'s handle-side half.
    pub fn max_beacon_reading_ts(
        &self,
        mac: String,
        event_type: String,
    ) -> StorageResult<Option<i64>> {
        let (tx, rx) = bounded(1);
        self.sender
            .send(StorageMessage::MaxBeaconReadingTs {
                mac,
                event_type,
                reply: tx,
            })
            .map_err(|e| {
                crate::libs::storage::error::StorageError::ChannelError(format!(
                    "Failed to send max_beacon_reading_ts: {}",
                    e
                ))
            })?;
        rx.recv_timeout(REPLY_TIMEOUT).map_err(|e| {
            crate::libs::storage::error::StorageError::ChannelError(format!(
                "Failed to receive max_beacon_reading_ts reply: {}",
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
        rx.recv_timeout(REPLY_TIMEOUT).map_err(|e| {
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
    /// active Node).
    pub fn dev_eui_last_event_was_removal_or_absent(&self, dev_eui: String) -> StorageResult<bool> {
        let (tx, rx) = bounded(1);
        self.sender
            .send(StorageMessage::DevEuiLastEventWasRemovalOrAbsent { dev_eui, reply: tx })
            .map_err(|e| {
                crate::libs::storage::error::StorageError::ChannelError(format!(
                    "Failed to send dev_eui_last_event_was_removal_or_absent: {}",
                    e
                ))
            })?;
        rx.recv_timeout(REPLY_TIMEOUT).map_err(|e| {
            crate::libs::storage::error::StorageError::ChannelError(format!(
                "Failed to receive dev_eui_last_event_was_removal_or_absent reply: {}",
                e
            ))
        })?
    }

    /// Enforce retention on `sticker_readings`.
    pub fn enforce_node_retention(&self, retention_seconds: i64) -> StorageResult<()> {
        self.sender
            .send(StorageMessage::EnforceNodeRetention { retention_seconds })
            .map_err(|e| {
                crate::libs::storage::error::StorageError::ChannelError(format!(
                    "Failed to send enforce_node_retention: {}",
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
    pub fn spawn(
        db_path: &str,
        max_size_gb: i32,
    ) -> StorageResult<(StorageHandle, thread::JoinHandle<()>)> {
        Self::spawn_with_hmac(db_path, max_size_gb, None)
    }

    /// Spawn with a precomputed byte cap (honors sub-GB granularity).
    pub fn spawn_with_max_bytes(
        db_path: &str,
        max_size_bytes: i64,
        hmac_secret_path: Option<&str>,
        window: Option<MaintenanceWindow>,
    ) -> StorageResult<(StorageHandle, thread::JoinHandle<()>)> {
        Self::spawn_with_hmac_and_max_bytes(db_path, max_size_bytes, hmac_secret_path, window)
    }

    /// Spawn the background storage thread with optional HMAC secret path
    /// If hmac_secret_path is provided, loads the HMAC key for sensor reading integrity (EU MDR)
    pub fn spawn_with_hmac(
        db_path: &str,
        max_size_gb: i32,
        hmac_secret_path: Option<&str>,
    ) -> StorageResult<(StorageHandle, thread::JoinHandle<()>)> {
        // No window: the convenience constructors are used by tests, and a
        // test that wants a schedule asks for one explicitly.
        Self::spawn_with_hmac_and_max_bytes(
            db_path,
            (max_size_gb.max(1) as i64) * 1024 * 1024 * 1024,
            hmac_secret_path,
            None,
        )
    }

    fn spawn_with_hmac_and_max_bytes(
        db_path: &str,
        max_size_bytes: i64,
        hmac_secret_path: Option<&str>,
        window: Option<MaintenanceWindow>,
    ) -> StorageResult<(StorageHandle, thread::JoinHandle<()>)> {
        let db_path = db_path.to_string();

        // A window whose `start_utc` does not parse is treated as no window at
        // all, loudly. Silently defaulting it to midnight would put a rewrite
        // at an hour nobody chose, on a device that monitors patients.
        let window = window.filter(|w| match w.start_minute_utc() {
            Some(_) if w.duration_minutes > 0 => true,
            _ => {
                eprintln!(
                    "STORAGE THREAD: maintenance_window {:?}/{} minutes is not a valid \
                     24-hour HH:MM window; scheduled maintenance is DISABLED",
                    w.start_utc, w.duration_minutes
                );
                false
            }
        });
        match &window {
            Some(w) => eprintln!(
                "STORAGE THREAD: scheduled maintenance window {} UTC for {} minutes",
                w.start_utc, w.duration_minutes
            ),
            None => eprintln!(
                "STORAGE THREAD: no maintenance window; a rewrite runs only on the \
                 emergency threshold"
            ),
        }

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
                Self::run(
                    &db_path,
                    max_size_bytes,
                    receiver,
                    hmac_secret.as_deref(),
                    window,
                );
            })
            .expect("Failed to spawn storage thread");

        Ok((handle, thread_handle))
    }

    /// Main storage thread loop
    fn run(
        db_path: &str,
        max_size_bytes: i64,
        receiver: Receiver<StorageMessage>,
        hmac_secret: Option<&[u8]>,
        // Already validated by the caller: a malformed window arrives here as
        // `None`, never as a silently-defaulted midnight.
        window: Option<MaintenanceWindow>,
    ) {
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

        // The spill journal: where durable writes go while a rewrite holds the
        // main database's write lock, instead of queueing in the bounded
        // channel's RAM. Opened before the first message is accepted, and
        // drained immediately — a non-empty journal here can only have been
        // left by a process that died mid-rewrite, and those writes were
        // already reported as accepted to their producers.
        let mut journal = match SpillJournal::open(
            db.path(),
            match Database::encryption_key() {
                Ok(key) => key,
                Err(e) => {
                    eprintln!(
                        "STORAGE THREAD: cannot read the DB key for the spill journal: {}",
                        e
                    );
                    return;
                }
            }
            .as_deref(),
        ) {
            Ok(j) => j,
            Err(e) => {
                // Hard stop, like a failed initial connection above: without
                // the journal a maintenance run would be back to queueing
                // accepted writes in RAM, which is the defect this replaces.
                eprintln!("STORAGE THREAD: Failed to open the spill journal: {}", e);
                return;
            }
        };
        match journal.drain(&mut conn, hmac_secret) {
            Ok(0) => {}
            Ok(n) => {
                eprintln!("STORAGE THREAD: replayed {} write(s) left by a previous run", n);
                // A non-empty journal at startup means the previous process
                // died mid-rewrite. The database is unharmed — VACUUM is
                // transactional and rolls back whole — but that has to be
                // readable from the audit trail rather than inferred from a
                // STARTED row with no FINISHED plus a journald line nobody
                // kept.
                let _ = crate::libs::storage::audit::AuditLogger::log_operation_with_details(
                    &conn,
                    "DATABASE_MAINTENANCE_INTERRUPTED",
                    Some("sensor_readings"),
                    &format!(r#"{{"reason":"process_died_mid_rewrite","replayed":{n}}}"#),
                );
            }
            Err(e) => eprintln!(
                "STORAGE THREAD: startup replay stopped: {} (the rows stay queued for the next drain)",
                e
            ),
        }

        /// A rewrite in flight, and how the helper thread reports back.
        ///
        /// While this is `Some`, every durable write goes to the journal
        /// instead of the locked database. It is cleared only after the
        /// journal has been replayed, so nothing newer can overtake the
        /// backlog.
        type VacuumInFlight = Receiver<StorageResult<retention::VacuumOutcome>>;
        let mut vacuum_in_flight: Option<VacuumInFlight> = None;
        let mut spilled_this_run = 0u64;
        // Writes the journal refused during a rewrite, which `accept_durably`
        // put straight into the database instead. Non-zero means the journal
        // was faulty, not that anything was lost — worth seeing in the audit
        // row next to `spilled`.
        let mut written_directly_this_run = 0u64;

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

        // Raw retention sweep: every hour, drop raw rows older than 30 days
        // whose minute has already been folded into the aggregate. Without
        // this the raw table grows ~90MB/day and fills /data in ~5 weeks.
        const RAW_RETENTION_SECONDS: i64 = 30 * 24 * 3600;
        let raw_retention_interval = Duration::from_secs(3600);
        let mut last_raw_retention_run = std::time::Instant::now();

        // Node retention sweep: every hour, drop sticker_readings older
        // than 30 days. The matching aggregate (probe_1m / minute aggregates)
        // is shipped via the export pipeline and replayed on demand by
        // viewers, so raw Node rows past the live window are safe to drop.
        // Without this the table grew unbounded — the StorageHandle method
        // exists but had no scheduler hooked up.
        const NODE_RETENTION_SECONDS: i64 = 30 * 24 * 3600;
        let node_retention_interval = Duration::from_secs(3600);
        let mut last_node_retention_run = std::time::Instant::now();

        // EYE retention sweep: every hour, drop eye_readings older than 30
        // days, mirroring the sticker_readings policy above. Without this
        // eye_readings grows unbounded for as long as configured tags keep
        // advertising, eventually filling /data.
        const BEACON_RETENTION_SECONDS: i64 = 30 * 24 * 3600;
        let beacon_retention_interval = Duration::from_secs(3600);
        let mut last_beacon_retention_run = std::time::Instant::now();
        let mut last_maintenance_check = std::time::Instant::now();
        // `None` runs the first capacity check on the first idle tick, so a Hub
        // that restarts on a full disk starts freeing space at once.
        let mut last_capacity_check: Option<std::time::Instant> = None;
        // A rewrite the size trigger asked for and the window has not opened
        // for yet. Held rather than dropped, so the next capacity pass does
        // not have to re-derive it.
        let mut vacuum_owed = false;

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
                    // Not while a rewrite holds the write lock.
                    //
                    // Every job below writes to the *main* connection, and each
                    // one derives its work from rows that are already durable —
                    // the aggregator from `sensor_readings`, the three sweeps
                    // from the tables they prune, the audit summary from a
                    // counter this thread owns. So none of them is spilled;
                    // all of them are simply postponed, which is what makes one
                    // guard right instead of a per-job decision.
                    //
                    // Without it they contend with the `VACUUM` for the lock,
                    // block this thread for the full `busy_timeout`, and then
                    // fail — the storage thread stops draining its channel for
                    // no gain. Deadlines are deliberately *not* advanced by a
                    // suppressed tick, so every skipped job runs on the first
                    // tick after the replay. `audit_summary_count` keeps
                    // accumulating across the suppression too, so the
                    // `INSERT_BATCH` row that follows still covers the whole
                    // extended interval exactly.
                    if vacuum_in_flight.is_none() {
                        // Timeout - do periodic flush if needed
                        if pending_writes > 0 && last_flush.elapsed() > flush_interval {
                            if let Err(e) = checkpoint(&conn, "PASSIVE") {
                                eprintln!("STORAGE THREAD: periodic checkpoint failed: {}", e);
                            }
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
                        // No per-minute roll-up: the sensor monitor writes one
                        // bucket per storage window itself, and `sensor_readings`
                        // only holds alarm transitions now
                        // (plan/control/storage_intervals.md §4).
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
                        if last_node_retention_run.elapsed() >= node_retention_interval
                            && consecutive_write_failures < RECONNECT_FAILURE_THRESHOLD
                        {
                            match RetentionPolicy::default()
                                .sweep_node_readings(&mut conn, NODE_RETENTION_SECONDS)
                            {
                                Ok(stats) if stats.purged > 0 => {
                                    eprintln!(
                                        "STORAGE THREAD: Node retention swept {} sticker_readings rows older than {} days (unexported_dropped={})",
                                        stats.purged,
                                        NODE_RETENTION_SECONDS / 86400,
                                        stats.unexported_dropped,
                                    );
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    eprintln!("STORAGE THREAD: Node retention sweep failed: {}", e);
                                }
                            }
                            last_node_retention_run = std::time::Instant::now();
                        }
                        if last_beacon_retention_run.elapsed() >= beacon_retention_interval
                            && consecutive_write_failures < RECONNECT_FAILURE_THRESHOLD
                        {
                            match RetentionPolicy::default()
                                .sweep_beacon_readings(&mut conn, BEACON_RETENTION_SECONDS)
                            {
                                Ok(stats) if stats.purged > 0 => {
                                    eprintln!(
                                        "STORAGE THREAD: eye retention swept {} eye_readings rows older than {} days (unexported_dropped={})",
                                        stats.purged,
                                        BEACON_RETENTION_SECONDS / 86400,
                                        stats.unexported_dropped,
                                    );
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    eprintln!("STORAGE THREAD: eye retention sweep failed: {}", e);
                                }
                            }
                            last_beacon_retention_run = std::time::Instant::now();
                        }

                        // Capacity pass (#124): the cap and the free space on
                        // `/data`, checked on a clock. It used to run every
                        // 10 000 messages, which a quiet Hub takes days to
                        // reach. Deliberately not behind the write-failure
                        // guard: a full disk is what makes the writes fail,
                        // and deleting is how they recover.
                        if last_capacity_check
                            .is_none_or(|t| t.elapsed() >= CAPACITY_CHECK_INTERVAL)
                        {
                            if last_capacity_check.is_none() {
                                match retention::Capacity::measure(&db, &conn) {
                                    Ok(c) => eprintln!(
                                        "STORAGE THREAD: capacity: {}",
                                        retention_policy.describe(&c)
                                    ),
                                    Err(e) => eprintln!("STORAGE THREAD: capacity: {}", e),
                                }
                            }
                            last_capacity_check = Some(std::time::Instant::now());
                            match retention_policy.needs_cleanup(&db, &conn) {
                                Ok(true) => match retention_policy.enforce(&db, &mut conn) {
                                    // Out of cap space: reclaiming beats
                                    // waiting for 02:30, if the partition has
                                    // room for the rewrite at all.
                                    Ok(stats)
                                        if stats.vacuum == retention::VacuumUrgency::Emergency =>
                                    {
                                        vacuum_in_flight =
                                            start_vacuum(&db, &mut conn, VacuumTrigger::Emergency);
                                    }
                                    // The freed pages are reused as they are;
                                    // the repack only shrinks the file, so it
                                    // waits for the window. With no window
                                    // configured it runs now, or never.
                                    Ok(stats)
                                        if stats.vacuum == retention::VacuumUrgency::Deferred =>
                                    {
                                        if window.is_none() {
                                            vacuum_in_flight = start_vacuum(
                                                &db,
                                                &mut conn,
                                                VacuumTrigger::Retention,
                                            );
                                        } else if !vacuum_owed {
                                            eprintln!(
                                                "STORAGE THREAD: a rewrite is owed; \
                                                 waiting for the maintenance window"
                                            );
                                            vacuum_owed = true;
                                        }
                                    }
                                    Ok(_) => {}
                                    Err(e) => eprintln!("STORAGE THREAD: retention failed: {}", e),
                                },
                                Ok(false) => {}
                                Err(e) => eprintln!("STORAGE THREAD: capacity check failed: {}", e),
                            }
                        }

                        // The maintenance window. Inside the housekeeping guard
                        // because a rewrite already in flight must not start a
                        // second one, and outside the size trigger because that
                        // one counts messages, which stop arriving at night —
                        // precisely when the window opens.
                        if last_maintenance_check.elapsed() >= MAINTENANCE_CHECK_INTERVAL {
                            last_maintenance_check = std::time::Instant::now();
                            if vacuum_owed && in_maintenance_window(window.as_ref()) {
                                eprintln!(
                                    "STORAGE THREAD: maintenance window is open and a rewrite is \
                                     owed; starting it"
                                );
                                vacuum_in_flight =
                                    start_vacuum(&db, &mut conn, VacuumTrigger::Schedule);
                                if vacuum_in_flight.is_some() {
                                    vacuum_owed = false;
                                }
                            }
                        }
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
                                reconnect_backoff =
                                    std::cmp::min(reconnect_backoff * 2, RECONNECT_BACKOFF_MAX);
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

            // A rewrite in flight? Check whether it finished, and if so replay
            // the backlog *before* touching any newer work. Placed here rather
            // than in the timeout arm so it runs on every iteration, busy or
            // idle.
            if let Some(rx) = vacuum_in_flight.as_ref() {
                if let Some(outcome) = vacuum_report(rx) {
                    // Audited from this thread, never from the helper: the
                    // `AuditLogger` hash-chain has no locking, so a second
                    // concurrent writer would fork it.
                    match outcome {
                        Ok(o) => {
                            eprintln!(
                                "STORAGE THREAD: VACUUM finished in {}ms, {} -> {} bytes",
                                o.elapsed_ms, o.bytes_before, o.bytes_after
                            );
                            let _ = crate::libs::storage::audit::AuditLogger::log_operation_with_details(
                                &conn,
                                "DATABASE_MAINTENANCE_FINISHED",
                                Some("sensor_readings"),
                                &format!(
                                    r#"{{"elapsed_ms":{},"bytes_before":{},"bytes_after":{},"spilled":{},"written_directly":{}}}"#,
                                    o.elapsed_ms,
                                    o.bytes_before,
                                    o.bytes_after,
                                    spilled_this_run,
                                    written_directly_this_run
                                ),
                            );
                        }
                        Err(e) => {
                            eprintln!("STORAGE THREAD: VACUUM failed: {}", e);
                            let _ = crate::libs::storage::audit::AuditLogger::log_error(
                                &conn,
                                "DATABASE_MAINTENANCE_FAILED",
                                Some("sensor_readings"),
                                &e.to_string(),
                                None,
                            );
                        }
                    }

                    match journal.drain(&mut conn, hmac_secret) {
                        Ok(n) if n > 0 => {
                            eprintln!("STORAGE THREAD: replayed {} deferred write(s)", n)
                        }
                        Ok(_) => {}
                        Err(e) => eprintln!(
                            "STORAGE THREAD: replay stopped: {} (rows stay queued for the next drain)",
                            e
                        ),
                    }

                    // Cleared only now: a direct write must never overtake the
                    // backlog.
                    vacuum_in_flight = None;
                    spilled_this_run = 0;
                    written_directly_this_run = 0;
                }
            }

            if let Some(msg) = msg {
                // While the rewrite holds the write lock, durable writes are
                // committed to the journal instead. That commit *is* the
                // acceptance point — unlike the bounded channel, which is RAM a
                // power cut takes with it. Queries and control messages fall
                // through and are served live; WAL readers are not blocked by a
                // VACUUM, and a reply channel is meaningless after a restart.
                let msg = if vacuum_in_flight.is_some() {
                    match spill::route(msg) {
                        Routed::Spill(record) => {
                            match accept_durably(&journal, &mut conn, &record, hmac_secret) {
                                Accepted::Spilled => spilled_this_run += 1,
                                Accepted::WrittenDirectly => written_directly_this_run += 1,
                            }
                            if spilled_this_run > 0 && spilled_this_run.is_multiple_of(1000) {
                                eprintln!(
                                    "STORAGE THREAD: {} write(s) waiting in the spill journal",
                                    spilled_this_run
                                );
                            }
                            continue;
                        }
                        // Derived from rows that are already durable, so the
                        // next tick redoes it for free. Blocking on the
                        // rewrite's lock instead would stall this thread for
                        // `busy_timeout` and then fail anyway.
                        Routed::Postpone(job) => {
                            eprintln!(
                                "STORAGE THREAD: postponing {job} until the rewrite finishes"
                            );
                            continue;
                        }
                        Routed::Serve(msg) => msg,
                    }
                } else {
                    msg
                };

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
                                consecutive_write_failures =
                                    consecutive_write_failures.saturating_add(1);
                            }
                        }
                    }

                    StorageMessage::WriteSensorReadingsBatch { readings } => {
                        match StorageWriter::write_sensor_readings_batch(
                            &mut conn,
                            &readings,
                            hmac_secret,
                        ) {
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
                                consecutive_write_failures =
                                    consecutive_write_failures.saturating_add(1);
                            }
                        }
                    }

                    StorageMessage::WriteSensorBuckets { rows } => {
                        let mut ok = 0usize;
                        for row in &rows {
                            match StorageWriter::write_sensor_bucket(&conn, row, hmac_secret) {
                                Ok(_) => ok += 1,
                                Err(e) => {
                                    eprintln!("STORAGE THREAD: Failed to write bucket: {}", e);
                                    consecutive_write_failures =
                                        consecutive_write_failures.saturating_add(1);
                                }
                            }
                        }
                        if ok > 0 {
                            pending_writes += ok;
                            message_count += ok as u64;
                            consecutive_write_failures = 0;
                            reconnect_backoff = RECONNECT_BACKOFF_INITIAL;
                            next_reconnect_attempt = None;
                        }
                    }

                    StorageMessage::WriteAlarmEvent {
                        timestamp,
                        sensor_line,
                        from_state,
                        to_state,
                        temperature,
                    } => {
                        let event = AlarmEvent::new(
                            timestamp,
                            sensor_line,
                            from_state,
                            to_state,
                            temperature,
                        );

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
                        if let Err(e) = checkpoint(&conn, "PASSIVE") {
                            eprintln!("STORAGE THREAD: flush checkpoint failed: {}", e);
                        }
                        pending_writes = 0;
                        last_flush = std::time::Instant::now();
                    }

                    StorageMessage::FlushSync { reply } => {
                        // RESTART, not PASSIVE: the caller is about to lose
                        // power, so we want the WAL fully folded back into the
                        // main database rather than a best-effort partial
                        // checkpoint. Same choice the Shutdown handler makes.
                        // See `checkpoint` for why this cannot be `execute`.
                        let result = checkpoint(&conn, "RESTART").map_err(|e| {
                            crate::libs::storage::error::StorageError::QueryError(format!(
                                "wal_checkpoint(RESTART) failed: {}",
                                e
                            ))
                        });
                        pending_writes = 0;
                        last_flush = std::time::Instant::now();
                        // The receiver may have timed out and gone away; that is
                        // its problem, not ours — the checkpoint still happened.
                        let _ = reply.send(result);
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

                    StorageMessage::WriteNodeReading {
                        dev_eui,
                        provisioning_epoch,
                        ts,
                        received_at,
                        message_id,
                        event_type,
                        payload_json,
                    } => {
                        match StorageWriter::write_node_reading(
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
                                eprintln!("STORAGE THREAD: write_node_reading failed: {}", e);
                            }
                        }
                    }

                    StorageMessage::WriteBeaconReading {
                        mac,
                        ts,
                        received_at,
                        message_id,
                        event_type,
                        payload_json,
                    } => {
                        match StorageWriter::write_beacon_reading(
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
                                eprintln!("STORAGE THREAD: write_beacon_reading failed: {}", e);
                            }
                        }
                    }

                    StorageMessage::AppendNodeRemoved { dev_eui, ts } => {
                        match StorageWriter::append_node_removed_event(&mut conn, &dev_eui, ts) {
                            Ok(_) => {
                                pending_writes += 1;
                                message_count += 1;
                            }
                            Err(e) => {
                                eprintln!(
                                    "STORAGE THREAD: append_node_removed_event failed: {}",
                                    e
                                );
                            }
                        }
                    }

                    StorageMessage::BumpProvisioningEpoch { dev_eui, reply } => {
                        let _ =
                            reply.send(StorageWriter::bump_provisioning_epoch(&mut conn, &dev_eui));
                    }

                    StorageMessage::GetProvisioningEpoch { dev_eui, reply } => {
                        let _ = reply.send(StorageWriter::get_provisioning_epoch(&conn, &dev_eui));
                    }

                    StorageMessage::MaxBeaconReadingTs {
                        mac,
                        event_type,
                        reply,
                    } => {
                        let _ = reply.send(
                            crate::libs::storage::reader::StorageReader::max_beacon_reading_ts_of(
                                &conn,
                                &mac,
                                &event_type,
                            ),
                        );
                    }

                    StorageMessage::GetRecentNodeUplinks {
                        dev_eui,
                        limit,
                        reply,
                    } => {
                        let _ = reply.send(
                            crate::libs::storage::reader::StorageReader::recent_node_uplink_times(
                                &conn, &dev_eui, limit,
                            ),
                        );
                    }

                    StorageMessage::AdvanceExportCursor {
                        broker_id,
                        stream,
                        new_id,
                    } => {
                        if let Err(e) = StorageWriter::advance_export_cursor(
                            &mut conn, &broker_id, &stream, new_id,
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

                    StorageMessage::WriteAuditEvent {
                        operation,
                        table_name,
                        details,
                    } => {
                        // Fire-and-forget audit row. Used by config-applier
                        // paths (e.g. device label changes) that don't
                        // otherwise touch the database. record_count and
                        // duration_ms aren't meaningful for these events.
                        let result = match details.as_deref() {
                            Some(d) => {
                                crate::libs::storage::audit::AuditLogger::log_operation_with_details(
                                    &conn,
                                    &operation,
                                    table_name.as_deref(),
                                    d,
                                )
                            }
                            None => crate::libs::storage::audit::AuditLogger::log_operation(
                                &conn,
                                &operation,
                                table_name.as_deref(),
                                None,
                                None,
                            ),
                        };
                        if let Err(e) = result {
                            eprintln!("STORAGE THREAD: audit '{}' failed: {}", operation, e);
                        }
                    }

                    StorageMessage::EnforceNodeRetention { retention_seconds } => {
                        match RetentionPolicy::default()
                            .sweep_node_readings(&mut conn, retention_seconds)
                        {
                            Ok(r) => {
                                if r.purged > 0 {
                                    eprintln!(
                                        "STORAGE THREAD: Node retention purged {} rows ({} un-exported)",
                                        r.purged, r.unexported_dropped
                                    );
                                }
                            }
                            Err(e) => {
                                eprintln!("STORAGE THREAD: sweep_node_readings failed: {}", e);
                            }
                        }
                    }

                    StorageMessage::Shutdown => {
                        if pending_writes > 0 {
                            if let Err(e) = checkpoint(&conn, "RESTART") {
                                eprintln!("STORAGE THREAD: shutdown checkpoint failed: {}", e);
                            }
                            eprintln!(
                                "STORAGE THREAD: Final flush of {} pending writes",
                                pending_writes
                            );
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
                    if let Err(e) = checkpoint(&conn, "PASSIVE") {
                        eprintln!("STORAGE THREAD: periodic checkpoint failed: {}", e);
                    }
                    pending_writes = 0;
                    last_flush = std::time::Instant::now();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// The trap `FlushSync` documented and the rest of this file then copied
    /// four times: `PRAGMA wal_checkpoint` yields a row, and rusqlite's
    /// `execute` refuses any statement that does. Every
    /// `let _ = conn.execute("PRAGMA wal_checkpoint(...)")` was therefore a
    /// silent no-op — the periodic flush, `Flush`, and `Shutdown` never
    /// actually checkpointed.
    ///
    /// Pinned both ways, because the failure mode is invisible: the wrong call
    /// still compiles and still "succeeds" from the caller's point of view.
    #[test]
    fn a_checkpoint_actually_checkpoints() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = crate::libs::storage::db::Database::new(dir.path().join("fiber.db"), 5)
            .expect("open db");
        let conn = db.connect().expect("connect");

        assert!(
            conn.execute("PRAGMA wal_checkpoint(PASSIVE)", []).is_err(),
            "if this ever starts succeeding, rusqlite changed and this file's \
             `checkpoint` helper can be simplified — until then, `execute` is a no-op"
        );

        for mode in ["PASSIVE", "RESTART", "TRUNCATE"] {
            checkpoint(&conn, mode).unwrap_or_else(|e| panic!("checkpoint({mode}) must work: {e}"));
        }
    }

    use super::*;

    /// A journal that will refuse every append, for the ladder tests below.
    /// Dropping its own table is the bluntest permanent failure available, and
    /// it fails the way a real one would — at `INSERT`, not at open.
    fn broken_journal(main_db: &std::path::Path) -> SpillJournal {
        let journal = SpillJournal::open(main_db, None).expect("open journal");
        rusqlite::Connection::open(spill::spill_path(main_db))
            .expect("open the journal file directly")
            .execute_batch("DROP TABLE spill")
            .expect("break the journal");
        journal
    }

    fn a_reading(ts: i64) -> SpillRecord {
        SpillRecord::SensorReading {
            timestamp: ts,
            sensor_line: 0,
            temperature: 36.5,
            is_connected: true,
            alarm_state: AlarmState::Normal,
        }
    }

    /// A rewrite whose helper thread panicked must still end the rewrite.
    ///
    /// The regression: `if let Ok(outcome) = rx.try_recv()` swallowed
    /// `Disconnected`, so a panicking `fiber-vacuum` thread left
    /// `vacuum_in_flight` set for the life of the process — every durable write
    /// spilling forever into an uncapped journal, the aggregator and all three
    /// retention sweeps suppressed, and the emergency reclaim gated off. Only a
    /// restart recovered.
    #[test]
    fn a_vacuum_helper_that_panics_still_ends_the_rewrite() {
        let (tx, rx) = bounded::<StorageResult<retention::VacuumOutcome>>(1);

        assert!(
            vacuum_report(&rx).is_none(),
            "a helper that has not reported yet leaves the rewrite in flight"
        );

        let helper = thread::spawn(move || {
            let _tx = tx;
            panic!("the rewrite blew up");
        });
        assert!(helper.join().is_err(), "the helper must have panicked");

        let report = vacuum_report(&rx).expect("a disconnected helper must end the rewrite");
        let err = report.expect_err("and must be recorded as a failure, not a success");
        assert!(
            err.to_string().contains("panicked"),
            "the audit row must say what happened: {err}"
        );
    }

    /// The ordinary paths are unchanged: a helper that reports is reported.
    #[test]
    fn a_vacuum_helper_that_reports_is_read_normally() {
        let (tx, rx) = bounded::<StorageResult<retention::VacuumOutcome>>(1);
        tx.send(Ok(retention::VacuumOutcome {
            elapsed_ms: 12,
            bytes_before: 200,
            bytes_after: 100,
        }))
        .expect("send");

        let outcome = vacuum_report(&rx)
            .expect("a reported outcome ends the rewrite")
            .expect("and it succeeded");
        assert_eq!(outcome.bytes_after, 100);
    }

    /// Step 2 of `accept_durably`'s ladder. This used to be a `continue` that
    /// logged "at risk of being lost" and dropped the row — the one outcome
    /// plan/portal/sqlite-vacuum.md §3 forbids.
    #[test]
    fn a_write_the_journal_refuses_is_written_to_the_database_rather_than_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let main = dir.path().join("fiber_medical.db");
        let db = Database::new(&main, 5).expect("open db");
        let mut conn = db.connect().expect("connect");
        let journal = broken_journal(&main);

        assert_eq!(
            accept_durably(&journal, &mut conn, &a_reading(1_000), None),
            Accepted::WrittenDirectly
        );

        let stored = StorageReader::get_last_readings(&conn, 0, 10).expect("read back");
        assert_eq!(stored.len(), 1, "the refused write must still be on disk");
        assert_eq!(stored[0].timestamp, 1_000);
    }

    /// Step 3: when neither the journal nor the database will take a write, the
    /// thread stalls rather than dropping it. The assertion is not "it fails
    /// gracefully" — it is that it *never returns* having lost the row, so the
    /// test proves the stall and then repairs the journal to prove it resumes.
    #[test]
    fn a_write_that_neither_the_journal_nor_the_database_can_take_stalls_until_one_can() {
        let dir = tempfile::tempdir().expect("tempdir");
        let main = dir.path().join("fiber_medical.db");
        let db = Database::new(&main, 5).expect("open db");
        let journal_path = spill::spill_path(&main);
        let journal = broken_journal(&main);

        // Hold the main database exclusively, so step 2 cannot succeed either.
        let blocker = db.connect().expect("connect blocker");
        blocker
            .execute_batch("BEGIN EXCLUSIVE")
            .expect("hold the write lock");

        let (done_tx, done_rx) = bounded::<Accepted>(1);
        let worker = thread::spawn(move || {
            let mut conn = db.connect().expect("connect worker");
            // 50 ms, not the production 30 s: step 2 of the ladder is *supposed*
            // to wait out a lock, and here it never gets it, so a realistic
            // timeout would make one pass of the loop take half a minute. The
            // property under test is the loop, not the timeout.
            conn.busy_timeout(Duration::from_millis(50))
                .expect("shorten the wait");
            let _ = done_tx.send(accept_durably(&journal, &mut conn, &a_reading(2_000), None));
        });

        assert!(
            done_rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "with nowhere to put the row, it must still be trying — not returning"
        );

        // Repair the journal; the next pass through the ladder must take it.
        rusqlite::Connection::open(&journal_path)
            .expect("reopen the journal file")
            .execute_batch(
                "CREATE TABLE spill (id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 ts INTEGER NOT NULL, payload BLOB NOT NULL)",
            )
            .expect("repair the journal");

        let outcome = done_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the stall must end once the journal works again");
        assert_eq!(outcome, Accepted::Spilled);
        worker.join().expect("worker");

        drop(blocker);
    }

    #[test]
    fn test_storage_handle_creation() {
        let (handle, _) =
            StorageThread::spawn("/tmp/test_thread.db", 5).expect("Failed to spawn thread");

        // Send a write message
        let result = handle.write_sensor_reading(1000, 0, 36.5, true, AlarmState::Normal);
        assert!(result.is_ok());

        let _ = handle.shutdown();
        let _ = std::fs::remove_file("/tmp/test_thread.db");
    }

    #[test]
    fn storage_handle_write_node_reading_persists_via_thread() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        let (handle, join) = StorageThread::spawn(&path, 1).unwrap();
        handle
            .write_node_reading(
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
    fn flush_sync_makes_a_queued_audit_row_readable_without_shutting_down() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        let (handle, join) = StorageThread::spawn(&path, 1).unwrap();

        handle
            .log_audit_event(
                "POWER_OFF".to_string(),
                Some("audit_log".to_string()),
                Some(r#"{"reason":"decommissioned","requested_by":"dr.jane"}"#.to_string()),
            )
            .unwrap();

        // The point of the whole primitive: this returns only once the worker has
        // committed and checkpointed. Deliberately NO shutdown()/join() before
        // the read below — that is what a power-off does not get to do, and it is
        // exactly the property plain flush() cannot provide.
        handle.flush_sync(Duration::from_secs(5)).unwrap();

        let db = Database::new(&path, 1).unwrap();
        let conn = db.connect().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audit_log WHERE operation = 'POWER_OFF'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "audit row was not durable after flush_sync");

        let details: String = conn
            .query_row(
                "SELECT details FROM audit_log WHERE operation = 'POWER_OFF'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            details.contains("dr.jane"),
            "signer missing from audit row: {details}"
        );

        handle.shutdown().unwrap();
        join.join().unwrap();
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
        assert!(handle
            .dev_eui_last_event_was_removal_or_absent("abc".into())
            .unwrap());

        handle
            .write_node_reading(
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
        assert!(!handle
            .dev_eui_last_event_was_removal_or_absent("abc".into())
            .unwrap());

        handle.append_node_removed("abc".into(), 1100).unwrap();
        handle.flush().unwrap();
        // Last event is sticker_removed → true
        assert!(handle
            .dev_eui_last_event_was_removal_or_absent("abc".into())
            .unwrap());

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
