//! The central's gateways as the portal sees them — `plan/control/radio/p2p_cluster_gateway.md`
//! §8.
//!
//! The central keeps one [`GatewayBoard`] row per gateway it has served: its own Northbridge
//! and every Client Hub's. The host publishes from it, retained on the Server's broker:
//!
//! - each remote gateway's Northbridge document ([`crate::p2p::northbridge`]) on
//!   `proximos/<gw>/radio/northbridge`, the topic the Client's own family uses for it — the
//!   Client does not publish it itself while it lends its Northbridge;
//! - one `proximos/<server>/radio/p2p/gateways` document with a row per gateway.
//!
//! A gateway that went offline keeps its row, `online: false`, until the process restarts.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::p2p::gateways::LOCAL;
use crate::p2p::northbridge::{NorthbridgeStatus, UpdateSummary};
use crate::p2p::plane::{GatewayState, GatewayUpdate, NorthbridgeLink, Online};
use crate::p2p::runtime::Liveness;

/// The `p2p/gateways` document's topic for the Server `server`.
pub fn topic(server: &str) -> String {
    format!("{}/{server}/radio/p2p/gateways", crate::northbound::ROOT)
}

/// One gateway's row.
#[derive(Debug, Clone)]
pub struct GatewayRow {
    pub online: bool,
    /// How many joined Nodes have it as their home.
    pub homes: usize,
    /// The last `GET_INFO` round trip, ms: the network and two broker hops for a remote one.
    pub link_rtt_ms: Option<u64>,
    /// Its Northbridge document's status, fed by the central.
    pub status: NorthbridgeStatus,
    /// When it last said anything.
    pub liveness: Liveness,
}

/// The shared handle. Cheap to clone; every clone sees the same rows.
#[derive(Debug, Clone, Default)]
pub struct GatewayBoard {
    rows: Arc<Mutex<BTreeMap<String, GatewayRow>>>,
    /// Each Client's Northbridge update as its `state` document last said it: it arrives
    /// before the gateway is attached and outlives the row's status, which a re-attach
    /// replaces.
    updates: Arc<Mutex<BTreeMap<String, Option<UpdateSummary>>>>,
    /// The Clients whose last `state` said their Northbridge is being flashed: the forwarder
    /// has stepped off the plane for it, and the row says `starting`, not `unreachable`.
    flashing: Arc<Mutex<BTreeSet<String>>>,
    /// Bumped on every change of a row, so the host publishes on change.
    version: Arc<AtomicU64>,
}

impl GatewayBoard {
    fn update<T>(&self, f: impl FnOnce(&mut BTreeMap<String, GatewayRow>) -> (bool, T)) -> T {
        let (changed, out) = f(&mut self.rows.lock().unwrap_or_else(|p| p.into_inner()));
        if changed {
            self.version.fetch_add(1, Ordering::Relaxed);
        }
        out
    }

    /// A counter that changes whenever a row does.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    /// `gw` is online, fed through `status` and `liveness` from now on.
    pub fn online(&self, gw: &str, status: NorthbridgeStatus, liveness: Liveness) {
        if let Some(update) = self.updates_lock().get(gw) {
            status.set_update(update.clone());
        }
        self.update(|rows| {
            let homes = rows.get(gw).map_or(0, |r| r.homes);
            rows.insert(
                gw.to_string(),
                GatewayRow {
                    online: true,
                    homes,
                    link_rtt_ms: None,
                    status,
                    liveness,
                },
            );
            (true, ())
        });
    }

    /// `gw` went offline: its row stays, and its Northbridge document says why — `starting`
    /// while its Northbridge is being flashed, `unreachable` otherwise.
    pub fn offline(&self, gw: &str, now: i64, why: &str) {
        let flashing = self.flashing_lock().contains(gw);
        self.update(|rows| match rows.get_mut(gw) {
            Some(r) if r.online => {
                r.online = false;
                if flashing {
                    r.status.restarting();
                } else {
                    r.status.unreachable(now, why);
                }
                (true, ())
            }
            _ => (false, ()),
        });
    }

