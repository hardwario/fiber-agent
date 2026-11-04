//! Per-device offline detection (plan/portal/devices.backend.md §5, B2).
//!
//! The Hub-scoped [`super::checkers::SystemAlertKind::DeviceOffline`] answers
//! "is the Hub reachable" and is a pure function of a retained MQTT status
//! topic — it needs no clock, and it says nothing about the Nodes and
//! Beacons behind a Hub that is perfectly online. This module answers the
//! other question: **has this one device gone quiet for longer than its own
//! reporting cadence allows**, which is the only thing in the plan that
//! genuinely needs a timer.
//!
//! # Shape
//!
//! [`OfflineDetector::evaluate`] is pure and takes `now` as an argument.
//! That is deliberate: silence detection is all boundary conditions, and a
//! function that reads the clock itself can only be tested by moving the
//! clock. Here the boundaries are ordinary assertions, and the paused-clock
//! test is left to prove the *task* ticks — not the arithmetic.
//!
//! Resolution of each device's inputs — which interval, which multiplier,
//! whose override — is [`node_liveness`]/[`beacon_liveness`], also pure.
//! The task at the bottom is the only part that touches the world.

use std::collections::HashMap;

use serde_json::Value;

use crate::config::{BeaconsConfig, NodesConfig};

/// Fallback report interval for a Node whose config has never been read, in
/// seconds (devices.backend.md §5: "fall back to 15 minutes").
///
/// Deliberately **not** `backfill::DEFAULT_INTERVAL_REPORT_S` (300 s), and
/// the difference is not an oversight. Backfill uses its fallback to decide
/// how wide a gap has to be before it is worth a downlink — guessing short
/// there costs one extra request. Here the fallback decides how long silence
/// must last before an operator is told a device is missing, and guessing
/// short there costs a false alarm on every Node whose config has not landed
/// yet. The two want opposite errors.
pub const UNKNOWN_INTERVAL_FALLBACK_S: i64 = 900;

/// Which family a device belongs to. Decides nothing on its own — the caller
/// has already resolved the interval and the multiplier — but keys the
/// detector's memory so a Node and a Beacon cannot collide on one identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceScope {
    Node,
    Beacon,
}

/// One device's liveness inputs, fully resolved: no config lookup, no
/// override precedence and no clock left to get wrong inside `evaluate`.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceLiveness {
    pub scope: DeviceScope,
    /// Lowercase `node_key` for a Node, uppercase MAC for a Beacon — the
    /// same `entity` convention `field_thresholds` and `device_meta` use.
    pub entity: String,
    /// Unix seconds of the last message. `None` = never heard from, which is
    /// **not** offline — see [`OfflineDetector::evaluate`].
    pub last_seen: Option<i64>,
    /// The cadence this device is expected to keep, in seconds.
    pub interval_s: i64,
    /// Effective silence multiplier. `0` disables detection for this device.
    pub k: u32,
}

impl DeviceLiveness {
    /// Seconds of silence tolerated before this device counts as offline.
    /// `None` when detection is disabled for it.
    fn threshold_s(&self) -> Option<i64> {
        if self.k == 0 {
            return None;
        }
        Some(i64::from(self.k) * self.interval_s.max(1))
    }
}

/// What changed for one device on this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    WentOffline,
    CameBack,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OfflineTransition {
    pub scope: DeviceScope,
    pub entity: String,
    pub transition: Transition,
    /// How long the device had been silent when the transition was noticed.
    /// Carried for the alert message, which is the whole reason an operator
    /// can act on it.
    pub silent_for_s: i64,
}

