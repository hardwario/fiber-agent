//! Sharing one adapter, and recovering it without a shell — `plan/control/ble/central.md` §7,
//! `plan/control/ble/plan.md` §0.1, decision #6 / CP-1, CP-2.
//!
//! **The sharing.** BlueZ reports an inbound phone connection and an outbound beacon dial as the
//! same connection transition, so shared state is the only way to tell them apart. v1 carries that
//! state in one direction; this keeps it and extends it twice:
//!
//! - **Raw-ATT dials are tracked too.** They are invisible to BlueZ entirely, so without explicit
//!   tracking the GATT server cannot see them at all and the mutual-exclusion rule cannot hold.
//! - **Claim before dialling, release on `Drop`** — including on every error path and on a panic. A
//!   slot leaked by a panicking download is an adapter that never dials again until restart.
//!
//! **The recovery ladder.** The failure is specific and nasty: BlueZ still reports discovery as
//! running, the start call returned success, no call errors — and zero advertising reports arrive
//! while the controller's receive counter sits frozen. Nothing in the API surface says anything is
//! wrong.
//!
//! Two things make this ladder different from v1's:
//!
//! - **No shell, ever.** The actuators are a `/dev/rfkill` write and a D-Bus power cycle. That is
//!   expressed as [`RecoveryStep`] — a closed enum with no "exec" variant at all, so "the ladder
//!   never execs" is a property of the type rather than a claim about the code.
//! - **The cooldown is enforced on every rung, from one place.** v1's selector took the cooldown as
//!   a parameter on one path and ignored it on the other, so a wedged controller was given a full
//!   rfkill and power cycle roughly every 30 seconds. [`recovery_rung`] is pure and is the only
//!   place that decides.
//!
//! *Amended 2026-10-14.* HIL on Hub 182 showed bluetoothd itself hanging: every D-Bus call timed
//! out, the power cycle could not reach the controller and the ladder never got past rung 0. The
//! ladder now has a rung 2 that restarts `bluetooth.service` through systemd's D-Bus API (still no
//! exec), rate-limited by `bluetoothd_restart_min_secs`; every actuator call is bounded by a
//! timeout so a hung daemon cannot stall the ladder; and consecutive failed discovery starts count
//! as a stall on their own. See `plan/control/ble/central.md` §7 (amendment of the same date).

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::RecoveryConfig;
use crate::error::{Component, Error, Kind, Result, Role};
use crate::host::{DegradedState, Publisher};
use crate::mac::MacAddr;

// ---------------------------------------------------------------------------------------------
// The slot
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
struct SlotState {
    /// The BlueZ-side outbound dial in flight, if any.
    outbound: Option<MacAddr>,
    /// Raw-ATT dials, which BlueZ cannot see at all.
    raw_dials: BTreeSet<MacAddr>,
    last_recovery: Option<Instant>,
    escalation: u32,
    last_stack_restart: Option<Instant>,
}

/// One adapter, two roles. Every outbound dial claims this first; the GATT server consults the same
/// slot so it never mistakes our own dial for a phone.
#[derive(Debug, Default)]
pub struct AdapterSlot {
    inner: Mutex<SlotState>,
    /// Bumped after every bluetoothd restart, so the FB00 peripheral re-registers its GATT
    /// application and advertisement on the new daemon.
    stack_generation: tokio::sync::watch::Sender<u64>,
}

/// A poisoned lock must not wedge the adapter forever — recover the guard and carry on.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// An outbound dial in progress. **Released on `Drop`**, which is the whole point: every `?`, every
/// early return and every panic releases the adapter.
#[derive(Debug)]
pub struct Claim<'a> {
    slot: &'a AdapterSlot,
    mac: MacAddr,
    raw: bool,
}

impl Claim<'_> {
    pub const fn mac(&self) -> MacAddr {
        self.mac
    }
    /// Whether this claim is a raw-ATT dial (invisible to BlueZ).
    pub const fn is_raw(&self) -> bool {
        self.raw
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        let mut s = lock(&self.slot.inner);
        if s.outbound == Some(self.mac) {
            s.outbound = None;
        }
        if self.raw {
            s.raw_dials.remove(&self.mac);
        }
    }
}

/// An outbound dial that owns its share of the slot.
///
/// The borrowed [`Claim`] cannot outlive the function that took it, which is exactly wrong for the
/// EN12830 timeout path: when a job blows its deadline the blocking thread is **still running and
/// still owns the socket**, so the claim must be handed to a detached task that releases it when
/// that thread finally finishes. Releasing early would let a second dial race a live L2CAP session
/// that BlueZ cannot even see.
#[derive(Debug)]
pub struct OwnedClaim {
    slot: std::sync::Arc<AdapterSlot>,
    mac: MacAddr,
    raw: bool,
}

impl OwnedClaim {
    pub const fn mac(&self) -> MacAddr {
        self.mac
    }
    pub const fn is_raw(&self) -> bool {
        self.raw
    }
}

