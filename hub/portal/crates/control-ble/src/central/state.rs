//! Central state — the Beacon entity, the three axes, and the bounded snapshot.
//! `plan/control/ble/central.md` §4, `plan/control/ble/plan.md` §2.
//!
//! Three properties here are contract, and each closes a named v1 defect:
//!
//! - **An absent field publishes as absent, never as zero** (§4 rule 1). A missing humidity reading
//!   and 0 % humidity are different facts, and a freezer alarm can turn on the difference.
//! - **The discovered set is bounded, and the bound is reported** (§4). In v1 a single oversized
//!   publish exceeded the MQTT client's packet cap and tore down *the entire client* — not the
//!   Beacon topic — taking every other subsystem's publishing with it.
//! - **`discovered: null` means *registered*** on the wire. That is a v1 contract fact, not a
//!   convention this component may change, and it is exactly the thing a reimplementation gets
//!   backwards.
//!
//! Membership is the removal signal: every publish is the complete tracked set, published even when
//! empty, and a discovered-only beacon that expired or was cap-evicted is simply **absent** from the
//! next snapshot. The consumer prunes by absence, with `tags: []` as the empty case.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::CentralConfig;
use crate::decode::{registry, ParseError, Reading};
use crate::host::{BeaconPayload, BleConfigPayload, SensorsSnapshot};
use crate::mac::MacAddr;

use super::provision::{holds_profile, BleConfig};

// ---------------------------------------------------------------------------------------------
// Axes
// ---------------------------------------------------------------------------------------------

/// The `provisioning` axis — what the Hub knows about the beacon's flash (central.md §5, amended
/// 2026-09-23). A removal resets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provisioning {
    /// Registered, configuration not read yet (or the read did not reach the beacon).
    Pending,
    /// A configuration session is queued or in flight.
    InProgress,
    /// The last read — plain, or after writing — holds the fleet profile.
    Provisioned,
    /// The last plain read does not hold the fleet profile. Derived, never stored.
    Differs,
    /// The last configuration session failed: refused, unreachable, or its read-back did not match.
    Failed,
    /// Deliberately not provisioned — a borrowed beacon this Hub does not own.
    ReadOnly,
}

impl Provisioning {
    pub const fn as_str(self) -> &'static str {
        match self {
            Provisioning::Pending => "pending",
            // The wire spelling is "provisioning", not "in_progress" — plan.md §5's shape.
            Provisioning::InProgress => "provisioning",
            Provisioning::Provisioned => "provisioned",
            Provisioning::Differs => "differs",
            Provisioning::Failed => "failed",
            Provisioning::ReadOnly => "read_only",
        }
    }
}

/// Alarm levels on the Beacon wire scale. Ordered, so an aggregate is a `max`.
///
/// There is deliberately **no `ALARM` rank on this surface**: the strings below are the ones the
/// portal's `alarm_state` already carries for beacons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AlarmLevel {
    Normal,
    Warning,
    Critical,
    /// Not a threshold outcome — a beacon we have stopped hearing.
    Disconnected,
}

impl AlarmLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            AlarmLevel::Normal => "NORMAL",
            AlarmLevel::Warning => "WARNING",
            AlarmLevel::Critical => "CRITICAL",
            AlarmLevel::Disconnected => "DISCONNECTED",
        }
    }
}

/// One `eye.tags[].field_thresholds[]` entry. Every bound is optional and an absent bound is simply
/// not evaluated — a half-configured threshold must not read as "always critical".
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FieldThreshold {
    pub field: String,
    #[serde(default)]
    pub critical_low: Option<f64>,
    #[serde(default)]
    pub warning_low: Option<f64>,
    #[serde(default)]
    pub warning_high: Option<f64>,
    #[serde(default)]
    pub critical_high: Option<f64>,
    /// The portal's hysteresis for the field (decision #41); absent = the field's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hysteresis: Option<f64>,
}

/// Default hysteresis of a field: 0.5 °C for a temperature, 3 % RH for a humidity, else 0.
/// The same numbers as `fiber_protocol::grading` (asserted by control-core's parity test).
pub fn default_hysteresis(field: &str) -> f64 {
    match field {
        "humidity" => 3.0,
        "temperature" => 0.5,
        _ => 0.0,
    }
}

impl AlarmLevel {
    /// The wire string back to a level; anything else is no level.
    pub fn parse(s: &str) -> Option<AlarmLevel> {
        match s {
            "NORMAL" => Some(AlarmLevel::Normal),
            "WARNING" => Some(AlarmLevel::Warning),
            "CRITICAL" => Some(AlarmLevel::Critical),
            "DISCONNECTED" => Some(AlarmLevel::Disconnected),
            _ => None,
        }
    }
}

impl FieldThreshold {
    /// Decode the thresholds of one tag, skipping entries that are not threshold objects.
    pub fn from_values(values: &[serde_yaml::Value]) -> Vec<FieldThreshold> {
        values
            .iter()
            .filter_map(|v| serde_yaml::from_value::<FieldThreshold>(v.clone()).ok())
            .collect()
    }

    /// The range rule: critical bounds first, then warning, else normal.
    pub fn evaluate(&self, v: f64) -> AlarmLevel {
        self.evaluate_inset(v, 0.0)
    }

    /// [`Self::evaluate`] with every bound moved `inset` towards the normal zone.
    fn evaluate_inset(&self, v: f64, inset: f64) -> AlarmLevel {
        let below = |bound: Option<f64>| bound.is_some_and(|b| v < b + inset);
        let above = |bound: Option<f64>| bound.is_some_and(|b| v > b - inset);
        if below(self.critical_low) || above(self.critical_high) {
            return AlarmLevel::Critical;
        }
        if below(self.warning_low) || above(self.warning_high) {
            return AlarmLevel::Warning;
        }
        AlarmLevel::Normal
    }

    /// The hysteresis this field is graded with: its own, else [`default_hysteresis`].
    pub fn effective_hysteresis(&self) -> f64 {
        self.hysteresis
            .filter(|h| h.is_finite() && *h >= 0.0)
            .unwrap_or_else(|| default_hysteresis(&self.field))
    }

    /// The grade given the one held so far (plan/control/alarm_hysteresis.md §3): a step up is
    /// at the bound, a step down only once the value is the hysteresis back inside it; no
    /// previous grade is graded strictly. A copy of `fiber_protocol::grading::Bounds::grade`,
    /// because this crate may depend on nothing in the workspace (decision #12); control-core's
    /// parity test keeps the two equal.
    pub fn evaluate_held(&self, v: f64, previous: Option<AlarmLevel>) -> AlarmLevel {
        let strict = self.evaluate(v);
        let h = self.effective_hysteresis();
        match previous {
            Some(prev) if prev != AlarmLevel::Disconnected && prev > strict && h > 0.0 => {
                strict.max(prev.min(self.evaluate_inset(v, h)))
            }
            _ => strict,
        }
    }
}

