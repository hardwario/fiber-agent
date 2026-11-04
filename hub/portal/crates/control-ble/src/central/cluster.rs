//! Beacons belong to the cluster — `plan/decisions.md` #42.
//!
//! Three things live here, all of them pure enough to test without a radio or a broker:
//!
//! - **The pushed view** ([`ClusterView`], `proximos/<hub>/eye/cluster`, retained). The Server's
//!   agent publishes one per Hub: the Server's registry (`eye.tags`), the fleet `eye.*` keys, and
//!   the Beacons this Hub is **home** for.
//! - **The config store a Hub reads through** ([`ClusterConfigStore`]). A Client answers `load()`
//!   with the Server's view instead of its own `eye:` section, so the Server's registry is the
//!   Client's enrolled set and the cluster has one fleet profile and one aggregation interval. On
//!   every Hub the loaded section carries the home set ([`EyeSection::home`]), which is what
//!   gates GATT ([`EyeSection::allows_gatt`]).
//! - **The home computation** ([`HomeTracker`]), run by the Server's agent over every Hub's
//!   `eye/sensors`: the P2P home rules of `plan/control/radio/p2p_cluster_gateway.md` §5,
//!   applied per [`ROUND_S`] round instead of per uplink counter, with a tie keeping the current
//!   home. Its result is published as [`HomesDoc`] on `proximos/<server>/eye/homes`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, RwLock};

use crate::config::{En12830Config, EyeSection, EyeTag};
use crate::error::{Component, Error, Kind, Result, Role};
use crate::host::{AuditEntry, ConfigStore, Host};
use crate::mac::MacAddr;

/// One review round, in seconds: the Beacon analogue of a Node's uplink counter.
pub const ROUND_S: i64 = 60;
/// A Hub, or a Hub's sighting of a Beacon, older than this is stale (decision #42: 15 min).
pub const STALE_S: i64 = 15 * 60;
const EWMA_ALPHA: f64 = 0.3;
/// Another Hub must hear the Beacon this much better than the home …
const BETTER_BY_DB: f64 = 6.0;
/// … in at least this many of the last [`WINDOW`] rounds.
const BETTER_HEARD: usize = 3;
const WINDOW: usize = 8;
/// Rounds in which someone else heard the Beacon and the home did not, before the home moves.
const HOME_MISSED: u32 = 3;
/// The wire version of [`ClusterView`] and [`HomesDoc`].
pub const WIRE_VERSION: u32 = 1;

// ---------------------------------------------------------------------------------------------
// The wire
// ---------------------------------------------------------------------------------------------

/// The fleet `eye.*` keys a Client takes from the Server (Q4: one value for the cluster).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FleetKeys {
    pub enabled: bool,
    pub auto_provision: bool,
    pub auto_discover: bool,
    pub aggregation_interval_s: u64,
    pub recording_interval_min: u16,
    #[serde(default)]
    pub en12830: En12830Config,
}

impl FleetKeys {
    pub fn of(section: &EyeSection) -> FleetKeys {
        FleetKeys {
            enabled: section.enabled,
            auto_provision: section.auto_provision,
            auto_discover: section.auto_discover,
            aggregation_interval_s: section.aggregation_interval_s,
            recording_interval_min: section.recording_interval_min,
            en12830: section.en12830.clone(),
        }
    }
}

/// `proximos/<hub>/eye/cluster`, retained, one per Hub, written by the Server's agent.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClusterView {
    pub v: u32,
    /// The Server's hostname — the registry, and the name a refusal on a Client gives.
    pub server: String,
    /// The Server's registry, verbatim (`eye.tags`).
    pub tags: Vec<EyeTag>,
    /// The MACs this Hub is home for, canonical spelling.
    pub home: Vec<String>,
    pub fleet: FleetKeys,
}

impl ClusterView {
    /// The view the Server pushes to `hub`.
    pub fn for_hub(
        hub: &str,
        server: &str,
        registry: &EyeSection,
        homes: &BTreeMap<MacAddr, String>,
    ) -> ClusterView {
        ClusterView {
            v: WIRE_VERSION,
            server: server.to_string(),
            tags: registry.tags.clone(),
            home: homes
                .iter()
                .filter(|(_, h)| h.eq_ignore_ascii_case(hub))
                .map(|(m, _)| m.to_string())
                .collect(),
            fleet: FleetKeys::of(registry),
        }
    }