impl Drop for OwnedClaim {
    fn drop(&mut self) {
        let mut s = lock(&self.slot.inner);
        if s.outbound == Some(self.mac) {
            s.outbound = None;
        }
        if self.raw {
            s.raw_dials.remove(&self.mac);
        }
    }
}

impl AdapterSlot {
    pub fn new() -> AdapterSlot {
        AdapterSlot::default()
    }

    /// Claim the adapter for a raw-ATT dial, taking ownership of the claim so it can be moved into
    /// a detached task. See [`OwnedClaim`].
    pub fn claim_owned_raw(slot: &std::sync::Arc<AdapterSlot>, mac: MacAddr) -> Result<OwnedClaim> {
        let mut s = lock(&slot.inner);
        if let Some(busy) = s.outbound {
            return Err(busy_err(busy));
        }
        if let Some(busy) = s.raw_dials.iter().next().copied() {
            return Err(busy_err(busy));
        }
        s.outbound = Some(mac);
        s.raw_dials.insert(mac);
        drop(s);
        Ok(OwnedClaim {
            slot: slot.clone(),
            mac,
            raw: true,
        })
    }

    /// Claim the adapter for a BlueZ-side dial to `mac`.
    pub fn claim(&self, mac: MacAddr) -> Result<Claim<'_>> {
        self.claim_inner(mac, false)
    }

    /// Claim the adapter for a **raw-ATT** dial to `mac`. Tracked separately because BlueZ cannot
    /// see it, and the GATT server has to know anyway.
    pub fn claim_raw(&self, mac: MacAddr) -> Result<Claim<'_>> {
        self.claim_inner(mac, true)
    }

    fn claim_inner(&self, mac: MacAddr, raw: bool) -> Result<Claim<'_>> {
        let mut s = lock(&self.inner);
        if let Some(busy) = s.outbound {
            return Err(busy_err(busy));
        }
        if let Some(busy) = s.raw_dials.iter().next().copied() {
            return Err(busy_err(busy));
        }
        s.outbound = Some(mac);
        if raw {
            s.raw_dials.insert(mac);
        }
        drop(s);
        Ok(Claim {
            slot: self,
            mac,
            raw,
        })
    }

    /// Is `addr` a dial **we** started? The GATT server asks this before treating a `Connected`
    /// transition as a phone (plan.md §0.1).
    pub fn is_our_dial(&self, addr: &MacAddr) -> bool {
        let s = lock(&self.inner);
        s.outbound == Some(*addr) || s.raw_dials.contains(addr)
    }

    pub fn busy_with(&self) -> Option<MacAddr> {
        lock(&self.inner).outbound
    }

    pub fn escalation(&self) -> u32 {
        lock(&self.inner).escalation
    }

    /// Seconds since the last recovery firing, or `None` if none has fired.
    pub fn since_recovery_secs(&self, now: Instant) -> Option<u64> {
        lock(&self.inner)
            .last_recovery
            .map(|t| now.saturating_duration_since(t).as_secs())
    }

    /// Record that a rung fired. One place, so the cooldown cannot be enforced on one path and
    /// forgotten on the other — which is exactly the v1 defect.
    pub fn record_recovery(&self, now: Instant) {
        let mut s = lock(&self.inner);
        s.last_recovery = Some(now);
        s.escalation = s.escalation.saturating_add(1);
    }

    /// Seconds since the last bluetoothd restart, or `None` if none has happened.
    pub fn since_stack_restart_secs(&self, now: Instant) -> Option<u64> {
        lock(&self.inner)
            .last_stack_restart
            .map(|t| now.saturating_duration_since(t).as_secs())
    }

    /// Record a bluetoothd restart and tell every subscriber (the FB00 peripheral) to re-register.
    pub fn record_stack_restart(&self, now: Instant) {
        lock(&self.inner).last_stack_restart = Some(now);
        self.stack_generation
            .send_modify(|g| *g = g.wrapping_add(1));
    }

    /// A receiver that changes whenever bluetoothd has been restarted by the ladder.
    pub fn subscribe_stack_restarts(&self) -> tokio::sync::watch::Receiver<u64> {
        self.stack_generation.subscribe()
    }

    /// A successful scan clears the escalation, so a transient stall does not permanently promote
    /// the ladder to its loud rung.
    pub fn clear_escalation(&self) {
        lock(&self.inner).escalation = 0;
    }
}

fn busy_err(busy: MacAddr) -> Error {
    Error::new(
        Kind::AlreadyExists,
        Role::Central,
        Component::BluetoothAdapter,
        format!("the adapter is already dialling {busy}: one outbound dial at a time (§0.1)"),
    )
}

// ---------------------------------------------------------------------------------------------
// The ladder
// ---------------------------------------------------------------------------------------------

