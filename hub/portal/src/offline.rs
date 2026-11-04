//! The per-device offline sweep (plan/portal/devices.backend.md §5, B2).
//!
//! The detector itself — [`crate::notify::offline`] — is pure and takes
//! `now` as an argument. This is the half that touches the world: it wakes
//! on a timer, reads the store snapshot and the `device_meta` overrides,
//! runs one pass, and fans the verdict out three ways.
//!
//! # Why a timer, and why here
//!
//! Nothing else in the process could host this. `persist.rs` is strictly
//! `while let Some(event) = rx.blocking_recv()` — it has no tick at all —
//! and `notify/checkers.rs` has no clock either: the Hub-scoped
//! `DeviceOffline` is a pure function of a retained MQTT status topic.
//! Silence, by definition, arrives as the absence of a message, so it can
//! only be noticed by something that wakes up on its own. The shape follows
//! `backfill::spawn_tasks` — one `tokio::spawn` holding an `Arc<StateStore>`
//! and an `interval` — which is the existing precedent for exactly that.
//!
//! # Three consumers, one verdict
//!
//! A transition has to reach all three or the UI and the mailbox disagree:
//!
//! 1. **the notifier**, as pre-evaluated `SystemAlertEvent`s — this detector
//!    owns its own debounce, so it cannot ride the `SystemSnapshot` path
//!    that re-evaluates from telemetry;
//! 2. **SSE**, as a `ChangeEvent`, or the row never repaints — a verdict
//!    that changed because time passed produces no inbound message, and
//!    that broadcast is the only push channel the templates have;
//! 3. **the render layer**, via [`OfflineService::liveness_snapshot`] — the
//!    fault indicator (§4) has to agree with the email that was just sent,
//!    and since §19 (F2/F3) this is also the portal's *only* answer to "is
//!    this device connected": the Hub's echoed `alarm_state` is consulted
//!    only for a device the sweep holds no verdict on.
//!
//! # Four, since B3: the transition broadcast
//!
//! [`OfflineService::subscribe_transitions`] publishes every verdict —
//! onset *and* recovery — as a [`HubTransition`]. B2 computed the
//! offline→online edge and threw it away: `Transition::CameBack` was
//! constructed and reached nothing (devices.backend.md §16.1). B3's
//! backfill trigger is the first consumer, the monitoring backend's
//! offline/online event writer (monitoring.backend.md, gap H) the second —
//! which is why it is a `broadcast` rather than a channel handed to
//! backfill.
//!
//! `ChangeEvent` was considered and rejected for this: it is the
//! render-repaint signal, deliberately coarse enough that a device reading
//! and a liveness transition are indistinguishable on it, and a subscriber
//! that must not re-arm a backfill on every uplink cannot tell them apart.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tokio::sync::{broadcast, mpsc};

use crate::alarms::notifier::NotifyJob;
use crate::config::{BeaconsConfig, NodesConfig};
use crate::db::Db;
use crate::notify::offline::{
    beacon_liveness, node_liveness, DeviceLiveness, DeviceScope, OfflineDetector,
    OfflineTransition, Transition,
};
use crate::notify::{SystemAlertEvent, SystemAlertKind};
use crate::state::{ChangeEvent, StateStore};

/// How often the sweep wakes.
///
/// One minute against budgets measured in tens of minutes (the shipped
/// defaults are 20 min for a Node, 15 for a Beacon), so the tick contributes
/// at most a minute of lag to a verdict — well inside the noise of the
/// cadence being measured, and far cheaper than a timer per device. The
/// boundary itself is exact regardless: the detector compares timestamps,
/// so a late tick reports the true silence, not the tick's own rounding.
const SWEEP_PERIOD: Duration = Duration::from_secs(60);

/// The shortest gap between two early passes for one Hub (O-4). A reading batch per Node
/// uplink would otherwise run a pass for every uplink while one device stays offline.
const EARLY_PASS_GAP: Duration = Duration::from_secs(5);

/// Transition-broadcast capacity. Generous against the traffic: a sweep
/// emits one message per device that actually *changed* verdict, and a
/// fleet-wide flap is still bounded by the device count. A lagged
/// subscriber loses edges it cannot reconstruct, so the buffer is sized to
/// make that not happen rather than to be recovered from.
const TRANSITION_CHANNEL_CAPACITY: usize = 256;

