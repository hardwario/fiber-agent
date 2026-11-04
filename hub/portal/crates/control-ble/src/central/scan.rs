//! The central scan loop — `plan/control/ble/central.md` §4, §7;
//! `plan/control/ble/plan.md` §5 "Runtime form".
//!
//! One `async` task. Everything decidable without a controller lives in [`super::state`] and
//! [`super::adapter`] and is tested there; what is here is the BlueZ plumbing plus three behaviours
//! that are contract:
//!
//! - **`duplicate_data: true`.** Duplicates are the whole point. A beacon re-broadcasts its current
//!   state rather than emitting events, so an unchanged reading produces an identical frame — and a
//!   Hub that treats "the advertisement changed" as "a new reading arrived" silently under-samples a
//!   stable environment. A freezer holding temperature is exactly the case that matters. Reading
//!   cadence comes from **this loop's clock**, not from frame changes.
//! - **The standby gate and `eye.enabled` are re-checked on every tick**, never once per session.
//!   Checked once, a `disable` persists the flag while a running loop keeps scanning and keeps
//!   writing rows for as long as the session lives.
//! - **The loop idles rather than exiting** when either says stop. An exit would need something to
//!   restart it, and nothing does that when a flag returns to `true` in a file.
//!
//! An absent adapter is **degraded-green**: log once, keep polling, never exit, never restart the
//! Bluetooth stack.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use std::sync::RwLock;
use tokio::sync::{mpsc, watch};

use crate::config::CentralConfig;
use crate::error::{Component, Error, Kind, Result, Role};
use crate::host::{AggregatedSnapshot, Host, ReadingRow, WindowAggregate, EVENT_ADVERTISING};
use crate::mac::MacAddr;
use crate::tasks::DialJob;

use super::adapter::{select_rung, AdapterSlot, StallObservation, STEP_TIMEOUT};
use super::aggregate::{sample_of, transition_id, Closed, WindowAggregator};
use super::state::{
    evaluate_alarms, read_state, rfc3339_utc, write_state, CentralState, FieldThreshold,
    FrameOutcome,
};

/// `message_id` of one Beacon's `advertising` row: `{MAC}-adv-{window_start}`
/// (`plan/control/storage_intervals.md` §5) — **the timestamp is part of the id**, so a window is
/// stored once however often it is replayed, and two windows are two rows.
///
/// v1 derived it from a hash of the payload with no timestamp and inserted `OR IGNORE`, so for one
/// MAC each *distinct payload* was stored at most once for the life of the deployment: a freezer
/// oscillating between a handful of values yielded a handful of rows, not one per reading. On a Class
/// IIa measurement path that is a defect, not a saving.
pub fn dedup_id(mac: &MacAddr, window_start: i64) -> String {
    format!("{mac}-adv-{window_start}")
}

/// What the loop shares with its handle: a wake it can be kicked with, and the flag it publishes so
/// `is_scanning()` reports what is actually happening rather than what was configured.
#[derive(Debug, Clone, Default)]
pub struct LoopSignals {
    pub wake: Arc<tokio::sync::Notify>,
    pub scanning: Arc<AtomicBool>,
    /// A dial has asked the loop to tear down BlueZ discovery and hold it off — regardless of the
    /// `eye.enabled`/standby gate — for the lifetime of one outbound connect (issue #79). It is
    /// **orthogonal to `scanning`**: the guard that sets it never touches the configured intent, so
    /// resuming restores whatever the gate says rather than force-enabling the scan.
    pub pause: Arc<AtomicBool>,
    /// The loop's truthful "a BlueZ discovery session is open right now" bit: set while `discover`
    /// holds the events stream, cleared the moment it returns (the stream is dropped and
    /// `StopDiscovery` issued). A dial waits on this going false before it asks the controller to
    /// connect, because `scanning` reflects intent, not whether the radio is actually scanning.
    pub discovering: Arc<AtomicBool>,
}

/// How long [`ScanPauseGuard::acquire`] waits for the scan loop to confirm discovery is down before
/// it dials anyway. A backstop, not a fixed delay: the wait ends the instant `discovering` clears,
/// which is normally well under this bound.
pub const SCAN_PAUSE_GRACE: Duration = Duration::from_secs(2);

/// Pauses the scan loop's BlueZ discovery for the lifetime of an outbound dial and resumes it on
/// `Drop` — success, error, timeout and panic alike (issue #79).
///
/// On a single controller an outbound connect that races active discovery is aborted locally
/// (`le-connection-abort-by-local`), so a beacon can never be provisioned while the scan runs. This
/// guard tears discovery down first and, mirroring [`super::adapter::AdapterSlot`]'s `Claim`, resumes
/// it from `Drop` so no path — including an early `?` or a panic — can leave broadcast reception off.
/// It is orthogonal to `scanning`: a scan already gated off by `eye.enabled`/standby is not
/// spuriously re-enabled, because resuming only clears `pause` and lets the loop re-apply its gate.
#[derive(Debug)]
pub struct ScanPauseGuard {
    signals: LoopSignals,
}

