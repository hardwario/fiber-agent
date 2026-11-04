//! The per-beacon provisioning ledger — `plan/control/ble/central.md` §5, AC 5.
//!
//! **A budget that resets on reboot is not a budget.** This file is where a give-up survives a
//! restart, and it exists as a separate store from `eye.tags[]` for one specific reason, which
//! central.md §5 calls out as a v1 defect:
//!
//! > a *fleet-borrowed* beacon has no local enrollment record, so a give-up cannot be persisted and
//! > the attempt budget restarts after every reboot — which turns a bounded retry into an unbounded
//! > one against a device this Hub does not own.
//!
//! Keying the ledger on the MAC rather than on an enrollment record resolves central.md's open
//! question 4 in the "borrowed beacons *are* auto-provisioned, and their budget is persisted here"
//! direction. Nothing about a borrowed beacon is written into the fleet config, so this Hub still
//! makes no ownership claim on it.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{Component, Error, Kind, Result, Role};
use crate::mac::MacAddr;
use crate::store::{Slot, Store};

use super::history::RETRY_DELAYS_S;
use super::provision::BleConfig;
use super::state::{RecordingReport, ReportedBle};

/// Bumped only for an incompatible layout change; an unknown version is refused, not guessed at.
pub const LEDGER_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BeaconRecord {
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub gave_up_at: Option<i64>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// Tri-state, as central.md §5 requires: `true`, `false`, or **absent** meaning "never
    /// attempted, or written by a build that predates the field".
    #[serde(default)]
    pub provisioned: Option<bool>,
    #[serde(default)]
    pub last_attempt_ts: Option<i64>,
    /// What the beacon reported the last time its configuration was read over GATT, when, and
    /// whether that read verified a profile this Hub had just written. Persisted so the reported
    /// configuration outlives a restart of the component rather than reverting to "not reported"
    /// until the next dial. Absent for a record written by an older build.
    #[serde(default)]
    pub ble_config: Option<BleConfig>,
    #[serde(default)]
    pub ble_config_ts: Option<i64>,
    #[serde(default)]
    pub ble_config_verified: Option<bool>,
    /// Unix seconds before which the Hub does not retry a failed provisioning session by itself;
    /// the retry goes on the beacon's next advertisement at or after it (the history ladder,
    /// beacon_history.md §2.4, amended for provisioning in §8 2026-10-09). Present only while a
    /// failed full session waits for its retry; absent for a record written by an older build.
    #[serde(default)]
    pub retry_at: Option<i64>,
    /// What the recorder last said about its archive recording (its record info, read after a
    /// recording dial or before a download), and when — beside `ble_config` rather than inside
    /// it, because a transport fault that clears the GATT read leaves the archive as it was.
    /// Persisted so the `differs` judgment and the recording target survive a restart
    /// (beacon_history.md §8, amended 2026-10-09). Absent until first read.
    #[serde(default)]
    pub recording: Option<bool>,
    #[serde(default)]
    pub logging_interval_min: Option<u16>,
    #[serde(default)]
    pub recording_ts: Option<i64>,
}

/// How many provisioning sessions the ladder allows in all: the first, then one after each delay.
pub const PROVISION_LADDER_ATTEMPTS: u32 = RETRY_DELAYS_S.len() as u32 + 1;

/// What a failed full provisioning session leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterProvisionFailure {
    /// Retried on the beacon's next advertisement at or after this Unix second.
    RetryAt(i64),
    /// The ladder or the budget is spent: `gave_up_at` is set and the beacon stays `failed` until
    /// an operator acts.
    GaveUp,
    /// Not retried by the Hub — a later session would only repeat the failure (a refused
    /// password) — but the budget is not spent either; it stays `failed`.
    Stays,
}

/// Whether a later provisioning session may succeed where this one failed: the beacon did not
/// answer, the adapter was busy, the link dropped, or the read-back did not match. A refused
/// password, a bad argument or an unreadable ledger is repeated, not fixed, by a retry.
pub fn provision_retryable(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::BeaconUnreachable
            | Kind::Timeout
            | Kind::Transport
            | Kind::AlreadyExists
            | Kind::AdapterDown
            | Kind::ServiceDown
    )
}

