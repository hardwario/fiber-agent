//! Beacons belong to the cluster (`plan/decisions.md` #42) — the agent's half.
//!
//! `control_ble::central::cluster` holds the rules and the wire; this module wires them to the
//! process:
//!
//! - [`membership`]: the one [`SharedMembership`] the BLE config store reads through. Its role
//!   comes from the cluster role marker and is re-read every tick, so a join takes effect without
//!   a restart.
//! - On a **Server** (or a standalone Hub — a cluster of one) the [`Coordinator`] hears every
//!   Hub's `eye/sensors` on the local broker, closes a home round every
//!   [`control_ble::central::cluster::ROUND_S`], and publishes, retained, one `eye/cluster` view
//!   per Hub and its own `eye/homes`. Its own view it installs in-process.
//! - On a **Client** the agent only takes its view from `<self>/eye/cluster` on the Server's
//!   broker; it computes and publishes nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use control_ble::central::cluster::{
    install_view, sightings_from_payload, ClusterView, HomeTracker, HomesDoc, Membership,
    SharedMembership, ROUND_S,
};
use control_ble::config::EyeSection;
use control_ble::host::ConfigStore;
use control_ble::mac::MacAddr;

use crate::libs::cluster_state::ClusterDir;
use crate::libs::mqtt::messages::MqttMessage;

/// How often the coordinator thread wakes.
const TICK: Duration = Duration::from_secs(5);
/// A view or the homes document is republished at least this often, even unchanged, so a
/// retained copy lost by a broker restart comes back.
const REPUBLISH_S: i64 = 600;
/// How long a fresh Server waits for its own retained `eye/homes` before it computes homes from
/// nothing (every registered Beacon starts on the Server then).
const SEED_GRACE_S: i64 = 30;

/// The Server a Client names, from the role marker: `None` on a Server or a standalone Hub. An
/// unreadable marker reads as Client — a Hub that cannot tell must not act as the registry.
fn client_of(dir: &ClusterDir) -> Option<String> {
    match dir.is_client() {
        Ok(false) => None,
        Ok(true) | Err(_) => Some(
            dir.server_host()
                .unwrap_or_else(|| "of this cluster".to_string()),
        ),
    }
}

/// The process's membership, created on first use from the role marker.
pub fn membership() -> &'static SharedMembership {
    static M: OnceLock<SharedMembership> = OnceLock::new();
    M.get_or_init(|| Membership::shared(client_of(&ClusterDir::at_default())))
}

fn refresh_role(m: &SharedMembership) {
    let now = client_of(&ClusterDir::at_default());
    let mut g = m.write().unwrap_or_else(|p| p.into_inner());
    if g.client_of != now {
        eprintln!(
            "[beacon-cluster] role now {}",
            now.as_deref()
                .map(|s| format!("Client of {s}"))
                .unwrap_or_else(|| "Server".into())
        );
        g.client_of = now;
        // A view from the other role does not carry over.
        g.view = None;
    }
}

pub(crate) fn is_client() -> bool {
    membership()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .client_of
        .is_some()
}

/// The Server's home computation and what it last published.
#[derive(Debug)]
pub struct Coordinator {
    hostname: String,
    tracker: HomeTracker,
    started: i64,
    seeded: bool,
    last_round: Option<i64>,
    /// Topic hub (or `""` for the homes document) → (payload, when).
    published: BTreeMap<String, (String, i64)>,
}

/// One retained publish the coordinator wants.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    View { hub: String, payload: String },
    Homes { payload: String },
}

impl Coordinator {
    pub fn new(hostname: &str, now: i64) -> Coordinator {
        Coordinator {
            hostname: hostname.to_ascii_lowercase(),
            tracker: HomeTracker::default(),
            started: now,
            seeded: false,
            last_round: None,
            published: BTreeMap::new(),
        }
    }

