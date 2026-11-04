//! Archive history jobs — `plan/portal/beacon_history.md` §2.1–2.4.
//!
//! A `beacon-history` request becomes a [`HistoryRequest`]: an optional window, the caller's job
//! id (or one the Hub makes), and the retry count. Its progress goes out on
//! `proximos/{hostname}/eye/history` as [`HistoryMessage`]s, not retained: `requested` →
//! `receiving` → `records` batches → `done` | `failed`.
//!
//! The retry ladder is Hub-owned (§2.4) and lives in memory only: a failure that a later attempt
//! can fix (`unreachable`, `busy`, `deadline`) is re-queued on the next advertisement heard from
//! that Beacon, no sooner than 5, 15 and 60 minutes after the 1st, 2nd and 3rd failure; the 4th
//! failure is final. A Hub restart loses the ladder, and the Portal's silence timeout covers that.

use std::collections::BTreeMap;

use crate::error::{Component, Error, Kind, Result, Role};
use crate::mac::MacAddr;

/// At most this many records ride one `records` message (≈ 5 KB).
pub const RECORDS_PER_MESSAGE: usize = 240;
/// At most one `receiving` message per this many seconds.
pub const RECEIVING_EVERY_S: u64 = 5;
/// The ladder: how long after the 1st, 2nd and 3rd failure a retry may start. A 4th failure is
/// final, so a job makes at most four attempts.
pub const RETRY_DELAYS_S: [u64; 3] = [5 * 60, 15 * 60, 60 * 60];
/// The longest job id accepted from a caller.
pub const MAX_JOB_ID_LEN: usize = 64;

/// One archive download as requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRequest {
    /// Unix seconds. `None` resumes from the newest archive row this Hub already holds.
    pub from: Option<i64>,
    /// Unix seconds. `None` is "up to now".
    pub to: Option<i64>,
    /// The Portal's job id, echoed in every message, or a Hub-made `bh-hub-<unix>-<rand>`.
    pub job: String,
    /// How many earlier attempts of this job failed.
    pub retry: u8,
    /// Why the previous attempt failed, for the `requested` message of a retry.
    pub last_reason: Option<FailReason>,
}

impl HistoryRequest {
    /// Validate a caller's request. A missing `job` gets a Hub-made id.
    pub fn new(
        from: Option<i64>,
        to: Option<i64>,
        job: Option<String>,
        now: i64,
    ) -> Result<HistoryRequest> {
        for (key, v) in [("from", from), ("to", to)] {
            if v.is_some_and(|v| !(0..=i64::from(u32::MAX)).contains(&v)) {
                return Err(invalid(format!(
                    "beacon-history `{key}` must be Unix seconds within 0..=4294967295"
                )));
            }
        }
        if let (Some(f), Some(t)) = (from, to) {
            if f > t {
                return Err(invalid(format!(
                    "beacon-history window is empty: from {f} is after to {t}"
                )));
            }
        }
        let job = match job {
            Some(j) => {
                let j = j.trim().to_string();
                if j.is_empty()
                    || j.len() > MAX_JOB_ID_LEN
                    || !j.chars().all(|c| c.is_ascii_graphic())
                {
                    return Err(invalid(format!(
                        "beacon-history `job` must be 1..={MAX_JOB_ID_LEN} printable ASCII \
                         characters without spaces"
                    )));
                }
                j
            }
            None => hub_job_id(now),
        };
        Ok(HistoryRequest {
            from,
            to,
            job,
            retry: 0,
            last_reason: None,
        })
    }

    /// A message about this job, from the Beacon `mac`.
    pub fn message(&self, mac: &MacAddr, body: HistoryBody) -> HistoryMessage {
        HistoryMessage {
            job: self.job.clone(),
            mac: mac.to_string(),
            from: self.from,
            to: self.to,
            retry: self.retry,
            body,
        }
    }

    /// The `requested` message: queued on the dial worker (or, for a retry, waiting for the
    /// Beacon to be heard again), with the reason the previous attempt failed.
    pub fn requested(&self, mac: &MacAddr) -> HistoryMessage {
        self.message(
            mac,
            HistoryBody::Requested {
                reason: self.last_reason,
            },
        )
    }
}