impl ScanPauseGuard {
    /// Ask the loop to drop discovery, wait (bounded by `grace`) until it confirms via
    /// `discovering`, then return a guard whose `Drop` resumes. Never blocks longer than `grace`.
    pub async fn acquire(signals: &LoopSignals, grace: Duration) -> ScanPauseGuard {
        signals.pause.store(true, Ordering::SeqCst);
        signals.wake.notify_one();
        let deadline = Instant::now() + grace;
        while signals.discovering.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                eprintln!(
                    "control.ble: discovery did not confirm stopped within {grace:?}; dialling anyway"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        ScanPauseGuard {
            signals: signals.clone(),
        }
    }
}

impl Drop for ScanPauseGuard {
    fn drop(&mut self) {
        self.signals.pause.store(false, Ordering::SeqCst);
        // Kick the loop's pause-gate so discovery re-enters now, not at the next cadence tick.
        self.signals.wake.notify_one();
    }
}

#[derive(Debug, Clone)]
pub struct ScanConfig {
    pub central: CentralConfig,
    pub adapter: Option<String>,
    /// The `gateway` field on every published payload.
    pub gateway: String,
}

/// Everything the loop needs from the fleet section on a tick, read fresh each time.
struct Tick {
    enabled: bool,
    /// `eye.auto_discover` — whether beacons nobody registered are tracked at all.
    discovering: bool,
    enrolled: BTreeSet<MacAddr>,
    thresholds: BTreeMap<MacAddr, Vec<FieldThreshold>>,
    /// `eye.aggregation_interval_s` as written; the aggregator validates it.
    aggregation_interval_s: u64,
    /// `eye.recording_interval_min` — the archive cadence a provisioned recorder is held to.
    recording_interval_min: u16,
    /// Tags whose own `recording`/`logging_interval_min` overrides that cadence (`0` = off).
    recording_overrides: BTreeMap<MacAddr, u16>,
    /// `eye.en12830.enabled` — whether the Hub may dial a recorder at all.
    en12830: bool,
    /// The Beacons this Hub is home for in a cluster; `None` standalone (decision #42).
    home: Option<BTreeSet<MacAddr>>,
}

impl Tick {
    /// Whether this Hub may dial `mac` — owed dials wait for the home Hub, not for this one.
    fn allows_gatt(&self, mac: &MacAddr) -> bool {
        self.home.as_ref().is_none_or(|h| h.contains(mac))
    }

