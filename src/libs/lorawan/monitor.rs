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
}

/// Handle for sending messages to the LoRaWAN monitor
#[derive(Clone)]
pub struct LoRaWANHandle {
    pub state: SharedLoRaWANState,
    cmd_tx: Sender<CommandRequest>,
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
            })
            .map_err(|_| "LoRaWAN monitor not running".to_string())?;
        resp_rx
            .recv_timeout(timeout)
            .map_err(|_| format!("no fPort-85 response for seq {seq} within {timeout:?}"))
    }
}

/// LoRaWAN monitor that bridges ChirpStack MQTT to FIBER MQTT
pub struct LoRaWANMonitor {
    thread_handle: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
    pub state: SharedLoRaWANState,
    cmd_tx: Sender<CommandRequest>,
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
        // Detect gateway hardware
        let detection = detector::detect_gateway();
        let gateway_present = detection.is_present();

        let state = create_shared_lorawan_state(gateway_present);

        let (cmd_tx, cmd_rx) = unbounded::<CommandRequest>();
        let seq = Arc::new(AtomicU32::new(0));

        if !gateway_present {
            eprintln!("[LoRaWAN Monitor] No gateway detected, monitor will not start");
            // cmd_rx dropped → send_command returns "monitor not running".
            return Ok(Self {
                thread_handle: None,
                shutdown_flag: Arc::new(AtomicBool::new(false)),
                state,
                cmd_tx,
                seq,
            });
        }

        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_flag_clone = shutdown_flag.clone();
        let state_clone = state.clone();

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
            );
        });

        eprintln!("[LoRaWAN Monitor] Started");

        Ok(Self {
            thread_handle: Some(thread_handle),
            shutdown_flag,
            state,
            cmd_tx,
            seq,
        })
    }

    /// Get a handle for reading LoRaWAN state and sending downlink commands.
    pub fn handle(&self) -> LoRaWANHandle {
        LoRaWANHandle {
            state: self.state.clone(),
            cmd_tx: self.cmd_tx.clone(),
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

        // fPort-85 command/response correlation (#34): (dev_eui, seq) ->
        // waiting sender. The key is per-device so a malicious or buggy
        // device on the same tenant cannot deliver a forged Response with
        // the matching seq to the waiter of another device — the seq counter
        // wraps every 250, so cross-device collisions are trivial otherwise.
        // Persists across MQTT reconnects. last_app_id is learned from uplink
        // topics so downlinks can target application/{app_id}/device/.../command/down.
        let mut pending: HashMap<(String, u32), Sender<DecodedResponse>> = HashMap::new();
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

            // Dedicated event pump. `EventLoop::poll()` is NOT cancel-safe —
            // dropping the future mid-read loses bytes from the in-progress
            // MQTT frame and corrupts the eventloop's state. We previously
            // wrapped poll() in `tokio::time::timeout(1s, ...)` to also do
            // periodic work, which silently dropped uplink frames whenever
            // the timeout fired mid-frame. Now: a dedicated task owns the
            // eventloop, never cancels poll(), and feeds events through a
            // bounded mpsc to the periodic-work loop. mpsc::Receiver::recv
            // IS cancel-safe so the outer select! below is safe.
            let (event_tx, mut event_rx) =
                tokio::sync::mpsc::channel::<Result<Event, rumqttc::ConnectionError>>(64);
            let pump_handle = tokio::spawn(async move {
                loop {
                    match eventloop.poll().await {
                        Ok(ev) => {
                            if event_tx.send(Ok(ev)).await.is_err() {
                                break; // receiver dropped (outer reconnect)
                            }
                        }
                        Err(e) => {
                            let _ = event_tx.send(Err(e)).await;
                            break;
                        }
                    }
                }
            });

            // Event loop
            loop {
                if shutdown_flag.load(Ordering::Relaxed) {
                    break;
                }

                // Publish queued downlink commands (#33) and register them for
                // fPort-85 response correlation (#34). Done at the top of every
                // iteration so commands don't wait for the next uplink to drain.
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
                                    pending.insert((req.dev_eui.clone(), req.seq), req.resp_tx);
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

                // Select between events from the pump and a 1-second tick
                // for periodic work. event_rx.recv() is cancel-safe; sleep
                // is cancel-safe; the eventloop itself never gets cancelled.
                let polled = tokio::select! {
                    biased;
                    ev = event_rx.recv() => Some(ev),
                    _ = tokio::time::sleep(Duration::from_secs(1)) => None,
                };

                match polled {
                    Some(Some(Ok(Event::Incoming(Incoming::Publish(publish))))) => {
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
                                Ok(resp) => match pending.remove(&(dev_eui.clone(), resp.seq)) {
                                    Some(tx) => {
                                        let _ = tx.send(resp);
                                    }
                                    None => eprintln!(
                                        "[LoRaWAN Monitor] fPort-85 response from {} (unmatched seq={}): {:?}",
                                        dev_eui, resp.seq, resp.kind
                                    ),
                                },
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
                                // `ts` is the device-reported event time (ChirpStack
                                // `time`, RFC3339); `received_at` is our ingest time.
                                // Falling back to `now_ts` when the upstream time is
                                // missing or malformed keeps a usable timestamp on
                                // the row instead of dropping the uplink, but the
                                // message_id is anchored on the device time so
                                // redeliveries dedup deterministically.
                                let device_ts =
                                    chirpstack::received_at_epoch(&reading).unwrap_or(now_ts);
                                let message_id = chirpstack::message_id_for(&reading);
                                let epoch = storage
                                    .get_provisioning_epoch(reading.dev_eui.clone())
                                    .unwrap_or(1);
                                // Slim payload: omit any null/empty fields so
                                // sticker_readings.payload_json stays as tight
                                // as possible (this row is kept for 30 days at
                                // ~1 uplink/min/sticker; trimming defaults
                                // saves ~25-30% over the always-write-nulls
                                // version). device_name is dropped — it's
                                // redundant with dev_eui and the YAML config
                                // holds the operator-set display name. The
                                // events array is serialized in full (type +
                                // extra) so AlarmReport / Telemetry detail
                                // (value/source/slot/edge/side/rel_s/base_time)
                                // survives the save-and-feed replay.
                                let mut obj = serde_json::Map::new();
                                if !reading.fields.is_empty() {
                                    obj.insert("fields".into(), serde_json::json!(reading.fields));
                                }
                                if !reading.counters.is_empty() {
                                    obj.insert("counters".into(), serde_json::json!(reading.counters));
                                }
                                if !reading.events.is_empty() {
                                    obj.insert("events".into(), serde_json::json!(reading.events));
                                }
                                if let Some(r) = reading.rssi {
                                    obj.insert("rssi".into(), serde_json::json!(r));
                                }
                                if let Some(s) = reading.snr {
                                    obj.insert("snr".into(), serde_json::json!(s));
                                }
                                if !reading.received_at.is_empty() {
                                    obj.insert("received_at".into(), serde_json::json!(reading.received_at));
                                }
                                let payload_json = serde_json::to_string(&serde_json::Value::Object(obj))
                                    .unwrap_or_else(|_| "{}".to_string());
                                let _ = storage.write_sticker_reading(
                                    reading.dev_eui.clone(),
                                    epoch,
                                    device_ts,
                                    now_ts,
                                    message_id,
                                    "uplink".to_string(),
                                    payload_json,
                                );

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
                    Some(Some(Ok(_))) => {
                        // Other MQTT events (ConnAck, SubAck, etc.) - ignore
                    }
                    Some(Some(Err(e))) => {
                        eprintln!("[LoRaWAN Monitor] Connection error: {}", e);
                        break; // Reconnect
                    }
                    Some(None) => {
                        // Pump task exited (channel closed) — treat as
                        // disconnect and reconnect.
                        eprintln!("[LoRaWAN Monitor] Event pump closed unexpectedly");
                        break;
                    }
                    None => {
                        // 1-second tick — check timeouts and publish if needed
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
                }
            }

            // Drop the receiver so the pump task notices and exits cleanly.
            drop(event_rx);
            let _ = pump_handle.await;

            // Wait before reconnecting
            eprintln!("[LoRaWAN Monitor] Disconnected, reconnecting in 5s...");
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

/// Publish current LoRaWAN sensor state to the main FIBER MQTT
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
