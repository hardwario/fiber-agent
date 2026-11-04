//! Data retention policy enforcement
//! Age sweeps per table, plus a FIFO capacity pass when the database nears its
//! cap or its partition runs short of free space (#124).
//! Medical device compliance: maintains full data integrity while respecting storage constraints

use rusqlite::{Connection, OptionalExtension};
use std::path::Path;

use crate::libs::storage::audit::AuditLogger;
use crate::libs::storage::db::Database;
use crate::libs::storage::error::{StorageError, StorageResult};

/// Retention policy for medical data storage
pub struct RetentionPolicy {
    /// Maximum database size in bytes (5GB)
    max_size_bytes: i64,

    /// Percentage threshold for triggering cleanup (e.g., 90 = cleanup at 90%)
    cleanup_threshold_percent: f32,

    /// Minimum age of records to delete (in seconds) - delete oldest first
    min_age_seconds: i64,
}

impl Default for RetentionPolicy {
    /// Default 5GB capacity policy. Convenience for Node-stream sweeping
    /// where caller does not have a configured `max_size_gb` to pass in.
    fn default() -> Self {
        Self::new(5)
    }
}

/// Result of a `sweep_node_readings` pass.
#[derive(Debug, Clone, Default)]
pub struct NodeRetentionResult {
    /// Total rows deleted from `sticker_readings`.
    pub purged: i64,
    /// Of those deleted, how many had `id > min(cursor)` — i.e. were dropped
    /// before any destination had a chance to export them. Emits a WARN log.
    pub unexported_dropped: i64,
}

impl RetentionPolicy {
    /// Create a new retention policy (5GB with 90% cleanup threshold)
    pub fn new(max_size_gb: i32) -> Self {
        Self::with_max_bytes((max_size_gb.max(1) as i64) * 1024 * 1024 * 1024)
    }

    /// Create a retention policy from a precomputed byte cap. Use this
    /// when the operator config specifies sub-GB precision (via
    /// `StorageConfig::max_size_mb`); the legacy `new(i32)` constructor
    /// is fine for whole-GB defaults.
    pub fn with_max_bytes(max_size_bytes: i64) -> Self {
        Self {
            max_size_bytes,
            cleanup_threshold_percent: 90.0,
            min_age_seconds: 60, // Don't delete records less than 1 minute old
        }
    }

    /// One log line with the capacity figures, for the first check after a
    /// start: the file size says nothing about how full the database is.
    pub fn describe(&self, c: &Capacity) -> String {
        let mib = |b: u64| b / MIB;
        format!(
            "live {} MiB, freelist {} MiB, partition free {}, cap {} MiB",
            mib(c.used_bytes),
            mib(c.freelist_bytes),
            c.available_bytes
                .map_or_else(|| "unknown".to_string(), |a| format!("{} MiB", mib(a))),
            self.max_size_bytes.max(0) as u64 / MIB,
        )
    }

    /// How many bytes a capacity pass has to free, 0 when none is needed.
    ///
    /// Two independent limits, whichever is short (#124):
    ///
    /// - **The cap.** Live pages (not the file size: DELETE returns pages to
    ///   the freelist, the file only shrinks on a `VACUUM`, and a `VACUUM` on a
    ///   2.9 GB `/data` never has room for a 2 GB copy) above
    ///   `cleanup_threshold_percent` of `max_size_bytes`.
    /// - **The partition.** Free space on it plus the freelist inside the file
    ///   below [`DISK_FREE_FLOOR`]. The cap alone cannot keep `/data` from
    ///   filling: the partition is shared with `portal.db`, the journal and
    ///   ChirpStack, and is smaller than the shipped cap on some Hubs.
    ///
    /// `started` is true inside a pass, which then runs on to the lower marks
    /// (85 % of the cap, [`DISK_FREE_TARGET`]) so the next write does not
    /// start another one.
    pub fn deficit(&self, c: &Capacity, started: bool) -> u64 {
        let cap = self.max_size_bytes.max(1) as u64;
        let target = cap / 100 * 85;
        let trigger = if started {
            target
        } else {
            (cap as f64 * (self.cleanup_threshold_percent as f64 / 100.0)) as u64
        };
        let over_cap = if c.used_bytes > trigger {
            c.used_bytes - target
        } else {
            0
        };
        let short_of_disk = match c.available_bytes {
            Some(available) => {
                let room = available.saturating_add(c.freelist_bytes);
                let floor = if started {
                    DISK_FREE_TARGET
                } else {
                    DISK_FREE_FLOOR
                };
                if room < floor {
                    DISK_FREE_TARGET - room
                } else {
                    0
                }
            }
            None => 0,
        };
        over_cap.max(short_of_disk)
    }

    /// Whether a capacity pass is due. See [`Self::deficit`].
    pub fn needs_cleanup(&self, db: &Database, conn: &Connection) -> StorageResult<bool> {
        Ok(self.deficit(&Capacity::measure(db, conn)?, false) > 0)
    }

    /// Get the percentage of storage currently used
    pub fn get_usage_percent(&self, db: &Database) -> StorageResult<f32> {
        db.get_utilization_percent()
    }

    /// Past [`EMERGENCY_UTILISATION_PERCENT`] of the cap — the point at which a
    /// rewrite stops waiting for a maintenance window.
    pub fn is_critically_full(&self, db: &Database) -> StorageResult<bool> {
        Ok(self.get_usage_percent(db)? >= EMERGENCY_UTILISATION_PERCENT)
    }

