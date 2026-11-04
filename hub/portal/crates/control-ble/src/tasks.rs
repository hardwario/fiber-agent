//! The loop duties as tasks — `plan/control/ble/plan.md` §5 "Runtime form".
//!
//! > The component's continuous duties — the scan loop, the FB00 GATT server, the dial queue — are
//! > **tasks inside `proximos-control`**, not a unit of their own. `ble-setup.service` survives as
//! > the boot oneshot; there is no `control-ble.service` and no component-owned socket.
//!
//! [`LiveHandles`] is what a one-shot CLI does **not** have, and that absence is the whole reason
//! `DaemonDown` exists: `beacon-get`, `beacon-scan-list` and the phone commands need the running
//! tasks, and saying so is better than answering from nothing.
//!
//! **`CentralHandle` is the `SubsystemControl` implementation.** That is the load-bearing part:
//! without a seam that can actually start the scan loop at runtime, §9.3's auto-enable is theatre —
//! the flag reads `true` in YAML and the monitor stays dead until the next reboot, which is exactly
//! what v1 does.

use futures::StreamExt;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use std::sync::RwLock;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::task::JoinHandle;

use crate::central::adapter::AdapterSlot;
use crate::central::en12830::capability::En12830Capability;
use crate::central::en12830::protocol::RecordInfo;
use crate::central::en12830::{
    is_not_recorder, protocol, Download, RawChunk, Recorder, RecorderOps,
};
use crate::central::history::{
    drain_job_id, expected_records, not_recording_error, AfterFailure, FailReason, HistoryBody,
    HistoryRequest, RecordStream, Throttle,
};
use crate::central::ledger::{AfterProvisionFailure, ProvisionLedger};
use crate::central::provision::{provision, read_config, BluerPeer, GattPeer, Profile};
use crate::central::recording::{plan_recording, AfterRecordingFailure, RecordingStep};
use crate::central::scan::{
    run_scan_loop, LoopSignals, ScanConfig, ScanPauseGuard, SCAN_PAUSE_GRACE,
};
use crate::central::state::{
    rfc3339_utc, write_state, CentralState, Provisioning, RecordingReport, ReportedBle,
};
use crate::config::{BeaconProfile, BleConfig, DialBudget};
use crate::error::{Component, Error, Kind, Result, Role};
use crate::host::{
    DegradedState, DetectResult, Host, Publisher, ReadingRow, SensorsSnapshot, SubsystemControl,
    SystemClock, EVENT_RECORDING,
};
use crate::mac::MacAddr;
use crate::peripheral::advertise;
use crate::peripheral::buffers::PeerAddr;
use crate::peripheral::events::{EventBus, PeripheralEvent};
use crate::peripheral::gatt::{self, GattCtx};
use crate::peripheral::session::SessionSlot;
use crate::store::Store;

/// How long `shutdown` waits for the tasks to finish.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Structural backstop on how long one dial may hold the scan paused (issue #79). Every dial kind
/// already bounds itself — `provision_dial` via `session_timeout_s`, the raw-ATT kinds via
/// `En12830Capability`'s `JOB_DEADLINE` (300s) — so this only fires if a future dial forgets its own
/// timeout. It sits just above `JOB_DEADLINE` so it never truncates a legitimate archive download,
/// and it is what makes "broadcast is never left off" a property of the worker, not of each dial.
pub const SCAN_PAUSE_BACKSTOP: Duration = Duration::from_secs(305);
/// The dial queue's depth. `concurrent_dials` is 1, so this is a backlog bound, not a parallelism one.
pub const DIAL_QUEUE_DEPTH: usize = 32;
/// The peripheral housekeeping cadence — what reaps an idle session.
pub const HOUSEKEEPING_TICK: Duration = Duration::from_secs(1);

/// One queued outbound dial. Every variant claims the adapter slot before touching the radio.
#[derive(Debug, Clone, PartialEq)]
pub enum DialKind {
    /// The full default profile.
    Provision,
    /// A partial, ad hoc override — the signed `beacon-send config` path.
    ConfigOverride(Profile),
    /// Read the configuration back and write nothing — what follows an enrollment unless
    /// `eye.auto_provision` is on (GitLab #84).
    ReadConfig,
    Detect,
    /// `0` stops recording.
    Recording(u16),
    /// Download the archive (beacon_history.md §2): the window, the job id, the retry count.
    History(HistoryRequest),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DialJob {
    pub mac: MacAddr,
    pub kind: DialKind,
}

/// The central role's live state.
#[derive(Clone)]
pub struct CentralHandle {
    pub state: Arc<RwLock<CentralState>>,
    pub slot: Arc<AdapterSlot>,
    pub capability: Arc<En12830Capability>,
    pub dial_tx: mpsc::Sender<DialJob>,
    signals: LoopSignals,
    /// Whether a scan task is alive in this process at all.
    alive: Arc<AtomicBool>,
    stop: watch::Sender<bool>,
    /// The provisioning ledger's home, so a removal can forget a beacon ([`CentralHandle::forget`]).
    store: Store,
}

impl std::fmt::Debug for CentralHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CentralHandle")
            .field("alive", &self.alive.load(Ordering::SeqCst))
            .field("scanning", &self.is_scanning())
            .finish()
    }
}

impl CentralHandle {
    /// Kick the loop to re-read its gate now.
    ///
    /// `Ok(true)` means: a scan task is alive in this process and has been told to re-check
    /// immediately. It does **not** claim the radio is already listening — that is
    /// [`Self::is_scanning`]'s answer, and the two are deliberately different questions.
    pub fn start_scan(&self) -> bool {
        if !self.alive.load(Ordering::SeqCst) {
            return false;
        }
        self.signals.wake.notify_one();
        true
    }

    /// Ask the loop to idle. The task stays alive: an exit would need something to restart it, and
    /// nothing does that when a flag returns to `true` in a file.
    pub fn stop_scan(&self) {
        self.signals.scanning.store(false, Ordering::SeqCst);
        self.signals.wake.notify_one();
    }

    pub fn is_scanning(&self) -> bool {
        self.alive.load(Ordering::SeqCst) && self.signals.scanning.load(Ordering::SeqCst)
    }

    pub fn snapshot(
        &self,
        now: i64,
        cfg: &crate::config::CentralConfig,
        gateway: &str,
        enrolled: &BTreeSet<MacAddr>,
    ) -> SensorsSnapshot {
        write_state(&self.state).snapshot(now, cfg, gateway, enrolled, &Default::default())
    }

    /// Install the fleet allowlist. **The seam the CLI deliberately lacks**: `known-beacons` reports
    /// what the daemon would accept and cannot install it, because the allowlist is daemon state and
    /// is never written to config.
    pub fn set_known_beacons(&self, macs: BTreeSet<MacAddr>) {
        write_state(&self.state).known_beacons = macs;
    }

    pub fn known_beacons(&self) -> BTreeSet<MacAddr> {
        crate::central::state::read_state(&self.state)
            .known_beacons
            .clone()
    }

    /// Forget everything the central role remembers about a beacon — what a removal does
    /// (central.md §5, amended 2026-09-23): its provisioning-ledger record (attempt budget, give-up,
    /// `provisioned`, the reported configuration) and its runtime provisioning state, so a later
    /// re-add starts from nothing and is read afresh. Returns whether the ledger held a record.
    ///
    /// Like `beacon-provision`'s reset this writes the ledger outside the dial worker, so a dial
    /// already in flight for the same beacon can still record its outcome afterwards; nothing
    /// queues a new one for a beacon that is no longer enrolled.
    pub fn forget(&self, mac: &MacAddr) -> Result<bool> {
        let mut ledger = ProvisionLedger::load(&self.store)?;
        let had = ledger.reset(mac);
        if had {
            ledger.save(&self.store)?;
        }
        let mut state = write_state(&self.state);
        state.ble_configs.remove(mac);
        state.failed.remove(mac);
        state.provision_retries.remove(mac);
        state.recordings.remove(mac);
        state.provisioned.remove(mac);
        state.recording_retries.forget(mac);
        if let Some(b) = state.beacons.get_mut(mac) {
            b.provisioning = Provisioning::Pending;
            b.provision_attempts = 0;
            b.last_error = None;
        }
        Ok(had)
    }

    /// The operator's reconfiguration — the signed `beacon-provision`, the portal's "Odeslat
    /// žádost o rekonfiguraci", the CLI (central.md §5, amended 2026-09-23). An operator acting
    /// reopens the attempt budget; the beacon publishes `provisioning` at once rather than when the
    /// dial starts, and the session ends it on `provisioned` or `failed`.
    pub fn request_provision(&self, mac: &MacAddr) -> Result<()> {
        let mut ledger = ProvisionLedger::load(&self.store)?;
        ledger.reopen_budget(mac);
        ledger.save(&self.store)?;
        let before = {
            let mut s = write_state(&self.state);
            // The operator's session replaces a retry the Hub was waiting to make.
            s.provision_retries.remove(mac);
            s.beacons.get_mut(mac).map(|b| {
                let was = b.provisioning;
                b.provisioning = Provisioning::InProgress;
                was
            })
        };
        self.queue(DialJob {
            mac: *mac,
            kind: DialKind::Provision,
        })
        .map_err(|_| {
            if let Some(was) = before {
                if let Some(b) = write_state(&self.state).beacons.get_mut(mac) {
                    b.provisioning = was;
                }
            }
            Error::new(
                Kind::AlreadyExists,
                Role::Central,
                Component::BluetoothAdapter,
                "the dial queue is full",
            )
        })
    }

    /// Queue an archive download and announce it as `requested` on `eye/history` — the signed
    /// `beacon-history` and the CLI (beacon_history.md §2.1–2.2). A retry still waiting for the
    /// same Beacon is superseded: the newer request is what the operator wants now.
    ///
    /// A full dial queue does not fail the request: it is the job's first `busy` failure, and the
    /// retry ladder (§2.4) picks it up on the Beacon's next advertisement.
    pub fn request_history(
        &self,
        mac: &MacAddr,
        request: HistoryRequest,
        publisher: &dyn Publisher,
        now_mono: u64,
    ) -> bool {
        if let Some(old) = write_state(&self.state).history_retries.cancel(mac) {
            eprintln!(
                "control.ble: history job {} for {mac} superseded by {}",
                old.job, request.job
            );
        }
        publisher.publish_history(&request.requested(mac));
        let queued = self
            .queue(DialJob {
                mac: *mac,
                kind: DialKind::History(request.clone()),
            })
            .is_ok();
        if !queued {
            history_failed(
                &self.state,
                publisher,
                *mac,
                &request,
                FailReason::Busy,
                now_mono,
            );
        }
        queued
    }

    /// Queue a dial. Full is not an error worth failing a command over — it is backpressure.
    pub fn queue(
        &self,
        job: DialJob,
    ) -> std::result::Result<(), mpsc::error::TrySendError<DialJob>> {
        self.dial_tx.try_send(job)
    }

    fn signal_stop(&self) {
        let _ = self.stop.send(true);
        self.alive.store(false, Ordering::SeqCst);
        self.signals.scanning.store(false, Ordering::SeqCst);
        self.signals.wake.notify_waiters();
    }
}

/// `CentralHandle` **is** the subsystem control. See the module header.
impl SubsystemControl for CentralHandle {
    fn ensure_scanning(&self) -> Result<bool> {
        Ok(self.start_scan())
    }
    fn stop_scanning(&self) {
        self.stop_scan();
    }
    fn is_scanning(&self) -> bool {
        CentralHandle::is_scanning(self)
    }
    fn forget_beacon(&self, mac: &MacAddr) {
        if let Err(e) = self.forget(mac) {
            eprintln!("control.ble: cannot forget the removed beacon {mac}: {e}");
        }
    }
}

/// The peripheral role's live state.
#[derive(Clone)]
pub struct PeripheralHandle {
    pub session: Arc<SessionSlot>,
    pub connected_peer: Arc<Mutex<Option<PeerAddr>>>,
    /// Display-worthy transitions from the FB00 server. Subscribe to drive an LCD; ignore it on a
    /// headless Hub. This is what replaces v1's `libs/ble/event_router.rs`.
    pub events: EventBus,
    advertising: Arc<AtomicBool>,
    adv_ctl: watch::Sender<bool>,
    stop: watch::Sender<bool>,
}

impl std::fmt::Debug for PeripheralHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeripheralHandle")
            .field("advertising", &self.is_advertising())
            .finish()
    }
}

impl PeripheralHandle {
    pub fn start_advertising(&self) -> bool {
        let was = self.advertising.swap(true, Ordering::SeqCst);
        let _ = self.adv_ctl.send(true);
        !was
    }
    pub fn stop_advertising(&self) -> bool {
        let was = self.advertising.swap(false, Ordering::SeqCst);
        let _ = self.adv_ctl.send(false);
        was
    }
    pub fn is_advertising(&self) -> bool {
        self.advertising.load(Ordering::SeqCst)
    }
    /// Listen for [`PeripheralEvent`]s. Only events emitted after this call arrive.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<PeripheralEvent> {
        self.events.subscribe()
    }
    fn signal_stop(&self) {
        let _ = self.stop.send(true);
        self.advertising.store(false, Ordering::SeqCst);
    }
}

/// What a hosting process has that a one-shot CLI does not.
#[derive(Clone, Debug, Default)]
pub struct LiveHandles {
    pub central: Option<CentralHandle>,
    pub peripheral: Option<PeripheralHandle>,
}

/// The spawned runtime.
pub struct Runtime {
    pub handles: LiveHandles,
    join: Vec<JoinHandle<()>>,
}

impl Runtime {
    /// Signal every task to stop and join with a bounded wait.
    ///
    /// Bounded because a shutdown that can hang is a device that cannot be updated: a beacon in the
    /// middle of an archive download must not hold the whole process open.
    pub async fn shutdown(mut self) {
        if let Some(c) = &self.handles.central {
            c.signal_stop();
        }
        if let Some(p) = &self.handles.peripheral {
            p.signal_stop();
        }
        for h in self.join.drain(..) {
            let _ = tokio::time::timeout(SHUTDOWN_GRACE, h).await;
        }
    }
}

