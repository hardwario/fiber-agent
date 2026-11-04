//! One SQLite connection behind an async-safe handle.
//!
//! `rusqlite::Connection` is `!Sync` and every `Storage` call is blocking, so
//! each async caller used to carry its own copy of the same eleven lines:
//! clone the `Arc<Mutex<Storage>>`, `spawn_blocking`, take the lock for one
//! statement, map a cancelled task to an error. Seven services had that
//! copy, one of them subtly different (it swallowed a panic into an `Io`
//! error instead of resuming it). This is the one copy.
//!
//! The lock is held only for the closure's duration, on the blocking pool —
//! never across an await. A closure that panics propagates the panic; a
//! task cancelled by runtime shutdown becomes a `StorageError::Io` naming
//! the connection's `label`, so a log line says which handle it was.
//!
//! Every handle is its own connection (WAL lets readers run beside the one
//! writer in `persist.rs`). Whether two subsystems share a handle or open
//! their own is decided where they are constructed (`main.rs`), not here.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use portal_storage::{with_retry, Storage, StorageError};

#[derive(Clone)]
pub struct Db {
    storage: Arc<Mutex<Storage>>,
    /// Names the connection in the cancelled-task error and nowhere else.
    label: &'static str,
}

impl Db {
    /// Opens (creating if needed) `<data_dir>/portal.db` as a new connection.
    pub fn open(
        data_dir: &str,
        db_key: Option<&str>,
        label: &'static str,
    ) -> Result<Self, StorageError> {
        std::fs::create_dir_all(data_dir)?;
        let path = Path::new(data_dir).join("portal.db");
        Ok(Self::from_storage(Storage::open(&path, db_key)?, label))
    }

    /// Wraps an already-open connection — for a service that configures its
    /// connection before handing it over, and for tests that need a storage
    /// whose statements fail (`Storage::open_at_version`).
    pub fn from_storage(storage: Storage, label: &'static str) -> Self {
        Self {
            storage: Arc::new(Mutex::new(storage)),
            label,
        }
    }

    /// Runs `f` on a blocking-pool thread with the connection locked for
    /// exactly that long. Reads go here; so do writes that carry their own
    /// busy handling (`crate::durable`).
    pub async fn run<T, F>(&self, f: F) -> Result<T, StorageError>
    where
        F: FnOnce(&Storage) -> Result<T, StorageError> + Send + 'static,
        T: Send + 'static,
    {
        let storage = Arc::clone(&self.storage);
        let joined = tokio::task::spawn_blocking(move || {
            let guard = storage.lock().unwrap_or_else(|poison| poison.into_inner());
            f(&guard)
        })
        .await;

        match joined {
            Ok(result) => result,
            Err(join_err) => match join_err.try_into_panic() {
                Ok(payload) => std::panic::resume_unwind(payload),
                Err(_) => Err(StorageError::Io(std::io::Error::other(format!(
                    "{} db task was cancelled",
                    self.label
                )))),
            },
        }
    }

    /// [`Self::run`] with `portal_storage::with_retry` around `f`: the
    /// statement is retried on `SQLITE_BUSY` with backoff. Every plain write
    /// goes here, so no call site has to remember the retry.
    pub async fn write<T, F>(&self, f: F) -> Result<T, StorageError>
    where
        F: Fn(&Storage) -> Result<T, StorageError> + Send + 'static,
        T: Send + 'static,
    {
        self.run(move |s| with_retry(|| f(s))).await
    }

    /// [`Storage::full_names_for`] for a page's worth of usernames, feeding
    /// [`crate::render::person_label`]. A failed lookup is logged and yields
    /// no names: the reader then sees usernames, which is correct, only less
    /// friendly, and never worth failing a page or an e-mail over.
    pub async fn full_names(&self, usernames: Vec<String>) -> HashMap<String, String> {
        self.run(move |s| s.full_names_for(&usernames))
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(%err, "failed to resolve user full names");
                HashMap::new()
            })
    }

    /// The raw guard, for tests that seed or inspect rows synchronously.
    #[cfg(test)]
    pub(crate) fn lock(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, Storage>> {
        self.storage.lock()
    }

    /// Takes the lock on the *current* thread. Only for a `Drop` impl or a
    /// context that cannot await; every other holder is on the blocking
    /// pool, so this waits at most one statement.
    pub fn blocking<T>(&self, f: impl FnOnce(&Storage) -> T) -> T {
        let guard = self
            .storage
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        f(&guard)
    }
}