    /// Enforce the capacity limits: delete the oldest data until
    /// [`Self::deficit`] is met.
    ///
    /// Oldest first across the tables that hold measurements, one hour of
    /// data per batch and per short transaction:
    ///
    /// 1. `sensor_readings`, `eye_readings` and `sticker_readings` together,
    ///    by their own timestamp, so the oldest hour goes whatever wrote it.
    ///    The cap used to delete `sensor_readings` alone, which wiped the
    ///    Hub's wired probes and freed nothing on a Hub whose database is
    ///    mostly Beacon rows (#124).
    /// 2. Only when those hold nothing older than `min_age_seconds`, the
    ///    signed 1-Wire window buckets in `sensor_readings_minute`, again
    ///    oldest first. They are the long-term record, so they go last.
    ///
    /// `audit_log` and `alarm_events` are never touched. Progress is measured,
    /// not estimated: after every batch the live pages are counted again. A
    /// pass deletes at most [`MAX_BATCHES_PER_PASS`] hours, so a large backlog
    /// is worked off over several checks instead of one long stall.
    pub fn enforce(&self, db: &Database, conn: &mut Connection) -> StorageResult<RetentionStats> {
        let start = std::time::Instant::now();
        let before = Capacity::measure(db, conn)?;
        let deficit = self.deficit(&before, false);
        if deficit == 0 {
            return Ok(RetentionStats::default());
        }
        eprintln!(
            "RETENTION: {} MB live in {} MB cap, {} MB free on the partition + {} MB freelist; \
             freeing ~{} MB",
            before.used_bytes / MIB,
            self.max_size_bytes / MIB as i64,
            before
                .available_bytes
                .map_or_else(|| "?".to_string(), |a| (a / MIB).to_string()),
            before.freelist_bytes / MIB,
            deficit / MIB,
        );

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let max_age_ts = now - self.min_age_seconds;

        let mut stats = RetentionStats::default();
        let mut after = before;
        for _ in 0..MAX_BATCHES_PER_PASS {
            if self.deficit(&after, true) == 0 {
                break;
            }
            let Some(batch) = delete_oldest_hour(conn, max_age_ts)? else {
                eprintln!("RETENTION: nothing older than the minimum age is left to delete");
                break;
            };
            stats.deleted_count += batch.rows;
            stats.unexported_dropped += batch.unexported;
            if batch.summaries {
                stats.summaries_deleted += batch.rows;
            }
            stats.oldest_deleted_timestamp.get_or_insert(batch.from);
            after = Capacity::measure(db, conn)?;
        }
        stats.freed_bytes = before.used_bytes.saturating_sub(after.used_bytes) as i64;
        stats.duration_ms = start.elapsed().as_millis() as i64;

        if stats.unexported_dropped > 0 {
            eprintln!(
                "WARN [retention] capacity pass dropped {} rows no destination had exported",
                stats.unexported_dropped
            );
        }
        if stats.summaries_deleted > 0 {
            eprintln!(
                "WARN [retention] capacity pass deleted {} 1-Wire window buckets",
                stats.summaries_deleted
            );
        }

        // The DELETEs return pages to the freelist, which the next writes
        // reuse; the file does not shrink. A repack is optional now that the
        // limits are measured in live pages, so it is the caller's decision:
        // deferred to the maintenance window, or at once when the file is past
        // the emergency mark. SQLite forbids VACUUM inside a transaction, so
        // it never runs from here.
        stats.vacuum = if stats.deleted_count == 0 {
            VacuumUrgency::NotNeeded
        } else if self.is_critically_full(db)? {
            VacuumUrgency::Emergency
        } else {
            VacuumUrgency::Deferred
        };

        if stats.deleted_count > 0 {
            if let Some(ts) = stats.oldest_deleted_timestamp {
                let _ = AuditLogger::log_retention_cleanup(
                    conn,
                    stats.deleted_count,
                    ts,
                    stats.duration_ms,
                );
            }
            eprintln!(
                "RETENTION: deleted {} rows, {} MB live freed, in {}ms",
                stats.deleted_count,
                stats.freed_bytes / MIB as i64,
                stats.duration_ms
            );
        }
        Ok(stats)
    }

    /// Check if record is old enough to be eligible for deletion
    pub fn is_eligible_for_deletion(&self, record_timestamp: i64) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let record_age_seconds = now - record_timestamp;
        record_age_seconds > self.min_age_seconds
    }

    /// Sweep `sensor_readings`: delete raw rows older than
    /// `retention_seconds` that a bucket in `sensor_readings_minute` already
    /// contains, so raw data is never dropped before it is aggregated.
    ///
    /// A bucket contains a raw row of its line when
    /// `minute_ts <= timestamp < minute_ts + window_s`. `window_s` is the
    /// storage window that wrote the bucket (plan/control/storage_intervals.md
    /// §4); a NULL one is a pre-2026-10-12 one-minute roll-up and covers 60 s.
    /// No bucket is wider than `MAX_BUCKET_SECONDS`, which bounds the index
    /// range. A raw row no bucket contains survives.
    ///
    /// Returns the number of rows deleted.
    pub fn sweep_raw_sensor_readings(
        conn: &mut Connection,
        retention_seconds: i64,
    ) -> StorageResult<i64> {
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
            - retention_seconds;

        let purged = conn
            .execute(
                "DELETE FROM sensor_readings
                 WHERE timestamp < ?1
                   AND EXISTS (
                       SELECT 1 FROM sensor_readings_minute m
                       WHERE m.sensor_line = sensor_readings.sensor_line
                         AND m.minute_ts <= sensor_readings.timestamp
                         AND m.minute_ts > sensor_readings.timestamp - ?2
                         AND sensor_readings.timestamp
                             < m.minute_ts + COALESCE(m.window_s, 60)
                   )",
                rusqlite::params![cutoff, MAX_BUCKET_SECONDS],
            )
            .map_err(|e| StorageError::DeleteError(format!("sweep_raw_sensor_readings: {}", e)))?
            as i64;

        if purged > 0 {
            let _ = AuditLogger::log_operation(
                conn,
                "DELETE",
                Some("sensor_readings"),
                Some(purged),
                None,
            );
        }

        Ok(purged)
    }

    /// Sweep the `sticker_readings` table — delete rows older than
    /// `retention_seconds` (by `ts`). Reports how many of the deleted rows
    /// had `id > min(last_exported_id across all destinations for the
    /// "sticker" stream)`; those are data losses for at least one
    /// destination and are logged at WARN level.
    pub fn sweep_node_readings(
        &self,
        conn: &mut Connection,
        retention_seconds: i64,
    ) -> StorageResult<NodeRetentionResult> {
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
            - retention_seconds;

        // Lowest cursor across all destinations on the "sticker" stream.
        // If no cursor row exists yet (no destinations have ever exported),
        // treat as 0 so every soon-to-be-deleted row counts as un-exported.
        let min_cursor: i64 = conn
            .query_row(
                "SELECT COALESCE(MIN(last_exported_id), 0) FROM export_cursor
                 WHERE stream = 'sticker'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        let unexported_dropped: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sticker_readings WHERE ts < ? AND id > ?",
                rusqlite::params![cutoff, min_cursor],
                |r| r.get(0),
            )
            .unwrap_or(0);

        let purged = conn
            .execute(
                "DELETE FROM sticker_readings WHERE ts < ?",
                rusqlite::params![cutoff],
            )
            .map_err(|e| StorageError::DeleteError(format!("sweep_node_readings: {}", e)))?
            as i64;

        if unexported_dropped > 0 {
            eprintln!(
                "WARN [retention] dropped {} un-exported Node rows (min_cursor={}, cutoff={})",
                unexported_dropped, min_cursor, cutoff,
            );
        }

        if purged > 0 {
            let _ = AuditLogger::log_operation(
                conn,
                "DELETE",
                Some("sticker_readings"),
                Some(purged),
                None,
            );
        }

        Ok(NodeRetentionResult {
            purged,
            unexported_dropped,
        })
    }

    /// Sweep the `eye_readings` table — delete rows older than
    /// `retention_seconds` (by `ts`). Reports how many of the deleted rows
    /// had `id > min(last_exported_id across all destinations for the "eye"
    /// stream)`; those are data losses for at least one destination and are
    /// logged at WARN level. Mirrors `sweep_node_readings`.
    pub fn sweep_beacon_readings(
        &self,
        conn: &mut Connection,
        retention_seconds: i64,
    ) -> StorageResult<BeaconRetentionResult> {
        let cutoff = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
            - retention_seconds;

        let min_cursor: i64 = conn
            .query_row(
                "SELECT COALESCE(MIN(last_exported_id), 0) FROM export_cursor
                 WHERE stream = 'eye'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        let unexported_dropped: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM eye_readings WHERE ts < ? AND id > ?",
                rusqlite::params![cutoff, min_cursor],
                |r| r.get(0),
            )
            .unwrap_or(0);

        let purged = conn
            .execute(
                "DELETE FROM eye_readings WHERE ts < ?",
                rusqlite::params![cutoff],
            )
            .map_err(|e| StorageError::DeleteError(format!("sweep_eye_readings: {}", e)))?
            as i64;

        if unexported_dropped > 0 {
            eprintln!(
                "WARN [retention] dropped {} un-exported eye_readings rows (min_cursor={}, cutoff={})",
                unexported_dropped, min_cursor, cutoff,
            );
        }

        if purged > 0 {
            let _ = AuditLogger::log_operation(
                conn,
                "DELETE",
                Some("eye_readings"),
                Some(purged),
                None,
            );
        }

        Ok(BeaconRetentionResult {
            purged,
            unexported_dropped,
        })
    }
}

