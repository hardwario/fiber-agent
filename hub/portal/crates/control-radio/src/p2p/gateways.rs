//! The central's gateways and the Nodes' homes — `plan/control/radio/p2p_cluster_gateway.md`
//! §4, §5.
//!
//! The central drives this Hub's own Northbridge ([`LOCAL`]) and the Northbridge of every
//! Client Hub on the gateway plane. Every gateway holds every live peer; only a Node's
//! **home** holds it with `HOME`, ACKs it and sends its downlinks. This module is the pure
//! half: what the central believes each gateway holds ([`GwPeer`]), and the signal
//! statistics a home is chosen and moved by ([`Signals`]). The runtime applies them.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use crate::p2p::session::Session;

/// This Hub's own Northbridge, as a gateway name. A Client Hub is named by its hostname, which
/// is never this word (`proximos-<serial>`).
pub const LOCAL: &str = "local";

/// The first copy of a JoinReq waits this long for the copies of other gateways (§7).
pub const JOIN_WINDOW: Duration = Duration::from_millis(200);

/// The weight of a new sample in a gateway's signal average (§5.1).
const EWMA_ALPHA: f64 = 0.3;
/// How much better, in dB, another gateway must hear a Node to become its home.
const BETTER_BY_DB: f64 = 6.0;
/// How many of the last [`WINDOW`] counters the better gateway must have heard.
const BETTER_HEARD: usize = 3;
/// The counters a Node's reporting gateways are remembered for.
const WINDOW: usize = 8;
/// Consecutive counters the home did not report before the home moves.
const HOME_MISSED: u32 = 3;
/// How recently a gateway must have heard a Node to take it over from a home gone offline.
pub const OFFLINE_HEARD_WITHIN: Duration = Duration::from_secs(3600);

/// A Node's home as a gateway name: [`LOCAL`] when the session names none.
pub fn home_of(s: &Session) -> &str {
    s.home.as_deref().unwrap_or(LOCAL)
}

/// The session's `home` for a gateway name: `None` for [`LOCAL`].
pub fn home_tag(gw: &str) -> Option<String> {
    (gw != LOCAL).then(|| gw.to_string())
}

/// A peer as the central put it on one gateway: the Node, the key it holds, and whether it
/// holds it as the home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GwPeer {
    pub dev_eui: String,
    /// The session key, hex, as handed over — a rejoin changes it and the peer is re-added.
    pub key: String,
    pub home: bool,
}

impl GwPeer {
    /// Whether the gateway holds `s` as it should, with `home` saying whether it is its home.
    pub fn matches(&self, s: &Session, home: bool) -> bool {
        self.dev_eui == s.dev_eui && self.key == s.session_key && self.home == home
    }
}

/// The quality a home is chosen by: RSSI, less a negative SNR (§5.1).
pub fn signal_q(rssi_dbm: i16, snr_q_db: i8) -> f64 {
    f64::from(rssi_dbm) + (f64::from(snr_q_db) / 4.0).min(0.0)
}

#[derive(Debug, Clone, Copy)]
struct GwSignal {
    ewma_q: f64,
    last_heard: Instant,
}

#[derive(Debug, Default)]
struct NodeSignal {
    per_gw: BTreeMap<String, GwSignal>,
    /// The newest counter and the gateways that reported it so far.
    open: Option<(u32, BTreeSet<String>)>,
    /// The gateways that reported each of the last closed counters, newest last.
    closed: VecDeque<BTreeSet<String>>,
    /// Consecutive closed counters the home did not report.
    home_missed: u32,
}

/// Why a home moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveReason {
    /// Another gateway hears the Node better (§5.1).
    Better,
    /// The home missed the Node's last counters.
    HomeNotHearing,
    /// The home went offline.
    HomeOffline,
}

impl MoveReason {
    pub fn as_str(self) -> &'static str {
        match self {
            MoveReason::Better => "another gateway hears it better",
            MoveReason::HomeNotHearing => "the home missed its last counters",
            MoveReason::HomeOffline => "the home went offline",
        }
    }
}