/// What the scan loop observed when it asks the ladder for a rung. Plain values, so the whole
/// decision is testable without a Bluetooth stack or a clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StallObservation {
    /// Seconds since the last applied advertising report.
    pub idle_secs: u64,
    /// Seconds since the last rung fired, `None` if none has.
    pub since_recovery: Option<u64>,
    pub adapter_present: bool,
    /// Beacons currently tracked.
    pub audible: usize,
    /// Rungs fired since the last applied report.
    pub escalation: u32,
    /// Consecutive failed discovery starts (D-Bus timeout, stream dead on arrival).
    pub start_failures: u32,
    /// Seconds since the ladder last restarted bluetoothd, `None` if it never has.
    pub since_stack_restart: Option<u64>,
}

/// The rung that restarts bluetoothd.
pub const RUNG_RESTART_STACK: u8 = 2;

/// Which rung to fire, or `None` for "do nothing".
///
/// **Pure.** Every input is a value, so escalation, the cooldown and the no-audible-beacons guard
/// are provable without a Bluetooth stack — which central.md §Testing asks for by name.
///
/// Two ways in. **Silence**: no reports for `stall_secs` *with audible beacons present* (a Hub
/// with nothing to listen for has no silence to explain). **A dead daemon**: `start_failures`
/// consecutive failed discovery starts, which needs no tracked beacon — after a restart of the
/// agent the beacon table is empty while bluetoothd is still hung.
///
/// Escalation 0 → rung 0, 1 → rung 1, 2+ → rung 2 (bluetoothd restart) when that is enabled and
/// the last restart is at least `bluetoothd_restart_min_secs` old; otherwise rung 1 again.
pub fn select_rung(o: &StallObservation, cfg: &RecoveryConfig) -> Option<u8> {
    if !cfg.enabled {
        return None;
    }
    let silent = o.adapter_present && o.audible > 0 && o.idle_secs >= cfg.stall_secs;
    let dead = cfg.start_failures > 0 && o.start_failures >= cfg.start_failures;
    if !silent && !dead {
        return None;
    }
    // The cooldown gate is *before* the rung choice, so it applies to every rung identically.
    if let Some(since) = o.since_recovery {
        if since < cfg.cooldown_secs {
            return None;
        }
    }
    Some(match o.escalation {
        0 => 0,
        1 => 1,
        _ => {
            let restart_due = o
                .since_stack_restart
                .is_none_or(|s| s >= cfg.bluetoothd_restart_min_secs);
            if cfg.restart_bluetoothd && restart_due {
                RUNG_RESTART_STACK
            } else {
                1
            }
        }
    })
}

/// The silence-only form of [`select_rung`], kept for callers with no start-failure count.
pub fn recovery_rung(
    idle_secs: u64,
    since_recovery: Option<u64>,
    adapter_present: bool,
    audible: usize,
    escalation: u32,
    cfg: &RecoveryConfig,
) -> Option<u8> {
    select_rung(
        &StallObservation {
            idle_secs,
            since_recovery,
            adapter_present,
            audible,
            escalation,
            start_failures: 0,
            since_stack_restart: None,
        },
        cfg,
    )
}

/// One actuator step. A closed set with **no exec variant**: CP-1's "no shell anywhere in this
/// role's recovery path" is a property of this type, not a promise in a comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryStep {
    RfkillBlock,
    RfkillUnblock,
    AdapterPowerOff,
    AdapterPowerOn,
    Sleep(Duration),
    /// The loud degraded report.
    ReportDegraded,
    /// `RestartUnit("bluetooth.service")` over systemd's D-Bus API — not `systemctl`.
    RestartBluetoothd,
}

/// How long the rfkill toggle is held, and how long the adapter stays powered down.
pub const RFKILL_HOLD: Duration = Duration::from_millis(300);
pub const POWER_CYCLE_HOLD: Duration = Duration::from_millis(500);
/// Time for a restarted bluetoothd to export its adapter before the scan reopens.
pub const STACK_SETTLE: Duration = Duration::from_secs(5);
/// Upper bound on any single actuator call: a hung bluetoothd must not stall the ladder.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(15);

/// The steps for a rung, in order. Rung 1 is rung 0 **plus** a loud report; rung 2 restarts
/// bluetoothd (whose D-Bus calls the power cycle depends on) and reports loudly.
pub fn recovery_plan(rung: u8) -> Vec<RecoveryStep> {
    if rung >= RUNG_RESTART_STACK {
        return vec![
            RecoveryStep::RestartBluetoothd,
            RecoveryStep::Sleep(STACK_SETTLE),
            RecoveryStep::ReportDegraded,
        ];
    }
    let mut steps = vec![
        RecoveryStep::RfkillBlock,
        RecoveryStep::Sleep(RFKILL_HOLD),
        RecoveryStep::RfkillUnblock,
        RecoveryStep::AdapterPowerOff,
        RecoveryStep::Sleep(POWER_CYCLE_HOLD),
        RecoveryStep::AdapterPowerOn,
    ];
    if rung >= 1 {
        steps.push(RecoveryStep::ReportDegraded);
    }
    steps
}