/// One device's liveness transition, tagged with the Hub it sits behind.
///
/// [`OfflineTransition`] itself carries no hostname — the detector is pure
/// and evaluates one Hub's devices at a time, so it never learns one. Every
/// consumer needs it (a `node_key` is only unique under its Hub), so the
/// service wraps rather than widening the pure type.
#[derive(Debug, Clone, PartialEq)]
pub struct HubTransition {
    pub hostname: String,
    pub transition: OfflineTransition,
}

/// Owns the detector, its schedule, and the shared verdict.
#[derive(Clone)]
pub struct OfflineService {
    db: Db,
    nodes: NodesConfig,
    beacons: BeaconsConfig,
    notify_tx: mpsc::UnboundedSender<NotifyJob>,
    /// Shared with the render layer. `parking_lot`, like `StateStore` — held
    /// only for the length of a set lookup, never across an await.
    detector: Arc<RwLock<OfflineDetector>>,
    /// How often [`Self::run`] wakes. A field rather than a constant so a
    /// test can prove the loop actually ticks in bounded real time: the
    /// sweep awaits two `spawn_blocking` database reads per pass, and a
    /// paused clock cannot deterministically order those against
    /// `tokio::time::advance` — the blocking pool is not on the timer.
    sweep_period: Duration,
    /// The B3 seam (§16.1). Both edges, unlike the notifier fan-out, which
    /// mails only the onset.
    transitions_tx: broadcast::Sender<HubTransition>,
}

impl OfflineService {
    pub fn new(
        db: Db,
        nodes: NodesConfig,
        beacons: BeaconsConfig,
        notify_tx: mpsc::UnboundedSender<NotifyJob>,
    ) -> Self {
        Self {
            db,
            nodes,
            beacons,
            notify_tx,
            detector: Arc::new(RwLock::new(OfflineDetector::new())),
            sweep_period: SWEEP_PERIOD,
            transitions_tx: broadcast::channel(TRANSITION_CHANNEL_CAPACITY).0,
        }
    }

    /// The fleet-wide silence multiplier a Node with no override falls back
    /// to (`nodes.offline_detector`). The Node detail page shows it as the
    /// blank field's placeholder, the way the Beacon page shows
    /// `state.beacons` — the config lives here, so the value is read here
    /// rather than carried a second time on `AppState`.
    pub fn node_offline_k(&self) -> u32 {
        self.nodes.offline_detector
    }

    /// Subscribe to per-device liveness transitions (devices.backend.md
    /// §16.1). Subscribe **before** `spawn_task`: a `broadcast` delivers
    /// only what is sent after the receiver exists.
    pub fn subscribe_transitions(&self) -> broadcast::Receiver<HubTransition> {
        self.transitions_tx.subscribe()
    }

    /// A service with no notifier behind it, for harnesses that need an
    /// `AppState` but no delivery. The receiver is dropped, so every send
    /// fails silently — which is already the production behaviour when the
    /// notifier task has exited, and is why `announce` ignores the result.
    #[cfg(test)]
    pub fn test_instance(db: Db) -> Self {
        let (tx, _rx) = mpsc::unbounded_channel();
        Self::new(db, NodesConfig::default(), BeaconsConfig::default(), tx)
    }

    #[cfg(test)]
    fn with_sweep_period(mut self, period: Duration) -> Self {
        self.sweep_period = period;
        self
    }

    /// Spawns the sweep. Call once from `main` after the store exists; it
    /// runs for the process lifetime.
    pub fn spawn_task(&self, store: Arc<StateStore>) {
        let this = self.clone();
        tokio::spawn(async move { this.run(store).await });
    }

