//! The central's half of the gateway's reliable event delivery (link doc §4.1, `#470` C3).
//!
//! The Northbridge ACKs a Node's frame on air before the central has seen it, so it keeps the
//! two ring events, `EVT_TWR_UPLINK` and `EVT_TWR_TX`, in a ring until the central confirms
//! them with a cumulative `TWR_EVT_ACK {seq}`, and resends the unconfirmed ones go-back-N from
//! the oldest. This is the receiving side of that: which ring events to process, which are
//! resends to drop, and when to ack. It is pure — the caller hands it the time — so the pacing
//! and the `seq` arithmetic are tested without a clock or a link.
//!
//! - **In order only.** The ack is cumulative: acking `seq` tells the gateway every event up to
//!   it arrived. So an event is processed only when it is the one after the last processed;
//!   one further ahead (a frame before it was lost on the link) is dropped, and the gateway's
//!   next resend pass brings both again. A go-back-N receiver.
//! - **Dedup by `seq`.** One up to [`WINDOW`] − 1 behind the last processed (or equal to it) is
//!   a resend of an event already processed: dropped, and re-acked. This sits on top of the
//!   `(addr, counter)` dedup, which `EVT_TWR_TX` has no counter for.
//! - **Ack after persisting.** The caller marks an event [`Delivery::done`] only once its
//!   effects are stored; one whose handling failed is not, and the gateway resends it.
//! - **Pacing.** An ack goes out once [`ACK_EVERY`] ring events are owed one, or [`ACK_WITHIN`]
//!   after the first of them, whichever is first (link doc §4.1).
//! - **Learning where the gateway is.** A central that starts (or a gateway that rebooted) does
//!   not know the oldest unconfirmed event, and acking the first one it happens to see could
//!   confirm older ones it never got — a central opens the UART mid-resend-pass. So it starts
//!   [`Base::Learning`]: it **processes nothing** and holds every ring event that arrives
//!   ([`Admit::Hold`]), acks nothing, until a `seq` arrives a second time — the gateway began
//!   a new pass, so a whole one has been seen — or [`LEARN_FOR`] has passed (several of its
//!   resend passes). Then [`Delivery::settle`] picks the start: the gateway's ring is far
//!   smaller than the `seq` circle, so the held `seq`s form one ascending run mod 256 and the
//!   start is the one after the largest gap between them. The caller processes the held
//!   events in `seq` order from there. Processing in arrival order instead would hand the
//!   `(addr, counter)` rule a mid-pass start first and make it drop the older, unprocessed
//!   frames of the pass as replays (T3 on c66: 11 frames lost). After an `EVT_BOOT`, or with
//!   the gateway found in its `boot` state, there is nothing to learn: a rebooted gateway's
//!   first ring event is seq 1 ([`Delivery::booted`]).
//! - **Counting what a relearn skips.** A gap relearn gives up on the events between the last
//!   processed and the start it learns: the gateway no longer has them. Each such `seq` is
//!   counted in [`DeliveryStats::evt_seq_gap`], so a loss is never silent.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// How far behind the last processed event a `seq` is still a resend (link doc §4.1).
pub const WINDOW: u8 = 128;

/// Ring events owed an ack that send one at once (link doc §4.1).
pub const ACK_EVERY: u32 = 8;

/// The longest an owed ack waits (link doc §4.1).
pub const ACK_WITHIN: Duration = Duration::from_millis(100);

/// How long a fresh receiver watches before it trusts the oldest `seq` it has seen: the gateway
/// starts a resend pass at most every 300 ms (link doc §4.1), so this spans three of them.
pub const LEARN_FOR: Duration = Duration::from_millis(1000);

/// How long an event ahead of the next expected one may keep arriving before the receiver
/// gives up on the gap and learns afresh. A gap from a frame lost on the link closes with the
/// next resend pass (≤ 300 ms); one that does not is a gateway that restarted its numbering —
/// rebooted with its `EVT_BOOT` lost.
pub const GAP_RELEARN_AFTER: Duration = Duration::from_millis(2000);

/// What to do with one ring event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// Process it, then call [`Delivery::done`] once its effects are stored.
    Process,
    /// Already processed: drop it. The ack it is owed goes out with the pacing.
    Duplicate,
    /// Ahead of the next expected one: drop it, the gateway sends both again.
    Gap,
    /// The start is being learned: hold it, unprocessed, until [`Delivery::settle`] says where
    /// the run starts, then process the held events in `seq` order.
    Hold,
}