/// The degraded state rung 1 and 2 publish.
pub fn stall_degraded_state() -> DegradedState {
    DegradedState {
        condition: "ble_scan_stalled",
        mac: None,
        active: true,
        detail: "scan stall persisted after an rfkill/power cycle; bluetoothd is restarted at \
                 most once per bluetoothd_restart_min_secs and the agent never restarts itself"
            .to_string(),
    }
}

// ---------------------------------------------------------------------------------------------
// rfkill
// ---------------------------------------------------------------------------------------------

pub trait Rfkill: Send + Sync {
    fn block(&self) -> std::io::Result<()>;
    fn unblock(&self) -> std::io::Result<()>;
}

/// `RFKILL_TYPE_BLUETOOTH`.
pub const RFKILL_TYPE_BLUETOOTH: u8 = 2;
/// `RFKILL_OP_CHANGE_ALL` — act on every device of the type, so no index has to be discovered.
pub const RFKILL_OP_CHANGE_ALL: u8 = 3;
/// The kernel's `/dev/rfkill` device.
pub const RFKILL_DEVICE: &str = "/dev/rfkill";

/// The kernel's `struct rfkill_event`, little-endian, 8 bytes.
///
/// Written with a plain `File::write` — **no `unsafe`, no ioctl, no `rfkill(8)`**. v1 shelled out to
/// `sh -c "rfkill block bluetooth; ..."`; this is the same effect as four values in a byte array.
pub fn rfkill_event_bytes(soft_block: bool) -> [u8; 8] {
    let idx: u32 = 0;
    let i = idx.to_le_bytes();
    [
        i[0],
        i[1],
        i[2],
        i[3],
        RFKILL_TYPE_BLUETOOTH,
        RFKILL_OP_CHANGE_ALL,
        u8::from(soft_block),
        0, // hard block is the kernel's to set, never ours
    ]
}

/// Writes the kernel ABI to `/dev/rfkill`.
#[derive(Debug, Clone, Copy, Default)]
pub struct DevRfkill;

impl DevRfkill {
    fn write(soft: bool) -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(RFKILL_DEVICE)?;
        f.write_all(&rfkill_event_bytes(soft))?;
        f.flush()
    }
}

impl Rfkill for DevRfkill {
    fn block(&self) -> std::io::Result<()> {
        DevRfkill::write(true)
    }
    fn unblock(&self) -> std::io::Result<()> {
        DevRfkill::write(false)
    }
}

/// Records calls instead of touching the kernel.
#[derive(Debug, Default)]
pub struct NullRfkill {
    calls: Mutex<Vec<bool>>,
}

impl NullRfkill {
    /// `true` for a block, `false` for an unblock, in order.
    pub fn calls(&self) -> Vec<bool> {
        lock(&self.calls).clone()
    }
}

impl Rfkill for NullRfkill {
    fn block(&self) -> std::io::Result<()> {
        lock(&self.calls).push(true);
        Ok(())
    }
    fn unblock(&self) -> std::io::Result<()> {
        lock(&self.calls).push(false);
        Ok(())
    }
}

/// The adapter power actuator, as a seam — so `actuate` can be exercised end to end without BlueZ.
/// Async so a hung bluetoothd is bounded by [`STEP_TIMEOUT`] instead of blocking a worker thread.
pub trait AdapterPower: Send + Sync {
    fn set_powered(&self, on: bool) -> BoxFuture<'_, Result<()>>;
}

/// Restarts the Bluetooth daemon. The real one talks to systemd over D-Bus; tests count calls.
pub trait StackRestarter: Send + Sync {
    fn restart(&self) -> BoxFuture<'_, Result<()>>;
}

pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// The unit rung 2 restarts.
pub const BLUETOOTH_UNIT: &str = "bluetooth.service";

/// `org.freedesktop.systemd1.Manager.RestartUnit(BLUETOOTH_UNIT, "replace")` on the system bus.
/// The agent runs as root, which systemd authorises without polkit; no exec, no `systemctl`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemdRestarter;

impl StackRestarter for SystemdRestarter {
    fn restart(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            let joined = tokio::task::spawn_blocking(|| -> std::result::Result<(), String> {
                let conn = dbus::blocking::Connection::new_system().map_err(|e| e.to_string())?;
                let proxy = conn.with_proxy(
                    "org.freedesktop.systemd1",
                    "/org/freedesktop/systemd1",
                    STEP_TIMEOUT,
                );
                let _job: (dbus::Path<'static>,) = proxy
                    .method_call(
                        "org.freedesktop.systemd1.Manager",
                        "RestartUnit",
                        (BLUETOOTH_UNIT, "replace"),
                    )
                    .map_err(|e| e.to_string())?;
                Ok(())
            })
            .await;
            match joined {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(stack_err(format!("cannot restart {BLUETOOTH_UNIT}: {e}"))),
                Err(e) => Err(stack_err(format!("restart task failed: {e}"))),
            }
        })
    }
}

/// The actuators a rung may use.
pub struct Actuators<'a> {
    pub power: &'a dyn AdapterPower,
    pub rfkill: &'a dyn Rfkill,
    pub restarter: &'a dyn StackRestarter,
    pub publisher: &'a dyn Publisher,
}