    async fn run(&self, store: Arc<StateStore>) {
        let mut ticker = tokio::time::interval(self.sweep_period);
        // The first tick fires immediately; skipped ticks are dropped rather
        // than burst-replayed, which for a sweep over current state is the
        // only sane behaviour — a machine that was suspended wants one pass,
        // not one per minute it missed.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // O-4 (night test 2026-09-24): a device that reports again closes its offline alarm
        // with that report, not at the next sweep up to a period later. Only a Hub with
        // something offline gets the early pass, and at most once per EARLY_PASS_GAP.
        let mut changes = Some(store.subscribe());
        let mut early: HashMap<String, std::time::Instant> = HashMap::new();
        loop {
            let seen_hub = match changes.as_mut() {
                None => {
                    ticker.tick().await;
                    None
                }
                Some(rx) => tokio::select! {
                    _ = ticker.tick() => None,
                    event = rx.recv() => match event {
                        Ok(ChangeEvent::NodeBatch { hostname }) => Some(hostname),
                        // In a cluster a Beacon is evaluated under the registry (decision #42),
                        // so any Hub hearing it again is an early pass for the Server.
                        Ok(ChangeEvent::BeaconBatch { hostname }) => Some(
                            store
                                .snapshot_beacon_cluster()
                                .map(|c| c.server)
                                .unwrap_or(hostname),
                        ),
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => {
                            changes = None;
                            continue;
                        }
                    },
                },
            };
            let Some(hostname) = seen_hub else {
                self.pass(&store, now_unix()).await;
                continue;
            };
            if !self.detector.read().any_offline(&hostname) {
                continue;
            }
            let key = hostname.to_ascii_lowercase();
            if early
                .get(&key)
                .is_some_and(|at| at.elapsed() < EARLY_PASS_GAP)
            {
                continue;
            }
            early.insert(key, std::time::Instant::now());
            let snapshot = store.snapshot();
            if let Some(device) = snapshot.device(&hostname) {
                self.pass_one_hub(&store, &snapshot, now_unix(), device)
                    .await;
            }
        }
    }

    /// One sweep. `now` is a parameter so a test can place the boundary
    /// exactly instead of inferring it from the wall clock.
    pub async fn pass(&self, store: &StateStore, now: i64) -> Vec<OfflineTransition> {
        let snapshot = store.snapshot();

        let mut all = Vec::new();
        for device in snapshot.devices.values() {
            let transitions = self.pass_one_hub(store, &snapshot, now, device).await;
            all.extend(transitions);
        }
        all
    }

    async fn pass_one_hub(
        &self,
        store: &StateStore,
        snapshot: &crate::state::PortalState,
        now: i64,
        device: &crate::state::DeviceState,
    ) -> Vec<OfflineTransition> {
        let hostname = device.hostname.clone();

        // Two bulk reads per Hub per sweep, not two per device — see
        // `Storage::device_meta_map`. A read failure skips this Hub rather
        // than defaulting the overrides away: inheriting the fleet-wide K
        // for a device an operator explicitly disabled would alarm on it.
        let (h1, h2) = (hostname.clone(), hostname.clone());
        let (node_meta, beacon_meta) = match tokio::try_join!(
            self.db
                .run(move |s| s.device_meta_map(&h1, portal_storage::THRESHOLD_SCOPE_NODE)),
            self.db
                .run(move |s| s.device_meta_map(&h2, portal_storage::THRESHOLD_SCOPE_EYE)),
        ) {
            Ok(pair) => pair,
            Err(err) => {
                tracing::warn!(%err, hostname, "offline sweep skipped: device_meta unreadable");
                return Vec::new();
            }
        };

        let mut devices: Vec<DeviceLiveness> = Vec::new();
        for (node_key, node) in &device.nodes {
            devices.push(node_liveness(
                node_key,
                node.last_seen.as_deref(),
                device.node_configs.get(node_key).map(|e| &e.config),
                node_meta.get(node_key).and_then(|m| m.offline_k_override),
                &self.nodes,
            ));
        }
        // LE-25 (local E2E 2026-09-26): only registered Beacons. A discovered-only candidate,
        // or another Hub's Beacon this one overheard, is nobody's to go offline, and raised a
        // Beacon Offline alarm and e-mail when it stopped being heard.
        match &snapshot.beacon_cluster {
            // Decision #42: a registered Beacon is the cluster's. It is evaluated once, under
            // the registry (the Server), and it is offline only when no Hub hears it: its
            // `last_seen` is the newest any Hub reports. Other Hubs evaluate no Beacon.
            Some(cluster) => {
                if device.hostname.eq_ignore_ascii_case(&cluster.server) {
                    for mac in &cluster.registered {
                        let last_seen = snapshot
                            .devices
                            .values()
                            .filter_map(|d| d.ble_sensors.get(mac))
                            .filter_map(|b| b.last_seen)
                            .max();
                        let meta = beacon_meta.get(mac);
                        devices.push(beacon_liveness(
                            mac,
                            last_seen,
                            meta.and_then(|m| m.offline_k_override),
                            &self.beacons,
                        ));
                    }
                }
            }
            None => {
                for (mac, beacon) in device
                    .ble_sensors
                    .iter()
                    .filter(|(_, b)| b.discovered != Some(true))
                {
                    let meta = beacon_meta.get(mac);
                    devices.push(beacon_liveness(
                        mac,
                        beacon.last_seen,
                        meta.and_then(|m| m.offline_k_override),
                        &self.beacons,
                    ));
                }
            }
        }

        let transitions = self.detector.write().evaluate(&hostname, now, &devices);
        if transitions.is_empty() {
            return transitions;
        }

        self.announce(store, &hostname, &transitions);
        transitions
    }

