//! Multi-destination store-and-forward MQTT exporter for the save-and-feed
//! pipeline. The firmware DB (`sticker_readings`, `sensor_readings`,
//! `alarm_events`) is the authoritative store; this module drains rows
//! past per-(broker_id, stream) cursors and publishes them at QoS 1.

pub mod config;
pub mod destination;
pub mod drain;
pub mod envelope;

pub use config::{DestinationConfig, ExportConfig, TlsConfig};
pub use destination::RumqttcDestination;
pub use drain::{drain_one_batch, DrainConfig, Publisher, Stream};
pub use envelope::{alarm_envelope, probe_envelope, sticker_envelope};

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

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

/// Orchestrator task: one tokio task per process that drives N destinations,
/// each with M streams. On each tick, the orchestrator runs `drain_one_batch`
/// for every (destination, stream) pair.
pub struct MqttExportThread {
    pub handle: ExportHandle,
    _join: tokio::task::JoinHandle<()>,
}

impl MqttExportThread {
    /// Spawn the orchestrator. Caller must hold a tokio runtime alive for
    /// the lifetime of the returned object. If `cfg.enabled` is false, the
    /// task is created but drains nothing — only the command channel is
    /// honored, which keeps callers (ConfigApplier, MQTT command handlers)
    /// from having to check enabled state themselves.
    pub fn spawn(
        cfg: ExportConfig,
        db_path: PathBuf,
        storage: StorageHandle,
        hostname: String,
    ) -> Self {
        let (tx, mut rx) = mpsc::channel::<ExportCommand>(64);
        let handle = ExportHandle { tx };
        let join = tokio::spawn(async move {
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

            let mut tick =
                tokio::time::interval(Duration::from_millis(cfg.drain_interval_ms.max(50)));

            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        for (broker_id, dest, streams) in &destinations {
                            for s in streams {
                                let drain_cfg = DrainConfig {
                                    broker_id: broker_id.clone(),
                                    db_path:   db_path.clone(),
                                    batch_size: cfg.batch_size,
                                    drain_interval_ms: cfg.drain_interval_ms,
                                };
                                if let Err(e) = drain_one_batch(&drain_cfg, *s, dest.as_ref(), &storage).await {
                                    eprintln!("[mqtt_export:{}:{:?}] drain error: {}", broker_id, s, e);
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
                                let _ = storage.reset_export_cursor(broker_id, stream);
                            }
                        }
                    }
                }
            }
            eprintln!("[mqtt_export] shutting down");
        });

        Self {
            handle,
            _join: join,
        }
    }
}
