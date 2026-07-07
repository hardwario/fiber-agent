//! EYE BLE tag monitor thread.
//!
//! Owns a dedicated BlueZ session (independent of the GATT-server `BleMonitor`),
//! runs an active scan, parses the advertising of configured tags, auto-provisions
//! a tag on first sight, and publishes a snapshot to MQTT — mirroring the
//! structure of the `lorawan` monitor.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use std::collections::{HashMap, HashSet};

use crossbeam::channel::Sender;
use futures::{FutureExt, StreamExt};

use crate::libs::eye::config::EyeConfig;
use crate::libs::mqtt::messages::{EyeTagPayload, MqttMessage};
use crate::libs::storage::db::Database;
use crate::libs::storage::{StorageHandle, StorageReader};

use super::advertising::{parse_manufacturer_value, EyeReading, TELTONIKA_COMPANY_ID};
use super::en12830;
use super::provisioning::{provision, EyeProfile, ProvisionError};
use super::state::{
    create_shared_eye_state, register_eye_config, register_eye_state, ProvisioningStatus,
    SharedEyeConfig, SharedEyeState,
};

/// Max consecutive auto-provision attempts before giving up (avoids tripping
/// the tag's anti-bruteforce lockout).
const MAX_PROVISION_ATTEMPTS: u32 = 3;

