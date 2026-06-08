//! Multi-destination store-and-forward MQTT exporter for the save-and-feed
//! pipeline. The firmware DB (`sticker_readings`, `sensor_readings`,
//! `alarm_events`) is the authoritative store; this module drains rows
//! past per-(broker_id, stream) cursors and publishes them at QoS 1.

pub mod config;
pub mod destination;
pub mod drain;
pub mod envelope;
pub mod replay;
#[cfg(test)]
mod integration_tests;

pub use config::{DestinationConfig, ExportConfig, TlsConfig};
pub use destination::RumqttcDestination;
pub use drain::{drain_one_batch, DrainConfig, Publisher, Stream};
pub use envelope::{alarm_envelope, probe_envelope, sticker_envelope};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::libs::storage::db::Database;
use crate::libs::storage::reader::StorageReader;
use crate::libs::storage::StorageHandle;

/// Commands the export thread accepts from the rest of the system.
#[derive(Debug, Clone)]
pub enum ExportCommand {
    /// Hint that the drain should flush rows for a specific `dev_eui` ASAP.
    /// Currently advisory — the natural drain pass picks up the rows on its
    /// next tick. See spec Section 4 for a targeted flush.
    FlushDevEui { dev_eui: String, max_wait: Duration },
    /// Reset a `(broker_id, stream)` cursor to 0 so the next drain replays
    /// the entire stream.
    ResetCursor { broker_id: String, stream: String },
    /// Graceful shutdown of the orchestrator task.
    Shutdown,
}

/// Handle for sending commands to the export orchestrator from any thread.
#[derive(Clone)]
pub struct ExportHandle {
    tx: mpsc::Sender<ExportCommand>,
}

impl ExportHandle {
    pub fn request_flush(&self, dev_eui: String, max_wait: Duration) -> Result<(), String> {
        self.tx
            .try_send(ExportCommand::FlushDevEui { dev_eui, max_wait })
            .map_err(|e| format!("flush: {}", e))
    }

    pub fn reset_cursor(&self, broker_id: String, stream: String) -> Result<(), String> {
        self.tx
            .try_send(ExportCommand::ResetCursor { broker_id, stream })
            .map_err(|e| format!("reset: {}", e))
    }

    pub fn shutdown(&self) -> Result<(), String> {
        self.tx
            .try_send(ExportCommand::Shutdown)
            .map_err(|e| format!("shutdown: {}", e))
    }
}

/// Orchestrator handle plus the future that drives it. Callers (`main`)
/// run the future on a dedicated current-thread tokio runtime; the handle
/// is the side-channel used by the rest of the process to send commands.
///
/// We do NOT `tokio::spawn` internally because the orchestrator holds a
/// non-`Send` `rusqlite::Connection` across `.await` points and the
/// multi-thread spawn API requires `Send`. The dedicated OS thread already
/// gives us isolation; spawning was an unneeded indirection.
pub struct MqttExportThread;

