//! `SystemClock` over the agent's time-sync helpers (FB0B).
//!
//! Setting the clock from a phone is a privileged act with a real hazard: a Hub whose clock jumps
//! backwards re-timestamps readings into the past, and one whose clock jumps forward expires a live
//! provisioning session. The crate handles the second (FB0B re-bases the session idle timer); the
//! first is why `validate_epoch` refuses a value outside the plausible window before anything is
//! applied.

use control_ble::host::SystemClock;

use crate::libs::network::time_sync;

pub struct SystemClockImpl;

impl SystemClock for SystemClockImpl {
    fn now_unix(&self) -> i64 {
        time_sync::now_epoch()
    }

    fn set_time(&self, epoch: i64) -> Result<(), String> {
        time_sync::validate_epoch(epoch)?;
        time_sync::set_system_time(epoch)
    }

    fn synchronized(&self) -> bool {
        time_sync::get_time_status().synchronized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_a_plausible_wall_clock() {
        // Not a tautology: a `SystemClock` that returned 0 would make every reading 1970 and would
        // pass any test that only compared it to itself.
        let now = SystemClockImpl.now_unix();
        assert!(now > 1_700_000_000, "after 2023-11: {now}");
        assert!(now < 4_102_444_800, "before 2100: {now}");
    }

    #[test]
    fn an_implausible_epoch_is_refused_before_anything_is_applied() {
        // The refusal has to come from validation, not from a failed syscall: on a Hub running as
        // root the syscall would succeed and the damage would be done.
        let err = SystemClockImpl.set_time(0).expect_err("refused");
        assert!(!err.is_empty(), "and says why");
        let err = SystemClockImpl.set_time(i64::MAX).expect_err("refused");
        assert!(!err.is_empty());
    }
}
