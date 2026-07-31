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

use super::config::EyeTagConfig;

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

/// Consecutive `StartDiscovery` failures before the ladder is climbed. The call
/// times out on D-Bus when the controller is wedged, and one timeout is not
/// evidence — three in a row is.
const START_DISCOVERY_FAILURE_LIMIT: u32 = 3;

/// Don't attempt recovery again for this long after one. A rung takes seconds to
/// land and the controller needs time to resume delivering advertisements, so
/// re-firing sooner would stack resets on top of each other.
const RECOVERY_COOLDOWN_SECS: u64 = 120;

/// Rebuild the BlueZ session this often. Long enough that the rebuild cost is
/// noise, short enough to bound the D-Bus object-cache growth a tag-dense room
/// produces. Not a config key: there is no deployment for which leaking is
/// preferable, so there is nothing to tune.
///
/// Thirty minutes, and deliberately **not** shorter — see below.
///
/// Measured on FIBER-OFFICE-5 with 16 tags: RssAnon grows ~1.3 MB/min
/// (52.6 → 92.3 MB over 31 min) and a recycle returns it to a ~30 MB baseline
/// (29.9 MB five minutes later). So the interval sets the sawtooth amplitude and
/// a shorter one looks strictly better on memory.
///
/// It is not, because **the recycle itself stalls the scan.** Three of four
/// recycles were followed by a genuine "no advertisement for 180s" about five
/// minutes later — 12:31:58→12:37:10, 12:54:21→12:59:52, 13:24:35→13:29:42 — at
/// both a 30- and a 10-minute interval, and unchanged by giving a new session a
/// full stall window. Dropping the `bluer` session while its discovery stream is
/// live evidently leaves the controller delivering for a minute or two and then
/// silent; rung 0's rfkill cycle repairs it in about a second, so the system
/// self-heals, but each repair briefly drops the gateway's own advertising.
///
/// So the interval trades memory against induced stalls: 10 min meant ~6 rfkill
/// cycles an hour, 30 min means ~2, and a 92 MB peak is harmless where a
/// controller reset every ten minutes is not. The real fix is to stop discovery
/// cleanly before dropping the session, which needs more investigation than a
/// constant — until then this stays conservative. See the 2026-07-31 report.
const SESSION_RECYCLE: Duration = Duration::from_secs(30 * 60);

/// Decide whether a *silently* stalled scan warrants recovery, and at which rung.
///
/// This is the nastier of the two failure modes: BlueZ still reports
/// `Discovering: yes`, `StartDiscovery` returned success, and no call errors — but
/// zero advertising reports arrive and the controller's RX counter is frozen. From
/// inside the process it is indistinguishable from "every tag happens to be out of
/// range", which is why the decision is gated on there being tags to hear at all.
///
/// Pure so the ladder can be tested without a Bluetooth stack.
///
/// * `idle_secs` — since the last advertisement from any audible tag.
/// * `since_recovery_secs` — since the last recovery attempt (`None` = never).
/// * Returns the rung to run, or `None` to do nothing.
fn scan_recovery_action(
    idle_secs: u64,
    since_recovery_secs: Option<u64>,
    adapter_present: bool,
    audible_tags: usize,
    escalation: u32,
    stall_secs: u64,
    recovery_enabled: bool,
) -> Option<u8> {
    if !recovery_enabled {
        return None;
    }
    // A gateway with no adapter, or with nothing to listen for, has no silence to
    // explain. Without this guard a bare unit would reset its controller every
    // `stall_secs` forever.
    if !adapter_present || audible_tags == 0 {
        return None;
    }
    if idle_secs < stall_secs {
        return None;
    }
    if since_recovery_secs.is_some_and(|s| s < RECOVERY_COOLDOWN_SECS) {
        return None;
    }
    Some(escalation.min(1) as u8)
}

/// Decide whether repeated `StartDiscovery` failures warrant recovery.
///
/// The scan-won't-start mode: the call itself fails (typically a D-Bus timeout),
/// so unlike the silent stall it is visible — but retrying every 10s forever does
/// not fix a wedged controller, which is what used to turn this into a multi-hour
/// outage.
fn start_discovery_recovery_rung(
    consecutive_failures: u32,
    limit: u32,
    recovery_enabled: bool,
    audible_tags: usize,
    escalation: u32,
) -> Option<u8> {
    if !recovery_enabled || audible_tags == 0 {
        return None;
    }
    if consecutive_failures < limit {
        return None;
    }
    Some(escalation.min(1) as u8)
}

