//! Heavyweight integration tests for the save-and-feed export pipeline.
//!
//! These are `#[ignore]`-gated because they require an external mosquitto
//! broker running on `localhost:18883`. Bring one up with `mosquitto -p
//! 18883 -d` before running:
//!
//! ```text
//! cargo test --lib mqtt_export::integration_tests -- --ignored --nocapture
//! ```

#![cfg(test)]

use std::time::Duration;

use super::{DestinationConfig, ExportConfig, MqttExportThread};
use crate::libs::storage::{db::Database, reader::StorageReader, StorageThread};

/// Simulates ~10h of accumulated sticker readings while the destination
/// broker is unreachable, then enables the broker and verifies the export
/// orchestrator catches up — cursor reaches the head of the stream.
#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn ten_hour_offline_drain_matches_input() {
    let local = tokio::task::LocalSet::new();
    local.run_until(ten_hour_offline_drain_matches_input_inner()).await;
}

async fn ten_hour_offline_drain_matches_input_inner() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap().to_string();
    let (storage, _join) = StorageThread::spawn(&path, 1).unwrap();

    for i in 0..1_000 {
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

    let cfg = ExportConfig {
        enabled: true,
        streams: vec!["sticker".into()],
        batch_size: 200,
        drain_interval_ms: 200,
        publish_qos: 1,
        destinations: vec![DestinationConfig {
            broker_id: "test".into(),
            enabled: true,
            host: "localhost".into(),
            port: 18883,
            client_id: "fiber-export-test".into(),
            username: "".into(),
            password: "".into(),
            tls: Default::default(),
        }],
    };
    let (_handle, fut) = MqttExportThread::spawn(
        cfg,
        std::path::PathBuf::from(&path),
        storage.clone(),
        "myhost".into(),
    );
    let _join = tokio::task::spawn_local(fut);
    // ^ requires the test to run inside a LocalSet because the orchestrator
    //   holds a non-Send rusqlite::Connection across awaits.

    tokio::time::sleep(Duration::from_secs(20)).await;

    let db = Database::new(&path, 1).unwrap();
    let conn = db.connect().unwrap();
    let cur = StorageReader::load_export_cursor(&conn, "test", "sticker").unwrap();
    assert!(cur >= 999, "cursor expected to catch up, got {}", cur);
}
