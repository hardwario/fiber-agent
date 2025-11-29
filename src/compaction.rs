// src/compaction.rs
// Storage compaction for 5.4GB /data partition management
// Weekly aggregation + monthly archival to maintain compliance retention

use chrono::{DateTime, Duration, Datelike, Utc};
use rusqlite::Connection;
use std::fs;
use std::path::Path;

/// Scheduling for compaction jobs
pub struct CompactionScheduler {
    last_weekly_aggregation: Option<DateTime<Utc>>,
    last_monthly_archival: Option<DateTime<Utc>>,
    last_disk_check: Option<DateTime<Utc>>,
}

impl CompactionScheduler {
    pub fn new() -> Self {
        Self {
            last_weekly_aggregation: None,
            last_monthly_archival: None,
            last_disk_check: None,
        }
    }

    /// Check if we should run weekly aggregation (every Monday midnight UTC)
    pub fn should_aggregate_weekly(&mut self, now: DateTime<Utc>) -> bool {
        // Run on Monday midnight
        if now.weekday() != chrono::Weekday::Mon {
            return false;
        }

        // Only once per week
        if let Some(last) = self.last_weekly_aggregation {
            if (now - last).num_days() < 7 {
                return false;
            }
        }

        true
    }

    /// Check if we should run monthly archival (every 1st of month midnight UTC)
    pub fn should_archive_monthly(&mut self, now: DateTime<Utc>) -> bool {
        // Run on 1st of month
        if now.day() != 1 {
            return false;
        }

        // Only once per month
        if let Some(last) = self.last_monthly_archival {
            if (now - last).num_days() < 28 {
                return false;
            }
        }

        true
    }

    /// Check if we should run disk usage check (every hour)
    pub fn should_check_disk(&mut self, now: DateTime<Utc>) -> bool {
        if let Some(last) = self.last_disk_check {
            if (now - last).num_minutes() < 60 {
                return false;
            }
        }
        true
    }

    /// Mark weekly aggregation as completed
    pub fn mark_weekly_done(&mut self, now: DateTime<Utc>) {
        self.last_weekly_aggregation = Some(now);
    }

    /// Mark monthly archival as completed
    pub fn mark_monthly_done(&mut self, now: DateTime<Utc>) {
        self.last_monthly_archival = Some(now);
    }

    /// Mark disk check as completed
    pub fn mark_disk_check_done(&mut self, now: DateTime<Utc>) {
        self.last_disk_check = Some(now);
    }
}

/// Storage usage statistics
#[derive(Debug, Clone)]
pub struct DiskStatus {
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub total_bytes: u64,
    pub used_percent: f32,
    pub is_critical: bool, // > 95%
    pub is_warning: bool,  // > 80%
}

impl DiskStatus {
    pub fn new(used: u64, total: u64) -> Self {
        let free = total.saturating_sub(used);
        let used_percent = if total > 0 {
            (used as f32 / total as f32) * 100.0
        } else {
            0.0
        };

        Self {
            used_bytes: used,
            free_bytes: free,
            total_bytes: total,
            used_percent,
            is_critical: used_percent > 95.0,
            is_warning: used_percent > 80.0,
        }
    }

    pub fn status_str(&self) -> &'static str {
        if self.is_critical {
            "CRITICAL"
        } else if self.is_warning {
            "WARNING"
        } else {
            "OK"
        }
    }
}

/// Storage breakdown by database
#[derive(Debug, Clone)]
pub struct StorageBreakdown {
    pub readings_db: u64,
    pub archives_total: u64,
    pub audit_db: u64,
    pub blockchain_db: u64,
}

impl StorageBreakdown {
    pub fn total(&self) -> u64 {
        self.readings_db + self.archives_total + self.audit_db + self.blockchain_db
    }
}

/// Monitor disk usage in /data partition
pub struct StorageMonitor {
    data_dir: String,
    warning_threshold: f32,   // 80% full = warn
    critical_threshold: f32,  // 95% full = critical
}

impl StorageMonitor {
    pub fn new(data_dir: String) -> Self {
        Self {
            data_dir,
            warning_threshold: 80.0,
            critical_threshold: 95.0,
        }
    }

