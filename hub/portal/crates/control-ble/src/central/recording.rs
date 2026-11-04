//! Holding a provisioned recorder to its archive cadence — `plan/portal/beacon_history.md` §3,
//! §8 (amended 2026-10-09).
//!
//! The recording dial that follows a provisioning session used to live only in the dial queue, so a
//! restart (or a full queue, or one failed dial) left the Beacon at whatever cadence it had. Now
//! the Hub compares what the Beacon last reported (persisted in the ledger) with its target on
//! every advertisement and dials when they differ — on the same ladder as an archive download
//! (§2.4): no sooner than 5, 15 and 60 minutes after the 1st, 2nd and 3rd failure, and the 4th
//! failure gives up. The ladder is in memory: a restart or a new target starts it afresh.
//!
//! **A recording change erases the archive** (§8, amended 2026-10-09): a START at another interval,
//! or on a stopped recorder, wipes every record on the Beacon. [`plan_recording`] decides from the
//! record info whether a dial sends nothing, a stop, or a start that the archive must be read
//! (drained) before.

use std::collections::BTreeMap;

use crate::mac::MacAddr;

use super::en12830::protocol::RecordInfo;
use super::history::RETRY_DELAYS_S;

/// What a recording dial does once it has read the record info.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingStep {
    /// The Beacon already holds the target: nothing is sent, so nothing is erased.
    Hold,
    /// Stop a recording Beacon. A stop keeps the archive (every download stops it), so nothing is
    /// read first; the start that ends the stop drains it.
    Stop,
    /// Read the archive, then start at the target. `restart_s` is what the download leaves the
    /// Beacon recording at until the start: its own interval (the same interval keeps the
    /// archive), or `None` for a stopped recorder, which stays stopped until the start.
    DrainThenStart { restart_s: Option<u16> },
}

/// The step that takes a Beacon reading `info` to `target_s` (seconds; `0` stops).
pub fn plan_recording(info: &RecordInfo, target_s: u16) -> RecordingStep {
    if target_s == 0 {
        return if info.is_recording {
            RecordingStep::Stop
        } else {
            RecordingStep::Hold
        };
    }
    if info.is_recording && info.interval_s == target_s {
        return RecordingStep::Hold;
    }
    RecordingStep::DrainThenStart {
        restart_s: (info.is_recording && info.interval_s != 0).then_some(info.interval_s),
    }
}

/// One Beacon's recording dials towards one target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordingRetry {
    /// The cadence being set, in minutes; `0` stops recording.
    pub target_min: u16,
    /// Failed dials towards this target so far.
    pub failures: u8,
    /// Monotonic seconds before which no dial is queued.
    pub not_before: u64,
    /// A dial is queued or running: an advertisement must not queue another.
    pub in_flight: bool,
    /// The ladder is spent; nothing is queued until the target changes or the Hub restarts.
    pub gave_up: bool,
}

/// What a failed recording dial leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterRecordingFailure {
    /// Re-queued on an advertisement this many seconds from now, at the earliest.
    RetryIn(u64),
    GaveUp,
}

/// The ladder's state, per Beacon.
#[derive(Debug, Clone, Default)]
pub struct RecordingRetries {
    entries: BTreeMap<MacAddr, RecordingRetry>,
}

impl RecordingRetries {
    /// The Beacon was heard and does not hold `target_min`: may a dial be queued now? If so it is
    /// marked in flight. A new target starts a fresh ladder.
    pub fn take_due(&mut self, mac: MacAddr, target_min: u16, now_mono: u64) -> bool {
        match self.entries.get_mut(&mac) {
            Some(e) if e.target_min == target_min => {
                if e.in_flight || e.gave_up || e.not_before > now_mono {
                    return false;
                }
                e.in_flight = true;
                true
            }
            _ => {
                self.started(mac, target_min, now_mono);
                true
            }
        }
    }

    /// A dial towards `target_min` was queued outside an advertisement (after a provisioning
    /// session): keep the advertisement from queueing a second one.
    pub fn started(&mut self, mac: MacAddr, target_min: u16, now_mono: u64) {
        self.entries.insert(
            mac,
            RecordingRetry {
                target_min,
                failures: 0,
                not_before: now_mono,
                in_flight: true,
                gave_up: false,
            },
        );
    }

    /// The queue was full: nothing is in flight, and the next advertisement tries again.
    pub fn not_queued(&mut self, mac: &MacAddr) {
        if let Some(e) = self.entries.get_mut(mac) {
            e.in_flight = false;
        }
    }

    /// The dial set `target_min` and the Beacon read back holding it.
    pub fn on_success(&mut self, mac: &MacAddr) {
        self.entries.remove(mac);
    }