fn invalid(message: String) -> Error {
    Error::new(
        Kind::InvalidArgument,
        Role::Central,
        Component::BeaconGatt,
        message,
    )
}

/// `bh-hub-<unix>-<8 hex>` — distinct from the Portal's `bh-<ulid>` at a glance.
pub fn hub_job_id(now: i64) -> String {
    format!("bh-hub-{now}-{:08x}", rand::random::<u32>())
}

/// `bh-hub-drain-<unix>-<8 hex>` — the download a recording change runs first, because a change
/// erases the Beacon's archive (beacon_history.md §8, amended 2026-10-09).
pub fn drain_job_id(now: i64) -> String {
    format!("bh-hub-drain-{now}-{:08x}", rand::random::<u32>())
}

/// Why a job failed — the `reason` of devices.beacon.md §3.8's "nelze stáhnout".
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailReason {
    /// The Beacon (or this Hub's controller) did not answer.
    Unreachable,
    /// The Beacon answered something the archive protocol cannot use.
    Protocol,
    /// The adapter was taken by another connection, or the dial queue was full.
    Busy,
    /// The Beacon is not recording, so there is no archive to read; the Hub does not start it.
    NotRecording,
    /// The download ran past its deadline.
    Deadline,
    /// This Hub is not the Beacon's home in the cluster, so it does not dial it (decision #42).
    /// Final: the portal asks the home Hub again, from its own cursor.
    NotHome,
}

impl FailReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            FailReason::Unreachable => "unreachable",
            FailReason::Protocol => "protocol",
            FailReason::Busy => "busy",
            FailReason::NotRecording => "not_recording",
            FailReason::Deadline => "deadline",
            FailReason::NotHome => "not_home",
        }
    }

    /// Whether a later attempt can succeed where this one failed. A protocol failure or a Beacon
    /// that does not record stays that way until someone acts.
    pub const fn retryable(self) -> bool {
        matches!(
            self,
            FailReason::Unreachable | FailReason::Busy | FailReason::Deadline
        )
    }

    /// Classify a failed attempt.
    pub fn of(e: &Error) -> FailReason {
        if is_not_recording(e) {
            return FailReason::NotRecording;
        }
        match e.kind() {
            Kind::BeaconUnreachable | Kind::AdapterDown | Kind::ServiceDown => {
                FailReason::Unreachable
            }
            Kind::AlreadyExists => FailReason::Busy,
            Kind::Timeout => FailReason::Deadline,
            _ => FailReason::Protocol,
        }
    }
}

const NOT_RECORDING: &str = "is not recording";

/// The refusal for a Beacon whose record info says it is not recording.
pub fn not_recording_error(mac: &MacAddr) -> Error {
    Error::new(
        Kind::NotFound,
        Role::Central,
        Component::BeaconGatt,
        format!(
            "{mac} {NOT_RECORDING}: there is no archive to read, and a download would restart \
             recording on a Beacon someone stopped"
        ),
    )
}

pub fn is_not_recording(e: &Error) -> bool {
    e.kind() == Kind::NotFound && e.message().contains(NOT_RECORDING)
}

/// `floor((to - from) / interval) + 1` — the most records a window can hold. Both ends are
/// inclusive (`in_window`), so a 60-min window at 5 min holds 13. `None` when the start is
/// unknown (no `from` and nothing stored yet) or the window is empty.
pub fn expected_records(from: i64, to: i64, interval_s: u16) -> Option<u64> {
    if from <= 0 || to <= from || interval_s == 0 {
        return None;
    }
    let span = u64::try_from(to - from).ok()?;
    Some(span / u64::from(interval_s) + 1)
}