/// Per Node and gateway: how well and how recently it is heard (§5.1). In memory: a restarted
/// central starts without statistics, and the homes stand until they are earned again.
#[derive(Debug, Default)]
pub struct Signals {
    nodes: BTreeMap<u32, NodeSignal>,
}

impl Signals {
    /// One `EVT_TWR_UPLINK` from `gw`, before the `(addr, counter)` dedup. Returns whether it
    /// closed the Node's previous counter, which is when its home is reviewed.
    pub fn observe(&mut self, addr: u32, counter: u32, gw: &str, q: f64, now: Instant) -> bool {
        let n = self.nodes.entry(addr).or_default();
        let sig = n.per_gw.entry(gw.to_string()).or_insert(GwSignal {
            ewma_q: q,
            last_heard: now,
        });
        sig.ewma_q = EWMA_ALPHA * q + (1.0 - EWMA_ALPHA) * sig.ewma_q;
        sig.last_heard = now;
        match &mut n.open {
            Some((c, gws)) if *c == counter => {
                gws.insert(gw.to_string());
                false
            }
            // A ring replay of an older counter: its signal counts, its set is closed.
            Some((c, _)) if counter < *c => false,
            open => {
                let closed = open.take().map(|(_, gws)| gws);
                *open = Some((counter, BTreeSet::from([gw.to_string()])));
                match closed {
                    Some(gws) => {
                        n.closed.push_back(gws);
                        while n.closed.len() > WINDOW {
                            n.closed.pop_front();
                        }
                        true
                    }
                    None => false,
                }
            }
        }
    }

    /// The home `addr` should have now, if not `home`: after a closed counter (§5.1), among the
    /// `online` gateways. Counts the home's misses as it goes, so call it once per closed
    /// counter.
    pub fn review(
        &mut self,
        addr: u32,
        home: &str,
        online: &BTreeSet<String>,
    ) -> Option<(String, MoveReason)> {
        let n = self.nodes.get_mut(&addr)?;
        if n.closed.back()?.contains(home) {
            n.home_missed = 0;
        } else {
            n.home_missed = n.home_missed.saturating_add(1);
        }
        let (better, takeover) = {
            let n = &*n;
            let home_q = n.per_gw.get(home).map(|s| s.ewma_q);
            let heard = |g: &str| n.closed.iter().filter(|s| s.contains(g)).count();
            let best_of = |it: &mut dyn Iterator<Item = &String>| {
                it.filter_map(|g| n.per_gw.get(g).map(|s| (g.clone(), s.ewma_q)))
                    .max_by(|a, b| a.1.total_cmp(&b.1))
            };
            let last = n.closed.back();
            // A challenger must have heard the counter just closed: a gateway gone silent
            // keeps its last average and its window entries.
            let better = best_of(&mut online.iter().filter(|g| {
                g.as_str() != home
                    && last.is_some_and(|l| l.contains(g.as_str()))
                    && heard(g) >= BETTER_HEARD
                    && n.per_gw
                        .get(g.as_str())
                        .is_some_and(|s| home_q.is_none_or(|h| s.ewma_q >= h + BETTER_BY_DB))
            }));
            let takeover = (n.home_missed >= HOME_MISSED)
                .then(|| {
                    best_of(&mut online.iter().filter(|g| {
                        g.as_str() != home && last.is_some_and(|l| l.contains(g.as_str()))
                    }))
                })
                .flatten();
            (better, takeover)
        };
        if let Some((g, _)) = better {
            n.home_missed = 0;
            return Some((g, MoveReason::Better));
        }
        if let Some((g, _)) = takeover {
            n.home_missed = 0;
            return Some((g, MoveReason::HomeNotHearing));
        }
        None
    }

    /// Where a Node whose home `gone` went offline moves: the best `online` gateway that heard
    /// it within [`OFFLINE_HEARD_WITHIN`], or nowhere (it keeps its home).
    pub fn take_over(
        &self,
        addr: u32,
        gone: &str,
        online: &BTreeSet<String>,
        now: Instant,
    ) -> Option<String> {
        let n = self.nodes.get(&addr)?;
        online
            .iter()
            .filter(|g| g.as_str() != gone)
            .filter_map(|g| n.per_gw.get(g).map(|s| (g, *s)))
            .filter(|(_, s)| now.saturating_duration_since(s.last_heard) <= OFFLINE_HEARD_WITHIN)
            .max_by(|a, b| a.1.ewma_q.total_cmp(&b.1.ewma_q))
            .map(|(g, _)| g.clone())
    }