    /// `gw`'s `state` document (§2): an online one carries its Northbridge update and says
    /// whether the Northbridge is being flashed; an offline one (the will, a shutdown) ends a
    /// flash, so a Client that died during one reads `unreachable` again.
    pub fn on_state(&self, gw: &str, st: &GatewayState, now: i64) {
        let online = st.state == Online::Online;
        if online {
            self.set_update(gw, st.update.as_ref().map(GatewayUpdate::summary));
        }
        let flashing = online && st.northbridge == Some(NorthbridgeLink::Updating);
        let was = if flashing {
            !self.flashing_lock().insert(gw.to_string())
        } else {
            self.flashing_lock().remove(gw)
        };
        // A row already offline follows the change; an online one does when it detaches.
        if let Some(r) = self.rows.lock().unwrap_or_else(|p| p.into_inner()).get(gw) {
            if !r.online && flashing && !was {
                r.status.restarting();
            } else if !r.online && !flashing && was {
                r.status.unreachable(now, "gateway offline");
            }
        }
    }

    fn flashing_lock(&self) -> std::sync::MutexGuard<'_, BTreeSet<String>> {
        self.flashing.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// `gw`'s `state` document said its Northbridge update is `update` (§2): its Northbridge
    /// document carries it from now on, across re-attaches.
    pub fn set_update(&self, gw: &str, update: Option<UpdateSummary>) {
        self.updates_lock().insert(gw.to_string(), update.clone());
        // The status bumps its own version, which the fingerprint covers.
        if let Some(r) = self.rows.lock().unwrap_or_else(|p| p.into_inner()).get(gw) {
            r.status.set_update(update);
        }
    }

    fn updates_lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Option<UpdateSummary>>> {
        self.updates.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The figures the central refreshes on its tick.
    pub fn set_figures(&self, gw: &str, homes: usize, link_rtt_ms: Option<u64>) {
        self.update(|rows| match rows.get_mut(gw) {
            Some(r) if r.homes != homes || r.link_rtt_ms != link_rtt_ms => {
                r.homes = homes;
                r.link_rtt_ms = link_rtt_ms;
                (true, ())
            }
            _ => (false, ()),
        });
    }

    /// Whether `gw` has a row.
    pub fn has(&self, gw: &str) -> bool {
        self.rows
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(gw)
    }

    /// The remote gateways' rows, for their Northbridge documents.
    pub fn remotes(&self) -> Vec<(String, GatewayRow)> {
        self.rows
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(gw, _)| gw.as_str() != LOCAL)
            .map(|(gw, r)| (gw.clone(), r.clone()))
            .collect()
    }

    /// What changes when any document would: the board's version and every remote status's.
    pub fn fingerprint(&self) -> (u64, Vec<u64>) {
        let rows = self.rows.lock().unwrap_or_else(|p| p.into_inner());
        (
            self.version(),
            rows.values().map(|r| r.status.version()).collect(),
        )
    }