    fn home_set(&self) -> BTreeSet<MacAddr> {
        self.home
            .iter()
            .filter_map(|m| MacAddr::parse(m).ok())
            .collect()
    }
}

/// `proximos/<server>/eye/homes`, retained: the registry and every registered Beacon's home Hub.
/// What the portal reads; it computes nothing itself.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HomesDoc {
    pub v: u32,
    pub server: String,
    pub registered: Vec<String>,
    /// MAC → home Hub. A registered Beacon is always here: one nobody heard yet is the Server's.
    pub homes: BTreeMap<String, String>,
}

// ---------------------------------------------------------------------------------------------
// What a Hub reads through
// ---------------------------------------------------------------------------------------------

/// This Hub's place in the cluster.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Membership {
    /// `Some(server)` on a cluster Client (the role marker); `None` on a Server or a standalone
    /// Hub.
    pub client_of: Option<String>,
    /// The last view the Server pushed to this Hub. `None` until one arrives.
    pub view: Option<ClusterView>,
}

pub type SharedMembership = Arc<RwLock<Membership>>;

impl Membership {
    pub fn shared(client_of: Option<String>) -> SharedMembership {
        Arc::new(RwLock::new(Membership {
            client_of,
            view: None,
        }))
    }

    /// The `eye:` section this Hub works with, from its own file `local`.
    ///
    /// A Client takes the registry and the fleet keys from the view and nothing from its file;
    /// without a view yet it tracks nothing registered and dials nothing. A Server keeps its
    /// file. Both carry the home set the view names.
    pub fn effective(&self, local: EyeSection) -> EyeSection {
        match (&self.client_of, &self.view) {
            (Some(_), Some(view)) => EyeSection {
                enabled: view.fleet.enabled,
                auto_provision: view.fleet.auto_provision,
                auto_discover: view.fleet.auto_discover,
                tags: view.tags.clone(),
                en12830: view.fleet.en12830.clone(),
                aggregation_interval_s: view.fleet.aggregation_interval_s,
                recording_interval_min: view.fleet.recording_interval_min,
                legacy: BTreeMap::new(),
                home: Some(view.home_set()),
            },
            (Some(_), None) => EyeSection {
                tags: Vec::new(),
                home: Some(BTreeSet::new()),
                legacy: BTreeMap::new(),
                ..local
            },
            (None, Some(view)) => EyeSection {
                home: Some(view.home_set()),
                ..local
            },
            (None, None) => local,
        }
    }

    /// The server a Client refusal names: the view's, else the role marker's.
    fn server_name(&self) -> Option<String> {
        self.client_of.as_ref().map(|marker| {
            self.view
                .as_ref()
                .map(|v| v.server.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| marker.clone())
        })
    }
}

fn read(m: &SharedMembership) -> std::sync::RwLockReadGuard<'_, Membership> {
    m.read().unwrap_or_else(|p| p.into_inner())
}

/// Install a pushed view. Returns whether it changed anything.
pub fn install_view(m: &SharedMembership, view: ClusterView) -> bool {
    let mut g = m.write().unwrap_or_else(|p| p.into_inner());
    if g.view.as_ref() == Some(&view) {
        return false;
    }
    g.view = Some(view);
    true
}

/// The refusal a Client gives `beacon-add` and `beacon-remove` from any surface (Q6).
pub fn client_refusal(server: &str) -> Error {
    Error::new(
        Kind::CapabilityDisabled,
        Role::Central,
        Component::BeaconRegistry,
        format!("this Hub is a cluster Client; Beacons are registered on the Server {server}"),
    )
}

/// The refusal a Hub gives a GATT request for a Beacon it is not home for.
pub fn not_home_refusal(mac: &MacAddr) -> Error {
    Error::new(
        Kind::CapabilityDisabled,
        Role::Central,
        Component::BeaconGatt,
        format!(
            "{mac} is not homed on this Hub; only its home Hub talks to it over GATT \
             (decision #42)"
        ),
    )
}

