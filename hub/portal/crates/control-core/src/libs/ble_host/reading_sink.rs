//! `ReadingSink` over the agent's storage thread.
//!
//! **Decision #13 is not settled** (`strnad/telemetry-single-store` vs `storage-lifecycle-plan`
//! §8). This is the "#13 not merged" binding the plan specifies: the agent's own SQLite store,
//! reached through `StorageHandle`. If #13 lands, this file is replaced by a `PortalStorageReadingSink`
//! over `portal_storage::EyeRepo` — one file, no feature flag, because only one of the two stores
//! can exist at a time. The crate itself changes not at all, which is the entire reason
//! `ReadingSink` is a trait.
//!
//! **Idempotence is the store's, not ours.** `message_id` is `UNIQUE` in `eye_readings`, so a
//! replayed row is an insert that does nothing rather than a duplicate reading. The crate builds
//! ids that include the timestamp (`{MAC}-adv-{received_at}`, `{MAC}-rec-{ts}`) precisely so that a
//! re-broadcast of the same payload is a second reading and a genuine replay is not.

use control_ble::error::{Component, Error, Kind, Result};
use control_ble::host::{ReadingRow, ReadingSink};
use control_ble::mac::MacAddr;

use crate::libs::storage::StorageHandle;

pub struct StorageReadingSink {
    handle: StorageHandle,
}

impl StorageReadingSink {
    pub fn new(handle: StorageHandle) -> StorageReadingSink {
        StorageReadingSink { handle }
    }
}

fn storage_error(what: &str, e: impl std::fmt::Display) -> Error {
    Error::new(
        Kind::StorageError,
        None,
        Component::BeaconRegistry,
        format!("{what}: {e}"),
    )
}

impl ReadingSink for StorageReadingSink {
    fn store_reading(&self, row: &ReadingRow) -> Result<()> {
        let payload = serde_json::to_string(&row.payload)
            .map_err(|e| storage_error("cannot serialise the reading payload", e))?;
        self.handle
            .write_beacon_reading(
                row.mac.clone(),
                row.ts,
                row.received_at,
                row.message_id.clone(),
                row.event_type.clone(),
                payload,
            )
            .map_err(|e| storage_error("cannot queue an eye reading", e))
    }

    fn max_reading_ts(&self, mac: &MacAddr, event_type: &str) -> Result<Option<i64>> {
        self.handle
            .max_beacon_reading_ts(mac.to_string(), event_type.to_string())
            .map_err(|e| storage_error("cannot read the archive resume point", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::storage::StorageThread;

    /// `spawn` returns `(handle, join)`; the join handle is kept alive for the test's duration so
    /// the thread is not torn down while a query is in flight.
    fn storage(dir: &std::path::Path) -> (StorageHandle, std::thread::JoinHandle<()>) {
        StorageThread::spawn(dir.join("fiber.db").to_str().expect("path"), 1)
            .expect("storage thread")
    }

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let base = std::env::temp_dir().join(format!(
                "ble-sink-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&base).expect("temp dir");
            TempDir(base)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn row(mac: &str, ts: i64, id: &str, kind: &str) -> ReadingRow {
        ReadingRow {
            mac: mac.to_string(),
            ts,
            received_at: ts,
            message_id: id.to_string(),
            event_type: kind.to_string(),
            payload: serde_json::json!({ "temperature_c": -18.5 }),
        }
    }

    #[test]
    fn reading_sink_writes_eye_readings_idempotently() {
        let dir = TempDir::new("idem");
        let (_handle, _join) = storage(&dir.0);
        let sink = StorageReadingSink::new(_handle.clone());
        let mac = MacAddr::parse("AA:BB:CC:DD:EE:FF").expect("mac");

        let first = row(
            "AA:BB:CC:DD:EE:FF",
            1_700_000_000,
            "AA-rec-1700000000",
            "recording",
        );
        sink.store_reading(&first).expect("first write");
        assert_eq!(
            wait_for_ts(&sink, &mac, "recording"),
            Some(1_700_000_000),
            "the first write lands"
        );

        // The same `message_id`, a LATER timestamp. `message_id` is UNIQUE, so this insert must do
        // nothing at all — and if it instead became an upsert, or the constraint went missing, the
        // resume point would move to 1_700_000_999 and this assertion would catch it.
        //
        // Asserted through the sink rather than by opening the file: a second connection while the
        // storage thread holds the database is exactly what breaks under CI's container
        // filesystem, and the question here is about the schema, not about SQLite's locking.
        let replay = row(
            "AA:BB:CC:DD:EE:FF",
            1_700_000_999,
            "AA-rec-1700000000",
            "recording",
        );
        sink.store_reading(&replay).expect("a replay is accepted");
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert_eq!(
            sink.max_reading_ts(&mac, "recording").expect("query"),
            Some(1_700_000_000),
            "one message_id is one row, however often it is replayed"
        );

        // And a genuinely new id does move it — otherwise the assertion above would also pass on a
        // sink that silently dropped every write after the first.
        let next = row(
            "AA:BB:CC:DD:EE:FF",
            1_700_000_999,
            "AA-rec-1700000999",
            "recording",
        );
        sink.store_reading(&next).expect("a new reading");
        assert_eq!(
            wait_for_ts_at_least(&sink, &mac, "recording", 1_700_000_999),
            Some(1_700_000_999)
        );
    }

    #[test]
    fn max_reading_ts_is_scoped_to_the_event_type_and_the_beacon() {
        let dir = TempDir::new("scope");
        let (handle, _join) = storage(&dir.0);
        let sink = StorageReadingSink::new(handle);
        let mac = MacAddr::parse("AA:BB:CC:DD:EE:FF").expect("mac");

        sink.store_reading(&row("AA:BB:CC:DD:EE:FF", 100, "a-rec-100", "recording"))
            .expect("write");
        // A later *advertising* row must not move the archive resume point: resuming from it would
        // skip every archive record between the last download and now.
        sink.store_reading(&row("AA:BB:CC:DD:EE:FF", 999, "a-adv-999", "advertising"))
            .expect("write");
        // Nor must another beacon's archive.
        sink.store_reading(&row("11:22:33:44:55:66", 888, "b-rec-888", "recording"))
            .expect("write");

        assert_eq!(wait_for_ts(&sink, &mac, "recording"), Some(100));
    }

    #[test]
    fn an_unknown_beacon_has_no_resume_point_rather_than_a_zero() {
        // Zero would be 1970 and would ask a beacon for its entire archive on every boot.
        let dir = TempDir::new("empty");
        let (handle, _join) = storage(&dir.0);
        let sink = StorageReadingSink::new(handle);
        let mac = MacAddr::parse("00:00:00:00:00:01").expect("mac");
        assert_eq!(sink.max_reading_ts(&mac, "recording").expect("query"), None);
    }

    /// The storage thread is asynchronous; poll briefly rather than sleeping a fixed time.
    fn wait_for_ts(sink: &StorageReadingSink, mac: &MacAddr, kind: &str) -> Option<i64> {
        for _ in 0..200 {
            if let Ok(Some(ts)) = sink.max_reading_ts(mac, kind) {
                return Some(ts);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    }

    /// Poll until the resume point reaches `want`.
    fn wait_for_ts_at_least(
        sink: &StorageReadingSink,
        mac: &MacAddr,
        kind: &str,
        want: i64,
    ) -> Option<i64> {
        for _ in 0..200 {
            if let Ok(Some(ts)) = sink.max_reading_ts(mac, kind) {
                if ts >= want {
                    return Some(ts);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    }
}