/// The only two threshold-driven fields on this surface.
pub const THRESHOLD_FIELDS: [&str; 2] = ["temperature", "humidity"];

// ---------------------------------------------------------------------------------------------
// One beacon
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct BeaconState {
    pub mac: MacAddr,
    pub name: Option<String>,
    pub reading: Option<Reading>,
    pub rssi: Option<i16>,
    pub last_seen_ts: Option<i64>,
    pub provisioning: Provisioning,
    pub provision_attempts: u32,
    pub gave_up_at: Option<i64>,
    pub last_error: Option<String>,
    /// **Tri-state.** `None` until probed — and it stays `None` while the EN12830 capability flag is
    /// off, because the probe uses the recorder protocol rather than a plain read. `None` is the
    /// honest answer there: *we have not asked*, not *not a recorder*.
    pub is_en12830: Option<bool>,
    pub last_archived_ts: Option<i64>,
    pub last_download_ts: Option<i64>,
    /// `true` = heard but not enrolled. On the wire, `null`/`false` means **registered**.
    pub discovered: bool,
    pub field_alarm_states: BTreeMap<String, String>,
    pub alarm_state: String,
}

impl BeaconState {
    pub fn new(mac: MacAddr, name: Option<String>, discovered: bool) -> BeaconState {
        BeaconState {
            mac,
            name,
            reading: None,
            rssi: None,
            last_seen_ts: None,
            provisioning: Provisioning::Pending,
            provision_attempts: 0,
            gave_up_at: None,
            last_error: None,
            is_en12830: None,
            last_archived_ts: None,
            last_download_ts: None,
            discovered,
            field_alarm_states: BTreeMap::new(),
            alarm_state: AlarmLevel::Normal.as_str().to_string(),
        }
    }

    /// Merge a frame into the stored reading.
    ///
    /// **Present fields overwrite; absent fields keep the last known value** — matching v1, so the
    /// portal sees the same merge semantics. Flags always overwrite, because a flag's absence from
    /// the flags byte *is* its value.
    pub fn apply_reading(&mut self, r: &Reading, rssi: Option<i16>, now: i64) {
        self.reading = Some(match self.reading {
            None => *r,
            Some(prev) => Reading {
                protocol_version: r.protocol_version,
                flags: r.flags,
                temperature_c: r.temperature_c.or(prev.temperature_c),
                humidity_pct: r.humidity_pct.or(prev.humidity_pct),
                magnet_present: r.magnet_present,
                magnet_detected: r.magnet_detected,
                moving: r.moving.or(prev.moving),
                movement_count: r.movement_count.or(prev.movement_count),
                pitch_deg: r.pitch_deg.or(prev.pitch_deg),
                roll_deg: r.roll_deg.or(prev.roll_deg),
                low_battery: r.low_battery,
                battery_mv: r.battery_mv.or(prev.battery_mv),
            },
        });
        if rssi.is_some() {
            self.rssi = rssi;
        }
        self.last_seen_ts = Some(now);
    }

    /// A beacon never heard from is stale — `None` is not "fresh".
    pub fn is_stale(&self, now: i64, timeout_s: u64) -> bool {
        match self.last_seen_ts {
            None => true,
            Some(seen) => now.saturating_sub(seen) > timeout_s as i64,
        }
    }
}

/// Evaluate the per-field alarm states and the aggregate.
///
/// `low_battery` forces **at least** `WARNING` on `battery`: the beacon is telling us its own
/// supply is going, and there is no threshold to configure for that.
///
/// Returns the aggregate threshold level (never `Disconnected`: staleness is escalated by the
/// snapshot, not here) — what the storage window tracks as its worst state.
pub fn evaluate_alarms(state: &mut BeaconState, thresholds: &[FieldThreshold]) -> AlarmLevel {
    let mut fields: BTreeMap<String, AlarmLevel> = BTreeMap::new();

    if let Some(r) = &state.reading {
        for t in thresholds {
            if !THRESHOLD_FIELDS.contains(&t.field.as_str()) {
                continue;
            }
            let value = match t.field.as_str() {
                "temperature" => r.temperature_c,
                "humidity" => r.humidity_pct.map(f64::from),
                _ => None,
            };
            if let Some(v) = value {
                let previous = state
                    .field_alarm_states
                    .get(&t.field)
                    .and_then(|s| AlarmLevel::parse(s));
                let level = t.evaluate_held(v, previous);
                let slot = fields.entry(t.field.clone()).or_insert(AlarmLevel::Normal);
                *slot = (*slot).max(level);
            }
        }
        if r.low_battery {
            let slot = fields
                .entry("battery".to_string())
                .or_insert(AlarmLevel::Normal);
            *slot = (*slot).max(AlarmLevel::Warning);
        }
    }

    let aggregate = fields.values().copied().max().unwrap_or(AlarmLevel::Normal);
    state.field_alarm_states = fields
        .into_iter()
        .map(|(k, v)| (k, v.as_str().to_string()))
        .collect();
    state.alarm_state = aggregate.as_str().to_string();
    aggregate
}

// ---------------------------------------------------------------------------------------------
// The tracked set
// ---------------------------------------------------------------------------------------------

/// What happened to one advertisement frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    /// Decoded and merged into the beacon's state.
    Applied,
    /// Deliberately not tracked (currently unused; reserved for a per-beacon `enabled: false`).
    Ignored,
    /// Counted and dropped whole.
    Unparseable(ParseError),
    /// A new stranger arriving at a full discovered set.
    Capped,
}

/// Take the tracked set for writing, recovering from poisoning rather than panicking.
///
/// `std::sync::RwLock` and not tokio's on purpose: the GATT handlers are synchronous functions so
/// they can be unit-tested without a controller, and this guard is deliberately not `Send` — an
/// accidental hold across an `.await` in a spawned task becomes a compile error instead of a
/// deadlock nobody finds until the bench.
pub fn write_state(
    lock: &std::sync::RwLock<CentralState>,
) -> std::sync::RwLockWriteGuard<'_, CentralState> {
    lock.write().unwrap_or_else(|p| p.into_inner())
}

pub fn read_state(
    lock: &std::sync::RwLock<CentralState>,
) -> std::sync::RwLockReadGuard<'_, CentralState> {
    lock.read().unwrap_or_else(|p| p.into_inner())
}

