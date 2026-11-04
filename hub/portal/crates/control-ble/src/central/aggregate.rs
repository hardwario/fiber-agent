//! The Beacon storage window — `plan/control/storage_intervals.md` §5.
//!
//! Every applied frame is a **sample**: it drives the live state, the thresholds and the
//! `eye/sensors` snapshot exactly as before. What reaches the database is one row per Beacon per
//! **window** of `eye.aggregation_interval_s`, aligned to UTC multiples of the interval, holding
//! min / avg / max per numeric field, the frame count and the worst alarm level. A threshold
//! transition (an alarm raised or cleared) is stored at once, outside the window (decision 3).
//!
//! Pure and clock-free: the scan loop passes `now` in, so the windowing is unit-tested here
//! without a controller. The accumulator lives in memory; a restart loses the open window, as for
//! the 1-Wire lines.

use std::collections::BTreeMap;

use crate::config::{validate_aggregation_interval_s, AGGREGATION_INTERVAL_S};
use crate::host::{AlarmTransition, BeaconAggregate, FieldStats, WindowAggregate};
use crate::mac::MacAddr;

use super::state::{AlarmLevel, BeaconState};

/// The numeric fields aggregated per Beacon, and their units: `temperature` °C, `humidity` %,
/// `battery` mV, `rssi` dBm, `pitch` / `roll` degrees, `movement_count` the Beacon's counter.
pub const AGGREGATE_FIELDS: [&str; 7] = [
    "temperature",
    "humidity",
    "battery",
    "rssi",
    "pitch",
    "roll",
    "movement_count",
];

/// The default `eye.aggregation_interval_s`, used until the fleet section says otherwise.
pub const DEFAULT_INTERVAL_S: u64 = 600;

/// One frame's numeric values, from the Beacon's merged state — the same values the per-frame
/// row used to carry (present fields overwrite, absent ones keep the last known value).
pub fn sample_of(beacon: &BeaconState) -> Vec<(&'static str, f64)> {
    let mut v = Vec::with_capacity(AGGREGATE_FIELDS.len());
    if let Some(r) = &beacon.reading {
        if let Some(t) = r.temperature_c {
            v.push(("temperature", t));
        }
        if let Some(h) = r.humidity_pct {
            v.push(("humidity", f64::from(h)));
        }
        if let Some(b) = r.battery_mv {
            v.push(("battery", f64::from(b)));
        }
    }
    if let Some(rssi) = beacon.rssi {
        v.push(("rssi", f64::from(rssi)));
    }
    if let Some(r) = &beacon.reading {
        if let Some(p) = r.pitch_deg {
            v.push(("pitch", f64::from(p)));
        }
        if let Some(r2) = r.roll_deg {
            v.push(("roll", f64::from(r2)));
        }
        if let Some(m) = r.movement_count {
            v.push(("movement_count", f64::from(m)));
        }
    }
    v
}

/// Round to three decimals: an average of 0.1 °C readings must not print as 21.299999999.
fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[derive(Debug, Clone, Copy)]
struct Stats {
    min: f64,
    max: f64,
    sum: f64,
    n: u64,
}

impl Stats {
    fn new(x: f64) -> Stats {
        Stats {
            min: x,
            max: x,
            sum: x,
            n: 1,
        }
    }
    fn add(&mut self, x: f64) {
        self.min = self.min.min(x);
        self.max = self.max.max(x);
        self.sum += x;
        self.n += 1;
    }
    fn finish(&self) -> FieldStats {
        FieldStats {
            min: round3(self.min),
            avg: round3(self.sum / self.n as f64),
            max: round3(self.max),
        }
    }
}

#[derive(Debug, Clone)]
struct Acc {
    name: Option<String>,
    count: u64,
    fields: BTreeMap<&'static str, Stats>,
    worst: AlarmLevel,
}

/// The open window's bounds: `[start, end)`, Unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub start: i64,
    pub end: i64,
}

/// A closed window: one aggregate per Beacon that had frames in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Closed {
    pub window: Window,
    pub beacons: Vec<BeaconAggregate>,
}

/// A threshold transition to store at once: the triggering frame's values in the row shape.
#[derive(Debug, Clone, PartialEq)]
pub struct TransitionRow {
    pub mac: MacAddr,
    pub ts: i64,
    pub aggregate: WindowAggregate,
}

/// The per-Beacon window accumulator.
#[derive(Debug, Clone)]
pub struct WindowAggregator {
    interval_s: u64,
    /// The last out-of-range value reported, so a bad key is logged once, not every frame.
    rejected: Option<u64>,
    window: Option<Window>,
    /// Where the last closed window ended: the next one never starts before it, so a window
    /// lengthened at runtime cannot reuse a `message_id` already written.
    last_end: Option<i64>,
    acc: BTreeMap<MacAddr, Acc>,
    /// The level of each Beacon's previous frame, for transition detection. A Beacon not seen
    /// since start is taken as `NORMAL`, so one already in alarm at start is stored at once.
    levels: BTreeMap<MacAddr, AlarmLevel>,
}