/// The receiver's cumulative counters, for the northbridge document.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeliveryStats {
    /// Gap relearns: an event ahead of the next expected kept arriving for
    /// [`GAP_RELEARN_AFTER`] while the expected one did not.
    pub evt_relearn: u64,
    /// The `seq`s those relearns skipped: from the one after the last processed up to the
    /// start the relearn settled on. Events the gateway no longer holds, so lost — or, after a
    /// gateway reboot the central did not see, a numbering that restarted (an upper bound).
    pub evt_seq_gap: u64,
    /// Of [`Self::evt_seq_gap`], the `seq`s that did arrive, were dropped as
    /// [`Admit::Gap`] and were never processed.
    pub evt_seq_gap_seen: u64,
}

/// Where a gap relearn started from: the last processed event, and the `seq`s ahead of it
/// dropped as [`Admit::Gap`] since.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GapRelearn {
    last: u8,
    dropped: BTreeSet<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Base {
    /// The start is not known yet (§ module docs): every `seq` held, when the first arrived,
    /// whether one arrived twice (a whole pass seen), and — after a gap — where the receiver
    /// was before.
    Learning {
        first_at: Option<Instant>,
        arrived: BTreeSet<u8>,
        repeated: bool,
        after_gap: Option<GapRelearn>,
    },
    /// The last ring event processed, since when the events arriving have been ahead of it
    /// while it did not come again, and the `seq`s dropped as ahead of it.
    Known {
        last: u8,
        gap_since: Option<Instant>,
        dropped: BTreeSet<u8>,
    },
}

impl Base {
    fn learning() -> Self {
        Base::Learning {
            first_at: None,
            arrived: BTreeSet::new(),
            repeated: false,
            after_gap: None,
        }
    }

    fn known(last: u8) -> Self {
        Base::Known {
            last,
            gap_since: None,
            dropped: BTreeSet::new(),
        }
    }
}

/// The receiving side of the event ring. One per central; [`Delivery::restart`] on every
/// `EVT_BOOT`, since a rebooted gateway numbers its ring afresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    base: Base,
    /// Ring events since the last ack sent, processed or dropped: a dropped one is a resend,
    /// which means the gateway has not had the ack yet.
    owed: u32,
    owed_since: Option<Instant>,
    /// Whether the gateway keeps a ring at all (its `GET_INFO` `evt_ring_max` > 0). Without
    /// one its event `seq` counts every event, best-effort ones too, so it means nothing to a
    /// receiver; every event is processed and nothing is acked.
    enabled: bool,
    stats: DeliveryStats,
}

impl Default for Delivery {
    fn default() -> Self {
        Delivery {
            base: Base::learning(),
            owed: 0,
            owed_since: None,
            enabled: true,
            stats: DeliveryStats::default(),
        }
    }
}

/// The start of the run the `seq`s in `held` form on the circle: the one after the largest gap
/// between neighbours. The ring is far smaller than the circle, so that gap is the one between
/// the newest and the oldest.
fn run_start(held: &BTreeSet<u8>) -> Option<u8> {
    let v: Vec<u8> = held.iter().copied().collect();
    let n = v.len();
    (0..n)
        .max_by_key(|&i| {
            let next = v[(i + 1) % n];
            // From `v[i]` up to the next one; a lone `seq` is a gap of the whole circle.
            let gap = u16::from(next.wrapping_sub(v[i]));
            (if gap == 0 { 256 } else { gap }, std::cmp::Reverse(i))
        })
        .map(|i| v[(i + 1) % n])
}

/// `a - b` as a signed distance on the `u8` circle, in `-128..=127`.
fn offset(a: u8, b: u8) -> i8 {
    a.wrapping_sub(b) as i8
}

impl Delivery {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget where the gateway is: it rebooted, or this is a new central.
    pub fn restart(&mut self) {
        self.base = Base::learning();
        self.owed = 0;
        self.owed_since = None;
    }

    /// The gateway sent `EVT_BOOT`: it numbers its ring afresh, the first ring event is seq 1
    /// (link doc §4.1), so nothing needs learning.
    pub fn booted(&mut self) {
        self.base = Base::known(0);
        self.owed = 0;
        self.owed_since = None;
    }

    /// From a `GET_INFO`: whether the gateway keeps an event ring.
    pub fn set_ring(&mut self, ring: bool) {
        if ring != self.enabled {
            self.enabled = ring;
            self.restart();
        }
    }

