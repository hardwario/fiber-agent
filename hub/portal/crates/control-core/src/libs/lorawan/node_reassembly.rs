//! Reassembly of paged NODE telemetry reports (#64, firmware #425 paging).
//!
//! # The page model
//!
//! sticker-firmware PR #480 puts the paging on the wire: `Telemetry.page_index`
//! (28) and `page_count` (29). A report that does not fit one frame goes out as
//! pages `0..page_count-1` of one frozen snapshot: the motherboard groups first,
//! then whole 1-Wire `SensorReading`s, a reading never split. Both fields absent
//! (proto3: `page_index` 0 is omitted on the first page), or `page_count` 0 or 1,
//! means the frame is the complete report.
//!
//! There is no sequence number or snapshot id, so pages are correlated by arrival
//! order per device: `page_index` 0 starts a report, and the report is complete
//! once `page_count` distinct pages arrived. Merging is the union of the fields,
//! counters and events of the pages, which is the union of the motherboard groups
//! plus the concatenation of the 1-Wire readings (they are already flattened to
//! `w1_{slot}_*` names by the decoder, and a reading is never split).
//!
//! # Never drop data
//!
//! A lost page cannot be asked for. A partial report is published as it is when
//! the next report starts (a page 0 or an unpaged frame arrives), when a page that
//! cannot belong to it arrives (another `page_count`, a page seen twice), when no
//! page came for the idle window, or when the span cap is hit. A page whose report
//! start was lost starts a partial of its own. Every received frame ends up in
//! exactly one published reading; the closing reason is recorded with it.
//!
//! *Amended 2026-10-12:* a page whose index is not below its count is malformed.
//! It is published alone, closed as `mismatch`, and neither closes nor counts
//! toward an open report.
//!
//! This replaces the v1.4 heuristic (merge disjoint key sets within a 20 s idle
//! window), which a firmware without page fields needed because it put no frame
//! index on the wire. Such a firmware's frames are now each a report of their own.
//!
//! # Window sizing
//!
//! Pages are `FRAME_GAP_SEC = 3` s apart on success and `FRAME_RETRY_SEC = 15` s
//! after a duty-cycle failure, and the minimum `interval_report` is 60 s, so the
//! idle window stays at 20 s: it only decides when a report with a lost page is
//! published, never whether two pages belong together.
//!
//! *Amended 2026-10-12 (#126):* the 3–15 s gap holds only while the duty cycle has
//! room. At EU868 DR0 one ~50 B page is ~2.5 s of airtime, and the 1 % duty cycle of
//! the sub-band then holds the next page back for up to ~250 s; the firmware retries
//! every 15 s until it may send. A 20 s window published every DR0 report as separate
//! partials. An open partial always knows its `page_count`, so it now waits
//! [`DEFAULT_PAGE_WAIT`] (300 s, one duty-cycle off-time plus a retry and margin) per
//! page still missing, counted from the last page received — a lost middle page then
//! does not close the report before the page after it can arrive. [`DEFAULT_MAX_SPAN`]
//! (15 min) bounds the whole report: three DR0 off-times fit with room, and the
//! shortest `interval_report` that DR0 can sustain is longer than that anyway, so the
//! next report's page 0 normally closes a partial first. Unpaged frames never open a
//! partial and are unaffected.

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use super::chirpstack::NodeReading;
use super::node_payload::TelemetryPage;

/// Counters that are transport metadata rather than sensor readings: the merged
/// report keeps the first page's.
const META_COUNTERS: &[&str] = &["fCnt"];

/// Floor of the idle gap after which a partial report is published as it is.
pub const DEFAULT_IDLE_WINDOW: Duration = Duration::from_secs(20);

/// Idle wait per page still missing, counted from the last page received (#126):
/// one EU868 DR0 duty-cycle off-time (~250 s for a full page) plus the firmware's
/// 15 s retry and margin. See "Window sizing" above.
pub const DEFAULT_PAGE_WAIT: Duration = Duration::from_secs(300);