impl Default for WindowAggregator {
    fn default() -> Self {
        WindowAggregator::new(DEFAULT_INTERVAL_S)
    }
}

impl WindowAggregator {
    pub fn new(interval_s: u64) -> WindowAggregator {
        WindowAggregator {
            interval_s: if AGGREGATION_INTERVAL_S.contains(&interval_s) {
                interval_s
            } else {
                DEFAULT_INTERVAL_S
            },
            rejected: None,
            window: None,
            last_end: None,
            acc: BTreeMap::new(),
            levels: BTreeMap::new(),
        }
    }

    pub fn interval_s(&self) -> u64 {
        self.interval_s
    }

    pub fn window(&self) -> Option<Window> {
        self.window
    }

    /// Hot reload: take the fleet section's value if it is in range, else keep the last good one
    /// and say so once per bad value (the same message `EyeSection::validate` gives). A new
    /// interval applies from the next window; the open one closes on its own bounds.
    pub fn set_interval(&mut self, interval_s: u64) {
        match validate_aggregation_interval_s(interval_s) {
            Ok(()) => {
                self.interval_s = interval_s;
                self.rejected = None;
            }
            Err(e) => {
                if self.rejected != Some(interval_s) {
                    eprintln!(
                        "control.ble: {e}; keeping the storage window at {}s",
                        self.interval_s
                    );
                    self.rejected = Some(interval_s);
                }
            }
        }
    }

    fn open(&mut self, now: i64) {
        let i = self.interval_s as i64;
        let aligned = now.div_euclid(i) * i;
        let start = match self.last_end {
            Some(end) if end > aligned && end <= now => end,
            _ => aligned,
        };
        self.window = Some(Window {
            start,
            end: aligned + i,
        });
    }

    /// Close the open window if `now` is past it, then make sure one is open for `now`.
    ///
    /// Returns the closed window when it held any frames. A clock that steps backwards before the
    /// open window closes it at once rather than holding it open until the clock catches up.
    pub fn roll(&mut self, now: i64) -> Option<Closed> {
        let mut closed = None;
        if let Some(w) = self.window {
            if now >= w.end || now < w.start {
                closed = self.close(w);
                self.last_end = if now >= w.end { Some(w.end) } else { None };
                self.window = None;
            }
        }
        if self.window.is_none() {
            self.open(now);
        }
        closed
    }

    fn close(&mut self, w: Window) -> Option<Closed> {
        let acc = std::mem::take(&mut self.acc);
        if acc.is_empty() {
            return None;
        }
        let window_s = w.end - w.start;
        let beacons = acc
            .into_iter()
            .map(|(mac, a)| BeaconAggregate {
                mac: mac.to_string(),
                name: a.name,
                message_id: super::scan::dedup_id(&mac, w.start),
                window_start: w.start,
                aggregate: WindowAggregate {
                    window_s,
                    count: a.count,
                    alarm_state: a.worst.as_str().to_string(),
                    fields: a
                        .fields
                        .iter()
                        .map(|(k, s)| (k.to_string(), s.finish()))
                        .collect(),
                    transition: None,
                },
            })
            .collect();
        Some(Closed { window: w, beacons })
    }