    pub fn ring(&self) -> bool {
        self.enabled
    }

    /// The cumulative counters of this receiver.
    pub fn stats(&self) -> DeliveryStats {
        self.stats
    }

    /// The last ring event processed, once the start is known.
    pub fn last(&self) -> Option<u8> {
        match self.base {
            Base::Known { last, .. } => Some(last),
            Base::Learning { .. } => None,
        }
    }

    fn owe(&mut self, now: Instant) {
        self.owed = self.owed.saturating_add(1);
        self.owed_since.get_or_insert(now);
    }

    /// Decide what to do with the ring event `seq` arriving at `now`.
    pub fn admit(&mut self, seq: u8, now: Instant) -> Admit {
        if !self.enabled {
            return Admit::Process;
        }
        match &mut self.base {
            Base::Learning {
                first_at,
                arrived,
                repeated,
                ..
            } => {
                first_at.get_or_insert(now);
                if !arrived.insert(seq) {
                    *repeated = true;
                }
                Admit::Hold
            }
            Base::Known {
                last,
                gap_since,
                dropped,
            } => {
                let last = *last;
                if seq == last.wrapping_add(1) {
                    // The expected one is still coming: a gap behind it is not a renumbering,
                    // only a slow resend or an event whose handling failed.
                    *gap_since = None;
                    return Admit::Process;
                }
                if last.wrapping_sub(seq) < WINDOW {
                    self.owe(now);
                    return Admit::Duplicate;
                }
                let since = *gap_since.get_or_insert(now);
                if now.saturating_duration_since(since) >= GAP_RELEARN_AFTER {
                    // Learned afresh, from this event on. What it skips is counted when the
                    // learning settles on a start.
                    let from = GapRelearn {
                        last,
                        dropped: std::mem::take(dropped),
                    };
                    self.stats.evt_relearn = self.stats.evt_relearn.saturating_add(1);
                    self.restart();
                    if let Base::Learning { after_gap, .. } = &mut self.base {
                        *after_gap = Some(from);
                    }
                    return self.admit(seq, now);
                }
                dropped.insert(seq);
                self.owe(now);
                Admit::Gap
            }
        }
    }

    /// The ring event `seq`, admitted with [`Admit::Process`], is processed and its effects
    /// are stored.
    pub fn done(&mut self, seq: u8, now: Instant) {
        if !self.enabled {
            return;
        }
        match &mut self.base {
            // Nothing is processed while learning.
            Base::Learning { .. } => {}
            Base::Known {
                last,
                gap_since,
                dropped,
            } => {
                *last = seq;
                *gap_since = None;
                // Only those still ahead are still a gap.
                dropped.retain(|s| offset(*s, seq) > 0);
                self.owe(now);
            }
        }
    }

    /// End learning once a `seq` arrived twice or [`LEARN_FOR`] has passed since the first
    /// event: the start is the held `seq` after the largest gap between them on the circle.
    /// Returns it — the caller then processes what it holds in `seq` order from there — or
    /// `None` while still learning (or not learning at all). Nothing is owed: the first ack
    /// follows the first event processed from the start.
    pub fn settle(&mut self, now: Instant) -> Option<u8> {
        if !self.enabled {
            return None;
        }
        let Base::Learning {
            first_at: Some(first_at),
            arrived,
            repeated,
            after_gap,
        } = &self.base
        else {
            return None;
        };
        if !*repeated && now.saturating_duration_since(*first_at) < LEARN_FOR {
            return None;
        }
        let start = run_start(arrived)?;
        if let Some(from) = after_gap.clone() {
            self.count_skipped(&from, start);
        }
        self.base = Base::known(start.wrapping_sub(1));
        self.owed = 0;
        self.owed_since = None;
        Some(start)
    }

    /// A gap relearn, or the gateway's own head moving past the next expected, skips the
    /// events from the one after the last processed up to `start`: counted. A `start` at or
    /// behind that is a resend pass that caught up: nothing skipped.
    fn count_skipped(&mut self, from: &GapRelearn, start: u8) {
        let expected = from.last.wrapping_add(1);
        let skipped = offset(start, expected);
        if skipped > 0 {
            let seen_dropped = from
                .dropped
                .iter()
                .filter(|s| offset(**s, expected) >= 0 && offset(**s, start) < 0)
                .count();
            self.stats.evt_seq_gap = self.stats.evt_seq_gap.saturating_add(skipped as u64);
            self.stats.evt_seq_gap_seen = self
                .stats
                .evt_seq_gap_seen
                .saturating_add(seen_dropped as u64);
        }
    }