    pub fn on_sensors(&mut self, hub: &str, payload: &[u8], now: i64) {
        self.tracker
            .observe(hub, &sightings_from_payload(payload), now);
    }

    /// Our own retained `eye/homes`, from before a restart.
    pub fn on_homes(&mut self, payload: &[u8]) {
        if let Ok(doc) = serde_json::from_slice::<HomesDoc>(payload) {
            self.tracker.seed(&doc);
            self.seeded = true;
        }
    }

    /// One tick: maybe close a round, then the publishes that are due. Before the first round
    /// it publishes nothing, so a restart never pushes an empty home set over a good one.
    pub fn tick(&mut self, now: i64, registry: &EyeSection, clients: &[String]) -> Vec<Out> {
        let registered: BTreeSet<MacAddr> = registry
            .tags
            .iter()
            .filter_map(|t| MacAddr::parse(&t.mac).ok())
            .collect();
        let round_due = match self.last_round {
            None => self.seeded || now - self.started >= SEED_GRACE_S,
            Some(at) => now - at >= ROUND_S,
        };
        if round_due {
            self.last_round = Some(now);
            for m in self.tracker.close_round(&self.hostname, &registered, now) {
                eprintln!(
                    "[beacon-cluster] {} home {} -> {} ({})",
                    m.mac,
                    m.from.as_deref().unwrap_or("-"),
                    m.to,
                    m.reason.as_str()
                );
            }
        }
        if self.last_round.is_none() {
            return Vec::new();
        }
        let mut hubs: BTreeSet<String> = self.tracker.hubs().cloned().collect();
        hubs.extend(clients.iter().map(|c| c.to_ascii_lowercase()));
        // A home that has not reported since a restart still gets its view.
        hubs.extend(self.tracker.homes().values().cloned());
        hubs.insert(self.hostname.clone());

        let mut out = Vec::new();
        for hub in hubs {
            let view = ClusterView::for_hub(&hub, &self.hostname, registry, self.tracker.homes());
            if hub == self.hostname {
                install_view(membership(), view.clone());
            }
            if let Ok(payload) = serde_json::to_string(&view) {
                if self.due(&hub, &payload, now) {
                    out.push(Out::View { hub, payload });
                }
            }
        }
        let doc = self.tracker.homes_doc(&self.hostname, &registered);
        if let Ok(payload) = serde_json::to_string(&doc) {
            if self.due("", &payload, now) {
                out.push(Out::Homes { payload });
            }
        }
        out
    }

    fn due(&mut self, key: &str, payload: &str, now: i64) -> bool {
        let fresh = self
            .published
            .get(key)
            .is_some_and(|(p, at)| p == payload && now - at < REPUBLISH_S);
        if !fresh {
            self.published
                .insert(key.to_string(), (payload.to_string(), now));
        }
        !fresh
    }

    pub fn homes(&self) -> &BTreeMap<MacAddr, String> {
        self.tracker.homes()
    }
}

fn coordinator_slot() -> &'static Mutex<Option<Coordinator>> {
    static C: OnceLock<Mutex<Option<Coordinator>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

/// Our retained `eye/homes` when it arrived before [`spawn`] made the coordinator.
fn early_homes() -> &'static Mutex<Option<Vec<u8>>> {
    static H: OnceLock<Mutex<Option<Vec<u8>>>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(None))
}

fn take_early_homes(c: &mut Coordinator) {
    let early = early_homes()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take();
    if let Some(payload) = early {
        c.on_homes(&payload);
    }
}