/// Run one rung of the recovery ladder.
///
/// Rung 0 is deliberately gentle: an rfkill cycle plus an `hciconfig` reset, which
/// leaves `bluetoothd` alone so the gateway's own GATT service and advertising
/// survive. An `hciconfig reset` on its own was measured to be insufficient — the
/// rfkill cycle is what actually un-wedges the combo controller.
///
/// Rung 1 restarts `bluetooth` and then `fiber`. It has to be detached: restarting
/// `fiber` kills this process, so the command cannot be awaited by it.
///
/// Note rung 0 drops the gateway's *own* advertising instances on some combo
/// chips until `bluetoothd` is restarted; that is the price of not restarting the
/// daemon on the first attempt, and rung 1 repairs it.
async fn run_scan_recovery(rung: u8, adapter: &str) {
    match rung {
        0 => {
            eprintln!("[EYE Monitor] scan recovery rung 0: rfkill cycle + {adapter} reset");
            let script = format!(
                "rfkill block bluetooth; sleep 0.3; rfkill unblock bluetooth; \
                 sleep 0.3; hciconfig {adapter} reset"
            );
            match tokio::process::Command::new("sh").arg("-c").arg(&script).status().await {
                Ok(st) if st.success() => eprintln!("[EYE Monitor] rung 0 done"),
                Ok(st) => eprintln!("[EYE Monitor] rung 0 exited {st}"),
                Err(e) => eprintln!("[EYE Monitor] rung 0 failed to run: {e}"),
            }
        }
        _ => {
            eprintln!(
                "[EYE Monitor] scan recovery rung 1: restart bluetooth, then fiber \
                 (this process will be replaced)"
            );
            let script = format!(
                "rfkill unblock bluetooth; hciconfig {adapter} reset; \
                 systemctl restart bluetooth; sleep 2; systemctl restart fiber"
            );
            // Detached on purpose: `systemctl restart fiber` terminates us, so
            // awaiting it would mean awaiting our own death.
            if let Err(e) = tokio::process::Command::new("setsid")
                .arg("sh")
                .arg("-c")
                .arg(&script)
                .spawn()
            {
                eprintln!("[EYE Monitor] rung 1 failed to spawn: {e}");
            }
        }
    }
}

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
    hostname: String,
    storage: StorageHandle,
    db_path: String,
) {
    // Live view of the config; re-read from the shared handle each poll cycle so
    // add/remove_eye_tag take effect without restarting the monitor.
    let mut config = shared_config.read().map(|g| g.clone()).unwrap_or_default();
    // Fleet allowlist (system#6): MACs registered on *any* gateway. Re-read every
    // poll next to the live config, so a server push takes effect within a second
    // without restarting the monitor.
    let mut known_tags = super::state::known_tags_snapshot();

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
            // Resume provisioning state from the config, so a restart does not
            // re-provision a tag whose flash already has the profile. Only the
            // positive case is seeded: `Some(false)`/`None` stay
            // `PendingProvisioning`, which is the safe default.
            if tag.provisioned == Some(true) {
                entry.provisioning = ProvisioningStatus::Provisioned;
            }
        }
    }

    rt.block_on(async {
        let publish_interval = Duration::from_secs(config.publish_interval_s.max(1));
        let mut last_publish = Instant::now();
        // Scan-stall watchdog state. Deliberately outside the session loop: a
        // recovery that restarts the session must not reset its own cooldown or
        // escalation, or a controller that wedges again immediately would ladder
        // from rung 0 forever instead of escalating.
        let mut last_advert = Instant::now();
        let mut last_recovery: Option<Instant> = None;
        let mut recovery_escalation: u32 = 0;
        let mut start_discovery_failures: u32 = 0;
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
            // Use the configured adapter (e.g. "hci1") when set, else the default.
            let adapter_result = match config.adapter.as_deref() {
                Some(name) => session.adapter(name),
                None => session.default_adapter().await,
            };
            let adapter = match adapter_result {
                Ok(a) => a,
                Err(e) => {
                    eprintln!(
                        "[EYE Monitor] No adapter ({}): {e}; retrying in 10s",
                        config.adapter.as_deref().unwrap_or("default"),
                    );
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
                Ok(d) => {
                    start_discovery_failures = 0;
                    d
                }
                Err(e) => {
                    start_discovery_failures += 1;
                    eprintln!(
                        "[EYE Monitor] Failed to start discovery ({start_discovery_failures} in a row): {e}"
                    );
                    let audible = audible_tags(&config, &known_tags).len();
                    if let Some(rung) = start_discovery_recovery_rung(
                        start_discovery_failures,
                        START_DISCOVERY_FAILURE_LIMIT,
                        config.scan_stall_recovery,
                        audible,
                        recovery_escalation,
                    ) {
                        run_scan_recovery(rung, adapter.name()).await;
                        recovery_escalation += 1;
                        last_recovery = Some(Instant::now());
                        last_advert = Instant::now();
                        start_discovery_failures = 0;
                    }
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    continue;
                }
            };

            if let Ok(mut s) = state.write() {
                s.adapter_present = true;
            }
            let session_started = Instant::now();
            // A fresh session gets a full stall window to produce its first
            // advertisement. `last_advert` deliberately outlives the session loop
            // (so a recovery cannot reset its own cooldown or escalation), but
            // without this the watchdog inherits the *previous* session's silence
            // across a recycle and declares a perfectly healthy new session wedged.
            //
            // Measured on FIBER-OFFICE-5 after shortening the recycle to 10 min:
            // recycle at 12:54:21, then a false "no advertisement for 180s" plus an
            // rfkill cycle at 12:59:52. At a 10-minute recycle that is a spurious
            // controller reset every 10 minutes — worse than the leak it bounds.
            //
            // The question the watchdog must ask is "has *this* session been silent
            // for stall_secs", not "has there been silence spanning a rebuild".
            last_advert = Instant::now();
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
                known_tags = super::state::known_tags_snapshot();
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

                // Scan-stall watchdog. `last_advert` moves on *any* event from the
                // adapter, not just an audible tag's — a wedged controller
                // delivers nothing at all, so any traffic proves the scan is
                // alive, and using only audible tags would fire recovery whenever
                // the tags genuinely went out of range.
                if !fresh_macs.is_empty() {
                    last_advert = Instant::now();
                }
                let audible_now = audible_tags(&config, &known_tags).len();
                if let Some(rung) = scan_recovery_action(
                    last_advert.elapsed().as_secs(),
                    last_recovery.map(|t| t.elapsed().as_secs()),
                    state.read().map(|s| s.adapter_present).unwrap_or(false),
                    audible_now,
                    recovery_escalation,
                    config.scan_stall_secs,
                    config.scan_stall_recovery,
                ) {
                    eprintln!(
                        "[EYE Monitor] scan appears wedged: no advertisement for {}s \
                         with {} audible tag(s) — BlueZ still claims to be discovering",
                        last_advert.elapsed().as_secs(),
                        audible_now,
                    );
                    run_scan_recovery(rung, adapter.name()).await;
                    recovery_escalation += 1;
                    last_recovery = Some(Instant::now());
                    last_advert = Instant::now();
                    if let Ok(mut s) = state.write() {
                        s.adapter_present = false;
                    }
                    // Rebuild the session: the rfkill cycle invalidated the
                    // discovery stream we are holding.
                    break;
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

                // Scan the union of the tags this gateway owns and the ones the
                // fleet knows about (system#6). A borrowed tag gets a default
                // profile: no thresholds (its owner raises the alarms, so
                // evaluating them here would double every notification) and
                // recording left off (the archive download opens the tag's single
                // GATT connection, and two gateways racing for it fails both).
                for tag in audible_tags(&config, &known_tags) {
                    let tag = &tag;
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
                                        entry.evaluate_alarms(tag);
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
                                            // Mark it in the live config too. The
                                            // loop re-reads `shared_config` every
                                            // tick, so without this the next tick
                                            // would restore a config that still
                                            // says the tag was never provisioned.
                                            if let Ok(mut c) = shared_config.write() {
                                                c.set_provisioned(&mac_key, true);
                                            }
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
                    // Publish a borrowed tag too, otherwise capturing it would be
                    // pointless — the prune below is the second place a
                    // fleet-known MAC used to be dropped.
                    let mut configured: HashSet<String> =
                        config.tags.iter().map(|t| t.mac.to_uppercase()).collect();
                    configured.extend(known_tags.iter().cloned());
                    publish_snapshot(
                        &state,
                        &mqtt_tx,
                        now_ts,
                        config.tag_timeout_s,
                        &configured,
                        &hostname,
                    );
                }

                // A recorder job was queued: leave the inner loop so the outer
                // loop drops the discovery guard (stops the scan) and runs it.
                if !pending.is_empty() {
                    break;
                }

                // Recycle the BlueZ session periodically. A long-lived
                // `discover_devices_with_changes` stream in a tag-dense room grows
                // the heap steadily — measured on FIBER-OFFICE-5 with 16 tags — and
                // the growth is inside the D-Bus/BlueZ object cache, not anything
                // this loop owns, so there is nothing here to free. Dropping the
                // session and rebuilding it releases the lot.
                //
                // Cheap, because the outer loop already knows how to rebuild:
                // per-tag state lives in `state`, and the archive cursor is
                // re-seeded from SQLite, so a recycle loses nothing but the
                // freshness bookkeeping BlueZ was about to re-report anyway.
                if session_started.elapsed() >= SESSION_RECYCLE {
                    eprintln!(
                        "[EYE Monitor] recycling BlueZ session after {}s (bounds heap growth)",
                        session_started.elapsed().as_secs()
                    );
                    break;
                }

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
                Ok(Ok(())) => {
                    eprintln!("[EYE Monitor] Recording stopped on {mac}");
                    // A successful STOP_RECORD proves the recorder characteristic
                    // exists → this is an EN12830 (white) tag.
                    if let Ok(mut s) = state.write() {
                        if let Some(t) = s.tags.get_mut(mac) {
                            t.is_en12830 = Some(true);
                        }
                    }
                }
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

/// Tags this gateway should listen for: the ones it owns, plus the ones the fleet
/// knows about (system#6).
///
/// A fleet-known MAC that is not in the local config gets a minimal profile — no
/// thresholds, recording untouched — because this gateway is only *listening* for
/// it. Ownership stays with whichever gateway has it in `fiber.config.yaml`, and
/// ownership is what decides who downloads the archive and who raises the alarms.
fn audible_tags(config: &EyeConfig, known: &HashSet<String>) -> Vec<EyeTagConfig> {
    let mut out: Vec<EyeTagConfig> = config.tags.iter().filter(|t| t.enabled).cloned().collect();
    let owned: HashSet<String> = out.iter().map(|t| t.mac.to_uppercase()).collect();
    for mac in known {
        if owned.contains(mac) {
            continue;
        }
        out.push(EyeTagConfig {
            mac: mac.clone(),
            name: None,
            enabled: true,
            logging_interval_min: None,
            // Explicitly off rather than inheriting `recording_enabled`: the
            // download opens the tag's single GATT connection and only its owner
            // may do that.
            recording: Some(false),
            field_thresholds: Vec::new(),
            // A borrowed tag's provisioning is its owner's business, and this
            // entry is synthetic — it is never written back to the config.
            provisioned: None,
        });
    }
    out
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
    gateway: &str,
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
        .map(|t| {
            let stale = t.is_stale(now_ts, tag_timeout_s);
            // A tag not seen within tag_timeout_s is offline: escalate the
            // aggregate alarm to Disconnected (ranked above Critical by worst())
            // so a lost tag raises a distinct alarm rather than freezing on its
            // last-known threshold state. The viewer independently maps `stale`,
            // but emitting it here keeps the firmware's own alarm_state honest.
            let alarm_state = if stale {
                t.alarm_state
                    .worst(&crate::libs::lorawan::state::LoRaWANAlarmState::Disconnected)
            } else {
                t.alarm_state.clone()
            };
            EyeTagPayload {
                // Which gateway heard this (system#6). The topic also carries the
                // hostname, but only when `mqtt.include_hostname` is on — that is an
                // operator setting, so a consumer cannot rely on it. Naming the
                // gateway in the payload is what lets the server attribute a capture
                // once more than one gateway can report the same tag.
                gateway: gateway.to_string(),
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
                stale,
                provisioning: t.provisioning.as_str().to_string(),
                is_en12830: t.is_en12830,
                field_alarm_states: t
                    .field_alarm_states
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_string()))
                    .collect(),
                alarm_state: alarm_state.to_string(),
            }
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

    const STALL: u64 = 180;

    /// `scan_recovery_action` with the arguments that are constant across the
    /// stall tests, so each test states only what it is varying.
    fn stall_action(idle: u64, since_recovery: Option<u64>, escalation: u32) -> Option<u8> {
        scan_recovery_action(idle, since_recovery, true, 3, escalation, STALL, true)
    }

    #[test]
    fn a_quiet_scan_inside_the_window_is_left_alone() {
        assert_eq!(stall_action(STALL - 1, None, 0), None);
    }

    #[test]
    fn a_wedged_scan_starts_at_the_gentle_rung_then_escalates_and_stays() {
        // One rfkill-and-reset attempt, then the full restart — and the full
        // restart from then on, rather than alternating back to rung 0.
        assert_eq!(stall_action(STALL, None, 0), Some(0));
        assert_eq!(stall_action(STALL, None, 1), Some(1));
        assert_eq!(stall_action(STALL, None, 2), Some(1));
        assert_eq!(stall_action(STALL, None, 99), Some(1));
    }

    #[test]
    fn a_bare_gateway_never_resets_its_controller() {
        // No adapter, or nothing to listen for, means the silence explains
        // itself. Without these guards an idle unit would rfkill-cycle every
        // stall window forever.
        assert_eq!(
            scan_recovery_action(STALL * 10, None, false, 3, 0, STALL, true),
            None,
            "no adapter"
        );
        assert_eq!(
            scan_recovery_action(STALL * 10, None, true, 0, 0, STALL, true),
            None,
            "no audible tags"
        );
    }

    #[test]
    fn recovery_respects_its_cooldown() {
        // A rung takes seconds to land and the controller needs time to resume,
        // so a second attempt inside the cooldown would stack resets.
        assert_eq!(stall_action(STALL, Some(RECOVERY_COOLDOWN_SECS - 1), 0), None);
        assert_eq!(stall_action(STALL, Some(RECOVERY_COOLDOWN_SECS), 0), Some(0));
    }

    #[test]
    fn recovery_can_be_switched_off_entirely() {
        assert_eq!(
            scan_recovery_action(STALL * 10, None, true, 3, 0, STALL, false),
            None,
        );
    }

    #[test]
    fn start_discovery_needs_a_streak_before_the_ladder_is_climbed() {
        // One D-Bus timeout is not evidence of a wedged controller.
        let limit = START_DISCOVERY_FAILURE_LIMIT;
        for n in 0..limit {
            assert_eq!(start_discovery_recovery_rung(n, limit, true, 3, 0), None, "n={n}");
        }
        assert_eq!(start_discovery_recovery_rung(limit, limit, true, 3, 0), Some(0));
        assert_eq!(start_discovery_recovery_rung(limit, limit, true, 3, 1), Some(1));
    }

    #[test]
    fn start_discovery_recovery_also_spares_a_gateway_with_nothing_to_hear() {
        let limit = START_DISCOVERY_FAILURE_LIMIT;
        assert_eq!(start_discovery_recovery_rung(limit * 10, limit, true, 0, 0), None);
        assert_eq!(start_discovery_recovery_rung(limit * 10, limit, false, 3, 0), None);
    }

    #[test]
    fn audible_tags_adds_fleet_known_macs_without_claiming_them() {
        // system#6: a MAC registered on another gateway must become audible here,
        // but must not inherit ownership — no thresholds (its owner raises the
        // alarms) and recording explicitly off (only the owner may open the tag's
        // single GATT connection).
        let mut config = EyeConfig::default();
        config.recording_enabled = true;
        config.tags.push(EyeTagConfig {
            mac: "AA:BB:CC:DD:EE:01".into(),
            name: Some("mine".into()),
            enabled: true,
            logging_interval_min: None,
            recording: None,
            field_thresholds: Vec::new(),
            provisioned: None,
        });
        let known: HashSet<String> = ["AA:BB:CC:DD:EE:01", "AA:BB:CC:DD:EE:02"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let out = audible_tags(&config, &known);
        let macs: HashSet<String> = out.iter().map(|t| t.mac.clone()).collect();
        assert_eq!(macs.len(), 2, "own tag + one borrowed, no duplicate");
        assert!(macs.contains("AA:BB:CC:DD:EE:02"));

        let owned = out.iter().find(|t| t.mac.ends_with(":01")).unwrap();
        assert_eq!(owned.name.as_deref(), Some("mine"), "own tag keeps its profile");
        assert_eq!(owned.recording, None, "own tag still inherits recording_enabled");

        let borrowed = out.iter().find(|t| t.mac.ends_with(":02")).unwrap();
        assert_eq!(borrowed.recording, Some(false), "a borrowed tag must not be recorded");
        assert!(borrowed.field_thresholds.is_empty(), "a borrowed tag must not alarm");
    }

    #[test]
    fn a_disabled_own_tag_stays_out_even_if_the_fleet_knows_it() {
        // Disabling a tag locally is an explicit "stop listening", so the fleet
        // allowlist re-adding it would silently override the operator... except it
        // is then a *borrowed* tag, which is the honest outcome: audible, not owned.
        let mut config = EyeConfig::default();
        config.tags.push(EyeTagConfig {
            mac: "AA:BB:CC:DD:EE:03".into(),
            name: Some("off".into()),
            enabled: false,
            logging_interval_min: None,
            recording: None,
            field_thresholds: Vec::new(),
            provisioned: None,
        });
        let none: HashSet<String> = HashSet::new();
        assert!(audible_tags(&config, &none).is_empty(), "disabled and unknown => silent");

        let known: HashSet<String> = ["AA:BB:CC:DD:EE:03"].iter().map(|s| s.to_string()).collect();
        let out = audible_tags(&config, &known);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].recording, Some(false), "re-added as borrowed, not as owned");
        assert!(out[0].name.is_none());
    }

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
