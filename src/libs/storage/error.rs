//! Storage module error types for EU MDR compliance

use std::fmt;

/// Storage system errors
#[derive(Debug, Clone)]
pub enum StorageError {
    /// Database initialization failed
    DatabaseInitError(String),

    /// Query execution failed
    QueryError(String),

    /// Data insertion failed
    InsertError(String),

    /// Data deletion failed (e.g., for retention policy)
    DeleteError(String),

    /// Storage capacity exceeded and no more data can be written
    CapacityExceeded,

    /// Disk I/O error
    IoError(String),

    /// Database corruption detected
    CorruptionDetected(String),

    /// Thread communication error (storage thread unreachable)
    ChannelError(String),

    /// Schema migration error
    MigrationError(String),

    /// Audit log error
    AuditError(String),

    /// Invalid operation (e.g., query on uninitialized database)
    InvalidOperation(String),

    /// Data integrity verification failed (hash chain broken or HMAC mismatch)
    IntegrityError(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::DatabaseInitError(msg) => write!(f, "Database init failed: {}", msg),
            StorageError::QueryError(msg) => write!(f, "Query failed: {}", msg),
            StorageError::InsertError(msg) => write!(f, "Insert failed: {}", msg),
            StorageError::DeleteError(msg) => write!(f, "Delete failed: {}", msg),
            StorageError::CapacityExceeded => write!(f, "Storage capacity exceeded (5GB limit)"),
            StorageError::IoError(msg) => write!(f, "I/O error: {}", msg),
            StorageError::CorruptionDetected(msg) => write!(f, "Database corruption: {}", msg),
            StorageError::ChannelError(msg) => write!(f, "Channel error: {}", msg),
            StorageError::MigrationError(msg) => write!(f, "Migration failed: {}", msg),
            StorageError::AuditError(msg) => write!(f, "Audit error: {}", msg),
            StorageError::InvalidOperation(msg) => write!(f, "Invalid operation: {}", msg),
            StorageError::IntegrityError(msg) => write!(f, "Integrity error: {}", msg),
        }
    }
}

impl std::error::Error for StorageError {}

/// Result type for storage operations
pub type StorageResult<T> = Result<T, StorageError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = StorageError::CapacityExceeded;
        assert_eq!(err.to_string(), "Storage capacity exceeded (5GB limit)");

        let err = StorageError::QueryError("No data".to_string());
        assert!(err.to_string().contains("Query failed"));
    }
}