/// Spawn the loop duties `config.mode` calls for.
///
/// Nothing is spawned for a role the mode excludes, so a `peripheral`-only Hub never opens a scan
/// and a `central`-only Hub never registers FB00.
pub fn spawn(
    rt: &tokio::runtime::Handle,
    config: &BleConfig,
    host: Host,
    store: Store,
    hub_mac: MacAddr,
) -> Runtime {
    let mut join: Vec<JoinHandle<()>> = Vec::new();
    let mut handles = LiveHandles::default();

    // Create the `/data` tree up front rather than on first write.
    //
    // `Store::write` and the archive appender both `create_dir_all` their parent, so this is not
    // what makes them work — it is what stops the runtime depending on `ble-setup.service` having
    // run. That unit is not installed by any recipe yet (the same is true of `radio-setup.service`),
    // and a component that only half-works on an image missing one boot oneshot is a component
    // whose failure mode nobody will recognise.
    //
    // Degraded-green if it fails: a read-only or full `/data` must not stop the Hub answering a
    // phone, and the individual writes will report their own errors with the slot named.
    if let Err(e) = store.ensure_dirs() {
        eprintln!(
            "control.ble: cannot prepare {}: {e}",
            store.root().display()
        );
    }

    // **One slot and one tracked set, shared by both roles.** §0.1 is not a tidiness point: BlueZ
    // reports an inbound phone connection and our own outbound beacon dial as the same transition,
    // so the GATT server has to consult the very slot the dials claim. Two slots would mean the
    // server could not tell a phone from a provisioning burst.
    let slot = Arc::new(AdapterSlot::new());
    // Seeded from the ledger, so a restart does not turn a verified configuration back into "not
    // reported" until the next dial. An unreadable ledger is the dial path's error to raise — it
    // refuses to guess at one — so here it only means there is nothing to seed.
    let mut central = CentralState::default();
    if let Ok(ledger) = ProvisionLedger::load(&store) {
        central.ble_configs = ledger.reported_configs();
        // "konfigurace selhala" is sticky across a restart (central.md §5, amended 2026-09-23).
        central.failed = ledger.failed_sessions();
        // …and so is a provisioning retry still waiting on the ladder.
        central.provision_retries = ledger.provision_retries();
        // What each recorder last reported, and which beacons are held to the archive cadence:
        // a restart must not forget either (beacon_history.md §8, amended 2026-10-09).
        central.recordings = ledger.recordings();
        central.provisioned = ledger.provisioned_set();
    }
    let state = Arc::new(RwLock::new(central));
    let session = Arc::new(SessionSlot::new());

    if config.mode.allows(Role::Central) {
        let state = state.clone();
        let slot = slot.clone();
        let capability = Arc::new(En12830Capability::new(
            &host.config.load().unwrap_or_default().en12830,
        ));
        let signals = LoopSignals::default();
        let (stop_tx, stop_rx) = watch::channel(false);
        let (dial_tx, dial_rx) = mpsc::channel::<DialJob>(DIAL_QUEUE_DEPTH);
        let alive = Arc::new(AtomicBool::new(true));

        let scan_cfg = ScanConfig {
            central: config.central.clone(),
            adapter: config.adapter.clone(),
            gateway: host.device.hostname(),
        };

        join.push(rt.spawn({
            let host = host.clone();
            let state = state.clone();
            let slot = slot.clone();
            let signals = signals.clone();
            // The retry ladder re-queues a download on an advertisement (beacon_history.md §2.4).
            let retry_tx = dial_tx.clone();
            async move {
                if let Err(e) =
                    run_scan_loop(scan_cfg, host, state, slot, stop_rx, signals, retry_tx).await
                {
                    eprintln!("control.ble: the scan loop ended: {e}");
                }
            }
        }));

        // One worker, because `concurrent_dials` is 1: the adapter is shared between both roles and
        // a second concurrent dial buys only contention.
        join.push(rt.spawn({
            let host = host.clone();
            let slot = slot.clone();
            let capability = capability.clone();
            let state = state.clone();
            let store = store.clone();
            let adapter_name = config.adapter.clone();
            let dial_budget = config.central.dial_budget.clone();
            let profile = config.central.beacon_profile.clone();
            let scan_signals = signals.clone();
            // Weak, so the worker's own handle never keeps its queue open.
            let requeue = Some(dial_tx.downgrade());
            async move {
                dial_worker(
                    dial_rx,
                    DialWorker {
                        host,
                        slot,
                        capability,
                        state,
                        store,
                        adapter_name,
                        dial_budget,
                        profile,
                        scan_signals,
                        requeue,
                    },
                )
                .await
            }
        }));

        handles.central = Some(CentralHandle {
            state,
            slot,
            capability,
            dial_tx,
            signals,
            alive,
            stop: stop_tx,
            store: store.clone(),
        });
    }

    if config.mode.allows(Role::Peripheral) {
        let (adv_tx, adv_rx) = watch::channel(true);
        let (stop_tx, stop_rx) = watch::channel(false);
        let advertising = Arc::new(AtomicBool::new(false));
        let connected_peer = Arc::new(Mutex::new(None));
        let events = EventBus::new();
        let mut housekeeping_stop = stop_rx.clone();

        // The housekeeping tick: reap an idle session and de-authenticate every peer with it.
        join.push(rt.spawn({
            let host = host.clone();
            let session = session.clone();
            async move {
                loop {
                    if *housekeeping_stop.borrow() {
                        return;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(HOUSEKEEPING_TICK) => {}
                        _ = housekeeping_stop.changed() => {
                            if *housekeeping_stop.borrow() { return; }
                        }
                    }
                    if session.expire_if_idle(host.clock.as_ref()) {
                        eprintln!(
                            "control.ble: the provisioning session timed out; every authenticated \
                             peer is de-authenticated"
                        );
                    }
                }
            }
        }));

        // The FB00 server itself. Without this the handlers are dead code and no phone can pair —
        // which is the difference between a tested surface and a served one.
        join.push(rt.spawn({
            let host = host.clone();
            let session = session.clone();
            let slot = slot.clone();
            let state = state.clone();
            let store = store.clone();
            let cfg = config.peripheral.clone();
            let adapter_name = config.adapter.clone();
            let advertising = advertising.clone();
            let connected_peer = connected_peer.clone();
            let events = events.clone();
            let stop = stop_rx.clone();
            let dial_tx = handles.central.as_ref().map(|c| c.dial_tx.clone());
            async move {
                serve_fb00(Fb00Params {
                    host,
                    session,
                    slot,
                    state,
                    store,
                    cfg,
                    adapter_name,
                    hub_mac,
                    advertising,
                    connected_peer,
                    events,
                    adv_rx,
                    stop,
                    dial_tx,
                })
                .await
            }
        }));

        handles.peripheral = Some(PeripheralHandle {
            session,
            connected_peer,
            events,
            advertising,
            adv_ctl: adv_tx,
            stop: stop_tx,
        });
    }

    Runtime { handles, join }
}

/// The dial queue worker. One job at a time, each under the shared adapter slot.
///
/// **Per beacon, never a worker crash.** One beacon that refuses its password, times out or panics
/// its archive session must not stop the Hub hearing the other forty (AC 1), so every outcome is
/// logged and the loop continues.
async fn dial_worker(mut rx: mpsc::Receiver<DialJob>, w: DialWorker) {
    let DialWorker {
        host,
        slot,
        capability,
        state,
        store,
        adapter_name,
        dial_budget,
        profile,
        scan_signals,
        requeue,
    } = w;
    while let Some(job) = rx.recv().await {
        // Keep the capability flag live: a config rollout must take effect without a restart.
        let section = host.config.load().unwrap_or_default();
        capability.refresh(&section.en12830);
        // Only the Beacon's home Hub dials it (decision #42). Every queue — the operator's, a
        // retry, an enrollment's follow-up, the recording catch-up — ends here, so a Hub that
        // lost the home between queueing and dialling drops the job here too, without pausing
        // its scan. A download is answered `not_home` so the portal's job closes.
        if !section.allows_gatt(&job.mac) {
            eprintln!(
                "control.ble: dial {:?} for {} skipped: this Hub is not its home",
                job.kind, job.mac
            );
            if let DialKind::History(request) = &job.kind {
                history_failed(
                    &state,
                    host.publisher.as_ref(),
                    job.mac,
                    request,
                    FailReason::NotHome,
                    host.clock.monotonic_secs(),
                );
            }
            // A configuration session is deferred, not dropped (#161): a Beacon added on this
            // Hub is dialled before beacon-cluster has picked its first home, and nothing else
            // would queue it once the home arrives. A retry due at once rides the provisioning
            // ladder's requeue, which the scan loop runs only for a Beacon this Hub is home for —
            // so it goes as soon as this Hub becomes the home and hears it, and never if another
            // Hub does (no loop). An existing retry keeps its floor.
            if matches!(job.kind, DialKind::Provision) {
                let now = host.clock.now_unix();
                let mut s = write_state(&state);
                s.provision_retries.entry(job.mac).or_insert(now);
                // `request_provision` published `provisioning` at queue time; it is not.
                if let Some(b) = s.beacons.get_mut(&job.mac) {
                    if b.provisioning == Provisioning::InProgress {
                        b.provisioning = Provisioning::Pending;
                    }
                }
            }
            continue;
        }
        let ctx = DialCtx {
            host: &host,
            slot: &slot,
            capability: &capability,
            state: &state,
            store: &store,
            adapter_name: adapter_name.as_deref(),
            dial_budget: &dial_budget,
            profile: &profile,
            requeue: requeue.as_ref(),
        };
        // Pause the scan loop's BlueZ discovery for the whole dial (issue #79): on this single
        // controller an outbound connect that races active discovery is aborted locally
        // (`le-connection-abort-by-local`), so a beacon can never be provisioned while the scan
        // runs. The guard resumes discovery on Drop — success, error, timeout and panic alike —
        // and the backstop timeout guarantees it drops even if a dial hangs past its own deadline,
        // so broadcast reception is never left off. Order: pause discovery, then `run_dial` claims
        // the AdapterSlot and connects.
        // A configuration dial is "konfigurace probíhá" from here, whichever surface queued it, and
        // it has to be set before the pause: the scan loop publishes its one snapshot on the way
        // into the pause, and nothing publishes again until the dial is over.
        if matches!(job.kind, DialKind::Provision | DialKind::ConfigOverride(_)) {
            if let Some(b) = write_state(&state).beacons.get_mut(&job.mac) {
                b.provisioning = Provisioning::InProgress;
            }
        }
        let _pause = ScanPauseGuard::acquire(&scan_signals, SCAN_PAUSE_GRACE).await;
        let result = match tokio::time::timeout(SCAN_PAUSE_BACKSTOP, run_dial(&job, &ctx)).await {
            Ok(r) => r,
            Err(_) => Err(Error::new(
                Kind::Timeout,
                Role::Central,
                Component::BeaconGatt,
                format!(
                    "dial {:?} exceeded the {}s scan-pause backstop; discovery resumed",
                    job.kind,
                    SCAN_PAUSE_BACKSTOP.as_secs()
                ),
            )),
        };
        // Explicit for readability; Drop would resume discovery here regardless.
        drop(_pause);
        // A recording dial logs its own line, success or not, and owns its retries.
        if let DialKind::Recording(minutes) = job.kind {
            recording_settled(
                &state,
                job.mac,
                minutes,
                &result,
                host.clock.monotonic_secs(),
            );
        }
        if let Err(e) = result {
            if !matches!(job.kind, DialKind::Recording(_)) {
                eprintln!(
                    "control.ble: dial {:?} for {} failed: {e}",
                    job.kind, job.mac
                );
            }
            // The beacon's own record carries the reason, so the failure is visible per beacon on
            // the next snapshot rather than only in the journal.
            let mut s = write_state(&state);
            let configuring = matches!(job.kind, DialKind::Provision | DialKind::ConfigOverride(_));
            let mut stuck = false;
            if let Some(b) = s.beacons.get_mut(&job.mac) {
                b.last_error = Some(e.message().to_string());
                // A configuration dial that failed before it could record its own outcome (a busy
                // adapter, the backstop) must not leave "konfigurace probíhá" standing.
                if configuring && b.provisioning == Provisioning::InProgress {
                    b.provisioning = Provisioning::Failed;
                    stuck = true;
                }
            }
            if stuck {
                s.failed.insert(job.mac);
            }
            drop(s);
            // Every failure of a download ends here — its own, the backstop's — so this is the one
            // place the job is reported `failed` or handed to the retry ladder.
            if let DialKind::History(request) = &job.kind {
                history_failed(
                    &state,
                    host.publisher.as_ref(),
                    job.mac,
                    request,
                    FailReason::of(&e),
                    host.clock.monotonic_secs(),
                );
            }
        }
    }
}

/// A failed download: the ladder decides between a retry, announced as `requested` with the next
/// `retry` count and the reason, and the final `failed` (beacon_history.md §2.4).
fn history_failed(
    state: &RwLock<CentralState>,
    publisher: &dyn Publisher,
    mac: MacAddr,
    request: &HistoryRequest,
    reason: FailReason,
    now_mono: u64,
) {
    let after = write_state(state)
        .history_retries
        .on_failure(mac, request, reason, now_mono);
    let message = match after {
        AfterFailure::Retry(p) => {
            eprintln!(
                "control.ble: history job {} for {mac} failed ({}); retry {} once the beacon is \
                 heard again, not before {} s from now",
                request.job,
                reason.as_str(),
                p.request.retry,
                p.not_before.saturating_sub(now_mono)
            );
            p.request.requested(&mac)
        }
        AfterFailure::Final => request.message(&mac, HistoryBody::Failed { reason }),
    };
    publisher.publish_history(&message);
}

/// The scan loop heard `mac`: queue its download retry if one is due (beacon_history.md §2.4). A
/// full queue puts it back for the next advertisement.
pub(crate) fn requeue_due_history(
    host: &Host,
    state: &RwLock<CentralState>,
    dial_tx: &mpsc::Sender<DialJob>,
    mac: MacAddr,
) {
    let now = host.clock.monotonic_secs();
    let Some(request) = write_state(state).history_retries.take_due(&mac, now) else {
        return;
    };
    let message = request.requested(&mac);
    match dial_tx.try_send(DialJob {
        mac,
        kind: DialKind::History(request),
    }) {
        Ok(()) => host.publisher.publish_history(&message),
        Err(e) => {
            if let DialKind::History(request) = e.into_inner().kind {
                write_state(state)
                    .history_retries
                    .put_back(mac, request, now);
            }
        }
    }
}

/// The scan loop heard `mac`: queue its provisioning retry if one is due (beacon_history.md §8,
/// amended 2026-10-09 — the history ladder, applied to a failed provisioning session). Only a
/// beacon that is still ours is dialled. It publishes `provisioning` from the moment it is queued,
/// like the operator's request; a full queue puts the retry back for the next advertisement.
pub(crate) fn requeue_due_provision(
    host: &Host,
    state: &RwLock<CentralState>,
    dial_tx: &mpsc::Sender<DialJob>,
    mac: MacAddr,
    enrolled: &BTreeSet<MacAddr>,
) {
    let now = host.clock.now_unix();
    let mut s = write_state(state);
    let Some(&at) = s.provision_retries.get(&mac) else {
        return;
    };
    if !crate::central::ledger::provision_retry_due(at, now)
        || !(enrolled.contains(&mac) || s.known_beacons.contains(&mac))
    {
        return;
    }
    s.provision_retries.remove(&mac);
    let was = s
        .beacons
        .get_mut(&mac)
        .map(|b| std::mem::replace(&mut b.provisioning, Provisioning::InProgress));
    match dial_tx.try_send(DialJob {
        mac,
        kind: DialKind::Provision,
    }) {
        Ok(()) => eprintln!("control.ble: {mac} heard again; retrying its failed provisioning"),
        Err(_) => {
            s.provision_retries.insert(mac, at);
            if let (Some(was), Some(b)) = (was, s.beacons.get_mut(&mac)) {
                b.provisioning = was;
            }
        }
    }
}

/// What the dial worker owns for its lifetime.
struct DialWorker {
    host: Host,
    slot: Arc<AdapterSlot>,
    capability: Arc<En12830Capability>,
    state: Arc<RwLock<CentralState>>,
    store: Store,
    adapter_name: Option<String>,
    dial_budget: DialBudget,
    profile: BeaconProfile,
    /// The scan loop's signals, so a dial can pause discovery for the duration of its connect.
    scan_signals: LoopSignals,
    /// The worker's own queue, for a dial that schedules a follow-up (§3's recording after a
    /// provisioning session). Weak, so it never keeps the queue open; `None` in tests.
    requeue: Option<mpsc::WeakSender<DialJob>>,
}

/// Everything one dial needs. Bundled so the signature stays readable.
struct DialCtx<'a> {
    host: &'a Host,
    slot: &'a Arc<AdapterSlot>,
    capability: &'a Arc<En12830Capability>,
    state: &'a Arc<RwLock<CentralState>>,
    store: &'a Store,
    adapter_name: Option<&'a str>,
    dial_budget: &'a DialBudget,
    profile: &'a BeaconProfile,
    requeue: Option<&'a mpsc::WeakSender<DialJob>>,
}

async fn run_dial(job: &DialJob, ctx: &DialCtx<'_>) -> Result<()> {
    match &job.kind {
        DialKind::Detect => detect(job.mac, ctx).await,
        DialKind::Recording(min) => recording(job.mac, *min, ctx).await,
        DialKind::History(request) => history(job.mac, request, ctx).await,
        DialKind::Provision => {
            provision_dial(job.mac, Profile::from_config(ctx.profile), true, ctx).await?;
            queue_recording_after_provision(job.mac, ctx);
            Ok(())
        }
        DialKind::ConfigOverride(p) => provision_dial(job.mac, *p, false, ctx).await,
        DialKind::ReadConfig => read_config_dial(job.mac, ctx).await,
    }
}

/// Probe whether a beacon is an EN12830 recorder.
///
/// The tri-state is the whole point: a **missing recorder characteristic** is the only outcome that
/// may write `Some(false)`, because it is the only one that means "we asked and it is not a
/// recorder". A timeout or an absent controller means *we could not tell*, and must leave the axis
/// `None` — a UI that renders `None` as "standard beacon" is wrong.
async fn detect(mac: MacAddr, ctx: &DialCtx<'_>) -> Result<()> {
    let answer = ctx
        .capability
        .run_contained(
            ctx.slot,
            &mac,
            "beacon-detect",
            ctx.host.publisher.as_ref(),
            move || match Recorder::connect(&mac) {
                Ok(_) => Ok(Some(true)),
                // Not a failure — an answer. Returning `Err` here would count a perfectly healthy
                // standard beacon toward the protocol-failure alarm.
                Err(e) if is_not_recorder(&e) => Ok(Some(false)),
                Err(e) => Err(e),
            },
        )
        .await;

    let now = ctx.host.clock.now_unix();
    let (is_en12830, status) = match &answer {
        Ok(v) => (*v, "ok"),
        Err(e) if e.kind() == Kind::BeaconUnreachable => (None, "unreachable"),
        Err(_) => (None, "error"),
    };

    // Only a definite answer touches the axis; "could not tell" leaves whatever was known.
    if is_en12830.is_some() {
        if let Some(b) = write_state(ctx.state).beacons.get_mut(&mac) {
            b.is_en12830 = is_en12830;
        }
    }
    ctx.host.publisher.publish_detect(&DetectResult {
        timestamp: rfc3339_utc(now),
        mac: mac.to_string(),
        is_en12830,
        status: status.to_string(),
    });
    answer.map(|_| ())
}