/// Whether this Hub may dial `mac` now, as an error a command can return.
pub fn check_home(host: &Host, mac: &MacAddr) -> Result<()> {
    if host.config.load()?.allows_gatt(mac) {
        Ok(())
    } else {
        Err(not_home_refusal(mac))
    }
}

/// The `ConfigStore` every Hub's BLE reads its `eye:` section through (decision #42).
pub struct ClusterConfigStore {
    local: Arc<dyn ConfigStore>,
    membership: SharedMembership,
}

impl ClusterConfigStore {
    pub fn new(local: Arc<dyn ConfigStore>, membership: SharedMembership) -> ClusterConfigStore {
        ClusterConfigStore { local, membership }
    }
}

impl ConfigStore for ClusterConfigStore {
    fn load(&self) -> Result<EyeSection> {
        let local = self.local.load()?;
        Ok(read(&self.membership).effective(local))
    }

    /// A Client's `eye:` is the Server's, so a Client writes none of it: the registry and the
    /// fleet keys are changed on the Server and arrive with the next view.
    fn mutate(&self, f: &mut dyn FnMut(&mut EyeSection)) -> Result<EyeSection> {
        if let Some(server) = read(&self.membership).server_name() {
            return Err(client_refusal(&server));
        }
        let section = self.local.mutate(f)?;
        Ok(read(&self.membership).effective(section))
    }

    fn audit(&self, entry: &AuditEntry) {
        self.local.audit(entry);
    }
}

// ---------------------------------------------------------------------------------------------
// The Server's home computation
// ---------------------------------------------------------------------------------------------

/// One Beacon as one Hub's `eye/sensors` reported it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sighting {
    pub mac: MacAddr,
    pub rssi: Option<i16>,
    pub last_seen_ts: Option<i64>,
    pub stale: bool,
}

/// The sightings in one Hub's `eye/sensors` payload. Lenient: an entry without a parsable MAC is
/// skipped, and a payload that is not a snapshot yields none.
pub fn sightings_from_payload(payload: &[u8]) -> Vec<Sighting> {
    #[derive(serde::Deserialize)]
    struct Entry {
        mac: String,
        #[serde(default)]
        rssi: Option<i16>,
        #[serde(default)]
        last_seen_ts: Option<i64>,
        #[serde(default)]
        stale: bool,
    }
    #[derive(serde::Deserialize)]
    struct Snapshot {
        #[serde(default)]
        tags: Vec<serde_json::Value>,
    }
    let Ok(snapshot) = serde_json::from_slice::<Snapshot>(payload) else {
        return Vec::new();
    };
    snapshot
        .tags
        .into_iter()
        .filter_map(|v| serde_json::from_value::<Entry>(v).ok())
        .filter_map(|e| {
            Some(Sighting {
                mac: MacAddr::parse(&e.mac).ok()?,
                rssi: e.rssi,
                last_seen_ts: e.last_seen_ts,
                stale: e.stale,
            })
        })
        .collect()
}

/// Why a home moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveReason {
    /// The first home of a Beacon.
    Initial,
    /// Another Hub hears it ≥ 6 dB better in 3 of the last 8 rounds.
    Better,
    /// The home missed 3 rounds that another Hub heard.
    HomeNotHearing,
    /// The home itself has been silent for [`STALE_S`].
    HomeOffline,
}