    /// The cadence `mac` is held to, or `None` while the EN12830 capability is off.
    fn recording_target(&self, mac: &MacAddr) -> Option<u16> {
        self.en12830.then(|| {
            self.recording_overrides
                .get(mac)
                .copied()
                .unwrap_or(self.recording_interval_min)
        })
    }
}

fn read_tick(host: &Host) -> Tick {
    let section = host.config.load().unwrap_or_default();
    let mut enrolled = BTreeSet::new();
    let mut thresholds = BTreeMap::new();
    let mut recording_overrides = BTreeMap::new();
    for tag in &section.tags {
        if let Ok(mac) = MacAddr::parse(&tag.mac) {
            if let Some(min) = tag.recording_override_min() {
                recording_overrides.insert(mac, min);
            }
            if tag.enabled {
                enrolled.insert(mac);
            }
            let t = FieldThreshold::from_values(&tag.field_thresholds);
            if !t.is_empty() {
                thresholds.insert(mac, t);
            }
        }
    }
    Tick {
        enabled: section.enabled,
        discovering: section.auto_discover,
        enrolled,
        thresholds,
        aggregation_interval_s: section.aggregation_interval_s,
        recording_interval_min: section.recording_interval_min,
        recording_overrides,
        en12830: section.en12830.enabled,
        home: section.home,
    }
}

/// Publish a snapshot and store any new reading. Split out so the tick body is readable.
fn publish_tick(host: &Host, state: &mut CentralState, cfg: &ScanConfig, tick: &Tick, now: i64) {
    state.recording_target_min = Some(tick.recording_interval_min);
    state
        .recording_overrides
        .clone_from(&tick.recording_overrides);
    state.expire(now, &cfg.central, &tick.enrolled);
    let snapshot = state.snapshot(
        now,
        &cfg.central,
        &cfg.gateway,
        &tick.enrolled,
        &tick.thresholds,
    );
    host.publisher.publish_sensors(&snapshot);
}

/// Store one `advertising` row. Errors are logged, never fatal: a full disk must not stop the Hub
/// hearing.
fn store_row(host: &Host, mac: &str, ts: i64, message_id: String, aggregate: &WindowAggregate) {
    let row = ReadingRow {
        mac: mac.to_string(),
        ts,
        received_at: host.clock.now_unix(),
        message_id,
        event_type: EVENT_ADVERTISING.to_string(),
        payload: serde_json::to_value(aggregate).unwrap_or(serde_json::Value::Null),
    };
    if let Err(e) = host.readings.store_reading(&row) {
        eprintln!("control.ble: cannot store a reading for {mac}: {e}");
    }
}

/// A closed storage window: one row per Beacon, then one `eye/aggregated` for all of them.
fn flush_window(host: &Host, closed: Closed) {
    for b in &closed.beacons {
        store_row(
            host,
            &b.mac,
            b.window_start,
            b.message_id.clone(),
            &b.aggregate,
        );
    }
    host.publisher.publish_aggregated(&AggregatedSnapshot {
        timestamp: rfc3339_utc(closed.window.end),
        period_start_ts: closed.window.start,
        period_end_ts: closed.window.end,
        duration_sec: closed.window.end - closed.window.start,
        tags: closed.beacons,
    });
}

/// One applied frame into the storage window (`plan/control/storage_intervals.md` §5): close a
/// window that is due, add the frame, and store a threshold transition at once.
fn sample_frame(
    host: &Host,
    windows: &mut WindowAggregator,
    state: &mut CentralState,
    tick: &Tick,
    mac: &MacAddr,
    now: i64,
) {
    windows.set_interval(tick.aggregation_interval_s);
    if let Some(closed) = windows.roll(now) {
        flush_window(host, closed);
    }
    let Some(beacon) = state.beacons.get_mut(mac) else {
        return;
    };
    let empty = Vec::new();
    let level = evaluate_alarms(beacon, tick.thresholds.get(mac).unwrap_or(&empty));
    let values = sample_of(beacon);
    if let Some(t) = windows.add(*mac, beacon.name.clone(), &values, level, now) {
        store_row(
            host,
            &t.mac.to_string(),
            t.ts,
            transition_id(&t.mac, t.ts),
            &t.aggregate,
        );
    }
}

/// The loop.
///
/// Never returns an error for a condition it exists to report: an absent or powered-off adapter is
/// degraded-green, and only a `stop` signal ends it.
pub async fn run_scan_loop(
    cfg: ScanConfig,
    host: Host,
    shared: Arc<RwLock<CentralState>>,
    slot: Arc<AdapterSlot>,
    mut stop: watch::Receiver<bool>,
    signals: LoopSignals,
    dial_tx: mpsc::Sender<DialJob>,
) -> Result<()> {
    let interval = Duration::from_secs(cfg.central.scan_interval_s.max(1));
    let mut announced_absent = false;
    // Held across discovery sessions, not per session: bluetoothd has been seen ending every
    // session 30 s in, and a publish tick anchored to the session start never came round.
    let mut cadence = Cadence {
        next_publish: Instant::now() + interval,
        last_frame: Instant::now(),
    };
    let mut short_ends: u32 = 0;
    // Consecutive failed discovery starts; feeds the recovery ladder (a hung bluetoothd answers
    // StartDiscovery with a D-Bus timeout and never trips the silence detector on its own).
    let mut start_failures: u32 = 0;
    let mut end_log = EndLog::default();
    // One snapshot per pause for a dial — see the pause gate below.
    let mut parked_published = false;
    // The storage window outlives discovery sessions for the same reason the cadence does.
    let mut windows = WindowAggregator::default();

    loop {
        if *stop.borrow() {
            return Ok(());
        }

        // A dial has asked discovery to stay down (issue #79). Do not open a new session while
        // paused; `discovering` is already false here (no session held), and we idle until the
        // guard clears `pause` and kicks `wake`, a stop arrives, or the interval elapses.
        if signals.pause.load(Ordering::SeqCst) {
            signals.discovering.store(false, Ordering::SeqCst);
            if !parked_published {
                // Nothing publishes while a dial holds the radio, so the state that dial is about
                // — "konfigurace probíhá" — would never reach the wire. One snapshot on the way in.
                parked_published = true;
                let tick = read_tick(&host);
                if tick.enabled && !host.standby.in_standby() {
                    let mut state = write_state(&shared);
                    state.discovery_off = !tick.discovering;
                    publish_tick(&host, &mut state, &cfg, &tick, host.clock.now_unix());
                }
            }
            tokio::select! {
                _ = signals.wake.notified() => {}
                _ = stop.changed() => {}
                _ = tokio::time::sleep(interval) => {}
            }
            // Time spent paused for a dial is not silence the recovery ladder should act on.
            cadence.last_frame = Instant::now();
            continue;
        }
        parked_published = false;

        let session = match bluer::Session::new().await {
            Ok(s) => s,
            Err(e) => {
                // bluetoothd not reachable: report once, keep trying. Never restart the stack.
                if !announced_absent {
                    eprintln!(
                        "control.ble: bluetoothd is not reachable over D-Bus ({e}); the central \
                         role is idle and will retry every {}s",
                        interval.as_secs()
                    );
                    announced_absent = true;
                }
                write_state(&shared).adapter_present = false;
                if wait_or_stop(&mut stop, interval).await {
                    return Ok(());
                }
                continue;
            }
        };

        let adapter = match &cfg.adapter {
            Some(name) => session.adapter(name).map_err(|e| e.to_string()),
            None => match tokio::time::timeout(STEP_TIMEOUT, session.default_adapter()).await {
                Ok(r) => r.map_err(|e| e.to_string()),
                Err(_) => {
                    Err("org.freedesktop.DBus.Error.Timeout: no answer from bluetoothd".into())
                }
            },
        };
        let adapter = match adapter {
            Ok(a) => a,
            Err(e) => {
                if is_dbus_timeout(&e) {
                    // bluetoothd is on the bus but not answering: that is a dead daemon, not a
                    // missing controller, and it counts towards the ladder.
                    start_failures = start_failures.saturating_add(1);
                    let o = observe(&shared, &slot, &cadence, start_failures);
                    if let Some(rung) = select_rung(&o, &cfg.central.recovery) {
                        fire_recovery(rung, None, &host, &slot, &o).await;
                        cadence.last_frame = Instant::now();
                    }
                }
                if !announced_absent {
                    eprintln!(
                        "control.ble: no Bluetooth controller ({e}); the central role is idle \
                         (degraded-green) and will retry every {}s",
                        interval.as_secs()
                    );
                    announced_absent = true;
                }
                write_state(&shared).adapter_present = false;
                if wait_or_stop(&mut stop, interval).await {
                    return Ok(());
                }
                continue;
            }
        };

        announced_absent = false;
        write_state(&shared).adapter_present = true;
        let _ = adapter.set_powered(true).await;

        let ctx = Discovery {
            cfg: &cfg,
            host: &host,
            shared: &shared,
            slot: &slot,
            signals: &signals,
            interval,
            dial_tx: &dial_tx,
        };
        let opened = Instant::now();
        let outcome = discover(&ctx, &adapter, &mut stop, &mut cadence, &mut windows).await;
        // `discover` has returned, so its `events` stream is dropped and StopDiscovery is issued:
        // the radio is no longer scanning. Publish that truthfully so a dial waiting to connect can
        // proceed (this covers the pause exit as well as every other return path).
        signals.discovering.store(false, Ordering::SeqCst);
        if let Err(e) = outcome {
            // A transport failure re-enters the outer loop rather than ending the task: the whole
            // point is that nothing takes the scan loop down. The listen reopens promptly — a full
            // `scan_interval_s` of deafness per ended session is what silenced `eye/sensors` — and
            // backs off only while sessions keep dying as soon as they open.
            short_ends = if opened.elapsed() < SHORT_SESSION {
                short_ends.saturating_add(1)
            } else {
                0
            };
            start_failures = if short_ends > 0 {
                start_failures.saturating_add(1)
            } else {
                0
            };
            let backoff = restart_backoff(short_ends, interval);
            end_log.record(&e, backoff);
            // The ladder is also consulted here, not only on the in-session tick: a session that
            // never opens never reaches that tick.
            let o = observe(&shared, &slot, &cadence, start_failures);
            if let Some(rung) = select_rung(&o, &cfg.central.recovery) {
                fire_recovery(rung, Some(&adapter), &host, &slot, &o).await;
                cadence.last_frame = Instant::now();
                continue;
            }
            if wait_or_stop(&mut stop, backoff).await {
                return Ok(());
            }
        }
    }
}

/// When the next snapshot is due and when a frame was last applied — the publish cadence and the
/// silence clock, both of which must outlive a single discovery session.
struct Cadence {
    next_publish: Instant,
    last_frame: Instant,
}

/// The first reopen after a discovery session ends.
const RESTART_BACKOFF: Duration = Duration::from_secs(1);
/// A session that ends sooner than this counts towards the backoff.
const SHORT_SESSION: Duration = Duration::from_secs(5);
/// An ending stream is logged at most this often, with the count since the last line.
const END_LOG_EVERY: Duration = Duration::from_secs(600);

/// 1 s, doubling per consecutive short session, capped at the scan interval.
fn restart_backoff(short_ends: u32, cap: Duration) -> Duration {
    RESTART_BACKOFF
        .saturating_mul(1u32 << short_ends.min(16))
        .min(cap)
}

/// Rate-limits the "discovery ended" line: a stream that ends every 30 s must not write one line
/// per session forever, and the count says how often it happened in between.
#[derive(Default)]
struct EndLog {
    since_line: u32,
    last_line: Option<Instant>,
}

impl EndLog {
    fn record(&mut self, e: &Error, backoff: Duration) {
        self.since_line = self.since_line.saturating_add(1);
        if self
            .last_line
            .is_some_and(|at| at.elapsed() < END_LOG_EVERY)
        {
            return;
        }
        let more = match self.since_line {
            1 => String::new(),
            n => format!(" ({n} ends since the last line)"),
        };
        eprintln!(
            "control.ble: discovery ended ({e}); restarting the listen in {}s{more}",
            backoff.as_secs()
        );
        self.since_line = 0;
        self.last_line = Some(Instant::now());
    }
}

/// Sleep, or return `true` if a stop arrived first.
async fn wait_or_stop(stop: &mut watch::Receiver<bool>, d: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => *stop.borrow(),
        _ = stop.changed() => *stop.borrow(),
    }
}