/// Set the archive logging interval. `0` stops recording.
///
/// **A start that changes the interval, or starts a stopped recorder, erases the Beacon's whole
/// archive** (beacon_history.md §8, amended 2026-10-09). So the dial reads the record info first:
/// a Beacon that already holds the target is sent nothing. Otherwise the archive is downloaded
/// from the newest `recording` row this Hub holds — published on `eye/history` under a Hub job
/// (`bh-hub-drain-…`), exactly like a download without `from` — and only a download that completed
/// lets the start go out. A failed download changes nothing, and the dial fails onto the recording
/// ladder. The signed `beacon-recording` command and the Hub's own cadence dials both come here.
async fn recording(mac: MacAddr, interval_min: u16, ctx: &DialCtx<'_>) -> Result<()> {
    // Refused, never snapped to a neighbour: an operator who asked for five minutes and silently
    // got fifteen has a compliance problem and no way to know.
    let target_s = if interval_min == 0 {
        0
    } else {
        protocol::interval_s(interval_min)?
    };
    let now_unix = ctx.host.clock.now_unix();
    let now = u32::try_from(now_unix.max(0)).unwrap_or(0);
    let drain = ArchiveJob::new(
        ctx,
        mac,
        HistoryRequest::new(None, None, Some(drain_job_id(now_unix)), now_unix)?,
    );
    let worker_drain = drain.clone();
    let dial = ctx
        .capability
        .run_contained(
            ctx.slot,
            &mac,
            "beacon-recording",
            ctx.host.publisher.as_ref(),
            move || {
                let r = Recorder::connect(&mac)?;
                change_recording(&r, target_s, now, &worker_drain)
            },
        )
        .await?;
    settle_recording_dial(ctx, &drain, dial)
}

/// What the blocking half of a recording dial hands back.
struct RecordingDial {
    /// The record info read before anything was sent.
    before: RecordInfo,
    /// The archive read before a start, when the start needed one.
    drained: Option<(Download, Streamed)>,
    /// The Beacon's answer to the stop or start; `None` when nothing was sent.
    sent: Option<Result<bool>>,
    /// The record info read after the command; `None` when nothing was sent or the read failed.
    after: Option<RecordInfo>,
}

/// The blocking half of a recording dial, on any [`RecorderOps`]: read the record info, then
/// [`plan_recording`] decides between nothing, a stop, and a drained start. A drain that fails
/// publishes `failed` for its job, logs its line and fails the dial before anything is sent.
fn change_recording(
    r: &dyn RecorderOps,
    target_s: u16,
    now: u32,
    drain: &ArchiveJob,
) -> Result<RecordingDial> {
    let before = r.read_record_info()?;
    let mut drained = None;
    match plan_recording(&before, target_s) {
        RecordingStep::Hold => {
            return Ok(RecordingDial {
                before,
                drained: None,
                sent: None,
                after: None,
            })
        }
        RecordingStep::Stop => {}
        RecordingStep::DrainThenStart { restart_s } => {
            // Decode at the Beacon's own interval; a stopped one that reports none, at the target.
            let interval_s = if before.interval_s != 0 {
                before.interval_s
            } else {
                target_s
            };
            let (download, streamed) = drain.stream(r, interval_s, restart_s);
            match download {
                Ok(d) => drained = Some((d, streamed)),
                Err(e) => {
                    // The download may have stopped the Beacon part way. A start at its own
                    // interval keeps the archive (hardware, 2026-10-09), so put it back; if the
                    // link is gone this fails too, and the stop alone erases nothing.
                    if let Some(own) = restart_s {
                        let _ = r.set_recording(own, now);
                    }
                    drain.publish(HistoryBody::Failed {
                        reason: FailReason::of(&e),
                    });
                    eprintln!(
                        "control.ble: archive drain of {} before its recording change (job {}): \
                         {} records sent, failed: {e}; recording left unchanged",
                        drain.mac, drain.request.job, streamed.sent
                    );
                    return Err(Error::new(
                        e.kind(),
                        Role::Central,
                        e.component(),
                        format!(
                            "{}: the archive could not be read before the recording change, so \
                             the recording was left as it was: {}",
                            drain.mac,
                            e.message()
                        ),
                    ));
                }
            }
        }
    }
    let sent = r.set_recording(target_s, now);
    let after = r.read_record_info().ok();
    Ok(RecordingDial {
        before,
        drained,
        sent: Some(sent),
        after,
    })
}

/// The async half of a recording dial: keep what the Beacon reports, store and close the drain,
/// and turn the Beacon's answer into the dial's result.
fn settle_recording_dial(ctx: &DialCtx<'_>, drain: &ArchiveJob, dial: RecordingDial) -> Result<()> {
    let mac = drain.mac;
    let RecordingDial {
        before,
        drained,
        sent,
        after,
    } = dial;
    // What the Beacon says, not the command's answer, is what `ble_config.recording` reports: the
    // read after a command, or the first read when nothing was sent. A failed read after a command
    // leaves the last report standing.
    if let Some(info) = after.as_ref().or(sent.is_none().then_some(&before)) {
        record_recording(ctx, mac, info);
    }
    if let Some((mut download, streamed)) = drained {
        // The drain's `done.restart_ok` says whether the Beacon was left recording — which, after
        // a drain, is the start's doing.
        download.restart_ok = after.map_or(matches!(sent, Some(Ok(true))), |a| a.is_recording);
        let stored = finish_download(ctx, drain, &download, &streamed);
        eprintln!(
            "control.ble: archive drain of {mac} before its recording change (job {}): {} records, \
             {} sessions, {stored} stored: ok",
            drain.request.job,
            download.records.len(),
            download.sessions
        );
    }
    match sent {
        Some(Err(e)) => return Err(e),
        Some(Ok(false)) => {
            return Err(Error::new(
                Kind::Transport,
                Role::Central,
                Component::L2cap,
                format!("{mac} refused the recording command"),
            ))
        }
        _ => {}
    }
    // A beacon that answered the recorder protocol is a recorder, which is knowledge worth keeping.
    if let Some(b) = write_state(ctx.state).beacons.get_mut(&mac) {
        b.is_en12830 = Some(true);
    }
    Ok(())
}

/// One archive download's fixed inputs: the Beacon, the job it reports under, the window, and
/// where it reports. Cloned into the blocking thread.
#[derive(Clone)]
struct ArchiveJob {
    mac: MacAddr,
    request: HistoryRequest,
    /// The request's `from`, or the newest `recording` row this Hub holds, else 0 (everything).
    since: i64,
    until: Option<i64>,
    /// The wall clock when the job started: nothing ahead of it is kept.
    now_unix: i64,
    publisher: Arc<dyn Publisher>,
    clock: Arc<dyn SystemClock>,
}

/// What streaming a download sent besides its records.
struct Streamed {
    expected: Option<u64>,
    /// `records` messages sent.
    batches: u32,
    /// Records sent.
    sent: u64,
}

impl ArchiveJob {
    fn new(ctx: &DialCtx<'_>, mac: MacAddr, request: HistoryRequest) -> ArchiveJob {
        let since = match request.from {
            Some(from) => from,
            None => ctx
                .host
                .readings
                .max_reading_ts(&mac, EVENT_RECORDING)
                .unwrap_or(None)
                .unwrap_or(0),
        };
        ArchiveJob {
            mac,
            since,
            until: request.to,
            request,
            now_unix: ctx.host.clock.now_unix(),
            publisher: ctx.host.publisher.clone(),
            clock: ctx.host.clock.clone(),
        }
    }

    fn publish(&self, body: HistoryBody) {
        self.publisher
            .publish_history(&self.request.message(&self.mac, body));
    }

    /// Run the download, publishing `receiving` (at most one per 5 s) and the `records` batches as
    /// chunks are decoded (beacon_history.md §2.3, §8). Blocking. `restart_s` is what the Beacon
    /// is left recording at (`None`: left stopped).
    fn stream(
        &self,
        r: &dyn RecorderOps,
        interval_s: u16,
        restart_s: Option<u16>,
    ) -> (Result<Download>, Streamed) {
        let now = u32::try_from(self.now_unix.max(0)).unwrap_or(0);
        let end = self.until.map_or(self.now_unix, |t| t.min(self.now_unix));
        let expected = expected_records(self.since, end, interval_s);
        let mut throttle = Throttle::default();
        let mut stream = RecordStream::default();
        let mut on_chunk = |received: usize, fresh: &[(i64, f64)]| {
            // Records stream as each chunk is decoded, like a Node's history frames: a download
            // cut part way still delivers what it read, and the Portal's INSERT OR IGNORE makes a
            // retry's overlap harmless.
            for body in stream.batches(fresh, interval_s) {
                self.publish(body);
            }
            if throttle.ready(self.clock.monotonic_secs()) {
                self.publish(HistoryBody::Receiving {
                    received: received as u64,
                    expected,
                    reason: None,
                });
            }
        };
        on_chunk(0, &[]);
        let download = r.download_since(
            self.since,
            self.until,
            interval_s,
            restart_s,
            now,
            &mut |p| on_chunk(p.received, p.fresh),
        );
        (
            download,
            Streamed {
                expected,
                batches: stream.batches,
                sent: stream.sent,
            },
        )
    }
}

/// What the blocking half of a download hands back.
enum Fetched {
    /// The beacon is not recording: nothing was read, and nothing was restarted.
    NotRecording(RecordInfo),
    Downloaded {
        download: Download,
        info: RecordInfo,
        streamed: Streamed,
    },
}

/// Back-fill a recorder beacon's archive (beacon_history.md §2).
///
/// The window starts at the request's `from`, or — without one — at the newest `recording` row this
/// Hub holds, so a re-run does not re-download what is already stored; the device returns whole
/// pages, so both ends are cut on the way out. Progress goes out as `receiving` (at most one per
/// 5 s), the records in `records` batches, then `done`. A failure is the dial worker's to report.
async fn history(mac: MacAddr, request: &HistoryRequest, ctx: &DialCtx<'_>) -> Result<()> {
    let job = ArchiveJob::new(ctx, mac, request.clone());
    let worker_job = job.clone();

    let fetched = ctx
        .capability
        .run_contained(
            ctx.slot,
            &mac,
            "beacon-history",
            ctx.host.publisher.as_ref(),
            move || {
                let r = Recorder::connect(&mac)?;
                fetch_history(&r, &worker_job)
            },
        )
        .await?;

    let (download, streamed) = match fetched {
        Fetched::NotRecording(info) => {
            record_recording(ctx, mac, &info);
            return Err(not_recording_error(&mac));
        }
        Fetched::Downloaded {
            download,
            info,
            streamed,
        } => {
            record_recording(ctx, mac, &info);
            (download, streamed)
        }
    };
    let stored = finish_download(ctx, &job, &download, &streamed);

    // **A beacon left not recording is the failure to surface** — so it is reported, loudly, rather
    // than folded into a successful download.
    if !download.restart_ok {
        ctx.host.publisher.publish_degraded(&DegradedState {
            condition: "en12830_not_recording",
            mac: Some(mac.to_string()),
            active: true,
            detail: format!(
                "{mac} did not resume recording after an archive download; its temperature history \
                 has a gap from now until an operator restarts it"
            ),
        });
    }
    eprintln!(
        "control.ble: archive from {mac} (job {}): {} records in {} chunks ({} sessions, {} \
         rejected chunks, {} future records dropped), {stored} stored, restart_ok={}",
        request.job,
        download.records.len(),
        download.chunks_read,
        download.sessions,
        download.rejected_chunks,
        download.future_dropped,
        download.restart_ok
    );
    Ok(())
}

/// The blocking half of a download, on any [`RecorderOps`]. A Beacon that is not recording is
/// left alone. A download that fails part way may have left the Beacon stopped (it stops it
/// first), so it is put back recording at the interval it had — the same interval keeps the
/// archive (hardware, 2026-10-09) — best effort, and the failure is returned unchanged for the
/// dial worker to report and ladder.
fn fetch_history(r: &dyn RecorderOps, job: &ArchiveJob) -> Result<Fetched> {
    let info = r.read_record_info()?;
    if !info.is_recording {
        return Ok(Fetched::NotRecording(info));
    }
    // The beacon's own interval wins when it reports one; otherwise the requested one.
    let interval_s = if info.interval_s != 0 {
        info.interval_s
    } else {
        60
    };
    // Restarted at the same interval: that keeps the archive.
    let (download, streamed) = job.stream(r, interval_s, Some(interval_s));
    match download {
        Ok(download) => Ok(Fetched::Downloaded {
            download,
            info,
            streamed,
        }),
        Err(e) => {
            let now = u32::try_from(job.now_unix.max(0)).unwrap_or(0);
            let restarted = match r.set_recording(interval_s, now) {
                Ok(true) => "ok".to_string(),
                Ok(false) => "refused".to_string(),
                Err(re) => format!("failed: {re}"),
            };
            eprintln!(
                "control.ble: archive download from {} (job {}) failed part way; restart at {}                  s: {restarted}",
                job.mac, job.request.job, interval_s
            );
            Err(e)
        }
    }
}

/// A download that completed: store every record and append the segment and the raw chunks on the
/// Hub, then close the job with `done` (every `records` batch went out during the download).
/// Storage errors are reported but do not discard the rest. Returns the rows stored.
fn finish_download(
    ctx: &DialCtx<'_>,
    job: &ArchiveJob,
    download: &Download,
    streamed: &Streamed,
) -> usize {
    let mac = job.mac;
    let mut stored = 0usize;
    for (ts, celsius) in &download.records {
        let row = ReadingRow {
            mac: mac.to_string(),
            ts: *ts,
            received_at: ctx.host.clock.now_unix(),
            // `-rec-` and the record's own timestamp: distinct from the `-adv-` advertising id, and
            // idempotent across re-downloads of the same page.
            message_id: format!("{mac}-rec-{ts}"),
            event_type: EVENT_RECORDING.to_string(),
            payload: serde_json::json!({ "temperature_c": celsius }),
        };
        match ctx.host.readings.store_reading(&row) {
            Ok(()) => stored += 1,
            Err(e) => eprintln!("control.ble: cannot store an archive row for {mac}: {e}"),
        }
    }
    if let Err(e) = append_archive_segment(ctx.store, &mac, download) {
        eprintln!("control.ble: cannot append the archive segment for {mac}: {e}");
    }
    if let Err(e) = append_archive_chunks(ctx.store, &mac, job.now_unix, &download.chunks) {
        eprintln!("control.ble: cannot append the raw archive chunks for {mac}: {e}");
    }

    {
        let mut state = write_state(ctx.state);
        if let Some(b) = state.beacons.get_mut(&mac) {
            b.is_en12830 = Some(true);
            b.last_download_ts = Some(ctx.host.clock.now_unix());
            b.last_archived_ts = download
                .records
                .last()
                .map(|(ts, _)| *ts)
                .or(b.last_archived_ts);
        }
    }

    job.publish(HistoryBody::Done {
        received: download.records.len() as u64,
        expected: streamed.expected,
        batches: streamed.batches,
        sessions: download.sessions,
        rejected_chunks: download.rejected_chunks,
        future_dropped: download.future_dropped as u64,
        // The block CRC polynomial is not known yet (§7 Q6 layer 2).
        crc_failed: None,
        restart_ok: download.restart_ok,
        reason: None,
    });
    stored
}

/// Keep what a recorder said about its own recording, for `ble_config` and the `differs` judgment —
/// in the ledger too, so it outlives a restart. A ledger that cannot be written is logged; the
/// runtime report still stands.
fn record_recording(ctx: &DialCtx<'_>, mac: MacAddr, info: &RecordInfo) {
    let report = RecordingReport::from_info(info, ctx.host.clock.now_unix());
    write_state(ctx.state).recordings.insert(mac, report);
    let persisted = ProvisionLedger::load(ctx.store).and_then(|mut ledger| {
        ledger.record_recording(&mac, report);
        ledger.save(ctx.store)
    });
    if let Err(e) = persisted {
        eprintln!("control.ble: cannot persist the archive recording of {mac}: {e}");
    }
}

/// §3: a Beacon this Hub provisioned records at the fleet cadence (`eye.recording_interval_min`).
///
/// Queued as its own dial rather than run inside the provisioning one: the raw ATT session gets its
/// own adapter claim, deadline and scan-pause backstop instead of borrowing what is left of the GATT
/// session's, and it runs only after that session has disconnected.
fn queue_recording_after_provision(mac: MacAddr, ctx: &DialCtx<'_>) {
    if !ctx.capability.enabled() {
        eprintln!("control.ble: {mac} provisioned; archive recording not set (en12830 disabled)");
        return;
    }
    let Some(tx) = ctx.requeue.and_then(|w| w.upgrade()) else {
        return;
    };
    // A tag's own `beacon-recording` wins over the fleet cadence, `0` included: re-provisioning must
    // not restart a recorder an operator stopped (v1 defect H1).
    let minutes = ctx
        .host
        .config
        .load()
        .map(|eye| eye.recording_target_min(&mac))
        .unwrap_or(crate::config::DEFAULT_RECORDING_INTERVAL_MIN);
    match tx.try_send(DialJob {
        mac,
        kind: DialKind::Recording(minutes),
    }) {
        Ok(()) => {
            write_state(ctx.state).recording_retries.started(
                mac,
                minutes,
                ctx.host.clock.monotonic_secs(),
            );
            eprintln!("control.ble: {mac} provisioned; queued archive recording {minutes} min")
        }
        Err(_) => eprintln!(
            "control.ble: {mac} provisioned, but the dial queue is full: archive recording \
             {minutes} min not queued; its next advertisement queues it"
        ),
    }
}