/// Hard cap on how long one report may accumulate, so a misbehaving device cannot
/// hold a partial open indefinitely. Sized for a DR0 report of four pages (#126).
pub const DEFAULT_MAX_SPAN: Duration = Duration::from_secs(15 * 60);

/// Why a report was published — recorded in the payload so a partial report is
/// visible in the data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosedBy {
    /// Every page arrived, or the frame was a complete report by itself.
    Complete,
    /// A new report started (page 0 or an unpaged frame) before this one was
    /// complete: a page was lost.
    NextReport,
    /// A page that cannot belong to this report arrived (another page count, or a
    /// page index seen twice): a page was lost or the device restarted.
    Mismatch,
    /// No further page arrived within the idle window.
    Idle,
    /// The hard span cap was reached.
    MaxSpan,
    /// Flushed because the monitor is reconnecting or shutting down, or the reading
    /// bypassed the assembler.
    Drain,
}

impl ClosedBy {
    pub fn as_str(self) -> &'static str {
        match self {
            ClosedBy::Complete => "complete",
            ClosedBy::NextReport => "next_report",
            ClosedBy::Mismatch => "mismatch",
            ClosedBy::Idle => "idle",
            ClosedBy::MaxSpan => "max_span",
            ClosedBy::Drain => "drain",
        }
    }
}

/// A reassembled report plus the provenance of the merge.
#[derive(Debug, Clone)]
pub struct Reassembled {
    pub reading: NodeReading,
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
    reading: NodeReading,
    /// `page_count` of the report.
    count: u32,
    /// Page indexes received so far.
    seen: BTreeSet<u32>,
    first_seen: Instant,
    last_seen: Instant,
    frames: u32,
    fcnt_first: Option<u64>,
    fcnt_last: Option<u64>,
}

impl Partial {
    fn complete(&self) -> bool {
        self.seen.len() as u64 >= u64::from(self.count)
    }

    /// Pages of this report not received yet (at least 1 while it is open).
    fn missing(&self) -> u32 {
        let seen = u32::try_from(self.seen.len()).unwrap_or(u32::MAX);
        self.count.saturating_sub(seen).max(1)
    }

    fn accepts(&self, page: TelemetryPage) -> bool {
        page.index != 0 && page.count == self.count && !self.seen.contains(&page.index)
    }
}

/// Accumulates paged fPort-2 frames into whole snapshots, one partial per device.
///
/// Keyed on `dev_eui` alone because the wire carries no report id — and that is
/// sound, since the firmware holds exactly one frozen snapshot per device, so a
/// device can never have two reports in flight.
pub struct FrameAssembler {
    idle_window: Duration,
    page_wait: Duration,
    max_span: Duration,
    pending: HashMap<String, Partial>,
}

impl Default for FrameAssembler {
    fn default() -> Self {
        Self::new(DEFAULT_IDLE_WINDOW, DEFAULT_PAGE_WAIT, DEFAULT_MAX_SPAN)
    }
}

impl FrameAssembler {
    /// A partial is published once it has been idle for
    /// `max(idle_window, page_wait × pages missing)` or has spanned `max_span`.
    pub fn new(idle_window: Duration, page_wait: Duration, max_span: Duration) -> Self {
        Self {
            idle_window,
            page_wait,
            max_span,
            pending: HashMap::new(),
        }
    }

    pub fn idle_window(&self) -> Duration {
        self.idle_window
    }

    /// The longest a received frame can be held before it is published: the span
    /// cap. Only a report with a lost page waits this long.
    pub fn max_hold(&self) -> Duration {
        self.max_span
    }

    /// Idle gap after which `p` is published: its missing pages may each still be
    /// held back by one duty-cycle off-time (#126).
    fn idle_deadline(&self, p: &Partial) -> Duration {
        self.idle_window
            .max(self.page_wait.saturating_mul(p.missing()))
    }

    fn fcnt_of(r: &NodeReading) -> Option<u64> {
        r.counters.get("fCnt").copied()
    }