/// One `eye/history` message. `job`, `mac`, `from`, `to` and `retry` ride every state; the rest is
/// the state's own body.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct HistoryMessage {
    pub job: String,
    pub mac: String,
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub retry: u8,
    #[serde(flatten)]
    pub body: HistoryBody,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum HistoryBody {
    Requested {
        reason: Option<FailReason>,
    },
    Receiving {
        received: u64,
        expected: Option<u64>,
        reason: Option<FailReason>,
    },
    Records {
        /// 1-based, in the order sent; `done.batches` says how many there were.
        seq: u32,
        interval_s: u16,
        records: Vec<(i64, f64)>,
    },
    Done {
        received: u64,
        expected: Option<u64>,
        batches: u32,
        sessions: u16,
        rejected_chunks: u16,
        future_dropped: u64,
        /// `null` until the block CRC polynomial is known (§7 Q6 layer 2) — never a guessed 0.
        crc_failed: Option<u64>,
        restart_ok: bool,
        reason: Option<FailReason>,
    },
    Failed {
        reason: FailReason,
    },
}

/// The `records` messages of one job, sent while the download runs (beacon_history.md §2.3):
/// each chunk's new records go out at once, at most [`RECORDS_PER_MESSAGE`] per message, and `seq`
/// counts on across chunks from 1.
#[derive(Debug, Default)]
pub struct RecordStream {
    /// `records` messages made so far — `done`'s `batches`.
    pub batches: u32,
    /// Records sent so far.
    pub sent: u64,
}

impl RecordStream {
    pub fn batches(&mut self, records: &[(i64, f64)], interval_s: u16) -> Vec<HistoryBody> {
        records
            .chunks(RECORDS_PER_MESSAGE)
            .map(|batch| {
                self.batches = self.batches.saturating_add(1);
                self.sent += batch.len() as u64;
                HistoryBody::Records {
                    seq: self.batches,
                    interval_s,
                    records: batch.to_vec(),
                }
            })
            .collect()
    }
}

/// Lets one `receiving` through per [`RECEIVING_EVERY_S`], on the monotonic clock.
#[derive(Debug, Default)]
pub struct Throttle {
    last: Option<u64>,
}

impl Throttle {
    pub fn ready(&mut self, now_mono: u64) -> bool {
        if self
            .last
            .is_some_and(|l| now_mono.saturating_sub(l) < RECEIVING_EVERY_S)
        {
            return false;
        }
        self.last = Some(now_mono);
        true
    }
}

/// A retry waiting for its Beacon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRetry {
    pub request: HistoryRequest,
    /// Monotonic seconds ([`crate::host::SystemClock::monotonic_secs`]) before which it waits.
    pub not_before: u64,
}

/// What a failed attempt leads to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterFailure {
    /// Re-queued on the next advertisement at or after `not_before`, as `request`.
    Retry(PendingRetry),
    /// Reported `failed` for good.
    Final,
}

/// The ladder's state: at most one waiting retry per Beacon. In memory only.
#[derive(Debug, Clone, Default)]
pub struct HistoryRetries {
    pending: BTreeMap<MacAddr, PendingRetry>,
}

impl HistoryRetries {
    /// Record a failed attempt of `request` and decide what follows.
    pub fn on_failure(
        &mut self,
        mac: MacAddr,
        request: &HistoryRequest,
        reason: FailReason,
        now_mono: u64,
    ) -> AfterFailure {
        self.pending.remove(&mac);
        if !reason.retryable() {
            return AfterFailure::Final;
        }
        // `retry` earlier failures, so this is failure number `retry + 1`.
        let Some(delay) = RETRY_DELAYS_S.get(usize::from(request.retry)) else {
            return AfterFailure::Final;
        };
        let pending = PendingRetry {
            request: HistoryRequest {
                retry: request.retry.saturating_add(1),
                last_reason: Some(reason),
                ..request.clone()
            },
            not_before: now_mono.saturating_add(*delay),
        };
        self.pending.insert(mac, pending.clone());
        AfterFailure::Retry(pending)
    }

    /// The Beacon was heard: hand back its retry if one is due.
    pub fn take_due(&mut self, mac: &MacAddr, now_mono: u64) -> Option<HistoryRequest> {
        if self.pending.get(mac)?.not_before > now_mono {
            return None;
        }
        self.pending.remove(mac).map(|p| p.request)
    }

