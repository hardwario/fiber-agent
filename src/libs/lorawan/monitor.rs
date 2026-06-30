//! LoRaWAN monitor thread
//!
//! Creates a second MQTT client connected to the local Mosquitto broker
//! (where ChirpStack publishes), subscribes to uplink events, parses them,
//! updates shared LoRaWAN state, and periodically publishes sensor data
//! to the FIBER MQTT topic hierarchy.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam::channel::Sender;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};

use crate::libs::config::LoRaWANConfig;
use crate::libs::mqtt::messages::MqttMessage;
use crate::libs::storage::StorageHandle;

use super::chirpstack;
use super::detector;
use super::state::{SharedLoRaWANState, create_shared_lorawan_state};

/// Handle for sending messages to the LoRaWAN monitor
#[derive(Clone)]
pub struct LoRaWANHandle {
    pub state: SharedLoRaWANState,
}

/// LoRaWAN monitor that bridges ChirpStack MQTT to FIBER MQTT
pub struct LoRaWANMonitor {
    thread_handle: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
    pub state: SharedLoRaWANState,
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

        // `gateway_present` (built-in concentrator presence) drives the display
        // state. Whether the monitor RUNS is a separate decision: it only needs
        // ChirpStack + Mosquitto, both of which run independently of the
        // concentrator. Start it when the concentrator is up, OR ChirpStack is
        // running (so an external gateway added at runtime is handled without a
        // reboot), OR an external gateway is already configured.
        let should_run = gateway_present || detection.chirpstack_running || has_external;

        let state = create_shared_lorawan_state(gateway_present);

        if !should_run {
            eprintln!(
                "[LoRaWAN Monitor] Not starting: concentratord={}, chirpstack={}, external_gateway={}",
                detection.concentratord_running, detection.chirpstack_running, has_external
            );
            return Ok(Self {
                thread_handle: None,
                shutdown_flag: Arc::new(AtomicBool::new(false)),
                state,
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
            );
        });

        eprintln!("[LoRaWAN Monitor] Started");

        Ok(Self {
            thread_handle: Some(thread_handle),
            shutdown_flag,
            state,
        })
    }

    /// Get a handle for reading LoRaWAN state
    pub fn handle(&self) -> LoRaWANHandle {
        LoRaWANHandle {
            state: self.state.clone(),
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

                        match chirpstack::parse_uplink(&payload) {
                            Ok(reading) => {
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
                                // Slim payload: omit any null/empty fields so
                                // sticker_readings.payload_json stays as tight
                                // as possible (this row is kept for 30 days at
                                // ~1 uplink/min/sticker; trimming defaults
                                // saves ~25-30% over the previous version
                                // that always wrote `"rssi": null` etc.).
                                // device_name is dropped entirely — it's
                                // redundant with the sticker's own dev_eui
                                // and the YAML config holds the
                                // operator-set display name.
                                let mut obj = serde_json::Map::new();
                                if !reading.fields.is_empty() {
                                    obj.insert("fields".into(), serde_json::json!(reading.fields));
                                }
                                if !reading.counters.is_empty() {
                                    obj.insert("counters".into(), serde_json::json!(reading.counters));
                                }
                                if !reading.events.is_empty() {
                                    let evs: Vec<String> = reading.events.iter()
                                        .map(|e| e.event_type.clone())
                                        .collect();
                                    obj.insert("events".into(), serde_json::json!(evs));
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
                                    now_ts,
                                    now_ts,
                                    message_id,
                                    "uplink".to_string(),
                                    payload_json,
                                );

                                if let Ok(mut s) = state.write() {
                                    s.update_sensor(&reading);
                                }
                            }
                            Err(e) => {
                                eprintln!("[LoRaWAN Monitor] Failed to parse uplink from {}: {}", topic, e);
                            }
                        }
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
                    publish_lorawan_gateways(&mqtt_tx).await;
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
