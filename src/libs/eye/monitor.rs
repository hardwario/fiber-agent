//! EYE BLE tag monitor thread.
//!
//! Owns a dedicated BlueZ session (independent of the GATT-server `BleMonitor`),
//! runs an active scan, parses the advertising of configured tags, auto-provisions
//! a tag on first sight, and publishes a snapshot to MQTT — mirroring the
//! structure of the `lorawan` monitor.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam::channel::Sender;

use crate::libs::eye::config::EyeConfig;
use crate::libs::mqtt::messages::{EyeTagPayload, MqttMessage};

use super::advertising::{parse_manufacturer_value, TELTONIKA_COMPANY_ID};
use super::provisioning::{provision, EyeProfile};
use super::state::{create_shared_eye_state, ProvisioningStatus, SharedEyeState};

/// Max consecutive auto-provision attempts before giving up (avoids tripping
/// the tag's anti-bruteforce lockout).
const MAX_PROVISION_ATTEMPTS: u32 = 3;

/// Read-only handle to the EYE monitor state.
#[derive(Clone)]
pub struct EyeHandle {
    pub state: SharedEyeState,
}

/// EYE BLE tag monitor.
pub struct EyeMonitor {
    thread_handle: Option<JoinHandle<()>>,
    shutdown_flag: Arc<AtomicBool>,
    pub state: SharedEyeState,
}

impl EyeMonitor {
    /// Create and spawn the EYE monitor. Inert (no thread) when `config.enabled`
    /// is false.
    pub fn new(
        config: EyeConfig,
        mqtt_tx: Sender<MqttMessage>,
        hostname: String,
    ) -> io::Result<Self> {
        let state = create_shared_eye_state(false);

        if !config.enabled {
            eprintln!("[EYE Monitor] Disabled in config");
            return Ok(Self {
                thread_handle: None,
                shutdown_flag: Arc::new(AtomicBool::new(false)),
                state,
            });
        }

        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown_flag.clone();
        let state_clone = state.clone();

        let thread_handle = thread::spawn(move || {
            eye_loop(shutdown_clone, state_clone, config, mqtt_tx, hostname);
        });

        eprintln!("[EYE Monitor] Started");

        Ok(Self {
            thread_handle: Some(thread_handle),
            shutdown_flag,
            state,
        })
    }

    pub fn handle(&self) -> EyeHandle {
        EyeHandle {
            state: self.state.clone(),
        }
    }
}