/// Result of a `sweep_beacon_readings` pass.
#[derive(Debug, Clone, Default)]
pub struct BeaconRetentionResult {
    /// Total rows deleted from `eye_readings`.
    pub purged: i64,
    /// Of those deleted, how many had `id > min(cursor)` — i.e. were dropped
    /// before any destination had a chance to export them. Emits a WARN log.
    pub unexported_dropped: i64,
}

/// Statistics from a retention cleanup operation
#[derive(Debug, Clone, Default)]
pub struct RetentionStats {
    /// Number of records deleted
    pub deleted_count: i64,

    /// Live bytes freed, measured from the page counts before and after
    pub freed_bytes: i64,

    /// Oldest timestamp of deleted records
    pub oldest_deleted_timestamp: Option<i64>,

    /// Duration of cleanup operation in milliseconds
    pub duration_ms: i64,

    /// Of the deleted Node and Beacon rows, those past the lowest export
    /// cursor of their stream: no destination had them yet.
    pub unexported_dropped: i64,

    /// Of the deleted rows, the 1-Wire window buckets (`sensor_readings_minute`).
    pub summaries_deleted: i64,

    /// Whether the caller should follow up with a full `VACUUM`, and how
    /// urgently. See [`VacuumUrgency`].
    pub vacuum: VacuumUrgency,
}

const MIB: u64 = 1024 * 1024;

/// Free space below which a capacity pass starts, whatever the cap says:
/// the free blocks on the database's partition plus the freelist inside the
/// file (#124). Above the journal's own `SystemKeepFree`, so journald gives
/// way before the measurements do.
pub const DISK_FREE_FLOOR: u64 = 256 * MIB;

/// Free space a capacity pass restores once it runs, so the next write does
/// not start another.
pub const DISK_FREE_TARGET: u64 = 384 * MIB;

/// One-hour batches a single capacity pass deletes at most: a week of data.
pub const MAX_BATCHES_PER_PASS: usize = 168;

/// How full the database and its partition are, in the terms the capacity
/// limits use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capacity {
    /// Pages in use: `page_count - freelist_count`, times the page size.
    pub used_bytes: u64,
    /// Free pages inside the file; SQLite reuses them before it grows the file.
    pub freelist_bytes: u64,
    /// Free space on the partition, `None` when `statvfs` failed.
    pub available_bytes: Option<u64>,
}

impl Capacity {
    pub fn measure(db: &Database, conn: &Connection) -> StorageResult<Self> {
        let pragma = |name: &str| -> StorageResult<u64> {
            conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get::<_, i64>(0))
                .map(|v| v.max(0) as u64)
                .map_err(|e| StorageError::QueryError(format!("PRAGMA {name}: {e}")))
        };
        let page_size = pragma("page_size")?;
        let page_count = pragma("page_count")?;
        let freelist = pragma("freelist_count")?.min(page_count);
        let dir = db
            .path()
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "/".to_string());
        let usage = crate::libs::storage::disk::get_partition_usage(&dir);
        Ok(Self {
            used_bytes: (page_count - freelist) * page_size,
            freelist_bytes: freelist * page_size,
            available_bytes: (usage.total_bytes > 0).then_some(usage.available_bytes),
        })
    }
}

/// One batch of a capacity pass.
struct Batch {
    from: i64,
    rows: i64,
    unexported: i64,
    summaries: bool,
}