/// Everything one discovery session needs, bundled so the signature stays readable.
struct Discovery<'a> {
    cfg: &'a ScanConfig,
    host: &'a Host,
    shared: &'a Arc<RwLock<CentralState>>,
    slot: &'a Arc<AdapterSlot>,
    signals: &'a LoopSignals,
    interval: Duration,
    /// For a download or provisioning retry waiting on this beacon's next advertisement
    /// (beacon_history.md §2.4, §8).
    dial_tx: &'a mpsc::Sender<DialJob>,
}

async fn discover(
    ctx: &Discovery<'_>,
    adapter: &bluer::Adapter,
    stop: &mut watch::Receiver<bool>,
    cadence: &mut Cadence,
    windows: &mut WindowAggregator,
) -> Result<()> {
    let Discovery {
        cfg,
        host,
        shared,
        slot,
        signals,
        interval,
        dial_tx,
    } = *ctx;
    let filter = bluer::DiscoveryFilter {
        transport: bluer::DiscoveryTransport::Le,
        // See the module header: duplicates ARE the readings.
        duplicate_data: true,
        ..Default::default()
    };
    tokio::time::timeout(STEP_TIMEOUT, adapter.set_discovery_filter(filter))
        .await
        .map_err(|_| transport_err("cannot set the discovery filter: D-Bus timeout".to_string()))?
        .map_err(|e| transport_err(format!("cannot set the discovery filter: {e}")))?;
    let mut events = tokio::time::timeout(STEP_TIMEOUT, adapter.discover_devices_with_changes())
        .await
        .map_err(|_| transport_err("cannot start discovery: D-Bus timeout".to_string()))?
        .map_err(|e| transport_err(format!("cannot start discovery: {e}")))?;
    // A BlueZ discovery session is now open. A dial waits on this before it connects.
    signals.discovering.store(true, Ordering::SeqCst);

    // Whether this loop is currently allowed to scan and publish at all.
    let mut idling = false;

    loop {
        if *stop.borrow() {
            return Ok(());
        }
        // A dial asked discovery to stop: return so `events` is dropped (StopDiscovery) and the
        // outer loop parks in its pause-gate. The wake-branch below is the prompt trigger when no
        // advertising events are arriving; this catches it on any other iteration.
        if signals.pause.load(Ordering::SeqCst) {
            return Ok(());
        }

        // Copied out so the tick arm borrows nothing the frame arm writes.
        let publish_due = cadence.next_publish;
        tokio::select! {
            _ = stop.changed() => {
                if *stop.borrow() { return Ok(()); }
            }
            maybe = events.next() => {
                let Some(event) = maybe else {
                    return Err(transport_err("the discovery stream ended".to_string()));
                };
                if idling { continue; }
                if let bluer::AdapterEvent::DeviceAdded(addr) = event {
                    let now = host.clock.now_unix();
                    let tick = read_tick(host);
                    if let Ok(device) = adapter.device(addr) {
                        let mac = MacAddr::from(addr);
                        let rssi = device.rssi().await.ok().flatten();
                        let data = device.manufacturer_data().await.ok().flatten();
                        if let Some(value) = data.and_then(|m| m.get(&crate::decode::TELTONIKA_COMPANY_ID).cloned()) {
                            let applied = {
                            let mut state = write_state(shared);
                            state.discovery_off = !tick.discovering;
                            let outcome = state.on_frame(
                                mac, &value, rssi, now, &cfg.central, &tick.enrolled,
                            );
                            if outcome == FrameOutcome::Applied {
                                cadence.last_frame = Instant::now();
                                slot.clear_escalation();
                                sample_frame(host, windows, &mut state, &tick, &mac, now);
                            }
                            outcome == FrameOutcome::Applied
                            };
                            // Heard again: a download, provisioning or recording dial owed to
                            // this beacon goes now — from its home Hub only (decision #42).
                            if applied && tick.allows_gatt(&mac) {
                                crate::tasks::requeue_due_history(host, shared, dial_tx, mac);
                                crate::tasks::requeue_due_provision(
                                    host, shared, dial_tx, mac, &tick.enrolled,
                                );
                                // After the provisioning retry, which queues its own recording.
                                crate::tasks::requeue_due_recording(
                                    host, shared, dial_tx, mac, tick.recording_target(&mac),
                                );
                            }
                        }
                    }
                }
            }
            // Either the cadence tick or an explicit wake runs the same body. A config flip must
            // take effect now, not up to `scan_interval_s` from now: that is what makes
            // `ensure_scanning`'s answer honest rather than aspirational.
            _ = async {
                tokio::select! {
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(publish_due)) => {}
                    _ = signals.wake.notified() => {}
                }
            } => {
                // The pause-gate's kick lands here (a `wake`). Return so `events` is dropped and
                // discovery actually stops — this is the reliable teardown path even when no
                // advertisements are arriving to drive the events branch above.
                if signals.pause.load(Ordering::SeqCst) {
                    return Ok(());
                }
                cadence.next_publish = Instant::now() + interval;
                let now = host.clock.now_unix();
                let tick = read_tick(host);
                // A window closes on time even when no frame arrives to close it, and even while
                // the scan is paused: its frames are already in.
                windows.set_interval(tick.aggregation_interval_s);
                if let Some(closed) = windows.roll(now) {
                    flush_window(host, closed);
                }

                // Re-checked HERE, on every tick — not once per session. This is the difference
                // between a `disable` that takes effect and one that takes effect at the next reboot.
                let should_run = tick.enabled && !host.standby.in_standby();
                signals.scanning.store(should_run, std::sync::atomic::Ordering::SeqCst);
                if !should_run {
                    if !idling {
                        eprintln!(
                            "control.ble: pausing the beacon scan ({}); the task stays alive and \
                             resumes when the condition clears",
                            if !tick.enabled { "eye.enabled is false" } else { "the Hub is in standby" }
                        );
                        idling = true;
                    }
                    continue;
                }
                if idling {
                    eprintln!("control.ble: resuming the beacon scan");
                    idling = false;
                }

                {
                    let mut state = write_state(shared);
                    state.discovery_off = !tick.discovering;
                    publish_tick(host, &mut state, cfg, &tick, now);
                    windows.retain_levels(|mac| state.beacons.contains_key(mac));
                }

                // Stall detection, on the same tick. Deliberately conservative: silence only counts
                // when there is something to hear.
                let o = observe(shared, slot, cadence, 0);
                if let Some(rung) = select_rung(&o, &cfg.central.recovery) {
                    fire_recovery(rung, Some(adapter), host, slot, &o).await;
                    // Re-enter the outer loop so discovery is started afresh on the cycled adapter,
                    // with a fresh silence window, as a new session always gave it.
                    cadence.last_frame = Instant::now();
                    return Ok(());
                }
            }
        }
    }
}

