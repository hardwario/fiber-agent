//! LoRaWAN monitor thread
//!
//! Creates a second MQTT client connected to the local Mosquitto broker
//! (where ChirpStack publishes), subscribes to uplink events, parses them,
//! updates shared LoRaWAN state, and periodically publishes sensor data
//! to the FIBER MQTT topic hierarchy.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use crossbeam::channel::{bounded, unbounded, Receiver, Sender};
use prost::Message;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};

use crate::libs::config::LoRaWANConfig;
use crate::libs::mqtt::messages::MqttMessage;
use crate::libs::storage::StorageHandle;

use super::chirpstack;
use super::detector;
use super::state::{SharedLoRaWANState, create_shared_lorawan_state};
use super::sticker_proto::Command;
use super::sticker_response::{decode_response, DecodedResponse};

/// A downlink command queued by a `LoRaWANHandle`, picked up and published by
/// the monitor loop, which then correlates the fPort-85 `Response` back by seq.
struct CommandRequest {
    dev_eui: String,
    seq: u32,
    /// Encoded `Command` protobuf (no proto-version prefix on downlinks).
    bytes: Vec<u8>,
    resp_tx: Sender<DecodedResponse>,
    /// When true the seq may receive several responses (e.g. paged
    /// HistoryFrames that all echo the command seq); the monitor keeps the
    /// registration until the caller drops the receiver, instead of removing
    /// it after the first response.
    multi: bool,
}

/// A raw downlink queued by `LoRaWANHandle::send_raw` — published verbatim on the
/// requested fPort with no seq allocation and no response correlation, so it
/// cannot disturb the seq-correlated command stream. Fire-and-forget ("expert").
struct RawDownlink {
    dev_eui: String,
    bytes: Vec<u8>,
    fport: u8,
}

/// Handle for sending messages to the LoRaWAN monitor
#[derive(Clone)]
pub struct LoRaWANHandle {
    pub state: SharedLoRaWANState,
    cmd_tx: Sender<CommandRequest>,
    raw_tx: Sender<RawDownlink>,
    /// Signals the monitor loop to drop a multi-response registration once its
    /// collector has finished, so a recycled seq cannot alias a stale waiter.
    release_tx: Sender<u32>,
    seq: Arc<AtomicU32>,
}

impl LoRaWANHandle {
    /// Send a downlink `Command` to a STICKER and await the correlated fPort-85
    /// `Response` (matched by the echoed `seq`). The monitor publishes the command
    /// to ChirpStack's `command/down` topic (#33) and resolves the reply by seq
    /// (#34). Returns the decoded response, or an error on timeout / monitor down.
    pub fn send_command(
        &self,
        dev_eui: &str,
        mut command: Command,
        timeout: Duration,
    ) -> Result<DecodedResponse, String> {
        // seq in 1..=250 (avoid 0 = "unsolicited"); wraps for long-lived sessions.
        let seq = (self.seq.fetch_add(1, Ordering::Relaxed) % 250) + 1;
        command.seq = seq;
        let bytes = command.encode_to_vec();
        let (resp_tx, resp_rx) = bounded(1);
        self.cmd_tx
            .send(CommandRequest {
                dev_eui: dev_eui.to_lowercase(),
                seq,
                bytes,
                resp_tx,
                multi: false,
            })
            .map_err(|_| "LoRaWAN monitor not running".to_string())?;
        resp_rx
            .recv_timeout(timeout)
            .map_err(|_| format!("no fPort-85 response for seq {seq} within {timeout:?}"))
    }