    /// Add one frame to the open window (call [`Self::roll`] first). Returns the row to store at
    /// once when the frame's alarm level differs from the Beacon's previous frame.
    pub fn add(
        &mut self,
        mac: MacAddr,
        name: Option<String>,
        values: &[(&'static str, f64)],
        level: AlarmLevel,
        now: i64,
    ) -> Option<TransitionRow> {
        if self.window.is_none() {
            self.open(now);
        }
        let acc = self.acc.entry(mac).or_insert_with(|| Acc {
            name: None,
            count: 0,
            fields: BTreeMap::new(),
            worst: AlarmLevel::Normal,
        });
        if name.is_some() {
            acc.name = name;
        }
        acc.count += 1;
        acc.worst = acc.worst.max(level);
        for &(k, x) in values {
            if !x.is_finite() {
                continue;
            }
            acc.fields
                .entry(k)
                .and_modify(|s| s.add(x))
                .or_insert_with(|| Stats::new(x));
        }

        let previous = self.levels.insert(mac, level).unwrap_or(AlarmLevel::Normal);
        if previous == level {
            return None;
        }
        Some(TransitionRow {
            mac,
            ts: now,
            aggregate: WindowAggregate {
                window_s: 0,
                count: 1,
                alarm_state: level.as_str().to_string(),
                fields: values
                    .iter()
                    .filter(|(_, x)| x.is_finite())
                    .map(|&(k, x)| {
                        let x = round3(x);
                        (
                            k.to_string(),
                            FieldStats {
                                min: x,
                                avg: x,
                                max: x,
                            },
                        )
                    })
                    .collect(),
                transition: Some(AlarmTransition {
                    from: previous.as_str().to_string(),
                    to: level.as_str().to_string(),
                }),
            },
        })
    }

    /// Keep the transition baselines of tracked Beacons only, so strangers that came and went do
    /// not accumulate. A Beacon dropped and heard again starts from `NORMAL`.
    pub fn retain_levels(&mut self, tracked: impl Fn(&MacAddr) -> bool) {
        self.levels.retain(|mac, _| tracked(mac));
    }
}

/// `message_id` of a transition row: distinct from the window id even when the frame lands on a
/// window start.
pub fn transition_id(mac: &MacAddr, ts: i64) -> String {
    format!("{mac}-adv-{ts}-alarm")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac(s: &str) -> MacAddr {
        MacAddr::parse(s).unwrap()
    }

    const A: &str = "AA:BB:CC:DD:EE:01";
    const B: &str = "AA:BB:CC:DD:EE:02";
    // 2026-10-12T10:00:00Z, a multiple of 600 and 3600.
    const T0: i64 = 1_791_799_200;

    #[test]
    fn windows_align_to_utc_multiples_of_the_interval() {
        assert_eq!(T0 % 3600, 0);
        let mut agg = WindowAggregator::new(600);
        assert!(agg.roll(T0 + 125).is_none());
        assert_eq!(
            agg.window(),
            Some(Window {
                start: T0,
                end: T0 + 600
            }),
            "a window opened mid-interval still starts on the boundary"
        );
        let mut agg = WindowAggregator::new(60);
        agg.roll(T0 + 125);
        assert_eq!(
            agg.window(),
            Some(Window {
                start: T0 + 120,
                end: T0 + 180
            })
        );
    }

    #[test]
    fn a_window_closes_into_min_avg_max_per_field_and_one_entry_per_beacon() {
        let mut agg = WindowAggregator::new(600);
        agg.roll(T0);
        for (i, t) in [4.0, 5.0, 9.0].into_iter().enumerate() {
            let now = T0 + 10 * i as i64;
            assert!(agg.roll(now).is_none());
            agg.add(
                mac(A),
                Some("freezer".into()),
                &[("temperature", t), ("rssi", -60.0 - i as f64)],
                AlarmLevel::Normal,
                now,
            );
        }
        agg.add(
            mac(B),
            None,
            &[("humidity", 40.0)],
            AlarmLevel::Normal,
            T0 + 30,
        );
        assert!(agg.roll(T0 + 599).is_none(), "still inside the window");

        let closed = agg.roll(T0 + 600).expect("the window closes at its end");
        assert_eq!(
            closed.window,
            Window {
                start: T0,
                end: T0 + 600
            }
        );
        assert_eq!(closed.beacons.len(), 2);
        let a = closed.beacons.iter().find(|b| b.mac == A).unwrap();
        assert_eq!(a.message_id, format!("{A}-adv-{T0}"));
        assert_eq!(a.window_start, T0);
        assert_eq!(a.name.as_deref(), Some("freezer"));
        assert_eq!(a.aggregate.window_s, 600);
        assert_eq!(a.aggregate.count, 3);
        assert_eq!(a.aggregate.alarm_state, "NORMAL");
        let t = a.aggregate.fields["temperature"];
        assert_eq!((t.min, t.avg, t.max), (4.0, 6.0, 9.0));
        let r = a.aggregate.fields["rssi"];
        assert_eq!((r.min, r.avg, r.max), (-62.0, -61.0, -60.0));
        assert!(!a.aggregate.fields.contains_key("humidity"));
        let b = closed.beacons.iter().find(|b| b.mac == B).unwrap();
        assert_eq!(b.aggregate.count, 1);

        // The next window starts empty and on the next boundary.
        assert_eq!(agg.window().unwrap().start, T0 + 600);
        assert!(
            agg.roll(T0 + 1200).is_none(),
            "an empty window closes into nothing"
        );
    }

    #[test]
    fn the_worst_alarm_level_of_the_window_is_kept() {
        let mut agg = WindowAggregator::new(600);
        agg.roll(T0);
        agg.add(mac(A), None, &[], AlarmLevel::Normal, T0);
        agg.add(mac(A), None, &[], AlarmLevel::Critical, T0 + 10);
        agg.add(mac(A), None, &[], AlarmLevel::Warning, T0 + 20);
        let closed = agg.roll(T0 + 600).unwrap();
        assert_eq!(closed.beacons[0].aggregate.alarm_state, "CRITICAL");
    }

    #[test]
    fn a_threshold_transition_is_returned_at_once_with_the_frames_values() {
        let mut agg = WindowAggregator::new(600);
        agg.roll(T0);
        assert!(agg
            .add(
                mac(A),
                None,
                &[("temperature", 4.0)],
                AlarmLevel::Normal,
                T0
            )
            .is_none());
        let raised = agg
            .add(
                mac(A),
                None,
                &[("temperature", 9.25)],
                AlarmLevel::Warning,
                T0 + 10,
            )
            .expect("raised");
        assert_eq!(raised.ts, T0 + 10);
        assert_eq!(raised.aggregate.window_s, 0);
        assert_eq!(raised.aggregate.count, 1);
        assert_eq!(raised.aggregate.alarm_state, "WARNING");
        let t = raised.aggregate.fields["temperature"];
        assert_eq!((t.min, t.avg, t.max), (9.25, 9.25, 9.25));
        assert_eq!(
            raised.aggregate.transition,
            Some(AlarmTransition {
                from: "NORMAL".into(),
                to: "WARNING".into()
            })
        );
        assert!(
            agg.add(mac(A), None, &[], AlarmLevel::Warning, T0 + 20)
                .is_none(),
            "a level that holds is not a transition"
        );
        let cleared = agg
            .add(mac(A), None, &[], AlarmLevel::Normal, T0 + 30)
            .expect("cleared");
        assert_eq!(cleared.aggregate.transition.unwrap().to, "NORMAL");
        assert_eq!(
            agg.roll(T0 + 600).unwrap().beacons[0].aggregate.count,
            4,
            "transition frames are samples of the window too"
        );
        assert_ne!(transition_id(&mac(A), T0), format!("{A}-adv-{T0}"));
    }

    #[test]
    fn a_beacon_already_in_alarm_at_start_is_stored_at_once() {
        let mut agg = WindowAggregator::new(600);
        let row = agg.add(mac(A), None, &[], AlarmLevel::Critical, T0 + 5);
        assert_eq!(row.unwrap().aggregate.transition.unwrap().from, "NORMAL");
    }

    #[test]
    fn a_longer_interval_never_reuses_a_window_start_already_written() {
        let mut agg = WindowAggregator::new(600);
        agg.roll(T0 + 600);
        agg.add(mac(A), None, &[], AlarmLevel::Normal, T0 + 610);
        agg.set_interval(3600);
        assert_eq!(
            agg.window().unwrap().end,
            T0 + 1200,
            "the open window keeps its bounds"
        );
        let first = agg.roll(T0 + 1200).unwrap();
        assert_eq!(first.beacons[0].window_start, T0 + 600);
        // Aligned to the hour this would start at T0 again; it starts where the last one ended.
        assert_eq!(
            agg.window(),
            Some(Window {
                start: T0 + 1200,
                end: T0 + 3600
            })
        );
        agg.add(mac(A), None, &[], AlarmLevel::Normal, T0 + 1300);
        let second = agg.roll(T0 + 3600).unwrap();
        assert_eq!(second.beacons[0].aggregate.window_s, 2400);
        assert_eq!(agg.window().unwrap().start, T0 + 3600);

        // An out-of-range value keeps the last good interval.
        agg.set_interval(5);
        assert_eq!(agg.interval_s(), 3600);
    }

    #[test]
    fn a_clock_stepping_back_closes_the_window_instead_of_holding_it_open() {
        let mut agg = WindowAggregator::new(600);
        agg.roll(T0);
        agg.add(mac(A), None, &[], AlarmLevel::Normal, T0 + 10);
        let closed = agg.roll(T0 - 3600).expect("closed by the step");
        assert_eq!(closed.beacons[0].window_start, T0);
        assert_eq!(agg.window().unwrap().start, T0 - 3600);
    }

    #[test]
    fn non_finite_values_are_not_aggregated() {
        let mut agg = WindowAggregator::new(600);
        agg.roll(T0);
        agg.add(
            mac(A),
            None,
            &[("temperature", f64::NAN), ("humidity", 40.0)],
            AlarmLevel::Normal,
            T0,
        );
        let closed = agg.roll(T0 + 600).unwrap();
        assert!(!closed.beacons[0]
            .aggregate
            .fields
            .contains_key("temperature"));
    }

    #[test]
    fn the_row_payload_has_the_documented_shape() {
        let mut agg = WindowAggregator::new(600);
        agg.roll(T0);
        agg.add(
            mac(A),
            None,
            &[("temperature", 4.5)],
            AlarmLevel::Normal,
            T0,
        );
        let b = &agg.roll(T0 + 600).unwrap().beacons[0];
        let v = serde_json::to_value(&b.aggregate).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "window_s": 600,
                "count": 1,
                "alarm_state": "NORMAL",
                "fields": {"temperature": {"min": 4.5, "avg": 4.5, "max": 4.5}}
            })
        );
    }
}