#[cfg(test)]
mod migration_tests {
    use portal_storage::Storage;

    /// V30 (issue #126 item 3): every configuration row keyed on `w1_N_tilt_alert` moves to
    /// `w1_N_tilt`; a row already under the new name wins; readings keep the old name.
    #[test]
    fn v30_renames_the_tilt_field_in_configuration_not_in_readings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("portal.db");
        drop(Storage::open_at_version(&path, None, 29).expect("open at V29"));
        {
            let conn = rusqlite::Connection::open(&path).expect("raw open");
            conn.execute_batch(
                r#"
                INSERT INTO field_thresholds (hostname, scope, entity, field, critical_high)
                VALUES ('hub', 'node', 'aa11', 'w1_1_tilt_alert', 1.0),
                       ('hub', 'node', 'aa11', 'w1_2_tilt_alert', 2.0),
                       ('hub', 'node', 'aa11', 'w1_2_tilt', 3.0),
                       ('hub', 'eye', 'AA:BB', 'w1_1_tilt_alert', 4.0);
                INSERT INTO field_threshold_history (hostname, scope, entity, field, actor)
                VALUES ('hub', 'node', 'aa11', 'w1_1_tilt_alert', 'anna');
                INSERT INTO node_threshold_delivery (hostname, node_key, field, state, updated_at)
                VALUES ('hub', 'aa11', 'w1_3_tilt_alert', 'pending', 1);
                INSERT INTO pending_config (hostname, node_key, param_key, kind, desired_value,
                                            author, created_at, updated_at)
                VALUES ('hub', 'aa11', 'threshold.w1_1_tilt_alert.critical_high', 'threshold',
                        '1', 'anna', 1, 1),
                       ('hub', 'aa11', 'sensors.w1_1_tilt_alert_note', 'config', '1', 'anna', 1, 1);
                INSERT INTO sensor_groups (id, name) VALUES (1, 'g');
                INSERT INTO sensor_group_members (group_id, hostname, node_key, fields)
                VALUES (1, 'hub', 'aa11',
                        '["temperature","w1_1_tilt_alert","w1_1_tilt","humidity"]');
                INSERT INTO node_alarm_layout (hostname, node_key, layout, sent_at)
                VALUES ('hub', 'aa11', '["temperature","w1_4_tilt_alert","@sensor_types:3,0,0,0"]', 1);
                INSERT INTO node_field_readings (hostname, node_key, field, ts, value)
                VALUES ('hub', 'aa11', 'w1_1_tilt_alert', 100, 1.0);
                "#,
            )
            .expect("V29 rows");
        }
        drop(Storage::open(&path, None).expect("migrate to V30"));

        let conn = rusqlite::Connection::open(&path).expect("raw open");
        let strings = |sql: &str| -> Vec<String> {
            let mut stmt = conn.prepare(sql).expect("prepare");
            stmt.query_map([], |r| r.get::<_, String>(0))
                .expect("query")
                .map(|r| r.expect("row"))
                .collect()
        };
        assert_eq!(
            strings(
                "SELECT scope || ':' || field || ':' || critical_high FROM field_thresholds
                 ORDER BY scope, field"
            ),
            vec![
                "eye:w1_1_tilt_alert:4.0",
                "node:w1_1_tilt:1.0",
                // The Node's own row under the new name wins over the renamed one.
                "node:w1_2_tilt:3.0",
            ]
        );
        assert_eq!(
            strings("SELECT field FROM field_threshold_history"),
            vec!["w1_1_tilt"]
        );
        assert_eq!(
            strings("SELECT field FROM node_threshold_delivery"),
            vec!["w1_3_tilt"]
        );
        assert_eq!(
            strings("SELECT param_key FROM pending_config ORDER BY param_key"),
            vec![
                "sensors.w1_1_tilt_alert_note",
                "threshold.w1_1_tilt.critical_high"
            ]
        );
        assert_eq!(
            strings("SELECT fields FROM sensor_group_members"),
            vec![r#"["temperature","w1_1_tilt","humidity"]"#]
        );
        assert_eq!(
            strings("SELECT layout FROM node_alarm_layout"),
            vec![r#"["temperature","w1_4_tilt","@sensor_types:3,0,0,0"]"#]
        );
        assert_eq!(
            strings("SELECT field FROM node_field_readings"),
            vec!["w1_1_tilt_alert"],
            "readings are history and keep their name"
        );
    }
}