    /// Put a due retry back, for the next advertisement (the dial queue was full).
    pub fn put_back(&mut self, mac: MacAddr, request: HistoryRequest, now_mono: u64) {
        self.pending.insert(
            mac,
            PendingRetry {
                request,
                not_before: now_mono,
            },
        );
    }

    /// A new request for the Beacon supersedes a waiting retry.
    pub fn cancel(&mut self, mac: &MacAddr) -> Option<HistoryRequest> {
        self.pending.remove(mac).map(|p| p.request)
    }

    pub fn pending(&self, mac: &MacAddr) -> Option<&PendingRetry> {
        self.pending.get(mac)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{FakeClock, SystemClock};

    fn mac() -> MacAddr {
        MacAddr::parse("7C:D9:F4:10:6E:30").expect("mac")
    }

    fn unreachable() -> Error {
        Error::new(
            Kind::BeaconUnreachable,
            Role::Central,
            Component::BeaconGatt,
            "no answer",
        )
    }

    #[test]
    fn a_request_without_a_job_gets_a_hub_made_id_and_old_callers_need_nothing_else() {
        let r = HistoryRequest::new(None, None, None, 1_791_500_000).expect("valid");
        assert!(r.job.starts_with("bh-hub-1791500000-"), "{}", r.job);
        assert_eq!(r.job.len(), "bh-hub-1791500000-".len() + 8);
        assert_eq!((r.from, r.to, r.retry), (None, None, 0));
        let other = HistoryRequest::new(None, None, None, 1_791_500_000).expect("valid");
        assert_ne!(r.job, other.job, "two requests in one second are two jobs");
    }

    #[test]
    fn a_window_and_a_portal_job_id_are_kept_as_given() {
        let r = HistoryRequest::new(
            Some(1_791_500_000),
            Some(1_791_586_400),
            Some("bh-01J9ZQ".into()),
            0,
        )
        .expect("valid");
        assert_eq!(r.from, Some(1_791_500_000));
        assert_eq!(r.to, Some(1_791_586_400));
        assert_eq!(r.job, "bh-01J9ZQ");
    }

    #[test]
    fn an_empty_window_or_a_malformed_job_is_refused() {
        for (from, to, job) in [
            (Some(10), Some(5), None),
            (Some(-1), None, None),
            (None, Some(i64::from(u32::MAX) + 1), None),
            (None, None, Some(String::new())),
            (None, None, Some("has space".into())),
            (None, None, Some("x".repeat(MAX_JOB_ID_LEN + 1))),
        ] {
            let err = HistoryRequest::new(from, to, job.clone(), 0).expect_err("refused");
            assert_eq!(err.kind(), Kind::InvalidArgument, "{from:?} {to:?} {job:?}");
        }
        assert!(
            HistoryRequest::new(Some(5), Some(5), None, 0).is_ok(),
            "one instant"
        );
    }

    #[test]
    fn failures_are_classified_into_the_five_reasons() {
        let k = |kind| Error::new(kind, Role::Central, Component::L2cap, "x");
        assert_eq!(FailReason::of(&unreachable()), FailReason::Unreachable);
        assert_eq!(
            FailReason::of(&k(Kind::AdapterDown)),
            FailReason::Unreachable
        );
        assert_eq!(FailReason::of(&k(Kind::AlreadyExists)), FailReason::Busy);
        assert_eq!(FailReason::of(&k(Kind::Timeout)), FailReason::Deadline);
        assert_eq!(FailReason::of(&k(Kind::Transport)), FailReason::Protocol);
        assert_eq!(
            FailReason::of(&k(Kind::CapabilityDisabled)),
            FailReason::Protocol
        );
        assert_eq!(
            FailReason::of(&not_recording_error(&mac())),
            FailReason::NotRecording
        );
        // A standard beacon's "not a recorder" is NotFound too, and is not "not recording".
        assert_eq!(
            FailReason::of(&k(Kind::NotFound)),
            FailReason::Protocol,
            "only the not-recording refusal maps to not_recording"
        );
    }

    #[test]
    fn the_ladder_waits_5_15_60_minutes_and_the_fourth_failure_is_final() {
        let clock = FakeClock::at(1_791_500_000);
        let mut ladder = HistoryRetries::default();
        let mut req = HistoryRequest::new(None, None, Some("bh-1".into()), 0).expect("valid");

        for (attempt, delay) in RETRY_DELAYS_S.iter().enumerate() {
            let now = clock.monotonic_secs();
            let AfterFailure::Retry(p) =
                ladder.on_failure(mac(), &req, FailReason::Unreachable, now)
            else {
                panic!("failure {} must be retried", attempt + 1);
            };
            assert_eq!(p.not_before, now + delay);
            assert_eq!(usize::from(p.request.retry), attempt + 1);
            assert_eq!(p.request.job, "bh-1", "a retry keeps the job id");
            assert_eq!(p.request.last_reason, Some(FailReason::Unreachable));

            // Heard too early: nothing.
            clock.advance(i64::try_from(*delay).expect("fits") - 1);
            assert_eq!(ladder.take_due(&mac(), clock.monotonic_secs()), None);
            // Heard once the wait is over: the retry, once.
            clock.advance(1);
            req = ladder
                .take_due(&mac(), clock.monotonic_secs())
                .expect("due");
            assert_eq!(ladder.take_due(&mac(), clock.monotonic_secs()), None);
        }
        assert_eq!(req.retry, 3);
        assert_eq!(
            ladder.on_failure(mac(), &req, FailReason::Deadline, clock.monotonic_secs()),
            AfterFailure::Final,
            "the 4th failure is failed for good"
        );
        assert!(ladder.pending(&mac()).is_none());
    }

    #[test]
    fn protocol_and_not_recording_are_never_retried() {
        let mut ladder = HistoryRetries::default();
        let req = HistoryRequest::new(None, None, None, 0).expect("valid");
        for reason in [FailReason::Protocol, FailReason::NotRecording] {
            assert_eq!(
                ladder.on_failure(mac(), &req, reason, 0),
                AfterFailure::Final
            );
        }
        for reason in [
            FailReason::Unreachable,
            FailReason::Busy,
            FailReason::Deadline,
        ] {
            assert!(matches!(
                ladder.on_failure(mac(), &req, reason, 0),
                AfterFailure::Retry(_)
            ));
        }
    }

    #[test]
    fn the_ladder_runs_on_the_monotonic_clock_so_a_wall_clock_step_does_not_release_it() {
        let clock = FakeClock::at(1_000);
        let mut ladder = HistoryRetries::default();
        let req = HistoryRequest::new(None, None, None, 0).expect("valid");
        ladder.on_failure(mac(), &req, FailReason::Busy, clock.monotonic_secs());
        clock.set(1_000 + 86_400);
        assert_eq!(ladder.take_due(&mac(), clock.monotonic_secs()), None);
        clock.advance_monotonic(RETRY_DELAYS_S[0]);
        assert!(ladder.take_due(&mac(), clock.monotonic_secs()).is_some());
    }

    #[test]
    fn a_new_request_cancels_a_waiting_retry_and_put_back_makes_it_due_at_once() {
        let mut ladder = HistoryRetries::default();
        let req = HistoryRequest::new(None, None, None, 0).expect("valid");
        ladder.on_failure(mac(), &req, FailReason::Busy, 0);
        assert!(ladder.cancel(&mac()).is_some());
        assert!(ladder.pending(&mac()).is_none());

        ladder.put_back(mac(), req.clone(), 100);
        assert_eq!(ladder.take_due(&mac(), 100), Some(req));
    }

    #[test]
    fn expected_counts_both_ends_of_the_window_or_null() {
        assert_eq!(expected_records(1_000, 1_900, 900), Some(2));
        assert_eq!(expected_records(1_000, 1_899, 900), Some(1));
        assert_eq!(expected_records(1_000, 1_901, 900), Some(2));
        assert_eq!(
            expected_records(1_791_500_000, 1_791_586_400, 900),
            Some(97)
        );
        assert_eq!(expected_records(1, 3_601, 300), Some(13), "1 h at 5 min");
        assert_eq!(expected_records(0, 1_900, 900), None, "unknown start");
        assert_eq!(expected_records(2_000, 1_900, 900), None, "empty window");
        assert_eq!(expected_records(1_000, 1_900, 0), None);
    }

    #[test]
    fn receiving_is_let_through_at_most_once_per_five_seconds() {
        let mut t = Throttle::default();
        assert!(t.ready(100), "the first one goes at once");
        assert!(!t.ready(101));
        assert!(!t.ready(104));
        assert!(t.ready(105));
        assert!(!t.ready(106));
    }

    #[test]
    fn records_go_out_in_batches_of_at_most_240() {
        let records: Vec<(i64, f64)> = (0..500).map(|i| (1_000 + i * 900, 4.0)).collect();
        let mut stream = RecordStream::default();
        let batches = stream.batches(&records, 900);
        let sizes: Vec<usize> = batches
            .iter()
            .map(|b| match b {
                HistoryBody::Records { records, .. } => records.len(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(sizes, vec![240, 240, 20]);
        assert!(matches!(batches[2], HistoryBody::Records { seq: 3, .. }));
        assert!(
            stream.batches(&[], 900).is_empty(),
            "a chunk with nothing new sends nothing"
        );
        // The next chunk's batch carries on the numbering.
        let next = stream.batches(&records[..1], 900);
        assert!(matches!(next[..], [HistoryBody::Records { seq: 4, .. }]));
        assert_eq!((stream.batches, stream.sent), (4, 501));
    }

    #[test]
    fn the_wire_shapes_are_the_ones_beacon_history_md_specifies() {
        let req = HistoryRequest::new(
            Some(1_791_500_000),
            Some(1_791_586_400),
            Some("bh-x".into()),
            0,
        )
        .expect("valid");
        let v = |body| serde_json::to_value(req.message(&mac(), body)).expect("serializes");

        assert_eq!(
            v(HistoryBody::Receiving {
                received: 240,
                expected: Some(1344),
                reason: None
            }),
            serde_json::json!({"job": "bh-x", "mac": "7C:D9:F4:10:6E:30", "state": "receiving",
                "received": 240, "expected": 1344, "from": 1_791_500_000, "to": 1_791_586_400,
                "retry": 0, "reason": null})
        );
        assert_eq!(
            v(HistoryBody::Records {
                seq: 3,
                interval_s: 900,
                records: vec![(1_791_500_900, 22.81), (1_791_501_800, 22.79)]
            }),
            serde_json::json!({"job": "bh-x", "mac": "7C:D9:F4:10:6E:30", "state": "records",
                "seq": 3, "interval_s": 900, "from": 1_791_500_000, "to": 1_791_586_400,
                "retry": 0, "records": [[1_791_500_900, 22.81], [1_791_501_800, 22.79]]})
        );
        let done = v(HistoryBody::Done {
            received: 96,
            expected: Some(96),
            batches: 1,
            sessions: 2,
            rejected_chunks: 0,
            future_dropped: 0,
            crc_failed: None,
            restart_ok: true,
            reason: None,
        });
        assert_eq!(done["state"], "done");
        for key in ["sessions", "rejected_chunks", "future_dropped"] {
            assert!(done[key].is_number(), "{key}: {done}");
        }
        assert!(
            done.get("crc_failed").is_some_and(|c| c.is_null()),
            "crc_failed is present and null, never 0: {done}"
        );
        assert_eq!(
            v(HistoryBody::Failed {
                reason: FailReason::NotRecording
            })["reason"],
            "not_recording"
        );
        let mut retried = req.clone();
        retried.retry = 2;
        retried.last_reason = Some(FailReason::Unreachable);
        let r = serde_json::to_value(retried.requested(&mac())).expect("serializes");
        assert_eq!(
            (
                r["state"].as_str(),
                r["retry"].as_u64(),
                r["reason"].as_str()
            ),
            (Some("requested"), Some(2), Some("unreachable"))
        );
        for reason in [
            FailReason::Unreachable,
            FailReason::Protocol,
            FailReason::Busy,
            FailReason::NotRecording,
            FailReason::Deadline,
        ] {
            assert_eq!(
                serde_json::to_value(reason).expect("serializes"),
                reason.as_str()
            );
        }
    }
}