    /// Check disk usage for /data partition
    pub fn check_disk_usage(&self) -> Result<DiskStatus, Box<dyn std::error::Error>> {
        let path = Path::new(&self.data_dir);

        // Get filesystem stats
        #[cfg(target_os = "linux")]
        {
            // Note: This is a simplified approach - in production, use statfs/statvfs
            // For now, just report file system metadata
            let total = 5_400_000_000u64; // 5.4GB hardcoded for CM4
            let used = get_dir_size(path)?;
            Ok(DiskStatus::new(used, total))
        }

        #[cfg(not(target_os = "linux"))]
        {
            let total = 5_400_000_000u64; // 5.4GB for testing
            let used = get_dir_size(path)?;
            Ok(DiskStatus::new(used, total))
        }
    }

    /// Get breakdown of storage by database
    pub fn get_database_sizes(&self) -> Result<StorageBreakdown, Box<dyn std::error::Error>> {
        let readings_db = get_file_size(&self.data_dir, "readings.db")?;
        let audit_db = get_file_size(&self.data_dir, "audit.db")?;
        let blockchain_db = get_file_size(&self.data_dir, "blockchain.db")?;

        // Sum all archive databases
        let archives_total = sum_archive_sizes(&self.data_dir)?;

        Ok(StorageBreakdown {
            readings_db,
            archives_total,
            audit_db,
            blockchain_db,
        })
    }
}

/// Get size of a single file
fn get_file_size(data_dir: &str, filename: &str) -> Result<u64, Box<dyn std::error::Error>> {
    let path = format!("{}/{}", data_dir, filename);
    match fs::metadata(&path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(_) => Ok(0), // File doesn't exist yet
    }
}

/// Get total size of directory recursively
fn get_dir_size(path: &Path) -> Result<u64, Box<dyn std::error::Error>> {
    let mut total = 0u64;

    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;

            if metadata.is_dir() {
                total += get_dir_size(&entry.path())?;
            } else {
                total += metadata.len();
            }
        }
    }

    Ok(total)
}

/// Sum sizes of all archive databases (readings_archive_YYYY_MM.db)
fn sum_archive_sizes(data_dir: &str) -> Result<u64, Box<dyn std::error::Error>> {
    let mut total = 0u64;

    for entry in fs::read_dir(data_dir)? {
        let entry = entry?;
        let filename = entry.file_name();
        let filename_str = filename.to_string_lossy();

        if filename_str.starts_with("readings_archive_") && filename_str.ends_with(".db") {
            let metadata = entry.metadata()?;
            total += metadata.len();
        }
    }

    Ok(total)
}

/// Aggregate raw readings to 1-minute averages
pub fn aggregate_readings_to_minutes(
    db_path: &str,
    _from_ts: DateTime<Utc>,
    to_ts: DateTime<Utc>,
) -> Result<u64, Box<dyn std::error::Error>> {
    let conn = Connection::open(db_path)?;

    // Create aggregated_readings table if not exists
    conn.execute(
        "CREATE TABLE IF NOT EXISTS aggregated_readings (
            sensor_id INTEGER NOT NULL,
            minute_bucket DATETIME NOT NULL,
            reading_count INTEGER NOT NULL,
            value_mean REAL,
            value_min REAL,
            value_max REAL,
            PRIMARY KEY (sensor_id, minute_bucket)
        )",
        [],
    )?;

    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_agg_sensor_ts ON aggregated_readings(sensor_id, minute_bucket)",
        [],
    )?;

    // Count readings before aggregation
    let mut stmt = conn.prepare("SELECT COUNT(*) FROM readings WHERE ts_utc < ?1")?;
    let count_before: u64 = stmt.query_row([to_ts.to_rfc3339()], |row| row.get(0))?;

    // Aggregate readings older than 7 days, keep last 7 days raw
    let cutoff_ts = Utc::now() - Duration::days(7);

    // Insert aggregated readings for old data
    conn.execute(
        "INSERT OR REPLACE INTO aggregated_readings
        SELECT
            sensor_id,
            datetime(strftime('%Y-%m-%d %H:%M:00', ts_utc)) as minute_bucket,
            COUNT(*) as reading_count,
            AVG(value) as value_mean,
            MIN(value) as value_min,
            MAX(value) as value_max
        FROM readings
        WHERE ts_utc < ?1
        GROUP BY sensor_id, minute_bucket",
        [cutoff_ts.to_rfc3339()],
    )?;

    // Delete raw readings older than 7 days (now in aggregated_readings)
    let deleted = conn.execute(
        "DELETE FROM readings WHERE ts_utc < ?1",
        [cutoff_ts.to_rfc3339()],
    )?;

    // VACUUM to reclaim space
    conn.execute("VACUUM", [])?;

    println!(
        "[compaction] Weekly aggregation: {} entries aggregated, {} raw entries deleted, freed space",
        count_before, deleted
    );

    Ok(deleted as u64)
}