    /// The gateway reported its ring's head (`GET_INFO` `evt_head_seq`, Northbridge ≥ 0.3.1
    /// final): the `seq` the next event it delivers carries. No learning is needed.
    ///
    /// - Learning: the start is `head`.
    /// - Known, `head` at or behind the next expected: events this receiver processed and has
    ///   not had acked yet — they stay processed, and their resends are duplicates.
    /// - Known, `head` ahead of the next expected: the gateway no longer holds the ones
    ///   between; they are skipped and counted, and `head` is next.
    pub fn expect(&mut self, head: u8) {
        if !self.enabled {
            return;
        }
        match &self.base {
            Base::Learning { after_gap, .. } => {
                if let Some(from) = after_gap.clone() {
                    self.count_skipped(&from, head);
                }
                self.base = Base::known(head.wrapping_sub(1));
                self.owed = 0;
                self.owed_since = None;
            }
            Base::Known { last, dropped, .. } => {
                if offset(head, last.wrapping_add(1)) <= 0 {
                    return;
                }
                let from = GapRelearn {
                    last: *last,
                    dropped: dropped.clone(),
                };
                self.count_skipped(&from, head);
                self.base = Base::known(head.wrapping_sub(1));
            }
        }
    }

    /// Whether the start of the ring is being learned.
    pub fn learning(&self) -> bool {
        self.enabled && matches!(self.base, Base::Learning { .. })
    }

    /// The ack to send at `now`, if one is due. Call [`Delivery::sent`] once it is written.
    pub fn due(&mut self, now: Instant) -> Option<u8> {
        if !self.enabled {
            return None;
        }
        let Base::Known { last, .. } = self.base else {
            return None;
        };
        let since = self.owed_since?;
        (self.owed >= ACK_EVERY || now.saturating_duration_since(since) >= ACK_WITHIN)
            .then_some(last)
    }

    /// Any ack owed at all, regardless of the pacing: what a central that is stopping sends,
    /// so its successor finds fewer resends.
    pub fn owed(&mut self, _now: Instant) -> Option<u8> {
        if !self.enabled {
            return None;
        }
        match self.base {
            Base::Known { last, .. } if self.owed > 0 => Some(last),
            _ => None,
        }
    }

    /// The ack just went out.
    pub fn sent(&mut self) {
        self.owed = 0;
        self.owed_since = None;
    }

