//! Inner drain loop for the save-and-feed export pipeline.
//!
//! For a given destination + stream, the drain:
//! 1. Loads the per-(broker_id, stream) export cursor.
//! 2. Reads up to `batch_size` rows past the cursor from storage.
//! 3. Wraps each row in an envelope (see `envelope.rs`).
//! 4. Publishes via an injected `Publisher` (rumqttc in production, a stub
//!    in tests) and advances the cursor on each success. On failure, the
//!    drain stops mid-batch so retries pick up exactly where they left off.

use std::path::PathBuf;

use rusqlite::Connection;

use crate::libs::storage::reader::StorageReader;
use crate::libs::storage::{StorageHandle, StorageResult};

/// One of the three streams exported by the save-and-feed pipeline.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Stream {
    Sticker,
    Probe,
    Alarm,
}

impl Stream {
    pub fn as_str(&self) -> &'static str {
        match self {
            Stream::Sticker => "sticker",
            Stream::Probe => "probe",
            Stream::Alarm => "alarm",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sticker" => Some(Stream::Sticker),
            "probe" => Some(Stream::Probe),
            "alarm" => Some(Stream::Alarm),
            _ => None,
        }
    }
}

/// Abstraction over the MQTT publish path. Production uses a `rumqttc::Client`
/// wrapper (see `destination.rs`); tests use an in-memory stub.
#[async_trait::async_trait]
pub trait Publisher: Send + Sync + 'static {
    async fn publish(&self, topic: &str, payload: &[u8]) -> Result<(), String>;
}

/// Per-(destination, drain pass) configuration.
pub struct DrainConfig {
    pub broker_id: String,
    pub db_path: PathBuf,
    pub batch_size: usize,
    pub drain_interval_ms: u64,
}

/// Drain a single batch of `stream` for the destination identified by
/// `cfg.broker_id`, starting from `cursor_in`. Returns `(rows_fetched,
/// new_cursor)` — `new_cursor` is the id of the last successfully published
/// row (or `cursor_in` if nothing was published). Caller MUST treat the
/// returned cursor as the authoritative next-read position; reading the
/// persisted cursor from SQLite at the start of each pass races with the
/// storage thread that applies `advance_export_cursor` messages and causes
/// the same rows to be re-fetched and re-published in a hot loop.
///
/// The caller owns the SQLite `Connection` and reuses it across drain
/// calls — opening a fresh `Database` per call re-runs `create_schema` and
/// `verify_schema` against the encrypted store and easily burns a full CPU
/// core on its own (observed: ~90% CPU at the default 500 ms tick).
pub async fn drain_one_batch(
    cfg: &DrainConfig,
    stream: Stream,
    publisher: &dyn Publisher,
    storage: &StorageHandle,
    conn: &Connection,
    cursor_in: i64,
) -> StorageResult<(usize, i64)> {
    let mut last_id = cursor_in;
    let fetched = match stream {
        Stream::Sticker => {
            let rows =
                StorageReader::fetch_sticker_readings_after(conn, cursor_in, cfg.batch_size)?;
            for row in &rows {
                let (topic, payload) = super::envelope::sticker_envelope(row);
                if let Err(e) = publisher.publish(&topic, payload.as_bytes()).await {
                    eprintln!("[mqtt_export] publish failed on {}: {}", topic, e);
                    break;
                }
                last_id = row.id;
            }
            rows.len()
        }
        Stream::Probe => {
            let rows =
                StorageReader::fetch_sensor_readings_after(conn, cursor_in, cfg.batch_size)?;
            for row in &rows {
                let (topic, payload) = super::envelope::probe_envelope(row);
                if let Err(e) = publisher.publish(&topic, payload.as_bytes()).await {
                    eprintln!("[mqtt_export] publish failed on {}: {}", topic, e);
                    break;
                }
                last_id = row.id;
            }
            rows.len()
        }
        Stream::Alarm => {
            let rows =
                StorageReader::fetch_alarm_events_after(conn, cursor_in, cfg.batch_size)?;
            for row in &rows {
                let (topic, payload) = super::envelope::alarm_envelope(row);
                if let Err(e) = publisher.publish(&topic, payload.as_bytes()).await {
                    eprintln!("[mqtt_export] publish failed on {}: {}", topic, e);
                    break;
                }
                last_id = row.id;
            }
            rows.len()
        }
    };

    // One channel message per batch instead of one per row — keeps the
    // storage thread's inbox from drowning under the export workload.
    if last_id > cursor_in {
        storage.advance_export_cursor(
            cfg.broker_id.clone(),
            stream.as_str().to_string(),
            last_id,
        )?;
    }

    Ok((fetched, last_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::storage::db::Database;
    use crate::libs::storage::StorageThread;
    use std::sync::Mutex;

    struct StubPub {
        calls: Mutex<Vec<(String, String)>>,
        fail_on: Option<usize>,
    }

    #[async_trait::async_trait]
    impl Publisher for StubPub {
        async fn publish(&self, topic: &str, payload: &[u8]) -> Result<(), String> {
            let mut g = self.calls.lock().unwrap();
            let idx = g.len();
            if let Some(f) = self.fail_on {
                if idx == f {
                    return Err("simulated".into());
                }
            }
            g.push((topic.to_string(), String::from_utf8_lossy(payload).into_owned()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn drain_advances_cursor_and_stops_on_publish_failure() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();
        let (storage, join) = StorageThread::spawn(&path, 1).unwrap();

        for i in 0..5 {
            storage
                .write_sticker_reading(
                    "abc".into(),
                    1,
                    1000 + i,
                    1000 + i,
                    format!("abc-{}-0", 1000 + i),
                    "uplink".into(),
                    "{}".into(),
                )
                .unwrap();
        }
        storage.flush().unwrap();
        // Wait briefly for the storage thread to process the writes; without
        // this the drain may observe a partial set of rows.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let stub = StubPub {
            calls: Mutex::new(vec![]),
            fail_on: Some(3),
        };
        let cfg = DrainConfig {
            broker_id: "remote".into(),
            db_path: PathBuf::from(&path),
            batch_size: 10,
            drain_interval_ms: 0,
        };

        let db = Database::new(&path, 1).unwrap();
        let conn = db.connect().unwrap();
        let (n, new_cursor) =
            drain_one_batch(&cfg, Stream::Sticker, &stub, &storage, &conn, 0)
                .await
                .unwrap();
        assert_eq!(n, 5);
        assert_eq!(stub.calls.lock().unwrap().len(), 3); // succeeds 3 then fails
        assert_eq!(new_cursor, 3, "returned cursor should be last published id");

        // Allow the storage thread to apply the (single) advance_export_cursor
        // message before we read back the persisted cursor.
        storage.flush().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let cur = StorageReader::load_export_cursor(&conn, "remote", "sticker").unwrap();
        assert_eq!(cur, 3, "cursor should be at last successfully published id");

        storage.shutdown().unwrap();
        join.join().unwrap();
    }
}
