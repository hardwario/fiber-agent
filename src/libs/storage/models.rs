//! Data models for storage layer
//! Represents the core entities stored in the medical thermometer database

use crate::libs::alarms::AlarmState;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A single temperature sensor reading
/// Represents one instantaneous measurement from a sensor
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorReading {
    /// Unique database ID
    pub id: i64,

    /// Unix timestamp (seconds since epoch) when reading was taken
    pub timestamp: i64,

    /// Sensor line number (0-7 for 8 sensors)
    pub sensor_line: u8,

    /// Temperature in Celsius with 0.01°C precision
    pub temperature_c: f32,

    /// Whether sensor is connected and responding
    pub is_connected: bool,

    /// Current alarm state of this sensor at this moment
    pub alarm_state: String, // Stored as string in DB for compliance

    /// Unix timestamp when this record was inserted into the database
    pub created_at: i64,

    /// HMAC-SHA256 of reading data for integrity verification (EU MDR)
    pub data_hmac: Option<String>,
}

impl SensorReading {
    /// Create a new sensor reading
    pub fn new(
        timestamp: i64,
        sensor_line: u8,
        temperature_c: f32,
        is_connected: bool,
        alarm_state: AlarmState,
    ) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        Self {
            id: 0, // Will be auto-assigned by database
            timestamp,
            sensor_line,
            temperature_c,
            is_connected,
            alarm_state: alarm_state.to_string(),
            created_at: now,
            data_hmac: None,
        }
    }
}

/// An alarm event record
/// Tracks when and how alarm states transition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlarmEvent {
    /// Unique database ID
    pub id: i64,

    /// Unix timestamp (seconds) when this state transition occurred
    pub timestamp: i64,

    /// Which sensor line (0-7) triggered this event
    pub sensor_line: u8,

    /// Previous alarm state
    pub from_state: String,

    /// New alarm state
    pub to_state: String,

    /// Temperature reading at the moment of transition
    pub temperature_c: Option<f32>,

    /// JSON-encoded additional context (sensor name, thresholds crossed, etc.)
    pub details: Option<String>,
}

impl AlarmEvent {
    /// Create a new alarm event
    pub fn new(
        timestamp: i64,
        sensor_line: u8,
        from_state: AlarmState,
        to_state: AlarmState,
        temperature_c: Option<f32>,
    ) -> Self {
        Self {
            id: 0,
            timestamp,
            sensor_line,
            from_state: from_state.to_string(),
            to_state: to_state.to_string(),
            temperature_c,
            details: None,
        }
    }

}

/// Audit trail entry for system operations
/// Tracks all database operations for EU MDR compliance
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLogEntry {
    /// Unique database ID
    pub id: i64,

    /// Unix timestamp when operation occurred
    pub timestamp: i64,

    /// Type of operation: INSERT, DELETE, SCHEMA_CHANGE, EXPORT, ERROR, etc.
    pub operation: String,

    /// Which table was affected (sensor_readings, alarm_events, etc.)
    pub table_name: Option<String>,

    /// How many records were affected by this operation
    pub record_count: Option<i64>,

    /// How long the operation took in milliseconds
    pub duration_ms: Option<i64>,

    /// Name of the Rust thread that performed this operation
    pub thread_id: Option<String>,

    /// JSON-encoded additional context about the operation
    pub details: Option<String>,

    /// Error message if the operation failed, None if successful
    pub error_msg: Option<String>,

    /// SHA-256 hash of this record's content (tamper detection)
    pub record_hash: Option<String>,

    /// SHA-256 hash of previous audit record (chain integrity)
    pub previous_hash: Option<String>,
}

