//! Heuristic reassembly of multi-frame STICKER telemetry reports (#64).
//!
//! # Why this is heuristic
//!
//! Firmware v1.4.0 splits one report across several fPort-2 frames when the
//! snapshot does not fit the on-air budget (`app_compose.c:355-397`), but it puts
//! **no frame index, frame count or more-flag on the wire**. `*more` is an
//! internal return value in the firmware, never transmitted. So a receiver cannot
//! reassemble deterministically; it can only infer boundaries. Everything below is
//! inference, and the acceptance criterion "multi-frame reports persist as a
//! single coherent snapshot" cannot be met exactly without a firmware wire change.
//!
//! # What the firmware does guarantee
//!
//! Three properties make the inference sound in practice:
//!
//! 1. **Groups are atomic.** A frame carries whole groups: `G_INTERNAL`
//!    (temperature, humidity), `G_SYSTEM` (voltage, system_flags), `G_BAROMETER`,
//!    `G_LIGHT`, `G_ACCEL` (orientation, accel_motion_count), `G_PIR`, `G_HALL_L/R`,
//!    `G_INPUT_A/B`, then `w1_sensors` one reading at a time in ascending slot
//!    order (`app_compose.c:52-64`).
//! 2. **A field never repeats within one report.** The snapshot is frozen once and
//!    each group is cleared from `m_pending` as it is sent, so every frame of a
//!    report carries a disjoint set of keys.
//! 3. **`G_SYSTEM` is present in every report.** So the *second* time `voltage`
//!    appears, that is a new report — a firmware-guaranteed boundary, and the
//!    reason a too-long window is safe while a too-short one is not.
//!
//! `system_flags` is deliberately **not** used as a frame-0 marker: a single
//! oversized group can be force-emitted alone into a frame
//! (`app_compose.c:399-421`), so the system group is not always first.
//!
//! # Window sizing
//!
//! Frames are `FRAME_GAP_SEC = 3` s apart on success but `FRAME_RETRY_SEC = 15` s
//! apart after a duty-cycle failure (`app_lrw.c:139-140`), and the minimum
//! `interval_report` is 60 s. That leaves exactly one safe band: the window must
//! exceed 15 s and stay under 60 s. 20 s is the default. A shorter window (say
//! 10 s) works on the happy path and silently splits any report that hit a retry.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use super::chirpstack::StickerReading;

/// Counters that are transport metadata rather than sensor readings, so they must
/// not be treated as report content.
///
/// `fCnt` is the important one: `parse_uplink` injects it into `counters` for
/// **every** uplink, so a naive overlap check built from `counters.keys()` would
/// fire on the very first continuation frame and reassembly would never happen.
const META_COUNTERS: &[&str] = &["fCnt"];

/// Default idle gap after which a partial report is considered complete.
pub const DEFAULT_IDLE_WINDOW: Duration = Duration::from_secs(20);

/// Hard cap on how long one report may accumulate, so a misbehaving device cannot
/// hold a partial open indefinitely.
pub const DEFAULT_MAX_SPAN: Duration = Duration::from_secs(120);

/// Why a report was flushed — recorded in the payload so the heuristic is visible
/// in the data rather than only in this file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosedBy {
    /// A key repeated, so the previous report must have ended (the strong signal).
    Overlap,
    /// No further frame arrived within the idle window.
    Idle,
    /// The hard span cap was reached.
    MaxSpan,
    /// Flushed because the monitor is reconnecting or shutting down.
    Drain,
}

impl ClosedBy {
    pub fn as_str(self) -> &'static str {
        match self {
            ClosedBy::Overlap => "overlap",
            ClosedBy::Idle => "idle",
            ClosedBy::MaxSpan => "max_span",
            ClosedBy::Drain => "drain",
        }
    }
}