/// Which devices the last pass found alive, silent, or unmeasurable.
///
/// In-memory and per-process, matching [`super::checkers::SystemAlertState`]:
/// a restart re-arms every device rather than replaying old transitions, and
/// the first pass after a restart re-derives the whole table from `last_seen`
/// anyway, so nothing is actually forgotten but the notification.
#[derive(Debug, Default)]
pub struct OfflineDetector {
    /// The last pass's verdicts, `hostname -> scope -> entity -> is_offline`.
    ///
    /// A row exists only for a device the pass produced a *usable* verdict
    /// for, so the key's **presence** is itself an answer: "this device is
    /// online" and "nobody has measured this device" are different things,
    /// and a bare offline set spells them the same way (see
    /// [`Self::verdict`]). Nested per scope so a lookup can borrow the
    /// entity as `&str` instead of allocating a `(scope, String)` key —
    /// which a bulk render does twice per row (devices.backend.md §19).
    ///
    /// **Keyed by hostname since the pre-merge review.** [`Self::evaluate`]
    /// is called once per Hub — `OfflineService::pass` loops over
    /// `snapshot.devices`, one entry per Hub — and it used to replace this
    /// whole table each time. With two Hubs on the broker, each pass wiped
    /// the other's verdicts, so `was_offline` was always `false` and
    /// `WentOffline` re-fired for every silent device on every sweep: one
    /// email a minute per device, forever, plus a `HubTransition` and a
    /// repaint each time. `node_key` and a MAC are unique only *under* a
    /// Hub (see `offline::HubTransition`), so the hostname is part of the
    /// identity, not decoration — a BLE tag in range of two Hubs is routine.
    verdicts: HashMap<String, HashMap<DeviceScope, HashMap<String, bool>>>,
}

/// The detector's per-device answer to "is this thing alive", as of the last
/// pass, in the one shape a bulk render needs (devices.backend.md §19).
///
/// Cloned out of the service under its lock and then read many times, so it
/// owns its table rather than borrowing it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LivenessVerdicts {
    by_scope: HashMap<DeviceScope, HashMap<String, bool>>,
}

impl LivenessVerdicts {
    /// `Some(true)` offline, `Some(false)` online, `None` no verdict — see
    /// [`OfflineDetector::verdict`].
    ///
    /// Borrows `entity`: this is the hottest read in a monitoring render
    /// (once per row, and the fault axis used to ask a second time), and the
    /// nested map means neither the scope nor the identity has to be
    /// allocated to look one up.
    pub fn verdict(&self, scope: DeviceScope, entity: &str) -> Option<bool> {
        self.by_scope.get(&scope)?.get(entity).copied()
    }

    /// A verdict table built by hand.
    ///
    /// Test seam: the render tests need every one of the three answers
    /// (`Some(true)`, `Some(false)`, `None`) and a real detector pass can
    /// only produce them through a config, a clock and a database. A device
    /// left out of the list keeps the `None` verdict, which is the case the
    /// Hub's echo still answers.
    #[cfg(test)]
    pub fn from_verdicts<'a>(
        verdicts: impl IntoIterator<Item = (DeviceScope, &'a str, bool)>,
    ) -> Self {
        let mut out = Self::default();
        for (scope, entity, offline) in verdicts {
            out.by_scope
                .entry(scope)
                .or_default()
                .insert(entity.to_string(), offline);
        }
        out
    }
}