impl Drop for EyeMonitor {
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

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn eye_loop(
    shutdown: Arc<AtomicBool>,
    state: SharedEyeState,
    config: EyeConfig,
    mqtt_tx: Sender<MqttMessage>,
    _hostname: String,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[EYE Monitor] Failed to create tokio runtime: {e}");
            return;
        }
    };

    // Pre-seed configured tags into the shared state so the UI shows them as
    // "pending" before the first advertisement arrives.
    if let Ok(mut s) = state.write() {
        for tag in config.tags.iter().filter(|t| t.enabled) {
            s.entry(&tag.mac.to_uppercase(), tag.name.clone());
        }
    }

    rt.block_on(async {
        let publish_interval = Duration::from_secs(config.publish_interval_s.max(1));
        let mut last_publish = Instant::now();

        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            // (Re)establish a BlueZ session + adapter and start an active scan.
            let session = match bluer::Session::new().await {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[EYE Monitor] No BlueZ session: {e}; retrying in 10s");
                    if let Ok(mut s) = state.write() {
                        s.adapter_present = false;
                    }
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    continue;
                }
            };
            let adapter = match session.default_adapter().await {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("[EYE Monitor] No default adapter: {e}; retrying in 10s");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    continue;
                }
            };
            let _ = adapter.set_powered(true).await;
            // Active scan, deliver duplicate advertisements so unchanged
            // manufacturer data keeps being reported.
            let filter = bluer::DiscoveryFilter {
                transport: bluer::DiscoveryTransport::Le,
                duplicate_data: true,
                ..Default::default()
            };
            let _ = adapter.set_discovery_filter(filter).await;
            let _discovery = match adapter.discover_devices().await {
                Ok(d) => d, // held alive → keeps discovery running
                Err(e) => {
                    eprintln!("[EYE Monitor] Failed to start discovery: {e}; retrying in 10s");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    continue;
                }
            };

            if let Ok(mut s) = state.write() {
                s.adapter_present = true;
            }
            eprintln!(
                "[EYE Monitor] Scanning for {} configured tag(s) on {}",
                config.tags.iter().filter(|t| t.enabled).count(),
                adapter.name()
            );

            // Inner poll loop.
            loop {
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }

                let now_ts = now_secs();

                for tag in config.tags.iter().filter(|t| t.enabled) {
                    let mac_key = tag.mac.to_uppercase();
                    let addr: bluer::Address = match mac_key.parse() {
                        Ok(a) => a,
                        Err(_) => {
                            eprintln!("[EYE Monitor] Invalid MAC in config: {}", tag.mac);
                            continue;
                        }
                    };
                    let device = match adapter.device(addr) {
                        Ok(d) => d,
                        Err(_) => continue,
                    };

                    // Read & parse the latest advertising manufacturer data.
                    let md = device.manufacturer_data().await.ok().flatten();
                    if let Some(value) = md.as_ref().and_then(|m| m.get(&TELTONIKA_COMPANY_ID)) {
                        match parse_manufacturer_value(value) {
                            Ok(reading) => {
                                let rssi = device.rssi().await.ok().flatten();
                                if let Ok(mut s) = state.write() {
                                    let entry = s.entry(&mac_key, tag.name.clone());
                                    entry.apply_reading(&reading, rssi, now_ts);
                                }
                            }
                            Err(e) => {
                                eprintln!("[EYE Monitor] Parse error for {mac_key}: {e}");
                            }
                        }
                    }

                    // Auto-provision on first sight.
                    if config.auto_provision {
                        let should_provision = state
                            .read()
                            .ok()
                            .and_then(|s| s.tags.get(&mac_key).map(|t| {
                                t.last_seen_ts.is_some()
                                    && t.provisioning == ProvisioningStatus::PendingProvisioning
                                    && t.provision_attempts < MAX_PROVISION_ATTEMPTS
                            }))
                            .unwrap_or(false);

                        if should_provision {
                            if let Ok(mut s) = state.write() {
                                if let Some(t) = s.tags.get_mut(&mac_key) {
                                    t.provisioning = ProvisioningStatus::Provisioning;
                                }
                            }
                            eprintln!("[EYE Monitor] Provisioning {mac_key} (first sight)...");
                            let result = provision(&device, &EyeProfile::default()).await;
                            let _ = device.disconnect().await;
                            if let Ok(mut s) = state.write() {
                                if let Some(t) = s.tags.get_mut(&mac_key) {
                                    match result {
                                        Ok(()) => {
                                            t.provisioning = ProvisioningStatus::Provisioned;
                                            eprintln!("[EYE Monitor] Provisioned {mac_key}");
                                        }
                                        Err(ref e) => {
                                            t.provision_attempts += 1;
                                            t.provisioning = if t.provision_attempts
                                                >= MAX_PROVISION_ATTEMPTS
                                            {
                                                ProvisioningStatus::Failed
                                            } else {
                                                ProvisioningStatus::PendingProvisioning
                                            };
                                            eprintln!(
                                                "[EYE Monitor] Provisioning {mac_key} failed (attempt {}): {e}",
                                                t.provision_attempts
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Publish snapshot periodically.
                if last_publish.elapsed() >= publish_interval {
                    last_publish = Instant::now();
                    publish_snapshot(&state, &mqtt_tx);
                }

                // Recreate the session occasionally? No — just keep polling.
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    });
}

/// Build the payload from current state and hand it to the MQTT publisher.
fn publish_snapshot(state: &SharedEyeState, mqtt_tx: &Sender<MqttMessage>) {
    let snapshot = match state.read() {
        Ok(s) => s,
        Err(_) => return,
    };
    let tags: Vec<EyeTagPayload> = snapshot
        .tags
        .values()
        .map(|t| EyeTagPayload {
            mac: t.mac.clone(),
            name: t.name.clone(),
            temperature_c: t.temperature_c,
            humidity_pct: t.humidity_pct,
            battery_mv: t.battery_mv,
            low_battery: t.low_battery,
            magnet_present: t.magnet_present,
            magnet_detected: t.magnet_detected,
            moving: t.moving,
            movement_count: t.movement_count,
            pitch_deg: t.pitch_deg,
            roll_deg: t.roll_deg,
            rssi: t.rssi,
            last_seen_ts: t.last_seen_ts,
            provisioning: t.provisioning.as_str().to_string(),
        })
        .collect();
    if tags.is_empty() {
        return;
    }
    let _ = mqtt_tx.try_send(MqttMessage::PublishEyeSensorData { tags });
}