/// A reassembled report plus the provenance of the merge.
#[derive(Debug, Clone)]
pub struct Reassembled {
    pub reading: StickerReading,
    /// How many fPort-2 frames were merged (1 = an ordinary single-frame report).
    pub frames: u32,
    pub fcnt_first: Option<u64>,
    pub fcnt_last: Option<u64>,
    pub closed_by: ClosedBy,
}

impl Reassembled {
    /// True when this report actually spanned more than one frame.
    pub fn was_split(&self) -> bool {
        self.frames > 1
    }
}

struct Partial {
    reading: StickerReading,
    /// Content keys seen so far, used purely as the overlap detector.
    keys: HashSet<String>,
    first_seen: Instant,
    last_seen: Instant,
    frames: u32,
    fcnt_first: Option<u64>,
    fcnt_last: Option<u64>,
}

/// Accumulates fPort-2 frames into coherent snapshots, one partial per device.
///
/// Keyed on `dev_eui` alone because the wire carries no report id — and that is
/// sound, since the firmware holds exactly one frozen `m_snapshot` per device, so
/// a device can never have two reports in flight.
pub struct FrameAssembler {
    idle_window: Duration,
    max_span: Duration,
    pending: HashMap<String, Partial>,
}

impl Default for FrameAssembler {
    fn default() -> Self {
        Self::new(DEFAULT_IDLE_WINDOW, DEFAULT_MAX_SPAN)
    }
}

impl FrameAssembler {
    pub fn new(idle_window: Duration, max_span: Duration) -> Self {
        Self {
            idle_window,
            max_span,
            pending: HashMap::new(),
        }
    }

    pub fn idle_window(&self) -> Duration {
        self.idle_window
    }