    /// The dial towards `target_min` failed, or the Beacon did not read back holding it. A dial
    /// nobody was tracking (an operator's `beacon-recording`) starts its own ladder here, so the
    /// next advertisement does not dial again at once.
    pub fn on_failure(
        &mut self,
        mac: MacAddr,
        target_min: u16,
        retryable: bool,
        now_mono: u64,
    ) -> AfterRecordingFailure {
        let failures = match self.entries.get(&mac) {
            Some(e) if e.target_min == target_min => e.failures,
            _ => 0,
        }
        .saturating_add(1);
        let delay = RETRY_DELAYS_S
            .get(usize::from(failures - 1))
            .copied()
            .filter(|_| retryable);
        self.entries.insert(
            mac,
            RecordingRetry {
                target_min,
                failures,
                not_before: now_mono.saturating_add(delay.unwrap_or(0)),
                in_flight: false,
                gave_up: delay.is_none(),
            },
        );
        match delay {
            Some(d) => AfterRecordingFailure::RetryIn(d),
            None => AfterRecordingFailure::GaveUp,
        }
    }

    /// The Beacon holds its target: nothing is owed, unless a dial is already on its way.
    pub fn settled(&mut self, mac: &MacAddr) {
        if self.entries.get(mac).is_some_and(|e| !e.in_flight) {
            self.entries.remove(mac);
        }
    }

    pub fn forget(&mut self, mac: &MacAddr) {
        self.entries.remove(mac);
    }

    pub fn get(&self, mac: &MacAddr) -> Option<&RecordingRetry> {
        self.entries.get(mac)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac() -> MacAddr {
        MacAddr::parse("7C:D9:F4:10:6E:30").expect("mac")
    }

    fn info(is_recording: bool, interval_s: u16) -> RecordInfo {
        RecordInfo {
            is_recording,
            interval_s,
            number_of_records: 10,
            start_ts: 1_791_500_000,
        }
    }

    #[test]
    fn only_a_start_that_changes_the_recording_reads_the_archive_first() {
        assert_eq!(plan_recording(&info(true, 900), 900), RecordingStep::Hold);
        assert_eq!(plan_recording(&info(false, 900), 0), RecordingStep::Hold);
        assert_eq!(plan_recording(&info(true, 300), 0), RecordingStep::Stop);
        assert_eq!(
            plan_recording(&info(true, 300), 900),
            RecordingStep::DrainThenStart {
                restart_s: Some(300)
            },
            "an interval change: drained, and left at its own interval until the start"
        );
        assert_eq!(
            plan_recording(&info(false, 900), 900),
            RecordingStep::DrainThenStart { restart_s: None },
            "a stopped recorder at the same interval still erases on its start"
        );
        assert_eq!(
            plan_recording(&info(true, 0), 900),
            RecordingStep::DrainThenStart { restart_s: None },
            "no interval to fall back to: never a START at 0"
        );
    }

    #[test]
    fn a_dial_is_queued_once_and_a_failure_waits_on_the_ladder_until_it_gives_up() {
        let mut r = RecordingRetries::default();
        assert!(r.take_due(mac(), 15, 0));
        assert!(!r.take_due(mac(), 15, 0), "one dial in flight at a time");

        let mut now = 0;
        for delay in RETRY_DELAYS_S {
            assert_eq!(
                r.on_failure(mac(), 15, true, now),
                AfterRecordingFailure::RetryIn(delay)
            );
            assert!(!r.take_due(mac(), 15, now + delay - 1), "the floor holds");
            now += delay;
            assert!(r.take_due(mac(), 15, now));
        }
        assert_eq!(
            r.on_failure(mac(), 15, true, now),
            AfterRecordingFailure::GaveUp,
            "the 4th failure is the last"
        );
        assert!(!r.take_due(mac(), 15, now + 1_000_000));
        assert!(
            r.take_due(mac(), 5, now),
            "a new target starts a fresh ladder"
        );
    }

    #[test]
    fn a_failure_a_retry_cannot_fix_gives_up_at_once() {
        let mut r = RecordingRetries::default();
        assert!(r.take_due(mac(), 15, 0));
        assert_eq!(
            r.on_failure(mac(), 15, false, 0),
            AfterRecordingFailure::GaveUp
        );
        assert!(!r.take_due(mac(), 15, 10_000));
    }

    #[test]
    fn an_untracked_failure_starts_a_ladder_rather_than_an_immediate_redial() {
        let mut r = RecordingRetries::default();
        assert_eq!(
            r.on_failure(mac(), 5, true, 100),
            AfterRecordingFailure::RetryIn(300)
        );
        assert!(!r.take_due(mac(), 5, 101));
        assert!(r.take_due(mac(), 5, 400));
    }

    #[test]
    fn a_full_queue_and_a_settled_beacon_leave_the_right_state() {
        let mut r = RecordingRetries::default();
        assert!(r.take_due(mac(), 15, 0));
        r.not_queued(&mac());
        assert!(
            r.take_due(mac(), 15, 0),
            "the next advertisement tries again"
        );
        r.settled(&mac());
        assert!(r.get(&mac()).is_some(), "a dial on its way is kept");
        r.on_success(&mac());
        assert!(r.get(&mac()).is_none());
        r.started(mac(), 15, 0);
        r.not_queued(&mac());
        r.settled(&mac());
        assert!(r.get(&mac()).is_none());
    }
}