    /// Send a downlink `Command` and collect a multi-response reply whose pages
    /// share one seq (e.g. an fPort-85 `HistoryFrame` set). Returns every
    /// response received until `frame_count` frames arrive (learned from the
    /// first HistoryFrame) or no further frame arrives within `frame_timeout`.
    /// A non-history reply (Error/Empty/…) returns immediately as a single item.
    pub fn send_command_collect(
        &self,
        dev_eui: &str,
        mut command: Command,
        frame_timeout: Duration,
    ) -> Result<Vec<DecodedResponse>, String> {
        use super::sticker_response::ResponseKind;
        let seq = (self.seq.fetch_add(1, Ordering::Relaxed) % 250) + 1;
        command.seq = seq;
        let bytes = command.encode_to_vec();
        let (resp_tx, resp_rx) = unbounded();
        self.cmd_tx
            .send(CommandRequest {
                dev_eui: dev_eui.to_lowercase(),
                seq,
                bytes,
                resp_tx,
                multi: true,
            })
            .map_err(|_| "LoRaWAN monitor not running".to_string())?;

        // The first response bounds the collection.
        let first = match resp_rx.recv_timeout(frame_timeout) {
            Ok(resp) => resp,
            Err(_) => {
                // Release the registration so a seq that never got a reply
                // cannot alias a later command that recycles the same seq.
                let _ = self.release_tx.send(seq);
                return Err(format!(
                    "no fPort-85 response for seq {seq} within {frame_timeout:?}"
                ));
            }
        };
        // Bound the collection by DISTINCT frame_index: a duplicated frame must
        // not advance completion, and frame_count is a device estimate that can
        // drift upward mid-replay, so keep the max seen. Non-history replies
        // (Error/Empty/…) leave target 0 and return as a single item.
        let mut seen = std::collections::HashSet::<u32>::new();
        let mut target: u32 = 0;
        if let ResponseKind::HistoryFrame { frame_index, frame_count, .. } = &first.kind {
            seen.insert(*frame_index);
            target = (*frame_count).max(1);
        }
        let mut frames = vec![first];
        while (seen.len() as u32) < target {
            match resp_rx.recv_timeout(frame_timeout) {
                Ok(resp) => {
                    if let ResponseKind::HistoryFrame { frame_index, frame_count, .. } = &resp.kind {
                        seen.insert(*frame_index);
                        target = target.max((*frame_count).max(1));
                    }
                    frames.push(resp);
                }
                Err(_) => break, // inter-frame timeout → return what we have
            }
        }
        // Collection done: deregister the multi seq so its pending_multi entry
        // is dropped deterministically instead of leaking until seq recycles.
        let _ = self.release_tx.send(seq);
        Ok(frames)
    }

    /// Enqueue a raw downlink to a STICKER (default fPort 85) — fire-and-forget.
    /// The bytes are sent verbatim (the caller owns the encoding, e.g. the
    /// docs.hardwario.com downlink generator); no seq is allocated and no
    /// response is awaited, so this cannot disturb the seq-correlated
    /// `send_command` stream. Any device reply is logged as an unmatched
    /// fPort-85 response and dropped.
    pub fn send_raw(&self, dev_eui: &str, bytes: Vec<u8>, fport: u8) -> Result<(), String> {
        self.raw_tx
            .send(RawDownlink {
                dev_eui: dev_eui.to_lowercase(),
                bytes,
                fport,
            })
            .map_err(|_| "LoRaWAN monitor not running".to_string())
    }
}

/// LoRaWAN monitor that bridges ChirpStack MQTT to FIBER MQTT
pub struct LoRaWANMonitor {
    thread_handle: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
    pub state: SharedLoRaWANState,
    cmd_tx: Sender<CommandRequest>,
    raw_tx: Sender<RawDownlink>,
    release_tx: Sender<u32>,
    seq: Arc<AtomicU32>,
}

