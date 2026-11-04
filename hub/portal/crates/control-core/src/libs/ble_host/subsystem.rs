//! `SubsystemControl` and `StandbyGate`.
//!
//! **The v1 defect this closes.** `applier.rs:2009` carried the comment "the thread is spawned once
//! at boot, so flipping the flag cannot start it": enabling `eye` on a running v1 Hub wrote the
//! config and changed nothing until the next restart. The crate's scan loop is signalled rather
//! than spawned, so `ensure_scanning()` is real — and AC 4 can require that the loop is running by
//! the time an enrolment response is sent.
//!
//! `CentralHandle` already implements `SubsystemControl` inside the crate, so the wrapper here
//! exists only for the ordering problem in `main.rs`: the `Host` has to be built before
//! `tasks::spawn` returns the handle it would contain. `DeferredSubsystem` is that hole, filled
//! once, immediately after the spawn.

use std::sync::{Arc, OnceLock};

use control_ble::error::{Component, Error, Kind, Result};
use control_ble::host::{StandbyGate, SubsystemControl};
use control_ble::tasks::CentralHandle;

/// The `SubsystemControl` a `Host` is built with, before the runtime that satisfies it exists.
///
/// Until `fill` is called every method is a no-op that reports "not scanning" — which is the honest
/// answer during the few milliseconds of start-up in which it is true, and the correct answer
/// forever on a Hub whose mode excludes the central role (`peripheral`, `off`), where `fill` is
/// never called at all.
#[derive(Default)]
pub struct DeferredSubsystem {
    inner: OnceLock<CentralHandle>,
}

impl DeferredSubsystem {
    pub fn new() -> Arc<DeferredSubsystem> {
        Arc::new(DeferredSubsystem::default())
    }

    /// Install the handle. Returns false if one was already installed — the caller has a bug, not a
    /// race, and silently replacing it would leave two views of "is it scanning".
    pub fn fill(&self, handle: CentralHandle) -> bool {
        self.inner.set(handle).is_ok()
    }

    pub fn handle(&self) -> Option<&CentralHandle> {
        self.inner.get()
    }
}

impl SubsystemControl for DeferredSubsystem {
    fn ensure_scanning(&self) -> Result<bool> {
        match self.inner.get() {
            Some(h) => Ok(h.start_scan()),
            None => Err(Error::new(
                Kind::CentralUnavailable,
                None,
                Component::BluetoothAdapter,
                "the central role is not running on this Hub, so scanning cannot be started",
            )),
        }
    }

    fn stop_scanning(&self) {
        if let Some(h) = self.inner.get() {
            h.stop_scan();
        }
    }

    fn is_scanning(&self) -> bool {
        self.inner.get().is_some_and(|h| h.is_scanning())
    }

    fn forget_beacon(&self, mac: &control_ble::mac::MacAddr) {
        if let Some(h) = self.inner.get() {
            SubsystemControl::forget_beacon(h, mac);
        }
    }
}

/// `StandbyGate` over the agent's one standby signal.
///
/// Standby means an operator pressed the button to make the device quiet. The scan loop consults
/// this every tick: continuing to radiate — and to publish — through a deliberate standby would
/// make the button a lie.
pub struct PowerStandby;

impl StandbyGate for PowerStandby {
    fn in_standby(&self) -> bool {
        crate::libs::power::standby::is_standby()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unfilled_subsystem_reports_not_scanning_rather_than_pretending() {
        let s = DeferredSubsystem::default();
        assert!(!s.is_scanning());
        s.stop_scanning(); // must not panic
        let err = s
            .ensure_scanning()
            .expect_err("cannot start what is not there");
        assert_eq!(err.kind(), Kind::CentralUnavailable);
        assert!(
            err.message().contains("central role"),
            "and says why: {}",
            err.message()
        );
    }

    #[test]
    fn the_standby_gate_follows_the_process_wide_signal() {
        // Round-trips the real signal rather than a copy: two views of standby is the bug this
        // guards against.
        let gate = PowerStandby;
        let was = gate.in_standby();
        if was {
            crate::libs::power::standby::resume();
        }
        assert!(!gate.in_standby());
        crate::libs::power::standby::request_standby();
        assert!(gate.in_standby());
        crate::libs::power::standby::resume();
        assert!(!gate.in_standby());
        if was {
            crate::libs::power::standby::request_standby();
        }
    }
}
