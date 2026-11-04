//! The capability flag and its containment — `plan/control/ble/central.md` §6.
//!
//! > **The flag is a circuit breaker; the isolation is unconditional engineering.** Those are two
//! > separate mechanisms and only one of them is optional.
//!
//! **The flag** (`eye.en12830.enabled`) is runtime config, deliberately not a Cargo feature: the day
//! a beacon firmware update breaks the reverse-engineered protocol, the fleet response must be a
//! config rollout, not a rebuild and a full image update. It ships **on**, because a capability
//! nobody exercises is a capability nobody notices has broken.
//!
//! **The isolation** holds whether the flag is on or off and whether the protocol is working or
//! failing — and the case that matters is the one the flag cannot help with, "enabled and failing":
//!
//! - every download runs in its own task with a hard deadline;
//! - panics are contained at the join boundary, so a vendor-protocol panic cannot unwind into the
//!   scan loop;
//! - the adapter slot is released on **every** path — including the orphaned one, where it is
//!   released *late*, when the runaway thread finally finishes, never while it still owns the socket;
//! - the scan loop never blocks on a download.
//!
//! **It never auto-disables.** Auto-disabling hides exactly the breakage the operator needs to see
//! and turns a loud failure into a silent capability regression.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::En12830Config;
use crate::error::{Component, Error, Kind, Result, Role};
use crate::host::{DegradedState, Publisher};
use crate::mac::MacAddr;

use super::super::adapter::AdapterSlot;

/// The hard deadline on one archive job.
pub const JOB_DEADLINE: Duration = Duration::from_secs(300);

/// The degraded condition an exhausted failure streak raises.
pub const CONDITION_PROTOCOL_FAILING: &str = "en12830_protocol_failing";

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug)]
pub struct En12830Capability {
    enabled: AtomicBool,
    alarm_after: u32,
    deadline: Duration,
    failures: Mutex<BTreeMap<MacAddr, u32>>,
    alarmed: Mutex<BTreeSet<MacAddr>>,
}

impl En12830Capability {
    pub fn new(cfg: &En12830Config) -> En12830Capability {
        En12830Capability {
            enabled: AtomicBool::new(cfg.enabled),
            // Fixed at construction: an alarm threshold that moved under a running streak would
            // make the counter meaningless.
            alarm_after: cfg.protocol_failure_alarm_after.max(1),
            deadline: JOB_DEADLINE,
            failures: Mutex::new(BTreeMap::new()),
            alarmed: Mutex::new(BTreeSet::new()),
        }
    }

    /// For tests that cannot wait five minutes.
    pub fn with_deadline(cfg: &En12830Config, deadline: Duration) -> En12830Capability {
        let c = En12830Capability::new(cfg);
        En12830Capability { deadline, ..c }
    }