#[derive(Debug, Clone, Default)]
pub struct CentralState {
    pub adapter_present: bool,
    pub beacons: BTreeMap<MacAddr, BeaconState>,
    /// `true` while the tracked stranger set sits at `discovered_max`. Recomputed on every change,
    /// so `status` and the wire snapshot can never disagree.
    pub discovered_capped: bool,
    /// Frames counted and dropped — §4 rule 2 turns "we are seeing frames we do not understand"
    /// from invisible into visible.
    pub unparseable_frames: u64,
    /// The fleet allowlist. **Daemon state, never written to config** (§8): a beacon enrolled on
    /// another Hub must become audible here without this Hub claiming ownership of it.
    pub known_beacons: BTreeSet<MacAddr>,
    /// What each beacon last reported when its configuration was read over GATT — after a
    /// provisioning session, or by the read-only follow-up to an enrollment. Kept apart from
    /// `beacons`, whose entries come and go with the radio, and seeded from the ledger at start —
    /// a beacon's configuration is a fact about its flash, not about this process.
    pub ble_configs: BTreeMap<MacAddr, ReportedBle>,
    /// Beacons whose last configuration session failed. Sticky until a later success, a read or a
    /// removal, and seeded from the ledger at start, so "konfigurace selhala" survives a restart —
    /// kept apart from `beacons` for the same reason as `ble_configs`.
    pub failed: BTreeSet<MacAddr>,
    /// `eye.auto_discover` is off: beacons that are neither enrolled nor fleet-known are not tracked.
    /// Mirrored from the fleet section on every scan tick; `false` (tracking on) is the default.
    pub discovery_off: bool,
    /// What each recorder last said about its archive recording — read over the raw ATT bearer
    /// after a recording command or before a download. Persisted in the ledger and seeded from it
    /// at start (beacon_history.md §8, amended 2026-10-09); never read is unknown, and unknown is
    /// never judged.
    pub recordings: BTreeMap<MacAddr, RecordingReport>,
    /// `eye.recording_interval_min`, mirrored from the fleet section on every scan tick like
    /// `discovery_off`. `None` until the first tick, and nothing is judged against it until then.
    pub recording_target_min: Option<u16>,
    /// Per-tag overrides of that cadence from `beacon-recording` (`0` = stopped), mirrored likewise.
    pub recording_overrides: BTreeMap<MacAddr, u16>,
    /// Archive downloads waiting to be retried (beacon_history.md §2.4).
    pub history_retries: crate::central::history::HistoryRetries,
    /// Failed provisioning sessions the Hub retries by itself on the beacon's next advertisement,
    /// and the Unix second before which it does not (beacon_history.md §8, amended 2026-10-09).
    /// Mirrors the ledger's `retry_at` and is seeded from it, so a restart keeps the ladder.
    pub provision_retries: BTreeMap<MacAddr, i64>,
    /// Beacons a provisioning session of this Hub configured (the ledger's `provisioned: true`),
    /// seeded from the ledger: the ones held to their archive cadence (§3).
    pub provisioned: BTreeSet<MacAddr>,
    /// Recording dials owed to provisioned recorders that do not hold their cadence.
    pub recording_retries: crate::central::recording::RecordingRetries,
}

/// A recorder's archive recording as it reported it (record info, beacon-btsmp1.md §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordingReport {
    pub recording: bool,
    pub interval_min: u16,
    /// Unix seconds.
    pub read_at: i64,
}

impl RecordingReport {
    pub fn from_info(info: &crate::central::en12830::protocol::RecordInfo, read_at: i64) -> Self {
        RecordingReport {
            recording: info.is_recording,
            interval_min: info.interval_s / 60,
            read_at,
        }
    }

    /// Whether the Beacon records at the target cadence; a target of `0` means it should not.
    pub fn holds(&self, target_min: u16) -> bool {
        if target_min == 0 {
            return !self.recording;
        }
        self.recording && self.interval_min == target_min
    }
}

/// The archive cadence is part of the configuration (beacon_history.md §3): a Beacon read
/// recording at another interval, or not at all, `differs` from the fleet even when every GATT
/// characteristic matches. Only a known report is judged, and only against a known target.
fn judge_recording(
    provisioning: Provisioning,
    recording: Option<&RecordingReport>,
    target_min: Option<u16>,
) -> Provisioning {
    match (provisioning, recording, target_min) {
        (Provisioning::Provisioned, Some(r), Some(t)) if !r.holds(t) => Provisioning::Differs,
        _ => provisioning,
    }
}

/// The `provisioning` a beacon publishes (central.md §5, amended 2026-09-23). A session in flight or
/// a read-only beacon is what it is; a failed session is sticky; a beacon this process configured
/// and verified is configured. Otherwise the last read decides — `provisioned` when it holds the
/// fleet profile, whoever wrote it, `differs` when it does not — and with no read yet it is
/// `pending`. Derived rather than stored, so it holds across a restart for as long as the reported
/// configuration and the failure are seeded from the ledger; `ble_config.verified` still says
/// whether this Hub wrote and checked it.
fn effective_provisioning(
    provisioning: Provisioning,
    reported: Option<&ReportedBle>,
    failed: bool,
    profile: &crate::config::BeaconProfile,
) -> Provisioning {
    match provisioning {
        Provisioning::InProgress | Provisioning::ReadOnly => provisioning,
        _ if failed => Provisioning::Failed,
        Provisioning::Failed | Provisioning::Provisioned => provisioning,
        Provisioning::Pending | Provisioning::Differs => match reported {
            Some(r) if holds_profile(&r.config, profile) => Provisioning::Provisioned,
            Some(_) => Provisioning::Differs,
            None => Provisioning::Pending,
        },
    }
}

/// A configuration as read back from a beacon, and when (Unix seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportedBle {
    pub config: BleConfig,
    pub read_at: i64,
    /// `true` when the read closed a session that wrote the profile and it matched; `false` for a
    /// plain read, which reports what the beacon holds and vouches for nothing.
    pub verified: bool,
}

impl CentralState {
    /// A MAC occupies stranger budget only while it is in **neither** narrower set — the
    /// `tags[] ⊂ known_beacons ⊂ discovered` precedence chain of §4.
    pub fn is_stranger(&self, mac: &MacAddr, enrolled: &BTreeSet<MacAddr>) -> bool {
        !enrolled.contains(mac) && !self.known_beacons.contains(mac)
    }

    pub fn stranger_count(&self, enrolled: &BTreeSet<MacAddr>) -> u32 {
        self.beacons
            .keys()
            .filter(|m| self.is_stranger(m, enrolled))
            .count() as u32
    }

    /// The tracked members that are heard-but-not-enrolled — what `beacon-scan-list` reports.
    pub fn discovered_macs(&self, enrolled: &BTreeSet<MacAddr>) -> Vec<MacAddr> {
        self.beacons
            .keys()
            .filter(|m| !enrolled.contains(m))
            .copied()
            .collect()
    }

    /// Get or create the entry for `mac`. `discovered` is derived, never passed in, so the axis
    /// cannot drift from the membership that defines it.
    pub fn entry(
        &mut self,
        mac: MacAddr,
        name: Option<String>,
        enrolled: &BTreeSet<MacAddr>,
    ) -> &mut BeaconState {
        let discovered = !enrolled.contains(&mac);
        self.beacons
            .entry(mac)
            .or_insert_with(|| BeaconState::new(mac, name, discovered))
    }