/// Is a retry waiting for `retry_at` due at `now`? A wall clock that went back by more than the
/// longest delay (an unsynchronised boot) does not hold a retry for hours: it is due.
pub fn provision_retry_due(retry_at: i64, now: i64) -> bool {
    let longest = RETRY_DELAYS_S.iter().copied().max().unwrap_or(0);
    now >= retry_at || retry_at.saturating_sub(now) > i64::try_from(longest).unwrap_or(i64::MAX)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProvisionLedger {
    pub version: u32,
    /// Keyed by canonical MAC string, so the on-disk form is greppable by a human on the bench.
    #[serde(default)]
    pub beacons: BTreeMap<String, BeaconRecord>,
}

impl Default for ProvisionLedger {
    fn default() -> Self {
        ProvisionLedger {
            version: LEDGER_VERSION,
            beacons: BTreeMap::new(),
        }
    }
}

fn storage_err(message: impl Into<String>) -> Error {
    Error::new(
        Kind::StorageError,
        Role::Central,
        Component::BeaconRegistry,
        message,
    )
}

impl ProvisionLedger {
    /// Load the ledger, or the empty one when the file is absent.
    ///
    /// A *corrupt* ledger is an error rather than a fresh start: silently discarding it would reset
    /// every give-up, which is precisely the unbounded-retry failure this file exists to prevent.
    pub fn load(store: &Store) -> Result<ProvisionLedger> {
        match store.read(Slot::Beacons)? {
            None => Ok(ProvisionLedger::default()),
            Some(raw) if raw.trim().is_empty() => Ok(ProvisionLedger::default()),
            Some(raw) => {
                let l: ProvisionLedger = serde_json::from_str(&raw).map_err(|e| {
                    storage_err(format!(
                        "{} is not a readable provisioning ledger: {e}. Refusing to start from \
                         empty, because that would reset every persisted give-up.",
                        store.path(Slot::Beacons).display()
                    ))
                })?;
                if l.version != LEDGER_VERSION {
                    return Err(storage_err(format!(
                        "provisioning ledger version {} is not {LEDGER_VERSION}: this build cannot \
                         read it",
                        l.version
                    )));
                }
                Ok(l)
            }
        }
    }

    /// Persist atomically at 0600 — the store handles both.
    pub fn save(&self, store: &Store) -> Result<()> {
        let body = serde_json::to_string_pretty(self)
            .map_err(|e| storage_err(format!("cannot serialize the provisioning ledger: {e}")))?;
        store.write(Slot::Beacons, &body)
    }

    fn key(mac: &MacAddr) -> String {
        mac.to_string()
    }

    pub fn get(&self, mac: &MacAddr) -> Option<&BeaconRecord> {
        self.beacons.get(&Self::key(mac))
    }

    fn entry(&mut self, mac: &MacAddr) -> &mut BeaconRecord {
        self.beacons.entry(Self::key(mac)).or_default()
    }

    pub fn record_attempt(&mut self, mac: &MacAddr, now: i64) {
        let r = self.entry(mac);
        r.attempts = r.attempts.saturating_add(1);
        r.last_attempt_ts = Some(now);
    }

    pub fn record_success(&mut self, mac: &MacAddr, now: i64) {
        let r = self.entry(mac);
        r.provisioned = Some(true);
        r.last_error = None;
        r.gave_up_at = None;
        r.retry_at = None;
        r.last_attempt_ts = Some(now);
    }

    pub fn record_give_up(&mut self, mac: &MacAddr, now: i64, error: impl Into<String>) {
        let r = self.entry(mac);
        r.provisioned = Some(false);
        r.last_error = Some(error.into());
        r.gave_up_at = Some(now);
        r.retry_at = None;
        r.last_attempt_ts = Some(now);
    }

    /// Record a failed attempt that has not yet spent the budget.
    pub fn record_failure(&mut self, mac: &MacAddr, now: i64, error: impl Into<String>) {
        let r = self.entry(mac);
        r.last_error = Some(error.into());
        r.last_attempt_ts = Some(now);
    }

    /// Record a failed **full** provisioning session and decide what follows (beacon_history.md §8,
    /// amended 2026-10-09). A retryable failure climbs the history ladder — the next session no
    /// sooner than 5, 15 and 60 minutes after the 1st, 2nd and 3rd failure, measured from
    /// `failed_at` — and the 4th failed session (or the budget, if it is smaller) gives up. Anything
    /// else ends a waiting retry, and gives up only once the budget is spent, as before.
    ///
    /// `now` is when the session started, kept as `last_attempt_ts` like every other record here.
    pub fn record_provision_failure(
        &mut self,
        mac: &MacAddr,
        now: i64,
        failed_at: i64,
        e: &Error,
        per_beacon_attempts: u32,
    ) -> AfterProvisionFailure {
        if provision_retryable(e.kind()) {
            let attempts = self.get(mac).map_or(0, |r| r.attempts);
            let cap = PROVISION_LADDER_ATTEMPTS.min(per_beacon_attempts);
            let delay = usize::try_from(attempts.saturating_sub(1))
                .ok()
                .and_then(|i| RETRY_DELAYS_S.get(i))
                .and_then(|d| i64::try_from(*d).ok());
            return match delay {
                Some(d) if attempts < cap => {
                    let at = failed_at.saturating_add(d);
                    self.record_failure(mac, now, e.message());
                    self.entry(mac).retry_at = Some(at);
                    AfterProvisionFailure::RetryAt(at)
                }
                _ => {
                    self.record_give_up(mac, now, e.message());
                    AfterProvisionFailure::GaveUp
                }
            };
        }
        let spends = matches!(e.kind(), Kind::PairingRejected | Kind::Transport);
        if spends && self.exhausted(mac, per_beacon_attempts) {
            self.record_give_up(mac, now, e.message());
            AfterProvisionFailure::GaveUp
        } else {
            self.record_failure(mac, now, e.message());
            self.entry(mac).retry_at = None;
            AfterProvisionFailure::Stays
        }
    }

    /// Every provisioning retry still waiting, to seed the runtime state at start: a restart does
    /// not drop the ladder. A record that has given up is never retried, whatever it carries.
    pub fn provision_retries(&self) -> BTreeMap<MacAddr, i64> {
        self.beacons
            .iter()
            .filter(|(_, r)| r.gave_up_at.is_none())
            .filter_map(|(key, r)| Some((MacAddr::parse(key).ok()?, r.retry_at?)))
            .collect()
    }

    /// Has this beacon spent its budget? A beacon already provisioned is not "exhausted" — it is
    /// done, and nothing should dial it again.
    pub fn exhausted(&self, mac: &MacAddr, per_beacon_attempts: u32) -> bool {
        match self.get(mac) {
            None => false,
            Some(r) => r.gave_up_at.is_some() || r.attempts >= per_beacon_attempts,
        }
    }

    pub fn is_provisioned(&self, mac: &MacAddr) -> bool {
        self.get(mac).and_then(|r| r.provisioned) == Some(true)
    }

    /// Clear a beacon's record — what `beacon-provision` and a removal do. This is the operator
    /// action central.md §5 means by "marked and left alone **until an operator acts**".
    pub fn reset(&mut self, mac: &MacAddr) -> bool {
        self.beacons.remove(&Self::key(mac)).is_some()
    }

    /// Reopen the attempt budget without forgetting what was read — what an operator's
    /// reconfiguration does (central.md §5, amended 2026-09-23). The last error stays until the
    /// session it asks for ends one way or the other.
    pub fn reopen_budget(&mut self, mac: &MacAddr) {
        if let Some(r) = self.beacons.get_mut(&Self::key(mac)) {
            r.attempts = 0;
            r.gave_up_at = None;
            // The operator's own session replaces a retry the Hub was waiting to make.
            r.retry_at = None;
        }
    }

    /// A read reached the beacon, so its configuration is known again and an earlier failed session
    /// no longer describes it — nor is it retried: a retry waits only behind a standing failure.
    pub fn clear_error(&mut self, mac: &MacAddr) {
        if let Some(r) = self.beacons.get_mut(&Self::key(mac)) {
            r.last_error = None;
            r.retry_at = None;
        }
    }

    /// Every beacon whose last configuration session failed — a success clears `last_error`, so a
    /// recorded error is the last word. Seeds the sticky `failed` state at start.
    pub fn failed_sessions(&self) -> BTreeSet<MacAddr> {
        self.beacons
            .iter()
            .filter(|(_, r)| r.last_error.is_some())
            .filter_map(|(key, _)| MacAddr::parse(key).ok())
            .collect()
    }

    /// Record the configuration a session read back — `verified` when that session had just written
    /// the profile and the read matched it.
    pub fn record_ble_config(&mut self, mac: &MacAddr, reported: ReportedBle) {
        let r = self.entry(mac);
        r.ble_config = Some(reported.config);
        r.ble_config_ts = Some(reported.read_at);
        r.ble_config_verified = Some(reported.verified);
    }

    /// Forget it: a session that failed after the unlock may have left the beacon half-written, so
    /// the last reported configuration no longer describes it.
    pub fn clear_ble_config(&mut self, mac: &MacAddr) {
        if let Some(r) = self.beacons.get_mut(&Self::key(mac)) {
            r.ble_config = None;
            r.ble_config_ts = None;
            r.ble_config_verified = None;
        }
    }

    /// Record what the recorder said about its archive recording.
    pub fn record_recording(&mut self, mac: &MacAddr, report: RecordingReport) {
        let r = self.entry(mac);
        r.recording = Some(report.recording);
        r.logging_interval_min = Some(report.interval_min);
        r.recording_ts = Some(report.read_at);
    }

    /// Every recording report on record, to seed the runtime state at start.
    pub fn recordings(&self) -> BTreeMap<MacAddr, RecordingReport> {
        self.beacons
            .iter()
            .filter_map(|(key, r)| {
                let report = RecordingReport {
                    recording: r.recording?,
                    interval_min: r.logging_interval_min?,
                    read_at: r.recording_ts?,
                };
                Some((MacAddr::parse(key).ok()?, report))
            })
            .collect()
    }

    /// Every beacon a provisioning session of this Hub configured — the ones held to the archive
    /// cadence (beacon_history.md §3).
    pub fn provisioned_set(&self) -> BTreeSet<MacAddr> {
        self.beacons
            .iter()
            .filter(|(_, r)| r.provisioned == Some(true))
            .filter_map(|(key, _)| MacAddr::parse(key).ok())
            .collect()
    }

    /// Every reported configuration on record, to seed the runtime state at start. A key that no
    /// longer parses as a MAC is skipped rather than failing the whole start.
    pub fn reported_configs(&self) -> BTreeMap<MacAddr, ReportedBle> {
        self.beacons
            .iter()
            .filter_map(|(key, r)| {
                let mac = MacAddr::parse(key).ok()?;
                let reported = ReportedBle {
                    config: r.ble_config?,
                    read_at: r.ble_config_ts?,
                    verified: r.ble_config_verified.unwrap_or(false),
                };
                Some((mac, reported))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "AA:BB:CC:DD:EE:FF";

    fn mac(s: &str) -> MacAddr {
        MacAddr::parse(s).expect("mac")
    }

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let base = std::env::temp_dir().join(format!(
                "control-ble-ledger-{tag}-{}-{:?}",
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

    #[test]
    fn an_absent_ledger_loads_empty_and_nothing_is_exhausted() {
        let t = TempDir::new("absent");
        let l = ProvisionLedger::load(&t.store()).expect("loads");
        assert_eq!(l.version, LEDGER_VERSION);
        assert!(l.beacons.is_empty());
        assert!(!l.exhausted(&mac(A), 5));
    }

    #[test]
    fn budget_exhaustion_survives_a_reload() {
        // AC 5: with attempts exhausted against a beacon, the give-up **survives a restart**.
        let t = TempDir::new("survives");
        let store = t.store();
        let mut l = ProvisionLedger::default();
        for _ in 0..5 {
            l.record_attempt(&mac(A), 1_000);
        }
        assert!(l.exhausted(&mac(A), 5));
        l.save(&store).expect("saves");

        let reloaded = ProvisionLedger::load(&store).expect("reloads");
        assert_eq!(reloaded, l, "a byte-for-byte round trip");
        assert!(
            reloaded.exhausted(&mac(A), 5),
            "the budget did not restart on reboot"
        );
    }

    #[test]
    fn a_verified_configuration_survives_a_reload_and_seeds_the_runtime_state() {
        let t = TempDir::new("ble-config");
        let store = t.store();
        let cfg = BleConfig {
            advertising_interval_ms: 10_000,
            tx_power_dbm: 8,
            protocol_type: 0x02,
            active_sensors: Some(0x03),
        };
        let reported = ReportedBle {
            config: cfg,
            read_at: 1_005,
            verified: true,
        };
        let mut l = ProvisionLedger::default();
        l.record_success(&mac(A), 1_000);
        l.record_ble_config(&mac(A), reported);
        l.save(&store).expect("saves");

        let reloaded = ProvisionLedger::load(&store).expect("reloads");
        let seeded = reloaded.reported_configs();
        assert_eq!(seeded.get(&mac(A)), Some(&reported));

        let mut l = reloaded;
        l.clear_ble_config(&mac(A));
        assert!(l.reported_configs().is_empty());
        assert_eq!(
            l.get(&mac(A)).and_then(|r| r.provisioned),
            Some(true),
            "forgetting the configuration is not forgetting the outcome"
        );
    }

    #[test]
    fn a_ledger_written_before_the_read_back_loads_with_nothing_reported() {
        let t = TempDir::new("pre-readback");
        let store = t.store();
        let old = r#"{"version":1,"beacons":{"AA:BB:CC:DD:EE:FF":{"attempts":1,"gave_up_at":null,"last_error":null,"provisioned":true,"last_attempt_ts":1000}}}"#;
        store.ensure_dirs().expect("dirs");
        store.write(Slot::Beacons, old).expect("writes");
        let l = ProvisionLedger::load(&store).expect("an older layout still loads");
        assert_eq!(l.get(&mac(A)).and_then(|r| r.provisioned), Some(true));
        assert!(l.reported_configs().is_empty());
    }

    #[test]
    fn the_last_error_is_the_last_word_until_a_read_or_a_success() {
        let mut l = ProvisionLedger::default();
        l.record_attempt(&mac(A), 1_000);
        l.record_failure(&mac(A), 1_000, "read-back mismatch after the flash commit");
        assert_eq!(l.failed_sessions(), BTreeSet::from([mac(A)]));

        // The operator's reconfiguration reopens the budget and keeps the failure standing.
        l.record_attempt(&mac(A), 1_001);
        l.reopen_budget(&mac(A));
        assert_eq!(l.get(&mac(A)).map(|r| r.attempts), Some(0));
        assert_eq!(l.failed_sessions(), BTreeSet::from([mac(A)]));

        l.clear_error(&mac(A));
        assert!(
            l.failed_sessions().is_empty(),
            "a read that reached it clears it"
        );
        l.record_failure(&mac(A), 1_002, "unreachable");
        l.record_success(&mac(A), 1_003);
        assert!(l.failed_sessions().is_empty(), "and so does a success");
    }

    #[test]
    fn a_give_up_is_exhausted_regardless_of_the_attempt_count() {
        let mut l = ProvisionLedger::default();
        l.record_give_up(&mac(A), 1_000, "the beacon refused the password");
        assert!(
            l.exhausted(&mac(A), 1_000_000),
            "an explicit give-up outranks any budget"
        );
        let r = l.get(&mac(A)).expect("recorded");
        assert_eq!(r.provisioned, Some(false));
        assert_eq!(r.gave_up_at, Some(1_000));
        assert!(r.last_error.is_some());
    }

    #[test]
    fn a_reset_clears_the_budget_so_an_operator_can_retry() {
        let mut l = ProvisionLedger::default();
        l.record_give_up(&mac(A), 1_000, "refused");
        assert!(l.exhausted(&mac(A), 5));
        assert!(l.reset(&mac(A)));
        assert!(
            !l.exhausted(&mac(A), 5),
            "beacon-provision reopens the budget"
        );
        assert!(!l.reset(&mac(A)), "resetting twice is not an error");
    }

    #[test]
    fn a_success_clears_the_error_and_the_give_up() {
        let mut l = ProvisionLedger::default();
        l.record_attempt(&mac(A), 1_000);
        l.record_failure(&mac(A), 1_000, "timed out");
        assert_eq!(
            l.get(&mac(A)).expect("rec").last_error.as_deref(),
            Some("timed out")
        );
        l.record_success(&mac(A), 2_000);
        let r = l.get(&mac(A)).expect("rec");
        assert_eq!(r.provisioned, Some(true));
        assert_eq!(r.last_error, None);
        assert_eq!(r.gave_up_at, None);
        assert_eq!(r.attempts, 1, "the attempt count is history, not a state");
        assert!(l.is_provisioned(&mac(A)));
    }

    #[test]
    fn provisioned_is_tri_state_and_absent_means_never_attempted() {
        let l = ProvisionLedger::default();
        assert!(l.get(&mac(A)).is_none());
        assert!(!l.is_provisioned(&mac(A)), "absent is not `provisioned`");
        // …and it is also not `false`: the distinction is what central.md §5 asks for.
        let mut l = l;
        l.record_attempt(&mac(A), 1);
        assert_eq!(l.get(&mac(A)).expect("rec").provisioned, None);
    }

    #[test]
    fn a_corrupt_ledger_is_refused_rather_than_silently_reset() {
        let t = TempDir::new("corrupt");
        let store = t.store();
        store
            .write(Slot::Beacons, "{ this is not json")
            .expect("writes");
        let err = ProvisionLedger::load(&store).expect_err("must refuse");
        assert_eq!(err.kind(), Kind::StorageError);
        assert!(
            err.message().contains("Refusing to start from empty"),
            "{}",
            err.message()
        );
    }

    #[test]
    fn an_unknown_version_is_refused() {
        let t = TempDir::new("version");
        let store = t.store();
        store
            .write(Slot::Beacons, "{\"version\": 99, \"beacons\": {}}")
            .expect("writes");
        assert_eq!(
            ProvisionLedger::load(&store).expect_err("refuses").kind(),
            Kind::StorageError
        );
    }

    #[test]
    fn the_key_is_the_canonical_mac_so_a_reload_matches_whatever_spelling_arrived() {
        let mut l = ProvisionLedger::default();
        l.record_attempt(&mac("aa:bb:cc:dd:ee:ff"), 1);
        assert!(
            l.beacons.contains_key(A),
            "stored uppercase and colon-separated"
        );
        assert!(l.get(&mac("AABBCCDDEEFF")).is_some());
    }

    fn err(kind: Kind) -> Error {
        Error::new(kind, Role::Central, Component::BeaconGatt, "no answer")
    }

    #[test]
    fn a_retryable_provisioning_failure_climbs_the_history_ladder_then_gives_up() {
        let mut l = ProvisionLedger::default();
        let mut retries = Vec::new();
        for n in 0..4_i64 {
            let now = 1_000 + n * 10_000;
            l.record_attempt(&mac(A), now);
            retries.push(l.record_provision_failure(
                &mac(A),
                now,
                now + 30,
                &err(Kind::BeaconUnreachable),
                5,
            ));
        }
        assert_eq!(
            retries,
            [
                AfterProvisionFailure::RetryAt(1_030 + 300),
                AfterProvisionFailure::RetryAt(11_030 + 900),
                AfterProvisionFailure::RetryAt(21_030 + 3_600),
                AfterProvisionFailure::GaveUp,
            ],
            "5, 15 and 60 minutes after the failure; the 4th session is the last"
        );
        let r = l.get(&mac(A)).expect("rec");
        assert_eq!((r.gave_up_at, r.retry_at), (Some(31_000), None));
        assert_eq!(r.provisioned, Some(false));
        assert!(r.last_error.is_some(), "and it stays `failed`");
        assert!(l.provision_retries().is_empty());
    }

    #[test]
    fn a_smaller_budget_caps_the_ladder() {
        let mut l = ProvisionLedger::default();
        l.record_attempt(&mac(A), 1_000);
        let first = l.record_provision_failure(&mac(A), 1_000, 1_000, &err(Kind::AlreadyExists), 2);
        assert_eq!(first, AfterProvisionFailure::RetryAt(1_300));
        l.record_attempt(&mac(A), 1_300);
        let second = l.record_provision_failure(&mac(A), 1_300, 1_300, &err(Kind::Transport), 2);
        assert_eq!(second, AfterProvisionFailure::GaveUp);
    }

    #[test]
    fn a_refused_password_is_not_retried_and_ends_a_waiting_retry() {
        let mut l = ProvisionLedger::default();
        l.record_attempt(&mac(A), 1_000);
        l.record_provision_failure(&mac(A), 1_000, 1_000, &err(Kind::Timeout), 5);
        assert_eq!(l.provision_retries().get(&mac(A)), Some(&1_300));
        l.record_attempt(&mac(A), 1_300);
        let after =
            l.record_provision_failure(&mac(A), 1_300, 1_300, &err(Kind::PairingRejected), 5);
        assert_eq!(after, AfterProvisionFailure::Stays);
        assert!(l.provision_retries().is_empty());
        assert_eq!(l.get(&mac(A)).and_then(|r| r.gave_up_at), None);
    }

    #[test]
    fn a_waiting_retry_survives_a_reload_and_is_cleared_by_a_success_or_an_operator() {
        let t = TempDir::new("retry");
        let store = t.store();
        let mut l = ProvisionLedger::default();
        l.record_attempt(&mac(A), 1_000);
        l.record_provision_failure(&mac(A), 1_000, 1_030, &err(Kind::BeaconUnreachable), 5);
        l.save(&store).expect("saves");
        let mut l = ProvisionLedger::load(&store).expect("reloads");
        assert_eq!(
            l.provision_retries(),
            BTreeMap::from([(mac(A), 1_330)]),
            "a restart keeps the ladder"
        );

        let mut operator = l.clone();
        operator.reopen_budget(&mac(A));
        assert!(operator.provision_retries().is_empty());

        let mut read = l.clone();
        read.clear_error(&mac(A));
        assert!(read.provision_retries().is_empty());

        l.record_success(&mac(A), 1_400);
        assert!(l.provision_retries().is_empty());
    }

    #[test]
    fn a_retry_is_due_at_its_time_or_when_the_clock_went_back_past_the_ladder() {
        assert!(!provision_retry_due(1_300, 1_299));
        assert!(provision_retry_due(1_300, 1_300));
        assert!(provision_retry_due(1_300, 2_000));
        assert!(
            provision_retry_due(1_000_000, 10),
            "an unsynchronised boot does not hold a retry for days"
        );
        assert!(!provision_retry_due(10 + 3_600, 10));
    }

    #[test]
    fn the_recording_report_survives_a_reload_and_a_cleared_gatt_read() {
        let t = TempDir::new("recording");
        let store = t.store();
        let report = RecordingReport {
            recording: true,
            interval_min: 15,
            read_at: 1_234,
        };
        let mut l = ProvisionLedger::default();
        l.record_success(&mac(A), 1_000);
        l.record_recording(&mac(A), report);
        l.clear_ble_config(&mac(A));
        l.save(&store).expect("saves");

        let raw = std::fs::read_to_string(store.path(Slot::Beacons)).expect("read");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("json");
        let rec = &v["beacons"][A];
        assert_eq!(
            (
                &rec["recording"],
                &rec["logging_interval_min"],
                &rec["recording_ts"]
            ),
            (&true.into(), &15.into(), &1_234.into())
        );

        let l = ProvisionLedger::load(&store).expect("reloads");
        assert_eq!(l.recordings(), BTreeMap::from([(mac(A), report)]));
        assert_eq!(l.provisioned_set(), BTreeSet::from([mac(A)]));
        let mut l = l;
        l.reset(&mac(A));
        assert!(l.recordings().is_empty() && l.provisioned_set().is_empty());
    }

    #[test]
    fn the_ledger_file_is_the_beacons_slot_and_therefore_0600() {
        // Not a new decision — Slot::Beacons is classified secret in the store, and this is the
        // only thing that writes it.
        assert!(Slot::Beacons.is_secret());
        assert_eq!(Slot::Beacons.rel(), "central/beacons.json");
    }
}