/// A pending EN12830 recorder operation, run at the top of the outer loop while
/// the BlueZ scan is stopped (raw L2CAP and an active scan must not overlap).
enum EyeJob {
    /// Sync clock + start recording at `interval_s` (after provisioning).
    EnableRecording { interval_s: u16 },
    /// Back-fill archived samples with `ts >= since_ts`, then restart recording.
    Download { since_ts: i64, interval_s: u16 },
    /// Probe the recorder characteristics to determine `is_en12830` without
    /// changing recording state.
    Detect,
    /// Stop the tag's on-tag recording (set_eye_recording with interval 0).
    StopRecording,
}

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
        storage: StorageHandle,
        db_path: String,
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

        // Expose the state so the MQTT command handler can enqueue commands.
        register_eye_state(state.clone());

        // Expose the config so add/remove command handlers can mutate the tag set
        // the scan loop reads (the loop re-reads this each poll cycle).
        let shared_config: SharedEyeConfig = Arc::new(RwLock::new(config));
        register_eye_config(shared_config.clone());
        let config_clone = shared_config.clone();

        let thread_handle = thread::spawn(move || {
            eye_loop(shutdown_clone, state_clone, config_clone, mqtt_tx, hostname, storage, db_path);
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
    shared_config: SharedEyeConfig,
    mqtt_tx: Sender<MqttMessage>,
    _hostname: String,
    storage: StorageHandle,
    db_path: String,
) {
    // Live view of the config; re-read from the shared handle each poll cycle so
    // add/remove_eye_tag take effect without restarting the monitor.
    let mut config = shared_config.read().map(|g| g.clone()).unwrap_or_default();

    // Last raw manufacturer payload persisted per MAC — so we only write a new
    // DB row (save-and-feed) when the advertised data actually changes, instead
    // of once per 1 s poll.
    let mut last_persisted: HashMap<String, Vec<u8>> = HashMap::new();
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
    // "pending" before the first advertisement arrives. Also resume the archive
    // cursor (last stored recording ts) from the DB so a download after a FIBER
    // restart fetches only new samples instead of the whole history.
    let seed_archived: HashMap<String, i64> = {
        let mut m = HashMap::new();
        if let Ok(db) = Database::new(&db_path, 1) {
            if let Ok(conn) = db.connect() {
                for tag in config.tags.iter().filter(|t| t.enabled) {
                    let mac_key = tag.mac.to_uppercase();
                    if let Ok(Some(ts)) = StorageReader::max_eye_reading_ts(&conn, &mac_key) {
                        m.insert(mac_key, ts);
                    }
                }
            }
        }
        m
    };
    if let Ok(mut s) = state.write() {
        for tag in config.tags.iter().filter(|t| t.enabled) {
            let mac_key = tag.mac.to_uppercase();
            let entry = s.entry(&mac_key, tag.name.clone());
            entry.last_archived_ts = seed_archived.get(&mac_key).copied();
        }
    }

    rt.block_on(async {
        let publish_interval = Duration::from_secs(config.publish_interval_s.max(1));
        let mut last_publish = Instant::now();
        // EN12830 recorder jobs queued by the inner poll loop; drained here at the
        // top of the outer loop while no scan is running.
        let mut pending: HashMap<String, EyeJob> = HashMap::new();

        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            // --- Run queued recorder jobs while the BlueZ scan is stopped. Raw
            // L2CAP (recorder) and an active LE scan must not overlap on the same
            // adapter, so this deliberately runs before discovery is (re)started. ---
            if !pending.is_empty() {
                let jobs: Vec<(String, EyeJob)> = pending.drain().collect();
                let sync_fallback_secs = config.sync_fallback_hours as i64 * 3600;
                for (mac, job) in jobs {
                    run_recorder_job(&mac, job, &state, &storage, sync_fallback_secs, &mqtt_tx).await;
                }
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
            // `_with_changes` also yields a `DeviceAdded(addr)` event each time
            // a device's properties change (e.g. a fresh advertisement updates
            // ManufacturerData), not just on first discovery. We use that as
            // the sole signal that a tag's data is genuinely fresh — see the
            // `fresh_macs` drain below. Note: it also replays one `DeviceAdded`
            // per already-known device the instant the stream is created, so
            // the very first poll tick after a (re)connect may treat a tag as
            // "fresh" even if its cached BlueZ data is actually old; this is a
            // harmless, one-time-per-session-restart edge case.
            let mut discovery = match adapter.discover_devices_with_changes().await {
                Ok(d) => d,
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

                // Re-read the live config so a tag added/removed via an MQTT
                // command is picked up by the scan below within one poll cycle.
                if let Ok(g) = shared_config.read() {
                    config = g.clone();
                }

                let now_ts = now_secs();

                // Non-blocking drain: collect every tag whose BlueZ device
                // object genuinely changed (i.e. a fresh advertisement was
                // received) since the last tick. BlueZ keeps serving the last
                // cached ManufacturerData/RSSI for a device indefinitely even
                // after it stops transmitting, so polling `manufacturer_data()`
                // unconditionally would make a dead tag look perpetually live —
                // only a `PropertiesChanged`-driven event tells us the data is
                // actually new.
                let mut fresh_macs: HashSet<String> = HashSet::new();
                while let Some(Some(event)) = discovery.next().now_or_never() {
                    if let bluer::AdapterEvent::DeviceAdded(addr) = event {
                        fresh_macs.insert(addr.to_string());
                    }
                }

                // Drain externally-queued commands (from the MQTT handler) into
                // recorder jobs, which the outer loop runs with the scan paused.
                let external: Vec<super::state::EyeCommand> = state
                    .write()
                    .ok()
                    .map(|mut s| std::mem::take(&mut s.command_queue))
                    .unwrap_or_default();
                for cmd in external {
                    match cmd {
                        super::state::EyeCommand::SetRecording { mac, interval_min } => {
                            let job = if interval_min == 0 {
                                // interval 0 = turn recording off
                                EyeJob::StopRecording
                            } else {
                                let interval_s = match interval_min {
                                    1 => 60,
                                    15 => 900,
                                    _ => 300,
                                };
                                EyeJob::EnableRecording { interval_s }
                            };
                            pending.insert(mac.to_uppercase(), job);
                        }
                        super::state::EyeCommand::DownloadHistory { mac } => {
                            let mac_key = mac.to_uppercase();
                            let interval_s = config
                                .tags
                                .iter()
                                .find(|t| t.mac.to_uppercase() == mac_key)
                                .map(|t| config.interval_min_for(t) as u16 * 60)
                                .unwrap_or(300);
                            let since = state
                                .read()
                                .ok()
                                .and_then(|s| s.tags.get(&mac_key).and_then(|t| t.last_archived_ts))
                                .unwrap_or(0);
                            pending.insert(
                                mac_key,
                                EyeJob::Download { since_ts: since, interval_s },
                            );
                        }
                        super::state::EyeCommand::Detect { mac } => {
                            pending.insert(mac.to_uppercase(), EyeJob::Detect);
                        }
                    }
                }
                if !pending.is_empty() {
                    break;
                }

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

                    // Read & parse the latest advertising manufacturer data,
                    // but only when we just observed a genuine change for this
                    // tag — otherwise we'd be re-reading (and re-timestamping)
                    // BlueZ's stale cached frame every second forever.
                    if fresh_macs.contains(&mac_key) {
                        let md = device.manufacturer_data().await.ok().flatten();
                        if let Some(value) = md.as_ref().and_then(|m| m.get(&TELTONIKA_COMPANY_ID)) {
                            match parse_manufacturer_value(value) {
                                Ok(reading) => {
                                    let rssi = device.rssi().await.ok().flatten();
                                    // Gap detection: was the tag absent longer than 5×
                                    // the logging interval before this frame? If so it
                                    // was out of BLE range and its archive may hold
                                    // samples we missed. Also evaluate the fallback.
                                    let interval_s = config.interval_min_for(tag) as i64 * 60;
                                    if let Ok(mut s) = state.write() {
                                        let entry = s.entry(&mac_key, tag.name.clone());
                                        let prev_seen = entry.last_seen_ts;
                                        entry.apply_reading(&reading, rssi, now_ts);
                                        if config.recording_on_for(tag)
                                            && entry.is_en12830 != Some(false)
                                        {
                                            let gap = prev_seen.map_or(false, |p| {
                                                now_ts.saturating_sub(p) > 5 * interval_s
                                            });
                                            let fallback_due = now_ts
                                                .saturating_sub(entry.last_download_ts.unwrap_or(0))
                                                > config.sync_fallback_hours as i64 * 3600;
                                            // Rate-limit: at most one download per interval.
                                            let rate_ok = now_ts
                                                .saturating_sub(entry.last_download_ts.unwrap_or(0))
                                                >= interval_s;
                                            if (gap || fallback_due) && rate_ok {
                                                let since = entry.last_archived_ts.unwrap_or(0);
                                                entry.last_download_ts = Some(now_ts); // optimistic
                                                pending.insert(
                                                    mac_key.clone(),
                                                    EyeJob::Download {
                                                        since_ts: since,
                                                        interval_s: interval_s as u16,
                                                    },
                                                );
                                            }
                                        }
                                    }
                                    // Save-and-feed: persist only when the advertised
                                    // payload actually changed (the poll re-reads the
                                    // same cached frame every second otherwise). The
                                    // message_id is derived from the payload bytes so
                                    // that INSERT OR IGNORE also dedupes across FIBER
                                    // restarts (last_persisted is in-memory only).
                                    if last_persisted.get(&mac_key).map(Vec::as_slice)
                                        != Some(value.as_slice())
                                    {
                                        last_persisted.insert(mac_key.clone(), value.clone());
                                        let mut hasher =
                                            std::collections::hash_map::DefaultHasher::new();
                                        std::hash::Hash::hash_slice(value.as_slice(), &mut hasher);
                                        let value_hash =
                                            std::hash::Hasher::finish(&hasher);
                                        let message_id =
                                            format!("{}-{:016x}", mac_key, value_hash);
                                        let _ = storage.write_eye_reading(
                                            mac_key.clone(),
                                            now_ts,
                                            now_ts,
                                            message_id,
                                            "advertising".to_string(),
                                            reading_payload_json(&reading, rssi),
                                        );
                                    }
                                }
                                Err(e) => {
                                    eprintln!("[EYE Monitor] Parse error for {mac_key}: {e}");
                                }
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
                            // Bound the whole provisioning session so a stuck
                            // connect()/services() cannot freeze the single-thread
                            // runtime (scan + command queue) indefinitely.
                            let result = match tokio::time::timeout(
                                crate::libs::eye::provisioning::SERVICE_RESOLVE_TIMEOUT,
                                provision(&device, &EyeProfile::default()),
                            )
                            .await
                            {
                                Ok(r) => r,
                                Err(_) => Err(ProvisionError::Timeout),
                            };
                            let _ = device.disconnect().await;
                            if let Ok(mut s) = state.write() {
                                if let Some(t) = s.tags.get_mut(&mac_key) {
                                    match result {
                                        Ok(()) => {
                                            t.provisioning = ProvisioningStatus::Provisioned;
                                            eprintln!("[EYE Monitor] Provisioned {mac_key}");
                                            // Auto-enable the temperature archive.
                                            if config.recording_on_for(tag) {
                                                pending.insert(
                                                    mac_key.clone(),
                                                    EyeJob::EnableRecording {
                                                        interval_s: config
                                                            .interval_min_for(tag)
                                                            as u16
                                                            * 60,
                                                    },
                                                );
                                            }
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

                    // Fallback archive sync — independent of a fresh advertising
                    // frame (manufacturer_data may be absent right after recorder
                    // ops or a scan restart). Triggers when the tag is currently in
                    // range and the archive hasn't synced within sync_fallback_hours
                    // (or ever). The gap-driven path above handles quick catch-up
                    // when advertising flows; this guarantees eventual sync.
                    if config.recording_on_for(tag) {
                        let interval_s = config.interval_min_for(tag) as i64 * 60;
                        let due_job = state.read().ok().and_then(|s| {
                            s.tags.get(&mac_key).and_then(|t| {
                                let in_range = t
                                    .last_seen_ts
                                    .map_or(false, |ls| now_ts.saturating_sub(ls) <= config.tag_timeout_s);
                                let last_dl = t.last_download_ts.unwrap_or(0);
                                let due = now_ts.saturating_sub(last_dl)
                                    > config.sync_fallback_hours as i64 * 3600;
                                let rate_ok =
                                    now_ts.saturating_sub(last_dl) >= interval_s.max(60);
                                if t.is_en12830 != Some(false) && in_range && due && rate_ok {
                                    Some(t.last_archived_ts.unwrap_or(0))
                                } else {
                                    None
                                }
                            })
                        });
                        if let Some(since) = due_job {
                            if let Ok(mut s) = state.write() {
                                if let Some(t) = s.tags.get_mut(&mac_key) {
                                    t.last_download_ts = Some(now_ts);
                                }
                            }
                            pending.insert(
                                mac_key.clone(),
                                EyeJob::Download { since_ts: since, interval_s: interval_s as u16 },
                            );
                        }
                    }
                }

                // Publish snapshot periodically.
                if last_publish.elapsed() >= publish_interval {
                    last_publish = Instant::now();
                    let configured: HashSet<String> =
                        config.tags.iter().map(|t| t.mac.to_uppercase()).collect();
                    publish_snapshot(&state, &mqtt_tx, now_ts, config.tag_timeout_s, &configured);
                }

                // A recorder job was queued: leave the inner loop so the outer
                // loop drops the discovery guard (stops the scan) and runs it.
                if !pending.is_empty() {
                    break;
                }

                // Recreate the session occasionally? No — just keep polling.
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    });
}

/// Run one queued EN12830 recorder job over a raw L2CAP connection (blocking, so
/// dispatched to a blocking thread). Must be called only while the BlueZ scan is
/// stopped. Updates per-tag state and persists downloaded samples (dedup via the
/// `{mac}-rec-{ts}` message_id → `INSERT OR IGNORE`).
async fn run_recorder_job(
    mac: &str,
    job: EyeJob,
    state: &SharedEyeState,
    storage: &StorageHandle,
    sync_fallback_secs: i64,
    mqtt_tx: &Sender<MqttMessage>,
) {
    let now = now_secs();
    let now_u32 = now as u32;
    match job {
        EyeJob::EnableRecording { interval_s } => {
            let m = mac.to_string();
            let res =
                tokio::task::spawn_blocking(move || en12830::enable_recording(&m, interval_s, now_u32))
                    .await;
            match res {
                Ok(Ok(())) => {
                    eprintln!("[EYE Monitor] Recording enabled on {mac} ({interval_s}s interval)");
                    if let Ok(mut s) = state.write() {
                        if let Some(t) = s.tags.get_mut(mac) {
                            t.is_en12830 = Some(true);
                        }
                    }
                }
                Ok(Err(e)) => {
                    eprintln!("[EYE Monitor] enable_recording {mac} failed: {e}");
                    mark_not_en12830_if_absent(state, mac, &e);
                }
                Err(e) => eprintln!("[EYE Monitor] enable_recording {mac} task error: {e}"),
            }
        }
        EyeJob::StopRecording => {
            let m = mac.to_string();
            let res = tokio::task::spawn_blocking(move || en12830::stop_recording(&m)).await;
            match res {
                Ok(Ok(())) => eprintln!("[EYE Monitor] Recording stopped on {mac}"),
                Ok(Err(e)) => {
                    eprintln!("[EYE Monitor] stop_recording {mac} failed: {e}");
                    mark_not_en12830_if_absent(state, mac, &e);
                }
                Err(e) => eprintln!("[EYE Monitor] stop_recording {mac} task error: {e}"),
            }
        }
        EyeJob::Download { since_ts, interval_s } => {
            let m = mac.to_string();
            let res = tokio::task::spawn_blocking(move || {
                en12830::download_since(&m, since_ts, interval_s, now_u32)
            })
            .await;
            match res {
                Ok(Ok((records, restart_ok))) => {
                    let n = records.len();
                    let mut max_ts = since_ts;
                    for (ts, temp) in records {
                        if ts > max_ts {
                            max_ts = ts;
                        }
                        let message_id = format!("{mac}-rec-{ts}");
                        let payload = serde_json::json!({
                            "temperature_c": temp,
                            "ts": ts,
                            "source": "en12830",
                        })
                        .to_string();
                        let _ = storage.write_eye_reading(
                            mac.to_string(),
                            ts,
                            now,
                            message_id,
                            "recording".to_string(),
                            payload,
                        );
                    }
                    eprintln!("[EYE Monitor] Back-filled {n} archived record(s) from {mac}");
                    if !restart_ok {
                        // Records were downloaded but START_RECORD didn't ack —
                        // the tag is now silently stopped. Rewind the poll gate
                        // so the fallback sync fires again in ~5 min instead of
                        // waiting a full sync_fallback_hours window.
                        eprintln!(
                            "[EYE Monitor] ⚠ START_RECORD after download for {mac} did not ack — scheduling fast retry"
                        );
                    }
                    if let Ok(mut s) = state.write() {
                        if let Some(t) = s.tags.get_mut(mac) {
                            t.is_en12830 = Some(true);
                            t.last_download_ts = if restart_ok {
                                Some(now)
                            } else {
                                Some(now.saturating_sub(sync_fallback_secs.saturating_sub(300)))
                            };
                            if max_ts > t.last_archived_ts.unwrap_or(0) {
                                t.last_archived_ts = Some(max_ts);
                            }
                        }
                    }
                }
                Ok(Err(e)) => {
                    eprintln!("[EYE Monitor] download {mac} failed: {e}");
                    mark_not_en12830_if_absent(state, mac, &e);
                    // The queuer stamped last_download_ts optimistically to
                    // rate-limit re-queuing while the job was in flight. On a
                    // real download failure we don't want that stamp to gate
                    // the fallback path for a full sync_fallback_hours — rewind
                    // it so the fallback fires again in ~5 min. Skipped when
                    // we marked the tag as not-EN12830 (no point retrying).
                    if e.kind() != io::ErrorKind::NotFound {
                        if let Ok(mut s) = state.write() {
                            if let Some(t) = s.tags.get_mut(mac) {
                                t.last_download_ts = Some(
                                    now.saturating_sub(sync_fallback_secs.saturating_sub(300)),
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[EYE Monitor] download {mac} task error: {e}");
                    if let Ok(mut s) = state.write() {
                        if let Some(t) = s.tags.get_mut(mac) {
                            t.last_download_ts =
                                Some(now.saturating_sub(sync_fallback_secs.saturating_sub(300)));
                        }
                    }
                }
            }
        }
        EyeJob::Detect => {
            // Do NOT seed a state.tags entry: the result is always published on
            // eye/detect below, and seeding a MAC that isn't in eye.tags would
            // leave a phantom in the periodic eye/sensors snapshot forever (M2).
            // For a configured tag the entry already exists (scan/add seeded it)
            // and the resolved flag is persisted via get_mut below.
            let prev = state
                .read()
                .ok()
                .and_then(|s| s.tags.get(mac).and_then(|t| t.is_en12830));
            let m = mac.to_string();
            let res = tokio::task::spawn_blocking(move || en12830::read_record_info(&m)).await;
            let (is_en12830, status) = match res {
                Ok(Ok(_info)) => {
                    eprintln!("[EYE Monitor] Detect: {mac} is an EN12830 recorder");
                    classify_detect(Ok(()), prev)
                }
                Ok(Err(e)) => {
                    // NotFound → recorder characteristics absent → standard tag.
                    // Other errors (out of range / connect fail) are inconclusive.
                    eprintln!("[EYE Monitor] Detect {mac}: {e}");
                    classify_detect(Err(e.kind()), prev)
                }
                Err(e) => {
                    eprintln!("[EYE Monitor] Detect {mac} task error: {e}");
                    (prev, "error")
                }
            };
            // Persist the flag only when the probe was conclusive; otherwise leave
            // it for a later retry rather than corrupting a known value.
            if let Some(val) = is_en12830 {
                if let Ok(mut s) = state.write() {
                    if let Some(t) = s.tags.get_mut(mac) {
                        t.is_en12830 = Some(val);
                    }
                }
            }
            // Always report an explicit result — the periodic snapshot alone
            // cannot distinguish "still detecting" from "unreachable".
            let _ = mqtt_tx.try_send(MqttMessage::PublishEyeDetectResult {
                mac: mac.to_string(),
                is_en12830,
                status: status.to_string(),
            });
        }
    }
}

/// If the recorder characteristics were absent, the tag is not an EN12830 model
/// (e.g. a black standard tag) — remember that so we stop attempting downloads.
/// Other errors (connect timeout, out of range) leave the flag unknown to retry.
fn mark_not_en12830_if_absent(state: &SharedEyeState, mac: &str, e: &io::Error) {
    if e.kind() == io::ErrorKind::NotFound {
        if let Ok(mut s) = state.write() {
            if let Some(t) = s.tags.get_mut(mac) {
                t.is_en12830 = Some(false);
            }
        }
    }
}

/// Classify a detect probe outcome into (resolved `is_en12830`, status string).
/// `probe`: `Ok(())` = recorder characteristics present; `Err(kind)` = the read
/// failed with that io kind. `prev` (the tag's current flag) is preserved when
/// the outcome is inconclusive, so an unreachable tag is not misreported as
/// "not a recorder".
fn classify_detect(
    probe: Result<(), io::ErrorKind>,
    prev: Option<bool>,
) -> (Option<bool>, &'static str) {
    match probe {
        Ok(()) => (Some(true), "ok"),
        Err(io::ErrorKind::NotFound) => (Some(false), "ok"),
        Err(_) => (prev, "unreachable"),
    }
}

/// Slim JSON payload persisted per reading (omits absent fields).
fn reading_payload_json(r: &EyeReading, rssi: Option<i16>) -> String {
    let mut o = serde_json::Map::new();
    if let Some(t) = r.temperature_c {
        o.insert("temperature_c".into(), serde_json::json!(t));
    }
    if let Some(h) = r.humidity_pct {
        o.insert("humidity_pct".into(), serde_json::json!(h));
    }
    if let Some(b) = r.battery_mv {
        o.insert("battery_mv".into(), serde_json::json!(b));
    }
    if r.low_battery {
        o.insert("low_battery".into(), serde_json::json!(true));
    }
    if r.magnet_present {
        o.insert("magnet".into(), serde_json::json!(r.magnet_detected));
    }
    if let Some(m) = r.moving {
        o.insert("moving".into(), serde_json::json!(m));
    }
    if let Some(c) = r.movement_count {
        o.insert("movement_count".into(), serde_json::json!(c));
    }
    if let Some(p) = r.pitch_deg {
        o.insert("pitch".into(), serde_json::json!(p));
    }
    if let Some(rr) = r.roll_deg {
        o.insert("roll".into(), serde_json::json!(rr));
    }
    if let Some(s) = rssi {
        o.insert("rssi".into(), serde_json::json!(s));
    }
    serde_json::to_string(&serde_json::Value::Object(o)).unwrap_or_else(|_| "{}".to_string())
}

/// Build the payload from current state and hand it to the MQTT publisher.
fn publish_snapshot(
    state: &SharedEyeState,
    mqtt_tx: &Sender<MqttMessage>,
    now_ts: i64,
    tag_timeout_s: i64,
    configured: &HashSet<String>,
) {
    let snapshot = match state.read() {
        Ok(s) => s,
        Err(_) => return,
    };
    // Only publish tags still in the live config: this prunes a just-removed tag
    // that a sub-second scan race may have re-materialised in state.tags (M1) and
    // any non-configured detect target (M2).
    let tags: Vec<EyeTagPayload> = snapshot
        .tags
        .values()
        .filter(|t| configured.contains(&t.mac))
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
            stale: t.is_stale(now_ts, tag_timeout_s),
            provisioning: t.provisioning.as_str().to_string(),
            is_en12830: t.is_en12830,
        })
        .collect();
    if tags.is_empty() {
        return;
    }
    let _ = mqtt_tx.try_send(MqttMessage::PublishEyeSensorData { tags });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_detect_maps_outcomes() {
        // recorder characteristics present -> definitely EN12830
        assert_eq!(classify_detect(Ok(()), None), (Some(true), "ok"));
        // characteristics absent -> definitely a standard (black) tag
        assert_eq!(
            classify_detect(Err(io::ErrorKind::NotFound), None),
            (Some(false), "ok")
        );
        // inconclusive (out of range / connect fail) -> keep previous, report it
        assert_eq!(
            classify_detect(Err(io::ErrorKind::TimedOut), None),
            (None, "unreachable")
        );
        assert_eq!(
            classify_detect(Err(io::ErrorKind::TimedOut), Some(true)),
            (Some(true), "unreachable")
        );
    }
}