/// What the ladder needs to know, read from the shared state and the slot.
fn observe(
    shared: &Arc<RwLock<CentralState>>,
    slot: &AdapterSlot,
    cadence: &Cadence,
    start_failures: u32,
) -> StallObservation {
    let (audible, adapter_present) = {
        let state = read_state(shared);
        (state.beacons.len(), state.adapter_present)
    };
    let now = Instant::now();
    StallObservation {
        idle_secs: cadence.last_frame.elapsed().as_secs(),
        since_recovery: slot.since_recovery_secs(now),
        adapter_present,
        audible,
        escalation: slot.escalation(),
        start_failures,
        since_stack_restart: slot.since_stack_restart_secs(now),
    }
}

fn is_dbus_timeout(e: &str) -> bool {
    e.contains("Timeout") || e.contains("NoReply")
}

/// Fire one rung and log it. Every rung is logged, including the bluetoothd restart.
async fn fire_recovery(
    rung: u8,
    adapter: Option<&bluer::Adapter>,
    host: &Host,
    slot: &AdapterSlot,
    o: &StallObservation,
) {
    eprintln!(
        "control.ble: no advertising reports for {}s with {} beacon(s) tracked, {} failed \
         discovery start(s), escalation {}; firing recovery rung {rung}",
        o.idle_secs, o.audible, o.start_failures, o.escalation
    );
    slot.record_recovery(Instant::now());
    let power: Box<dyn super::adapter::AdapterPower> = match adapter {
        Some(a) => Box::new(BluerPower { adapter: a.clone() }),
        None => Box::new(NoPower),
    };
    let acts = super::adapter::Actuators {
        power: power.as_ref(),
        rfkill: &super::adapter::DevRfkill,
        restarter: &super::adapter::SystemdRestarter,
        publisher: host.publisher.as_ref(),
    };
    let result = super::adapter::actuate(rung, &acts).await;
    if rung >= super::adapter::RUNG_RESTART_STACK {
        // Recorded whether or not systemd accepted it: the rate limit is about attempts.
        slot.record_stack_restart(Instant::now());
        match &result {
            Ok(_) => eprintln!(
                "control.ble: restarted {}; the central and FB00 re-register",
                super::adapter::BLUETOOTH_UNIT
            ),
            Err(e) => eprintln!("control.ble: recovery rung {rung} failed: {e}"),
        }
    } else if let Err(e) = result {
        eprintln!("control.ble: recovery rung {rung} could not fully actuate: {e}");
    }
}