    /// Re-read the flag. Called on every tick, because a config rollout has to take effect without
    /// a restart — that is the whole reason it is config and not a feature.
    pub fn refresh(&self, cfg: &En12830Config) {
        self.enabled.store(cfg.enabled, Ordering::SeqCst);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    pub const fn alarm_after(&self) -> u32 {
        self.alarm_after
    }

    pub fn consecutive_failures(&self, mac: &MacAddr) -> u32 {
        lock(&self.failures).get(mac).copied().unwrap_or(0)
    }

    pub fn is_alarmed(&self, mac: &MacAddr) -> bool {
        lock(&self.alarmed).contains(mac)
    }

    /// The worst streak across the fleet — what `status` reports.
    pub fn worst_streak(&self) -> u32 {
        lock(&self.failures).values().copied().max().unwrap_or(0)
    }

    fn disabled_err(&self, command: &'static str) -> Error {
        Error::new(
            Kind::CapabilityDisabled,
            Role::Central,
            Component::BeaconGatt,
            format!(
                "`{command}` needs the EN12830 archive capability, which is disabled \
                 (eye.en12830.enabled: false)"
            ),
        )
    }

    /// Run a blocking archive job under the flag, the adapter slot and the deadline.
    ///
    /// Ordering matters and is asserted by test: the **flag is checked before the adapter is
    /// touched**, so a disabled capability cannot make a `beacon-get` fail with `AlreadyExists`
    /// because a refused archive command was holding the slot.
    pub async fn run_contained<T, F>(
        &self,
        slot: &Arc<AdapterSlot>,
        mac: &MacAddr,
        command: &'static str,
        publisher: &dyn Publisher,
        job: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        if !self.enabled() {
            return Err(self.disabled_err(command));
        }

        let claim = AdapterSlot::claim_owned_raw(slot, *mac)?;
        let mut handle = tokio::task::spawn_blocking(job);

        match tokio::time::timeout(self.deadline, &mut handle).await {
            Ok(Ok(Ok(value))) => {
                self.record_success(mac, publisher);
                Ok(value)
            }
            Ok(Ok(Err(e))) => {
                self.record_failure(mac, &e, publisher);
                Err(e)
            }
            Ok(Err(join)) => {
                // A vendor-protocol panic must not unwind into the scan loop; it becomes a typed
                // error at this boundary instead.
                let e = if join.is_panic() {
                    Error::new(
                        Kind::Transport,
                        Role::Central,
                        Component::L2cap,
                        format!("EN12830 job panicked (contained): {join}"),
                    )
                } else {
                    Error::new(
                        Kind::Transport,
                        Role::Central,
                        Component::L2cap,
                        format!("EN12830 job did not complete: {join}"),
                    )
                };
                self.record_failure(mac, &e, publisher);
                Err(e)
            }
            Err(_elapsed) => {
                // The blocking thread is STILL RUNNING and still owns the L2CAP socket. Releasing
                // the slot now would let the next dial race a live session BlueZ cannot see, so the
                // claim goes with the handle into a detached task and is dropped when that thread
                // finally finishes.
                tokio::spawn(async move {
                    let _ = handle.await;
                    drop(claim);
                });
                let e = Error::new(
                    Kind::Timeout,
                    Role::Central,
                    Component::L2cap,
                    format!(
                        "`{command}` exceeded its {} s deadline against {mac}; the adapter stays \
                         claimed until the orphaned session ends",
                        self.deadline.as_secs()
                    ),
                );
                // A deadline is not evidence of a protocol failure, so it does not count.
                self.record_failure(mac, &e, publisher);
                Err(e)
            }
        }
    }

    /// Only **protocol** failures count toward the alarm.
    ///
    /// `BeaconUnreachable`, `Timeout` and `AdapterDown` are RF and platform facts, not evidence that
    /// the reverse-engineered protocol has broken — counting them would make the alarm fire on a
    /// beacon that simply walked out of range. central.md §6 flags the opposite risk too (a genuine
    /// protocol failure misclassified as unreachable, so the alarm never fires); the classification
    /// lives in one place here so it can be traced end to end.
    fn counts_toward_alarm(kind: Kind) -> bool {
        matches!(kind, Kind::Transport)
    }

    fn record_failure(&self, mac: &MacAddr, e: &Error, publisher: &dyn Publisher) {
        if !Self::counts_toward_alarm(e.kind()) {
            return;
        }
        let streak = {
            let mut f = lock(&self.failures);
            let c = f.entry(*mac).or_insert(0);
            *c = c.saturating_add(1);
            *c
        };
        if streak < self.alarm_after {
            return;
        }
        // Edge-triggered: one report per transition into the alarmed state, not one per failure.
        let newly = lock(&self.alarmed).insert(*mac);
        if newly {
            publisher.publish_degraded(&DegradedState {
                condition: CONDITION_PROTOCOL_FAILING,
                mac: Some(mac.to_string()),
                active: true,
                detail: format!(
                    "{streak} consecutive EN12830 protocol failures against {mac}. The capability \
                     stays enabled: auto-disabling would hide exactly the breakage that needs \
                     seeing. Last error: {e}"
                ),
            });
        }
    }

    fn record_success(&self, mac: &MacAddr, publisher: &dyn Publisher) {
        lock(&self.failures).remove(mac);
        let was_alarmed = lock(&self.alarmed).remove(mac);
        if was_alarmed {
            publisher.publish_degraded(&DegradedState {
                condition: CONDITION_PROTOCOL_FAILING,
                mac: Some(mac.to_string()),
                active: false,
                detail: format!("EN12830 archive access to {mac} is working again"),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::RecordingPublisher;

    const A: &str = "AA:BB:CC:DD:EE:FF";
    const B: &str = "00:11:22:33:44:55";

    fn mac(s: &str) -> MacAddr {
        MacAddr::parse(s).expect("mac")
    }
    fn on() -> En12830Config {
        En12830Config {
            enabled: true,
            protocol_failure_alarm_after: 3,
        }
    }
    fn off() -> En12830Config {
        En12830Config {
            enabled: false,
            protocol_failure_alarm_after: 3,
        }
    }
    fn transport(msg: &str) -> Error {
        Error::new(Kind::Transport, Role::Central, Component::L2cap, msg)
    }

    #[tokio::test]
    async fn a_disabled_capability_refuses_before_claiming_the_slot() {
        // AC 3: flag off → CapabilityDisabled, and nothing is persisted or claimed as though the
        // command had taken effect.
        let cap = En12830Capability::new(&off());
        let slot = Arc::new(AdapterSlot::new());
        let publisher = RecordingPublisher::default();
        let err = cap
            .run_contained::<(), _>(&slot, &mac(A), "beacon-detect", &publisher, || {
                unreachable!("the job must never run")
            })
            .await
            .expect_err("must refuse");
        assert_eq!(err.kind(), Kind::CapabilityDisabled);
        assert_eq!(err.exit_code(), 39);
        assert_eq!(err.component(), Component::BeaconGatt);
        assert!(
            err.message().contains("eye.en12830.enabled: false"),
            "the message names the flag: {}",
            err.message()
        );
        assert_eq!(
            slot.busy_with(),
            None,
            "the adapter was never touched, so an unrelated command is not blocked"
        );
    }

    #[tokio::test]
    async fn the_flag_is_live_so_a_config_rollout_takes_effect_without_a_restart() {
        let cap = En12830Capability::new(&off());
        assert!(!cap.enabled());
        cap.refresh(&on());
        assert!(cap.enabled());
        cap.refresh(&off());
        assert!(!cap.enabled());
    }

    #[tokio::test]
    async fn a_successful_job_runs_and_releases_the_slot() {
        let cap = En12830Capability::new(&on());
        let slot = Arc::new(AdapterSlot::new());
        let publisher = RecordingPublisher::default();
        let v = cap
            .run_contained(&slot, &mac(A), "beacon-history", &publisher, || Ok(42u32))
            .await
            .expect("runs");
        assert_eq!(v, 42);
        assert_eq!(slot.busy_with(), None);
    }

    #[tokio::test]
    async fn a_busy_adapter_is_already_exists_and_the_job_never_runs() {
        let cap = En12830Capability::new(&on());
        let slot = Arc::new(AdapterSlot::new());
        let publisher = RecordingPublisher::default();
        let _held = slot.claim(mac(B)).expect("someone else is dialling");
        let err = cap
            .run_contained::<(), _>(&slot, &mac(A), "beacon-history", &publisher, || {
                unreachable!("the job must never run")
            })
            .await
            .expect_err("must refuse");
        assert_eq!(err.kind(), Kind::AlreadyExists);
        assert_eq!(err.component(), Component::BluetoothAdapter);
    }

    #[tokio::test]
    async fn a_panicking_job_is_contained_and_typed() {
        let cap = En12830Capability::new(&on());
        let slot = Arc::new(AdapterSlot::new());
        let publisher = RecordingPublisher::default();
        let err = cap
            .run_contained::<(), _>(&slot, &mac(A), "beacon-history", &publisher, || {
                panic!("the vendor protocol did something surprising")
            })
            .await
            .expect_err("the panic becomes an error, not an unwind");
        assert_eq!(err.kind(), Kind::Transport);
        assert_eq!(err.component(), Component::L2cap);
        assert!(
            err.message().contains("panicked (contained)"),
            "{}",
            err.message()
        );
        assert_eq!(
            slot.busy_with(),
            None,
            "the slot is released even when the job panicked"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hung_job_times_out_and_releases_the_slot_later_not_early() {
        let cap = En12830Capability::with_deadline(&on(), Duration::from_millis(50));
        let slot = Arc::new(AdapterSlot::new());
        let publisher = RecordingPublisher::default();
        let (release, blocked) = std::sync::mpsc::channel::<()>();

        let err = cap
            .run_contained::<(), _>(&slot, &mac(A), "beacon-history", &publisher, move || {
                // Stands in for a beacon that stopped answering mid-download: the thread is alive
                // and still owns the socket.
                let _ = blocked.recv();
                Ok(())
            })
            .await
            .expect_err("must time out");
        assert_eq!(err.kind(), Kind::Timeout);
        assert_eq!(err.component(), Component::L2cap);
        assert_eq!(
            slot.busy_with(),
            Some(mac(A)),
            "the slot is STILL claimed: the orphaned thread has the socket, and a second dial \
             would race a session BlueZ cannot see"
        );

        // Let the runaway job finish; only then may the slot come free.
        release.send(()).expect("release");
        let mut freed = false;
        for _ in 0..200 {
            if slot.busy_with().is_none() {
                freed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            freed,
            "the detached task must release the claim once the thread ends"
        );
    }

    #[tokio::test]
    async fn only_protocol_failures_count_toward_the_alarm_and_it_never_auto_disables() {
        let cap = En12830Capability::new(&on()); // alarm_after = 3
        let slot = Arc::new(AdapterSlot::new());
        let publisher = RecordingPublisher::default();

        // RF and platform facts do not count, however many of them there are.
        for kind in [Kind::BeaconUnreachable, Kind::Timeout, Kind::AdapterDown] {
            for _ in 0..10 {
                let _ = cap
                    .run_contained::<(), _>(
                        &slot,
                        &mac(A),
                        "beacon-history",
                        &publisher,
                        move || Err(Error::new(kind, Role::Central, Component::L2cap, "not us")),
                    )
                    .await;
            }
        }
        assert_eq!(cap.consecutive_failures(&mac(A)), 0);
        assert!(publisher.degraded().is_empty(), "no alarm from RF trouble");

        // Protocol failures do.
        for i in 1..=2 {
            let _ = cap
                .run_contained::<(), _>(&slot, &mac(A), "beacon-history", &publisher, || {
                    Err(transport("garbage response code"))
                })
                .await;
            assert_eq!(cap.consecutive_failures(&mac(A)), i);
            assert!(publisher.degraded().is_empty(), "not yet at the threshold");
        }
        let _ = cap
            .run_contained::<(), _>(&slot, &mac(A), "beacon-history", &publisher, || {
                Err(transport("garbage response code"))
            })
            .await;
        assert_eq!(cap.consecutive_failures(&mac(A)), 3);
        let reports = publisher.degraded();
        assert_eq!(reports.len(), 1, "edge-triggered, one report");
        assert_eq!(reports[0].condition, CONDITION_PROTOCOL_FAILING);
        assert!(reports[0].active);
        assert_eq!(reports[0].mac.as_deref(), Some(A));
        assert!(cap.is_alarmed(&mac(A)));

        // Still enabled. This is the point: auto-disabling would hide the breakage.
        assert!(cap.enabled(), "the capability NEVER auto-disables");

        // Further failures do not re-report.
        let _ = cap
            .run_contained::<(), _>(&slot, &mac(A), "beacon-history", &publisher, || {
                Err(transport("still broken"))
            })
            .await;
        assert_eq!(publisher.degraded().len(), 1, "still edge-triggered");

        // A success clears the streak and reports the recovery.
        cap.run_contained(&slot, &mac(A), "beacon-history", &publisher, || Ok(()))
            .await
            .expect("works again");
        assert_eq!(cap.consecutive_failures(&mac(A)), 0);
        assert!(!cap.is_alarmed(&mac(A)));
        let reports = publisher.degraded();
        assert_eq!(reports.len(), 2);
        assert!(!reports[1].active, "the clear is published too");
    }

    #[tokio::test]
    async fn the_failure_streak_is_per_beacon() {
        let cap = En12830Capability::new(&on());
        let slot = Arc::new(AdapterSlot::new());
        let publisher = RecordingPublisher::default();
        for _ in 0..3 {
            let _ = cap
                .run_contained::<(), _>(&slot, &mac(A), "beacon-history", &publisher, || {
                    Err(transport("broken"))
                })
                .await;
        }
        assert_eq!(cap.consecutive_failures(&mac(A)), 3);
        assert_eq!(
            cap.consecutive_failures(&mac(B)),
            0,
            "one bad beacon does not alarm the fleet"
        );
        assert!(cap.is_alarmed(&mac(A)));
        assert!(!cap.is_alarmed(&mac(B)));
        assert_eq!(cap.worst_streak(), 3);
    }

    #[test]
    fn an_alarm_threshold_of_zero_is_clamped_to_one() {
        // A zero would alarm before any failure had happened.
        let cap = En12830Capability::new(&En12830Config {
            enabled: true,
            protocol_failure_alarm_after: 0,
        });
        assert_eq!(cap.alarm_after(), 1);
    }

    #[test]
    fn the_classification_is_one_function_so_it_can_be_traced() {
        assert!(En12830Capability::counts_toward_alarm(Kind::Transport));
        for kind in [
            Kind::BeaconUnreachable,
            Kind::Timeout,
            Kind::AdapterDown,
            Kind::CapabilityDisabled,
            Kind::AlreadyExists,
            Kind::PairingRejected,
            Kind::NotFound,
        ] {
            assert!(
                !En12830Capability::counts_toward_alarm(kind),
                "{} must not count toward the protocol alarm",
                kind.as_str()
            );
        }
    }
}
