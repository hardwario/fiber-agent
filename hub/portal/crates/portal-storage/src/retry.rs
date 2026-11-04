//! Bounded retry around a contended write.
//!
//! [`crate::BUSY_TIMEOUT`] already makes SQLite wait 30 s inside a single
//! statement, so reaching this helper means the lock was held for longer than
//! that — in practice, a full `VACUUM` of a large database. Retrying is what
//! turns that from a dropped row into a delayed one.
//!
//! The retry budget is deliberately small. It is not sized to outlast a
//! rewrite; the spill journal does that. It exists to absorb the *ordinary*
//! contention — a checkpoint, an incremental-vacuum pause — without involving
//! the journal at all, and to give a writer that raced the start of a
//! maintenance run a moment to notice.

use std::time::Duration;

use crate::error::Result;

/// Attempts, including the first.
const MAX_ATTEMPTS: u32 = 4;

/// Base backoff; doubles per attempt (50, 100, 200 ms → ~350 ms total).
const BASE_BACKOFF: Duration = Duration::from_millis(50);

/// Run `op`, retrying while it fails with a retryable error.
///
/// Blocking — every caller is already on a dedicated writer thread or inside
/// `spawn_blocking`. Returns the last error if the budget is exhausted, so the
/// caller can decide to spill.
pub fn with_retry<T, F>(mut op: F) -> Result<T>
where
    F: FnMut() -> Result<T>,
{
    let mut backoff = BASE_BACKOFF;

    for attempt in 1..=MAX_ATTEMPTS {
        match op() {
            Ok(value) => return Ok(value),
            Err(err) if err.is_retryable() && attempt < MAX_ATTEMPTS => {
                tracing::debug!(
                    %err,
                    attempt,
                    backoff_ms = backoff.as_millis(),
                    "storage write contended; retrying"
                );
                std::thread::sleep(backoff);
                backoff *= 2;
            }
            Err(err) => return Err(err),
        }
    }

    unreachable!("the loop returns on the final attempt")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::StorageError;

    fn busy() -> StorageError {
        StorageError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".into()),
        ))
    }

    #[test]
    fn a_busy_error_is_retryable_and_a_constraint_error_is_not() {
        assert!(busy().is_retryable());
        assert!(
            !StorageError::Conflict("dup".into()).is_retryable(),
            "a unique violation must not be retried — it would busy-loop"
        );
    }

    #[test]
    fn a_contended_write_succeeds_on_a_later_attempt() {
        let mut attempts = 0;
        let result: Result<u8> = with_retry(|| {
            attempts += 1;
            if attempts < 3 {
                Err(busy())
            } else {
                Ok(7)
            }
        });

        assert_eq!(result.expect("should succeed once the lock clears"), 7);
        assert_eq!(attempts, 3);
    }

    #[test]
    fn an_exhausted_budget_returns_the_error_rather_than_looping_forever() {
        let mut attempts = 0;
        let result: Result<u8> = with_retry(|| {
            attempts += 1;
            Err(busy())
        });

        assert!(result.is_err(), "the caller must be told, so it can spill");
        assert_eq!(attempts, MAX_ATTEMPTS, "the budget is bounded");
    }

    #[test]
    fn a_non_retryable_error_is_not_retried() {
        let mut attempts = 0;
        let result: Result<u8> = with_retry(|| {
            attempts += 1;
            Err(StorageError::Conflict("dup".into()))
        });

        assert!(result.is_err());
        assert_eq!(attempts, 1, "retrying a constraint violation hides the bug");
    }
}