    /// The `p2p/gateways` document at `now`; `server` names the local gateway.
    pub fn document(&self, server: &str, now: i64) -> Value {
        let rows = self.rows.lock().unwrap_or_else(|p| p.into_inner());
        let gateways: Vec<Value> = rows
            .iter()
            .map(|(gw, r)| {
                let local = gw == LOCAL;
                json!({
                    "id": if local { server } else { gw.as_str() },
                    "local": local,
                    "online": r.online,
                    "link_rtt_ms": r.link_rtt_ms,
                    "homes": r.homes,
                    "last_event_at": r.liveness.last().map(|t| {
                        crate::lorawan::gateway::format_rfc3339(i64::try_from(t).unwrap_or(0))
                    }),
                })
            })
            .collect();
        json!({
            "updated_at": crate::lorawan::gateway::format_rfc3339(now),
            "gateways": gateways,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_topic_is_the_servers() {
        assert_eq!(
            topic("proximos-4e970f4a"),
            "proximos/proximos-4e970f4a/radio/p2p/gateways"
        );
    }

    #[test]
    fn an_offline_gateway_keeps_its_row_and_says_why() {
        let board = GatewayBoard::default();
        let status = NorthbridgeStatus::default();
        board.online(LOCAL, NorthbridgeStatus::default(), Liveness::default());
        board.online("gw-b", status.clone(), Liveness::default());
        board.set_figures("gw-b", 2, Some(41));
        let v = board.version();
        board.set_figures("gw-b", 2, Some(41));
        assert_eq!(board.version(), v, "unchanged figures are no change");

        board.offline("gw-b", 1_000, "gateway offline");
        let doc = board.document("proximos-4e970f4a", 1_000);
        let rows = doc["gateways"].as_array().expect("rows");
        assert_eq!(rows.len(), 2);
        let b = rows.iter().find(|r| r["id"] == "gw-b").expect("gw-b");
        assert_eq!(b["online"], false);
        assert_eq!(b["homes"], 2);
        assert_eq!(b["link_rtt_ms"], 41);
        let local = rows.iter().find(|r| r["local"] == true).expect("local");
        assert_eq!(local["id"], "proximos-4e970f4a");
        assert_eq!(
            status.document(1_000, false)["state"],
            "unreachable",
            "the remote document says it"
        );
        assert_eq!(board.remotes().len(), 1);
    }

    #[test]
    fn a_clients_update_reaches_its_document_before_and_after_a_re_attach() {
        let board = GatewayBoard::default();
        let flashing = UpdateSummary {
            state: "flashing",
            bundled: Some("0.4.1+d69b5d68".into()),
            attempts: 1,
            ..UpdateSummary::default()
        };
        // The state document comes first, the attach after it.
        board.set_update("gw-b", Some(flashing.clone()));
        let first = NorthbridgeStatus::default();
        board.online("gw-b", first.clone(), Liveness::default());
        assert_eq!(first.snapshot().update, Some(flashing));

        let fp = board.fingerprint();
        let idle = UpdateSummary {
            state: "idle",
            bundled: Some("0.4.1+d69b5d68".into()),
            ..UpdateSummary::default()
        };
        board.set_update("gw-b", Some(idle.clone()));
        assert_eq!(first.snapshot().update, Some(idle.clone()));
        assert_ne!(board.fingerprint(), fp, "the document is republished");

        board.offline("gw-b", 1_000, "gateway offline");
        let second = NorthbridgeStatus::default();
        board.online("gw-b", second.clone(), Liveness::default());
        assert_eq!(second.snapshot().update, Some(idle), "a re-attach keeps it");
        assert_eq!(second.document(1_000, true)["update"]["state"], "idle");
    }

    #[test]
    fn a_client_flashing_its_northbridge_is_starting_not_unreachable() {
        let board = GatewayBoard::default();
        let status = NorthbridgeStatus::default();
        board.online("gw-b", status.clone(), Liveness::default());
        let summary = UpdateSummary {
            state: "flashing",
            bundled: Some("0.4.1+d69b5d68".into()),
            attempts: 1,
            ..UpdateSummary::default()
        };
        let updating =
            GatewayState::online(None, NorthbridgeLink::Updating).with_update(Some(&summary));
        // The central detaches a gateway that is not serviceable after the state arrives.
        board.on_state("gw-b", &updating, 1_000);
        board.offline("gw-b", 1_000, "gateway offline");
        let doc = status.document(1_000, false);
        assert_eq!(doc["state"], "starting");
        assert_eq!(doc["update"]["state"], "flashing");

        // The Client died during the flash: its will ends it.
        board.on_state("gw-b", &GatewayState::offline(), 1_010);
        assert_eq!(status.document(1_010, false)["state"], "unreachable");

        // A plain outage is unreachable from the start.
        let other = NorthbridgeStatus::default();
        board.online("gw-c", other.clone(), Liveness::default());
        board.on_state("gw-c", &GatewayState::offline(), 1_000);
        board.offline("gw-c", 1_000, "gateway offline");
        assert_eq!(other.document(1_000, false)["state"], "unreachable");
    }
}