    /// A Node rejoined or left: its statistics start over.
    pub fn forget(&mut self, addr: u32) {
        self.nodes.remove(&addr);
    }

    /// Whether `gw` heard `addr` at all since the statistics started.
    pub fn heard_by(&self, addr: u32, gw: &str) -> bool {
        self.nodes
            .get(&addr)
            .is_some_and(|n| n.per_gw.contains_key(gw))
    }
}

/// The copies of one JoinReq while its collection window is open (§7).
#[derive(Debug)]
pub struct JoinCopies<E> {
    pub deadline: Instant,
    copies: Vec<(String, f64, E)>,
}

impl<E> JoinCopies<E> {
    pub fn open(now: Instant, gw: String, q: f64, ev: E) -> Self {
        JoinCopies {
            deadline: now + JOIN_WINDOW,
            copies: vec![(gw, q, ev)],
        }
    }

    pub fn add(&mut self, gw: String, q: f64, ev: E) {
        self.copies.push((gw, q, ev));
    }

    /// The copy heard best; the first of equals.
    pub fn best(self) -> Option<(String, E)> {
        let mut best: Option<(String, f64, E)> = None;
        for c in self.copies {
            if best.as_ref().is_none_or(|b| c.1 > b.1) {
                best = Some(c);
            }
        }
        best.map(|(g, _, e)| (g, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn online(gws: &[&str]) -> BTreeSet<String> {
        gws.iter().map(|g| g.to_string()).collect()
    }

    /// Feed counters `from..=to`, each heard by the `(gw, q)` listed.
    fn feed(
        s: &mut Signals,
        addr: u32,
        counters: std::ops::RangeInclusive<u32>,
        by: &[(&str, f64)],
    ) {
        let now = Instant::now();
        for c in counters {
            for (gw, q) in by {
                s.observe(addr, c, gw, *q, now);
            }
        }
    }

    #[test]
    fn q_takes_a_negative_snr_off_the_rssi_only() {
        assert_eq!(signal_q(-100, -20), -105.0);
        assert_eq!(signal_q(-80, 24), -80.0);
    }

    #[test]
    fn a_counter_closes_when_a_newer_one_arrives() {
        let mut s = Signals::default();
        let now = Instant::now();
        assert!(
            !s.observe(1, 10, LOCAL, -90.0, now),
            "the first counter opens"
        );
        assert!(
            !s.observe(1, 10, "gw-b", -80.0, now),
            "a second copy joins it"
        );
        assert!(
            !s.observe(1, 9, "gw-b", -80.0, now),
            "an older replay closes nothing"
        );
        assert!(s.observe(1, 11, LOCAL, -90.0, now), "a newer one closes 10");
    }

    #[test]
    fn a_gateway_6_db_better_on_3_of_8_counters_takes_the_home() {
        let mut s = Signals::default();
        let gws = online(&[LOCAL, "gw-b"]);
        // 5 dB better: not enough.
        feed(&mut s, 1, 1..=10, &[(LOCAL, -100.0), ("gw-b", -95.0)]);
        assert_eq!(s.review(1, LOCAL, &gws), None);
        // 10 dB better, heard on every counter.
        let mut s = Signals::default();
        feed(&mut s, 1, 1..=3, &[(LOCAL, -100.0), ("gw-b", -90.0)]);
        assert_eq!(s.review(1, LOCAL, &gws), None, "two closed counters only");
        feed(&mut s, 1, 4..=4, &[(LOCAL, -100.0), ("gw-b", -90.0)]);
        assert_eq!(
            s.review(1, LOCAL, &gws),
            Some(("gw-b".to_string(), MoveReason::Better))
        );
        // An offline gateway is no candidate.
        assert_eq!(s.review(1, LOCAL, &online(&[LOCAL])), None);
    }

    /// The Beacon cluster's B-6 (HIL 2026-10-13), Node side: the home gateway stays online but
    /// stops hearing the Node (it walked away, or that Northbridge went deaf). After the
    /// hand-over the silent gateway's old average and window entries must not win it back.
    #[test]
    fn a_home_handed_over_never_returns_to_a_gateway_that_has_gone_silent() {
        let mut s = Signals::default();
        let gws = online(&[LOCAL, "gw-b"]);
        let mut home = "gw-b".to_string();
        let mut moves = Vec::new();
        let step = |s: &mut Signals, c: u32, by: &[(&str, f64)], home: &mut String| {
            feed(s, 1, c..=c, by);
            if let Some((to, why)) = s.review(1, home, &gws) {
                *home = to.clone();
                Some((to, why))
            } else {
                None
            }
        };
        for c in 1..=10 {
            assert_eq!(
                step(&mut s, c, &[("gw-b", -50.0), (LOCAL, -80.0)], &mut home),
                None
            );
        }
        for c in 11..=25 {
            moves.extend(step(&mut s, c, &[(LOCAL, -80.0)], &mut home));
        }
        assert_eq!(
            moves,
            [(LOCAL.to_string(), MoveReason::HomeNotHearing)],
            "one hand-over, no flap"
        );
        let mut back = Vec::new();
        for c in 26..=35 {
            back.extend(step(
                &mut s,
                c,
                &[("gw-b", -50.0), (LOCAL, -80.0)],
                &mut home,
            ));
        }
        assert_eq!(back, [("gw-b".to_string(), MoveReason::Better)]);
    }

    #[test]
    fn a_home_that_misses_3_counters_moves_to_who_heard_them() {
        let mut s = Signals::default();
        let gws = online(&[LOCAL, "gw-b"]);
        feed(&mut s, 1, 1..=1, &[(LOCAL, -90.0), ("gw-b", -100.0)]);
        feed(&mut s, 1, 2..=2, &[("gw-b", -100.0)]);
        assert_eq!(
            s.review(1, LOCAL, &gws),
            None,
            "counter 1: the home heard it"
        );
        feed(&mut s, 1, 3..=3, &[("gw-b", -100.0)]);
        assert_eq!(s.review(1, LOCAL, &gws), None, "one miss");
        feed(&mut s, 1, 4..=4, &[("gw-b", -100.0)]);
        assert_eq!(s.review(1, LOCAL, &gws), None, "two misses");
        feed(&mut s, 1, 5..=5, &[("gw-b", -100.0)]);
        assert_eq!(
            s.review(1, LOCAL, &gws),
            Some(("gw-b".to_string(), MoveReason::HomeNotHearing))
        );
    }

    #[test]
    fn a_home_gone_offline_hands_over_to_a_gateway_that_heard_the_node() {
        let mut s = Signals::default();
        feed(&mut s, 1, 1..=2, &[("gw-b", -100.0), (LOCAL, -110.0)]);
        let now = Instant::now();
        assert_eq!(
            s.take_over(1, "gw-b", &online(&[LOCAL]), now),
            Some(LOCAL.to_string())
        );
        assert_eq!(
            s.take_over(2, "gw-b", &online(&[LOCAL]), now),
            None,
            "never heard"
        );
        let later = now + OFFLINE_HEARD_WITHIN + Duration::from_secs(1);
        assert_eq!(
            s.take_over(1, "gw-b", &online(&[LOCAL]), later),
            None,
            "too long ago"
        );
    }

    #[test]
    fn the_best_join_copy_wins_and_the_first_of_equals() {
        let now = Instant::now();
        let mut j = JoinCopies::open(now, LOCAL.to_string(), -100.0, 1);
        j.add("gw-b".to_string(), -90.0, 2);
        j.add("gw-c".to_string(), -90.0, 3);
        assert_eq!(j.deadline, now + JOIN_WINDOW);
        assert_eq!(j.best(), Some(("gw-b".to_string(), 2)));
    }
}