impl OfflineDetector {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` if this device is currently marked offline. The read the fault
    /// indicator (§4) renders from.
    pub fn is_offline(&self, scope: DeviceScope, entity: &str) -> bool {
        self.verdict(scope, entity) == Some(true)
    }

    /// This detector's answer for one device, with "I have no answer" spelled
    /// apart from "it is online".
    ///
    /// `None` means the caller must fall back to whatever else it knows (for
    /// the render layer: the Hub's echoed `alarm_state`), and it covers two
    /// cases: no pass has run yet, or the device has never been heard from —
    /// a registered Node waiting to join is not something this detector is
    /// entitled to call alive.
    ///
    /// **Detection disabled (`k == 0`) is not one of them**, since
    /// 2026-09-08: a device with detection switched off but a `last_seen` on
    /// record answers `Some(false)`. It is not a measurement — nothing will
    /// ever move it to `Some(true)` — but "we have heard from this device"
    /// is a fact the portal holds, and it beats the alternative: the echo,
    /// which for a Node fed only by the v2 per-Node envelope is
    /// `NeverConnected` for the device's whole life (§19.1, F3).
    pub fn verdict(&self, scope: DeviceScope, entity: &str) -> Option<bool> {
        self.verdicts
            .values()
            .find_map(|by_scope| by_scope.get(&scope)?.get(entity).copied())
    }

    /// The same answer, for one named Hub. This is the exact lookup —
    /// [`Self::verdict`] is the Phase-1 convenience that searches every Hub
    /// and therefore cannot tell two Hubs' identically-keyed devices apart.
    pub fn verdict_on(&self, hostname: &str, scope: DeviceScope, entity: &str) -> Option<bool> {
        self.verdicts
            .get(hostname)?
            .get(&scope)?
            .get(entity)
            .copied()
    }

    /// Whether anything of this Hub is judged offline right now (O-4: only such a Hub's
    /// readings trigger an early pass). Case-insensitive, as Hub spellings differ by path.
    pub fn any_offline(&self, hostname: &str) -> bool {
        self.verdicts
            .iter()
            .filter(|(h, _)| h.eq_ignore_ascii_case(hostname))
            .any(|(_, by_scope)| by_scope.values().any(|m| m.values().any(|v| *v)))
    }

    /// The whole verdict table, for one bulk render pass.
    ///
    /// Flattened across Hubs, because the render layer looks devices up by
    /// `node_key`/MAC alone (`monitoring::probe_row` and friends) and
    /// Phase 1 is single-Hub — the same assumption `devices.backend.md`
    /// §2180 and §2511 record for the pending-config SSE event and the
    /// `config/state` reconcile. Unlike the old shape, a second Hub now
    /// *merges* into this snapshot instead of deleting the first one's
    /// rows; when Phase 2 arrives the render callers need a hostname and
    /// [`Self::verdict_on`] is what they should reach for.
    pub fn verdicts(&self) -> LivenessVerdicts {
        let mut by_scope: HashMap<DeviceScope, HashMap<String, bool>> = HashMap::new();
        for hub in self.verdicts.values() {
            for (scope, entities) in hub {
                by_scope.entry(*scope).or_default().extend(
                    entities
                        .iter()
                        .map(|(entity, offline)| (entity.clone(), *offline)),
                );
            }
        }
        LivenessVerdicts { by_scope }
    }

    /// One detection pass over the full device list.
    ///
    /// `devices` must be the complete current set for the scopes it covers:
    /// a device that has disappeared from it (removed from the fleet) is
    /// dropped from the detector's memory **silently**, because "no longer
    /// present" is not "came back".
    ///
    /// Three cases produce no transition and are each deliberate:
    ///
    /// - **`k == 0`** — detection disabled, fleet-wide or for this one
    ///   device. An already-raised device is cleared, but silently: an
    ///   operator switching detection off is not a device coming back, and
    ///   emailing them as though it were would train them to ignore the
    ///   alert. Since 2026-09-08 such a device still gets a verdict —
    ///   `Some(false)` if it has ever been heard from — because the
    ///   alternative fallback is the Hub's echo, which is `NeverConnected`
    ///   for the whole life of a Node fed only by the v2 envelope (§19.1,
    ///   F3). Nothing here can ever move it to `Some(true)`: detection off
    ///   means no measurement, only the record of a message having arrived.
    /// - **`last_seen == None`** — never heard from. A Node registered and
    ///   waiting to join has been silent since forever, which would trip
    ///   every threshold instantly; "waiting for join" is a lifecycle state
    ///   (§7.4), not a fault, and it is not this detector's to raise. This
    ///   is the one case that keeps no verdict at all, and the one where the
    ///   echo's `NeverConnected` is exactly right.
    /// - **a future `last_seen`** — clock skew between Hub and portal. Read
    ///   as zero silence rather than as a negative, so a Hub whose clock ran
    ///   ahead does not make every device look permanently fresh *or*
    ///   permanently late depending on sign.
    ///
    /// The comparison is strictly greater: at exactly `K × interval` the
    /// device is still within budget, matching §5's "silence > K ×
    /// `interval_report`".
    pub fn evaluate(
        &mut self,
        hostname: &str,
        now: i64,
        devices: &[DeviceLiveness],
    ) -> Vec<OfflineTransition> {
        // Rebuilt rather than mutated in place: a device that vanished from
        // the fleet is simply absent from the new table, which is exactly
        // "leaves no trace and no event". Rebuilt for **this Hub only** —
        // `devices` is one Hub's complete set, never the broker's, so
        // replacing the whole table here is what re-fired every silent
        // device's alert once a sweep.
        let mut next: HashMap<DeviceScope, HashMap<String, bool>> = HashMap::new();
        let mut out = Vec::new();

        for device in devices {
            let was_offline = self.verdict_on(hostname, device.scope, &device.entity) == Some(true);

            let Some(threshold) = device.threshold_s() else {
                // Disabled: cleared without announcing, and no measurement —
                // only the fact that something has arrived from it, if it
                // has (see this function's doc).
                if device.last_seen.is_some() {
                    next.entry(device.scope)
                        .or_default()
                        .insert(device.entity.clone(), false);
                }
                continue;
            };
            let Some(last_seen) = device.last_seen else {
                continue;
            };

            let silent_for = (now - last_seen).max(0);
            let is_offline = silent_for > threshold;
            next.entry(device.scope)
                .or_default()
                .insert(device.entity.clone(), is_offline);

            match (was_offline, is_offline) {
                (false, true) => out.push(OfflineTransition {
                    scope: device.scope,
                    entity: device.entity.clone(),
                    transition: Transition::WentOffline,
                    silent_for_s: silent_for,
                }),
                (true, false) => out.push(OfflineTransition {
                    scope: device.scope,
                    entity: device.entity.clone(),
                    transition: Transition::CameBack,
                    silent_for_s: silent_for,
                }),
                _ => {}
            }
        }

        // Only this Hub's sub-table is replaced; every other Hub's verdicts
        // are left exactly as they were. An empty pass still writes an empty
        // map, so a Hub that lost its whole fleet forgets it silently — the
        // "no longer present is not came back" rule, kept per Hub.
        self.verdicts.insert(hostname.to_string(), next);
        out
    }
}

/// Resolves a Node's liveness inputs.
///
/// `interval_report` is not on `NodeState` — it lives in the Hub's cached
/// config map, read here exactly the way `backfill::auto_backfill_on_reconnect`
/// reads it, so the two cannot drift on the key name.
///
/// `last_seen` on a Node is an **RFC3339 string taken verbatim off the wire**,
/// not a store-clock integer; it doubles as the persistence dedup key, so it
/// is parsed here and never rewritten. An unparseable value is treated as
/// "never heard from" rather than as zero, which would read as 56 years of
/// silence.
pub fn node_liveness(
    node_key: &str,
    last_seen_rfc3339: Option<&str>,
    node_config: Option<&serde_json::Map<String, Value>>,
    offline_k_override: Option<i64>,
    cfg: &NodesConfig,
) -> DeviceLiveness {
    let interval_s = node_config
        .and_then(|c| c.get("application.interval_report"))
        .and_then(Value::as_i64)
        .filter(|s| *s > 0)
        .unwrap_or(UNKNOWN_INTERVAL_FALLBACK_S);

    DeviceLiveness {
        scope: DeviceScope::Node,
        entity: node_key.to_string(),
        last_seen: last_seen_rfc3339.and_then(parse_rfc3339),
        interval_s,
        k: effective_k(offline_k_override, cfg.offline_detector),
    }
}

/// Resolves a Beacon's liveness inputs. Its cadence is `beacons.store_interval`
/// (minutes), not a device-reported one: a Beacon advertises far faster than
/// it is stored, and the history the portal expects to see is the Hub's
/// storage window (`eye.aggregation_interval_s`, storage_intervals.md §6),
/// which that key should match. There is no per-Beacon override any more
/// (S4).
pub fn beacon_liveness(
    mac: &str,
    last_seen: Option<i64>,
    offline_k_override: Option<i64>,
    cfg: &BeaconsConfig,
) -> DeviceLiveness {
    DeviceLiveness {
        scope: DeviceScope::Beacon,
        entity: mac.to_string(),
        last_seen,
        interval_s: beacon_interval_s(cfg),
        k: effective_k(offline_k_override, cfg.offline_detector),
    }
}

/// The offline detector's expected Beacon interval, in seconds:
/// `beacons.store_interval` from the YAML. `config::validate` already
/// rejects `store_interval: 0` in the file (§19.7); the floor is a guard.
pub fn beacon_interval_s(cfg: &BeaconsConfig) -> i64 {
    i64::from(cfg.store_interval).max(1) * 60
}

/// `device_meta.offline_k_override` beats the YAML default, and `0` is a
/// real override meaning "off for this device" — which is why the column is
/// nullable rather than defaulted, and why this is not `unwrap_or_default`.
/// A negative value in the column is nonsense the schema does not forbid;
/// it is read as the inherited default rather than as disabled, so a typo
/// fails loud (still alarming) rather than silent (never alarming).
fn effective_k(override_k: Option<i64>, default_k: u32) -> u32 {
    match override_k {
        Some(k) if k >= 0 => u32::try_from(k).unwrap_or(default_k),
        _ => default_k,
    }
}

fn parse_rfc3339(s: &str) -> Option<i64> {
    use time::format_description::well_known::Rfc3339;
    use time::OffsetDateTime;
    OffsetDateTime::parse(s, &Rfc3339)
        .ok()
        .map(|dt| dt.unix_timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "h1";
    const NOW: i64 = 1_800_000_000;

    fn node(entity: &str, last_seen: Option<i64>, interval_s: i64, k: u32) -> DeviceLiveness {
        DeviceLiveness {
            scope: DeviceScope::Node,
            entity: entity.to_string(),
            last_seen,
            interval_s,
            k,
        }
    }

    /// `OfflineService::pass` calls `evaluate` once per Hub. When the table
    /// was replaced wholesale rather than per Hub, the second Hub's pass
    /// deleted the first's verdicts, `was_offline` came back `false`, and a
    /// device that had been silent for a day re-announced `WentOffline` on
    /// every single sweep — one email a minute, forever.
    #[test]
    fn a_second_hubs_pass_does_not_wipe_the_first_hubs_verdicts() {
        const H1: &str = "h1";
        const H2: &str = "h2";
        let silent = node("aa11", Some(NOW - 86_400), 300, 4);
        let fresh = node("bb22", Some(NOW - 10), 300, 4);

        let mut d = OfflineDetector::new();

        // Sweep 1: h1 raises its silent Node, then h2 runs.
        let raised = d.evaluate(H1, NOW, std::slice::from_ref(&silent));
        assert_eq!(raised.len(), 1, "h1's silent Node is announced once");
        assert_eq!(raised[0].transition, Transition::WentOffline);
        d.evaluate(H2, NOW, std::slice::from_ref(&fresh));

        assert_eq!(
            d.verdict_on(H1, DeviceScope::Node, "aa11"),
            Some(true),
            "h2's pass must not forget h1's verdict"
        );
        assert_eq!(d.verdict_on(H2, DeviceScope::Node, "bb22"), Some(false));

        // Sweep 2: nothing changed, so nothing is announced.
        assert!(
            d.evaluate(H1, NOW + 60, std::slice::from_ref(&silent))
                .is_empty(),
            "a still-silent Node re-announces nothing"
        );
        assert!(d
            .evaluate(H2, NOW + 60, std::slice::from_ref(&fresh))
            .is_empty());

        // And the render snapshot carries both Hubs, rather than whichever
        // one happened to be iterated last.
        let snapshot = d.verdicts();
        assert_eq!(snapshot.verdict(DeviceScope::Node, "aa11"), Some(true));
        assert_eq!(snapshot.verdict(DeviceScope::Node, "bb22"), Some(false));
    }

    /// §5's boundary is "silence **>** K × interval": at exactly the budget
    /// the device is still within it. Asserted on both sides of the edge and
    /// on the edge itself, because an off-by-one here is a whole extra
    /// reporting interval of either false alarms or blindness.
    #[test]
    fn the_boundary_is_strictly_greater_than_k_times_interval() {
        let interval = 300;
        let k = 4;
        let budget = interval * i64::from(k); // 1200 s

        for (silence, expect_offline) in [(budget - 1, false), (budget, false), (budget + 1, true)]
        {
            let mut d = OfflineDetector::new();
            let events = d.evaluate(HOST, NOW, &[node("n1", Some(NOW - silence), interval, k)]);
            assert_eq!(
                d.is_offline(DeviceScope::Node, "n1"),
                expect_offline,
                "silence {silence}s against a {budget}s budget"
            );
            assert_eq!(events.len(), usize::from(expect_offline));
        }
    }

    /// Raise once, stay raised, then clear exactly once when the device
    /// speaks again — the onset-debounce contract `SystemAlertState` already
    /// has for Hub-scoped alerts.
    #[test]
    fn raises_once_and_clears_once() {
        let mut d = OfflineDetector::new();

        let quiet = [node("n1", Some(NOW - 1_201), 300, 4)];
        let raised = d.evaluate(HOST, NOW, &quiet);
        assert_eq!(raised.len(), 1);
        assert_eq!(raised[0].transition, Transition::WentOffline);
        assert_eq!(raised[0].silent_for_s, 1_201);

        // Still quiet: no repeat event, still offline.
        assert!(d.evaluate(HOST, NOW + 60, &quiet).is_empty());
        assert!(d.is_offline(DeviceScope::Node, "n1"));

        let back = d.evaluate(HOST, NOW, &[node("n1", Some(NOW - 10), 300, 4)]);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].transition, Transition::CameBack);
        assert!(!d.is_offline(DeviceScope::Node, "n1"));

        // And clearing does not repeat either.
        assert!(d
            .evaluate(HOST, NOW, &[node("n1", Some(NOW - 10), 300, 4)])
            .is_empty());
    }

    /// `k == 0` is a real setting — detection off — not a missing one. It
    /// must also *clear* a device already raised, and do it silently: an
    /// operator turning the alert off is not a device coming back.
    #[test]
    fn zero_k_disables_and_clears_without_announcing() {
        let mut d = OfflineDetector::new();
        assert_eq!(
            d.evaluate(HOST, NOW, &[node("n1", Some(NOW - 99_999), 300, 4)])
                .len(),
            1
        );
        assert!(d.is_offline(DeviceScope::Node, "n1"));

        let events = d.evaluate(HOST, NOW, &[node("n1", Some(NOW - 99_999), 300, 0)]);
        assert!(
            events.is_empty(),
            "disabling must not emit a came-back event"
        );
        assert!(!d.is_offline(DeviceScope::Node, "n1"));
    }

    /// A Node registered and waiting to join has been silent since forever.
    /// That is a lifecycle state, not a fault, and tripping the detector on
    /// it would alarm on every device the moment it is added.
    #[test]
    fn a_device_never_heard_from_is_not_offline() {
        let mut d = OfflineDetector::new();
        assert!(d
            .evaluate(HOST, NOW, &[node("n1", None, 300, 4)])
            .is_empty());
        assert!(!d.is_offline(DeviceScope::Node, "n1"));
    }

    /// Hub/portal clock skew must not flip the verdict on the sign of the
    /// difference.
    #[test]
    fn a_last_seen_in_the_future_reads_as_zero_silence() {
        let mut d = OfflineDetector::new();
        assert!(d
            .evaluate(HOST, NOW, &[node("n1", Some(NOW + 5_000), 300, 4)])
            .is_empty());
        assert!(!d.is_offline(DeviceScope::Node, "n1"));
    }

    /// A device removed from the fleet is forgotten without an event — "no
    /// longer present" is not "came back".
    #[test]
    fn a_vanished_device_is_dropped_silently() {
        let mut d = OfflineDetector::new();
        d.evaluate(HOST, NOW, &[node("n1", Some(NOW - 99_999), 300, 4)]);
        assert!(d.is_offline(DeviceScope::Node, "n1"));

        assert!(d.evaluate(HOST, NOW, &[]).is_empty());
        assert!(!d.is_offline(DeviceScope::Node, "n1"));
        assert_eq!(
            d.verdicts(),
            LivenessVerdicts::default(),
            "not merely 'not offline' — no row survives at all"
        );
    }

    /// A Node and a Beacon sharing an identity string are two devices.
    #[test]
    fn scope_is_part_of_the_identity() {
        let mut d = OfflineDetector::new();
        let both = [
            node("AA", Some(NOW - 99_999), 300, 4),
            DeviceLiveness {
                scope: DeviceScope::Beacon,
                entity: "AA".to_string(),
                last_seen: Some(NOW - 10),
                interval_s: 900,
                k: 1,
            },
        ];
        let events = d.evaluate(HOST, NOW, &both);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].scope, DeviceScope::Node);
        assert!(d.is_offline(DeviceScope::Node, "AA"));
        assert!(!d.is_offline(DeviceScope::Beacon, "AA"));
    }

    // ---- input resolution -------------------------------------------------

    fn node_cfg(k: u32) -> NodesConfig {
        NodesConfig {
            offline_detector: k,
            ..NodesConfig::default()
        }
    }

    fn beacon_cfg(store_interval: u32, k: u32) -> BeaconsConfig {
        BeaconsConfig {
            store_interval,
            offline_detector: k,
        }
    }

    #[test]
    fn a_node_reads_its_interval_from_the_cached_config() {
        let mut config = serde_json::Map::new();
        config.insert("application.interval_report".into(), Value::from(60));

        let l = node_liveness(
            "n1",
            Some("2026-01-13T14:30:00Z"),
            Some(&config),
            None,
            &node_cfg(4),
        );
        assert_eq!(l.interval_s, 60);
        assert_eq!(l.k, 4);
        assert_eq!(l.last_seen, Some(1_768_314_600));
    }

    /// The 15-minute fallback (§5), which is deliberately not backfill's
    /// 300 s — see [`UNKNOWN_INTERVAL_FALLBACK_S`].
    #[test]
    fn a_node_with_no_config_falls_back_to_fifteen_minutes() {
        for config in [None, Some(&serde_json::Map::new())] {
            let l = node_liveness(
                "n1",
                Some("2026-01-13T14:30:00Z"),
                config,
                None,
                &node_cfg(4),
            );
            assert_eq!(l.interval_s, UNKNOWN_INTERVAL_FALLBACK_S);
            assert_eq!(l.interval_s, 900);
        }
    }

    /// An unparseable `last_seen` is "never heard from", not epoch zero —
    /// which would read as decades of silence and alarm instantly.
    #[test]
    fn an_unparseable_last_seen_is_not_epoch_zero() {
        let l = node_liveness("n1", Some("whenever"), None, None, &node_cfg(4));
        assert_eq!(l.last_seen, None);
        let l = node_liveness("n1", None, None, None, &node_cfg(4));
        assert_eq!(l.last_seen, None);
    }

    #[test]
    fn a_beacon_measures_against_its_effective_store_interval() {
        // Inherited: 15 min from config.
        let l = beacon_liveness("AA:BB", Some(NOW), None, &beacon_cfg(15, 1));
        assert_eq!(l.interval_s, 900);
        assert_eq!(l.k, 1);
    }

    /// `device_meta.offline_k_override` beats the YAML default in both
    /// directions, `Some(0)` included — the case that must not collapse into
    /// "unset".
    #[test]
    fn the_per_device_override_beats_the_configured_default() {
        assert_eq!(effective_k(None, 4), 4, "NULL inherits");
        assert_eq!(effective_k(Some(6), 4), 6, "an override wins");
        assert_eq!(
            effective_k(Some(0), 4),
            0,
            "0 disables, it does not inherit"
        );
        assert_eq!(
            effective_k(Some(-1), 4),
            4,
            "nonsense inherits rather than disabling"
        );

        let l = node_liveness("n1", None, None, Some(0), &node_cfg(4));
        assert_eq!(l.k, 0);
        assert!(l.threshold_s().is_none());
    }

    /// S4 (storage_intervals.md §6): the per-Beacon override is gone, so the
    /// expected interval is the YAML's alone.
    #[test]
    fn the_beacon_interval_is_the_configured_store_interval() {
        assert_eq!(beacon_interval_s(&beacon_cfg(15, 1)), 900);
        assert_eq!(beacon_interval_s(&beacon_cfg(10, 1)), 600);
        assert_eq!(
            beacon_interval_s(&beacon_cfg(0, 1)),
            60,
            "a guard, not a mode"
        );
    }

    // ---- §19 (F2/F3): "no verdict" is its own answer ---------------------

    /// The three-valued read the render layer takes. `Some(false)` is a
    /// measured "this device is alive"; `None` is "I have not measured it",
    /// and the two must not be spelled the same way — the whole point of
    /// `verdict` over `is_offline` is that only the first one may override
    /// the Hub's echoed `alarm_state`.
    #[test]
    fn a_measured_device_has_a_verdict_and_an_unmeasured_one_does_not() {
        let mut d = OfflineDetector::new();
        assert_eq!(
            d.verdict(DeviceScope::Node, "n1"),
            None,
            "before any pass nothing is known"
        );

        d.evaluate(HOST, NOW, &[node("n1", Some(NOW - 10), 300, 4)]);
        assert_eq!(d.verdict(DeviceScope::Node, "n1"), Some(false));

        d.evaluate(HOST, NOW, &[node("n1", Some(NOW - 5_000), 300, 4)]);
        assert_eq!(d.verdict(DeviceScope::Node, "n1"), Some(true));
    }

    /// Never heard from is "no verdict": a Node registered and waiting to
    /// join must not come out of this detector looking alive, and
    /// `NeverConnected` is what the echo is right about.
    ///
    /// **Detection switched off is not the same case** (2026-09-08, review
    /// round 2): with a `last_seen` on record the device answers
    /// `Some(false)`, because the only other fallback is the Hub's echoed
    /// `alarm_state`, and for a Node fed solely by the v2 per-Node envelope
    /// that echo is `NeverConnected` for the device's whole life — F3
    /// returning through the back door (§19.1). Detection off with nothing
    /// ever received stays "no verdict".
    #[test]
    fn detection_switched_off_falls_back_to_last_seen_not_to_the_hubs_echo() {
        let mut d = OfflineDetector::new();
        d.evaluate(
            HOST,
            NOW,
            &[
                node("never", None, 300, 4),
                node("disabled", Some(NOW - 999_999), 300, 0),
                node("disabled-and-never-seen", None, 300, 0),
            ],
        );
        assert_eq!(d.verdict(DeviceScope::Node, "never"), None);
        assert_eq!(
            d.verdict(DeviceScope::Node, "disabled"),
            Some(false),
            "heard from at least once, however long ago — detection off is not a measurement"
        );
        assert!(
            !d.is_offline(DeviceScope::Node, "disabled"),
            "and never a fault: nothing measured it silent"
        );
        assert!(
            crate::render::node_connected(
                "NeverConnected",
                d.verdict(DeviceScope::Node, "disabled")
            ),
            "the two halves composed: the echo is not consulted, so F3 stays closed"
        );
        assert_eq!(
            d.verdict(DeviceScope::Node, "disabled-and-never-seen"),
            None,
            "nothing has ever arrived, so the echo answers"
        );
    }

    /// A device that had a verdict and then leaves the fleet takes the
    /// verdict with it — the same silent forgetting `offline` already does.
    #[test]
    fn a_device_that_leaves_the_fleet_takes_its_verdict_with_it() {
        let mut d = OfflineDetector::new();
        d.evaluate(HOST, NOW, &[node("n1", Some(NOW - 5_000), 300, 4)]);
        assert_eq!(d.verdict(DeviceScope::Node, "n1"), Some(true));

        d.evaluate(HOST, NOW, &[]);
        assert_eq!(d.verdict(DeviceScope::Node, "n1"), None);
    }

    /// The bulk snapshot answers exactly as the detector does, including for
    /// the scope split — a Node and a Beacon sharing an entity string are
    /// two devices.
    #[test]
    fn the_verdict_snapshot_agrees_with_the_detector_scope_by_scope() {
        let mut d = OfflineDetector::new();
        d.evaluate(
            HOST,
            NOW,
            &[
                node("AA", Some(NOW - 5_000), 300, 4),
                DeviceLiveness {
                    scope: DeviceScope::Beacon,
                    entity: "AA".to_string(),
                    last_seen: Some(NOW - 10),
                    interval_s: 300,
                    k: 4,
                },
            ],
        );

        let snap = d.verdicts();
        assert_eq!(snap.verdict(DeviceScope::Node, "AA"), Some(true));
        assert_eq!(snap.verdict(DeviceScope::Beacon, "AA"), Some(false));
        assert_eq!(
            snap.verdict(DeviceScope::Node, "unknown"),
            None,
            "no verdict, which is neither a fault nor a claim of life"
        );
    }
}