    /// When [`Delivery::due`] next has something to say, for the central's timer: the pacing
    /// deadline of an owed ack, or the end of learning.
    pub fn deadline(&self) -> Option<Instant> {
        if !self.enabled {
            return None;
        }
        match &self.base {
            // A whole pass seen: settle at once.
            Base::Learning {
                first_at, repeated, ..
            } => first_at.map(|t| if *repeated { t } else { t + LEARN_FOR }),
            Base::Known { .. } => self.owed_since.map(|t| t + ACK_WITHIN),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A receiver that has learned `last` as its last processed event, with nothing owed.
    fn known(last: u8) -> Delivery {
        let mut d = Delivery::new();
        d.base = Base::known(last);
        d
    }

    #[test]
    fn in_order_events_are_processed_and_acked_every_eight() {
        let t = Instant::now();
        let mut d = known(9);
        for seq in 10..17 {
            assert_eq!(d.admit(seq, t), Admit::Process);
            d.done(seq, t);
            assert_eq!(d.due(t), None, "{seq}: seven owed, inside 100 ms");
        }
        assert_eq!(d.admit(17, t), Admit::Process);
        d.done(17, t);
        assert_eq!(d.due(t), Some(17), "the eighth sends it");
        d.sent();
        assert_eq!(d.due(t + ms(500)), None, "nothing owed");
        assert_eq!(d.deadline(), None);
    }

    #[test]
    fn an_owed_ack_goes_out_100_ms_after_the_first_event() {
        let t = Instant::now();
        let mut d = known(0);
        d.admit(1, t);
        d.done(1, t);
        d.admit(2, t + ms(40));
        d.done(2, t + ms(40));
        assert_eq!(d.deadline(), Some(t + ACK_WITHIN));
        assert_eq!(d.due(t + ms(99)), None);
        assert_eq!(d.due(t + ms(100)), Some(2));
    }

    #[test]
    fn a_resend_is_a_duplicate_within_the_window_and_is_re_acked() {
        let t = Instant::now();
        let mut d = known(200);
        assert_eq!(d.admit(200, t), Admit::Duplicate);
        assert_eq!(d.admit(73, t), Admit::Duplicate, "127 behind");
        assert_eq!(
            d.admit(72, t),
            Admit::Gap,
            "128 behind is outside the window"
        );
        assert_eq!(d.last(), Some(200), "a duplicate moves nothing");
        assert_eq!(
            d.due(t + ACK_WITHIN),
            Some(200),
            "the gateway is told again"
        );
    }

    #[test]
    fn the_seq_wraps_from_255_to_0() {
        let t = Instant::now();
        let mut d = known(254);
        for seq in [255, 0, 1] {
            assert_eq!(d.admit(seq, t), Admit::Process, "{seq}");
            d.done(seq, t);
        }
        assert_eq!(d.last(), Some(1));
        assert_eq!(d.admit(255, t), Admit::Duplicate, "behind across the wrap");
        assert_eq!(d.admit(130, t), Admit::Duplicate, "127 behind 1");
        assert_eq!(d.admit(129, t), Admit::Gap, "128 behind 1");
        assert_eq!(d.admit(3, t), Admit::Gap, "one missing ahead");
        assert_eq!(d.due(t + ACK_WITHIN), Some(1));
    }

    #[test]
    fn a_gap_is_dropped_until_the_resend_fills_it() {
        let t = Instant::now();
        let mut d = known(10);
        // 11 was lost on the link.
        assert_eq!(d.admit(12, t), Admit::Gap);
        assert_eq!(d.admit(13, t), Admit::Gap);
        assert_eq!(d.due(t + ACK_WITHIN), Some(10), "the ack stays at 10");
        d.sent();
        // The resend pass: 11, 12, 13.
        for seq in 11..14 {
            assert_eq!(d.admit(seq, t + ms(300)), Admit::Process);
            d.done(seq, t + ms(300));
        }
        assert_eq!(d.last(), Some(13));
        assert_eq!(d.admit(14, t + ms(5000)), Admit::Process, "gap cleared");
    }

    #[test]
    fn a_gap_that_never_closes_is_learned_afresh() {
        let t = Instant::now();
        let mut d = known(200);
        // A gateway that rebooted unseen numbers from 0 again.
        assert_eq!(d.admit(3, t), Admit::Gap);
        assert_eq!(d.admit(3, t + ms(1999)), Admit::Gap);
        assert_eq!(d.admit(3, t + GAP_RELEARN_AFTER), Admit::Hold);
        assert_eq!(d.last(), None, "learning");
        assert_eq!(d.settle(t + GAP_RELEARN_AFTER + ms(999)), None);
        assert_eq!(d.settle(t + GAP_RELEARN_AFTER + LEARN_FOR), Some(3));
        assert_eq!(
            d.admit(3, t + GAP_RELEARN_AFTER + LEARN_FOR),
            Admit::Process
        );
    }

    /// A gateway whose ring overflowed while the link was down: 11..=14 are gone, 15 onwards
    /// keep coming. The relearn starts at 15 and counts the four it skipped; 13, which arrived
    /// once and was dropped as ahead, is one of them.
    #[test]
    fn a_relearn_counts_every_seq_it_skips() {
        let t = Instant::now();
        let mut d = known(10);
        assert_eq!(d.admit(13, t), Admit::Gap);
        for at in [300, 600, 1200, 1800] {
            for seq in 15..18 {
                assert_eq!(d.admit(seq, t + ms(at)), Admit::Gap);
            }
        }
        assert_eq!(d.stats(), DeliveryStats::default(), "nothing skipped yet");
        let at = t + GAP_RELEARN_AFTER;
        for seq in 15..18 {
            assert_eq!(d.admit(seq, at), Admit::Hold, "{seq}");
        }
        assert_eq!(d.settle(at + LEARN_FOR), Some(15));
        assert_eq!(
            d.stats(),
            DeliveryStats {
                evt_relearn: 1,
                evt_seq_gap: 4,
                evt_seq_gap_seen: 1,
            }
        );
    }

    /// A relearn whose learning sees the gateway resend from the one expected skipped nothing.
    #[test]
    fn a_relearn_that_catches_up_skips_nothing() {
        let t = Instant::now();
        let mut d = known(250);
        assert_eq!(d.admit(253, t), Admit::Gap);
        let at = t + GAP_RELEARN_AFTER;
        assert_eq!(d.admit(253, at), Admit::Hold);
        for seq in [251, 252] {
            assert_eq!(d.admit(seq, at + ms(300)), Admit::Hold);
        }
        assert_eq!(d.settle(at + LEARN_FOR), Some(251));
        assert_eq!(
            d.stats(),
            DeliveryStats {
                evt_relearn: 1,
                evt_seq_gap: 0,
                evt_seq_gap_seen: 0,
            }
        );
    }

    /// The expected event arriving again (its handling failed, or the outbox is full) shows
    /// the gateway still numbers as before: no relearn, however long the gap behind it lasts.
    #[test]
    fn a_gap_behind_an_expected_event_that_keeps_coming_is_not_relearned() {
        let t = Instant::now();
        let mut d = known(5);
        for at in (0..5000).step_by(300) {
            assert_eq!(d.admit(6, t + ms(at)), Admit::Process, "failed again");
            assert_eq!(d.admit(7, t + ms(at + 10)), Admit::Gap);
        }
        assert_eq!(d.last(), Some(5));
        assert_eq!(d.stats().evt_relearn, 0);
    }

    /// A central opens the link mid-pass: it sees 6, 7, then the next pass 4, 5, 6. It
    /// processes nothing until 6 comes again — a whole pass seen — and then starts at 4, the
    /// oldest.
    #[test]
    fn a_fresh_receiver_starts_at_the_oldest_once_a_pass_repeats() {
        let t = Instant::now();
        let mut d = Delivery::new();
        for (seq, at) in [(6, 0), (7, 10), (4, 300), (5, 310)] {
            assert_eq!(d.admit(seq, t + ms(at)), Admit::Hold);
        }
        assert_eq!(d.due(t + ms(400)), None, "nothing acked while learning");
        assert_eq!(d.settle(t + ms(400)), None, "no pass repeated yet");
        assert_eq!(d.deadline(), Some(t + LEARN_FOR));
        assert_eq!(d.admit(6, t + ms(320)), Admit::Hold);
        assert_eq!(d.deadline(), Some(t), "a pass repeated: settle now");
        assert_eq!(d.settle(t + ms(320)), Some(4));
        for seq in 4..=7 {
            assert_eq!(d.admit(seq, t + ms(330)), Admit::Process, "{seq}");
            d.done(seq, t + ms(330));
        }
        assert_eq!(d.owed(t + ms(330)), Some(7));
    }

    /// T3 (c66): a central started mid resend pass of a full ring of 32 (seqs 0..=31) sees
    /// 12..=31 and then the pass wrap to 0..=11. The start is 0, not 12.
    #[test]
    fn a_wrapped_pass_starts_after_the_largest_gap() {
        let t = Instant::now();
        let mut d = Delivery::new();
        for seq in (12..=31).chain(0..=11) {
            assert_eq!(d.admit(seq, t), Admit::Hold);
        }
        assert_eq!(d.settle(t + LEARN_FOR), Some(0));
        for seq in 0..=31 {
            assert_eq!(d.admit(seq, t + LEARN_FOR), Admit::Process, "{seq}");
            d.done(seq, t + LEARN_FOR);
        }
        // The same across the 255 → 0 wrap of the seq itself.
        let mut d = Delivery::new();
        for seq in (250..=255).chain(0..=15).chain(240..=250) {
            d.admit(seq, t);
        }
        assert_eq!(d.settle(t), Some(240), "250 repeated: settled at once");
    }

    #[test]
    fn a_fresh_receiver_acks_only_the_consecutive_run() {
        let t = Instant::now();
        let mut d = Delivery::new();
        for seq in [254, 255, 1] {
            assert_eq!(d.admit(seq, t), Admit::Hold);
        }
        assert_eq!(d.settle(t + LEARN_FOR), Some(254));
        for seq in [254, 255] {
            assert_eq!(d.admit(seq, t + LEARN_FOR), Admit::Process);
            d.done(seq, t + LEARN_FOR);
        }
        // 0 never came: 1 waits for it.
        assert_eq!(d.admit(1, t + LEARN_FOR), Admit::Gap);
        assert_eq!(d.owed(t + LEARN_FOR), Some(255));
    }

    #[test]
    fn a_failed_event_is_not_acked_and_is_processed_again() {
        let t = Instant::now();
        let mut d = known(5);
        assert_eq!(d.admit(6, t), Admit::Process);
        // Its handling failed: no `done`.
        assert_eq!(d.admit(7, t), Admit::Gap, "7 waits for 6");
        assert_eq!(d.admit(6, t + ms(300)), Admit::Process, "the resend");
    }

    /// The first held event failing does not let anything past it be acked.
    #[test]
    fn a_fresh_receiver_does_not_ack_past_a_failed_oldest_event() {
        let t = Instant::now();
        let mut d = Delivery::new();
        for seq in [10, 11, 12] {
            assert_eq!(d.admit(seq, t), Admit::Hold);
        }
        assert_eq!(d.settle(t + LEARN_FOR), Some(10));
        assert_eq!(d.admit(10, t + LEARN_FOR), Admit::Process);
        // 10 failed: no `done`.
        assert_eq!(
            d.admit(11, t + LEARN_FOR),
            Admit::Gap,
            "11 and 12 wait for 10"
        );
        assert_eq!(d.owed(t + LEARN_FOR), Some(9), "nothing past the start");
        assert_eq!(d.admit(10, t + LEARN_FOR + ms(300)), Admit::Process);
        d.done(10, t + LEARN_FOR + ms(300));
        assert_eq!(d.owed(t + LEARN_FOR + ms(300)), Some(10));
    }

    /// Northbridge 0.3.1 final reports its head: no learning, and a head ahead of the next
    /// expected skips (and counts) what is between; one behind it changes nothing.
    #[test]
    fn the_gateways_head_is_the_start_without_learning() {
        let t = Instant::now();
        let mut d = Delivery::new();
        d.expect(12);
        assert_eq!(d.last(), Some(11));
        assert_eq!(d.admit(13, t), Admit::Gap, "12 first");
        assert_eq!(d.admit(12, t), Admit::Process);
        d.done(12, t);
        // Processed, not acked yet: the head is still 12.
        d.expect(12);
        assert_eq!(d.last(), Some(12));
        assert_eq!(d.admit(12, t), Admit::Duplicate);
        // The gateway dropped 13..=14.
        d.expect(15);
        assert_eq!(d.last(), Some(14));
        assert_eq!(d.stats().evt_seq_gap, 2);
        // A gap relearn ends at the head at once.
        let mut d = known(10);
        for at in [0, 2000] {
            d.admit(20, t + ms(at));
        }
        assert!(d.learning());
        d.expect(20);
        assert_eq!(d.last(), Some(19));
        assert_eq!(d.stats().evt_seq_gap, 9);
        assert_eq!(d.admit(20, t + ms(2000)), Admit::Process);
    }

    #[test]
    fn after_a_boot_the_first_ring_event_is_seq_1() {
        let t = Instant::now();
        let mut d = known(200);
        d.booted();
        assert_eq!(d.admit(2, t), Admit::Gap, "1 first");
        assert_eq!(d.admit(1, t), Admit::Process);
        d.done(1, t);
        assert_eq!(d.due(t + ACK_WITHIN), Some(1), "no learning");
    }

    #[test]
    fn a_gateway_without_a_ring_gets_no_acks_and_no_dedup() {
        let t = Instant::now();
        let mut d = Delivery::new();
        d.set_ring(false);
        for seq in [1, 1, 9] {
            assert_eq!(d.admit(seq, t), Admit::Process);
            d.done(seq, t);
        }
        assert_eq!(d.due(t + LEARN_FOR + ACK_WITHIN), None);
        assert_eq!(d.owed(t + LEARN_FOR), None);
        assert_eq!(d.deadline(), None);
    }

    #[test]
    fn a_restart_forgets_the_gateway() {
        let t = Instant::now();
        let mut d = known(40);
        d.admit(41, t);
        d.done(41, t);
        d.restart();
        assert_eq!(d.last(), None);
        assert_eq!(d.owed(t), None);
        assert_eq!(d.admit(41, t), Admit::Hold, "a new numbering, learned");
    }

    #[test]
    fn a_stopping_central_sends_what_it_owes() {
        let t = Instant::now();
        let mut d = known(3);
        assert_eq!(d.owed(t), None);
        d.admit(4, t);
        d.done(4, t);
        assert_eq!(d.due(t), None, "paced");
        assert_eq!(d.owed(t), Some(4), "but owed");
    }
}