impl LoRaWANMonitor {
    /// Create and spawn the LoRaWAN monitor thread.
    ///
    /// `mqtt_tx` is the channel sender for the main FIBER MQTT publisher.
    pub fn new(
        config: LoRaWANConfig,
        configs: super::state::SharedLoRaWANSensorConfigs,
        field_threshold_defaults: super::state::SharedFieldThresholdDefaults,
        mqtt_tx: Sender<MqttMessage>,
        hostname: String,
        buzzer_priority_manager: Option<Arc<crate::libs::buzzer::priority::BuzzerPriorityManager>>,
        storage: StorageHandle,
    ) -> io::Result<Self> {
        // Detect built-in gateway hardware and external-gateway config.
        let detection = detector::detect_gateway();
        let gateway_present = detection.is_present();
        let has_external = detector::has_external_gateway();
        let should_run = gateway_present || has_external;

        let state = create_shared_lorawan_state(gateway_present);

        let (cmd_tx, cmd_rx) = unbounded::<CommandRequest>();
        let (raw_tx, raw_rx) = unbounded::<RawDownlink>();
        let (release_tx, release_rx) = unbounded::<u32>();
        let seq = Arc::new(AtomicU32::new(0));

        if !should_run {
            eprintln!(
                "[LoRaWAN Monitor] Not starting: concentratord={}, chirpstack={}, external_gateway={}",
                detection.concentratord_running, detection.chirpstack_running, has_external
            );
            // cmd_rx/raw_rx dropped → send_command/send_raw return "monitor not running".
            return Ok(Self {
                thread_handle: None,
                shutdown_flag: Arc::new(AtomicBool::new(false)),
                state,
                cmd_tx,
                raw_tx,
                release_tx,
                seq,
            });
        }

        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_clone = shutdown_flag.clone();
        let state_clone = state.clone();

        // Handle the loop can use to drive its own downlinks (auto-backfill on a
        // STICKER reconnect, #43). Its cmd_tx feeds the same cmd_rx the loop
        // drains, so the backfill read must run on a blocking task, never inline.
        let self_handle = LoRaWANHandle {
            state: state.clone(),
            cmd_tx: cmd_tx.clone(),
            raw_tx: raw_tx.clone(),
            release_tx: release_tx.clone(),
            seq: seq.clone(),
        };

        let thread_handle = thread::spawn(move || {
            lorawan_loop(
                shutdown_flag_clone,
                state_clone,
                config,
                configs,
                field_threshold_defaults,
                mqtt_tx,
                hostname,
                buzzer_priority_manager,
                storage,
                cmd_rx,
                raw_rx,
                release_rx,
                self_handle,
            );
        });

        eprintln!("[LoRaWAN Monitor] Started");

        Ok(Self {
            thread_handle: Some(thread_handle),
            shutdown_flag,
            state,
            cmd_tx,
            raw_tx,
            release_tx,
            seq,
        })
    }

    /// Get a handle for reading LoRaWAN state and sending downlink commands.
    pub fn handle(&self) -> LoRaWANHandle {
        LoRaWANHandle {
            state: self.state.clone(),
            cmd_tx: self.cmd_tx.clone(),
            raw_tx: self.raw_tx.clone(),
            release_tx: self.release_tx.clone(),
            seq: self.seq.clone(),
        }
    }
}