    fn recompute_capped(&mut self, cfg: &CentralConfig, enrolled: &BTreeSet<MacAddr>) {
        self.discovered_capped = self.stranger_count(enrolled) >= cfg.discovered_max;
    }

    /// Handle one advertisement frame. Pure: no I/O, so the whole bound is unit-testable.
    pub fn on_frame(
        &mut self,
        mac: MacAddr,
        value: &[u8],
        rssi: Option<i16>,
        now: i64,
        cfg: &CentralConfig,
        enrolled: &BTreeSet<MacAddr>,
    ) -> FrameOutcome {
        // Decode first, and count a failure regardless of membership: a neighbour's newer firmware
        // is not this Hub's problem, but "we are hearing frames we cannot read" is worth seeing.
        let parse = match registry::lookup(registry::DEFAULT_PARSER) {
            Some(f) => f,
            None => return FrameOutcome::Ignored,
        };
        let reading = match parse(value) {
            Ok(r) => r,
            Err(e) => {
                self.unparseable_frames = self.unparseable_frames.saturating_add(1);
                return FrameOutcome::Unparseable(e);
            }
        };

        // A stranger arriving at a full set is not tracked at all — the cap is enforced, and the
        // overflow reported, never silently dropped.
        let known_here = self.beacons.contains_key(&mac);
        if !known_here && self.is_stranger(&mac, enrolled) {
            if self.discovery_off {
                // `eye.auto_discover: false` — nobody asked to see beacons nobody registered.
                return FrameOutcome::Ignored;
            }
            let strangers = self.stranger_count(enrolled);
            if strangers >= cfg.discovered_max {
                self.discovered_capped = true;
                return FrameOutcome::Capped;
            }
        }

        let entry = self.entry(mac, None, enrolled);
        entry.apply_reading(&reading, rssi, now);
        self.recompute_capped(cfg, enrolled);
        FrameOutcome::Applied
    }

    /// Drop strangers that have gone quiet; keep enrolled beacons and let them go stale.
    ///
    /// `enrolled` is a parameter rather than state because the fleet section is the authority and it
    /// can change under us between ticks — deriving membership from a cached copy is how the axes
    /// drift apart.
    pub fn expire(&mut self, now: i64, cfg: &CentralConfig, enrolled: &BTreeSet<MacAddr>) -> usize {
        let timeout = cfg.beacon_timeout_s;
        let doomed: Vec<MacAddr> = self
            .beacons
            .iter()
            .filter(|(mac, b)| {
                // An enrolled beacon is never dropped: it stays with `stale: true` and its alarm
                // escalated, because "the freezer sensor stopped answering" is the alarm. With
                // discovery off a stranger goes at once rather than when it falls silent.
                !enrolled.contains(*mac)
                    && (b.is_stale(now, timeout)
                        || (self.discovery_off && self.is_stranger(mac, enrolled)))
            })
            .map(|(mac, _)| *mac)
            .collect();
        for mac in &doomed {
            self.beacons.remove(mac);
        }
        self.recompute_capped(cfg, enrolled);
        doomed.len()
    }

    /// Removal resets a beacon to unregistered: it leaves the tracked set entirely and becomes a
    /// stranger again on its next frame (UI-13 — reappearing as `discovered: true` is *correct*,
    /// not a failed removal).
    pub fn remove_enrollment(&mut self, mac: &MacAddr) -> bool {
        self.beacons.remove(mac).is_some()
    }