/// Run a rung's plan and return the steps performed, so a caller (or a test) can assert the
/// sequence. Every D-Bus step is bounded by [`STEP_TIMEOUT`]. A failing power step does not
/// abort the plan: the loud report must still go out even when bluetoothd does not answer.
pub async fn actuate(rung: u8, a: &Actuators<'_>) -> Result<Vec<RecoveryStep>> {
    let plan = recovery_plan(rung);
    let mut first_err = None;
    for step in &plan {
        let r = match step {
            RecoveryStep::RfkillBlock => a.rfkill.block().map_err(rfkill_err),
            RecoveryStep::RfkillUnblock => a.rfkill.unblock().map_err(rfkill_err),
            RecoveryStep::AdapterPowerOff => bounded(a.power.set_powered(false)).await,
            RecoveryStep::AdapterPowerOn => bounded(a.power.set_powered(true)).await,
            RecoveryStep::Sleep(d) => {
                tokio::time::sleep(*d).await;
                Ok(())
            }
            RecoveryStep::ReportDegraded => {
                a.publisher.publish_degraded(&stall_degraded_state());
                Ok(())
            }
            RecoveryStep::RestartBluetoothd => bounded(a.restarter.restart()).await,
        };
        if let Err(e) = r {
            if *step == RecoveryStep::RestartBluetoothd {
                // Nothing after it makes sense without a fresh daemon, apart from the report.
                a.publisher.publish_degraded(&stall_degraded_state());
                return Err(e);
            }
            first_err.get_or_insert(e);
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(plan),
    }
}

async fn bounded(f: BoxFuture<'_, Result<()>>) -> Result<()> {
    tokio::time::timeout(STEP_TIMEOUT, f)
        .await
        .unwrap_or_else(|_| {
            Err(stack_err(format!(
                "no answer within {}s",
                STEP_TIMEOUT.as_secs()
            )))
        })
}

fn stack_err(message: String) -> Error {
    Error::new(
        Kind::Transport,
        Role::Central,
        Component::BluezDbus,
        message,
    )
}

fn rfkill_err(e: std::io::Error) -> Error {
    Error::new(
        Kind::Transport,
        Role::Central,
        Component::BluetoothAdapter,
        format!("cannot write {RFKILL_DEVICE}: {e}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::RecordingPublisher;

    const A: &str = "AA:AA:AA:AA:AA:AA";
    const B: &str = "BB:BB:BB:BB:BB:BB";

    fn mac(s: &str) -> MacAddr {
        MacAddr::parse(s).expect("mac")
    }
    fn cfg() -> RecoveryConfig {
        RecoveryConfig::default()
    }

    #[derive(Debug, Default)]
    struct FakePower {
        calls: Mutex<Vec<bool>>,
    }
    impl FakePower {
        fn calls(&self) -> Vec<bool> {
            lock(&self.calls).clone()
        }
    }
    impl AdapterPower for FakePower {
        fn set_powered(&self, on: bool) -> BoxFuture<'_, Result<()>> {
            lock(&self.calls).push(on);
            Box::pin(async { Ok(()) })
        }
    }

    /// A power actuator talking to a hung bluetoothd: never answers.
    struct HungPower;
    impl AdapterPower for HungPower {
        fn set_powered(&self, _on: bool) -> BoxFuture<'_, Result<()>> {
            Box::pin(std::future::pending())
        }
    }

    #[derive(Default)]
    struct FakeRestarter {
        calls: Mutex<u32>,
        fail: bool,
    }
    impl FakeRestarter {
        fn calls(&self) -> u32 {
            *lock(&self.calls)
        }
    }
    impl StackRestarter for FakeRestarter {
        fn restart(&self) -> BoxFuture<'_, Result<()>> {
            *lock(&self.calls) += 1;
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    Err(stack_err("refused".to_string()))
                } else {
                    Ok(())
                }
            })
        }
    }

    fn obs(escalation: u32) -> StallObservation {
        StallObservation {
            idle_secs: cfg().stall_secs,
            adapter_present: true,
            audible: 3,
            escalation,
            ..StallObservation::default()
        }
    }

    #[test]
    fn a_claimed_slot_refuses_a_second_dial_with_already_exists() {
        let slot = AdapterSlot::new();
        let _first = slot.claim(mac(A)).expect("first claim");
        let err = slot.claim(mac(B)).expect_err("second must be refused");
        assert_eq!(err.kind(), Kind::AlreadyExists);
        assert_eq!(err.component(), Component::BluetoothAdapter);
        assert_eq!(err.role(), Some(Role::Central));
        assert!(
            err.message().contains(A),
            "the refusal names the busy MAC: {}",
            err.message()
        );
    }

    #[test]
    fn a_raw_dial_and_a_bluez_dial_exclude_each_other() {
        // The raw dial is invisible to BlueZ, so only this slot can enforce the rule.
        let slot = AdapterSlot::new();
        let raw = slot.claim_raw(mac(A)).expect("raw claim");
        assert!(raw.is_raw());
        assert!(slot.claim(mac(B)).is_err());
        drop(raw);
        let _ok = slot.claim(mac(B)).expect("free again");
    }

    #[test]
    fn the_slot_is_released_when_the_claim_drops() {
        let slot = AdapterSlot::new();
        {
            let _c = slot.claim(mac(A)).expect("claim");
            assert_eq!(slot.busy_with(), Some(mac(A)));
        }
        assert_eq!(slot.busy_with(), None);
        let _again = slot.claim(mac(A)).expect("reclaimable");
    }

    #[test]
    fn the_slot_is_released_even_when_the_holder_panics() {
        // A slot leaked by a panicking download is an adapter that never dials again until restart.
        let slot = AdapterSlot::new();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _c = slot.claim(mac(A)).expect("claim");
            panic!("the vendor protocol did something surprising");
        }));
        assert!(outcome.is_err(), "the panic really happened");
        assert_eq!(
            slot.busy_with(),
            None,
            "Drop ran during the unwind, so the adapter is free"
        );
        let _again = slot.claim(mac(B)).expect("dialable after a panic");
    }

    #[test]
    fn our_outbound_dial_is_not_a_phone() {
        let slot = AdapterSlot::new();
        assert!(!slot.is_our_dial(&mac(A)), "nothing claimed yet");
        let _c = slot.claim(mac(A)).expect("claim");
        assert!(
            slot.is_our_dial(&mac(A)),
            "the GATT server must not count this"
        );
        assert!(
            !slot.is_our_dial(&mac(B)),
            "a different address really is a phone"
        );
    }

    #[test]
    fn a_raw_dial_is_also_recognised_as_ours() {
        let slot = AdapterSlot::new();
        let _c = slot.claim_raw(mac(A)).expect("claim");
        assert!(slot.is_our_dial(&mac(A)));
    }

    #[test]
    fn no_recovery_without_audible_beacons_or_without_an_adapter() {
        let c = cfg();
        // Long past the stall threshold, but nothing to explain.
        assert_eq!(
            recovery_rung(10_000, None, true, 0, 0, &c),
            None,
            "no audible beacons: there is no silence to explain"
        );
        assert_eq!(
            recovery_rung(10_000, None, false, 5, 0, &c),
            None,
            "no adapter: resetting it would be a self-inflicted outage"
        );
        let disabled = RecoveryConfig {
            enabled: false,
            ..cfg()
        };
        assert_eq!(recovery_rung(10_000, None, true, 5, 0, &disabled), None);
    }

    #[test]
    fn rung_selection_matches_the_ladder() {
        let c = cfg();
        // Below the stall threshold: nothing.
        assert_eq!(recovery_rung(c.stall_secs - 1, None, true, 3, 0, &c), None);
        // At it: rung 0 on a first firing.
        assert_eq!(recovery_rung(c.stall_secs, None, true, 3, 0, &c), Some(0));
        // Already escalated: rung 1, which adds the loud report.
        assert_eq!(recovery_rung(c.stall_secs, None, true, 3, 1, &c), Some(1));
        // Rung 1 did not bring reports back either: restart bluetoothd.
        assert_eq!(recovery_rung(c.stall_secs, None, true, 3, 2, &c), Some(2));
        assert_eq!(recovery_rung(c.stall_secs, None, true, 3, 9, &c), Some(2));
    }

    /// Drive the ladder the way the scan loop does, on a fake clock: each tick that selects a
    /// rung records it, and no report ever arrives. It must climb 0 → 1 → 2, then hold rung 1
    /// until the bluetoothd restart interval has passed, then restart again.
    #[test]
    fn a_stall_that_does_not_clear_climbs_every_rung_and_the_restart_is_rate_limited() {
        let c = cfg();
        let tick = 30u64;
        let mut now = 0u64;
        let mut last_recovery: Option<u64> = None;
        let mut last_restart: Option<u64> = None;
        let mut escalation = 0u32;
        let mut fired = Vec::new();
        while now <= 2 * c.bluetoothd_restart_min_secs + 600 {
            let o = StallObservation {
                idle_secs: now + c.stall_secs, // silence throughout
                since_recovery: last_recovery.map(|t| now - t),
                adapter_present: true,
                audible: 31,
                escalation,
                start_failures: 0,
                since_stack_restart: last_restart.map(|t| now - t),
            };
            if let Some(rung) = select_rung(&o, &c) {
                fired.push((now, rung));
                last_recovery = Some(now);
                escalation += 1;
                if rung == RUNG_RESTART_STACK {
                    last_restart = Some(now);
                }
            }
            now += tick;
        }
        let rungs: Vec<u8> = fired.iter().map(|f| f.1).collect();
        assert_eq!(&rungs[..3], &[0, 1, 2], "climbs, does not stop at rung 0");
        let restarts: Vec<u64> = fired.iter().filter(|f| f.1 == 2).map(|f| f.0).collect();
        assert!(
            restarts.len() >= 2,
            "a persisting stall is retried: {fired:?}"
        );
        for w in restarts.windows(2) {
            assert!(
                w[1] - w[0] >= c.bluetoothd_restart_min_secs,
                "bluetoothd restarted too often: {restarts:?}"
            );
        }
        for w in fired.windows(2) {
            assert!(w[1].0 - w[0].0 >= c.cooldown_secs, "cooldown: {fired:?}");
        }
        assert!(
            rungs[3..].iter().all(|r| *r != 0),
            "never falls back to the quiet rung while escalated"
        );
    }

    #[test]
    fn failed_discovery_starts_escalate_even_with_no_beacons_tracked() {
        // After an agent restart against a hung bluetoothd: no beacons, no silence clock, only
        // StartDiscovery timing out.
        let c = cfg();
        let mut o = StallObservation {
            adapter_present: true,
            start_failures: c.start_failures - 1,
            ..StallObservation::default()
        };
        assert_eq!(select_rung(&o, &c), None);
        o.start_failures = c.start_failures;
        assert_eq!(select_rung(&o, &c), Some(0));
        o.escalation = 2;
        assert_eq!(select_rung(&o, &c), Some(RUNG_RESTART_STACK));
        o.adapter_present = false; // default_adapter() itself timed out
        assert_eq!(select_rung(&o, &c), Some(RUNG_RESTART_STACK));
    }

    #[test]
    fn the_bluetoothd_restart_can_be_disabled_and_is_rate_limited() {
        let c = cfg();
        let mut o = obs(2);
        o.since_stack_restart = Some(c.bluetoothd_restart_min_secs - 1);
        assert_eq!(select_rung(&o, &c), Some(1), "inside the interval: rung 1");
        o.since_stack_restart = Some(c.bluetoothd_restart_min_secs);
        assert_eq!(select_rung(&o, &c), Some(2));
        let off = RecoveryConfig {
            restart_bluetoothd: false,
            ..cfg()
        };
        assert_eq!(select_rung(&obs(5), &off), Some(1));
    }

    #[test]
    fn recording_a_stack_restart_notifies_subscribers() {
        let slot = AdapterSlot::new();
        let rx = slot.subscribe_stack_restarts();
        assert!(!rx.has_changed().expect("open"));
        assert_eq!(slot.since_stack_restart_secs(Instant::now()), None);
        slot.record_stack_restart(Instant::now());
        assert!(rx.has_changed().expect("open"), "the peripheral is told");
        assert_eq!(slot.since_stack_restart_secs(Instant::now()), Some(0));
    }

    #[test]
    fn the_cooldown_is_enforced_on_every_rung() {
        // The v1 defect: the cooldown was a parameter on one path and ignored on the other, so a
        // wedged controller was power-cycled roughly every 30 s.
        let c = cfg();
        for escalation in [0u32, 1, 2, 7] {
            assert_eq!(
                recovery_rung(
                    c.stall_secs,
                    Some(c.cooldown_secs - 1),
                    true,
                    3,
                    escalation,
                    &c
                ),
                None,
                "escalation {escalation} must be refused inside the cooldown"
            );
            assert!(
                recovery_rung(c.stall_secs, Some(c.cooldown_secs), true, 3, escalation, &c)
                    .is_some(),
                "escalation {escalation} must be allowed once the cooldown has passed"
            );
        }
    }

    #[test]
    fn recording_a_recovery_advances_the_escalation_and_a_success_clears_it() {
        let slot = AdapterSlot::new();
        assert_eq!(slot.escalation(), 0);
        assert_eq!(slot.since_recovery_secs(Instant::now()), None);
        slot.record_recovery(Instant::now());
        assert_eq!(slot.escalation(), 1);
        assert_eq!(slot.since_recovery_secs(Instant::now()), Some(0));
        slot.record_recovery(Instant::now());
        assert_eq!(slot.escalation(), 2);
        slot.clear_escalation();
        assert_eq!(
            slot.escalation(),
            0,
            "a transient stall must not permanently promote the ladder"
        );
    }

    #[test]
    fn rfkill_event_bytes_are_the_kernel_abi() {
        assert_eq!(
            rfkill_event_bytes(true),
            [0x00, 0x00, 0x00, 0x00, 0x02, 0x03, 0x01, 0x00]
        );
        assert_eq!(
            rfkill_event_bytes(false),
            [0x00, 0x00, 0x00, 0x00, 0x02, 0x03, 0x00, 0x00]
        );
        assert_eq!(rfkill_event_bytes(true).len(), 8);
    }

    #[test]
    fn the_ladder_has_no_exec_step_at_all() {
        // AC 4: the ladder never execs. The step type has no variant that could; the bluetoothd
        // restart of rung 2 is a D-Bus call to systemd (amended 2026-10-14).
        for rung in [0u8, 1, 2] {
            for step in recovery_plan(rung) {
                assert!(matches!(
                    step,
                    RecoveryStep::RfkillBlock
                        | RecoveryStep::RfkillUnblock
                        | RecoveryStep::AdapterPowerOff
                        | RecoveryStep::AdapterPowerOn
                        | RecoveryStep::Sleep(_)
                        | RecoveryStep::ReportDegraded
                        | RecoveryStep::RestartBluetoothd
                ));
            }
        }
    }

    #[test]
    fn rung_one_is_rung_zero_plus_a_loud_report() {
        let zero = recovery_plan(0);
        let one = recovery_plan(1);
        assert_eq!(&one[..zero.len()], &zero[..], "the actuators are identical");
        assert_eq!(one.last(), Some(&RecoveryStep::ReportDegraded));
        assert!(
            !zero.contains(&RecoveryStep::ReportDegraded),
            "rung 0 is quiet: a single stall that self-heals is not an operator event"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn actuating_rung_zero_cycles_rfkill_and_power_and_publishes_nothing() {
        let power = FakePower::default();
        let rfkill = NullRfkill::default();
        let publisher = RecordingPublisher::default();
        let restarter = FakeRestarter::default();
        let done = actuate(0, &acts(&power, &rfkill, &restarter, &publisher))
            .await
            .expect("actuates");
        assert_eq!(done, recovery_plan(0));
        assert_eq!(rfkill.calls(), vec![true, false], "block then unblock");
        assert_eq!(power.calls(), vec![false, true], "off then on");
        assert!(
            publisher.degraded().is_empty(),
            "rung 0 does not raise an operator event"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn actuating_rung_one_also_reports_degraded_and_never_disables_anything() {
        let power = FakePower::default();
        let rfkill = NullRfkill::default();
        let publisher = RecordingPublisher::default();
        let restarter = FakeRestarter::default();
        actuate(1, &acts(&power, &rfkill, &restarter, &publisher))
            .await
            .expect("actuates");
        assert_eq!(restarter.calls(), 0, "rung 1 leaves bluetoothd alone");
        assert_eq!(rfkill.calls(), vec![true, false]);
        assert_eq!(power.calls(), vec![false, true]);
        let reports = publisher.degraded();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].condition, "ble_scan_stalled");
        assert!(reports[0].active);
        assert!(
            reports[0].detail.contains("never restarts itself"),
            "{}",
            reports[0].detail
        );
    }

    fn acts<'a>(
        power: &'a dyn AdapterPower,
        rfkill: &'a dyn Rfkill,
        restarter: &'a dyn StackRestarter,
        publisher: &'a dyn Publisher,
    ) -> Actuators<'a> {
        Actuators {
            power,
            rfkill,
            restarter,
            publisher,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn actuating_rung_two_restarts_bluetoothd_and_reports() {
        let power = FakePower::default();
        let rfkill = NullRfkill::default();
        let publisher = RecordingPublisher::default();
        let restarter = FakeRestarter::default();
        let done = actuate(2, &acts(&power, &rfkill, &restarter, &publisher))
            .await
            .expect("actuates");
        assert_eq!(done[0], RecoveryStep::RestartBluetoothd);
        assert_eq!(restarter.calls(), 1);
        assert_eq!(publisher.degraded().len(), 1);
        assert!(rfkill.calls().is_empty() && power.calls().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_restart_still_reports_loudly() {
        let power = FakePower::default();
        let rfkill = NullRfkill::default();
        let publisher = RecordingPublisher::default();
        let restarter = FakeRestarter {
            fail: true,
            ..FakeRestarter::default()
        };
        let r = actuate(2, &acts(&power, &rfkill, &restarter, &publisher)).await;
        assert!(r.is_err());
        assert_eq!(publisher.degraded().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_bluetoothd_cannot_stall_the_ladder() {
        // The 2026-10-14 incident: the power cycle's D-Bus call never returned and the ladder
        // went quiet for an hour. Each step is bounded; the report still goes out.
        let rfkill = NullRfkill::default();
        let publisher = RecordingPublisher::default();
        let restarter = FakeRestarter::default();
        let r = actuate(1, &acts(&HungPower, &rfkill, &restarter, &publisher)).await;
        let e = r.expect_err("the power steps timed out");
        assert!(e.to_string().contains("no answer"), "{e}");
        assert_eq!(publisher.degraded().len(), 1, "rung 1 still reports");
    }

    #[test]
    fn the_rfkill_unblock_of_rung_zero_clears_an_operator_block_too() {
        // A25 row 9 depends on this: an operator who runs `rfkill block bluetooth` to induce the
        // stall will see rung 0's unblock clear their own block. That is the documented behaviour,
        // not a surprise — the ladder's job is to get the controller back.
        let plan = recovery_plan(0);
        let block = plan
            .iter()
            .position(|s| *s == RecoveryStep::RfkillBlock)
            .expect("blocks");
        let unblock = plan
            .iter()
            .position(|s| *s == RecoveryStep::RfkillUnblock)
            .expect("unblocks");
        assert!(block < unblock, "block precedes unblock");
    }
}