impl Drop for LoRaWANMonitor {
    fn drop(&mut self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread_handle.take() {
            let timeout = Duration::from_secs(5);
            let start = Instant::now();
            while !handle.is_finished() && start.elapsed() < timeout {
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Main loop for the LoRaWAN monitor thread
fn lorawan_loop(
    shutdown_flag: Arc<AtomicBool>,
    state: SharedLoRaWANState,
    config: LoRaWANConfig,
    configs: super::state::SharedLoRaWANSensorConfigs,
    field_threshold_defaults: super::state::SharedFieldThresholdDefaults,
    mqtt_tx: Sender<MqttMessage>,
    hostname: String,
    buzzer_priority_manager: Option<Arc<crate::libs::buzzer::priority::BuzzerPriorityManager>>,
    storage: StorageHandle,
    cmd_rx: Receiver<CommandRequest>,
    raw_rx: Receiver<RawDownlink>,
    release_rx: Receiver<u32>,
    self_handle: LoRaWANHandle,
) {
    // Build a tokio runtime for the async MQTT client
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[LoRaWAN Monitor] Failed to create tokio runtime: {}", e);
            return;
        }
    };

    rt.block_on(async {
        // Track previous "any sticker critical" state across the full monitor lifetime
        // (NOT reset on reconnect) — so off→on transition detection is stable across
        // MQTT broker hiccups and only fires on_new_sensor_alarm() for genuinely new alarms.
        let mut prev_any_sticker_critical = false;

        // fPort-85 command/response correlation (#34): seq -> waiting sender.
        // Persists across MQTT reconnects. last_app_id is learned from uplink
        // topics so downlinks can target application/{app_id}/device/.../command/down.
        let mut pending: HashMap<u32, Sender<DecodedResponse>> = HashMap::new();
        // Multi-response registrations (paged HistoryFrames share one seq); kept
        // until the caller drops its receiver, at which point the next send fails
        // and the entry is dropped.
        let mut pending_multi: HashMap<u32, Sender<DecodedResponse>> = HashMap::new();
        let mut last_app_id: Option<String> = None;

        loop {
            if shutdown_flag.load(Ordering::Relaxed) {
                break;
            }

            // Update service status
            {
                if let Ok(mut s) = state.write() {
                    s.concentratord_running = detector::is_service_running("chirpstack-concentratord");
                    s.chirpstack_running = detector::is_service_running("chirpstack");
                }
            }

            // Connect to local Mosquitto
            let client_id = format!("fiber-lorawan-{}", &hostname);
            let mut mqttoptions = MqttOptions::new(
                &client_id,
                &config.chirpstack_mqtt_host,
                config.chirpstack_mqtt_port,
            );
            mqttoptions.set_keep_alive(Duration::from_secs(30));
            mqttoptions.set_clean_session(true);

            if let (Some(u), Some(p)) = (
                &config.chirpstack_mqtt_username,
                &config.chirpstack_mqtt_password,
            ) {
                mqttoptions.set_credentials(u, p);
            }

            let (client, mut eventloop) = AsyncClient::new(mqttoptions, 100);

            // Subscribe to ChirpStack uplink events
            if let Err(e) = client
                .subscribe("application/+/device/+/event/up", QoS::AtMostOnce)
                .await
            {
                eprintln!("[LoRaWAN Monitor] Failed to subscribe: {}", e);
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }

            eprintln!("[LoRaWAN Monitor] Connected to {}:{}, subscribed to uplinks",
                config.chirpstack_mqtt_host, config.chirpstack_mqtt_port);

            let mut last_publish = Instant::now();
            let publish_interval = Duration::from_secs(config.publish_interval_s);
            let timeout_secs = config.sensor_timeout_s;

            // Event loop
            loop {
                if shutdown_flag.load(Ordering::Relaxed) {
                    break;
                }

                // Publish queued downlink commands (#33) and register them for
                // fPort-85 response correlation (#34).
                while let Ok(req) = cmd_rx.try_recv() {
                    match &last_app_id {
                        Some(app_id) => {
                            let down_topic = format!(
                                "application/{}/device/{}/command/down",
                                app_id, req.dev_eui
                            );
                            let body = serde_json::json!({
                                "devEui": req.dev_eui,
                                "confirmed": false,
                                "fPort": 85,
                                "data": BASE64.encode(&req.bytes),
                            })
                            .to_string();
                            match client
                                .publish(&down_topic, QoS::AtMostOnce, false, body.into_bytes())
                                .await
                            {
                                Ok(_) => {
                                    eprintln!("[LoRaWAN Monitor] downlink cmd seq={} -> {}", req.seq, req.dev_eui);
                                    // Newest-wins: a recycled seq must not resolve to a stale
                                    // waiter, so evict any prior registration for this seq in
                                    // both maps first (separate removes, no short-circuit).
                                    let had = pending.remove(&req.seq).is_some();
                                    let had_multi = pending_multi.remove(&req.seq).is_some();
                                    if had || had_multi {
                                        eprintln!(
                                            "[LoRaWAN Monitor] seq={} reused; dropped stale pending registration",
                                            req.seq
                                        );
                                    }
                                    if req.multi {
                                        pending_multi.insert(req.seq, req.resp_tx);
                                    } else {
                                        pending.insert(req.seq, req.resp_tx);
                                    }
                                }
                                Err(e) => eprintln!("[LoRaWAN Monitor] downlink publish failed: {}", e),
                            }
                        }
                        None => eprintln!(
                            "[LoRaWAN Monitor] cannot send cmd seq={}: no application id seen yet",
                            req.seq
                        ),
                    }
                }

                // Publish queued raw downlinks (expert / fire-and-forget): sent
                // verbatim on the requested fPort with no seq allocation and no
                // response correlation, so they never touch the pending maps and
                // cannot disturb the seq-correlated command stream.
                while let Ok(raw) = raw_rx.try_recv() {
                    match &last_app_id {
                        Some(app_id) => {
                            let down_topic = format!(
                                "application/{}/device/{}/command/down",
                                app_id, raw.dev_eui
                            );
                            let body = serde_json::json!({
                                "devEui": raw.dev_eui,
                                "confirmed": false,
                                "fPort": raw.fport,
                                "data": BASE64.encode(&raw.bytes),
                            })
                            .to_string();
                            match client
                                .publish(&down_topic, QoS::AtMostOnce, false, body.into_bytes())
                                .await
                            {
                                Ok(_) => eprintln!(
                                    "[LoRaWAN Monitor] raw downlink fPort={} ({} bytes) -> {}",
                                    raw.fport,
                                    raw.bytes.len(),
                                    raw.dev_eui
                                ),
                                Err(e) => {
                                    eprintln!("[LoRaWAN Monitor] raw downlink publish failed: {}", e)
                                }
                            }
                        }
                        None => eprintln!(
                            "[LoRaWAN Monitor] cannot send raw downlink: no application id seen yet"
                        ),
                    }
                }

                // Deregister multi seqs whose collector has finished (F1) so the
                // re-inserted pending_multi entry cannot alias a recycled seq.
                while let Ok(done_seq) = release_rx.try_recv() {
                    pending_multi.remove(&done_seq);
                }

                // Poll with a timeout so we can check shutdown and publish periodically
                match tokio::time::timeout(Duration::from_secs(1), eventloop.poll()).await {
                    Ok(Ok(Event::Incoming(Incoming::Publish(publish)))) => {
                        let topic = publish.topic.clone();
                        let payload = publish.payload.to_vec();

                        // Learn the application id from the uplink topic
                        // (application/{app_id}/device/{dev_eui}/event/up) so downlinks
                        // can target .../command/down.
                        let topic_parts: Vec<&str> = topic.split('/').collect();
                        if topic_parts.len() >= 2 && topic_parts[0] == "application" {
                            last_app_id = Some(topic_parts[1].to_string());
                        }

                        // fPort 85: command/response (#34) — decode and correlate by seq.
                        if let Some((85, data)) = chirpstack::extract_fport_data(&payload) {
                            let dev_eui =
                                chirpstack::extract_dev_eui_from_topic(&topic).unwrap_or_default();
                            match chirpstack::strip_proto_version(&data, &dev_eui)
                                .and_then(decode_response)
                            {
                                Ok(resp) => {
                                    let seq = resp.seq;
                                    if let Some(tx) = pending.remove(&seq) {
                                        let _ = tx.send(resp);
                                    } else if let Some(tx) = pending_multi.remove(&seq) {
                                        // Paged response (e.g. HistoryFrame): forward this
                                        // page, keeping the registration for further pages
                                        // only while the caller is still listening.
                                        if tx.send(resp).is_ok() {
                                            pending_multi.insert(seq, tx);
                                        }
                                    } else {
                                        eprintln!(
                                            "[LoRaWAN Monitor] fPort-85 response from {} (unmatched seq={}): {:?}",
                                            dev_eui, seq, resp.kind
                                        );
                                    }
                                }
                                Err(e) => eprintln!(
                                    "[LoRaWAN Monitor] fPort-85 decode failed from {}: {}",
                                    dev_eui, e
                                ),
                            }
                        } else {
                        match chirpstack::parse_uplink(&payload) {
                            Ok(Some(reading)) => {
                                eprintln!(
                                    "[LoRaWAN Monitor] Uplink from {} ({}): {} fields, {} counters, rssi={:?}dBm",
                                    reading.device_name,
                                    reading.dev_eui,
                                    reading.fields.len(),
                                    reading.counters.len(),
                                    reading.rssi,
                                );

                                // Save-and-feed: persist every uplink BEFORE live publish
                                // so the firmware DB is the authoritative store for the
                                // sticker stream and downstream destinations can replay
                                // from it via the export drain loop.
                                let now_ts = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_secs() as i64;
                                let message_id = chirpstack::message_id_for(&reading, now_ts);
                                let epoch = storage
                                    .get_provisioning_epoch(reading.dev_eui.clone())
                                    .unwrap_or(1);
                                let payload_json = serde_json::to_string(&serde_json::json!({
                                    "fields":      reading.fields,
                                    "counters":    reading.counters,
                                    "events":      reading.events,
                                    "rssi":        reading.rssi,
                                    "snr":         reading.snr,
                                    "received_at": reading.received_at,
                                    "device_name": reading.device_name,
                                }))
                                .unwrap_or_else(|_| "{}".to_string());
                                let _ = storage.write_sticker_reading(
                                    reading.dev_eui.clone(),
                                    epoch,
                                    now_ts,
                                    now_ts,
                                    message_id,
                                    "uplink".to_string(),
                                    payload_json,
                                );

                                // Auto-backfill (#43): a STICKER buffers its samples
                                // while off the air; when it reappears after an outage,
                                // pull that gap over fPort 85 without operator action.
                                // Read the PREVIOUS last_seen before update_sensor
                                // overwrites it, so the window is [was_last_seen, now].
                                let prev_last_seen = state
                                    .read()
                                    .ok()
                                    .and_then(|s| {
                                        s.sensors
                                            .get(&reading.dev_eui)
                                            .and_then(|se| se.last_seen.clone())
                                    })
                                    .and_then(|ls| {
                                        chrono::DateTime::parse_from_rfc3339(&ls)
                                            .ok()
                                            .map(|dt| dt.timestamp())
                                    });
                                if let Some((from_unix, to_unix)) =
                                    auto_backfill_window(prev_last_seen, now_ts, config.sensor_timeout_s)
                                {
                                    eprintln!(
                                        "[LoRaWAN Monitor] sticker {} back after outage; auto-backfill {}..{}",
                                        reading.dev_eui, from_unix, to_unix
                                    );
                                    spawn_auto_backfill(
                                        self_handle.clone(),
                                        mqtt_tx.clone(),
                                        reading.dev_eui.clone(),
                                        from_unix,
                                        to_unix,
                                    );
                                }

                                if let Ok(mut s) = state.write() {
                                    s.update_sensor(&reading);
                                }
                            }
                            Ok(None) => {
                                // Uplink carried nothing to persist (fPort 85
                                // command/response, unknown fPort, or empty frame).
                            }
                            Err(e) => {
                                eprintln!("[LoRaWAN Monitor] Failed to parse uplink from {}: {}", topic, e);
                            }
                        }
                        } // end fPort-85 vs telemetry dispatch
                    }
                    Ok(Ok(_)) => {
                        // Other MQTT events (ConnAck, SubAck, etc.) - ignore
                    }
                    Ok(Err(e)) => {
                        eprintln!("[LoRaWAN Monitor] Connection error: {}", e);
                        break; // Reconnect
                    }
                    Err(_) => {
                        // Timeout - check timeouts and publish if needed
                    }
                }

                // Check sensor timeouts and evaluate alarms
                let any_sticker_critical = if let Ok(mut s) = state.write() {
                    s.check_timeouts(timeout_secs);
                    if let Ok(cfgs) = configs.read() {
                        s.evaluate_alarms(&cfgs, &field_threshold_defaults);
                    }
                    // Compute "is any sticker in Critical?" while we still hold the lock.
                    s.sensors.values().any(|sensor| {
                        sensor.alarm_state == super::state::LoRaWANAlarmState::Critical
                    })
                } else {
                    // RwLock poisoned — preserve previous decision so we don't fabricate
                    // a spurious transition edge.
                    prev_any_sticker_critical
                };

                // Notify the buzzer priority manager only on transitions to avoid log
                // spam. On NEW critical transitions (off→on), also clear the 30-min
                // button silence so the user hears the new alarm.
                if let Some(ref pm) = buzzer_priority_manager {
                    if any_sticker_critical != prev_any_sticker_critical {
                        if any_sticker_critical {
                            // off → on transition: break button silence (same as sensors do).
                            pm.on_new_sensor_alarm();
                        }
                        pm.set_sticker_critical(any_sticker_critical);
                    }
                }
                prev_any_sticker_critical = any_sticker_critical;

                // Publish sensor data periodically
                if last_publish.elapsed() >= publish_interval {
                    last_publish = Instant::now();
                    publish_lorawan_sensors(&state, &mqtt_tx, &hostname);
                    publish_lorawan_gateways(&mqtt_tx).await;
                }
            }

            // Wait before reconnecting
            eprintln!("[LoRaWAN Monitor] Disconnected, reconnecting in 5s...");
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

/// Publish current LoRaWAN sensor state to the main FIBER MQTT
/// Upper bound on a single auto-backfill history read (same budget as the
/// manual get_sticker_history). A paged HistoryFrame replay can take a while at
/// low data rates, so keep this generous.
const AUTO_BACKFILL_TIMEOUT: Duration = Duration::from_secs(180);

/// Decide the `[from, to]` unix window to auto-backfill when a STICKER uplink
/// arrives. Returns `None` on first contact (no prior `last_seen`) or when the
/// gap since the previous uplink is within normal reporting cadence
/// (`< offline_threshold_s`) — i.e. the device never actually went away, so
/// there is nothing buffered to fetch. A backwards clock also yields `None`.
///
/// Self-guarding by design: `update_sensor` advances `last_seen` to `now` right
/// after this check, so the next uplink sees a small gap and does not re-trigger
/// — one backfill per outage.
fn auto_backfill_window(
    prev_last_seen: Option<i64>,
    now: i64,
    offline_threshold_s: u64,
) -> Option<(u32, u32)> {
    let prev = prev_last_seen?;
    if now <= prev {
        return None;
    }
    if (now - prev) < offline_threshold_s as i64 {
        return None;
    }
    Some((prev.max(0) as u32, now.max(0) as u32))
}

/// Spawn a detached task that pulls a STICKER's buffered history for the outage
/// window and republishes each frame to `lorawan/sensors/<dev_eui>/history` via
/// the MQTT channel. Mirrors the manual `get_sticker_history` path, but is
/// triggered automatically on reconnect. The read runs on a blocking task: it
/// sends its downlink through `handle` (whose cmd_tx feeds the loop's cmd_rx),
/// so running it inline on the monitor thread would deadlock.
fn spawn_auto_backfill(
    handle: LoRaWANHandle,
    mqtt_tx: Sender<MqttMessage>,
    dev_eui: String,
    from_unix: u32,
    to_unix: u32,
) {
    tokio::spawn(async move {
        let dev = dev_eui.clone();
        let result = tokio::task::spawn_blocking(move || {
            super::sticker_config::read_history(
                &handle,
                &dev,
                Some(from_unix),
                Some(to_unix),
                AUTO_BACKFILL_TIMEOUT,
            )
        })
        .await;

        match result {
            Ok(Ok(hr)) => {
                if !hr.complete {
                    eprintln!(
                        "[LoRaWAN Monitor] auto-backfill {} incomplete: missing frames {:?}",
                        dev_eui, hr.missing_indices
                    );
                }
                for page in hr.pages {
                    let records: Vec<serde_json::Value> = page
                        .records
                        .iter()
                        .map(super::sticker_config::history_record_to_json)
                        .collect();
                    let _ = mqtt_tx.try_send(MqttMessage::PublishStickerHistory {
                        dev_eui: dev_eui.clone(),
                        frame_index: page.frame_index,
                        frame_count: page.frame_count,
                        records,
                    });
                }
            }
            Ok(Err(e)) => eprintln!(
                "[LoRaWAN Monitor] auto-backfill {} failed: {}",
                dev_eui, e
            ),
            Err(join_err) => eprintln!(
                "[LoRaWAN Monitor] auto-backfill {} task panicked: {}",
                dev_eui, join_err
            ),
        }
    });
}

#[cfg(test)]
mod auto_backfill_tests {
    use super::auto_backfill_window;

    #[test]
    fn none_on_first_contact() {
        assert_eq!(auto_backfill_window(None, 1000, 300), None);
    }

    #[test]
    fn none_within_cadence() {
        // 100 s gap, 300 s outage threshold -> normal reporting, nothing buffered.
        assert_eq!(auto_backfill_window(Some(900), 1000, 300), None);
    }

    #[test]
    fn window_after_outage() {
        // 4000 s gap >> 300 s threshold -> backfill the whole gap.
        assert_eq!(auto_backfill_window(Some(1000), 5000, 300), Some((1000, 5000)));
    }

    #[test]
    fn none_on_backwards_clock() {
        assert_eq!(auto_backfill_window(Some(5000), 1000, 300), None);
    }

    #[test]
    fn boundary_gap_equal_threshold_triggers() {
        // gap == threshold counts as an outage (>= threshold).
        assert_eq!(auto_backfill_window(Some(1000), 1300, 300), Some((1000, 1300)));
    }
}

fn publish_lorawan_sensors(
    state: &SharedLoRaWANState,
    mqtt_tx: &Sender<MqttMessage>,
    _hostname: &str,
) {
    let state_snapshot = match state.read() {
        Ok(s) => s.clone(),
        Err(_) => return,
    };

    // Reconcile against the sensors declared in the config — anything not in
    // the YAML's `lorawan.sensors[]` is filtered out of the published payload.
    // The effective `field_thresholds` (with defaults merged) live on each
    // `LoRaWANSensorState` after `evaluate_alarms`, so we read them straight
    // from the state instead of re-resolving here.
    let loaded = crate::libs::config::Config::load_default().ok();
    let allowed_dev_euis: Option<std::collections::HashSet<String>> = Some(
        loaded
            .as_ref()
            .and_then(|c| c.lorawan.as_ref().map(|l| {
                l.sensors.iter().map(|s| s.dev_eui.clone()).collect()
            }))
            .unwrap_or_default(),
    );

    let sensors: Vec<crate::libs::mqtt::messages::LoRaWANSensorPayload> = state_snapshot
        .sensors
        .values()
        .filter(|s| allowed_dev_euis.as_ref().map_or(true, |set| set.contains(&s.dev_eui)))
        .map(|s| {
            let field_alarm_states = s.field_alarm_states.iter()
                .map(|(k, v)| (k.clone(), v.to_string()))
                .collect();
            crate::libs::mqtt::messages::LoRaWANSensorPayload {
                dev_eui: s.dev_eui.clone(),
                name: s.name.clone(),
                serial_number: s.serial_number.clone(),
                location: s.location.clone(),
                fields: s.fields.clone(),
                field_alarm_states,
                field_thresholds: s.field_thresholds.clone(),
                counters: s.counters.clone(),
                events: s.recent_events.iter().cloned().collect(),
                rssi: s.rssi,
                snr: s.snr,
                last_seen: s.last_seen.clone(),
                alarm_state: s.alarm_state.to_string(),
            }
        })
        .collect();

    let _ = mqtt_tx.try_send(MqttMessage::PublishLoRaWANSensorData { sensors });
}

/// Publish external LoRaWAN gateway status to the main FIBER MQTT.
///
/// Reads the configured gateways fresh each tick (so a gateway added at runtime
/// is picked up without restarting the monitor) and queries ChirpStack for
/// online state. The ChirpStack query is blocking network I/O, so it runs on a
/// blocking thread to keep the async uplink pump responsive.
async fn publish_lorawan_gateways(mqtt_tx: &Sender<MqttMessage>) {
    let loaded = match crate::libs::config::Config::load_default() {
        Ok(c) => c,
        Err(_) => return,
    };
    let enabled: Vec<crate::libs::config::ExternalGatewayConfig> = match loaded.lorawan.as_ref() {
        Some(l) => l.gateways.iter().filter(|g| g.enabled).cloned().collect(),
        None => return,
    };

    // Always publish (even an empty list) so the viewer can reconcile/clear
    // stale gateway entries — mirrors publish_lorawan_sensors.
    if enabled.is_empty() {
        let _ = mqtt_tx.try_send(MqttMessage::PublishLoRaWANGatewayData { gateways: Vec::new() });
        return;
    }

    let euis: Vec<String> = enabled.iter().map(|g| g.gateway_eui.clone()).collect();
    let status = tokio::task::spawn_blocking(move || {
        crate::libs::lorawan::provisioning::get_gateways_status(&euis)
    })
    .await
    .unwrap_or_default();
    let status_map: std::collections::HashMap<String, crate::libs::lorawan::provisioning::GatewayStatus> =
        status.into_iter().map(|s| (s.gateway_eui.clone(), s)).collect();

    let gateways: Vec<crate::libs::mqtt::messages::LoRaWANGatewayPayload> = enabled
        .iter()
        .map(|g| {
            let st = status_map.get(&g.gateway_eui);
            crate::libs::mqtt::messages::LoRaWANGatewayPayload {
                gateway_eui: g.gateway_eui.clone(),
                name: g.name.clone(),
                online: st.map(|s| s.online).unwrap_or(false),
                last_seen: st.and_then(|s| s.last_seen.clone()),
            }
        })
        .collect();

    let _ = mqtt_tx.try_send(MqttMessage::PublishLoRaWANGatewayData { gateways });
}