/// Delete the oldest hour of measurements older than `max_age_ts`, in one
/// transaction. `None` when nothing that old is left. See
/// [`RetentionPolicy::enforce`] for the order.
fn delete_oldest_hour(conn: &mut Connection, max_age_ts: i64) -> StorageResult<Option<Batch>> {
    let err = |what: &str, e: rusqlite::Error| StorageError::DeleteError(format!("{what}: {e}"));
    let tx = conn
        .transaction()
        .map_err(|e| err("start capacity batch", e))?;

    // `eye_readings` and `sticker_readings` have no index on `ts` alone, and
    // a range on it scans the whole table: on a 1.4 GB Hub that is minutes of
    // decrypting per batch with the storage thread stalled. Both are appended
    // in arrival order, so their `id` (the rowid) runs with time: the oldest
    // row is the head, and a batch is the rowid prefix that ends at the first
    // row of the next hour. A row replayed later with an old `ts` sits further
    // on and goes when it reaches the head (or with the age sweep).
    let mut heads = Vec::with_capacity(2);
    for table in ["eye_readings", "sticker_readings"] {
        let head: Option<i64> = tx
            .query_row(
                &format!("SELECT ts FROM {table} ORDER BY id LIMIT 1"),
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| err(table, e))?;
        heads.push(head.filter(|ts| *ts < max_age_ts));
    }
    let wire: Option<i64> = tx
        .query_row(
            "SELECT MIN(timestamp) FROM sensor_readings WHERE timestamp < ?1",
            [max_age_ts],
            |r| r.get(0),
        )
        .map_err(|e| err("sensor_readings", e))?;
    let oldest = heads.into_iter().chain([wire]).flatten().min();

    let batch = if let Some(from) = oldest {
        let to = (from + 3600).min(max_age_ts);
        let mut rows = tx
            .execute(
                "DELETE FROM sensor_readings WHERE timestamp >= ?1 AND timestamp < ?2",
                [from, to],
            )
            .map_err(|e| err("sensor_readings", e))? as i64;
        let mut unexported = 0;
        for (table, stream) in [("eye_readings", "eye"), ("sticker_readings", "sticker")] {
            // The scan walks the rowid order and stops at the first match.
            let end: i64 = tx
                .query_row(
                    &format!("SELECT id FROM {table} WHERE ts >= ?1 ORDER BY id LIMIT 1"),
                    [to],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| err(table, e))?
                .unwrap_or(i64::MAX);
            unexported += tx
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {table} WHERE id < ?1
                           AND id > (SELECT COALESCE(MIN(last_exported_id), 0)
                                     FROM export_cursor WHERE stream = ?2)"
                    ),
                    rusqlite::params![end, stream],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0);
            rows += tx
                .execute(&format!("DELETE FROM {table} WHERE id < ?1"), [end])
                .map_err(|e| err(table, e))? as i64;
        }
        Batch {
            from,
            rows,
            unexported,
            summaries: false,
        }
    } else {
        let oldest: Option<i64> = tx
            .query_row(
                "SELECT MIN(minute_ts) FROM sensor_readings_minute WHERE minute_ts < ?1",
                [max_age_ts],
                |r| r.get(0),
            )
            .map_err(|e| err("find the oldest window bucket", e))?;
        let Some(from) = oldest else {
            return Ok(None);
        };
        let to = (from + 3600).min(max_age_ts);
        let rows = tx
            .execute(
                "DELETE FROM sensor_readings_minute WHERE minute_ts >= ?1 AND minute_ts < ?2",
                [from, to],
            )
            .map_err(|e| err("sensor_readings_minute", e))? as i64;
        Batch {
            from,
            rows,
            unexported: 0,
            summaries: true,
        }
    };
    tx.commit().map_err(|e| err("commit capacity batch", e))?;
    Ok(Some(batch))
}

/// What the DELETE pass concluded about repacking the file.
///
/// Deleting rows returns pages to SQLite's freelist; it does not shrink the
/// file or defragment it. Only a full `VACUUM` does, and that rewrite locks out
/// every writer for its duration — so *when* it runs is a decision, not an
/// afterthought.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VacuumUrgency {
    /// Nothing was deleted; the freelist did not grow.
    #[default]
    NotNeeded,
    /// Pages are reclaimable, but the device is not short of space. The
    /// rewrite waits for a maintenance window rather than stalling ingest now.
    Deferred,
    /// Utilisation is past [`EMERGENCY_UTILISATION_PERCENT`]. Running out of
    /// cap space is not deferrable — reclaiming beats waiting for a window.
    Emergency,
}

/// What a completed rewrite reclaimed.
#[derive(Debug, Clone, Copy)]
pub struct VacuumOutcome {
    pub elapsed_ms: i64,
    pub bytes_before: i64,
    pub bytes_after: i64,
}

/// Rewrite `db_path` on a connection of its own.
///
/// Meant to be called from a helper thread while the storage thread keeps
/// draining its channel into the spill journal — which is the whole point of
/// giving it a separate connection. It must never write an audit row: the
/// `AuditLogger` hash-chain reads the latest hash and appends with no locking,
/// so two threads writing audit rows concurrently would fork the chain. The
/// storage thread records the outcome.
///
/// Note this deliberately does *not* go through `Database::connect`, because
/// `configure_pragmas` sets `temp_store = MEMORY` on every connection. Combined
/// with `bundled-sqlcipher`'s `-DSQLITE_TEMP_STORE=2`, that makes VACUUM build
/// its complete temporary copy of an up-to-2.5 GB database **in RAM** on a CM4.
/// This connection asks for `temp_store = FILE` instead; where the file lands is
/// `SQLITE_TMPDIR`, set by `main()`. Every other connection keeps in-memory temp
/// storage, which is what keeps sorts fast.
pub fn full_vacuum(db_path: &Path) -> StorageResult<VacuumOutcome> {
    let bytes_before = std::fs::metadata(db_path)
        .map(|m| m.len() as i64)
        .unwrap_or(0);
    let started = std::time::Instant::now();

    let conn = Connection::open(db_path).map_err(|e| {
        StorageError::DatabaseInitError(format!("Failed to open the vacuum connection: {}", e))
    })?;
    if let Some(key) = Database::encryption_key()? {
        conn.execute_batch(&format!("PRAGMA key = '{}';", key))
            .map_err(|e| {
                StorageError::DatabaseInitError(format!(
                    "Failed to key the vacuum connection: {}",
                    e
                ))
            })?;
    }
    conn.execute("PRAGMA temp_store = FILE", []).map_err(|e| {
        StorageError::DatabaseInitError(format!("Failed to route VACUUM temp files to disk: {}", e))
    })?;
    // Patient, unlike the 5 s rusqlite default. VACUUM needs the exclusive lock
    // at the end of its rewrite; giving up because an export drain happened to
    // be mid-read would throw away the entire repack (transactionally safe, but
    // every page rewritten for nothing).
    conn.busy_timeout(std::time::Duration::from_secs(30))
        .map_err(|e| {
            StorageError::DatabaseInitError(format!("Failed to set the VACUUM busy timeout: {}", e))
        })?;

    conn.execute_batch("VACUUM")
        .map_err(|e| StorageError::QueryError(format!("VACUUM failed: {}", e)))?;

    let bytes_after = std::fs::metadata(db_path)
        .map(|m| m.len() as i64)
        .unwrap_or(0);
    Ok(VacuumOutcome {
        elapsed_ms: started.elapsed().as_millis() as i64,
        bytes_before,
        bytes_after,
    })
}