impl MqttExportThread {
    /// Build the orchestrator. Returns `(handle, future)` — the caller is
    /// responsible for driving the future (typically via `rt.block_on`).
    /// If `cfg.enabled` is false, the future still runs and honors
    /// commands (Shutdown, etc.) but performs no drains.
    pub fn spawn(
        cfg: ExportConfig,
        db_path: PathBuf,
        storage: StorageHandle,
        hostname: String,
    ) -> (ExportHandle, impl std::future::Future<Output = ()>) {
        let (tx, mut rx) = mpsc::channel::<ExportCommand>(64);
        let handle = ExportHandle { tx };
        let fut = async move {
            if !cfg.enabled {
                eprintln!("[mqtt_export] export.enabled=false — worker idle");
                while let Some(cmd) = rx.recv().await {
                    if matches!(cmd, ExportCommand::Shutdown) {
                        break;
                    }
                }
                return;
            }

            // Connect every enabled destination and spawn its eventloop.
            let mut destinations: Vec<(String, Arc<RumqttcDestination>, Vec<Stream>)> = vec![];
            for d in &cfg.destinations {
                if !d.enabled {
                    continue;
                }
                match RumqttcDestination::connect(d, &hostname, cfg.publish_qos) {
                    Ok(dest) => {
                        let dest = Arc::new(dest);
                        let _bg = dest.spawn_event_loop();
                        let streams: Vec<Stream> = cfg
                            .streams
                            .iter()
                            .filter_map(|s| Stream::parse(s))
                            .collect();
                        destinations.push((d.broker_id.clone(), dest, streams));
                    }
                    Err(e) => eprintln!("[mqtt_export] connect {} failed: {}", d.broker_id, e),
                }
            }

            if destinations.is_empty() {
                eprintln!(
                    "[mqtt_export] no enabled destinations connected — worker idle (commands still honored)"
                );
            }

            // One read-side SQLite connection for the whole orchestrator. The
            // drain used to call `Database::new(...)` on every pass, which
            // re-ran `create_schema` + `verify_schema` against SQLCipher and
            // burned ~90% of a core on AES alone (observed in field).
            // Database::new is idempotent on existing files, so doing it once
            // is enough — the WAL connection stays valid for the lifetime of
            // the export thread. Safe to hold here because the export runs on
            // its own current-thread tokio runtime; the connection never
            // crosses threads.
            let export_conn = match Database::new(&db_path, 1).and_then(|db| db.connect()) {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("[mqtt_export] cannot open export DB: {} — worker idle", e);
                    None
                }
            };

            // In-memory cursor cache, keyed by (broker_id, stream-as-str).
            // Reading the persisted cursor at the start of every drain pass
            // races with the storage thread that applies advance messages
            // asynchronously, which caused identical batches to be re-fetched
            // and re-published in a tight loop (~100% CPU). We seed the cache
            // once from disk on startup and trust the in-process value from
            // then on; the storage thread still owns the persisted copy for
            // crash recovery.
            let mut cursors: HashMap<(String, &'static str), i64> = HashMap::new();
            if let Some(conn) = export_conn.as_ref() {
                for (broker_id, _dest, streams) in &destinations {
                    for s in streams {
                        let cur = StorageReader::load_export_cursor(
                            conn,
                            broker_id,
                            s.as_str(),
                        )
                        .unwrap_or(0);
                        cursors.insert((broker_id.clone(), s.as_str()), cur);
                    }
                }
            }

            let mut tick =
                tokio::time::interval(Duration::from_millis(cfg.drain_interval_ms.max(50)));

            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        let conn = match export_conn.as_ref() {
                            Some(c) => c,
                            None => continue,
                        };
                        for (broker_id, dest, streams) in &destinations {
                            for s in streams {
                                let drain_cfg = DrainConfig {
                                    broker_id: broker_id.clone(),
                                    db_path:   db_path.clone(),
                                    batch_size: cfg.batch_size,
                                    drain_interval_ms: cfg.drain_interval_ms,
                                };
                                let key = (broker_id.clone(), s.as_str());
                                let cur_in = *cursors.get(&key).unwrap_or(&0);
                                match drain_one_batch(&drain_cfg, *s, dest.as_ref(), &storage, conn, cur_in).await {
                                    Ok((_n, new_cur)) => {
                                        cursors.insert(key, new_cur);
                                    }
                                    Err(e) => {
                                        eprintln!("[mqtt_export:{}:{:?}] drain error: {}", broker_id, s, e);
                                    }
                                }
                            }
                        }
                    }
                    cmd = rx.recv() => {
                        match cmd {
                            Some(ExportCommand::Shutdown) | None => break,
                            Some(ExportCommand::FlushDevEui { .. }) => {
                                // For now, the natural drain loop will pick up
                                // the sticker_removed marker row on its next
                                // tick. A targeted flush is future work — see
                                // spec Section 4.
                            }
                            Some(ExportCommand::ResetCursor { broker_id, stream }) => {
                                // Reset the persisted cursor AND invalidate the
                                // in-memory cache for matching streams, otherwise
                                // the drain would keep skipping rows we asked to
                                // replay.
                                let _ = storage.reset_export_cursor(broker_id.clone(), stream.clone());
                                for s in [
                                    Stream::Sticker,
                                    Stream::Probe,
                                    Stream::Probe1m,
                                    Stream::Alarm,
                                ] {
                                    if s.as_str() == stream {
                                        cursors.insert((broker_id.clone(), s.as_str()), 0);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            eprintln!("[mqtt_export] shutting down");
        };
        (handle, fut)
    }
}