fn with_coordinator<R>(f: impl FnOnce(&mut Coordinator) -> R) -> Option<R> {
    let mut g = coordinator_slot().lock().unwrap_or_else(|p| p.into_inner());
    g.as_mut().map(f)
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The topics this Hub's MQTT monitor subscribes to for the Beacon cluster.
pub fn subscriptions(topics: &crate::libs::mqtt::topics::TopicBuilder) -> Vec<String> {
    if is_client() {
        vec![topics.beacon_cluster()]
    } else {
        vec![
            topics.beacon_sensors_all_hubs(),
            topics.beacon_homes(),
            topics.beacon_cluster(),
        ]
    }
}

/// An incoming publish. `true` when it was one of ours and needs nothing else.
pub fn on_publish(prefix: &str, hostname: &str, topic: &str, payload: &[u8]) -> bool {
    let Some(rest) = topic.strip_prefix(prefix).and_then(|r| r.strip_prefix('/')) else {
        return false;
    };
    let mut parts = rest.split('/');
    let (Some(hub), Some("eye"), Some(kind), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let own = hub.eq_ignore_ascii_case(hostname);
    match kind {
        "sensors" => {
            if !is_client() {
                with_coordinator(|c| c.on_sensors(hub, payload, now_unix()));
            }
            true
        }
        "homes" if own => {
            // The monitor connects before Bluetooth starts the coordinator, so the retained copy
            // usually arrives first: keep it for `spawn`.
            if with_coordinator(|c| c.on_homes(payload)).is_none() {
                *early_homes().lock().unwrap_or_else(|p| p.into_inner()) = Some(payload.to_vec());
            }
            true
        }
        "cluster" if own => {
            // A Client's view comes only from here. A Server sets its own in-process and reads
            // its own retained copy only as a no-op echo.
            if is_client() {
                match serde_json::from_slice::<ClusterView>(payload) {
                    Ok(view) => {
                        if install_view(membership(), view) {
                            eprintln!("[beacon-cluster] view from the Server installed");
                        }
                    }
                    Err(e) => eprintln!("[beacon-cluster] unreadable eye/cluster view: {e}"),
                }
            }
            true
        }
        _ => false,
    }
}

/// The Clients this Server has accounts for — so a Client whose BLE is still idle gets its view.
pub(crate) fn known_clients() -> Vec<String> {
    let files = crate::libs::cluster_clients::BrokerFiles::this_hub();
    let Ok(text) = std::fs::read_to_string(files.passwd_path()) else {
        return Vec::new();
    };
    crate::libs::cluster_clients::PasswdFile::parse(&text)
        .map(|f| {
            f.users()
                .filter_map(|u| {
                    u.strip_prefix(crate::libs::cluster_state::PEER_USERNAME_PREFIX)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Start the coordinator thread. `config` is the BLE host's store (on a Server it loads the
/// local registry).
pub fn spawn(
    hostname: String,
    config: Arc<dyn ConfigStore>,
    mqtt_tx: crossbeam::channel::Sender<MqttMessage>,
) {
    let mut coordinator = Coordinator::new(&hostname, now_unix());
    take_early_homes(&mut coordinator);
    *coordinator_slot().lock().unwrap_or_else(|p| p.into_inner()) = Some(coordinator);
    // Until the first round names the homes, a Server dials nothing: a Beacon homed on a Client
    // must not get a second GATT peer across a restart. The first round replaces this view.
    refresh_role(membership());
    if !is_client() {
        install_view(
            membership(),
            ClusterView::for_hub(
                &hostname,
                &hostname,
                &EyeSection::default(),
                &BTreeMap::new(),
            ),
        );
    }
    let spawned = std::thread::Builder::new()
        .name("beacon-cluster".into())
        .spawn(move || loop {
            std::thread::sleep(TICK);
            refresh_role(membership());
            if is_client() {
                continue;
            }
            let Ok(registry) = config.load() else {
                continue;
            };
            let clients = known_clients();
            let outs =
                with_coordinator(|c| c.tick(now_unix(), &registry, &clients)).unwrap_or_default();
            for out in outs {
                let msg = match out {
                    Out::View { hub, payload } => {
                        MqttMessage::PublishBeaconCluster { hub, payload }
                    }
                    Out::Homes { payload } => MqttMessage::PublishBeaconHomes { payload },
                };
                if mqtt_tx.send(msg).is_err() {
                    return;
                }
            }
        });
    if let Err(e) = spawned {
        eprintln!("[beacon-cluster] not started: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use control_ble::config::EyeTag;

    const A: &str = "7C:D9:F4:00:00:0A";

    fn registry(macs: &[&str]) -> EyeSection {
        EyeSection {
            tags: macs
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
                .collect(),
            ..EyeSection::default()
        }
    }

    fn sensors(rssi: i16, ts: i64) -> Vec<u8> {
        format!(r#"{{"tags":[{{"mac":"{A}","rssi":{rssi},"last_seen_ts":{ts},"stale":false}}]}}"#)
            .into_bytes()
    }

    #[test]
    fn nothing_is_published_before_the_first_round_and_a_seed_starts_it_at_once() {
        let mut c = Coordinator::new("srv", 0);
        assert!(
            c.tick(5, &registry(&[A]), &[]).is_empty(),
            "waiting for the seed"
        );
        c.on_homes(br#"{"v":1,"server":"srv","registered":[],"homes":{"7C:D9:F4:00:00:0A":"c1"}}"#);
        let outs = c.tick(10, &registry(&[A]), &[]);
        assert!(outs.contains(&Out::View {
            hub: "c1".into(),
            payload: serde_json::to_string(&ClusterView::for_hub(
                "c1",
                "srv",
                &registry(&[A]),
                c.homes()
            ))
            .unwrap()
        }));
        assert!(outs.iter().any(|o| matches!(o, Out::Homes { .. })));
    }

    #[test]
    fn views_go_to_every_hub_heard_and_every_client_and_repeat_only_when_changed_or_old() {
        let mut c = Coordinator::new("srv", 0);
        c.on_sensors("C1", &sensors(-50, 1), 1);
        c.on_sensors("srv", &sensors(-80, 1), 1);
        let outs = c.tick(SEED_GRACE_S, &registry(&[A]), &["c2".into()]);
        let hubs: Vec<_> = outs
            .iter()
            .filter_map(|o| match o {
                Out::View { hub, .. } => Some(hub.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(hubs, vec!["c1", "c2", "srv"]);
        assert_eq!(c.homes()[&MacAddr::parse(A).unwrap()], "c1");
        assert!(c
            .tick(SEED_GRACE_S + 5, &registry(&[A]), &["c2".into()])
            .is_empty());
        let later = c.tick(SEED_GRACE_S + REPUBLISH_S, &registry(&[A]), &["c2".into()]);
        assert_eq!(later.len(), 4, "three views and the homes, republished");
    }

    #[test]
    fn a_retained_homes_document_that_beats_the_coordinator_still_seeds_it() {
        let doc = br#"{"v":1,"server":"srv","registered":[],"homes":{"7C:D9:F4:00:00:0A":"c1"}}"#;
        assert!(on_publish("proximos", "srv", "proximos/srv/eye/homes", doc));
        let mut c = Coordinator::new("srv", 0);
        take_early_homes(&mut c);
        let outs = c.tick(5, &registry(&[A]), &[]);
        assert!(!outs.is_empty(), "seeded: the first round runs at once");
        assert_eq!(c.homes()[&MacAddr::parse(A).unwrap()], "c1");
    }

    #[test]
    fn only_our_topics_are_consumed() {
        assert!(!on_publish(
            "proximos",
            "srv",
            "proximos/srv/commands/x",
            b"{}"
        ));
        assert!(!on_publish(
            "proximos",
            "srv",
            "other/c1/eye/sensors",
            b"{}"
        ));
        assert!(on_publish(
            "proximos",
            "srv",
            "proximos/c1/eye/sensors",
            b"{}"
        ));
        assert!(!on_publish(
            "proximos",
            "srv",
            "proximos/c1/eye/homes",
            b"{}"
        ));
        assert!(on_publish(
            "proximos",
            "srv",
            "proximos/srv/eye/cluster",
            b"{}"
        ));
    }
}