impl MoveReason {
    pub fn as_str(self) -> &'static str {
        match self {
            MoveReason::Initial => "first home",
            MoveReason::Better => "another Hub hears it better",
            MoveReason::HomeNotHearing => "the home missed its last rounds",
            MoveReason::HomeOffline => "the home went silent",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HomeMove {
    pub mac: MacAddr,
    pub from: Option<String>,
    pub to: String,
    pub reason: MoveReason,
}

#[derive(Debug, Clone, Copy)]
struct HubSignal {
    ewma: f64,
    last_heard: i64,
    last_seen_ts: Option<i64>,
    /// The best RSSI of a fresh sighting in the open round.
    round_q: Option<f64>,
}

#[derive(Debug, Default)]
struct BeaconSignal {
    per_hub: BTreeMap<String, HubSignal>,
    /// The Hubs that heard it in each of the last closed rounds that anyone heard it in.
    closed: VecDeque<BTreeSet<String>>,
    home_missed: u32,
}

/// Per Beacon and Hub: how well and how recently it is heard. In memory, like the P2P central's:
/// a restarted agent seeds the homes from its own retained [`HomesDoc`] and earns the rest again.
#[derive(Debug, Default)]
pub struct HomeTracker {
    beacons: BTreeMap<MacAddr, BeaconSignal>,
    homes: BTreeMap<MacAddr, String>,
    /// When each Hub's last snapshot arrived.
    hub_seen: BTreeMap<String, i64>,
}

impl HomeTracker {
    pub fn homes(&self) -> &BTreeMap<MacAddr, String> {
        &self.homes
    }

    /// Every Hub that has reported a snapshot, lowercase.
    pub fn hubs(&self) -> impl Iterator<Item = &String> {
        self.hub_seen.keys()
    }

    /// Homes from the retained document of an earlier run. Never overrides a home this run set.
    pub fn seed(&mut self, doc: &HomesDoc) {
        for (mac, hub) in &doc.homes {
            if let Ok(mac) = MacAddr::parse(mac) {
                self.homes
                    .entry(mac)
                    .or_insert_with(|| hub.to_ascii_lowercase());
            }
        }
    }

    /// One Hub's `eye/sensors` snapshot. A sighting is fresh when it is not stale, has an RSSI,
    /// and its `last_seen_ts` moved since this Hub's previous snapshot.
    pub fn observe(&mut self, hub: &str, sightings: &[Sighting], now: i64) {
        let hub = hub.to_ascii_lowercase();
        self.hub_seen.insert(hub.clone(), now);
        for s in sightings {
            let (Some(rssi), false) = (s.rssi, s.stale) else {
                continue;
            };
            let q = f64::from(rssi);
            let sig = self
                .beacons
                .entry(s.mac)
                .or_default()
                .per_hub
                .entry(hub.clone())
                .or_insert(HubSignal {
                    ewma: q,
                    last_heard: now,
                    last_seen_ts: None,
                    round_q: None,
                });
            let fresh = match (s.last_seen_ts, sig.last_seen_ts) {
                (Some(new), Some(old)) => new > old,
                _ => true,
            };
            if !fresh {
                continue;
            }
            sig.last_seen_ts = s.last_seen_ts.or(sig.last_seen_ts);
            sig.last_heard = now;
            sig.round_q = Some(sig.round_q.map_or(q, |old| old.max(q)));
        }
    }

    /// Close a round: fold the round's sightings into the averages and review every registered
    /// Beacon's home. Forgets whatever is no longer registered.
    pub fn close_round(
        &mut self,
        server: &str,
        registered: &BTreeSet<MacAddr>,
        now: i64,
    ) -> Vec<HomeMove> {
        let server = server.to_ascii_lowercase();
        self.beacons.retain(|m, _| registered.contains(m));
        self.homes.retain(|m, _| registered.contains(m));
        let online: BTreeSet<String> = self
            .hub_seen
            .iter()
            .filter(|(_, at)| now - **at <= STALE_S)
            .map(|(h, _)| h.clone())
            .chain(std::iter::once(server.clone()))
            .collect();

        let mut moves = Vec::new();
        for mac in registered {
            let b = self.beacons.entry(*mac).or_default();
            let mut heard = BTreeSet::new();
            for (hub, sig) in b.per_hub.iter_mut() {
                if let Some(q) = sig.round_q.take() {
                    sig.ewma = EWMA_ALPHA * q + (1.0 - EWMA_ALPHA) * sig.ewma;
                    heard.insert(hub.clone());
                }
            }
            if !heard.is_empty() {
                b.closed.push_back(heard.clone());
                while b.closed.len() > WINDOW {
                    b.closed.pop_front();
                }
            }
            let home = self.homes.get(mac).cloned();
            if let Some((to, reason)) = review(b, home.as_deref(), &heard, &online, &server, now) {
                self.homes.insert(*mac, to.clone());
                moves.push(HomeMove {
                    mac: *mac,
                    from: home,
                    to,
                    reason,
                });
            }
        }
        moves
    }

    pub fn homes_doc(&self, server: &str, registered: &BTreeSet<MacAddr>) -> HomesDoc {
        HomesDoc {
            v: WIRE_VERSION,
            server: server.to_string(),
            registered: registered.iter().map(MacAddr::to_string).collect(),
            homes: self
                .homes
                .iter()
                .filter(|(m, _)| registered.contains(m))
                .map(|(m, h)| (m.to_string(), h.clone()))
                .collect(),
        }
    }
}

/// The best of `candidates` by EWMA. Equal signals go to the Server, then to the first name, so
/// a review is deterministic.
fn best<'a>(
    b: &BeaconSignal,
    server: &str,
    candidates: impl Iterator<Item = &'a String>,
) -> Option<String> {
    candidates
        .filter_map(|g| b.per_hub.get(g).map(|s| (g, s.ewma)))
        .max_by(|(ga, qa), (gb, qb)| {
            qa.total_cmp(qb)
                .then_with(|| (ga.as_str() == server).cmp(&(gb.as_str() == server)))
                .then_with(|| gb.cmp(ga))
        })
        .map(|(g, _)| g.clone())
}

/// The home `b` should have now, if it is not `home`. Every rule asks strictly more of a
/// challenger than of the home, so equal signals keep the incumbent (Q8).
fn review(
    b: &mut BeaconSignal,
    home: Option<&str>,
    heard: &BTreeSet<String>,
    online: &BTreeSet<String>,
    server: &str,
    now: i64,
) -> Option<(String, MoveReason)> {
    let Some(home) = home else {
        // Nobody heard it yet: it is the Server's until a Hub does.
        let to = best(b, server, heard.iter()).unwrap_or_else(|| server.to_string());
        return Some((to, MoveReason::Initial));
    };
    if !heard.is_empty() {
        if heard.contains(home) {
            b.home_missed = 0;
        } else {
            b.home_missed = b.home_missed.saturating_add(1);
        }
    }
    let recent = |g: &str| {
        b.per_hub
            .get(g)
            .is_some_and(|s| now - s.last_heard <= STALE_S)
    };
    if !online.contains(home) {
        let to = best(
            b,
            server,
            online.iter().filter(|g| g.as_str() != home && recent(g)),
        )?;
        b.home_missed = 0;
        return Some((to, MoveReason::HomeOffline));
    }
    let home_q = b.per_hub.get(home).map(|s| s.ewma);
    let heard_in = |g: &str| b.closed.iter().filter(|r| r.contains(g)).count();
    // A challenger must hear the Beacon now: a Hub gone silent keeps its last average and its
    // window entries, and is still "online" for STALE_S.
    let better = best(
        b,
        server,
        online.iter().filter(|g| {
            g.as_str() != home
                && heard.contains(*g)
                && heard_in(g) >= BETTER_HEARD
                && b.per_hub
                    .get(g.as_str())
                    .is_some_and(|s| home_q.is_none_or(|h| s.ewma >= h + BETTER_BY_DB))
        }),
    );
    if let Some(to) = better {
        b.home_missed = 0;
        return Some((to, MoveReason::Better));
    }
    if b.home_missed >= HOME_MISSED {
        if let Some(to) = best(
            b,
            server,
            heard
                .iter()
                .filter(|g| g.as_str() != home && online.contains(*g)),
        ) {
            b.home_missed = 0;
            return Some((to, MoveReason::HomeNotHearing));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::MemoryConfigStore;

    const A: &str = "7C:D9:F4:00:00:0A";
    const B: &str = "7C:D9:F4:00:00:0B";

    fn mac(s: &str) -> MacAddr {
        MacAddr::parse(s).unwrap()
    }

    fn seen(m: &str, rssi: i16, ts: i64) -> Sighting {
        Sighting {
            mac: mac(m),
            rssi: Some(rssi),
            last_seen_ts: Some(ts),
            stale: false,
        }
    }

    fn reg(macs: &[&str]) -> BTreeSet<MacAddr> {
        macs.iter().map(|m| mac(m)).collect()
    }

    /// One round: each `(hub, rssi)` hears `A` once, then the round closes.
    fn round(t: &mut HomeTracker, now: i64, heard: &[(&str, i16)]) -> Vec<HomeMove> {
        for (hub, rssi) in heard {
            t.observe(hub, &[seen(A, *rssi, now)], now);
        }
        t.close_round("srv", &reg(&[A]), now)
    }

    #[test]
    fn a_registered_beacon_nobody_heard_is_homed_on_the_server() {
        let mut t = HomeTracker::default();
        let moves = t.close_round("SRV", &reg(&[A]), 0);
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].to, "srv");
        assert_eq!(moves[0].reason, MoveReason::Initial);
    }

    #[test]
    fn the_first_home_is_the_strongest_hub_that_heard_it() {
        let mut t = HomeTracker::default();
        let moves = round(&mut t, 60, &[("srv", -90), ("c1", -60)]);
        assert_eq!(moves[0].to, "c1");
    }

    #[test]
    fn a_hub_6_db_better_in_3_of_8_rounds_takes_the_home_and_5_db_never_does() {
        let mut t = HomeTracker::default();
        round(&mut t, 60, &[("srv", -70)]);
        assert_eq!(t.homes()[&mac(A)], "srv");
        // 5 dB better for many rounds: hysteresis holds.
        for i in 2..12 {
            assert!(round(&mut t, i * 60, &[("srv", -70), ("c1", -65)]).is_empty());
        }
        assert_eq!(t.homes()[&mac(A)], "srv");
        // 12 dB better: the average crosses +6 dB within a few rounds, and c1 has been heard
        // in far more than 3 of the last 8.
        let mut moved = None;
        for i in 12..20 {
            if let Some(m) = round(&mut t, i * 60, &[("srv", -70), ("c1", -58)]).pop() {
                moved = Some((i, m));
                break;
            }
        }
        let (_, m) = moved.expect("c1 takes the home");
        assert_eq!((m.to.as_str(), m.reason), ("c1", MoveReason::Better));
        assert_eq!(m.from.as_deref(), Some("srv"));
    }

    #[test]
    fn a_challenger_needs_3_rounds_heard_even_when_far_stronger() {
        let mut t = HomeTracker::default();
        round(&mut t, 60, &[("srv", -80)]);
        assert!(round(&mut t, 120, &[("srv", -80), ("c1", -40)]).is_empty());
        assert!(round(&mut t, 180, &[("srv", -80), ("c1", -40)]).is_empty());
        let m = round(&mut t, 240, &[("srv", -80), ("c1", -40)]);
        assert_eq!(m[0].to, "c1");
    }

    #[test]
    fn equal_signals_keep_the_incumbent() {
        let mut t = HomeTracker::default();
        round(&mut t, 60, &[("c1", -70), ("srv", -90)]);
        assert_eq!(t.homes()[&mac(A)], "c1");
        for i in 2..30 {
            assert!(round(&mut t, i * 60, &[("c1", -70), ("srv", -70)]).is_empty());
        }
        assert_eq!(t.homes()[&mac(A)], "c1", "a tie never moves the home");
    }

    #[test]
    fn a_home_that_misses_3_rounds_hands_over_to_who_heard_them() {
        let mut t = HomeTracker::default();
        round(&mut t, 60, &[("srv", -60), ("c1", -90)]);
        assert_eq!(t.homes()[&mac(A)], "srv");
        // The Beacon walks away from the Server: only c1 hears it now (weakly).
        assert!(round(&mut t, 120, &[("c1", -88)]).is_empty());
        assert!(round(&mut t, 180, &[("c1", -88)]).is_empty());
        let m = round(&mut t, 240, &[("c1", -88)]);
        assert_eq!(
            (m[0].to.as_str(), m[0].reason),
            ("c1", MoveReason::HomeNotHearing)
        );
    }

    /// HIL 2026-10-13 (B-6): the Client home went down; its snapshots stopped, but it stays
    /// "online" for STALE_S and its last average and window entries were still there. The home
    /// moved to the Server after 3 missed rounds, then straight back to the silent Client as
    /// "better" — a home on a Hub that hears nothing.
    #[test]
    fn a_home_handed_over_never_returns_to_a_hub_that_has_gone_silent() {
        let mut t = HomeTracker::default();
        for i in 1..12 {
            round(&mut t, i * 60, &[("c1", -50), ("srv", -80)]);
        }
        assert_eq!(t.homes()[&mac(A)], "c1");
        // c1 goes down: only the Server reports, for longer than the window.
        let mut moves = Vec::new();
        for i in 12..24 {
            moves.extend(round(&mut t, i * 60, &[("srv", -80)]));
        }
        let to: Vec<_> = moves.iter().map(|m| (m.to.as_str(), m.reason)).collect();
        assert_eq!(
            to,
            [("srv", MoveReason::HomeNotHearing)],
            "one hand-over, no flap"
        );
        // c1 comes back: it wins the home again by the usual rule.
        let mut back = Vec::new();
        for i in 24..32 {
            back.extend(round(&mut t, i * 60, &[("c1", -50), ("srv", -80)]));
        }
        let to: Vec<_> = back.iter().map(|m| (m.to.as_str(), m.reason)).collect();
        assert_eq!(to, [("c1", MoveReason::Better)]);
    }

    #[test]
    fn a_round_nobody_heard_counts_against_no_one() {
        let mut t = HomeTracker::default();
        round(&mut t, 60, &[("c1", -60)]);
        for i in 2..10 {
            assert!(round(&mut t, i * 60, &[]).is_empty());
        }
        assert_eq!(t.homes()[&mac(A)], "c1");
    }

    #[test]
    fn a_silent_home_hands_over_to_a_hub_that_heard_the_beacon_recently() {
        let mut t = HomeTracker::default();
        round(&mut t, 60, &[("c1", -60), ("c2", -80)]);
        assert_eq!(t.homes()[&mac(A)], "c1");
        // c1 publishes nothing any more; c2 heard A a little while ago, then keeps reporting
        // snapshots without it.
        assert!(round(&mut t, 60 + STALE_S - 60, &[("c2", -80)]).is_empty());
        let later = 60 + STALE_S + 1;
        t.observe("c2", &[], later);
        let m = t.close_round("srv", &reg(&[A]), later);
        assert_eq!(
            (m[0].to.as_str(), m[0].reason),
            ("c2", MoveReason::HomeOffline)
        );
    }

    #[test]
    fn a_stale_or_repeated_sighting_is_not_a_fresh_one() {
        let mut t = HomeTracker::default();
        t.observe("c1", &[seen(A, -60, 100)], 100);
        t.close_round("srv", &reg(&[A]), 100);
        assert_eq!(t.homes()[&mac(A)], "c1");
        // The same last_seen_ts again, and a stale one: neither is a hearing.
        t.observe("c1", &[seen(A, -60, 100)], 160);
        t.observe(
            "c1",
            &[Sighting {
                stale: true,
                ..seen(A, -60, 170)
            }],
            170,
        );
        t.observe("srv", &[seen(A, -90, 170)], 170);
        t.close_round("srv", &reg(&[A]), 180);
        assert_eq!(t.beacons[&mac(A)].home_missed, 1);
    }

    #[test]
    fn a_removed_beacon_is_forgotten_and_a_seed_never_overrides_this_run() {
        let mut t = HomeTracker::default();
        round(&mut t, 60, &[("c1", -60)]);
        t.seed(&HomesDoc {
            v: 1,
            server: "srv".into(),
            registered: vec![A.into(), B.into()],
            homes: BTreeMap::from([(A.into(), "c9".into()), (B.into(), "C2".into())]),
        });
        assert_eq!(t.homes()[&mac(A)], "c1");
        assert_eq!(t.homes()[&mac(B)], "c2");
        t.close_round("srv", &reg(&[B]), 120);
        assert!(!t.homes().contains_key(&mac(A)));
        let doc = t.homes_doc("srv", &reg(&[B]));
        assert_eq!(doc.homes.get(B).map(String::as_str), Some("c2"));
        assert_eq!(doc.registered, vec![B.to_string()]);
    }

    fn view(server: &str, tags: &[&str], home: &[&str]) -> ClusterView {
        let mut registry = EyeSection {
            aggregation_interval_s: 300,
            auto_provision: true,
            ..EyeSection::default()
        };
        registry.tags = tags
            .iter()
            .map(|m| EyeTag {
                mac: m.to_string(),
                name: None,
                enabled: true,
                logging_interval_min: None,
                recording: None,
                field_thresholds: Vec::new(),
                provisioned: None,
            })
            .collect();
        let homes = home.iter().map(|m| (mac(m), "c1".to_string())).collect();
        ClusterView::for_hub("c1", server, &registry, &homes)
    }

    #[test]
    fn a_client_reads_the_servers_registry_and_fleet_keys_not_its_own_file() {
        // A leftover per-Client registration, and an interval of its own.
        let own = EyeSection {
            tags: view("srv", &[B], &[]).tags,
            aggregation_interval_s: 60,
            ..EyeSection::default()
        };
        let m = Membership::shared(Some("srv".into()));
        let store =
            ClusterConfigStore::new(Arc::new(MemoryConfigStore::with_section(own)), m.clone());

        // No view yet: nothing registered, nothing dialled.
        let s = store.load().unwrap();
        assert!(s.tags.is_empty());
        assert!(!s.allows_gatt(&mac(B)));

        assert!(install_view(&m, view("srv", &[A, B], &[A])));
        assert!(
            !install_view(&m, view("srv", &[A, B], &[A])),
            "same view, no change"
        );
        let s = store.load().unwrap();
        assert_eq!(s.tags.len(), 2);
        assert_eq!(
            s.aggregation_interval_s, 300,
            "the cluster's interval, not 60"
        );
        assert!(s.auto_provision);
        assert!(s.allows_gatt(&mac(A)));
        assert!(!s.allows_gatt(&mac(B)), "registered, but homed elsewhere");
    }

    #[test]
    fn a_client_refuses_every_registry_write_naming_the_server() {
        let m = Membership::shared(Some("proximos-server".into()));
        let store = ClusterConfigStore::new(
            Arc::new(MemoryConfigStore::with_section(EyeSection::default())),
            m,
        );
        let err = store.mutate(&mut |s| s.tags.clear()).unwrap_err();
        assert_eq!(err.kind(), Kind::CapabilityDisabled);
        assert!(err
            .message()
            .contains("cluster Client; Beacons are registered on the Server proximos-server"));
    }

    #[test]
    fn a_server_keeps_its_file_and_gates_gatt_by_its_view_once_it_has_one() {
        let own = EyeSection {
            tags: view("srv", &[A, B], &[]).tags,
            ..EyeSection::default()
        };
        let m = Membership::shared(None);
        let store =
            ClusterConfigStore::new(Arc::new(MemoryConfigStore::with_section(own)), m.clone());
        let s = store.load().unwrap();
        assert!(
            s.allows_gatt(&mac(B)),
            "standalone: everything it registered"
        );
        install_view(&m, view("srv", &[A, B], &[A]));
        let s = store.load().unwrap();
        assert_eq!(s.tags.len(), 2);
        assert!(s.allows_gatt(&mac(A)));
        assert!(!s.allows_gatt(&mac(B)));
        // A Server still writes its registry.
        assert!(store
            .mutate(&mut |s| s.tags.pop().map(drop).unwrap_or(()))
            .is_ok());
    }

    #[test]
    fn sightings_are_read_leniently_from_an_eye_sensors_payload() {
        let got = sightings_from_payload(
            br#"{"timestamp":"x","tags":[
                {"mac":"7c:d9:f4:00:00:0a","rssi":-61,"last_seen_ts":5,"stale":false},
                {"mac":"nonsense","rssi":-1},
                {"mac":"7C:D9:F4:00:00:0B","stale":true}
            ]}"#,
        );
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], seen(A, -61, 5));
        assert_eq!((got[1].rssi, got[1].stale), (None, true));
        assert!(sightings_from_payload(b"not json").is_empty());
    }

    #[test]
    fn the_wire_shapes_round_trip() {
        let v = view("srv", &[A], &[A]);
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["home"][0], A);
        assert_eq!(json["fleet"]["aggregation_interval_s"], 300);
        assert_eq!(serde_json::from_value::<ClusterView>(json).unwrap(), v);
        let doc: HomesDoc = serde_json::from_str(
            r#"{"v":1,"server":"srv","registered":["7C:D9:F4:00:00:0A"],
                "homes":{"7C:D9:F4:00:00:0A":"c1"}}"#,
        )
        .unwrap();
        assert_eq!(doc.homes[A], "c1");
    }
}