    /// Fans one Hub's transitions out to SSE, the notifier, and the
    /// transition broadcast.
    fn announce(&self, store: &StateStore, hostname: &str, transitions: &[OfflineTransition]) {
        let mut events = Vec::new();

        for t in transitions {
            // Both edges go on the broadcast — a recovery is precisely what
            // B3's trigger is waiting for. No subscribers is normal (the
            // seam is optional wiring), so the result is ignored, same as
            // every other fan-out here.
            let sent = self.transitions_tx.send(HubTransition {
                hostname: hostname.to_string(),
                transition: t.clone(),
            });
            // The seam has no other trace: a subscriber that never runs and
            // a transition that never fires look identical from the outside,
            // which cost an afternoon once. `Err` means nobody is
            // subscribed, which is normal.
            tracing::debug!(
                hostname,
                entity = %t.entity,
                scope = ?t.scope,
                transition = ?t.transition,
                subscribers = sent.unwrap_or(0),
                "liveness transition published"
            );

            // Repaint the row. Without this the operator sees a stale
            // "online" until something unrelated happens to publish.
            store.emit(match t.scope {
                DeviceScope::Node => ChangeEvent::Node {
                    hostname: hostname.to_string(),
                    node_key: t.entity.clone(),
                },
                DeviceScope::Beacon => ChangeEvent::Beacon {
                    hostname: hostname.to_string(),
                    mac: t.entity.clone(),
                },
            });

            // Only the onset is mailed. A recovery repaints the row but does
            // not send a second email — v1 parity, and the same stance
            // `SystemAlertState` takes (clearing re-arms, it does not
            // announce).
            if t.transition == Transition::WentOffline {
                let (kind, key) = match t.scope {
                    DeviceScope::Node => (SystemAlertKind::NodeOffline, "node_offline"),
                    DeviceScope::Beacon => (SystemAlertKind::BeaconOffline, "beacon_offline"),
                };
                events.push(SystemAlertEvent::new(
                    kind,
                    offline_message(t),
                    key,
                    vec![
                        ("entity", t.entity.clone()),
                        ("minutes", (t.silent_for_s / 60).to_string()),
                    ],
                ));
            }
        }

        if !events.is_empty() {
            let _ = self.notify_tx.send(NotifyJob::SystemAlerts {
                hostname: hostname.to_string(),
                events,
            });
        }
    }

    /// The liveness verdict table (devices.backend.md §19, F2/F3) — the one
    /// read the render layer takes off this service.
    ///
    /// It replaced a plain offline *set* (`offline_snapshot`, deleted in
    /// review round 2 once nothing called it): anything that renders
    /// *connectivity* rather than only the fault triangle needs the third
    /// answer. The detector, not the Hub's echoed `alarm_state`, is the
    /// portal's one answer to "is this device alive", and a set cannot say
    /// "I do not know yet" — the only case in which the echo is still
    /// consulted.
    pub fn liveness_snapshot(&self) -> crate::notify::offline::LivenessVerdicts {
        self.detector.read().verdicts()
    }
}