    /// Build the northbound snapshot.
    ///
    /// Every tracked beacon appears; `tags: []` is published deliberately, because membership is
    /// authoritative and the consumer prunes discovered-only beacons by absence.
    pub fn snapshot(
        &mut self,
        now: i64,
        cfg: &CentralConfig,
        gateway: &str,
        enrolled: &BTreeSet<MacAddr>,
        thresholds: &BTreeMap<MacAddr, Vec<FieldThreshold>>,
    ) -> SensorsSnapshot {
        let empty: Vec<FieldThreshold> = Vec::new();
        let macs: Vec<MacAddr> = self.beacons.keys().copied().collect();
        let mut tags = Vec::with_capacity(macs.len());

        for mac in macs {
            let stale = {
                let b = match self.beacons.get(&mac) {
                    Some(b) => b,
                    None => continue,
                };
                b.is_stale(now, cfg.beacon_timeout_s)
            };
            let t = thresholds.get(&mac).unwrap_or(&empty);
            if let Some(b) = self.beacons.get_mut(&mac) {
                evaluate_alarms(b, t);
                if stale {
                    // Escalated on the way out, not stored as a threshold outcome: clearing the
                    // staleness must clear this without re-deriving a threshold state.
                    b.alarm_state = AlarmLevel::Disconnected.as_str().to_string();
                }
            }
            let b = match self.beacons.get(&mac) {
                Some(b) => b,
                None => continue,
            };
            let r = b.reading.as_ref();
            tags.push(BeaconPayload {
                gateway: gateway.to_string(),
                mac: b.mac.to_string(),
                name: b.name.clone(),
                temperature_c: r.and_then(|r| r.temperature_c),
                humidity_pct: r.and_then(|r| r.humidity_pct),
                battery_mv: r.and_then(|r| r.battery_mv),
                // A beacon never heard must not claim flag knowledge: no reading means `null`, not
                // `false` (UI-10's principle).
                low_battery: r.map(|r| r.low_battery),
                magnet_present: r.map(|r| r.magnet_present),
                magnet_detected: r.map(|r| r.magnet_detected),
                moving: r.and_then(|r| r.moving),
                movement_count: r.and_then(|r| r.movement_count),
                pitch_deg: r.and_then(|r| r.pitch_deg),
                roll_deg: r.and_then(|r| r.roll_deg),
                rssi: b.rssi,
                last_seen_ts: b.last_seen_ts,
                stale,
                provisioning: judge_recording(
                    effective_provisioning(
                        b.provisioning,
                        self.ble_configs.get(&mac),
                        self.failed.contains(&mac),
                        &cfg.beacon_profile,
                    ),
                    self.recordings.get(&mac),
                    self.recording_overrides
                        .get(&mac)
                        .copied()
                        .or(self.recording_target_min),
                )
                .as_str()
                .to_string(),
                discovered: !enrolled.contains(&mac),
                is_en12830: b.is_en12830,
                field_alarm_states: b.field_alarm_states.clone(),
                alarm_state: b.alarm_state.clone(),
                ble_config: self.ble_configs.get(&mac).map(|r| {
                    let mut p = BleConfigPayload::from(r);
                    if let Some(rec) = self.recordings.get(&mac) {
                        p.recording = Some(rec.recording);
                        p.logging_interval_min = Some(rec.interval_min);
                    }
                    p
                }),
            });
        }

        self.recompute_capped(cfg, enrolled);
        SensorsSnapshot {
            timestamp: rfc3339_utc(now),
            // Necessarily false when nothing is tracked: the component never emits the
            // contradictory "empty but capped" pair.
            discovered_capped: !tags.is_empty() && self.discovered_capped,
            tags,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Timestamps
// ---------------------------------------------------------------------------------------------

/// `%Y-%m-%dT%H:%M:%SZ` from a Unix second count.
///
/// Hand-rolled rather than pulling in a date library: this is the only date formatting the crate
/// does, and the standalone guarantee makes every dependency a cost to justify. The civil-from-days
/// algorithm is the standard one and is unit-tested against known instants below, including a leap
/// day.
pub fn rfc3339_utc(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (secs / 3_600, (secs % 3_600) / 60, secs % 60);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    // Shift the epoch to 0000-03-01 so leap years fall at the end of the cycle.
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March-based
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (y + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CentralConfig;

    const A: &str = "AA:AA:AA:AA:AA:AA";
    const B: &str = "BB:BB:BB:BB:BB:BB";
    const C: &str = "CC:CC:CC:CC:CC:CC";

    fn mac(s: &str) -> MacAddr {
        MacAddr::parse(s).expect("mac")
    }
    fn set(macs: &[&str]) -> BTreeSet<MacAddr> {
        macs.iter().map(|m| mac(m)).collect()
    }
    fn cfg(discovered_max: u32) -> CentralConfig {
        CentralConfig {
            discovered_max,
            ..CentralConfig::default()
        }
    }
    /// A valid frame: temperature + humidity + battery.
    const FRAME: [u8; 6] = [0x01, 0x83, 0x09, 0xAB, 0x3F, 0x6A];
    fn no_thresholds() -> BTreeMap<MacAddr, Vec<FieldThreshold>> {
        BTreeMap::new()
    }

    #[test]
    fn discovered_null_or_false_means_enrolled_true_means_stranger() {
        let mut s = CentralState::default();
        let c = cfg(64);
        let enrolled = set(&[A]);
        s.on_frame(mac(A), &FRAME, Some(-60), 100, &c, &enrolled);
        s.on_frame(mac(B), &FRAME, Some(-70), 100, &c, &enrolled);
        let snap = s.snapshot(100, &c, "hub", &enrolled, &no_thresholds());
        let by_mac: BTreeMap<&str, bool> = snap
            .tags
            .iter()
            .map(|t| (t.mac.as_str(), t.discovered))
            .collect();
        assert!(!by_mac[A], "enrolled beacons are not `discovered`");
        assert!(by_mac[B], "a stranger is `discovered`");
    }

    #[test]
    fn strangers_are_capped_and_the_cap_is_reported() {
        let mut s = CentralState::default();
        let c = cfg(2);
        let enrolled = BTreeSet::new();
        assert_eq!(
            s.on_frame(mac(A), &FRAME, None, 100, &c, &enrolled),
            FrameOutcome::Applied
        );
        assert_eq!(
            s.on_frame(mac(B), &FRAME, None, 100, &c, &enrolled),
            FrameOutcome::Applied
        );
        // The third stranger is not tracked at all.
        assert_eq!(
            s.on_frame(mac(C), &FRAME, None, 100, &c, &enrolled),
            FrameOutcome::Capped
        );
        assert_eq!(s.beacons.len(), 2, "the bound is on the tracked set");
        let snap = s.snapshot(100, &c, "hub", &enrolled, &no_thresholds());
        // AC 2: the assertion is on the *published* size, because the failure this prevents is a
        // torn-down MQTT client.
        assert_eq!(snap.tags.len(), 2);
        assert!(snap.discovered_capped);
    }

    #[test]
    fn enrolled_subset_known_subset_discovered_precedence() {
        let mut s = CentralState::default();
        let c = cfg(1);
        let enrolled = set(&[A]);
        s.known_beacons = set(&[B]);
        // Neither the enrolled beacon nor the fleet-known one consumes stranger budget...
        assert_eq!(
            s.on_frame(mac(A), &FRAME, None, 100, &c, &enrolled),
            FrameOutcome::Applied
        );
        assert_eq!(
            s.on_frame(mac(B), &FRAME, None, 100, &c, &enrolled),
            FrameOutcome::Applied
        );
        assert_eq!(s.stranger_count(&enrolled), 0);
        // ...so the single stranger slot is still free.
        assert_eq!(
            s.on_frame(mac(C), &FRAME, None, 100, &c, &enrolled),
            FrameOutcome::Applied
        );
        assert_eq!(s.stranger_count(&enrolled), 1);
        assert_eq!(s.beacons.len(), 3);
    }

    #[test]
    fn expiry_drops_strangers_but_keeps_enrolled_as_stale() {
        let mut s = CentralState::default();
        let c = CentralConfig {
            beacon_timeout_s: 600,
            ..CentralConfig::default()
        };
        let enrolled = set(&[A]);
        s.on_frame(mac(A), &FRAME, None, 1_000, &c, &enrolled);
        s.on_frame(mac(B), &FRAME, None, 1_000, &c, &enrolled);

        // Well past the timeout.
        let dropped = s.expire(2_000, &c, &enrolled);
        assert_eq!(dropped, 1);
        assert!(s.beacons.contains_key(&mac(A)), "enrolled beacons stay");
        assert!(
            !s.beacons.contains_key(&mac(B)),
            "the stranger leaves by absence"
        );

        let snap = s.snapshot(2_000, &c, "hub", &enrolled, &no_thresholds());
        assert_eq!(snap.tags.len(), 1);
        assert!(snap.tags[0].stale);
        assert_eq!(snap.tags[0].alarm_state, "DISCONNECTED");
    }

    #[test]
    fn removal_resets_to_unregistered_and_a_later_frame_rediscovers() {
        let mut s = CentralState::default();
        let c = cfg(64);
        let enrolled = set(&[A]);
        s.on_frame(mac(A), &FRAME, None, 100, &c, &enrolled);
        assert!(!s.snapshot(100, &c, "hub", &enrolled, &no_thresholds()).tags[0].discovered);

        // The operator removes it: the fleet section no longer lists it, and the tracked entry goes.
        assert!(s.remove_enrollment(&mac(A)));
        let now_enrolled = BTreeSet::new();
        assert!(s
            .snapshot(100, &c, "hub", &now_enrolled, &no_thresholds())
            .tags
            .is_empty());

        // It is still advertising, so the next frame rediscovers it as a stranger — UI-13: this is
        // correct, not a failed removal.
        s.on_frame(mac(A), &FRAME, None, 200, &c, &now_enrolled);
        let snap = s.snapshot(200, &c, "hub", &now_enrolled, &no_thresholds());
        assert_eq!(snap.tags.len(), 1);
        assert!(snap.tags[0].discovered);
        assert_eq!(snap.tags[0].provisioning, "pending", "the axis reset too");
    }

    #[test]
    fn absent_reading_fields_stay_absent_in_the_snapshot() {
        let mut s = CentralState::default();
        let c = cfg(64);
        let enrolled = set(&[A]);
        // Temperature only — no humidity, no battery, no movement.
        s.on_frame(mac(A), &[0x01, 0x01, 0x09, 0xAB], None, 100, &c, &enrolled);
        let snap = s.snapshot(100, &c, "hub", &enrolled, &no_thresholds());
        let t = &snap.tags[0];
        assert_eq!(t.temperature_c, Some(24.75));
        assert_eq!(t.humidity_pct, None, "absent, never 0");
        assert_eq!(t.battery_mv, None);
        assert_eq!(t.movement_count, None);
        assert_eq!(t.moving, None);
        // The JSON keeps them as explicit nulls, which is what the portal reads as "not known".
        let v = serde_json::to_value(t).expect("serialize");
        assert!(v["humidity_pct"].is_null());
    }

    #[test]
    fn a_present_field_overwrites_and_an_absent_one_keeps_the_last_value() {
        let mut b = BeaconState::new(mac(A), None, false);
        let full = registry::lookup("btsmp1").expect("parser")(&FRAME).expect("decodes");
        b.apply_reading(&full, Some(-60), 100);
        assert_eq!(b.reading.expect("set").humidity_pct, Some(63));

        // A temperature-only frame must not erase the last humidity — v1's merge semantics.
        let partial = registry::lookup("btsmp1").expect("parser")(&[0x01, 0x01, 0x00, 0x00])
            .expect("decodes");
        b.apply_reading(&partial, None, 200);
        let r = b.reading.expect("set");
        assert_eq!(r.temperature_c, Some(0.0), "present field overwrites");
        assert_eq!(
            r.humidity_pct,
            Some(63),
            "absent field keeps the last value"
        );
        assert_eq!(b.rssi, Some(-60), "an absent RSSI keeps the last one too");
        assert_eq!(b.last_seen_ts, Some(200));
    }

    #[test]
    fn a_flag_always_overwrites_because_its_absence_is_its_value() {
        let mut b = BeaconState::new(mac(A), None, false);
        // low battery set.
        let low = registry::lookup("btsmp1").expect("parser")(&[0x01, 0x40]).expect("decodes");
        b.apply_reading(&low, None, 100);
        assert!(b.reading.expect("set").low_battery);
        // A later frame without the flag means the battery recovered — not "no news".
        let ok = registry::lookup("btsmp1").expect("parser")(&[0x01, 0x00]).expect("decodes");
        b.apply_reading(&ok, None, 200);
        assert!(!b.reading.expect("set").low_battery);
    }

    #[test]
    fn stale_escalates_alarm_state_to_disconnected() {
        let mut s = CentralState::default();
        let c = CentralConfig {
            beacon_timeout_s: 60,
            ..CentralConfig::default()
        };
        let enrolled = set(&[A]);
        s.on_frame(mac(A), &FRAME, None, 1_000, &c, &enrolled);
        assert_eq!(
            s.snapshot(1_000, &c, "hub", &enrolled, &no_thresholds())
                .tags[0]
                .alarm_state,
            "NORMAL"
        );
        assert_eq!(
            s.snapshot(2_000, &c, "hub", &enrolled, &no_thresholds())
                .tags[0]
                .alarm_state,
            "DISCONNECTED"
        );
    }

    #[test]
    fn low_battery_is_at_least_a_warning_on_battery() {
        let mut b = BeaconState::new(mac(A), None, false);
        let low = registry::lookup("btsmp1").expect("parser")(&[0x01, 0x40]).expect("decodes");
        b.apply_reading(&low, None, 100);
        evaluate_alarms(&mut b, &[]);
        assert_eq!(
            b.field_alarm_states.get("battery").map(String::as_str),
            Some("WARNING")
        );
        assert_eq!(b.alarm_state, "WARNING", "the aggregate is the worst field");
    }

    #[test]
    fn thresholds_evaluate_the_documented_range_rule() {
        let t = FieldThreshold {
            field: "temperature".to_string(),
            critical_low: Some(-25.0),
            warning_low: Some(-20.0),
            warning_high: Some(-15.0),
            critical_high: Some(-10.0),
            hysteresis: None,
        };
        assert_eq!(t.evaluate(-18.0), AlarmLevel::Normal);
        assert_eq!(t.evaluate(-21.0), AlarmLevel::Warning);
        assert_eq!(t.evaluate(-14.0), AlarmLevel::Warning);
        assert_eq!(t.evaluate(-30.0), AlarmLevel::Critical);
        assert_eq!(t.evaluate(-5.0), AlarmLevel::Critical);
        // Exactly on a bound is inside it: the rule is strict `<` / `>`.
        assert_eq!(t.evaluate(-20.0), AlarmLevel::Normal);
        assert_eq!(t.evaluate(-25.0), AlarmLevel::Warning);
    }

    /// #110: a value hovering at a bound holds one grade; a step down waits for the
    /// hysteresis (default 0.5 °C), a step up does not.
    #[test]
    fn a_held_grade_steps_down_only_past_the_hysteresis() {
        let t = FieldThreshold {
            field: "temperature".to_string(),
            critical_low: Some(20.0),
            warning_low: Some(27.0),
            warning_high: Some(38.0),
            critical_high: Some(41.0),
            hysteresis: None,
        };
        let mut prev = None;
        let grades: Vec<AlarmLevel> = [27.3, 26.9, 27.1, 27.4, 27.5, 41.1, 40.6, 40.5]
            .iter()
            .map(|&v| {
                let g = t.evaluate_held(v, prev);
                prev = Some(g);
                g
            })
            .collect();
        use AlarmLevel::*;
        assert_eq!(
            grades,
            [Normal, Warning, Warning, Warning, Normal, Critical, Critical, Warning]
        );
        let strict = FieldThreshold {
            hysteresis: Some(0.0),
            ..t
        };
        assert_eq!(strict.evaluate_held(27.1, Some(Warning)), Normal);
    }

    #[test]
    fn an_absent_bound_is_not_evaluated() {
        // A half-configured threshold must not read as "always critical".
        let t = FieldThreshold {
            field: "temperature".to_string(),
            critical_low: None,
            warning_low: None,
            warning_high: None,
            critical_high: Some(8.0),
            hysteresis: None,
        };
        assert_eq!(t.evaluate(-40.0), AlarmLevel::Normal, "no low bound is set");
        assert_eq!(t.evaluate(9.0), AlarmLevel::Critical);
    }

    #[test]
    fn only_temperature_and_humidity_are_threshold_driven() {
        let mut b = BeaconState::new(mac(A), None, false);
        let full = registry::lookup("btsmp1").expect("parser")(&FRAME).expect("decodes");
        b.apply_reading(&full, None, 100);
        let thresholds = vec![
            FieldThreshold {
                field: "battery_mv".to_string(),
                critical_low: Some(9_000.0),
                warning_low: None,
                warning_high: None,
                critical_high: None,
                hysteresis: None,
            },
            FieldThreshold {
                field: "humidity".to_string(),
                critical_low: None,
                warning_low: None,
                warning_high: Some(50.0),
                critical_high: None,
                hysteresis: None,
            },
        ];
        evaluate_alarms(&mut b, &thresholds);
        assert!(
            !b.field_alarm_states.contains_key("battery_mv"),
            "a threshold on a non-threshold field is ignored, not applied"
        );
        assert_eq!(
            b.field_alarm_states.get("humidity").map(String::as_str),
            Some("WARNING"),
            "63 % is above the 50 % warning bound"
        );
    }

    #[test]
    fn is_en12830_stays_none_until_probed() {
        let mut s = CentralState::default();
        let c = cfg(64);
        let enrolled = set(&[A]);
        s.on_frame(mac(A), &FRAME, None, 100, &c, &enrolled);
        let snap = s.snapshot(100, &c, "hub", &enrolled, &no_thresholds());
        assert_eq!(
            snap.tags[0].is_en12830, None,
            "None means `we have not asked`, not `not a recorder`"
        );
        let v = serde_json::to_value(&snap.tags[0]).expect("serialize");
        assert!(v["is_en12830"].is_null(), "and it is null on the wire");
    }

    #[test]
    fn an_empty_snapshot_still_serializes_tags_as_an_empty_array() {
        let mut s = CentralState::default();
        let c = cfg(64);
        let snap = s.snapshot(100, &c, "hub", &BTreeSet::new(), &no_thresholds());
        assert!(snap.tags.is_empty());
        let json = serde_json::to_string(&snap).expect("serialize");
        assert!(json.contains("\"tags\":[]"), "{json}");
    }

    #[test]
    fn an_empty_snapshot_reports_discovered_capped_false() {
        let mut s = CentralState::default();
        let c = cfg(1);
        let enrolled = BTreeSet::new();
        // Fill and overflow, so the flag is set...
        s.on_frame(mac(A), &FRAME, None, 100, &c, &enrolled);
        assert_eq!(
            s.on_frame(mac(B), &FRAME, None, 100, &c, &enrolled),
            FrameOutcome::Capped
        );
        assert!(s.discovered_capped);
        // ...then let everything expire. An empty set cannot be capped.
        s.expire(100_000, &c, &enrolled);
        let snap = s.snapshot(100_000, &c, "hub", &enrolled, &no_thresholds());
        assert!(snap.tags.is_empty());
        assert!(
            !snap.discovered_capped,
            "the component never emits the contradictory empty-but-capped pair"
        );
    }

    #[test]
    fn capped_is_true_only_while_the_set_sits_at_the_bound() {
        let mut s = CentralState::default();
        let c = CentralConfig {
            discovered_max: 2,
            beacon_timeout_s: 60,
            ..CentralConfig::default()
        };
        let enrolled = BTreeSet::new();
        s.on_frame(mac(A), &FRAME, None, 1_000, &c, &enrolled);
        assert!(!s.discovered_capped, "one of two is not at the bound");
        s.on_frame(mac(B), &FRAME, None, 1_000, &c, &enrolled);
        assert!(s.discovered_capped, "two of two is at the bound");

        // A keeps talking; B goes quiet and is dropped, so there is room again.
        s.on_frame(mac(A), &FRAME, None, 1_100, &c, &enrolled);
        s.expire(1_100, &c, &enrolled);
        assert_eq!(s.beacons.len(), 1);
        assert!(!s.discovered_capped);
    }

    #[test]
    fn an_unparseable_frame_is_counted_and_the_beacon_is_not_tracked() {
        let mut s = CentralState::default();
        let c = cfg(64);
        let enrolled = BTreeSet::new();
        assert!(matches!(
            s.on_frame(mac(A), &[0x02, 0x00], None, 100, &c, &enrolled),
            FrameOutcome::Unparseable(ParseError::UnknownVersion(2))
        ));
        assert_eq!(s.unparseable_frames, 1);
        assert!(
            s.beacons.is_empty(),
            "nothing partial is ever published, so nothing partial is tracked"
        );
        // The loop keeps decoding the good frames.
        assert_eq!(
            s.on_frame(mac(B), &FRAME, None, 100, &c, &enrolled),
            FrameOutcome::Applied
        );
        assert_eq!(s.unparseable_frames, 1);
    }

    #[test]
    fn the_provisioning_axis_spells_itself_the_documented_way() {
        assert_eq!(Provisioning::Pending.as_str(), "pending");
        assert_eq!(
            Provisioning::InProgress.as_str(),
            "provisioning",
            "the wire spelling is the gerund, not in_progress"
        );
        assert_eq!(Provisioning::Provisioned.as_str(), "provisioned");
        assert_eq!(Provisioning::Differs.as_str(), "differs");
        assert_eq!(Provisioning::Failed.as_str(), "failed");
        assert_eq!(Provisioning::ReadOnly.as_str(), "read_only");
    }

    #[test]
    fn a_beacon_that_already_holds_the_fleet_profile_is_configured_whoever_wrote_it() {
        let profile = crate::config::BeaconProfile::default();
        let read = |adv: u16, sensors: Option<u8>| ReportedBle {
            config: BleConfig {
                advertising_interval_ms: adv,
                tx_power_dbm: profile.tx_power_dbm,
                protocol_type: super::super::provision::PROTOCOL_EYE_SENSOR,
                active_sensors: sensors,
            },
            read_at: 1_000,
            verified: false,
        };
        let fleet = read(10_000, Some(0x03));
        let recorder = read(10_000, None);
        let elsewhere = read(1_000, Some(0x03));

        let p = |state, reported: Option<&ReportedBle>| {
            effective_provisioning(state, reported, false, &profile)
        };
        assert_eq!(
            p(Provisioning::Pending, Some(&fleet)),
            Provisioning::Provisioned
        );
        assert_eq!(
            p(Provisioning::Pending, Some(&recorder)),
            Provisioning::Provisioned,
            "no active-sensor characteristic is not a mismatch — provisioning skips it too"
        );
        assert_eq!(
            p(Provisioning::Pending, Some(&elsewhere)),
            Provisioning::Differs,
            "a beacon on another interval is told apart from one not read yet"
        );
        assert_eq!(p(Provisioning::Pending, None), Provisioning::Pending);
        assert_eq!(
            p(Provisioning::Failed, Some(&fleet)),
            Provisioning::Failed,
            "a failed session is not overruled by an older read"
        );
        assert_eq!(
            p(Provisioning::InProgress, Some(&fleet)),
            Provisioning::InProgress
        );
    }

    #[test]
    fn a_failed_session_is_sticky_and_a_session_in_flight_shows_through_it() {
        let profile = crate::config::BeaconProfile::default();
        let fleet = ReportedBle {
            config: BleConfig {
                advertising_interval_ms: 10_000,
                tx_power_dbm: profile.tx_power_dbm,
                protocol_type: super::super::provision::PROTOCOL_EYE_SENSOR,
                active_sensors: Some(0x03),
            },
            read_at: 1_000,
            verified: false,
        };
        // A process that restarted after the failure: the entry is `Pending`, the ledger says it
        // failed, and the old read still says the beacon held the profile.
        assert_eq!(
            effective_provisioning(Provisioning::Pending, Some(&fleet), true, &profile),
            Provisioning::Failed
        );
        assert_eq!(
            effective_provisioning(Provisioning::Pending, None, true, &profile),
            Provisioning::Failed
        );
        assert_eq!(
            effective_provisioning(Provisioning::InProgress, None, true, &profile),
            Provisioning::InProgress,
            "the retry the operator just asked for shows as running"
        );
        assert_eq!(
            effective_provisioning(Provisioning::ReadOnly, None, true, &profile),
            Provisioning::ReadOnly
        );
    }

    #[test]
    fn a_recorder_off_the_fleet_cadence_differs_and_its_recording_rides_ble_config() {
        let profile = crate::config::BeaconProfile::default();
        let c = cfg(10);
        let mut state = CentralState::default();
        state.on_frame(mac(A), &FRAME, Some(-60), 100, &c, &set(&[A]));
        state.ble_configs.insert(
            mac(A),
            ReportedBle {
                config: BleConfig {
                    advertising_interval_ms: 10_000,
                    tx_power_dbm: profile.tx_power_dbm,
                    protocol_type: super::super::provision::PROTOCOL_EYE_SENSOR,
                    active_sensors: None,
                },
                read_at: 1_000,
                verified: true,
            },
        );
        let snap = |state: &mut CentralState| {
            state
                .snapshot(100, &c, "hub", &set(&[A]), &no_thresholds())
                .tags
                .remove(0)
        };

        // Nothing known about the archive: judged on GATT alone, and the keys stay absent.
        let t = snap(&mut state);
        assert_eq!(t.provisioning, "provisioned");
        let v = serde_json::to_value(t.ble_config.expect("read")).expect("serializes");
        assert!(v.get("recording").is_none() && v.get("logging_interval_min").is_none());

        state.recording_target_min = Some(15);
        let report = |recording, interval_min| RecordingReport {
            recording,
            interval_min,
            read_at: 1_100,
        };
        state.recordings.insert(mac(A), report(true, 15));
        let t = snap(&mut state);
        assert_eq!(t.provisioning, "provisioned");
        let b = t.ble_config.expect("read");
        assert_eq!(
            (b.recording, b.logging_interval_min),
            (Some(true), Some(15))
        );

        state.recordings.insert(mac(A), report(true, 5));
        assert_eq!(snap(&mut state).provisioning, "differs", "another cadence");
        state.recordings.insert(mac(A), report(false, 15));
        assert_eq!(snap(&mut state).provisioning, "differs", "not recording");

        // An operator's own `beacon-recording` for this tag is the target, not the fleet's.
        state.recording_overrides.insert(mac(A), 0);
        assert_eq!(
            snap(&mut state).provisioning,
            "provisioned",
            "stopped on purpose"
        );
        state.recording_overrides.insert(mac(A), 5);
        state.recordings.insert(mac(A), report(true, 5));
        assert_eq!(
            snap(&mut state).provisioning,
            "provisioned",
            "its own cadence"
        );

        // A session in flight or a failure is not overruled by the archive.
        assert_eq!(
            judge_recording(Provisioning::InProgress, Some(&report(false, 15)), Some(15)),
            Provisioning::InProgress
        );
        assert_eq!(
            judge_recording(Provisioning::Provisioned, Some(&report(false, 15)), None),
            Provisioning::Provisioned,
            "no target yet: nothing to judge against"
        );
    }

    #[test]
    fn with_discovery_off_strangers_are_neither_tracked_nor_kept() {
        let cfg = cfg(64);
        let enrolled = set(&[A]);
        let mut s = CentralState::default();
        assert_eq!(
            s.on_frame(mac(B), &FRAME, Some(-60), 1_000, &cfg, &enrolled),
            FrameOutcome::Applied,
            "tracking strangers is the default"
        );
        s.discovery_off = true;
        s.expire(1_001, &cfg, &enrolled);
        assert!(
            !s.beacons.contains_key(&mac(B)),
            "a tracked stranger goes on the next tick"
        );
        assert_eq!(
            s.on_frame(mac(C), &FRAME, Some(-60), 1_002, &cfg, &enrolled),
            FrameOutcome::Ignored
        );
        assert_eq!(
            s.on_frame(mac(A), &FRAME, Some(-60), 1_002, &cfg, &enrolled),
            FrameOutcome::Applied,
            "an enrolled beacon is heard as before"
        );
        s.known_beacons = set(&[C]);
        assert_eq!(
            s.on_frame(mac(C), &FRAME, Some(-60), 1_003, &cfg, &enrolled),
            FrameOutcome::Applied,
            "and so is a fleet-known one"
        );
    }

    #[test]
    fn alarm_levels_are_ordered_worst_last() {
        assert!(AlarmLevel::Normal < AlarmLevel::Warning);
        assert!(AlarmLevel::Warning < AlarmLevel::Critical);
        assert!(AlarmLevel::Critical < AlarmLevel::Disconnected);
    }

    #[test]
    fn the_timestamp_is_rfc3339_utc_to_the_second() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(946_684_800), "2000-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_577_836_800), "2020-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_736_775_000), "2025-01-13T13:30:00Z");
        assert_eq!(
            rfc3339_utc(1_709_164_800),
            "2024-02-29T00:00:00Z",
            "a leap day"
        );
        assert_eq!(rfc3339_utc(4_102_444_800), "2100-01-01T00:00:00Z");
    }

    #[test]
    fn a_never_heard_beacon_is_stale_rather_than_fresh() {
        let b = BeaconState::new(mac(A), None, false);
        assert!(b.is_stale(0, 600), "None last-seen is stale, not fresh");
    }

    #[test]
    fn thresholds_decode_from_the_fleet_yaml_shape() {
        let y = "- field: temperature\n  critical_low: -25.0\n  warning_low: -20.0\n- field: humidity\n  warning_high: 60\n";
        let values: Vec<serde_yaml::Value> = serde_yaml::from_str(y).expect("parses");
        let t = FieldThreshold::from_values(&values);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].field, "temperature");
        assert_eq!(t[0].critical_low, Some(-25.0));
        assert_eq!(t[0].critical_high, None);
        assert_eq!(t[1].warning_high, Some(60.0));
    }
}