    /// Merge a later page into an accumulating snapshot.
    ///
    /// Deliberate asymmetry, because the two halves answer different questions:
    ///   * `message_id` and `fCnt` anchor on the **first** frame, so the merged row
    ///     is byte-identical to the row frame 0 would have written on its own. That
    ///     is what keeps the `node_readings.message_id` UNIQUE constraint safe
    ///     and avoids colliding with rows already in the database.
    ///   * `rssi`, `snr` and `received_at` come from the **last** frame, since the
    ///     freshest link quality and wall clock are the useful ones.
    fn absorb(partial: &mut Partial, frame: NodeReading) {
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

    fn finish(partial: Partial, closed_by: ClosedBy) -> Reassembled {
        let mut reading = partial.reading;
        reading.page = None; // the published reading is the (partial) report
        Reassembled {
            reading,
            frames: partial.frames,
            fcnt_first: partial.fcnt_first,
            fcnt_last: partial.fcnt_last,
            closed_by,
        }
    }

    fn single(reading: NodeReading) -> Reassembled {
        let fcnt = Self::fcnt_of(&reading);
        Reassembled {
            reading,
            frames: 1,
            fcnt_first: fcnt,
            fcnt_last: fcnt,
            closed_by: ClosedBy::Complete,
        }
    }

    fn start(reading: NodeReading, page: TelemetryPage, now: Instant) -> Partial {
        let fcnt = Self::fcnt_of(&reading);
        Partial {
            reading,
            count: page.count,
            seen: BTreeSet::from([page.index]),
            first_seen: now,
            last_seen: now,
            frames: 1,
            fcnt_first: fcnt,
            fcnt_last: fcnt,
        }
    }

    /// Admit one fPort-2 frame. Returns the report(s) it completed or closed, in
    /// the order they were received.
    ///
    /// At most two come back: a frame that cannot continue the open report both
    /// closes it and is (or starts) the next one.
    ///
    /// Callers must only pass `fport == Some(2)` readings; anything else has no
    /// snapshot semantics and must bypass the assembler.
    pub fn admit(&mut self, reading: NodeReading, now: Instant) -> Vec<Reassembled> {
        let dev_eui = reading.dev_eui.clone();
        let mut out = Vec::new();

        // Expire a stale partial before considering this frame a continuation of it.
        if let Some(why) = self
            .pending
            .get(&dev_eui)
            .and_then(|p| self.expired(p, now))
        {
            let p = self.pending.remove(&dev_eui).expect("just checked");
            out.push(Self::finish(p, why));
        }

        let Some(page) = reading.page else {
            // A complete report by itself; it also ends any open one.
            if let Some(p) = self.pending.remove(&dev_eui) {
                out.push(Self::finish(p, ClosedBy::NextReport));
            }
            out.push(Self::single(reading));
            return out;
        };

        if !page.in_range() {
            // A page past its own count is malformed: it can belong to no report
            // and must never count toward one's completion, so it is published
            // alone as a mismatch and any open report keeps waiting.
            let mut r = Self::single(reading);
            r.reading.page = None;
            r.closed_by = ClosedBy::Mismatch;
            out.push(r);
            return out;
        }

        match self.pending.get_mut(&dev_eui) {
            Some(p) if p.accepts(page) => {
                p.seen.insert(page.index);
                p.last_seen = now;
                p.frames += 1;
                if let Some(f) = Self::fcnt_of(&reading) {
                    p.fcnt_last = Some(f);
                }
                Self::absorb(p, reading);
            }
            Some(_) => {
                let why = if page.index == 0 {
                    ClosedBy::NextReport
                } else {
                    ClosedBy::Mismatch
                };
                let p = self.pending.remove(&dev_eui).expect("just matched");
                out.push(Self::finish(p, why));
                self.pending
                    .insert(dev_eui.clone(), Self::start(reading, page, now));
            }
            None => {
                // A first page, or a later one whose start was lost: either way
                // it opens a report so nothing it carries is dropped.
                self.pending
                    .insert(dev_eui.clone(), Self::start(reading, page, now));
            }
        }

        if self.pending.get(&dev_eui).is_some_and(Partial::complete) {
            let p = self.pending.remove(&dev_eui).expect("just checked");
            out.push(Self::finish(p, ClosedBy::Complete));
        }
        out
    }

    fn expired(&self, p: &Partial, now: Instant) -> Option<ClosedBy> {
        if now.saturating_duration_since(p.first_seen) >= self.max_span {
            Some(ClosedBy::MaxSpan)
        } else if now.saturating_duration_since(p.last_seen) >= self.idle_deadline(p) {
            Some(ClosedBy::Idle)
        } else {
            None
        }
    }

    /// Publish partials whose idle window or span cap has elapsed. Call once per
    /// monitor loop iteration; resolution is the loop's poll interval.
    pub fn tick(&mut self, now: Instant) -> Vec<Reassembled> {
        let expired: Vec<(String, ClosedBy)> = self
            .pending
            .iter()
            .filter_map(|(k, p)| self.expired(p, now).map(|why| (k.clone(), why)))
            .collect();
        expired
            .into_iter()
            .filter_map(|(dev_eui, why)| {
                self.pending.remove(&dev_eui).map(|p| Self::finish(p, why))
            })
            .collect()
    }

    /// Flush everything, e.g. on MQTT reconnect or shutdown. A partial held across
    /// a reconnect would otherwise be merged with pages of a later report.
    pub fn drain_all(&mut self) -> Vec<Reassembled> {
        let keys: Vec<String> = self.pending.keys().cloned().collect();
        keys.into_iter()
            .filter_map(|dev_eui| {
                self.pending
                    .remove(&dev_eui)
                    .map(|p| Self::finish(p, ClosedBy::Drain))
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
    use crate::libs::lorawan::chirpstack::parse_uplink;

    /// HIL vectors (EU868, machine probe on s1), sticker-firmware PR #480 eff819ed:
    /// one report at DR0 in two pages, and the same sensors at DR5 in one frame.
    const DR0_PAGE_1: &str = "01089e01102718fc26205628ffffffff0f30ffffffff0f4800900100980100\
                              a00100a80100d00100e80102";
    const DR0_PAGE_2: &str = "01da01160801100358fb03620da226a2019c047e005c9a0cfe08e00101e80102";
    const DR5_SINGLE: &str = "01089e011026189827205528ffffffff0f30ffffffff0f4800900100980100\
                              a00100a80100d00100da01170801100358fb03620eb026a2019c04dc04004caa\
                              0c9c09";

    fn uplink(dev_eui: &str, hex_str: &str, fcnt: u64) -> NodeReading {
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        let raw = hex::decode(hex_str).expect("hex");
        let json = serde_json::json!({
            "deviceInfo": { "devEui": dev_eui, "deviceName": "Probe QA" },
            "fPort": 2, "fCnt": fcnt, "data": B64.encode(&raw),
            "rxInfo": [{ "rssi": -60 - fcnt as i64, "snr": 9.0 }],
            "time": format!("2026-10-09T12:00:{:02}Z", fcnt % 60),
        })
        .to_string();
        parse_uplink(json.as_bytes()).unwrap().expect("a reading")
    }

    fn page(r: NodeReading, index: u32, count: u32) -> NodeReading {
        NodeReading {
            page: TelemetryPage::of(index, count),
            ..r
        }
    }

    /// A bare paged reading with one field, for the ordering cases.
    fn frame(dev_eui: &str, field: &str, index: u32, count: u32) -> NodeReading {
        let mut r = uplink(dev_eui, "01", 1); // an empty Telemetry
        r.fields.insert(field.to_string(), 1.0);
        page(r, index, count)
    }

    #[test]
    fn the_hil_pages_decode_their_page_fields() {
        let p1 = uplink("70b3d57ed0051f2a", DR0_PAGE_1, 10);
        let p2 = uplink("70b3d57ed0051f2a", DR0_PAGE_2, 11);
        let one = uplink("70b3d57ed0051f2a", DR5_SINGLE, 12);
        assert_eq!(p1.page, Some(TelemetryPage { index: 0, count: 2 }));
        assert_eq!(p2.page, Some(TelemetryPage { index: 1, count: 2 }));
        assert_eq!(one.page, None);
        // Page 1 is the motherboard, page 2 the machine probe on s1.
        assert!(p1.fields.contains_key("temperature"));
        assert!(!p1.fields.keys().any(|k| k.starts_with("w1_")));
        assert!(p2.fields.contains_key("w1_1_temperature"));
        assert!(!p2.fields.contains_key("temperature"));
    }

    #[test]
    fn the_hil_dr0_pages_merge_into_the_dr5_report_shape() {
        let mut a = FrameAssembler::default();
        let base = Instant::now();
        let p1 = uplink("70b3d57ed0051f2a", DR0_PAGE_1, 10);
        let p2 = uplink("70b3d57ed0051f2a", DR0_PAGE_2, 11);
        assert!(a.admit(p1, base).is_empty(), "page 1 of 2 waits");
        let out = a.admit(p2, base + Duration::from_secs(3));
        assert_eq!(out.len(), 1);
        let merged = &out[0];
        assert_eq!(merged.closed_by, ClosedBy::Complete);
        assert_eq!(merged.frames, 2);
        assert_eq!((merged.fcnt_first, merged.fcnt_last), (Some(10), Some(11)));
        assert_eq!(merged.reading.counters.get("fCnt"), Some(&10));
        assert_eq!(
            merged.reading.rssi,
            Some(-71),
            "link quality of the last page"
        );
        assert_eq!(merged.reading.page, None);
        assert_eq!(a.pending_len(), 0);

        // Same sensors in one DR5 frame: the same keys come out.
        let single = a.admit(uplink("70b3d57ed0051f2a", DR5_SINGLE, 12), base);
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].closed_by, ClosedBy::Complete);
        assert_eq!(single[0].frames, 1);
        let keys = |r: &NodeReading| {
            let mut k: Vec<String> = r.fields.keys().cloned().collect();
            k.extend(r.counters.keys().cloned());
            k.sort();
            k
        };
        assert_eq!(keys(&merged.reading), keys(&single[0].reading));
        let types = |r: &NodeReading| {
            // The DR0 capture is the first report after boot (system_flags bit 0).
            let mut t: Vec<String> = r
                .events
                .iter()
                .map(|e| e.event_type.clone())
                .filter(|t| t != "boot")
                .collect();
            t.sort();
            t
        };
        assert_eq!(types(&merged.reading), types(&single[0].reading));
    }

    #[test]
    fn a_lost_last_page_is_published_on_timeout() {
        let mut a = FrameAssembler::default();
        let base = Instant::now();
        a.admit(uplink("aa", DR0_PAGE_1, 10), base);
        // One page missing: it may be held back by one duty-cycle off-time (#126).
        assert!(a.tick(base + Duration::from_secs(299)).is_empty());
        let done = a.tick(base + Duration::from_secs(300));
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].closed_by, ClosedBy::Idle);
        assert!(done[0].reading.fields.contains_key("temperature"));
    }