/// The outcome of a recording dial: one log line, and the ladder (beacon_history.md §8, amended
/// 2026-10-09). Success means the Beacon reads back holding `minutes`; a dial the Beacon accepted
/// but whose read-back says otherwise (or was not read) is a failure, or the next advertisement
/// would dial again at once.
fn recording_settled(
    state: &RwLock<CentralState>,
    mac: MacAddr,
    minutes: u16,
    result: &Result<()>,
    now_mono: u64,
) {
    let mut s = write_state(state);
    let report = s.recordings.get(&mac).copied();
    let read = report.map_or_else(
        || "not read back".to_string(),
        |r| format!("reads recording={} at {} min", r.recording, r.interval_min),
    );
    let (why, retryable) = match result {
        Ok(()) if report.is_some_and(|r| r.holds(minutes)) => {
            s.recording_retries.on_success(&mac);
            eprintln!("control.ble: archive recording of {mac} set to {minutes} min: ok, {read}");
            return;
        }
        Ok(()) => (format!("accepted, but {read}"), true),
        Err(e) => {
            // A standard beacon has no archive: knowing that stops the next advertisement asking.
            if is_not_recorder(e) {
                if let Some(b) = s.beacons.get_mut(&mac) {
                    b.is_en12830 = Some(false);
                }
            }
            (e.to_string(), FailReason::of(e).retryable())
        }
    };
    let next = match s
        .recording_retries
        .on_failure(mac, minutes, retryable, now_mono)
    {
        AfterRecordingFailure::RetryIn(d) => {
            format!("retry once the beacon is heard again, not before {d} s from now")
        }
        AfterRecordingFailure::GaveUp => {
            "giving up until the target changes or the Hub restarts".to_string()
        }
    };
    eprintln!("control.ble: archive recording of {mac} set to {minutes} min failed: {why}; {next}");
}

/// The scan loop heard `mac`: queue a recording dial if it is a recorder this Hub provisioned and
/// its last report does not hold `target_min` — nothing reported counts, since that is what a
/// restart or a full queue leaves behind (beacon_history.md §8, amended 2026-10-09). `None` is the
/// EN12830 capability switched off. Dials follow the ladder in `recording_retries`.
pub(crate) fn requeue_due_recording(
    host: &Host,
    state: &RwLock<CentralState>,
    dial_tx: &mpsc::Sender<DialJob>,
    mac: MacAddr,
    target_min: Option<u16>,
) {
    let Some(target) = target_min else {
        return;
    };
    let mut s = write_state(state);
    if !s.provisioned.contains(&mac) {
        return;
    }
    if let Some(b) = s.beacons.get(&mac) {
        // Not a recorder, or a configuration session in flight (it queues its own recording).
        if b.is_en12830 == Some(false) || b.provisioning == Provisioning::InProgress {
            return;
        }
    }
    let report = s.recordings.get(&mac).copied();
    if report.is_some_and(|r| r.holds(target)) {
        s.recording_retries.settled(&mac);
        return;
    }
    if !s
        .recording_retries
        .take_due(mac, target, host.clock.monotonic_secs())
    {
        return;
    }
    match dial_tx.try_send(DialJob {
        mac,
        kind: DialKind::Recording(target),
    }) {
        Ok(()) => eprintln!(
            "control.ble: {mac} {} but its target is {target} min; queued archive recording",
            report.map_or_else(
                || "has no recording on record".to_string(),
                |r| format!("reads recording={} at {} min", r.recording, r.interval_min)
            )
        ),
        Err(_) => s.recording_retries.not_queued(&mac),
    }
}

/// One line per download: the decrypted chunks, so the block CRC can be identified offline
/// (beacon_history.md §7 Q6 layer 2). Appended, like the segment.
fn append_archive_chunks(
    store: &Store,
    mac: &MacAddr,
    at: i64,
    chunks: &[RawChunk],
) -> std::io::Result<()> {
    use std::io::Write as _;
    let path = store.archive_chunks_path(mac);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let chunks: Vec<(u16, String)> = chunks
        .iter()
        .map(|(idx, body)| (*idx, body.iter().map(|b| format!("{b:02x}")).collect()))
        .collect();
    let line = serde_json::json!({ "at": at, "chunks": chunks }).to_string();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{line}")?;
    f.sync_all()
}

/// One JSON object per download, appended — so a segment is never partially overwritten.
fn append_archive_segment(store: &Store, mac: &MacAddr, d: &Download) -> std::io::Result<()> {
    use std::io::Write as _;
    let path = store.archive_path(mac);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let line = serde_json::to_string(d).unwrap_or_else(|_| "{}".to_string());
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{line}")?;
    f.sync_all()
}

/// The BlueZ side of a GATT dial: bluetoothd, the controller, and the device a scan has seen.
async fn bluer_peer(mac: MacAddr, ctx: &DialCtx<'_>) -> Result<BluerPeer> {
    let session = bluer::Session::new().await.map_err(|e| {
        Error::new(
            Kind::ServiceDown,
            Role::Central,
            Component::BluezDbus,
            format!("bluetoothd is not reachable: {e}"),
        )
    })?;
    let adapter = match ctx.adapter_name {
        Some(name) => session.adapter(name),
        None => session.default_adapter().await,
    }
    .map_err(|e| {
        Error::new(
            Kind::AdapterDown,
            Role::Central,
            Component::BluetoothAdapter,
            format!("no usable Bluetooth controller: {e}"),
        )
    })?;
    let device = adapter.device(mac.to_bluer()).map_err(|e| {
        Error::new(
            Kind::BeaconUnreachable,
            Role::Central,
            Component::BeaconGatt,
            format!("{mac} is not known to BlueZ (has it been seen by a scan?): {e}"),
        )
    })?;
    Ok(BluerPeer::new(device))
}

/// Read a beacon's configuration and change none of it — the follow-up to an enrollment while
/// configuring on enroll is off.
///
/// Same slot and same `session_timeout_s` bound as a provisioning dial, but it is **not** a
/// provisioning attempt: it spends no budget and leaves the `provisioning` axis alone, because the
/// beacon is not asked to store anything. A failed read changes nothing on the beacon either, so
/// the last reported configuration is kept rather than cleared.
async fn read_config_dial(mac: MacAddr, ctx: &DialCtx<'_>) -> Result<()> {
    let _claim = AdapterSlot::claim_owned_raw(ctx.slot, mac)?;
    let deadline = Duration::from_secs(ctx.dial_budget.session_timeout_s.max(1));
    let outcome = tokio::time::timeout(deadline, async {
        let peer = bluer_peer(mac, ctx).await?;
        let read = read_config(&peer).await;
        peer.disconnect().await;
        read
    })
    .await
    .unwrap_or_else(|_| {
        Err(Error::new(
            Kind::BeaconUnreachable,
            Role::Central,
            Component::BeaconGatt,
            format!(
                "{mac} did not answer the configuration read within {} s",
                deadline.as_secs()
            ),
        ))
    });

    match &outcome {
        Ok(config) => {
            let reported = ReportedBle {
                config: *config,
                read_at: ctx.host.clock.now_unix(),
                verified: false,
            };
            let mut ledger = ProvisionLedger::load(ctx.store)?;
            ledger.record_ble_config(&mac, reported);
            // The read reached the beacon, so it — not an earlier failed session — says what the
            // beacon holds now.
            ledger.clear_error(&mac);
            ledger.save(ctx.store)?;
            {
                let mut s = write_state(ctx.state);
                s.ble_configs.insert(mac, reported);
                s.failed.remove(&mac);
                s.provision_retries.remove(&mac);
                if let Some(b) = s.beacons.get_mut(&mac) {
                    if b.provisioning != Provisioning::InProgress {
                        // Let the snapshot judge this read: `provisioned` or `differs`.
                        b.provisioning = Provisioning::Pending;
                    }
                }
            }
            eprintln!("control.ble: read the configuration of {mac}: {config:?}");
        }
        Err(e) => eprintln!("control.ble: reading the configuration of {mac} failed: {e}"),
    }
    outcome.map(|_| ())
}

/// Provisioning and the ad hoc override, over BlueZ.
///
/// Not `run_contained`: that wrapper is for the blocking raw-ATT jobs, and this path is async all
/// the way down. It still claims the same shared adapter slot and still bounds itself — the slot is
/// what keeps a dial from racing a phone connection (§0.1), and the deadline is
/// `dial_budget.session_timeout_s`.
async fn provision_dial(
    mac: MacAddr,
    profile: Profile,
    full: bool,
    ctx: &DialCtx<'_>,
) -> Result<()> {
    if profile.is_empty() && !full {
        return Err(Error::new(
            Kind::InvalidArgument,
            Role::Central,
            Component::BeaconGatt,
            "a partial override with no fields would dial the beacon and change nothing",
        ));
    }

    // The budget, checked here as well as at the CLI: a queued job must not outlive the give-up that
    // was recorded while it sat in the queue.
    let mut ledger = ProvisionLedger::load(ctx.store)?;
    if ledger.exhausted(&mac, ctx.dial_budget.per_beacon_attempts) {
        return Err(Error::new(
            Kind::PairingRejected,
            Role::Central,
            Component::BeaconGatt,
            format!(
                "{mac} has spent its {} provisioning attempts; `beacon-provision` resets the budget",
                ctx.dial_budget.per_beacon_attempts
            ),
        ));
    }

    let now = ctx.host.clock.now_unix();
    // A held adapter is this session's failure like any other — recorded, and on the retry ladder
    // for a full session — rather than an error that leaves no trace in the ledger.
    let claim = AdapterSlot::claim_owned_raw(ctx.slot, mac);
    if let Some(b) = write_state(ctx.state).beacons.get_mut(&mac) {
        b.provisioning = Provisioning::InProgress;
    }
    ledger.record_attempt(&mac, now);
    ledger.save(ctx.store)?;

    let deadline = Duration::from_secs(ctx.dial_budget.session_timeout_s.max(1));
    let outcome = match &claim {
        Err(busy) => Err(busy.clone()),
        Ok(_) => tokio::time::timeout(deadline, async {
            let peer = bluer_peer(mac, ctx).await?;
            let written = provision(&peer, &profile, full).await;
            peer.disconnect().await;
            written
        })
        .await
        .unwrap_or_else(|_| {
            Err(Error::new(
                Kind::BeaconUnreachable,
                Role::Central,
                Component::BeaconGatt,
                format!(
                    "{mac} did not complete provisioning within {} s",
                    deadline.as_secs()
                ),
            ))
        }),
    };

    let mut ledger = ProvisionLedger::load(ctx.store)?;
    match &outcome {
        Ok(written) => {
            if full {
                ledger.record_success(&mac, now);
            } else {
                ledger.clear_error(&mac);
            }
            // `provision` only returns Ok once the read-back matched what it wrote.
            let reported = written.read_back.map(|config| ReportedBle {
                config,
                read_at: ctx.host.clock.now_unix(),
                verified: true,
            });
            if let Some(reported) = reported {
                ledger.record_ble_config(&mac, reported);
            }
            {
                let mut s = write_state(ctx.state);
                if let Some(b) = s.beacons.get_mut(&mac) {
                    b.provisioning = Provisioning::Provisioned;
                    b.last_error = None;
                    b.gave_up_at = None;
                }
                s.failed.remove(&mac);
                s.provision_retries.remove(&mac);
                if full {
                    s.provisioned.insert(mac);
                }
                if let Some(reported) = reported {
                    s.ble_configs.insert(mac, reported);
                }
            }
            eprintln!(
                "control.ble: provisioned {mac}: wrote {:?}{}, read back {:?}",
                written.written,
                if written.skipped.is_empty() {
                    String::new()
                } else {
                    format!(", skipped {:?}", written.skipped)
                },
                written.read_back
            );
        }
        Err(e) => {
            let failed_at = ctx.host.clock.now_unix();
            let budget = ctx.dial_budget.per_beacon_attempts;
            // A full session goes on the retry ladder (beacon_history.md §8, amended 2026-10-09):
            // the Hub dials again on a later advertisement, then gives up. A partial override is
            // the operator's ad hoc write, and is not repeated behind their back.
            let after = if full {
                ledger.record_provision_failure(&mac, now, failed_at, e, budget)
            } else {
                // The budget is spent only when the beacon actually refused us; an absent
                // controller is this Hub's problem and must not burn a foreign beacon's allowance.
                let spends = matches!(e.kind(), Kind::PairingRejected | Kind::Transport);
                if spends && ledger.exhausted(&mac, budget) {
                    ledger.record_give_up(&mac, now, e.message());
                    AfterProvisionFailure::GaveUp
                } else {
                    ledger.record_failure(&mac, now, e.message());
                    AfterProvisionFailure::Stays
                }
            };
            let attempts = ledger.get(&mac).map_or(0, |r| r.attempts);
            match after {
                AfterProvisionFailure::RetryAt(at) => eprintln!(
                    "control.ble: provisioning {mac} failed (attempt {attempts}); retry once the \
                     beacon is heard again, not before {} s from now",
                    at.saturating_sub(failed_at)
                ),
                AfterProvisionFailure::GaveUp => eprintln!(
                    "control.ble: provisioning {mac} failed (attempt {attempts}); giving up until \
                     an operator acts"
                ),
                AfterProvisionFailure::Stays => {}
            }
            // A transport fault or a read-back mismatch may have left the beacon half-written, so
            // its last verified configuration no longer describes it. A refused password or an
            // unreachable beacon changed nothing, and keeps what was verified.
            if e.kind() == Kind::Transport {
                ledger.clear_ble_config(&mac);
                write_state(ctx.state).ble_configs.remove(&mac);
            }
            // Every failed session is `failed`, not only one that spent the budget: the operator
            // asked for a configuration and did not get one (central.md §5, amended 2026-09-23).
            // The budget still decides whether an unattended retry may dial again.
            // It stays `failed` while a retry waits: that is what the beacon is until the retry
            // starts, which publishes `provisioning` again.
            let mut s = write_state(ctx.state);
            s.failed.insert(mac);
            match after {
                AfterProvisionFailure::RetryAt(at) => {
                    s.provision_retries.insert(mac, at);
                }
                AfterProvisionFailure::GaveUp => {
                    s.provision_retries.remove(&mac);
                    s.provisioned.remove(&mac);
                }
                // Only a full session ends a waiting retry; an override failing leaves it.
                AfterProvisionFailure::Stays if full => {
                    s.provision_retries.remove(&mac);
                }
                AfterProvisionFailure::Stays => {}
            }
            if let Some(b) = s.beacons.get_mut(&mac) {
                b.provision_attempts = attempts;
                b.provisioning = Provisioning::Failed;
                b.last_error = Some(e.message().to_string());
                if after == AfterProvisionFailure::GaveUp {
                    b.gave_up_at = Some(now);
                }
            }
        }
    }
    ledger.save(ctx.store)?;
    outcome.map(|_| ())
}

/// Everything [`serve_fb00`] needs.
struct Fb00Params {
    host: Host,
    session: Arc<SessionSlot>,
    slot: Arc<AdapterSlot>,
    state: Arc<RwLock<CentralState>>,
    store: Store,
    cfg: crate::config::PeripheralConfig,
    adapter_name: Option<String>,
    hub_mac: MacAddr,
    advertising: Arc<AtomicBool>,
    connected_peer: Arc<Mutex<Option<PeerAddr>>>,
    events: EventBus,
    adv_rx: watch::Receiver<bool>,
    stop: watch::Receiver<bool>,
    /// The central role's dial queue, when it runs — FB0E's enrollment follow-up goes here.
    dial_tx: Option<mpsc::Sender<DialJob>>,
}