/// Utilisation at which a rewrite stops waiting for a window.
///
/// Deliberately a constant with a doc comment rather than a YAML key: nobody
/// should tune "how full is too full" per deployment. The window is the part
/// that varies by deployment.
pub const EMERGENCY_UTILISATION_PERCENT: f32 = 95.0;

/// The widest bucket in `sensor_readings_minute`: the storage window's
/// upper bound (`aggregation_interval_s` <= 3600).
pub const MAX_BUCKET_SECONDS: i64 = 3600;

/// Headroom left free after a rewrite, so maintenance can never be the reason
/// `/data` fills up.
const FREE_SPACE_RESERVE: u64 = 512 * 1024 * 1024;

/// Free space a full `VACUUM` needs before it may start.
///
/// Twice the database, because VACUUM writes a complete temporary copy *and*,
/// in WAL mode, pushes every page through the WAL as well — plus
/// [`FREE_SPACE_RESERVE`] so the rewrite cannot be what fills the partition.
///
/// Mirrors `vacuum_space_required` in `portal/src/retention.rs`, deliberately
/// rather than shared: that helper lives in the `portal` binary package, which
/// this crate does not (and should not) depend on — the same reason
/// [`crate::libs::storage::disk::get_partition_usage`] mirrors
/// `portal/src/diskspace.rs`. Decision #9's monolith is where the two converge.
pub fn vacuum_space_required(db_size_bytes: u64) -> u64 {
    db_size_bytes
        .saturating_mul(2)
        .saturating_add(FREE_SPACE_RESERVE)
}

