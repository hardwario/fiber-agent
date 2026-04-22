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
        mqtt_tx: Sender<MqttMessage>,
        hostname: String,
    ) -> io::Result<Self> {
        // Detect gateway hardware
        let detection = detector::detect_gateway();
        let gateway_present = detection.is_present();

        let state = create_shared_lorawan_state(gateway_present);

        if !gateway_present {
            eprintln!("[LoRaWAN Monitor] No gateway detected, monitor will not start");
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
                mqtt_tx,
                hostname,
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
    mqtt_tx: Sender<MqttMessage>,
    hostname: String,
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

                // Poll with a timeout so we can check shutdown and publish periodically
                match tokio::time::timeout(Duration::from_secs(1), eventloop.poll()).await {
                    Ok(Ok(Event::Incoming(Incoming::Publish(publish)))) => {
                        let topic = publish.topic.clone();
                        let payload = publish.payload.to_vec();

                        match chirpstack::parse_uplink(&payload) {
                            Ok(reading) => {
                                eprintln!(
                                    "[LoRaWAN Monitor] Uplink from {} ({}): temp={:?}°C hum={:?}% rssi={:?}dBm",
                                    reading.device_name,
                                    reading.dev_eui,
                                    reading.temperature,
                                    reading.humidity,
                                    reading.rssi,
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
                if let Ok(mut s) = state.write() {
                    s.check_timeouts(timeout_secs);
                    s.evaluate_alarms(&config.sensors);
                }

                // Publish sensor data periodically
                if last_publish.elapsed() >= publish_interval {
                    last_publish = Instant::now();
                    publish_lorawan_sensors(&state, &mqtt_tx, &hostname);
                }
            }

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

    if state_snapshot.sensors.is_empty() {
        return;
    }

    let sensors: Vec<crate::libs::mqtt::messages::LoRaWANSensorPayload> = state_snapshot
        .sensors
        .values()
        .map(|s| crate::libs::mqtt::messages::LoRaWANSensorPayload {
            dev_eui: s.dev_eui.clone(),
            name: s.name.clone(),
            serial_number: s.serial_number.clone(),
            temperature: s.temperature,
            humidity: s.humidity,
            voltage: s.voltage,
            ext_temperature_1: s.ext_temperature_1,
            ext_temperature_2: s.ext_temperature_2,
            illuminance: s.illuminance,
            motion_count: s.motion_count,
            orientation: s.orientation,
            rssi: s.rssi,
            snr: s.snr,
            last_seen: s.last_seen.clone(),
            alarm_state: s.alarm_state.to_string(),
            temp_alarm_state: s.temp_alarm_state.to_string(),
            humidity_alarm_state: s.humidity_alarm_state.to_string(),
        })
        .collect();

    let _ = mqtt_tx.try_send(MqttMessage::PublishLoRaWANSensorData { sensors });
}