impl AuditLogEntry {
    /// Create a new successful audit log entry
    pub fn new_success(operation: impl Into<String>, table_name: Option<String>) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        Self {
            id: 0,
            timestamp: now,
            operation: operation.into(),
            table_name,
            record_count: None,
            duration_ms: None,
            thread_id: None,
            details: None,
            error_msg: None,
            record_hash: None,
            previous_hash: None,
        }
    }

    /// Create a new failed audit log entry
    pub fn new_error(
        operation: impl Into<String>,
        table_name: Option<String>,
        error_msg: impl Into<String>,
    ) -> Self {
        let mut entry = Self::new_success(operation, table_name);
        entry.error_msg = Some(error_msg.into());
        entry
    }

    /// Add record count affected by this operation
    pub fn with_record_count(mut self, count: i64) -> Self {
        self.record_count = Some(count);
        self
    }

    /// Add operation duration in milliseconds
    pub fn with_duration_ms(mut self, duration: i64) -> Self {
        self.duration_ms = Some(duration);
        self
    }

    /// Add thread identifier
    pub fn with_thread_id(mut self, thread_id: String) -> Self {
        self.thread_id = Some(thread_id);
        self
    }

    /// Add detailed context as JSON
    pub fn with_details(mut self, details: String) -> Self {
        self.details = Some(details);
        self
    }
}

/// Schema version tracking for migrations
/// Required for MDR compliance documentation
#[derive(Debug, Clone)]
pub struct SchemaVersion {
    /// Schema version number
    pub version: i32,

    /// Unix timestamp when this schema was applied
    pub applied_at: i64,

    /// Human-readable description of what changed in this version
    pub description: String,
}

impl SchemaVersion {
    /// Create a new schema version entry
    pub fn new(version: i32, description: impl Into<String>) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        Self {
            version,
            applied_at: now,
            description: description.into(),
        }
    }
}

/// Database statistics for monitoring storage consumption
#[derive(Debug, Clone)]
pub struct StorageStats {
    /// Total number of sensor readings stored
    pub total_readings: i64,

    /// Total number of alarm events stored
    pub total_alarm_events: i64,

    /// Total number of audit log entries
    pub total_audit_entries: i64,

    /// Current database file size in bytes
    pub db_size_bytes: i64,

    /// Oldest reading timestamp (for retention tracking)
    pub oldest_reading_timestamp: Option<i64>,

    /// Newest reading timestamp
    pub newest_reading_timestamp: Option<i64>,

    /// Database file path
    pub db_path: String,
}

impl fmt::Display for StorageStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let size_mb = self.db_size_bytes as f64 / (1024.0 * 1024.0);
        write!(
            f,
            "StorageStats {{ readings: {}, alarms: {}, audit: {}, size: {:.2}MB, path: {} }}",
            self.total_readings, self.total_alarm_events, self.total_audit_entries, size_mb, self.db_path
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sensor_reading_creation() {
        let reading = SensorReading::new(1000, 0, 36.5, true, AlarmState::Normal);
        assert_eq!(reading.sensor_line, 0);
        assert_eq!(reading.temperature_c, 36.5);
        assert!(reading.is_connected);
        assert_eq!(reading.alarm_state, "NORMAL");
    }

    #[test]
    fn test_alarm_event_creation() {
        let event = AlarmEvent::new(
            1000,
            1,
            AlarmState::Normal,
            AlarmState::Warning,
            Some(37.0),
        );
        assert_eq!(event.sensor_line, 1);
        assert_eq!(event.from_state, "NORMAL");
        assert_eq!(event.to_state, "WARNING");
        assert_eq!(event.temperature_c, Some(37.0));
    }

    #[test]
    fn test_audit_log_entry() {
        let entry = AuditLogEntry::new_success("INSERT", Some("sensor_readings".to_string()))
            .with_record_count(100)
            .with_duration_ms(25);

        assert_eq!(entry.operation, "INSERT");
        assert_eq!(entry.record_count, Some(100));
        assert_eq!(entry.duration_ms, Some(25));
        assert!(entry.error_msg.is_none());
    }

    #[test]
    fn test_schema_version() {
        let schema = SchemaVersion::new(1, "Initial schema with sensor_readings and alarm_events tables");
        assert_eq!(schema.version, 1);
        assert!(schema.description.contains("Initial"));
    }
}