    /// The content keys of a frame: sensor fields, non-meta counters, and events.
    ///
    /// Events have to be included because `G_ACCEL`'s `orientation` is surfaced as
    /// an event rather than a field, so a report whose only accelerometer content
    /// is an orientation would otherwise look key-less and never trip the overlap
    /// rule.
    fn content_keys(r: &StickerReading) -> HashSet<String> {
        let mut keys: HashSet<String> = r.fields.keys().cloned().collect();
        for k in r.counters.keys() {
            if !META_COUNTERS.contains(&k.as_str()) {
                keys.insert(k.clone());
            }
        }
        for e in &r.events {
            // Discriminate per channel/slot so two different hall channels in one
            // report are not mistaken for a repeat of each other.
            let disc = e
                .extra
                .get("channel")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| e.extra.get("slot").map(|v| v.to_string()));
            match disc {
                Some(d) => keys.insert(format!("event:{}:{}", e.event_type, d)),
                None => keys.insert(format!("event:{}", e.event_type)),
            };
        }
        keys
    }

    fn fcnt_of(r: &StickerReading) -> Option<u64> {
        r.counters.get("fCnt").copied()
    }

    /// Merge a continuation frame into an accumulating snapshot.
    ///
    /// Deliberate asymmetry, because the two halves answer different questions:
    ///   * `message_id` and `fCnt` anchor on the **first** frame, so the merged row
    ///     is byte-identical to the row frame 0 would have written on its own. That
    ///     is what keeps the `sticker_readings.message_id` UNIQUE constraint safe
    ///     and avoids colliding with rows already in the database.
    ///   * `rssi`, `snr` and `received_at` come from the **last** frame, since the
    ///     freshest link quality and wall clock are the useful ones.
    fn absorb(partial: &mut Partial, frame: StickerReading) {
        partial.reading.fields.extend(frame.fields);
        for (k, v) in frame.counters {
            if META_COUNTERS.contains(&k.as_str()) {
                continue; // keep frame 0's fCnt as the report's anchor
            }
            partial.reading.counters.insert(k, v);
        }
        partial.reading.events.extend(frame.events);
        if frame.rssi.is_some() {
            partial.reading.rssi = frame.rssi;
        }
        if frame.snr.is_some() {
            partial.reading.snr = frame.snr;
        }
        if !frame.received_at.is_empty() {
            partial.reading.received_at = frame.received_at;
        }
        if !frame.device_name.is_empty() {
            partial.reading.device_name = frame.device_name;
        }
    }

    fn finish(dev_eui: &str, partial: Partial, closed_by: ClosedBy) -> Reassembled {
        let _ = dev_eui;
        Reassembled {
            reading: partial.reading,
            frames: partial.frames,
            fcnt_first: partial.fcnt_first,
            fcnt_last: partial.fcnt_last,
            closed_by,
        }
    }

    fn start(reading: StickerReading, now: Instant) -> Partial {
        let keys = Self::content_keys(&reading);
        let fcnt = Self::fcnt_of(&reading);
        Partial {
            reading,
            keys,
            first_seen: now,
            last_seen: now,
            frames: 1,
            fcnt_first: fcnt,
            fcnt_last: fcnt,
        }
    }

    /// Admit one fPort-2 frame. Returns any report(s) that this frame completed.
    ///
    /// At most two come back: an overlapping frame both closes the previous report
    /// and starts a new one.
    ///
    /// Callers must only pass `fport == Some(2)` readings; anything else has no
    /// snapshot semantics and must bypass the assembler.
    pub fn admit(&mut self, reading: StickerReading, now: Instant) -> Vec<Reassembled> {
        let dev_eui = reading.dev_eui.clone();
        let mut out = Vec::new();

        // Expire a stale partial before considering this frame a continuation of it.
        if let Some(p) = self.pending.get(&dev_eui) {
            let idle = now.saturating_duration_since(p.last_seen) >= self.idle_window;
            let too_long = now.saturating_duration_since(p.first_seen) >= self.max_span;
            if idle || too_long {
                let p = self.pending.remove(&dev_eui).expect("just checked");
                let why = if idle {
                    ClosedBy::Idle
                } else {
                    ClosedBy::MaxSpan
                };
                out.push(Self::finish(&dev_eui, p, why));
            }
        }

        match self.pending.get_mut(&dev_eui) {
            None => {
                self.pending.insert(dev_eui, Self::start(reading, now));
            }
            Some(p) => {
                let keys = Self::content_keys(&reading);
                // A repeated key means the previous report finished: the firmware
                // clears each group from m_pending once sent, so within one report
                // no key can come twice.
                if keys.iter().any(|k| p.keys.contains(k)) {
                    let done = self.pending.remove(&dev_eui).expect("just checked");
                    out.push(Self::finish(&dev_eui, done, ClosedBy::Overlap));
                    self.pending.insert(dev_eui, Self::start(reading, now));
                } else {
                    p.keys.extend(keys);
                    p.last_seen = now;
                    p.frames += 1;
                    if let Some(f) = Self::fcnt_of(&reading) {
                        p.fcnt_last = Some(f);
                    }
                    Self::absorb(p, reading);
                }
            }
        }
        out
    }

    /// Flush partials whose idle window or span cap has elapsed. Call once per
    /// monitor loop iteration; resolution is the loop's poll interval.
    pub fn tick(&mut self, now: Instant) -> Vec<Reassembled> {
        let mut done = Vec::new();
        let expired: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| {
                now.saturating_duration_since(p.last_seen) >= self.idle_window
                    || now.saturating_duration_since(p.first_seen) >= self.max_span
            })
            .map(|(k, _)| k.clone())
            .collect();
        for dev_eui in expired {
            if let Some(p) = self.pending.remove(&dev_eui) {
                let why = if now.saturating_duration_since(p.first_seen) >= self.max_span {
                    ClosedBy::MaxSpan
                } else {
                    ClosedBy::Idle
                };
                done.push(Self::finish(&dev_eui, p, why));
            }
        }
        done
    }

    /// Flush everything, e.g. on MQTT reconnect or shutdown. A partial held across
    /// a reconnect would otherwise be merged with frames from a later report.
    pub fn drain_all(&mut self) -> Vec<Reassembled> {
        let keys: Vec<String> = self.pending.keys().cloned().collect();
        keys.into_iter()
            .filter_map(|dev_eui| {
                self.pending
                    .remove(&dev_eui)
                    .map(|p| Self::finish(&dev_eui, p, ClosedBy::Drain))
            })
            .collect()
    }

    /// Number of devices with a report currently accumulating (diagnostics/tests).
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::lorawan::chirpstack::{parse_uplink, StickerEvent};

    fn reading(
        dev_eui: &str,
        fcnt: u64,
        fields: &[(&str, f64)],
        counters: &[(&str, u64)],
    ) -> StickerReading {
        let mut c: HashMap<String, u64> =
            counters.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        c.insert("fCnt".to_string(), fcnt);
        StickerReading {
            dev_eui: dev_eui.to_string(),
            device_name: "sticker".to_string(),
            fields: fields.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            counters: c,
            events: Vec::new(),
            rssi: Some(-70),
            snr: Some(9.0),
            received_at: "2026-07-28T19:00:00Z".to_string(),
            fport: Some(2),
        }
    }

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn two_disjoint_frames_merge_into_one_snapshot() {
        let mut a = FrameAssembler::default();
        let base = t0();
        // frame 0: G_INTERNAL + G_SYSTEM
        let out = a.admit(
            reading(
                "aabb",
                10,
                &[("temperature", 24.5), ("humidity", 53.0), ("voltage", 2.74)],
                &[],
            ),
            base,
        );
        assert!(out.is_empty(), "a first frame completes nothing on its own");
        // frame 1: G_BAROMETER + G_LIGHT, 3 s later
        let out = a.admit(
            reading(
                "aabb",
                11,
                &[("pressure", 1013.2), ("illuminance", 120.0)],
                &[],
            ),
            base + Duration::from_secs(3),
        );
        assert!(out.is_empty(), "still accumulating");

        let done = a.tick(base + Duration::from_secs(25));
        assert_eq!(done.len(), 1);
        let r = &done[0];
        assert_eq!(r.frames, 2);
        assert!(r.was_split());
        assert_eq!(r.closed_by, ClosedBy::Idle);
        // One coherent snapshot carrying every group.
        assert_eq!(r.reading.fields.len(), 5);
        assert_eq!(r.reading.fields["temperature"], 24.5);
        assert_eq!(r.reading.fields["pressure"], 1013.2);
        // Anchored on frame 0 so the message_id matches what frame 0 alone produced.
        assert_eq!(r.reading.counters["fCnt"], 10);
        assert_eq!(r.fcnt_first, Some(10));
        assert_eq!(r.fcnt_last, Some(11));
    }

    #[test]
    fn repeated_voltage_starts_a_new_report() {
        // G_SYSTEM (voltage) is in every report, so a second voltage is the
        // firmware-guaranteed report boundary.
        let mut a = FrameAssembler::default();
        let base = t0();
        a.admit(
            reading("aabb", 10, &[("voltage", 2.74), ("temperature", 24.5)], &[]),
            base,
        );
        let out = a.admit(
            reading("aabb", 12, &[("voltage", 2.73), ("temperature", 24.1)], &[]),
            base + Duration::from_secs(5),
        );
        assert_eq!(
            out.len(),
            1,
            "the overlapping frame closes the previous report"
        );
        assert_eq!(out[0].closed_by, ClosedBy::Overlap);
        assert_eq!(out[0].frames, 1);
        assert_eq!(out[0].reading.fields["temperature"], 24.5);
        // and the new report is now accumulating
        assert_eq!(a.pending_len(), 1);
        let done = a.tick(base + Duration::from_secs(60));
        assert_eq!(done[0].reading.fields["temperature"], 24.1);
    }

    #[test]
    fn fcnt_is_not_an_overlap_key() {
        // REGRESSION GUARD: parse_uplink injects fCnt into counters on EVERY uplink.
        // If fCnt counted as report content, the first continuation frame would
        // always look like an overlap and reassembly would never happen at all.
        let mut a = FrameAssembler::default();
        let base = t0();
        a.admit(reading("aabb", 10, &[("temperature", 24.5)], &[]), base);
        let out = a.admit(
            reading("aabb", 11, &[("pressure", 1013.0)], &[]),
            base + Duration::from_secs(3),
        );
        assert!(
            out.is_empty(),
            "differing fCnt must not be read as an overlap"
        );
        assert_eq!(a.tick(base + Duration::from_secs(25))[0].frames, 2);
    }

    #[test]
    fn an_fcnt_gap_does_not_split_a_report() {
        // An fPort-3 alarm or an fPort-85 response can consume an fCnt between two
        // telemetry frames, so a gap is normal mid-report and must not be a signal.
        let mut a = FrameAssembler::default();
        let base = t0();
        a.admit(reading("aabb", 10, &[("temperature", 24.5)], &[]), base);
        let out = a.admit(
            reading("aabb", 14, &[("pressure", 1013.0)], &[]),
            base + Duration::from_secs(3),
        );
        assert!(out.is_empty());
        let done = a.tick(base + Duration::from_secs(25));
        assert_eq!(done[0].frames, 2);
        assert_eq!(done[0].fcnt_first, Some(10));
        assert_eq!(done[0].fcnt_last, Some(14));
    }

    #[test]
    fn a_retry_gap_of_15s_still_merges() {
        // FRAME_RETRY_SEC = 15 s after a duty-cycle failure. This is exactly the
        // case a 10 s window would silently split.
        let mut a = FrameAssembler::default();
        let base = t0();
        a.admit(reading("aabb", 10, &[("temperature", 24.5)], &[]), base);
        let out = a.admit(
            reading("aabb", 11, &[("pressure", 1013.0)], &[]),
            base + Duration::from_secs(16),
        );
        assert!(
            out.is_empty(),
            "a 16 s retry gap is still one report at a 20 s window"
        );
        assert_eq!(a.tick(base + Duration::from_secs(40))[0].frames, 2);
    }

    #[test]
    fn idle_window_expiry_flushes_the_partial() {
        let mut a = FrameAssembler::default();
        let base = t0();
        a.admit(reading("aabb", 10, &[("temperature", 24.5)], &[]), base);
        assert!(
            a.tick(base + Duration::from_secs(19)).is_empty(),
            "still inside the window"
        );
        let done = a.tick(base + Duration::from_secs(21));
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].closed_by, ClosedBy::Idle);
        assert_eq!(a.pending_len(), 0);
    }

    #[test]
    fn max_span_caps_a_pathological_partial() {
        // A device dribbling one new group every 10 s never trips the idle window,
        // because each frame refreshes last_seen. Only the span cap can stop it, so
        // without the cap a partial would accumulate indefinitely.
        //
        // The cap fires inside admit() — that is the point: the report is closed the
        // moment it becomes too old, not whenever a later tick happens to notice.
        let mut a = FrameAssembler::new(Duration::from_secs(20), Duration::from_secs(120));
        let base = t0();
        let mut flushed: Vec<Reassembled> = Vec::new();
        flushed.extend(a.admit(reading("aabb", 0, &[("temperature", 1.0)], &[]), base));
        for i in 1..20 {
            let mut r = reading("aabb", i as u64, &[], &[]);
            r.fields.insert(format!("ext_temperature_{i}"), i as f64);
            flushed.extend(a.admit(r, base + Duration::from_secs(i * 10)));
        }
        flushed.extend(a.tick(base + Duration::from_secs(400)));

        let capped: Vec<_> = flushed
            .iter()
            .filter(|r| r.closed_by == ClosedBy::MaxSpan)
            .collect();
        assert!(
            !capped.is_empty(),
            "the span cap must close the runaway partial"
        );
        // It was capped at the limit, not left to grow to all 20 frames.
        assert!(
            capped[0].frames < 20,
            "capped after {} frames, so the partial did not run away",
            capped[0].frames
        );
        assert_eq!(a.pending_len(), 0, "nothing left accumulating at the end");
    }

    #[test]
    fn a_single_frame_report_is_unchanged() {
        // The common case must be a pure pass-through: same fields, same fCnt,
        // frames == 1, so nothing about existing behaviour shifts.
        let mut a = FrameAssembler::default();
        let base = t0();
        let input = reading(
            "aabb",
            7,
            &[("temperature", 24.5), ("voltage", 2.74)],
            &[("motion_count", 9)],
        );
        a.admit(input.clone(), base);
        let done = a.tick(base + Duration::from_secs(25));
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].frames, 1);
        assert!(!done[0].was_split());
        assert_eq!(done[0].reading.fields, input.fields);
        assert_eq!(done[0].reading.counters["fCnt"], 7);
        assert_eq!(done[0].reading.counters["motion_count"], 9);
    }

    #[test]
    fn devices_are_independent() {
        let mut a = FrameAssembler::default();
        let base = t0();
        a.admit(reading("aaaa", 1, &[("temperature", 20.0)], &[]), base);
        a.admit(reading("bbbb", 1, &[("temperature", 30.0)], &[]), base);
        // A repeat on one device must not close the other's report.
        let out = a.admit(
            reading("aaaa", 2, &[("temperature", 21.0)], &[]),
            base + Duration::from_secs(2),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].reading.dev_eui, "aaaa");
        assert_eq!(a.pending_len(), 2);
    }

    #[test]
    fn merged_link_quality_comes_from_the_last_frame() {
        let mut a = FrameAssembler::default();
        let base = t0();
        a.admit(reading("aabb", 10, &[("temperature", 24.5)], &[]), base);
        let mut second = reading("aabb", 11, &[("pressure", 1013.0)], &[]);
        second.rssi = Some(-41);
        second.snr = Some(13.5);
        second.received_at = "2026-07-28T19:00:03Z".to_string();
        a.admit(second, base + Duration::from_secs(3));
        let done = a.tick(base + Duration::from_secs(25));
        // Freshest link quality and wall clock...
        assert_eq!(done[0].reading.rssi, Some(-41));
        assert_eq!(done[0].reading.snr, Some(13.5));
        assert_eq!(done[0].reading.received_at, "2026-07-28T19:00:03Z");
        // ...but the identity still anchors on frame 0.
        assert_eq!(done[0].reading.counters["fCnt"], 10);
    }

    #[test]
    fn events_participate_in_overlap_detection() {
        // G_ACCEL's orientation is an event, not a field, so a report whose only
        // accelerometer content is an orientation must still trip the overlap rule.
        let mut a = FrameAssembler::default();
        let base = t0();
        let mut first = reading("aabb", 10, &[], &[]);
        first.events.push(StickerEvent {
            event_type: "orientation".to_string(),
            ts: "t".to_string(),
            extra: serde_json::json!({ "value": 2 }),
        });
        a.admit(first, base);

        let mut second = reading("aabb", 11, &[], &[]);
        second.events.push(StickerEvent {
            event_type: "orientation".to_string(),
            ts: "t".to_string(),
            extra: serde_json::json!({ "value": 3 }),
        });
        let out = a.admit(second, base + Duration::from_secs(3));
        assert_eq!(
            out.len(),
            1,
            "a repeated orientation event is a report boundary"
        );
        assert_eq!(out[0].closed_by, ClosedBy::Overlap);
    }

    #[test]
    fn hall_channels_are_discriminated_not_confused() {
        // Two hall_active events in ONE report differ only by channel. They must not
        // look like a repeat of each other, or every dual-hall sticker would have
        // its reports split.
        let mut a = FrameAssembler::default();
        let base = t0();
        let mut first = reading("aabb", 10, &[], &[("hall_left_count", 4)]);
        first.events.push(StickerEvent {
            event_type: "hall_active".to_string(),
            ts: "t".to_string(),
            extra: serde_json::json!({ "channel": "left", "active": true }),
        });
        a.admit(first, base);

        let mut second = reading("aabb", 11, &[], &[("hall_right_count", 7)]);
        second.events.push(StickerEvent {
            event_type: "hall_active".to_string(),
            ts: "t".to_string(),
            extra: serde_json::json!({ "channel": "right", "active": true }),
        });
        let out = a.admit(second, base + Duration::from_secs(3));
        assert!(
            out.is_empty(),
            "left and right are distinct content, not an overlap"
        );
        let done = a.tick(base + Duration::from_secs(25));
        assert_eq!(done[0].frames, 2);
        assert_eq!(done[0].reading.events.len(), 2);
    }

    #[test]
    fn drain_all_flushes_every_device() {
        let mut a = FrameAssembler::default();
        let base = t0();
        a.admit(reading("aaaa", 1, &[("temperature", 20.0)], &[]), base);
        a.admit(reading("bbbb", 1, &[("temperature", 30.0)], &[]), base);
        let done = a.drain_all();
        assert_eq!(done.len(), 2);
        assert!(done.iter().all(|r| r.closed_by == ClosedBy::Drain));
        assert_eq!(a.pending_len(), 0);
    }

    #[test]
    fn merge_of_split_frames_equals_the_single_real_frame() {
        // The captured live frame 01088901100018b226206b40024809d00125 split on real
        // firmware group boundaries:
        //   frame 0 = G_INTERNAL + G_SYSTEM  -> 01 088901 1000 18b226 206b
        //   frame 1 = G_ACCEL + G_PIR        -> 01 4002 4809 d00125
        // The two halves are DERIVED from the captured frame, not separately
        // captured, so what this pins is that merging them reproduces the whole
        // snapshot exactly — the property the acceptance criterion is about.
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

        let uplink = |hex: &str, fcnt: u64| {
            let raw = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect::<Vec<u8>>();
            serde_json::json!({
                "deviceInfo": { "devEui": "70b3d57ed80051b2", "deviceName": "Motion QA" },
                "fPort": 2, "fCnt": fcnt, "data": B64.encode(&raw),
                "rxInfo": [{ "rssi": -41, "snr": 13.5 }],
                "time": "2026-07-28T19:01:02Z",
            })
            .to_string()
        };

        let whole = parse_uplink(uplink("01088901100018b226206b40024809d00125", 8).as_bytes())
            .unwrap()
            .unwrap();

        let mut a = FrameAssembler::default();
        let base = t0();
        let f0 = parse_uplink(uplink("01088901100018b226206b", 8).as_bytes())
            .unwrap()
            .unwrap();
        let f1 = parse_uplink(uplink("014002480 9d00125".replace(' ', "").as_str(), 9).as_bytes())
            .unwrap()
            .unwrap();
        a.admit(f0, base);
        a.admit(f1, base + Duration::from_secs(3));
        let done = a.tick(base + Duration::from_secs(25));
        assert_eq!(done.len(), 1);
        let merged = &done[0];
        assert_eq!(merged.frames, 2);

        // Field-identical to decoding the unsplit frame.
        assert_eq!(merged.reading.fields, whole.fields);
        assert_eq!(merged.reading.counters, whole.counters);
        assert_eq!(merged.reading.events.len(), whole.events.len());
        for (m, w) in merged.reading.events.iter().zip(whole.events.iter()) {
            assert_eq!(m.event_type, w.event_type);
            assert_eq!(m.extra, w.extra);
        }
    }
}