/// Power control when no adapter handle could be obtained.
struct NoPower;

impl super::adapter::AdapterPower for NoPower {
    fn set_powered(&self, _on: bool) -> super::adapter::BoxFuture<'_, Result<()>> {
        Box::pin(async { Err(transport_err("no adapter handle".to_string())) })
    }
}

/// The adapter power actuator, over D-Bus. No `hciconfig`, no shell.
struct BluerPower {
    adapter: bluer::Adapter,
}

impl super::adapter::AdapterPower for BluerPower {
    fn set_powered(&self, on: bool) -> super::adapter::BoxFuture<'_, Result<()>> {
        // Async, not `block_on`: blocking a runtime worker on a D-Bus call to a hung bluetoothd
        // is what left the ladder silent for an hour on 2026-10-14. `actuate` bounds this call.
        Box::pin(async move {
            self.adapter.set_powered(on).await.map_err(|e| {
                Error::new(
                    Kind::Transport,
                    Role::Central,
                    Component::BluezDbus,
                    format!("cannot set adapter power: {e}"),
                )
            })
        })
    }
}

fn transport_err(message: String) -> Error {
    Error::new(
        Kind::Transport,
        Role::Central,
        Component::BluezDbus,
        message,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EyeSection, EyeTag};
    use crate::host::MemoryConfigStore;

    // --- the storage window (plan/control/storage_intervals.md §5) ------------------------------

    /// A valid BTSMP1 frame: 24.75 °C, humidity, battery.
    const FRAME: [u8; 6] = [0x01, 0x83, 0x09, 0xAB, 0x3F, 0x6A];

    fn window_tick(thresholds: &str) -> Tick {
        let a = MacAddr::parse("AA:BB:CC:DD:EE:FF").expect("mac");
        let mut t = BTreeMap::new();
        if !thresholds.is_empty() {
            let v: Vec<serde_yaml::Value> = serde_yaml::from_str(thresholds).expect("thresholds");
            t.insert(a, FieldThreshold::from_values(&v));
        }
        Tick {
            enabled: true,
            discovering: true,
            enrolled: BTreeSet::from([a]),
            thresholds: t,
            aggregation_interval_s: 600,
            recording_interval_min: crate::config::DEFAULT_RECORDING_INTERVAL_MIN,
            recording_overrides: BTreeMap::new(),
            en12830: false,
            home: None,
        }
    }

    #[test]
    fn frames_are_stored_once_per_window_and_published_once_per_window() {
        let publisher = Arc::new(crate::host::RecordingPublisher::default());
        let readings = Arc::new(crate::host::MemoryReadingSink::default());
        let host = Host {
            publisher: publisher.clone(),
            readings: readings.clone(),
            ..Host::in_memory()
        };
        let tick = window_tick("");
        let a = MacAddr::parse("AA:BB:CC:DD:EE:FF").expect("mac");
        let mut state = CentralState::default();
        let mut windows = WindowAggregator::default();
        let t0 = 1_791_799_200; // a multiple of 600
        for i in 0..60 {
            let now = t0 + 10 * i;
            state.on_frame(
                a,
                &FRAME,
                Some(-70),
                now,
                &CentralConfig::default(),
                &tick.enrolled,
            );
            sample_frame(&host, &mut windows, &mut state, &tick, &a, now);
        }
        assert!(readings.is_empty(), "no per-frame rows inside the window");
        assert!(publisher.aggregated().is_empty());

        state.on_frame(
            a,
            &FRAME,
            Some(-70),
            t0 + 600,
            &CentralConfig::default(),
            &tick.enrolled,
        );
        sample_frame(&host, &mut windows, &mut state, &tick, &a, t0 + 600);
        let rows = readings.rows();
        assert_eq!(rows.len(), 1, "one row for the closed window");
        let row = &rows[0];
        assert_eq!(row.message_id, format!("AA:BB:CC:DD:EE:FF-adv-{t0}"));
        assert_eq!(row.ts, t0);
        assert_eq!(row.event_type, EVENT_ADVERTISING);
        assert_eq!(row.payload["window_s"], 600);
        assert_eq!(row.payload["count"], 60);
        assert_eq!(row.payload["fields"]["temperature"]["avg"], 24.75);
        assert_eq!(row.payload["fields"]["rssi"]["min"], -70.0);
        let published = publisher.aggregated();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].period_start_ts, t0);
        assert_eq!(published[0].duration_sec, 600);
        assert_eq!(published[0].tags.len(), 1);
        assert_eq!(published[0].tags[0].message_id, row.message_id);
    }

    #[test]
    fn a_threshold_transition_is_written_through_at_once() {
        let readings = Arc::new(crate::host::MemoryReadingSink::default());
        let host = Host {
            readings: readings.clone(),
            ..Host::in_memory()
        };
        let tick = window_tick("- field: temperature\n  critical_high: 20.0\n");
        let a = MacAddr::parse("AA:BB:CC:DD:EE:FF").expect("mac");
        let mut state = CentralState::default();
        let mut windows = WindowAggregator::default();
        let now = 1_791_799_210;
        state.on_frame(
            a,
            &FRAME,
            Some(-70),
            now,
            &CentralConfig::default(),
            &tick.enrolled,
        );
        sample_frame(&host, &mut windows, &mut state, &tick, &a, now);
        let rows = readings.rows();
        assert_eq!(
            rows.len(),
            1,
            "the raise is stored without waiting for the window"
        );
        assert_eq!(rows[0].ts, now);
        assert_eq!(
            rows[0].message_id,
            format!("AA:BB:CC:DD:EE:FF-adv-{now}-alarm")
        );
        assert_eq!(rows[0].payload["alarm_state"], "CRITICAL");
        assert_eq!(rows[0].payload["transition"]["from"], "NORMAL");
        assert_eq!(rows[0].payload["fields"]["temperature"]["max"], 24.75);
        // The level holds: no second row.
        state.on_frame(
            a,
            &FRAME,
            Some(-70),
            now + 10,
            &CentralConfig::default(),
            &tick.enrolled,
        );
        sample_frame(&host, &mut windows, &mut state, &tick, &a, now + 10);
        assert_eq!(readings.len(), 1);
    }

    // --- restarting an ended discovery session ------------------------------------------------

    #[test]
    fn an_ended_session_reopens_in_a_second_and_backs_off_only_while_sessions_die_at_once() {
        let cap = Duration::from_secs(60);
        // A session that lived (bluetoothd ending it 30 s in) reopens promptly, every time.
        assert_eq!(restart_backoff(0, cap), Duration::from_secs(1));
        // Sessions that die as soon as they open double the wait, up to the scan interval.
        assert_eq!(restart_backoff(1, cap), Duration::from_secs(2));
        assert_eq!(restart_backoff(3, cap), Duration::from_secs(8));
        assert_eq!(restart_backoff(6, cap), cap);
        assert_eq!(
            restart_backoff(u32::MAX, cap),
            cap,
            "no overflow however long it lasts"
        );
        assert_eq!(
            restart_backoff(0, Duration::from_millis(500)),
            Duration::from_millis(500),
            "never longer than the scan interval itself"
        );
    }

    #[test]
    fn the_ended_stream_line_is_rate_limited_and_counts_what_it_skipped() {
        let e = transport_err("the discovery stream ended".to_string());
        let mut log = EndLog::default();
        log.record(&e, Duration::from_secs(1));
        assert_eq!(log.since_line, 0, "the first end is logged at once");
        for _ in 0..5 {
            log.record(&e, Duration::from_secs(1));
        }
        assert_eq!(log.since_line, 5, "the next ones are counted, not printed");
        log.last_line = Some(Instant::now() - END_LOG_EVERY);
        log.record(&e, Duration::from_secs(1));
        assert_eq!(
            log.since_line, 0,
            "and reported together once the window has passed"
        );
    }

    // --- ScanPauseGuard (issue #79) -----------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scan_pause_guard_waits_until_discovery_confirms_stopped() {
        let signals = LoopSignals::default();
        signals.discovering.store(true, Ordering::SeqCst);

        // Stand in for the scan loop tearing its stream down shortly after being kicked.
        let bg = {
            let signals = signals.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                signals.discovering.store(false, Ordering::SeqCst);
            })
        };

        let guard = ScanPauseGuard::acquire(&signals, Duration::from_secs(5)).await;
        // acquire only returned because discovering went false, and it requested the pause.
        assert!(
            signals.pause.load(Ordering::SeqCst),
            "pause must be set while held"
        );
        assert!(!signals.discovering.load(Ordering::SeqCst));
        bg.await.unwrap();
        drop(guard);
        assert!(
            !signals.pause.load(Ordering::SeqCst),
            "pause must clear on drop"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scan_pause_guard_is_bounded_when_discovery_never_confirms() {
        let signals = LoopSignals::default();
        // Never cleared: the loop is wedged. acquire must still return within ~grace.
        signals.discovering.store(true, Ordering::SeqCst);

        let start = Instant::now();
        let guard = ScanPauseGuard::acquire(&signals, Duration::from_millis(100)).await;
        let waited = start.elapsed();

        assert!(signals.pause.load(Ordering::SeqCst));
        assert!(
            waited < Duration::from_secs(2),
            "acquire must be bounded by grace, waited {waited:?}"
        );
        drop(guard);
        assert!(!signals.pause.load(Ordering::SeqCst));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scan_pause_guard_leaves_scanning_untouched() {
        // A scan already gated off (eye disabled) must not be spuriously re-enabled by a dial.
        let signals = LoopSignals::default();
        signals.scanning.store(false, Ordering::SeqCst);

        let guard = ScanPauseGuard::acquire(&signals, Duration::from_millis(100)).await;
        assert!(
            !signals.scanning.load(Ordering::SeqCst),
            "guard must never touch scanning"
        );
        drop(guard);
        assert!(
            !signals.scanning.load(Ordering::SeqCst),
            "still untouched after resume"
        );
    }

    #[test]
    fn scan_pause_guard_resumes_discovery_even_on_panic() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let signals = LoopSignals::default();
        // discovering is false, so acquire returns without waiting.
        let guard = rt.block_on(ScanPauseGuard::acquire(&signals, Duration::from_millis(50)));
        assert!(signals.pause.load(Ordering::SeqCst));

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _held = guard; // dropped as the stack unwinds
            panic!("dial panicked");
        }));

        assert!(result.is_err(), "the closure was supposed to panic");
        assert!(
            !signals.pause.load(Ordering::SeqCst),
            "pause must clear even when the dial panics"
        );
    }

    #[test]
    fn the_dedup_id_carries_the_timestamp() {
        let mac = MacAddr::parse("AA:BB:CC:DD:EE:FF").expect("mac");
        assert_eq!(
            dedup_id(&mac, 1_700_000_000),
            "AA:BB:CC:DD:EE:FF-adv-1700000000"
        );
        // The whole point: the same payload a tick later is a *different* id, so it is a second
        // reading rather than a silently discarded duplicate.
        assert_ne!(dedup_id(&mac, 1_700_000_000), dedup_id(&mac, 1_700_000_030));
    }

    #[test]
    fn the_tick_reads_enrolled_and_thresholds_fresh_from_the_fleet_section() {
        let store = MemoryConfigStore::with_section(EyeSection {
            enabled: true,
            tags: vec![
                EyeTag {
                    mac: "aa:bb:cc:dd:ee:ff".to_string(),
                    name: None,
                    enabled: true,
                    logging_interval_min: None,
                    recording: None,
                    field_thresholds: serde_yaml::from_str(
                        "- field: temperature\n  critical_high: 8.0\n",
                    )
                    .expect("thresholds"),
                    provisioned: None,
                },
                EyeTag {
                    mac: "00:11:22:33:44:55".to_string(),
                    name: None,
                    // A disabled tag is not enrolled for scanning purposes.
                    enabled: false,
                    logging_interval_min: None,
                    recording: None,
                    field_thresholds: Vec::new(),
                    provisioned: None,
                },
                EyeTag {
                    mac: "not a mac".to_string(),
                    name: None,
                    enabled: true,
                    logging_interval_min: None,
                    recording: None,
                    field_thresholds: Vec::new(),
                    provisioned: None,
                },
            ],
            ..EyeSection::default()
        });
        let host = Host {
            config: Arc::new(store),
            ..Host::in_memory()
        };
        let tick = read_tick(&host);
        assert!(tick.enabled);
        let a = MacAddr::parse("AA:BB:CC:DD:EE:FF").expect("mac");
        assert!(tick.enrolled.contains(&a), "normalized on the way in");
        assert_eq!(tick.enrolled.len(), 1, "a disabled tag is not scanned for");
        assert_eq!(
            tick.thresholds.get(&a).map(Vec::len),
            Some(1),
            "thresholds travel with the tag"
        );
        // A malformed MAC in the fleet file is skipped rather than taking the loop down.
        assert_eq!(tick.enrolled.len() + tick.thresholds.len(), 2);
    }

    #[test]
    fn an_unreadable_fleet_section_falls_back_to_defaults_rather_than_stopping_the_loop() {
        // `load()` failing must not be fatal: the loop keeps hearing beacons while an operator fixes
        // the file. `enabled` defaults true, so the failure mode is "keeps working", not "goes deaf".
        let tick = read_tick(&Host::in_memory());
        assert!(tick.enabled);
        assert!(tick.enrolled.is_empty());
    }

    #[test]
    fn a_disabled_section_is_visible_to_the_tick_so_the_loop_can_idle() {
        let store = MemoryConfigStore::with_section(EyeSection {
            enabled: false,
            ..EyeSection::default()
        });
        let host = Host {
            config: Arc::new(store),
            ..Host::in_memory()
        };
        assert!(!read_tick(&host).enabled);
        // And flipping it back is visible on the very next read — no restart involved.
        crate::host::ConfigStore::mutate(host.config.as_ref(), &mut |s| s.enabled = true)
            .expect("mutates");
        assert!(read_tick(&host).enabled);
    }
}
