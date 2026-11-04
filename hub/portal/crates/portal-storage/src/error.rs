//! Storage error type.

/// Errors surfaced by [`crate::Storage`] and its repositories.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("migration error: {0}")]
    Migration(#[from] refinery::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Unique-constraint violation (e.g. duplicate username), distinguished
    /// from other sqlite errors so callers can turn it into a 409 without
    /// string-matching `StorageError::Sqlite`.
    #[error("conflict: {0}")]
    Conflict(String),
    /// Corrupt JSON in a column this crate (de)serializes itself
    /// (e.g. `recipients` arrays).
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl StorageError {
    /// `true` when retrying the same statement can plausibly succeed.
    ///
    /// This is the whole difference between "delayed" and "lost". `SQLITE_BUSY`
    /// / `SQLITE_LOCKED` mean another connection holds the write lock — a
    /// checkpoint, an incremental-vacuum pause, or a full `VACUUM` outlasting
    /// [`crate::BUSY_TIMEOUT`]. Those clear on their own. A constraint
    /// violation or a corrupt file does not, and retrying it is a busy-loop
    /// that hides the real failure, so those callers spill instead.
    ///
    /// Checked against the *primary* result code: SQLite reports the extended
    /// forms (`SQLITE_BUSY_SNAPSHOT`, `SQLITE_LOCKED_SHAREDCACHE`, …) in the
    /// same family, and every member of both families is a contention error.
    pub fn is_retryable(&self) -> bool {
        match self {
            StorageError::Sqlite(rusqlite::Error::SqliteFailure(f, _)) => matches!(
                f.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ),
            _ => false,
        }
    }

    /// `true` when the write failed because the partition is full (#124):
    /// `SQLITE_FULL`, or `ENOSPC` from a file this crate writes itself.
    /// Callers show it as such instead of a generic failure, because the
    /// operator can act on it and a retry cannot.
    pub fn is_disk_full(&self) -> bool {
        match self {
            StorageError::Sqlite(rusqlite::Error::SqliteFailure(f, _)) => {
                f.code == rusqlite::ErrorCode::DiskFull
            }
            StorageError::Io(e) => e.kind() == std::io::ErrorKind::StorageFull,
            _ => false,
        }
    }
}

pub type Result<T> = std::result::Result<T, StorageError>;

/// `true` when a rusqlite error is a UNIQUE-constraint violation — the repos
/// map these to [`StorageError::Conflict`].
pub(crate) fn is_unique_violation(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _)
            if f.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
    )
}
