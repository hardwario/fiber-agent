//! Medical device storage system with EU MDR 2017/745 compliance
//!
//! Thread-safe SQLite WAL storage for temperature readings and alarm events
//! with full audit trail, 5GB capacity management, and async message-based writes.
//!
//! # Architecture
//!
//! The storage system uses a background thread for non-blocking writes:
//! - Main thread sends StorageMessages via channel
//! - Storage thread processes messages and writes to SQLite
//! - Periodic auto-flush (every 100ms or 1000 messages)
//! - FIFO auto-purge when capacity reached
//!
//! # Example Usage
//!
//! ```ignore
//! use fiber_app::libs::storage::StorageThread;
//! use fiber_app::libs::alarms::AlarmState;
//!
//! // Spawn storage thread
//! let (storage, thread) = StorageThread::spawn("/data/fiber_medical.db", 5)?;
//!
//! // Write a sensor reading (non-blocking)
//! storage.write_sensor_reading(
//!     1000,  // timestamp
//!     0,     // sensor line
//!     36.5,  // temperature
//!     true,  // is_connected
//!     AlarmState::Normal,
//! )?;
//!
//! // Graceful shutdown
//! storage.shutdown()?;
//! thread.join()?;
//! ```

pub mod audit;
pub mod db;
pub mod disk;
pub mod error;
pub mod integrity;
pub mod models;
pub mod reader;
pub mod retention;
pub mod thread;
pub mod writer;

// Re-export public API
pub use disk::{get_partition_usage, PartitionUsage};
pub use error::{StorageError, StorageResult};
pub use models::{AlarmEvent, AuditLogEntry, SensorReading, StorageStats};
pub use reader::StorageReader;
pub use retention::RetentionPolicy;
pub use thread::{StorageHandle, StorageMessage, StorageThread};
pub use writer::StorageWriter;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::alarms::AlarmState;

    #[test]
    fn test_storage_module_exports() {
        // Verify all public types are accessible
        let _: AlarmEvent = AlarmEvent::new(
            1000,
            0,
            AlarmState::Normal,
            AlarmState::Warning,
            Some(37.0),
        );

        let _: SensorReading = SensorReading::new(
            1000,
            0,
            36.5,
            true,
            AlarmState::Normal,
        );
    }
}