/// The human sentence an outage carries — the notifier's system-alert body,
/// and since MB3 also the `alarm_events.message` of the liveness row
/// (`alarms::AlarmService::spawn_liveness_writer`). Shared rather than
/// re-worded there, so the email and the probe window say the same thing.
pub(crate) fn offline_message(t: &OfflineTransition) -> String {
    let kind = match t.scope {
        DeviceScope::Node => "Node",
        DeviceScope::Beacon => "Beacon",
    };
    let minutes = t.silent_for_s / 60;
    format!("{kind} {} has not reported for {minutes} minutes", t.entity)
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::events::{IngestEvent, NodeReadingMsg};

    /// 2026-01-13T14:30:00Z, the timestamp `persist`'s own RFC3339 test pins.
    const SEEN_AT: i64 = 1_768_314_600;
    const SEEN_AT_RFC3339: &str = "2026-01-13T14:30:00Z";

    fn reading() -> NodeReadingMsg {
        reading_at(SEEN_AT_RFC3339)
    }

    fn reading_at(received_at: &str) -> NodeReadingMsg {
        NodeReadingMsg {
            message_id: "m-1".into(),
            dev_eui: "aa11".into(),
            serial: Some("SN-1".into()),
            fields: [("temperature".to_string(), 21.5)].into_iter().collect(),
            counters: Default::default(),
            events: Vec::new(),
            gateways: Vec::new(),
            rssi: None,
            snr: None,
            fport: None,
            page_index: None,
            page_count: None,
            dr: None,
            received_at: received_at.to_string(),
        }
    }

    /// A store holding one Node last heard from at [`SEEN_AT`].
    fn store_with_a_node() -> Arc<StateStore> {
        let store = Arc::new(StateStore::new());
        store.apply(IngestEvent::NodeReading {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: Box::new(reading()),
        });
        store
    }

    fn service(dir: &tempfile::TempDir) -> OfflineService {
        let data_dir = dir.path().to_str().expect("utf8 path");
        OfflineService::test_instance(Db::open(data_dir, None, "test").expect("open db"))
    }

    /// The wiring, end to end through a real store and a real database: a
    /// Node past its budget raises exactly once, the verdict is readable
    /// where the renderer reads it, and — the part nothing else would catch
    /// — an SSE `ChangeEvent` goes out so the row actually repaints.
    #[tokio::test]
    async fn a_silent_node_raises_once_and_repaints_its_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let svc = service(&dir);
        let store = store_with_a_node();
        let mut changes = store.subscribe();

        // No config for this Node, so the 15-minute fallback applies:
        // 4 x 900 s = 3600 s of budget.
        let transitions = svc.pass(&store, SEEN_AT + 3_601).await;

        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].transition, Transition::WentOffline);
        assert_eq!(transitions[0].entity, "aa11");
        assert_eq!(
            svc.liveness_snapshot().verdict(DeviceScope::Node, "aa11"),
            Some(true)
        );

        let event = changes.try_recv().expect("a change event was broadcast");
        assert_eq!(
            event,
            ChangeEvent::Node {
                hostname: "h1".into(),
                node_key: "aa11".into()
            }
        );

        // Second pass, still silent: no repeat event and no second repaint.
        assert!(svc.pass(&store, SEEN_AT + 7_200).await.is_empty());
        assert!(changes.try_recv().is_err(), "no repeat repaint");
    }

    /// One second inside the budget is not offline. The boundary itself is
    /// covered exhaustively in `notify::offline`; this asserts the *wiring*
    /// carries the same `now` through unshifted — an off-by-a-tick in the
    /// service would not show up in the pure tests.
    #[tokio::test]
    async fn a_node_inside_its_budget_raises_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let svc = service(&dir);
        let store = store_with_a_node();

        assert!(svc.pass(&store, SEEN_AT + 3_600).await.is_empty());
        assert_eq!(
            svc.liveness_snapshot().verdict(DeviceScope::Node, "aa11"),
            Some(false)
        );
    }

    /// A `device_meta` override of 0 switches this one Node off while the
    /// fleet default stays at 4 — the whole reason the column is nullable.
    #[tokio::test]
    async fn a_per_device_override_of_zero_suppresses_the_alert() {
        let dir = tempfile::tempdir().expect("tempdir");
        let svc = service(&dir);
        let store = store_with_a_node();

        ({
            let a0 = portal_storage::NewDeviceMeta {
                offline_k_override: Some(0),
                ..portal_storage::NewDeviceMeta::new(
                    "h1",
                    portal_storage::THRESHOLD_SCOPE_NODE,
                    "aa11",
                )
            };
            svc.db.write(move |s| s.upsert_device_meta(&a0))
        })
        .await
        .expect("upsert device_meta");

        assert!(svc.pass(&store, SEEN_AT + 999_999).await.is_empty());
        assert_eq!(
            svc.liveness_snapshot().verdict(DeviceScope::Node, "aa11"),
            Some(false),
            "no alert and no fault, but a verdict all the same: the render layer is \
             told the device is alive rather than being sent to the Hub's echo, which \
             reads `NeverConnected` forever for a v2-envelope Node (§19.1, F3)"
        );
    }

    /// B3's seam (§16.1): a subscriber sees **both** edges. B2 computed
    /// `CameBack` and threw it away, and the notifier fan-out beside this
    /// one still mails only the onset — so "the recovery reaches a
    /// subscriber" is the single claim the backfill trigger rests on.
    #[tokio::test]
    async fn a_subscriber_sees_both_edges() {
        let dir = tempfile::tempdir().expect("tempdir");
        let svc = service(&dir);
        let store = store_with_a_node();
        let mut transitions = svc.subscribe_transitions();

        // Past its 4 x 900 s budget → onset.
        svc.pass(&store, SEEN_AT + 3_601).await;
        let onset = transitions.try_recv().expect("onset on the broadcast");
        assert_eq!(onset.hostname, "h1");
        assert_eq!(onset.transition.transition, Transition::WentOffline);
        assert_eq!(onset.transition.entity, "aa11");
        assert_eq!(onset.transition.scope, DeviceScope::Node);

        // One fresh uplink an hour later, then a sweep inside the new
        // budget → recovery, on the same channel.
        store.apply(IngestEvent::NodeReading {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: Box::new(reading_at("2026-01-13T15:30:00Z")),
        });
        svc.pass(&store, SEEN_AT + 3_700).await;

        let recovery = transitions.try_recv().expect("recovery on the broadcast");
        assert_eq!(recovery.hostname, "h1");
        assert_eq!(recovery.transition.transition, Transition::CameBack);
        assert_eq!(recovery.transition.entity, "aa11");
        assert_eq!(
            svc.liveness_snapshot().verdict(DeviceScope::Node, "aa11"),
            Some(false)
        );

        // Steady state publishes nothing: the broadcast carries verdict
        // *changes*, which is what stops a subscriber re-arming a backfill
        // on every uplink (the reason `ChangeEvent` was rejected).
        svc.pass(&store, SEEN_AT + 3_800).await;
        assert!(transitions.try_recv().is_err(), "no transition, no message");
    }

    /// A late subscriber is the wiring bug `main.rs`'s ordering comment
    /// guards against, so the seam must be usable with nobody listening.
    #[tokio::test]
    async fn a_transition_with_no_subscriber_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let svc = service(&dir);
        let store = store_with_a_node();

        let transitions = svc.pass(&store, SEEN_AT + 3_601).await;
        assert_eq!(transitions.len(), 1, "the sweep still does its own work");
        assert_eq!(
            svc.liveness_snapshot().verdict(DeviceScope::Node, "aa11"),
            Some(true)
        );
    }

    /// O-4: a Node that reports again comes back with that report, not at the next sweep.
    /// The sweep period is an hour here, so only the early pass can produce the transition.
    #[tokio::test]
    async fn a_reading_brings_an_offline_node_back_at_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let svc = service(&dir).with_sweep_period(Duration::from_secs(3_600));
        let store = store_with_a_node();
        let mut transitions = svc.subscribe_transitions();
        // The first tick fires at once: a Node last heard from in January is offline.
        svc.spawn_task(Arc::clone(&store));
        let went = tokio::time::timeout(Duration::from_secs(5), transitions.recv())
            .await
            .expect("the first sweep ran")
            .expect("open");
        assert_eq!(went.transition.transition, Transition::WentOffline);

        let now = crate::render::unix_to_rfc3339(now_unix()).expect("now formats");
        store.apply(IngestEvent::NodeReading {
            hostname: "h1".into(),
            node_key: "aa11".into(),
            payload: Box::new(reading_at(&now)),
        });
        let back = tokio::time::timeout(Duration::from_secs(5), transitions.recv())
            .await
            .expect("the reading brought the Node back before the next sweep")
            .expect("open");
        assert_eq!(back.transition.transition, Transition::CameBack);
    }

    /// LE-25: a discovered-only Beacon (a candidate, or another Hub's tag overheard) never goes
    /// offline here; only a registered one does.
    #[tokio::test]
    async fn only_a_registered_beacon_goes_offline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let svc = service(&dir);
        let store = Arc::new(StateStore::new());
        let payload: fiber_protocol::payloads::eye::EyeSensorsMsg =
            serde_json::from_value(serde_json::json!({
                "tags": [
                    { "mac": "EE:E0:00:00:02:01", "last_seen_ts": SEEN_AT },
                    { "mac": "EE:E0:00:00:02:99", "last_seen_ts": SEEN_AT, "discovered": true },
                ]
            }))
            .expect("eye payload");
        store.apply(IngestEvent::EyeSensors {
            hostname: "h1".into(),
            payload,
        });
        let transitions = svc.pass(&store, SEEN_AT + 86_400).await;
        let offline: Vec<&str> = transitions.iter().map(|t| t.entity.as_str()).collect();
        assert_eq!(
            offline,
            vec!["EE:E0:00:00:02:01"],
            "the candidate is not reported"
        );
    }

    /// Decision #42: in a cluster a registered Beacon goes offline only when no Hub hears it,
    /// and once — under the registry (the Server), not under each Hub.
    #[tokio::test]
    async fn a_cluster_beacon_is_offline_only_when_no_hub_hears_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let svc = service(&dir);
        let store = Arc::new(StateStore::new());
        const MAC: &str = "EE:E0:00:00:02:01";
        let heard = |host: &str, at: i64| {
            store.apply(IngestEvent::EyeSensors {
                hostname: host.into(),
                payload: serde_json::from_value(serde_json::json!({
                    "tags": [{ "mac": MAC, "last_seen_ts": at }]
                }))
                .expect("eye payload"),
            });
        };
        heard("srv", SEEN_AT);
        heard("c1", SEEN_AT);
        store.apply(IngestEvent::EyeHomes {
            hostname: "srv".into(),
            payload: fiber_protocol::payloads::eye::EyeHomesMsg {
                v: 1,
                server: "srv".into(),
                registered: vec![MAC.into()],
                homes: [(MAC.to_string(), "c1".to_string())].into(),
            },
        });
        // The store stamps `last_seen` with its own clock: the Server stopped hearing it long
        // ago, the Client hears it now.
        let late = SEEN_AT + 86_400;
        let now = |store: &StateStore| {
            store
                .snapshot()
                .devices
                .values()
                .filter_map(|d| d.ble_sensors.get(MAC))
                .filter_map(|b| b.last_seen)
                .max()
                .expect("seen")
        };
        let t = now(&store);
        assert!(
            svc.pass(&store, t + 60).await.is_empty(),
            "heard by a Hub: online"
        );
        let transitions = svc.pass(&store, late.max(t + 86_400)).await;
        let offline: Vec<&str> = transitions.iter().map(|t| t.entity.as_str()).collect();
        assert_eq!(offline, vec![MAC], "once, not per Hub");
    }

    /// The sweep runs itself. Everything above calls `pass` directly, which
    /// proves the arithmetic and the fan-out but not that anything ever
    /// *invokes* it — a service whose `spawn_task` never ticked would pass
    /// every other test in this file.
    ///
    /// Real time, not a paused clock, and deliberately: a pass awaits two
    /// `spawn_blocking` database reads, which run on the blocking pool and
    /// are therefore not ordered by `tokio::time::advance`. Waiting on the
    /// signal the sweep actually produces — the SSE repaint — is
    /// deterministic where advancing a clock past a blocking read is not.
    /// The period is shortened to keep it fast; the timer under test is the
    /// same one.
    #[tokio::test]
    async fn the_spawned_sweep_ticks_on_its_own() {
        let dir = tempfile::tempdir().expect("tempdir");
        let svc = service(&dir).with_sweep_period(Duration::from_millis(10));

        // `run` compares against the wall clock, so a Node last heard from
        // in 2026 is far past any budget the moment the task wakes.
        let store = store_with_a_node();
        let mut changes = store.subscribe();

        svc.spawn_task(Arc::clone(&store));

        let event = tokio::time::timeout(Duration::from_secs(5), changes.recv())
            .await
            .expect("the spawned sweep never ran a pass")
            .expect("change channel stayed open");

        assert_eq!(
            event,
            ChangeEvent::Node {
                hostname: "h1".into(),
                node_key: "aa11".into()
            }
        );
        assert_eq!(
            svc.liveness_snapshot().verdict(DeviceScope::Node, "aa11"),
            Some(true)
        );
    }
}