/// Whether there is room to rewrite a `db_size_bytes` database.
///
/// `available == None` ("could not measure") permits the run: refusing
/// maintenance because `statvfs` failed would mean a Hub that never repacks.
pub fn space_permits_vacuum(db_size_bytes: u64, available: Option<u64>) -> bool {
    match available {
        Some(available) => available >= vacuum_space_required(db_size_bytes),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    /// A rewrite changes the file's physical layout, never a row's identity —
    /// so the export cursor still points where it pointed, and the drain
    /// resumes at exactly the row it would have.
    ///
    /// Asserted rather than assumed: this is the property that decides whether
    /// a maintenance run can silently re-send or skip a batch of patient data
    /// on every MQTT destination.
    #[test]
    fn a_vacuum_does_not_move_the_export_cursor_or_the_rows_it_indexes() {
        use crate::libs::storage::reader::StorageReader;
        use crate::libs::storage::writer::StorageWriter;

        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::new(dir.path().join("fiber_medical.db"), 5).expect("open db");
        let mut conn = db.connect().expect("connect");

        for i in 0..40i64 {
            StorageWriter::write_node_reading(
                &mut conn,
                "aa11",
                1,
                1_700_000_000 + i,
                1_700_000_000 + i,
                &format!("msg-{i}"),
                "reading",
                r#"{"t":1.0}"#,
            )
            .expect("write");
        }

        // Drain half, exactly as the export orchestrator does.
        let first = StorageReader::fetch_node_readings_after(&conn, 0, 20).expect("fetch");
        assert_eq!(first.len(), 20);
        let cursor = first.last().expect("rows").id;
        StorageWriter::advance_export_cursor(&mut conn, "broker-1", "sticker", cursor)
            .expect("advance");

        // Free some pages so the rewrite has something to reclaim, then repack.
        conn.execute("DELETE FROM sticker_readings WHERE id <= ?1", [cursor])
            .expect("delete exported rows");
        full_vacuum(db.path()).expect("vacuum");

        let after =
            StorageReader::load_export_cursor(&conn, "broker-1", "sticker").expect("load cursor");
        assert_eq!(
            after, cursor,
            "the cursor is a data row like any other — VACUUM must not move it"
        );

        let rest = StorageReader::fetch_node_readings_after(&conn, after, 100).expect("fetch");
        assert_eq!(
            rest.len(),
            20,
            "the drain resumes with exactly what is left"
        );
        assert_eq!(
            rest.iter()
                .map(|r| r.message_id.clone())
                .collect::<Vec<_>>(),
            (20..40).map(|i| format!("msg-{i}")).collect::<Vec<_>>(),
            "and with the same rows, in the same order — no re-send, no gap"
        );
    }

    // ---- VACUUM preflight and urgency ------------------------------------

    #[test]
    fn a_vacuum_needs_twice_the_database_plus_the_reserve() {
        // VACUUM writes a complete temporary copy *and*, in WAL mode, pushes
        // every page through the WAL as well.
        assert_eq!(
            vacuum_space_required(1024),
            2048 + FREE_SPACE_RESERVE,
            "2x the file, plus headroom so maintenance is never what fills /data"
        );
    }

    #[test]
    fn an_absurd_database_size_does_not_wrap() {
        assert_eq!(vacuum_space_required(u64::MAX), u64::MAX);
    }

    #[test]
    fn the_preflight_refuses_a_rewrite_that_would_not_fit() {
        let db = 1_000_000_000u64;
        let required = vacuum_space_required(db);

        assert!(space_permits_vacuum(db, Some(required)));
        assert!(!space_permits_vacuum(db, Some(required - 1)));
    }

    #[test]
    fn unmeasurable_free_space_permits_the_run() {
        // Refusing maintenance because `statvfs` failed would mean a Hub that
        // never repacks — a worse outcome than attempting the rewrite.
        assert!(space_permits_vacuum(1_000_000_000, None));
    }

    #[test]
    fn the_emergency_threshold_is_above_the_cleanup_threshold() {
        // Otherwise every cleanup would be an emergency and nothing would ever
        // be deferrable.
        let policy = RetentionPolicy::default();
        assert!(
            EMERGENCY_UTILISATION_PERCENT > policy.cleanup_threshold_percent,
            "an emergency must be rarer than a routine cleanup"
        );
    }

    use super::*;
    use crate::libs::storage::db::Database;

    #[test]
    fn test_retention_policy_creation() {
        let policy = RetentionPolicy::new(5);
        assert_eq!(policy.max_size_bytes, 5 * 1024 * 1024 * 1024);
        assert_eq!(policy.cleanup_threshold_percent, 90.0);
    }

    #[test]
    fn test_eligibility_for_deletion() {
        let policy = RetentionPolicy::new(5);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // Very old record should be eligible
        assert!(policy.is_eligible_for_deletion(now - 10000));

        // Very new record should not be eligible
        assert!(!policy.is_eligible_for_deletion(now));
    }

    #[test]
    fn test_needs_cleanup_initially_false() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 5).expect("Failed to create test DB");
        let conn = db.connect().unwrap();
        let policy = RetentionPolicy::new(5);

        let needs = policy
            .needs_cleanup(&db, &conn)
            .expect("Failed to check cleanup need");
        assert!(!needs, "Empty database should not need cleanup");
    }

    const GIB: u64 = 1024 * MIB;

    fn capacity(used: u64, freelist: u64, available: Option<u64>) -> Capacity {
        Capacity {
            used_bytes: used,
            freelist_bytes: freelist,
            available_bytes: available,
        }
    }

    #[test]
    fn the_cap_counts_live_pages_and_runs_on_to_85_percent() {
        let policy = RetentionPolicy::with_max_bytes(GIB as i64);
        let plenty = Some(10 * GIB);
        // 89 % live is fine, however large the file (the freelist is not use).
        assert_eq!(
            policy.deficit(&capacity(GIB / 100 * 89, GIB, plenty), false),
            0
        );
        // Past 90 % a pass starts and frees down to 85 %.
        let c = capacity(GIB / 100 * 91, 0, plenty);
        assert_eq!(policy.deficit(&c, false), GIB / 100 * 91 - GIB / 100 * 85);
        // Inside a pass, 87 % is still short of the 85 % mark.
        assert!(policy.deficit(&capacity(GIB / 100 * 87, 0, plenty), true) > 0);
        assert_eq!(
            policy.deficit(&capacity(GIB / 100 * 87, 0, plenty), false),
            0
        );
    }

    #[test]
    fn a_full_partition_starts_a_pass_well_under_the_cap() {
        // #124: 2.5 GB cap, /data 2.9 GB shared with the portal and the
        // journal, 1.4 GB live and 200 MB free.
        let policy = RetentionPolicy::with_max_bytes(2500 * MIB as i64);
        let c = capacity(1400 * MIB, 0, Some(200 * MIB));
        assert_eq!(policy.deficit(&c, false), DISK_FREE_TARGET - 200 * MIB);
        // The freelist is room too: SQLite fills it before it grows the file.
        let c = capacity(1400 * MIB, 100 * MIB, Some(200 * MIB));
        assert_eq!(policy.deficit(&c, false), 0);
        // Inside a pass it runs on to the target.
        assert_eq!(policy.deficit(&c, true), DISK_FREE_TARGET - 300 * MIB);
        // An unmeasurable partition leaves only the cap.
        assert_eq!(policy.deficit(&capacity(1400 * MIB, 0, None), false), 0);
    }

    fn insert_bucket(conn: &Connection, minute_ts: i64) {
        conn.execute(
            "INSERT INTO sensor_readings_minute
                 (minute_ts, sensor_line, min_c, avg_c, max_c, sample_count, worst_alarm, created_at)
             VALUES (?1, 1, 4.0, 4.0, 4.0, 1, 'NORMAL', ?1)",
            [minute_ts],
        )
        .unwrap();
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn a_capacity_batch_takes_the_oldest_hour_of_any_table_and_buckets_last() {
        use crate::libs::alarms::AlarmState;
        use crate::libs::storage::models::SensorReading;
        use crate::libs::storage::writer::StorageWriter;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 1).unwrap();
        let mut conn = db.connect().unwrap();
        let now = 1_760_000_000i64;
        let h = 3600i64;

        insert_bucket(&conn, now - 20 * h);
        StorageWriter::write_beacon_reading(
            &mut conn,
            "AA:BB",
            now - 10 * h,
            now - 10 * h,
            "eye-1",
            "advertising",
            "{}",
        )
        .unwrap();
        let r = SensorReading::new(now - 5 * h, 1, 4.0, true, AlarmState::Normal);
        StorageWriter::write_sensor_reading(&conn, &r, None).unwrap();
        StorageWriter::write_node_reading(
            &mut conn,
            "aa11",
            1,
            now - 3 * h,
            now - 3 * h,
            "st-1",
            "telemetry",
            "{}",
        )
        .unwrap();

        // Beacon first: it is the oldest measurement, not the wired probe.
        let b = delete_oldest_hour(&mut conn, now).unwrap().unwrap();
        assert_eq!((b.from, b.rows, b.summaries), (now - 10 * h, 1, false));
        assert_eq!(
            b.unexported, 1,
            "no destination has exported the Beacon row"
        );
        assert_eq!(count(&conn, "eye_readings"), 0);
        assert_eq!(count(&conn, "sensor_readings"), 1);

        let b = delete_oldest_hour(&mut conn, now).unwrap().unwrap();
        assert_eq!(b.from, now - 5 * h);
        let b = delete_oldest_hour(&mut conn, now).unwrap().unwrap();
        assert_eq!(b.from, now - 3 * h);
        // The older window bucket survives every measurement table.
        assert_eq!(count(&conn, "sensor_readings_minute"), 1);

        let b = delete_oldest_hour(&mut conn, now).unwrap().unwrap();
        assert_eq!((b.from, b.summaries), (now - 20 * h, true));
        assert!(delete_oldest_hour(&mut conn, now).unwrap().is_none());
    }

    #[test]
    fn a_capacity_batch_never_touches_rows_younger_than_the_cutoff() {
        use crate::libs::storage::writer::StorageWriter;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 1).unwrap();
        let mut conn = db.connect().unwrap();
        let now = 1_760_000_000i64;
        for (ts, id) in [(now - 90, "eye-0"), (now - 30, "eye-1")] {
            StorageWriter::write_beacon_reading(
                &mut conn,
                "AA:BB",
                ts,
                ts,
                id,
                "advertising",
                "{}",
            )
            .unwrap();
        }
        // The batch ends at the cutoff even when the hour would reach past it.
        let b = delete_oldest_hour(&mut conn, now - 60).unwrap().unwrap();
        assert_eq!(b.rows, 1);
        assert_eq!(count(&conn, "eye_readings"), 1);
        assert!(delete_oldest_hour(&mut conn, now - 60).unwrap().is_none());
    }

    #[test]
    fn a_capacity_batch_is_a_rowid_prefix_that_stops_at_the_next_hour() {
        // The batch is a rowid prefix, so it never scans the table by `ts`
        // (#124: minutes per batch on a 1.4 GB Hub). A row replayed later
        // with an old `ts` goes with the prefix it sits in; a row of the next
        // hour ends the prefix.
        use crate::libs::storage::writer::StorageWriter;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 1).unwrap();
        let mut conn = db.connect().unwrap();
        let now = 1_760_000_000i64;
        for (ts, id) in [
            (now - 10_000, "eye-0"),
            (now - 9_000, "eye-1"),
            (now - 5_000, "eye-2"),
            (now - 20_000, "eye-replayed"),
            (now - 1_000, "eye-3"),
        ] {
            StorageWriter::write_beacon_reading(
                &mut conn,
                "AA:BB",
                ts,
                ts,
                id,
                "advertising",
                "{}",
            )
            .unwrap();
        }
        let b = delete_oldest_hour(&mut conn, now).unwrap().unwrap();
        assert_eq!((b.from, b.rows), (now - 10_000, 2));
        let b = delete_oldest_hour(&mut conn, now).unwrap().unwrap();
        assert_eq!(
            (b.from, b.rows),
            (now - 5_000, 2),
            "eye-2 and the replayed row"
        );
        let b = delete_oldest_hour(&mut conn, now).unwrap().unwrap();
        assert_eq!((b.from, b.rows), (now - 1_000, 1));
        assert!(delete_oldest_hour(&mut conn, now).unwrap().is_none());
    }

    #[test]
    fn enforce_over_the_cap_deletes_beacon_rows_too() {
        // #124: the cap used to delete `sensor_readings` only, so a database
        // of Beacon rows stayed over it for good.
        use crate::libs::storage::writer::StorageWriter;
        use std::time::{SystemTime, UNIX_EPOCH};

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 1).unwrap();
        let mut conn = db.connect().unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let payload = "x".repeat(400);
        // 2000 rows over 20 hours, so a batch takes about a twentieth.
        for i in 0..2000i64 {
            let ts = now - 20 * 3600 + i * 36;
            StorageWriter::write_beacon_reading(
                &mut conn,
                "AA:BB",
                ts,
                ts,
                &format!("eye-{i}"),
                "advertising",
                &payload,
            )
            .unwrap();
        }
        let before = Capacity::measure(&db, &conn).unwrap();
        let mut policy = RetentionPolicy::new(1);
        policy.max_size_bytes = (before.used_bytes / 2) as i64;

        let stats = policy.enforce(&db, &mut conn).unwrap();
        assert!(stats.deleted_count > 0);
        let after = Capacity::measure(&db, &conn).unwrap();
        assert!(
            after.used_bytes <= policy.max_size_bytes as u64 / 100 * 85,
            "live pages {} still over 85 % of the {} cap",
            after.used_bytes,
            policy.max_size_bytes
        );
        // Measured before the pass wrote its audit row, so at least what is
        // gone now.
        assert!(stats.freed_bytes as u64 >= before.used_bytes - after.used_bytes);
        assert!(count(&conn, "eye_readings") > 0, "only the oldest hours go");
    }

    #[test]
    fn node_retention_purges_old_rows_and_warns_when_unexported() {
        use crate::libs::storage::writer::StorageWriter;
        use std::time::{SystemTime, UNIX_EPOCH};

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 1).unwrap();
        let mut conn = db.connect().unwrap();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let day = 86400i64;

        // Old row (35 days ago)
        StorageWriter::write_node_reading(
            &mut conn,
            "abc",
            1,
            now - 35 * day,
            now - 35 * day,
            "abc-old",
            "uplink",
            "{}",
        )
        .unwrap();
        // Fresh row (1 day ago)
        StorageWriter::write_node_reading(
            &mut conn,
            "abc",
            1,
            now - day,
            now - day,
            "abc-fresh",
            "uplink",
            "{}",
        )
        .unwrap();

        // Cursor is at 0 (no destinations have exported anything) → both
        // un-exported. Sweep should drop the old row and warn.
        let policy = RetentionPolicy::default();
        let dropped = policy.sweep_node_readings(&mut conn, 30 * day).unwrap();
        assert_eq!(dropped.purged, 1);
        assert_eq!(dropped.unexported_dropped, 1);

        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM sticker_readings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[test]
    fn beacon_retention_purges_old_rows_and_warns_when_unexported() {
        use crate::libs::storage::writer::StorageWriter;
        use std::time::{SystemTime, UNIX_EPOCH};

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 1).unwrap();
        let mut conn = db.connect().unwrap();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let day = 86400i64;

        // Old row (35 days ago)
        StorageWriter::write_beacon_reading(
            &mut conn,
            "AA:BB:CC:DD:EE:FF",
            now - 35 * day,
            now - 35 * day,
            "mac-old",
            "advertising",
            "{}",
        )
        .unwrap();
        // Fresh row (1 day ago)
        StorageWriter::write_beacon_reading(
            &mut conn,
            "AA:BB:CC:DD:EE:FF",
            now - day,
            now - day,
            "mac-fresh",
            "advertising",
            "{}",
        )
        .unwrap();

        // Cursor is at 0 (no destinations have exported anything) → both
        // un-exported. Sweep should drop the old row and warn.
        let policy = RetentionPolicy::default();
        let dropped = policy.sweep_beacon_readings(&mut conn, 30 * day).unwrap();
        assert_eq!(dropped.purged, 1);
        assert_eq!(dropped.unexported_dropped, 1);

        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM eye_readings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[test]
    fn enforce_walks_forward_and_completes_without_rollback() {
        // Regression: the previous implementation (a) ran VACUUM inside an
        // open transaction (SQLite rejects this) and (b) advanced the cutoff
        // by reassigning it to the oldest timestamp found, so the next loop
        // iteration's WHERE clause matched nothing. The net effect was a
        // forced rollback that deleted zero rows even when far over capacity.
        use crate::libs::alarms::AlarmState;
        use crate::libs::storage::models::SensorReading;
        use crate::libs::storage::writer::StorageWriter;
        use std::time::{SystemTime, UNIX_EPOCH};

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 1).unwrap();
        let mut conn = db.connect().unwrap();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        // Three rows spread over a 4-hour window, all well past min_age.
        for offset_h in [4, 3, 2] {
            let ts = now - offset_h * 3600 - 120;
            let r = SensorReading::new(ts, 0, 20.0, true, AlarmState::Normal);
            StorageWriter::write_sensor_reading(&conn, &r, None).unwrap();
        }

        let mut policy = RetentionPolicy::new(1);
        // Force enforce() into the deletion path by pinning the cap below
        // current size, regardless of the actual file size.
        policy.max_size_bytes = 1;
        let stats = policy.enforce(&db, &mut conn).unwrap();

        // We should have deleted more than a single 1-hour batch — the old
        // bug capped this at one row's worth.
        assert!(
            stats.deleted_count >= 2,
            "enforce should delete multiple batches, deleted={}",
            stats.deleted_count,
        );

        // And the transaction must commit (the rows really go away — the
        // earlier VACUUM-in-transaction bug rolled back the deletes).
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM sensor_readings", [], |r| r.get(0))
            .unwrap();
        assert!(
            remaining < 3,
            "expected rows to be persisted-deleted, found {}",
            remaining
        );
    }

    /// A raw row is now an alarm transition inside a storage window of up to
    /// an hour; the sweep counts it covered by the window's bucket, not only
    /// by a bucket of its own minute.
    #[test]
    fn raw_sweep_counts_a_storage_window_bucket_as_coverage() {
        use crate::libs::alarms::AlarmState;
        use crate::libs::storage::models::{MinuteAggregateRow, SensorReading};
        use crate::libs::storage::writer::StorageWriter;
        use std::time::{SystemTime, UNIX_EPOCH};

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 1).unwrap();
        let mut conn = db.connect().unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let window = ((now - 35 * 86400) / 600) * 600;

        // Line 0: a transition 7 minutes into a covered window. Line 1: none.
        for line in [0u8, 1] {
            let r = SensorReading::new(window + 420, line, 9.0, true, AlarmState::Warning);
            StorageWriter::write_sensor_reading(&conn, &r, None).unwrap();
        }
        let bucket = MinuteAggregateRow {
            minute_ts: window,
            sensor_line: 0,
            min_c: 4.0,
            avg_c: 6.0,
            max_c: 9.0,
            sample_count: 300,
            disconnect_count: 0,
            worst_alarm: "WARNING".to_string(),
            created_at: now,
            data_hmac: None,
            window_s: Some(600),
        };
        StorageWriter::write_sensor_bucket(&conn, &bucket, None).unwrap();

        let purged = RetentionPolicy::sweep_raw_sensor_readings(&mut conn, 30 * 86400).unwrap();
        assert_eq!(purged, 1);
        let left: i64 = conn
            .query_row("SELECT sensor_line FROM sensor_readings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 1);
    }

    /// Coverage is containment: a bucket covers `[minute_ts, minute_ts +
    /// window_s)` of its line, and a legacy one-minute bucket (NULL
    /// `window_s`) only its minute. A raw row past a bucket's end, or after a
    /// legacy minute bucket whose next minute was never rolled up, survives.
    #[test]
    fn raw_sweep_deletes_only_rows_a_bucket_contains() {
        use crate::libs::alarms::AlarmState;
        use crate::libs::storage::models::{MinuteAggregateRow, SensorReading};
        use crate::libs::storage::writer::StorageWriter;
        use std::time::{SystemTime, UNIX_EPOCH};

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 1).unwrap();
        let mut conn = db.connect().unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let base = ((now - 35 * 86400) / 3600) * 3600;
        let bucket = |minute_ts: i64, line: u8, window_s: Option<i64>| MinuteAggregateRow {
            minute_ts,
            sensor_line: line,
            min_c: 4.0,
            avg_c: 5.0,
            max_c: 6.0,
            sample_count: 10,
            disconnect_count: 0,
            worst_alarm: "NORMAL".to_string(),
            created_at: now,
            data_hmac: None,
            window_s,
        };
        // Line 0: a 600 s window bucket. Line 1: a legacy minute bucket.
        // Line 2: a partial window closed early by an interval change.
        StorageWriter::write_sensor_bucket(&conn, &bucket(base, 0, Some(600)), None).unwrap();
        StorageWriter::write_sensor_bucket(&conn, &bucket(base, 1, None), None).unwrap();
        StorageWriter::write_sensor_bucket(&conn, &bucket(base, 2, Some(120)), None).unwrap();

        // (line, offset from base, expected to survive)
        let rows = [
            (0u8, 0, false),
            (0, 599, false),
            (0, 600, true),
            (0, 1_800, true),
            (1, 30, false),
            (1, 60, true),
            (1, 420, true),
            (2, 119, false),
            (2, 120, true),
        ];
        for (line, offset, _) in rows {
            let r = SensorReading::new(base + offset, line, 9.0, true, AlarmState::Warning);
            StorageWriter::write_sensor_reading(&conn, &r, None).unwrap();
        }

        let purged = RetentionPolicy::sweep_raw_sensor_readings(&mut conn, 30 * 86400).unwrap();
        assert_eq!(purged, 4);
        for (line, offset, survives) in rows {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sensor_readings
                     WHERE sensor_line = ?1 AND timestamp = ?2",
                    rusqlite::params![line, base + offset],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n == 1, survives, "line {} at +{}", line, offset);
        }
    }

    #[test]
    fn raw_sweep_only_deletes_aggregated_rows() {
        use crate::libs::alarms::AlarmState;
        use crate::libs::storage::aggregator::aggregate_closed_minutes;
        use crate::libs::storage::models::SensorReading;
        use crate::libs::storage::writer::StorageWriter;
        use std::time::{SystemTime, UNIX_EPOCH};

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::new(tmp.path(), 1).unwrap();
        let mut conn = db.connect().unwrap();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let day = 86400i64;

        // Two readings 35 days ago: should be aggregatable + sweepable.
        for offset in [0, 30] {
            let r = SensorReading::new(now - 35 * day + offset, 0, 20.0, true, AlarmState::Normal);
            StorageWriter::write_sensor_reading(&conn, &r, None).unwrap();
        }
        // One reading 35 days ago for a different sensor that we WON'T aggregate.
        // It must survive the sweep because no aggregate covers it.
        let r = SensorReading::new(now - 35 * day, 1, 21.0, true, AlarmState::Normal);
        StorageWriter::write_sensor_reading(&conn, &r, None).unwrap();

        // Aggregate only the sensor_line=0 readings by deleting any sensor_line=1
        // rows from the aggregate after running the aggregator.
        aggregate_closed_minutes(&mut conn, now, None).unwrap();
        conn.execute(
            "DELETE FROM sensor_readings_minute WHERE sensor_line = 1",
            [],
        )
        .unwrap();

        let purged = RetentionPolicy::sweep_raw_sensor_readings(&mut conn, 30 * day).unwrap();
        assert_eq!(
            purged, 2,
            "only the aggregated sensor_line=0 rows should drop"
        );

        let surviving_line_1: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sensor_readings WHERE sensor_line = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            surviving_line_1, 1,
            "unaggregated raw row must not be deleted even if past retention"
        );
    }
}