/// Archive month's readings to separate database
pub fn archive_month_readings(
    source_db: &str,
    data_dir: &str,
    year: i32,
    month: u32,
) -> Result<u64, Box<dyn std::error::Error>> {
    // Create archive database filename
    let archive_filename = format!("readings_archive_{:04}_{:02}.db", year, month);
    let archive_path = format!("{}/{}", data_dir, archive_filename);

    // Open both databases
    let source_conn = Connection::open(source_db)?;
    let archive_conn = Connection::open(&archive_path)?;

    // Create hourly aggregates table in archive
    archive_conn.execute(
        "CREATE TABLE IF NOT EXISTS hourly_readings (
            sensor_id INTEGER NOT NULL,
            hour_bucket DATETIME NOT NULL,
            reading_count INTEGER NOT NULL,
            value_mean REAL,
            value_min REAL,
            value_max REAL,
            value_stddev REAL,
            PRIMARY KEY (sensor_id, hour_bucket)
        )",
        [],
    )?;

    archive_conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_hour_sensor ON hourly_readings(sensor_id, hour_bucket)",
        [],
    )?;

    // Date range for this month
    let month_start = format!("{:04}-{:02}-01T00:00:00Z", year, month);
    let next_month = if month == 12 {
        format!("{:04}-01-01T00:00:00Z", year + 1)
    } else {
        format!("{:04}-{:02}-01T00:00:00Z", year, month + 1)
    };

    // Query aggregated readings for this month and archive them as hourly
    let mut stmt = source_conn.prepare(
        "SELECT sensor_id, minute_bucket, reading_count, value_mean, value_min, value_max
         FROM aggregated_readings
         WHERE minute_bucket >= ?1 AND minute_bucket < ?2
         ORDER BY sensor_id, minute_bucket"
    )?;

    let readings: Vec<_> = stmt
        .query_map([&month_start, &next_month], |row| {
            Ok((
                row.get::<_, i32>(0)?,   // sensor_id
                row.get::<_, String>(1)?, // minute_bucket
                row.get::<_, i32>(2)?,   // reading_count
                row.get::<_, f32>(3)?,   // value_mean
                row.get::<_, f32>(4)?,   // value_min
                row.get::<_, f32>(5)?,   // value_max
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // Group into hourly buckets and compute statistics
    let mut hourly: std::collections::HashMap<(i32, String), (Vec<f32>, i32)> =
        std::collections::HashMap::new();

    for (sensor_id, minute_bucket, _count, mean, _min, _max) in readings {
        // Extract hour from minute_bucket
        let hour_bucket = format!("{}:00:00Z", &minute_bucket[..13]);
        let key = (sensor_id, hour_bucket);

        hourly
            .entry(key.clone())
            .or_insert_with(|| (Vec::new(), 0))
            .0
            .push(mean);
        hourly.get_mut(&key).unwrap().1 += 1;
    }

    // Insert hourly aggregates
    for ((sensor_id, hour_bucket), (values, count)) in hourly.iter() {
        if values.is_empty() {
            continue;
        }

        let mean = values.iter().sum::<f32>() / values.len() as f32;
        let variance = values
            .iter()
            .map(|v| (v - mean).powi(2))
            .sum::<f32>()
            / values.len() as f32;
        let stddev = variance.sqrt();

        let min = values.iter().cloned().fold(f32::INFINITY, f32::min);
        let max = values.iter().cloned().fold(f32::NEG_INFINITY, f32::max);

        archive_conn.execute(
            "INSERT OR REPLACE INTO hourly_readings
             (sensor_id, hour_bucket, reading_count, value_mean, value_min, value_max, value_stddev)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![*sensor_id, hour_bucket.clone(), *count, mean, min, max, stddev],
        )?;
    }

    archive_conn.execute("VACUUM", [])?;

    // Delete aggregated readings for this month from source
    let deleted = source_conn.execute(
        "DELETE FROM aggregated_readings WHERE minute_bucket >= ?1 AND minute_bucket < ?2",
        rusqlite::params![month_start, next_month],
    )?;

    source_conn.execute("VACUUM", [])?;

    println!(
        "[compaction] Monthly archival: created {}, archived {} entries",
        archive_filename, deleted
    );

    Ok(deleted as u64)
}

/// Cleanup old aggregated readings (older than retention_days)
pub fn cleanup_old_aggregates(
    db_path: &str,
    retention_days: u32,
) -> Result<u64, Box<dyn std::error::Error>> {
    let conn = Connection::open(db_path)?;

    let cutoff_ts = Utc::now() - Duration::days(retention_days as i64);

    let deleted = conn.execute(
        "DELETE FROM aggregated_readings WHERE minute_bucket < ?1",
        [cutoff_ts.to_rfc3339()],
    )?;

    conn.execute("VACUUM", [])?;

    println!(
        "[compaction] Cleanup: deleted {} old aggregated readings",
        deleted
    );

    Ok(deleted as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn scheduler_weekly_aggregation_resets() {
        let mut scheduler = CompactionScheduler::new();
        let monday = Utc.with_ymd_and_hms(2024, 11, 25, 0, 0, 0).unwrap(); // A Monday

        assert!(scheduler.should_aggregate_weekly(monday));
        scheduler.mark_weekly_done(monday);

        // Same day should not trigger again
        assert!(!scheduler.should_aggregate_weekly(monday));

        // Next week should trigger
        let next_week = monday + Duration::days(7);
        assert!(scheduler.should_aggregate_weekly(next_week));
    }

    #[test]
    fn scheduler_monthly_archival_resets() {
        let mut scheduler = CompactionScheduler::new();
        let first_of_month = Utc.with_ymd_and_hms(2024, 11, 1, 0, 0, 0).unwrap();

        assert!(scheduler.should_archive_monthly(first_of_month));
        scheduler.mark_monthly_done(first_of_month);

        // Same day should not trigger again
        assert!(!scheduler.should_archive_monthly(first_of_month));

        // Next month should trigger
        let next_month = first_of_month + Duration::days(30);
        assert!(scheduler.should_archive_monthly(next_month));
    }

    #[test]
    fn disk_status_calculations() {
        let status = DiskStatus::new(5_400_000_000, 5_400_000_000); // 100% full
        assert!(status.is_critical);
        assert_eq!(status.used_percent, 100.0);

        let status = DiskStatus::new(4_860_000_000, 5_400_000_000); // 90% full
        assert!(status.is_warning);
        assert!(!status.is_critical);

        let status = DiskStatus::new(2_700_000_000, 5_400_000_000); // 50% full
        assert!(!status.is_warning);
        assert!(!status.is_critical);
    }

    #[test]
    fn scheduler_disk_check_timing() {
        let mut scheduler = CompactionScheduler::new();
        let now = Utc::now();

        assert!(scheduler.should_check_disk(now));
        scheduler.mark_disk_check_done(now);

        // 30 minutes later should not trigger
        let later_30m = now + Duration::minutes(30);
        assert!(!scheduler.should_check_disk(later_30m));

        // 61 minutes later should trigger
        let later_61m = now + Duration::minutes(61);
        assert!(scheduler.should_check_disk(later_61m));
    }

    #[test]
    fn storage_breakdown_total() {
        let breakdown = StorageBreakdown {
            readings_db: 1_000_000,
            archives_total: 2_000_000,
            audit_db: 500_000,
            blockchain_db: 100_000,
        };

        assert_eq!(breakdown.total(), 3_600_000);
    }

    #[test]
    fn disk_status_string() {
        let critical = DiskStatus::new(5_200_000_000, 5_400_000_000);
        assert_eq!(critical.status_str(), "CRITICAL");

        let warning = DiskStatus::new(4_500_000_000, 5_400_000_000);
        assert_eq!(warning.status_str(), "WARNING");

        let ok = DiskStatus::new(2_700_000_000, 5_400_000_000);
        assert_eq!(ok.status_str(), "OK");
    }
}