/// Register the FB00 GATT application, advertise it, and track connections.
///
/// Three behaviours are contract rather than plumbing:
///
/// - **An absent adapter is degraded-green.** Log once, keep retrying, never exit, never restart the
///   Bluetooth stack. A Hub with no controller must boot green.
/// - **A connection is a phone only if it is not our own dial.** BlueZ reports both the same way, so
///   the shared adapter slot is the only thing that can tell them apart — without it a provisioning
///   burst looks like a phone connecting over and over.
/// - **Advertising is separable from serving.** `phone-advertise-stop` takes FB00 off the air
///   without tearing down the component, so the handle is dropped and re-acquired rather than the
///   task being killed.
async fn serve_fb00(mut p: Fb00Params) {
    let mut announced_absent = false;
    let retry = Duration::from_secs(10);

    loop {
        if *p.stop.borrow() {
            return;
        }

        let adapter = match open_adapter(p.adapter_name.as_deref()).await {
            Some(a) => a,
            None => {
                if !announced_absent {
                    eprintln!(
                        "control.ble: no usable Bluetooth controller for the FB00 service; the \
                         peripheral role is idle (degraded-green) and will retry every {}s",
                        retry.as_secs()
                    );
                    announced_absent = true;
                }
                p.advertising.store(false, Ordering::SeqCst);
                tokio::select! {
                    _ = tokio::time::sleep(retry) => {}
                    _ = p.stop.changed() => {}
                }
                continue;
            }
        };
        announced_absent = false;

        // The real adapter address beats whatever the caller guessed: the QR payload and FB07 both
        // carry it, and a wrong one sends a phone to a Hub that is not there.
        let hub_mac = adapter
            .address()
            .await
            .map(MacAddr::from)
            .unwrap_or(p.hub_mac);

        let ctx = Arc::new(
            GattCtx::new(
                p.host.clone(),
                p.session.clone(),
                p.slot.clone(),
                p.state.clone(),
                p.cfg.clone(),
                hub_mac,
                p.events.clone(),
            )
            .with_dial(p.dial_tx.clone()),
        );

        let _app = match adapter
            .serve_gatt_application(gatt::app::application(ctx.clone()))
            .await
        {
            Ok(h) => h,
            Err(e) => {
                eprintln!(
                    "control.ble: cannot register the FB00 GATT application: {e}; retrying in {}s",
                    retry.as_secs()
                );
                tokio::select! {
                    _ = tokio::time::sleep(retry) => {}
                    _ = p.stop.changed() => {}
                }
                continue;
            }
        };
        eprintln!("control.ble: FB00 registered on {hub_mac}");

        // Advertising is a separate handle so it can go up and down under the running server.
        let alias = advertise::alias(p.cfg.advertise_name.as_deref(), &p.host.device.hostname());
        let mut adv_handle = None;
        if *p.adv_rx.borrow() {
            match advertise::start(&adapter, &alias).await {
                Ok(h) => {
                    adv_handle = Some(h);
                    p.advertising.store(true, Ordering::SeqCst);
                    eprintln!("control.ble: advertising FB00 as {alias:?}");
                }
                Err(e) => {
                    // Some controllers refuse an advertisement the kernel considers malformed even
                    // when every parameter is legal. Serving without advertising is still useful —
                    // a central that already knows the address can connect — so this is a warning.
                    eprintln!("control.ble: {e}");
                    p.advertising.store(false, Ordering::SeqCst);
                }
            }
        }

        let mut events = match adapter.events().await {
            Ok(e) => e,
            Err(e) => {
                eprintln!("control.ble: cannot watch adapter events: {e}");
                drop(adv_handle);
                continue;
            }
        };

        // Serve until something asks us to stop or the adapter goes away. A bluetoothd restart by
        // the recovery ladder drops our GATT application and advertisement without necessarily
        // ending the event stream, so it is watched for explicitly.
        let mut restarts = p.slot.subscribe_stack_restarts();
        loop {
            tokio::select! {
                _ = restarts.changed() => {
                    eprintln!("control.ble: bluetoothd was restarted; re-registering FB00");
                    break;
                }
                _ = p.stop.changed() => {
                    if *p.stop.borrow() {
                        p.advertising.store(false, Ordering::SeqCst);
                        return;
                    }
                }
                changed = p.adv_rx.changed() => {
                    if changed.is_err() { return; }
                    let want = *p.adv_rx.borrow();
                    if want && adv_handle.is_none() {
                        match advertise::start(&adapter, &alias).await {
                            Ok(h) => { adv_handle = Some(h); p.advertising.store(true, Ordering::SeqCst); }
                            Err(e) => eprintln!("control.ble: {e}"),
                        }
                    } else if !want && adv_handle.is_some() {
                        // Dropping the handle is how BlueZ is told to stop; the service stays served.
                        adv_handle = None;
                        p.advertising.store(false, Ordering::SeqCst);
                    }
                }
                event = events.next() => {
                    let Some(event) = event else { break };
                    match event {
                        bluer::AdapterEvent::DeviceAdded(addr) => {
                            let mac = MacAddr::from(addr);
                            // §0.1: only a connection that is NOT our own dial is a phone.
                            if !gatt::app::is_client_connection(&p.slot, &mac) {
                                continue;
                            }
                            let connected = match adapter.device(addr) {
                                Ok(d) => d.is_connected().await.unwrap_or(false),
                                Err(_) => false,
                            };
                            if connected {
                                let mut held = p.connected_peer.lock().await;
                                if *held != Some(addr.0) {
                                    *held = Some(addr.0);
                                    eprintln!("control.ble: a central connected: {mac}");
                                    p.events.emit(PeripheralEvent::ClientConnected {
                                        addr: mac.to_string(),
                                    });
                                }
                            }
                        }
                        bluer::AdapterEvent::DeviceRemoved(addr) => {
                            let mut held = p.connected_peer.lock().await;
                            if *held == Some(addr.0) {
                                *held = None;
                                // Buffers and authentication die with the connection.
                                ctx.on_disconnect(addr.0);
                                let mac = MacAddr::from(addr);
                                eprintln!("control.ble: the central disconnected: {mac}");
                                p.events.emit(PeripheralEvent::ClientDisconnected {
                                    addr: mac.to_string(),
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // The adapter went away under us. Report degraded-green and re-enter the outer loop rather
        // than exiting: nothing else would bring the FB00 service back.
        p.advertising.store(false, Ordering::SeqCst);
        drop(adv_handle);
        let _ = &p.store;
        eprintln!("control.ble: the FB00 adapter event stream ended; re-registering");
    }
}

/// Open the configured adapter, or `None` when there is not a usable one.
///
/// `session.adapter(name)` is lazy in bluer — it hands back a handle without checking the adapter
/// exists — so presence is established by actually reading a property off it.
async fn open_adapter(name: Option<&str>) -> Option<bluer::Adapter> {
    let session = bluer::Session::new().await.ok()?;
    let adapter = match name {
        Some(n) => session.adapter(n).ok()?,
        None => session.default_adapter().await.ok()?,
    };
    adapter.address().await.ok()?;
    let _ = adapter.set_powered(true).await;
    Some(adapter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{En12830Config, EyeSection};
    use crate::host::{ConfigStore, MemoryConfigStore, NullPublisher, NullReadingSink};
    use crate::mode::Mode;

    fn cfg(mode: Mode) -> BleConfig {
        BleConfig::from_device_config_yaml(&format!("ble:\n  mode: {}\n", mode.as_str()))
            .expect("parses")
    }

    fn host_with(section: EyeSection) -> (Host, Arc<MemoryConfigStore>) {
        let config = Arc::new(MemoryConfigStore::with_section(section));
        let host = Host {
            config: config.clone(),
            publisher: Arc::new(NullPublisher),
            readings: Arc::new(NullReadingSink),
            ..Host::in_memory()
        };
        (host, config)
    }

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let base = std::env::temp_dir().join(format!(
                "control-ble-dial-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&base);
            TempDir(base)
        }
        fn store(&self) -> Store {
            Store::new(&self.0)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn store() -> Store {
        Store::new(std::env::temp_dir().join(format!(
            "control-ble-tasks-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        )))
    }

    fn hub_mac() -> MacAddr {
        MacAddr::parse("B8:27:EB:11:22:33").expect("mac")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn only_the_roles_the_mode_allows_get_handles() {
        let (host, _) = host_with(EyeSection::default());
        let h = tokio::runtime::Handle::current();

        let rt = spawn(&h, &cfg(Mode::Off), host.clone(), store(), hub_mac());
        assert!(rt.handles.central.is_none());
        assert!(rt.handles.peripheral.is_none());
        rt.shutdown().await;

        let rt = spawn(&h, &cfg(Mode::Central), host.clone(), store(), hub_mac());
        assert!(rt.handles.central.is_some());
        assert!(
            rt.handles.peripheral.is_none(),
            "a central-only Hub never registers FB00"
        );
        rt.shutdown().await;

        let rt = spawn(&h, &cfg(Mode::Peripheral), host.clone(), store(), hub_mac());
        assert!(rt.handles.central.is_none(), "and never opens a scan");
        assert!(rt.handles.peripheral.is_some());
        rt.shutdown().await;

        let rt = spawn(&h, &cfg(Mode::Dual), host, store(), hub_mac());
        assert!(rt.handles.central.is_some());
        assert!(rt.handles.peripheral.is_some());
        rt.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ensure_scanning_from_the_enrollment_seam_starts_a_stopped_loop() {
        // The seam that makes §9.3's auto-enable real rather than theatre.
        let (host, config) = host_with(EyeSection {
            enabled: false,
            ..EyeSection::default()
        });
        let rt = spawn(
            &tokio::runtime::Handle::current(),
            &cfg(Mode::Central),
            host.clone(),
            store(),
            hub_mac(),
        );
        let central = rt.handles.central.clone().expect("central");

        // A live task exists, so the seam can start something — unlike `NoopSubsystem`, which is
        // what a one-shot CLI has and which reports `unavailable`.
        assert!(
            SubsystemControl::ensure_scanning(&central).expect("ok"),
            "a live scan task is what makes the auto-enable claim true"
        );

        // Now enrol through the seam with the flag off: the flip and the loop kick are one path.
        let host2 = Host {
            subsystem: Arc::new(central.clone()),
            ..host.clone()
        };
        let out = crate::central::enroll::add(
            &host2,
            crate::host::Origin::Ble,
            "AA:BB:CC:DD:EE:FF",
            Some("Freezer 1"),
        )
        .expect("enrolls");
        assert_eq!(
            out.monitoring,
            crate::central::enroll::Monitoring::EnabledByThisCall,
            "with a live loop the outcome is the strong one, not `unavailable`"
        );
        assert!(config.load().expect("load").enabled);
        rt.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_removal_through_the_enrollment_seam_forgets_the_beacon_completely() {
        // A removed beacon keeps nothing — budget, `provisioned`, reported configuration — so a
        // re-add starts from `pending` and is read afresh (central.md §5, amended 2026-09-23).
        let t = TempDir::new("forget");
        let (host, _) = host_with(EyeSection::default());
        let rt = spawn(
            &tokio::runtime::Handle::current(),
            &cfg(Mode::Central),
            host.clone(),
            t.store(),
            hub_mac(),
        );
        let central = rt.handles.central.clone().expect("central");
        let mac = MacAddr::parse("AA:BB:CC:DD:EE:FF").expect("mac");
        let reported = ReportedBle {
            config: crate::central::provision::BleConfig {
                advertising_interval_ms: 10_000,
                tx_power_dbm: 8,
                protocol_type: 2,
                active_sensors: None,
            },
            read_at: 1_790_163_174,
            verified: true,
        };
        let mut ledger = ProvisionLedger::load(&t.store()).expect("load");
        ledger.record_attempt(&mac, 1);
        ledger.record_success(&mac, 1);
        ledger.record_ble_config(&mac, reported);
        ledger.save(&t.store()).expect("save");
        write_state(&central.state)
            .ble_configs
            .insert(mac, reported);

        let host2 = Host {
            subsystem: Arc::new(central.clone()),
            ..host.clone()
        };
        crate::central::enroll::add(&host2, crate::host::Origin::Ble, "AA:BB:CC:DD:EE:FF", None)
            .expect("enrolls");
        let out =
            crate::central::enroll::remove(&host2, crate::host::Origin::Ble, "aa:bb:cc:dd:ee:ff")
                .expect("removes");
        assert!(out.removed);
        assert!(
            ProvisionLedger::load(&t.store())
                .expect("load")
                .get(&mac)
                .is_none(),
            "the ledger record is gone"
        );
        assert!(
            !crate::central::state::read_state(&central.state)
                .ble_configs
                .contains_key(&mac),
            "and so is the reported configuration"
        );
        rt.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stopped_runtime_reports_nothing_scanning() {
        let (host, _) = host_with(EyeSection::default());
        let rt = spawn(
            &tokio::runtime::Handle::current(),
            &cfg(Mode::Central),
            host,
            store(),
            hub_mac(),
        );
        let central = rt.handles.central.clone().expect("central");
        rt.shutdown().await;
        assert!(!central.is_scanning());
        assert!(
            !SubsystemControl::ensure_scanning(&central).expect("ok"),
            "after shutdown there is no task to start, and the seam says so"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_known_beacon_allowlist_is_daemon_state_installed_only_through_the_handle() {
        let (host, config) = host_with(EyeSection::default());
        let rt = spawn(
            &tokio::runtime::Handle::current(),
            &cfg(Mode::Central),
            host,
            store(),
            hub_mac(),
        );
        let central = rt.handles.central.clone().expect("central");
        let a = MacAddr::parse("AA:BB:CC:DD:EE:FF").expect("mac");
        central.set_known_beacons(BTreeSet::from([a]));
        assert!(central.known_beacons().contains(&a));
        // …and nothing about it reached the config file: a beacon enrolled on another Hub must
        // become audible here without this Hub claiming ownership of it.
        assert_eq!(config.write_count(), 0);
        assert!(config.load().expect("load").tags.is_empty());
        rt.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_dial_queue_accepts_jobs_and_bounds_its_backlog() {
        let (host, _) = host_with(EyeSection::default());
        let rt = spawn(
            &tokio::runtime::Handle::current(),
            &cfg(Mode::Central),
            host,
            store(),
            hub_mac(),
        );
        let central = rt.handles.central.clone().expect("central");
        let mac = MacAddr::parse("AA:BB:CC:DD:EE:FF").expect("mac");
        // A Provision job is refused by the worker (no GATT peer in this host) but must still be
        // *accepted* by the queue — the refusal is logged per beacon, not at submit time.
        central
            .queue(DialJob {
                mac,
                kind: DialKind::Provision,
            })
            .expect("queued");
        rt.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_peripheral_housekeeping_reaps_an_idle_session() {
        let clock = Arc::new(crate::host::FakeClock::at(1_000));
        let config = Arc::new(MemoryConfigStore::default());
        let host = Host {
            config,
            clock: clock.clone(),
            ..Host::in_memory()
        };
        let rt = spawn(
            &tokio::runtime::Handle::current(),
            &cfg(Mode::Peripheral),
            host,
            store(),
            hub_mac(),
        );
        let p = rt.handles.peripheral.clone().expect("peripheral");
        p.session.start(clock.as_ref(), 300, &hub_mac(), "hub");
        assert!(p.session.is_live(clock.as_ref()));

        // Move the clock past the idle timeout and let the tick run.
        clock.set(1_000 + 301);
        for _ in 0..100 {
            if !p.session.is_live(clock.as_ref()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !p.session.is_live(clock.as_ref()),
            "the housekeeping tick must reap it"
        );
        rt.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn advertising_start_and_stop_report_whether_they_changed_anything() {
        let (host, _) = host_with(EyeSection::default());
        let rt = spawn(
            &tokio::runtime::Handle::current(),
            &cfg(Mode::Peripheral),
            host,
            store(),
            hub_mac(),
        );
        let p = rt.handles.peripheral.clone().expect("peripheral");
        assert!(!p.is_advertising());
        assert!(p.start_advertising(), "it started something");
        assert!(!p.start_advertising(), "already advertising");
        assert!(p.is_advertising());
        assert!(p.stop_advertising());
        assert!(!p.stop_advertising());
        rt.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_capability_flag_is_read_from_config_at_spawn() {
        let (host, _) = host_with(EyeSection {
            en12830: En12830Config {
                enabled: false,
                protocol_failure_alarm_after: 3,
            },
            ..EyeSection::default()
        });
        let rt = spawn(
            &tokio::runtime::Handle::current(),
            &cfg(Mode::Central),
            host,
            store(),
            hub_mac(),
        );
        let central = rt.handles.central.clone().expect("central");
        assert!(
            !central.capability.enabled(),
            "a Hub that shipped with the archive off must start with it off"
        );
        rt.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_is_bounded_and_idempotent_in_effect() {
        // A shutdown that can hang is a device that cannot be updated.
        let (host, _) = host_with(EyeSection::default());
        let rt = spawn(
            &tokio::runtime::Handle::current(),
            &cfg(Mode::Dual),
            host,
            store(),
            hub_mac(),
        );
        let central = rt.handles.central.clone().expect("central");
        let p = rt.handles.peripheral.clone().expect("peripheral");
        let started = std::time::Instant::now();
        rt.shutdown().await;
        assert!(
            started.elapsed() < SHUTDOWN_GRACE * 3,
            "shutdown took {:?}",
            started.elapsed()
        );
        assert!(!central.is_scanning());
        assert!(!p.is_advertising());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_dial_kind_reaches_a_real_operation_rather_than_a_stub() {
        // The gap this closes: the worker used to `Recorder::connect(..).map(|_| ())` for the three
        // archive kinds and refuse Provision outright, so every one of them "succeeded" or failed
        // for the wrong reason without doing anything. On a host with no beacon in range each kind
        // must now fail from its *own* operation, not from a placeholder.
        let (host, _) = host_with(EyeSection::default());
        let mac = MacAddr::parse("AA:BB:CC:DD:EE:FF").expect("mac");
        let t = TempDir::new("dial-kinds");
        let ctx_store = t.store();
        let slot = Arc::new(AdapterSlot::new());
        let capability = Arc::new(En12830Capability::with_deadline(
            &En12830Config {
                enabled: true,
                protocol_failure_alarm_after: 3,
            },
            Duration::from_millis(400),
        ));
        let state = Arc::new(RwLock::new(CentralState::default()));
        let budget = DialBudget {
            session_timeout_s: 1,
            ..DialBudget::default()
        };
        let profile = BeaconProfile::default();
        let ctx = DialCtx {
            host: &host,
            slot: &slot,
            capability: &capability,
            state: &state,
            store: &ctx_store,
            // A name no adapter has, so nothing here can touch the developer's real controller.
            adapter_name: Some("hci-does-not-exist"),
            dial_budget: &budget,
            profile: &profile,
            requeue: None,
        };

        // Hold the adapter, so every kind is refused at the *claim* instead of going on the air.
        // Letting these run would dial the developer's real controller: the raw ATT bearer binds to
        // the any-adapter address, and a blocking L2CAP connect cannot be cancelled — tokio's
        // runtime drop then waits on it, which is a minute-long "hang" rather than a test.
        let _held = slot
            .claim(MacAddr::parse("00:11:22:33:44:55").expect("mac"))
            .expect("hold the adapter");

        for kind in [
            DialKind::Detect,
            DialKind::Recording(5),
            DialKind::History(HistoryRequest::new(None, None, None, 0).expect("request")),
            DialKind::Provision,
            DialKind::ConfigOverride(Profile::partial(None, Some(4), None)),
            DialKind::ReadConfig,
        ] {
            let job = DialJob {
                mac,
                kind: kind.clone(),
            };
            let err = match run_dial(&job, &ctx).await {
                Err(e) => e,
                Ok(()) => panic!("{kind:?} cannot succeed while the adapter is held"),
            };
            assert_ne!(
                err.kind(),
                Kind::Phase2NotImplemented,
                "{kind:?} must reach a real operation, not a placeholder: {}",
                err.message()
            );
            // Reaching the shared-adapter check is the proof the kind is wired to a real dial: a
            // placeholder would have refused before ever asking for the adapter.
            assert!(
                matches!(
                    err.kind(),
                    Kind::AlreadyExists
                        | Kind::BeaconUnreachable
                        | Kind::AdapterDown
                        | Kind::Transport
                        | Kind::Timeout
                        | Kind::ServiceDown
                        | Kind::PairingRejected
                ),
                "{kind:?} failed for the wrong reason: {} / {}",
                err.kind().as_str(),
                err.message()
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dial_worker_pauses_and_resumes_scan_around_a_dial() {
        // The dial worker must pause discovery for a dial and resume it on every path, including a
        // dial that fails immediately (issue #79). An empty ConfigOverride is refused inside
        // provision_dial before it ever claims the adapter or touches BlueZ, so this exercises the
        // worker's ScanPauseGuard without going on the air.
        let signals = LoopSignals::default();
        signals
            .scanning
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let observe = signals.clone();

        let t = TempDir::new("worker-scan-pause");
        let worker = DialWorker {
            host: Host::in_memory(),
            slot: Arc::new(AdapterSlot::new()),
            capability: Arc::new(En12830Capability::new(&EyeSection::default().en12830)),
            state: Arc::new(RwLock::new(CentralState::default())),
            store: t.store(),
            adapter_name: None,
            dial_budget: DialBudget::default(),
            profile: BeaconProfile::default(),
            scan_signals: signals,
            requeue: None,
        };

        let (tx, rx) = mpsc::channel::<DialJob>(4);
        tx.send(DialJob {
            mac: MacAddr::parse("7C:D9:F4:10:A8:E2").expect("mac"),
            kind: DialKind::ConfigOverride(Profile::partial(None, None, None)),
        })
        .await
        .expect("queue the job");
        drop(tx); // close the channel so the worker returns after this one job

        dial_worker(rx, worker).await;

        assert!(
            !observe.pause.load(std::sync::atomic::Ordering::SeqCst),
            "the worker must clear the scan pause after a dial (even a failing one)"
        );
        assert!(
            observe.scanning.load(std::sync::atomic::Ordering::SeqCst),
            "the guard must not disturb the configured scanning intent"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_detect_publishes_its_result_even_when_it_could_not_tell() {
        // beacon-detect must publish an `eye/detect` either way, and a dial it could not complete
        // must leave the tri-state at `None` — never `false`, which would read as "standard beacon".
        let publisher = Arc::new(crate::host::RecordingPublisher::default());
        let config = Arc::new(MemoryConfigStore::default());
        let host = Host {
            config,
            publisher: publisher.clone(),
            ..Host::in_memory()
        };
        let mac = MacAddr::parse("AA:BB:CC:DD:EE:FF").expect("mac");
        let t = TempDir::new("detect");
        let store = t.store();
        let slot = Arc::new(AdapterSlot::new());
        let capability = Arc::new(En12830Capability::with_deadline(
            &En12830Config {
                enabled: true,
                protocol_failure_alarm_after: 3,
            },
            Duration::from_millis(400),
        ));
        let state = Arc::new(RwLock::new(CentralState::default()));
        let budget = DialBudget::default();
        let profile = BeaconProfile::default();
        let ctx = DialCtx {
            host: &host,
            slot: &slot,
            capability: &capability,
            state: &state,
            store: &store,
            adapter_name: Some("hci-does-not-exist"),
            dial_budget: &budget,
            profile: &profile,
            requeue: None,
        };
        // Hold the adapter so the probe cannot reach the air. The property under test is that a
        // detect which could not complete still publishes, and still leaves the tri-state alone.
        let _held = slot
            .claim(MacAddr::parse("00:11:22:33:44:55").expect("mac"))
            .expect("hold the adapter");
        let _ = detect(mac, &ctx).await;

        let published = publisher.detects();
        assert_eq!(published.len(), 1, "an eye/detect is published either way");
        assert_eq!(published[0].mac, "AA:BB:CC:DD:EE:FF");
        assert_eq!(
            published[0].is_en12830, None,
            "could-not-tell must stay null, never false"
        );
        assert!(
            published[0].status == "unreachable" || published[0].status == "error",
            "status was {:?}",
            published[0].status
        );
        assert!(!published[0].timestamp.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_recording_interval_outside_the_set_never_reaches_the_radio() {
        let (host, _) = host_with(EyeSection::default());
        let mac = MacAddr::parse("AA:BB:CC:DD:EE:FF").expect("mac");
        let t = TempDir::new("interval");
        let store = t.store();
        let slot = Arc::new(AdapterSlot::new());
        let capability = Arc::new(En12830Capability::new(&En12830Config {
            enabled: true,
            protocol_failure_alarm_after: 3,
        }));
        let state = Arc::new(RwLock::new(CentralState::default()));
        let budget = DialBudget::default();
        let profile = BeaconProfile::default();
        let ctx = DialCtx {
            host: &host,
            slot: &slot,
            capability: &capability,
            state: &state,
            store: &store,
            adapter_name: Some("hci-does-not-exist"),
            dial_budget: &budget,
            profile: &profile,
            requeue: None,
        };
        let err = recording(mac, 7, &ctx)
            .await
            .expect_err("7 is not supported");
        assert_eq!(err.kind(), Kind::InvalidArgument);
        assert_eq!(
            slot.busy_with(),
            None,
            "and it was refused before the adapter was claimed"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_exhausted_budget_stops_a_queued_provision_before_it_dials() {
        // A job can sit in the queue while the budget is spent by earlier attempts; re-checking here
        // is what stops a queued dial outliving its own give-up.
        let (host, _) = host_with(EyeSection::default());
        let mac = MacAddr::parse("AA:BB:CC:DD:EE:FF").expect("mac");
        let t = TempDir::new("budget");
        let store = t.store();
        let mut ledger = ProvisionLedger::default();
        ledger.record_give_up(&mac, 1_000, "refused five times");
        ledger.save(&store).expect("saves");

        let slot = Arc::new(AdapterSlot::new());
        let capability = Arc::new(En12830Capability::new(&En12830Config {
            enabled: true,
            protocol_failure_alarm_after: 3,
        }));
        let state = Arc::new(RwLock::new(CentralState::default()));
        let budget = DialBudget::default();
        let profile = BeaconProfile::default();
        let ctx = DialCtx {
            host: &host,
            slot: &slot,
            capability: &capability,
            state: &state,
            store: &store,
            adapter_name: Some("hci-does-not-exist"),
            dial_budget: &budget,
            profile: &profile,
            requeue: None,
        };
        let err = provision_dial(mac, Profile::from_config(&profile), true, &ctx)
            .await
            .expect_err("the budget is spent");
        assert_eq!(err.kind(), Kind::PairingRejected);
        assert!(
            err.message().contains("beacon-provision"),
            "{}",
            err.message()
        );
        assert_eq!(slot.busy_with(), None, "and the adapter was never claimed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_archive_segment_is_appended_rather_than_overwritten() {
        let t = TempDir::new("archive");
        let store = t.store();
        let mac = MacAddr::parse("AA:BB:CC:DD:EE:FF").expect("mac");
        let one = Download {
            records: vec![(1_000, 4.5)],
            restart_ok: true,
            chunks_read: 2,
            sessions: 1,
            rejected_chunks: 0,
            future_dropped: 0,
            chunks: vec![],
        };
        let two = Download {
            records: vec![(1_060, 4.6)],
            restart_ok: true,
            chunks_read: 2,
            sessions: 1,
            rejected_chunks: 0,
            future_dropped: 0,
            chunks: vec![],
        };
        append_archive_segment(&store, &mac, &one).expect("first");
        append_archive_segment(&store, &mac, &two).expect("second");
        let raw = std::fs::read_to_string(store.archive_path(&mac)).expect("read");
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "a second download must not clobber the first"
        );
        for l in lines {
            serde_json::from_str::<serde_json::Value>(l).expect("one JSON object per line");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_peripheral_role_actually_starts_the_fb00_server_task() {
        // The gap this closes: `spawn` used to start only the housekeeping tick, so the FB00
        // handlers were tested code that nothing served and no phone could ever reach.
        let (host, _) = host_with(EyeSection::default());
        let rt = spawn(
            &tokio::runtime::Handle::current(),
            &cfg(Mode::Peripheral),
            host,
            store(),
            hub_mac(),
        );
        // Two tasks now: the housekeeping tick and the FB00 server.
        assert_eq!(
            rt.join.len(),
            2,
            "the peripheral role starts the housekeeping tick AND the FB00 server"
        );
        rt.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn both_roles_share_one_adapter_slot() {
        // §0.1: two slots would mean the GATT server could not tell a phone from our own dial.
        let (host, _) = host_with(EyeSection::default());
        let rt = spawn(
            &tokio::runtime::Handle::current(),
            &cfg(Mode::Dual),
            host,
            store(),
            hub_mac(),
        );
        let central = rt.handles.central.clone().expect("central");
        let mac = MacAddr::parse("AA:BB:CC:DD:EE:FF").expect("mac");
        // Claiming through the central handle must be visible to whatever the peripheral side sees,
        // which is the same Arc.
        let claim = central.slot.claim(mac).expect("claim");
        assert!(central.slot.is_our_dial(&mac));
        assert!(
            !crate::peripheral::gatt::app::is_client_connection(&central.slot, &mac),
            "the FB00 server must not mistake our own dial for a phone"
        );
        drop(claim);
        assert!(crate::peripheral::gatt::app::is_client_connection(
            &central.slot,
            &mac
        ));
        rt.shutdown().await;
    }

    fn history_json(publisher: &crate::host::RecordingPublisher) -> Vec<serde_json::Value> {
        publisher
            .history()
            .iter()
            .map(|m| serde_json::to_value(m).expect("serialise"))
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_busy_download_climbs_the_retry_ladder_and_is_requeued_on_an_advertisement() {
        let publisher = Arc::new(crate::host::RecordingPublisher::default());
        let clock = Arc::new(crate::host::FakeClock::at(1_700_000_000));
        let host = Host {
            publisher: publisher.clone(),
            clock: clock.clone(),
            ..Host::in_memory()
        };
        let slot = Arc::new(AdapterSlot::new());
        let state = Arc::new(RwLock::new(CentralState::default()));
        let mac = MacAddr::parse("7C:D9:F4:13:10:DE").expect("mac");
        let t = TempDir::new("history-ladder");
        let worker = DialWorker {
            host: host.clone(),
            slot: slot.clone(),
            capability: Arc::new(En12830Capability::new(&EyeSection::default().en12830)),
            state: state.clone(),
            store: t.store(),
            adapter_name: Some("hci-does-not-exist".into()),
            dial_budget: DialBudget::default(),
            profile: BeaconProfile::default(),
            scan_signals: LoopSignals::default(),
            requeue: None,
        };
        // A held adapter refuses the dial at the claim: the `busy` failure, without going on air.
        let _held = slot
            .claim(MacAddr::parse("00:11:22:33:44:55").expect("mac"))
            .expect("hold the adapter");
        let request = HistoryRequest::new(Some(1_000), Some(2_000), Some("job-1".into()), 0)
            .expect("request");
        let (tx, rx) = mpsc::channel::<DialJob>(4);
        tx.send(DialJob {
            mac,
            kind: DialKind::History(request),
        })
        .await
        .expect("queue");
        drop(tx);
        dial_worker(rx, worker).await;

        let sent = history_json(&publisher);
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0]["state"], "requested");
        assert_eq!(sent[0]["job"], "job-1", "a retry keeps the job id");
        assert_eq!(sent[0]["retry"], 1);
        assert_eq!(sent[0]["reason"], "busy");
        assert_eq!(
            (sent[0]["from"].clone(), sent[0]["to"].clone()),
            (1_000.into(), 2_000.into())
        );

        // Heard again too early: nothing is queued.
        let (tx, mut rx) = mpsc::channel::<DialJob>(1);
        clock.advance_monotonic(299);
        requeue_due_history(&host, &state, &tx, mac);
        assert!(rx.try_recv().is_err(), "the 5-minute floor holds");

        // The floor has passed, but the queue is full: the retry waits for the next advertisement.
        clock.advance_monotonic(1);
        tx.try_send(DialJob {
            mac,
            kind: DialKind::Detect,
        })
        .expect("fill the queue");
        requeue_due_history(&host, &state, &tx, mac);
        assert_eq!(
            history_json(&publisher).len(),
            1,
            "nothing announced for a retry not queued"
        );
        assert!(matches!(
            rx.try_recv().map(|j| j.kind),
            Ok(DialKind::Detect)
        ));

        requeue_due_history(&host, &state, &tx, mac);
        match rx.try_recv().map(|j| j.kind) {
            Ok(DialKind::History(r)) => assert_eq!((r.job.as_str(), r.retry), ("job-1", 1)),
            other => panic!("expected the retried download, got {other:?}"),
        }
        let sent = history_json(&publisher);
        assert_eq!(sent.len(), 2);
        assert_eq!(
            (sent[1]["state"].as_str(), sent[1]["retry"].as_u64()),
            (Some("requested"), Some(1))
        );
        assert!(crate::central::state::read_state(&state)
            .history_retries
            .pending(&mac)
            .is_none());
    }

    #[test]
    fn a_protocol_failure_is_final_and_reported_failed() {
        let publisher = crate::host::RecordingPublisher::default();
        let state = RwLock::new(CentralState::default());
        let mac = MacAddr::parse("7C:D9:F4:13:10:DE").expect("mac");
        let request = HistoryRequest::new(None, None, Some("job-2".into()), 0).expect("request");
        history_failed(
            &state,
            &publisher,
            mac,
            &request,
            FailReason::NotRecording,
            0,
        );
        let sent = history_json(&publisher);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["state"], "failed");
        assert_eq!(sent[0]["reason"], "not_recording");
        assert_eq!(sent[0]["retry"], 0);
        assert!(crate::central::state::read_state(&state)
            .history_retries
            .pending(&mac)
            .is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_history_request_is_announced_and_a_full_queue_is_its_first_busy_failure() {
        let publisher = crate::host::RecordingPublisher::default();
        let (dial_tx, mut dial_rx) = mpsc::channel::<DialJob>(1);
        let handle = CentralHandle {
            state: Arc::new(RwLock::new(CentralState::default())),
            slot: Arc::new(AdapterSlot::new()),
            capability: Arc::new(En12830Capability::new(&EyeSection::default().en12830)),
            dial_tx,
            signals: LoopSignals::default(),
            alive: Arc::new(AtomicBool::new(true)),
            stop: watch::channel(false).0,
            store: store(),
        };
        let mac = MacAddr::parse("7C:D9:F4:13:10:DE").expect("mac");
        let first = HistoryRequest::new(None, None, Some("a".into()), 0).expect("request");
        assert!(handle.request_history(&mac, first, &publisher, 0));
        let second = HistoryRequest::new(Some(10), None, Some("b".into()), 0).expect("request");
        assert!(!handle.request_history(&mac, second, &publisher, 0));

        let sent = history_json(&publisher);
        let states: Vec<_> = sent
            .iter()
            .map(|m| {
                (
                    m["job"].as_str().unwrap_or(""),
                    m["state"].as_str().unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(
            states,
            [("a", "requested"), ("b", "requested"), ("b", "requested")],
            "the second is announced, then re-announced as retry 1 for its busy queue"
        );
        assert_eq!(
            (sent[2]["retry"].as_u64(), sent[2]["reason"].as_str()),
            (Some(1), Some("busy"))
        );
        assert!(
            matches!(dial_rx.try_recv().map(|j| j.kind), Ok(DialKind::History(r)) if r.job == "a")
        );
        // A newer request supersedes the waiting retry.
        let third = HistoryRequest::new(None, None, Some("c".into()), 0).expect("request");
        assert!(handle.request_history(&mac, third, &publisher, 0));
        assert!(crate::central::state::read_state(&handle.state)
            .history_retries
            .pending(&mac)
            .is_none());
    }

    #[test]
    fn the_raw_chunks_are_appended_one_download_per_line() {
        let t = TempDir::new("chunks");
        let store = t.store();
        let mac = MacAddr::parse("AA:BB:CC:DD:EE:FF").expect("mac");
        append_archive_chunks(&store, &mac, 1_000, &[(0, vec![0x01, 0xab]), (1, vec![])])
            .expect("first");
        append_archive_chunks(&store, &mac, 2_000, &[(7, vec![0xff])]).expect("second");
        let raw = std::fs::read_to_string(store.archive_chunks_path(&mac)).expect("read");
        let lines: Vec<serde_json::Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).expect("JSON line"))
            .collect();
        assert_eq!(
            lines,
            [
                serde_json::json!({"at": 1_000, "chunks": [[0, "01ab"], [1, ""]]}),
                serde_json::json!({"at": 2_000, "chunks": [[7, "ff"]]}),
            ]
        );
    }

    /// A dial worker whose adapter is held, so every dial fails `busy` at the claim without going
    /// on air — the flaky first dial of F4, without a radio.
    fn busy_worker(host: &Host, state: &Arc<RwLock<CentralState>>, store: Store) -> DialWorker {
        DialWorker {
            host: host.clone(),
            slot: Arc::new(AdapterSlot::new()),
            capability: Arc::new(En12830Capability::new(&EyeSection::default().en12830)),
            state: state.clone(),
            store,
            adapter_name: Some("hci-does-not-exist".into()),
            dial_budget: DialBudget::default(),
            profile: BeaconProfile::default(),
            scan_signals: LoopSignals::default(),
            requeue: None,
        }
    }

    async fn run_one(worker: DialWorker, job: DialJob) {
        let slot = worker.slot.clone();
        let _held = slot
            .claim_raw(MacAddr::parse("00:11:22:33:44:55").expect("mac"))
            .expect("hold the adapter");
        let (tx, rx) = mpsc::channel::<DialJob>(1);
        tx.send(job).await.expect("queue");
        drop(tx);
        dial_worker(rx, worker).await;
    }

    fn published_provisioning(state: &RwLock<CentralState>, mac: &MacAddr) -> String {
        let enrolled = BTreeSet::from([*mac]);
        let snap = write_state(state).snapshot(
            1_700_000_000,
            &crate::config::CentralConfig::default(),
            "hub",
            &enrolled,
            &Default::default(),
        );
        snap.tags
            .iter()
            .find(|t| t.mac == mac.to_string())
            .map(|t| t.provisioning.clone())
            .expect("tracked")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_provisioning_is_retried_on_the_ladder_then_given_up() {
        let clock = Arc::new(crate::host::FakeClock::at(1_700_000_000));
        let host = Host {
            clock: clock.clone(),
            ..Host::in_memory()
        };
        let mac = MacAddr::parse("7C:D9:F4:10:6E:30").expect("mac");
        let enrolled = BTreeSet::from([mac]);
        let state = Arc::new(RwLock::new(CentralState::default()));
        write_state(&state).entry(mac, None, &enrolled);
        let t = TempDir::new("provision-ladder");
        let provision = || DialJob {
            mac,
            kind: DialKind::Provision,
        };

        // The first session fails: `failed`, and a retry waits 5 minutes for an advertisement.
        run_one(busy_worker(&host, &state, t.store()), provision()).await;
        assert_eq!(published_provisioning(&state, &mac), "failed");
        let ledger = ProvisionLedger::load(&t.store()).expect("ledger");
        assert_eq!(ledger.get(&mac).map(|r| r.attempts), Some(1));
        assert_eq!(
            ledger.get(&mac).and_then(|r| r.retry_at),
            Some(1_700_000_000 + 300),
            "the retry is persisted, so a restart keeps it"
        );

        let (tx, mut rx) = mpsc::channel::<DialJob>(1);
        for (n, delay) in [(1, 300), (2, 900), (3, 3_600)] {
            clock.advance(delay - 1);
            requeue_due_provision(&host, &state, &tx, mac, &enrolled);
            assert!(rx.try_recv().is_err(), "retry {n}: the floor holds");
            clock.advance(1);
            requeue_due_provision(&host, &state, &tx, mac, &enrolled);
            let job = rx.try_recv().expect("due: queued on the advertisement");
            assert_eq!(job.kind, DialKind::Provision);
            assert_eq!(
                published_provisioning(&state, &mac),
                "provisioning",
                "retry {n} publishes `provisioning` from the moment it is queued"
            );
            requeue_due_provision(&host, &state, &tx, mac, &enrolled);
            assert!(rx.try_recv().is_err(), "queued once, not per advertisement");
            run_one(busy_worker(&host, &state, t.store()), job).await;
            assert_eq!(published_provisioning(&state, &mac), "failed");
        }

        // The 4th failure gives up: nothing waits, and it stays `failed`.
        let ledger = ProvisionLedger::load(&t.store()).expect("ledger");
        let r = ledger.get(&mac).expect("record");
        assert_eq!(r.attempts, 4);
        assert!(r.gave_up_at.is_some());
        assert_eq!(r.retry_at, None);
        assert!(read_state_of(&state).provision_retries.is_empty());
        clock.advance(100_000);
        requeue_due_provision(&host, &state, &tx, mac, &enrolled);
        assert!(rx.try_recv().is_err(), "a give-up is never retried");
        assert_eq!(published_provisioning(&state, &mac), "failed");
    }

    /// #161: a Beacon added before beacon-cluster gave it a home had its first Provision dial
    /// skipped and never re-queued. The skip now defers it onto the requeue path the scan loop
    /// runs only for a Beacon this Hub is home for.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_provision_skipped_before_the_home_runs_once_this_hub_is_home() {
        let mac = MacAddr::parse("7C:D9:F4:10:6E:30").expect("mac");
        let enrolled = BTreeSet::from([mac]);
        let host_with_home = |home: BTreeSet<MacAddr>| Host {
            config: Arc::new(MemoryConfigStore::with_section(EyeSection {
                home: Some(home),
                ..EyeSection::default()
            })),
            ..Host::in_memory()
        };
        let state = Arc::new(RwLock::new(CentralState::default()));
        write_state(&state).entry(mac, None, &enrolled);
        let t = TempDir::new("provision-before-home");
        let job = || DialJob {
            mac,
            kind: DialKind::Provision,
        };

        // Added → no home yet: the dial is skipped but deferred, not dropped.
        let no_home = host_with_home(BTreeSet::new());
        run_one(busy_worker(&no_home, &state, t.store()), job()).await;
        assert!(read_state_of(&state).provision_retries.contains_key(&mac));
        assert_ne!(published_provisioning(&state, &mac), "provisioning");
        let ledger = ProvisionLedger::load(&t.store()).expect("ledger");
        assert_eq!(
            ledger.get(&mac).map(|r| r.attempts).unwrap_or(0),
            0,
            "a skip is not an attempt"
        );

        // Home assigned → heard → the provisioning dial is queued and runs (it reaches the
        // session, which fails on this host's absent adapter — an attempt is recorded).
        let home = host_with_home(enrolled.clone());
        let (tx, mut rx) = mpsc::channel::<DialJob>(1);
        requeue_due_provision(&home, &state, &tx, mac, &enrolled);
        let queued = rx.try_recv().expect("queued once this Hub is home");
        assert_eq!(queued.kind, DialKind::Provision);
        run_one(busy_worker(&home, &state, t.store()), queued).await;
        let ledger = ProvisionLedger::load(&t.store()).expect("ledger");
        assert_eq!(
            ledger.get(&mac).map(|r| r.attempts),
            Some(1),
            "provision ran"
        );
    }

    fn read_state_of(state: &RwLock<CentralState>) -> std::sync::RwLockReadGuard<'_, CentralState> {
        crate::central::state::read_state(state)
    }

    #[test]
    fn a_due_provisioning_retry_waits_for_a_queue_slot_and_for_a_beacon_still_ours() {
        let clock = Arc::new(crate::host::FakeClock::at(10_000));
        let host = Host {
            clock: clock.clone(),
            ..Host::in_memory()
        };
        let mac = MacAddr::parse("7C:D9:F4:10:6E:30").expect("mac");
        let enrolled = BTreeSet::from([mac]);
        let state = RwLock::new(CentralState::default());
        {
            let mut s = write_state(&state);
            s.entry(mac, None, &enrolled).provisioning = Provisioning::Failed;
            s.failed.insert(mac);
            s.provision_retries.insert(mac, 9_000);
        }
        let (tx, mut rx) = mpsc::channel::<DialJob>(1);

        requeue_due_provision(&host, &state, &tx, mac, &BTreeSet::new());
        assert!(
            rx.try_recv().is_err(),
            "a beacon no longer ours is not dialled"
        );

        tx.try_send(DialJob {
            mac,
            kind: DialKind::Detect,
        })
        .expect("fill the queue");
        requeue_due_provision(&host, &state, &tx, mac, &enrolled);
        assert_eq!(
            read_state_of(&state).provision_retries.get(&mac),
            Some(&9_000),
            "a full queue puts the retry back"
        );
        assert_eq!(published_provisioning(&state, &mac), "failed");
        assert!(matches!(
            rx.try_recv().map(|j| j.kind),
            Ok(DialKind::Detect)
        ));

        requeue_due_provision(&host, &state, &tx, mac, &enrolled);
        assert!(matches!(
            rx.try_recv().map(|j| j.kind),
            Ok(DialKind::Provision)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_waiting_provisioning_retry_is_seeded_from_the_ledger_and_an_operator_replaces_it() {
        let (host, _) = host_with(EyeSection::default());
        let mac = MacAddr::parse("7C:D9:F4:10:6E:30").expect("mac");
        let t = TempDir::new("provision-seed");
        let mut ledger = ProvisionLedger::default();
        ledger.record_attempt(&mac, 1_000);
        let unreachable = Error::new(
            Kind::BeaconUnreachable,
            Role::Central,
            Component::BeaconGatt,
            "unreachable in 30 s",
        );
        ledger.record_provision_failure(&mac, 1_000, 1_030, &unreachable, 5);
        ledger.save(&t.store()).expect("saves");

        let rt = spawn(
            &tokio::runtime::Handle::current(),
            &cfg(Mode::Central),
            host,
            t.store(),
            hub_mac(),
        );
        let seeded = rt.handles.central.clone().expect("central").state;
        rt.shutdown().await;
        assert_eq!(
            read_state_of(&seeded).provision_retries.get(&mac),
            Some(&1_330),
            "a restart keeps the ladder"
        );

        // The operator's "Try again" replaces it, without a live worker racing the ledger.
        let (dial_tx, mut dial_rx) = mpsc::channel::<DialJob>(1);
        let handle = CentralHandle {
            state: seeded,
            slot: Arc::new(AdapterSlot::new()),
            capability: Arc::new(En12830Capability::new(&EyeSection::default().en12830)),
            dial_tx,
            signals: LoopSignals::default(),
            alive: Arc::new(AtomicBool::new(true)),
            stop: watch::channel(false).0,
            store: t.store(),
        };
        handle.request_provision(&mac).expect("queued");
        assert!(matches!(
            dial_rx.try_recv().map(|j| j.kind),
            Ok(DialKind::Provision)
        ));
        assert!(read_state_of(&handle.state).provision_retries.is_empty());
        let ledger = ProvisionLedger::load(&t.store()).expect("ledger");
        assert_eq!(ledger.get(&mac).and_then(|r| r.retry_at), None);
    }

    #[test]
    fn a_provisioned_beacon_gets_a_recording_dial_at_the_fleet_cadence() {
        let config = Arc::new(MemoryConfigStore::default());
        let stopped = MacAddr::parse("7C:D9:F4:10:A8:E2").expect("mac");
        config
            .mutate(&mut |eye| {
                eye.recording_interval_min = 5;
                eye.tags.push(crate::config::EyeTag {
                    mac: stopped.to_string(),
                    name: None,
                    enabled: true,
                    logging_interval_min: Some(15),
                    recording: Some(false),
                    field_thresholds: vec![],
                    provisioned: None,
                });
            })
            .expect("the cadence, and a tag an operator stopped");
        let host = Host {
            config,
            ..Host::in_memory()
        };
        let slot = Arc::new(AdapterSlot::new());
        let state = Arc::new(RwLock::new(CentralState::default()));
        let t = TempDir::new("provision-recording");
        let store = t.store();
        let budget = DialBudget::default();
        let profile = BeaconProfile::default();
        let (tx, mut rx) = mpsc::channel::<DialJob>(4);
        let weak = tx.downgrade();
        let mac = MacAddr::parse("7C:D9:F4:13:10:DE").expect("mac");
        for (enabled, mac) in [(true, mac), (true, stopped), (false, mac)] {
            let capability = Arc::new(En12830Capability::new(&En12830Config {
                enabled,
                protocol_failure_alarm_after: 3,
            }));
            let ctx = DialCtx {
                host: &host,
                slot: &slot,
                capability: &capability,
                state: &state,
                store: &store,
                adapter_name: None,
                dial_budget: &budget,
                profile: &profile,
                requeue: Some(&weak),
            };
            queue_recording_after_provision(mac, &ctx);
        }
        assert!(matches!(
            rx.try_recv().map(|j| j.kind),
            Ok(DialKind::Recording(5))
        ));
        assert!(
            matches!(rx.try_recv().map(|j| j.kind), Ok(DialKind::Recording(0))),
            "a stopped recorder is kept stopped, never restarted at the fleet cadence"
        );
        assert!(
            rx.try_recv().is_err(),
            "a disabled recorder capability queues nothing"
        );
    }
    fn recorder_state(mac: MacAddr, report: Option<RecordingReport>) -> Arc<RwLock<CentralState>> {
        let state = Arc::new(RwLock::new(CentralState::default()));
        {
            let mut s = write_state(&state);
            s.entry(mac, None, &BTreeSet::from([mac]));
            s.provisioned.insert(mac);
            if let Some(r) = report {
                s.recordings.insert(mac, r);
            }
        }
        state
    }

    #[test]
    fn a_provisioned_recorder_off_its_cadence_gets_one_recording_dial_on_an_advertisement() {
        let host = Host::in_memory();
        let mac = MacAddr::parse("7C:D9:F4:10:6E:30").expect("mac");
        let stopped = RecordingReport {
            recording: false,
            interval_min: 15,
            read_at: 1_000,
        };
        let (tx, mut rx) = mpsc::channel::<DialJob>(4);

        // Holding its target, nothing; off the capability, nothing.
        let state = recorder_state(mac, Some(stopped));
        requeue_due_recording(&host, &state, &tx, mac, Some(0));
        requeue_due_recording(&host, &state, &tx, mac, None);
        assert!(rx.try_recv().is_err());

        // Not provisioned here, or known not to be a recorder: nothing.
        write_state(&state).provisioned.clear();
        requeue_due_recording(&host, &state, &tx, mac, Some(15));
        assert!(rx.try_recv().is_err(), "only beacons this Hub provisioned");
        write_state(&state).provisioned.insert(mac);
        if let Some(b) = write_state(&state).beacons.get_mut(&mac) {
            b.is_en12830 = Some(false);
        }
        requeue_due_recording(&host, &state, &tx, mac, Some(15));
        assert!(rx.try_recv().is_err(), "a standard beacon has no archive");

        // A recorder stopped against a 15-minute target, and one never read: one dial each.
        for state in [
            recorder_state(mac, Some(stopped)),
            recorder_state(mac, None),
        ] {
            requeue_due_recording(&host, &state, &tx, mac, Some(15));
            assert_eq!(
                rx.try_recv().map(|j| j.kind).ok(),
                Some(DialKind::Recording(15))
            );
            requeue_due_recording(&host, &state, &tx, mac, Some(15));
            assert!(
                rx.try_recv().is_err(),
                "one dial in flight, not one per frame"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_recording_dial_waits_on_the_ladder() {
        let clock = Arc::new(crate::host::FakeClock::at(1_700_000_000));
        let host = Host {
            clock: clock.clone(),
            ..Host::in_memory()
        };
        let mac = MacAddr::parse("7C:D9:F4:10:6E:30").expect("mac");
        let state = recorder_state(mac, None);
        let t = TempDir::new("recording-ladder");
        let (tx, mut rx) = mpsc::channel::<DialJob>(4);

        requeue_due_recording(&host, &state, &tx, mac, Some(15));
        let job = rx.try_recv().expect("queued");
        run_one(busy_worker(&host, &state, t.store()), job).await;
        let entry = *read_state_of(&state)
            .recording_retries
            .get(&mac)
            .expect("on the ladder");
        assert_eq!((entry.failures, entry.in_flight), (1, false));

        clock.advance_monotonic(299);
        requeue_due_recording(&host, &state, &tx, mac, Some(15));
        assert!(rx.try_recv().is_err(), "the 5-minute floor holds");
        clock.advance_monotonic(1);
        requeue_due_recording(&host, &state, &tx, mac, Some(15));
        assert_eq!(
            rx.try_recv().map(|j| j.kind).ok(),
            Some(DialKind::Recording(15))
        );
    }

    #[test]
    fn a_recording_dial_succeeds_only_when_the_beacon_reads_back_its_target() {
        let mac = MacAddr::parse("7C:D9:F4:10:6E:30").expect("mac");
        let state = recorder_state(mac, None);
        write_state(&state).recording_retries.started(mac, 15, 0);

        recording_settled(&state, mac, 15, &Ok(()), 0);
        let entry = *read_state_of(&state)
            .recording_retries
            .get(&mac)
            .expect("an unread result is not a success");
        assert_eq!(entry.failures, 1);

        write_state(&state).recordings.insert(
            mac,
            RecordingReport {
                recording: true,
                interval_min: 15,
                read_at: 1,
            },
        );
        recording_settled(&state, mac, 15, &Ok(()), 300);
        assert!(read_state_of(&state).recording_retries.get(&mac).is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_recording_report_is_persisted_and_seeded_into_ble_config_after_a_restart() {
        let (host, _) = host_with(EyeSection::default());
        let mac = MacAddr::parse("7C:D9:F4:10:6E:30").expect("mac");
        let t = TempDir::new("recording-persist");
        let mut ledger = ProvisionLedger::default();
        ledger.record_success(&mac, 1_000);
        ledger.record_ble_config(
            &mac,
            ReportedBle {
                config: crate::central::provision::BleConfig {
                    advertising_interval_ms: 10_000,
                    tx_power_dbm: 8,
                    protocol_type: 0x02,
                    active_sensors: None,
                },
                read_at: 1_000,
                verified: true,
            },
        );
        ledger.save(&t.store()).expect("saves");

        // A recording dial's read-back lands in the ledger…
        let store = t.store();
        let slot = Arc::new(AdapterSlot::new());
        let capability = Arc::new(En12830Capability::new(&EyeSection::default().en12830));
        let live = Arc::new(RwLock::new(CentralState::default()));
        let budget = DialBudget::default();
        let profile = BeaconProfile::default();
        let ctx = DialCtx {
            host: &host,
            slot: &slot,
            capability: &capability,
            state: &live,
            store: &store,
            adapter_name: None,
            dial_budget: &budget,
            profile: &profile,
            requeue: None,
        };
        let info = RecordInfo {
            is_recording: true,
            interval_s: 900,
            number_of_records: 3,
            start_ts: 0,
        };
        record_recording(&ctx, mac, &info);

        // …and a restart seeds it, so `ble_config` carries it again without a dial.
        let rt = spawn(
            &tokio::runtime::Handle::current(),
            &cfg(Mode::Central),
            host,
            t.store(),
            hub_mac(),
        );
        let seeded = rt.handles.central.clone().expect("central").state;
        rt.shutdown().await;
        assert!(read_state_of(&seeded).provisioned.contains(&mac));
        let enrolled = BTreeSet::from([mac]);
        let snap = {
            let mut s = write_state(&seeded);
            s.entry(mac, None, &enrolled);
            s.snapshot(
                1_700_000_000,
                &crate::config::CentralConfig::default(),
                "hub",
                &enrolled,
                &Default::default(),
            )
        };
        let tag = serde_json::to_value(&snap.tags[0]).expect("json");
        assert_eq!(tag["ble_config"]["recording"], true);
        assert_eq!(tag["ble_config"]["logging_interval_min"], 15);
    }

    // --- A recording change erases the archive (beacon_history.md §8, amended 2026-10-09) ---

    use crate::central::en12830::DownloadProgress;

    /// A recorder without a radio. It answers its current record info, logs every call in order,
    /// and a start or stop changes what it reports next, as the hardware does.
    struct FakeRecorder {
        info: std::sync::Mutex<RecordInfo>,
        calls: std::sync::Mutex<Vec<String>>,
        /// The archive a download streams, in one chunk.
        archive: Vec<(i64, f64)>,
        /// The download fails after streaming the archive's first record.
        download_fails: bool,
    }

    impl FakeRecorder {
        fn new(is_recording: bool, interval_s: u16, archive: Vec<(i64, f64)>) -> Self {
            FakeRecorder {
                info: std::sync::Mutex::new(RecordInfo {
                    is_recording,
                    interval_s,
                    number_of_records: archive.len() as u16,
                    start_ts: 1_700_000_000,
                }),
                calls: std::sync::Mutex::new(Vec::new()),
                archive,
                download_fails: false,
            }
        }
        fn log(&self, call: String) {
            self.calls.lock().expect("calls").push(call);
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("calls").clone()
        }
        fn start(&self, interval_s: Option<u16>) {
            let mut info = self.info.lock().expect("info");
            info.is_recording = interval_s.is_some_and(|s| s != 0);
            if let Some(s) = interval_s.filter(|s| *s != 0) {
                info.interval_s = s;
            }
        }
    }

    impl RecorderOps for FakeRecorder {
        fn read_record_info(&self) -> Result<RecordInfo> {
            self.log("info".into());
            Ok(*self.info.lock().expect("info"))
        }
        fn set_recording(&self, interval_s: u16, _now: u32) -> Result<bool> {
            self.log(format!("set {interval_s}"));
            self.start(Some(interval_s));
            Ok(true)
        }
        fn download_since(
            &self,
            since: i64,
            _until: Option<i64>,
            _interval_s: u16,
            restart_s: Option<u16>,
            _now: u32,
            progress: &mut dyn FnMut(DownloadProgress<'_>),
        ) -> Result<Download> {
            self.log(format!("download since={since} restart={restart_s:?}"));
            // Every download stops the recorder first.
            self.start(None);
            let fresh: Vec<(i64, f64)> = self
                .archive
                .iter()
                .copied()
                .filter(|(ts, _)| *ts >= since)
                .collect();
            if self.download_fails {
                progress(DownloadProgress {
                    chunks_read: 1,
                    received: 1,
                    fresh: &fresh[..1],
                });
                return Err(Error::new(
                    Kind::BeaconUnreachable,
                    Role::Central,
                    Component::BeaconGatt,
                    "the link dropped part way",
                ));
            }
            progress(DownloadProgress {
                chunks_read: 1,
                received: fresh.len(),
                fresh: &fresh,
            });
            self.start(restart_s);
            Ok(Download {
                records: fresh,
                restart_ok: restart_s.is_some(),
                chunks_read: 2,
                sessions: 3,
                rejected_chunks: 0,
                future_dropped: 0,
                chunks: vec![(0, vec![0x01])],
            })
        }
    }

    const DRAIN_NOW: i64 = 1_791_590_000;

    /// One recording dial against `fake`, through both halves and the ladder, as the dial worker
    /// runs it; `stored_up_to` seeds the newest archive row this Hub holds.
    fn drain_dial(
        fake: &FakeRecorder,
        target_min: u16,
        stored_up_to: Option<i64>,
    ) -> (
        Result<()>,
        Arc<crate::host::RecordingPublisher>,
        Host,
        Arc<RwLock<CentralState>>,
    ) {
        let publisher = Arc::new(crate::host::RecordingPublisher::default());
        let host = Host {
            publisher: publisher.clone(),
            clock: Arc::new(crate::host::FakeClock::at(DRAIN_NOW)),
            ..Host::in_memory()
        };
        let mac = MacAddr::parse("7C:D9:F4:10:6E:30").expect("mac");
        if let Some(ts) = stored_up_to {
            host.readings
                .store_reading(&ReadingRow {
                    mac: mac.to_string(),
                    ts,
                    received_at: ts,
                    message_id: format!("{mac}-rec-{ts}"),
                    event_type: EVENT_RECORDING.to_string(),
                    payload: serde_json::json!({ "temperature_c": 4.0 }),
                })
                .expect("seed");
        }
        let t = TempDir::new("drain");
        let store = t.store();
        let slot = Arc::new(AdapterSlot::new());
        let capability = Arc::new(En12830Capability::new(&EyeSection::default().en12830));
        let state = Arc::new(RwLock::new(CentralState::default()));
        let budget = DialBudget::default();
        let profile = BeaconProfile::default();
        let ctx = DialCtx {
            host: &host,
            slot: &slot,
            capability: &capability,
            state: &state,
            store: &store,
            adapter_name: None,
            dial_budget: &budget,
            profile: &profile,
            requeue: None,
        };
        let target_s = protocol::interval_s(target_min).unwrap_or(0);
        let job = ArchiveJob::new(
            &ctx,
            mac,
            HistoryRequest::new(None, None, Some(drain_job_id(DRAIN_NOW)), DRAIN_NOW)
                .expect("request"),
        );
        let result = change_recording(fake, target_s, DRAIN_NOW as u32, &job)
            .and_then(|dial| settle_recording_dial(&ctx, &job, dial));
        recording_settled(&state, mac, target_min, &result, 0);
        (result, publisher, host.clone(), state)
    }

    fn states(publisher: &crate::host::RecordingPublisher) -> Vec<String> {
        history_json(publisher)
            .iter()
            .map(|m| m["state"].as_str().unwrap_or("").to_string())
            .collect()
    }

    #[test]
    fn a_beacon_already_at_its_target_is_sent_nothing_and_not_drained() {
        let mac = MacAddr::parse("7C:D9:F4:10:6E:30").expect("mac");
        for (recording, interval_s, target_min) in [(true, 900, 15), (false, 900, 0)] {
            let fake = FakeRecorder::new(recording, interval_s, vec![(DRAIN_NOW - 900, 4.0)]);
            let (result, publisher, _, state) = drain_dial(&fake, target_min, None);
            result.expect("holds its target");
            assert_eq!(fake.calls(), ["info"], "no set_recording, no download");
            assert!(publisher.history().is_empty(), "no drain job");
            let s = crate::central::state::read_state(&state);
            assert!(s.recordings[&mac].holds(target_min));
            assert!(s.recording_retries.get(&mac).is_none(), "settled");
        }
    }

    #[test]
    fn an_interval_change_downloads_from_the_newest_stored_record_then_sets() {
        let mac = MacAddr::parse("7C:D9:F4:10:6E:30").expect("mac");
        let stored = DRAIN_NOW - 3_000;
        let archive = vec![
            (stored - 300, 3.9),
            (stored, 4.0),
            (stored + 300, 4.1),
            (stored + 600, 4.2),
        ];
        let fake = FakeRecorder::new(true, 300, archive);
        let (result, publisher, host, state) = drain_dial(&fake, 15, Some(stored));
        result.expect("drained, then set");
        assert_eq!(
            fake.calls(),
            [
                "info".to_string(),
                format!("download since={stored} restart=Some(300)"),
                "set 900".to_string(),
                "info".to_string(),
            ],
            "the archive is read, at its own interval, before the start that erases it"
        );
        let sent = history_json(&publisher);
        assert_eq!(states(&publisher), ["receiving", "records", "done"]);
        let job = sent[0]["job"].as_str().expect("job");
        assert!(job.starts_with("bh-hub-drain-"), "{job}");
        assert!(sent.iter().all(|m| m["job"] == job && m["from"].is_null()));
        assert_eq!(
            sent[1]["records"],
            serde_json::json!([[stored, 4.0], [stored + 300, 4.1], [stored + 600, 4.2]])
        );
        assert_eq!(sent[2]["received"], 3);
        assert_eq!(sent[2]["restart_ok"], true, "left recording, at the target");
        assert_eq!(
            host.readings
                .max_reading_ts(&mac, EVENT_RECORDING)
                .expect("read"),
            Some(stored + 600),
            "and the Hub stored them like any download"
        );
        let s = crate::central::state::read_state(&state);
        assert!(s.recordings[&mac].holds(15));
        assert!(s.recording_retries.get(&mac).is_none());
    }

    #[test]
    fn a_failed_drain_changes_nothing_and_the_dial_waits_on_the_ladder() {
        let mac = MacAddr::parse("7C:D9:F4:10:6E:30").expect("mac");
        let mut fake = FakeRecorder::new(true, 300, vec![(DRAIN_NOW - 600, 4.0)]);
        fake.download_fails = true;
        let (result, publisher, _, state) = drain_dial(&fake, 15, None);
        let err = result.expect_err("the drain failed");
        assert_eq!(err.kind(), Kind::BeaconUnreachable, "{err}");
        assert_eq!(
            fake.calls(),
            ["info", "download since=0 restart=Some(300)", "set 300"],
            "never a start at 900; only the old interval put back after the download's stop"
        );
        assert_eq!(
            *fake.info.lock().expect("info"),
            RecordInfo {
                is_recording: true,
                interval_s: 300,
                number_of_records: 1,
                start_ts: 1_700_000_000,
            }
        );
        let sent = history_json(&publisher);
        assert_eq!(states(&publisher), ["receiving", "records", "failed"]);
        assert_eq!(sent[2]["reason"], "unreachable");
        let s = crate::central::state::read_state(&state);
        let retry = s.recording_retries.get(&mac).expect("on the ladder");
        assert_eq!(
            (
                retry.target_min,
                retry.failures,
                retry.gave_up,
                retry.not_before
            ),
            (15, 1, false, 300)
        );
    }

    #[test]
    fn a_stopped_recorder_is_drained_then_started() {
        let mac = MacAddr::parse("7C:D9:F4:10:6E:30").expect("mac");
        let fake = FakeRecorder::new(false, 900, vec![(DRAIN_NOW - 900, 4.0)]);
        let (result, publisher, _, state) = drain_dial(&fake, 15, None);
        result.expect("drained, then started");
        assert_eq!(
            fake.calls(),
            ["info", "download since=0 restart=None", "set 900", "info"],
            "the download leaves it stopped; the one start comes after it"
        );
        assert_eq!(states(&publisher), ["receiving", "records", "done"]);
        assert_eq!(history_json(&publisher)[2]["restart_ok"], true);
        assert!(crate::central::state::read_state(&state).recordings[&mac].holds(15));
    }

    #[test]
    fn a_download_that_fails_part_way_restarts_the_recorder_at_its_own_interval() {
        let mut fake = FakeRecorder::new(true, 300, vec![(DRAIN_NOW - 600, 4.0)]);
        fake.download_fails = true;
        let publisher = Arc::new(crate::host::RecordingPublisher::default());
        let host = Host {
            publisher: publisher.clone(),
            clock: Arc::new(crate::host::FakeClock::at(DRAIN_NOW)),
            ..Host::in_memory()
        };
        let mac = MacAddr::parse("7C:D9:F4:10:6E:30").expect("mac");
        let t = TempDir::new("history-restart");
        let store = t.store();
        let slot = Arc::new(AdapterSlot::new());
        let capability = Arc::new(En12830Capability::new(&EyeSection::default().en12830));
        let state = Arc::new(RwLock::new(CentralState::default()));
        let budget = DialBudget::default();
        let profile = BeaconProfile::default();
        let ctx = DialCtx {
            host: &host,
            slot: &slot,
            capability: &capability,
            state: &state,
            store: &store,
            adapter_name: None,
            dial_budget: &budget,
            profile: &profile,
            requeue: None,
        };
        let request =
            HistoryRequest::new(None, None, Some("job-r".into()), DRAIN_NOW).expect("request");
        let job = ArchiveJob::new(&ctx, mac, request.clone());
        let err = match fetch_history(&fake, &job) {
            Err(e) => e,
            Ok(_) => panic!("the download failed"),
        };
        assert_eq!(
            err.kind(),
            Kind::BeaconUnreachable,
            "the failure is unchanged"
        );
        assert_eq!(
            fake.calls(),
            ["info", "download since=0 restart=Some(300)", "set 300"],
            "put back at its own interval, which keeps the archive"
        );
        assert!(fake.info.lock().expect("info").is_recording);

        // The ladder sees the same failure as before.
        history_failed(
            &state,
            host.publisher.as_ref(),
            mac,
            &request,
            FailReason::of(&err),
            0,
        );
        let sent = history_json(&publisher);
        let last = sent.last().expect("published");
        assert_eq!(
            (
                last["state"].as_str(),
                last["retry"].as_u64(),
                last["reason"].as_str()
            ),
            (Some("requested"), Some(1), Some("unreachable"))
        );
    }
}