    /// #126: at EU868 DR0 the duty cycle holds each page back for minutes; pages
    /// 4 min apart are still one report.
    #[test]
    fn dr0_pages_minutes_apart_reassemble_as_one_report() {
        let mut a = FrameAssembler::default();
        let base = Instant::now();
        let gap = Duration::from_secs(240);
        assert!(a.admit(frame("aa", "a", 0, 3), base).is_empty());
        assert!(a.tick(base + gap).is_empty());
        assert!(a.admit(frame("aa", "b", 1, 3), base + gap).is_empty());
        assert!(a.tick(base + 2 * gap).is_empty());
        let out = a.admit(frame("aa", "c", 2, 3), base + 2 * gap);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].closed_by, ClosedBy::Complete);
        assert_eq!(out[0].frames, 3);
        for f in ["a", "b", "c"] {
            assert!(out[0].reading.fields.contains_key(f), "{f}");
        }
    }

    /// #126: the wait is per missing page, so a lost middle page does not close the
    /// report before the page after it can arrive.
    #[test]
    fn the_idle_wait_scales_with_the_missing_pages() {
        let mut a = FrameAssembler::default();
        let base = Instant::now();
        a.admit(frame("aa", "a", 0, 3), base);
        // Two pages missing: 600 s.
        assert!(a.tick(base + Duration::from_secs(599)).is_empty());
        let out = a.admit(frame("aa", "c", 2, 3), base + Duration::from_secs(540));
        assert!(out.is_empty());
        // One page missing now, counted from the last page.
        assert!(a.tick(base + Duration::from_secs(839)).is_empty());
        let done = a.tick(base + Duration::from_secs(840));
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].closed_by, ClosedBy::Idle);
        assert_eq!(done[0].frames, 2);
    }

    /// #126: the long wait never merges two reports — the next page 0 closes it.
    #[test]
    fn a_new_page_zero_closes_a_waiting_partial() {
        let mut a = FrameAssembler::default();
        let base = Instant::now();
        a.admit(frame("aa", "a", 0, 2), base);
        let out = a.admit(frame("aa", "x", 0, 2), base + Duration::from_secs(200));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].closed_by, ClosedBy::NextReport);
        assert!(!out[0].reading.fields.contains_key("x"));
        assert_eq!(a.pending_len(), 1);
    }

    /// #126: the span cap still bounds a report whose pages keep trickling in.
    #[test]
    fn the_default_span_cap_publishes_a_slow_partial() {
        let mut a = FrameAssembler::default();
        let base = Instant::now();
        let gap = Duration::from_secs(280);
        for i in 0..4 {
            assert!(a.admit(frame("aa", "p", i, 5), base + gap * i).is_empty());
        }
        let done = a.tick(base + DEFAULT_MAX_SPAN);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].closed_by, ClosedBy::MaxSpan);
        assert_eq!(done[0].frames, 4);
    }

    #[test]
    fn a_lost_last_page_is_published_when_the_next_report_starts() {
        let mut a = FrameAssembler::default();
        let base = Instant::now();
        a.admit(uplink("aa", DR0_PAGE_1, 10), base);
        let out = a.admit(uplink("aa", DR0_PAGE_1, 11), base + Duration::from_secs(5));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].closed_by, ClosedBy::NextReport);
        assert_eq!(out[0].fcnt_first, Some(10));
        assert_eq!(a.pending_len(), 1, "the new report waits for its page 2");

        // An unpaged frame also ends an open report, then stands on its own.
        let out = a.admit(uplink("aa", DR5_SINGLE, 12), base + Duration::from_secs(9));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].closed_by, ClosedBy::NextReport);
        assert_eq!(out[0].fcnt_first, Some(11));
        assert_eq!(out[1].closed_by, ClosedBy::Complete);
        assert_eq!(out[1].fcnt_first, Some(12));
        assert_eq!(a.pending_len(), 0);
    }

    #[test]
    fn a_page_whose_start_was_lost_is_kept() {
        let mut a = FrameAssembler::default();
        let base = Instant::now();
        assert!(a.admit(uplink("aa", DR0_PAGE_2, 11), base).is_empty());
        let done = a.tick(base + DEFAULT_PAGE_WAIT);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].closed_by, ClosedBy::Idle);
        assert!(done[0].reading.fields.contains_key("w1_1_temperature"));
    }

    #[test]
    fn three_pages_complete_in_any_order_after_page_zero() {
        let mut a = FrameAssembler::default();
        let base = Instant::now();
        assert!(a.admit(frame("aa", "a", 0, 3), base).is_empty());
        assert!(a.admit(frame("aa", "c", 2, 3), base).is_empty());
        let out = a.admit(frame("aa", "b", 1, 3), base);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].frames, 3);
        assert_eq!(out[0].closed_by, ClosedBy::Complete);
        for f in ["a", "b", "c"] {
            assert!(out[0].reading.fields.contains_key(f), "{f}");
        }
    }

    #[test]
    fn a_page_that_cannot_belong_closes_the_report_and_is_kept() {
        let mut a = FrameAssembler::default();
        let base = Instant::now();
        a.admit(frame("aa", "a", 0, 3), base);
        a.admit(frame("aa", "b", 1, 3), base);
        // Page 1 again: a lost tail and a lost page 0 of the next report.
        let out = a.admit(frame("aa", "x", 1, 3), base);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].closed_by, ClosedBy::Mismatch);
        assert_eq!(out[0].frames, 2);
        // Another page count: same.
        let out = a.admit(frame("aa", "y", 1, 2), base);
        assert_eq!(out[0].closed_by, ClosedBy::Mismatch);
        assert!(out[0].reading.fields.contains_key("x"));
        let left = a.drain_all();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].closed_by, ClosedBy::Drain);
        assert!(left[0].reading.fields.contains_key("y"));
    }

    #[test]
    fn a_page_past_its_count_is_a_mismatch_and_never_completes_a_report() {
        let mut a = FrameAssembler::default();
        let base = Instant::now();
        a.admit(frame("aa", "a", 0, 2), base);
        // index 2 of 2 is out of range: it would make `seen` reach the count.
        let out = a.admit(frame("aa", "bad", 2, 2), base);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].closed_by, ClosedBy::Mismatch);
        assert_eq!(out[0].frames, 1);
        assert!(out[0].reading.fields.contains_key("bad"));
        assert!(out[0].reading.page.is_none());
        assert_eq!(a.pending_len(), 1, "the open report is untouched");
        // Without an open report it does not start one either.
        let out = a.admit(frame("bb", "bad", 7, 3), base);
        assert_eq!(out[0].closed_by, ClosedBy::Mismatch);
        assert_eq!(a.pending_len(), 1);
        // The real page 1 still completes the first report.
        let out = a.admit(frame("aa", "b", 1, 2), base);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].closed_by, ClosedBy::Complete);
        assert!(!out[0].reading.fields.contains_key("bad"));
    }

    #[test]
    fn page_count_one_or_zero_is_a_complete_report() {
        assert_eq!(TelemetryPage::of(0, 0), None);
        assert_eq!(TelemetryPage::of(0, 1), None);
        assert_eq!(
            TelemetryPage::of(0, 2),
            Some(TelemetryPage { index: 0, count: 2 })
        );
        let mut a = FrameAssembler::default();
        let out = a.admit(frame("aa", "a", 0, 1), Instant::now());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].closed_by, ClosedBy::Complete);
    }

    #[test]
    fn the_span_cap_publishes_a_partial() {
        let mut a = FrameAssembler::new(
            Duration::from_secs(20),
            Duration::from_secs(20),
            Duration::from_secs(30),
        );
        let base = Instant::now();
        a.admit(frame("aa", "a", 0, 4), base);
        a.admit(frame("aa", "b", 1, 4), base + Duration::from_secs(15));
        let out = a.admit(frame("aa", "c", 2, 4), base + Duration::from_secs(30));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].closed_by, ClosedBy::MaxSpan);
        assert_eq!(out[0].frames, 2);
        assert_eq!(a.pending_len(), 1, "page 2 opened a partial of its own");
    }

    #[test]
    fn devices_are_independent() {
        let mut a = FrameAssembler::default();
        let base = Instant::now();
        a.admit(frame("aa", "a", 0, 2), base);
        a.admit(frame("bb", "a", 0, 2), base);
        let out = a.admit(frame("aa", "b", 1, 2), base);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].reading.dev_eui, "aa");
        assert_eq!(a.pending_len(), 1);
    }
}
