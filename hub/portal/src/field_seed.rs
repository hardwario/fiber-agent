//! Refills each Node's last field values and sensor counts after a portal restart (N14,
//! combined23; counts GitLab #134).
//!
//! `NodeState::fields` is in-memory and only an uplink filled it, so after a restart the Node
//! detail showed "–" for Current and Measurements for up to a report interval, beside a chart
//! that had the data. The retained inventory document that re-creates the Node on reconnect
//! says nothing about values; storage does.
//!
//! This task watches the store for a Node appearing (`NodeAdded`, `NodeConfig` — what the
//! inventory replay raises) and, once per Node, reads each field's newest stored value into a
//! Node whose map is still empty ([`StateStore::seed_node_fields`]). A field whose newest
//! report was `null` is not refilled; it is marked so again
//! ([`StateStore::seed_node_no_data`]). No ingest event: nothing is persisted twice and no
//! alarm is evaluated on an old value.
//!
//! The signal (RSSI, SNR, the gateways that heard the Node) is in-memory the same way, so the
//! signal bars read "unknown" after a restart. The same pass takes it from the newest data frame
//! in the raw message ring ([`StateStore::seed_node_signal`]).

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;

use crate::db::Db;
use crate::state::{ChangeEvent, NodeGatewayLink, StateStore};

/// Ten raw rows are kept per port and a Node uses at most four port groups (2, 3, 85, none),
/// so this many always reaches the newest data frame.
const RAW_RING_READ: i64 = 50;

/// Subscribes before it returns, so a caller that spawns this ahead of the MQTT ingest cannot
/// lose the inventory replay to a task that has not started yet.
pub fn spawn(store: Arc<StateStore>, db: Db) {
    let changes = store.subscribe();
    tokio::spawn(run(store, db, changes));
}

async fn run(
    store: Arc<StateStore>,
    db: Db,
    mut changes: tokio::sync::broadcast::Receiver<ChangeEvent>,
) {
    // Nodes already looked up. A Node with no stored reading is looked up once, not on every
    // later config event; a failed read stays out of the set and is retried on the next one.
    let mut seeded: HashSet<(String, String)> = HashSet::new();
    loop {
        let (hostname, node_key) = match changes.recv().await {
            Ok(ChangeEvent::NodeAdded { hostname, node_key })
            | Ok(ChangeEvent::NodeConfig { hostname, node_key }) => (hostname, node_key),
            Ok(_) | Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
        };
        let key = (hostname.to_ascii_lowercase(), node_key.clone());
        if seeded.contains(&key) {
            continue;
        }
        // Values or signal still missing; each half is seeded only where it is missing.
        let needs_seed = store.snapshot_device(&hostname).and_then(|d| {
            d.nodes.get(&node_key).map(|n| {
                n.fields.is_empty()
                    || (n.rssi.is_none() && n.snr.is_none() && n.gateways.is_empty())
            })
        });
        match needs_seed {
            // Not in the store (a tombstone), nothing to fill.
            None => continue,
            // A live reading got there first.
            Some(false) => {
                seeded.insert(key);
                continue;
            }
            Some(true) => {}
        }
        // Storage keys a Hub under its display spelling, not the lowercase one on the v2 topics
        // (`persist::HostnameCanon`).
        let storage_host = store
            .display_hostname(&hostname)
            .unwrap_or_else(|| hostname.clone());
        let lookup = node_key.clone();
        match db
            .run(move |s| {
                let fields = s.latest_node_fields(&storage_host, &lookup)?;
                let counters = s.latest_node_counters(&storage_host, &lookup)?;
                let nulled = s.nulled_node_fields(&storage_host, &lookup)?;
                let ring = s.list_raw_messages(&storage_host, &lookup, Some(RAW_RING_READ))?;
                Ok((fields, counters, nulled, ring))
            })
            .await
        {
            Ok((fields, counters, nulled, ring)) => {
                // Values first: a seeded `null` mark stops a later value seed.
                store.seed_node_fields(&hostname, &node_key, &fields);
                store.seed_node_counters(&hostname, &node_key, &counters);
                store.seed_node_no_data(&hostname, &node_key, &nulled);
                // Newest first, so the first frame with a signal is the last one heard.
                if let Some(signal) = ring.iter().find_map(|m| last_signal(&m.payload)) {
                    store.seed_node_signal(
                        &hostname,
                        &node_key,
                        signal.rssi,
                        signal.snr,
                        signal.gateways,
                    );
                }
                seeded.insert(key);
            }
            Err(err) => {
                tracing::warn!(%err, %hostname, %node_key, "could not read a Node's last values");
            }
        }
    }
}

/// The signal one stored raw frame carried.
struct LastSignal {
    rssi: Option<i64>,
    snr: Option<f64>,
    gateways: Vec<NodeGatewayLink>,
}

/// The signal of one raw ring row. fPort 2 and 3 are the readings and alarms the live path
/// takes the signal from; a row with no port is a v1 sensor message with the same keys; fPort
/// 85 is command traffic and carries none. `None` when the row has no signal at all.
fn last_signal(payload: &str) -> Option<LastSignal> {
    let doc: Value = serde_json::from_str(payload).ok()?;
    if !matches!(doc.get("fport").and_then(Value::as_i64), Some(2 | 3) | None) {
        return None;
    }
    let gateways: Vec<NodeGatewayLink> = doc
        .get("gateways")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|g| NodeGatewayLink {
                    gateway_id: g
                        .get("gateway_id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    rssi: g.get("rssi").and_then(Value::as_i64),
                    snr: g.get("snr").and_then(Value::as_f64),
                })
                .collect()
        })
        .unwrap_or_default();
    let rssi = doc.get("rssi").and_then(Value::as_i64);
    let snr = doc.get("snr").and_then(Value::as_f64);
    (rssi.is_some() || snr.is_some() || !gateways.is_empty()).then_some(LastSignal {
        rssi,
        snr,
        gateways,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::IngestEvent;
    use portal_storage::{NodeFieldReading, Storage, SOURCE_LIVE};

    fn reading(host: &str, field: &str, ts: i64, value: f64) -> NodeFieldReading {
        NodeFieldReading {
            hostname: host.into(),
            node_key: "dd44".into(),
            field: field.into(),
            ts,
            value: Some(value),
            alarm_state: None,
            source: SOURCE_LIVE.into(),
        }
    }

    fn inventory() -> IngestEvent {
        IngestEvent::NodeInventory {
            hostname: "h1".into(),
            node_key: "dd44".into(),
            payload: serde_json::json!({
                "dev_eui": "dd44", "transport": "lorawan", "last_seen": "2026-09-24T17:21:19Z",
            }),
        }
    }

    async fn fields_after_settle(store: &StateStore) -> std::collections::BTreeMap<String, f64> {
        for _ in 0..50 {
            let fields = store
                .snapshot_device("h1")
                .and_then(|d| d.nodes.get("dd44").map(|n| n.fields.clone()))
                .unwrap_or_default();
            if !fields.is_empty() {
                return fields;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        Default::default()
    }

    /// N14: the inventory replay re-creates the Node with no values; the task fills in each
    /// field's newest stored value.
    #[tokio::test]
    async fn a_replayed_node_gets_its_last_values_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Storage::open(&dir.path().join("portal.db"), None).expect("open");
        storage
            .insert_field_readings(&[
                reading("h1", "temperature", 1000, 30.0),
                reading("h1", "temperature", 1900, 31.4),
                reading("h1", "humidity", 1900, 58.5),
            ])
            .expect("insert");
        let store = Arc::new(StateStore::new());
        spawn(Arc::clone(&store), Db::from_storage(storage, "test"));

        store.apply(inventory());

        let fields = fields_after_settle(&store).await;
        assert_eq!(fields.get("temperature"), Some(&31.4));
        assert_eq!(fields.get("humidity"), Some(&58.5));
    }

    /// GitLab #134: a count row is in `counters`, so its last value comes back from
    /// `node_counter_readings`, 0 included; a frame counter does not.
    #[tokio::test]
    async fn a_replayed_node_gets_its_last_counts_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Storage::open(&dir.path().join("portal.db"), None).expect("open");
        storage
            .insert_field_readings(&[reading("h1", "temperature", 1900, 22.0)])
            .expect("insert");
        let count = |counter: &str, ts: i64, value: i64| portal_storage::NodeCounterReading {
            hostname: "h1".into(),
            node_key: "dd44".into(),
            counter: counter.into(),
            ts,
            value: Some(value),
        };
        storage
            .insert_counter_readings(&[
                count("motion_count", 1000, 4),
                count("motion_count", 1900, 0),
                count("input_b_count", 1900, 7),
                count("counter", 1900, 812),
            ])
            .expect("insert");
        let store = Arc::new(StateStore::new());
        spawn(Arc::clone(&store), Db::from_storage(storage, "test"));

        store.apply(inventory());

        fields_after_settle(&store).await;
        let mut counters = Default::default();
        for _ in 0..50 {
            counters = store
                .snapshot_device("h1")
                .and_then(|d| d.nodes.get("dd44").map(|n| n.counters.clone()))
                .unwrap_or_default();
            if !counters.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(counters.get("motion_count"), Some(&0));
        assert_eq!(counters.get("input_b_count"), Some(&7));
        assert!(!counters.contains_key("counter"), "no stale frame counter");
    }

    /// GitLab #134 × #120: a switched-off count sensor is parked like a measurement, and
    /// switched on goes back to `counters`; a seed never overwrites a live count.
    #[test]
    fn a_switched_off_count_is_parked_and_comes_back_as_a_count() {
        let store = StateStore::new();
        store.load_disabled_sensors([(
            "H1".to_string(),
            "node".to_string(),
            "dd44".to_string(),
            "motion_count".to_string(),
        )]);
        store.apply(inventory());
        assert!(store.seed_node_counters(
            "h1",
            "dd44",
            &[
                ("motion_count".to_string(), 3),
                ("input_a_count".to_string(), 0)
            ],
        ));
        let node = store.snapshot_device("h1").unwrap().nodes["dd44"].clone();
        assert!(!node.counters.contains_key("motion_count"));
        assert_eq!(node.disabled_fields.get("motion_count"), Some(&Some(3.0)));
        assert_eq!(node.counters.get("input_a_count"), Some(&0));

        store.set_sensor_disabled("h1", "node", "dd44", "motion_count", false);
        let node = store.snapshot_device("h1").unwrap().nodes["dd44"].clone();
        assert_eq!(node.counters.get("motion_count"), Some(&3));
        assert!(!node.fields.contains_key("motion_count"));

        assert!(
            !store.seed_node_counters("h1", "dd44", &[("motion_count".to_string(), 9)]),
            "a held count is left alone"
        );
        let node = store.snapshot_device("h1").unwrap().nodes["dd44"].clone();
        assert_eq!(node.counters.get("motion_count"), Some(&3));
    }

    /// GitLab #120: a switched-off sensor's value is parked out of `fields`, whether it was
    /// there before the switch or arrives after it, and comes back when it is switched on.
    #[test]
    fn a_switched_off_sensor_is_parked_and_comes_back_on() {
        let store = StateStore::new();
        store.load_disabled_sensors([(
            "H1".to_string(),
            "node".to_string(),
            "dd44".to_string(),
            "humidity".to_string(),
        )]);
        store.apply(inventory());
        let seed = [
            ("temperature".to_string(), 20.0),
            ("humidity".to_string(), 55.0),
        ];
        assert!(store.seed_node_fields("h1", "dd44", &seed));
        let node = store.snapshot_device("h1").unwrap().nodes["dd44"].clone();
        assert_eq!(node.fields.get("temperature"), Some(&20.0));
        assert!(!node.fields.contains_key("humidity"));
        assert_eq!(node.disabled_fields.get("humidity"), Some(&Some(55.0)));
        assert!(store.sensor_disabled("h1", "node", "dd44", "humidity"));

        store.set_sensor_disabled("h1", "node", "dd44", "temperature", true);
        let node = store.snapshot_device("h1").unwrap().nodes["dd44"].clone();
        assert!(node.fields.is_empty());
        assert_eq!(node.disabled_fields.get("temperature"), Some(&Some(20.0)));

        store.set_sensor_disabled("h1", "node", "dd44", "humidity", false);
        let node = store.snapshot_device("h1").unwrap().nodes["dd44"].clone();
        assert_eq!(node.fields.get("humidity"), Some(&55.0));
        assert!(!node.disabled_fields.contains_key("humidity"));

        store.forget_disabled_sensors("h1", "node", "dd44");
        assert!(!store.sensor_disabled("h1", "node", "dd44", "temperature"));
    }

    /// A live value is newer than anything in storage, so the seed never writes over it.
    #[test]
    fn a_seed_never_overwrites_a_live_value() {
        let store = StateStore::new();
        store.apply(inventory());
        let old = [("temperature".to_string(), 20.0)];
        assert!(
            store.seed_node_fields("h1", "dd44", &old),
            "an empty map is filled"
        );

        let newer = [
            ("temperature".to_string(), 25.0),
            ("humidity".to_string(), 40.0),
        ];
        assert!(
            !store.seed_node_fields("h1", "dd44", &newer),
            "a filled map is left alone"
        );
        let node = store.snapshot_device("h1").unwrap().nodes["dd44"].clone();
        assert_eq!(node.fields.get("temperature"), Some(&20.0));
        assert!(!node.fields.contains_key("humidity"));

        assert!(
            !store.seed_node_fields("h1", "zz99", &old),
            "a Node the store does not know is skipped"
        );
    }

    fn gateway(id: &str, rssi: i64, snr: f64) -> NodeGatewayLink {
        NodeGatewayLink {
            gateway_id: Some(id.into()),
            rssi: Some(rssi),
            snr: Some(snr),
        }
    }

    async fn signal_after_settle(store: &StateStore) -> Option<crate::state::NodeState> {
        for _ in 0..50 {
            let node = store
                .snapshot_device("h1")
                .and_then(|d| d.nodes.get("dd44").cloned());
            if node.as_ref().is_some_and(|n| n.rssi.is_some()) {
                return node;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        None
    }

    /// The inventory replay re-creates the Node with no signal; the task takes it from the
    /// newest data frame in the raw ring, skipping command traffic, and leaves `last_seen` as
    /// the inventory set it.
    #[tokio::test]
    async fn a_replayed_node_gets_its_last_signal_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Storage::open(&dir.path().join("portal.db"), None).expect("open");
        for (received_at, payload) in [
            (
                1000,
                r#"{"fport":2,"rssi":-110,"snr":-5.0,
                    "gateways":[{"gateway_id":"old","rssi":-110,"snr":-5.0}]}"#,
            ),
            (
                1900,
                r#"{"fport":2,"rssi":-99,"snr":0.0,
                    "gateways":[{"gateway_id":"quiet","rssi":-87,"snr":4.25},
                                {"gateway_id":"loud","rssi":-41,"snr":13.5}]}"#,
            ),
            (2000, r#"{"fport":85,"kind":"answer"}"#),
        ] {
            storage
                .insert_raw_message("h1", "dd44", received_at, payload)
                .expect("insert");
        }
        let store = Arc::new(StateStore::new());
        spawn(Arc::clone(&store), Db::from_storage(storage, "test"));

        store.apply(inventory());

        let node = signal_after_settle(&store).await.expect("signal seeded");
        assert_eq!(
            node.rssi,
            Some(-41),
            "the loudest gateway of the newest data frame"
        );
        assert_eq!(node.snr, Some(13.5));
        assert_eq!(node.gateways.len(), 2);
        assert!(node
            .gateways
            .iter()
            .any(|g| g.gateway_id.as_deref() == Some("loud")));
        assert_eq!(node.last_seen.as_deref(), Some("2026-09-24T17:21:19Z"));
    }

    /// A live signal is newer than anything in storage, so the seed never writes over it.
    #[test]
    fn a_seed_never_overwrites_a_live_signal() {
        let store = StateStore::new();
        store.apply(inventory());
        assert!(
            store.seed_node_signal(
                "h1",
                "dd44",
                Some(-99),
                Some(0.0),
                vec![gateway("loud", -41, 13.5), gateway("quiet", -87, 4.25)],
            ),
            "a Node with no signal is filled"
        );
        let node = store.snapshot_device("h1").unwrap().nodes["dd44"].clone();
        assert_eq!(node.rssi, Some(-41));
        assert_eq!(node.snr, Some(13.5));

        assert!(
            !store.seed_node_signal(
                "h1",
                "dd44",
                Some(-120),
                None,
                vec![gateway("x", -120, -9.0)]
            ),
            "a Node with a signal is left alone"
        );
        let node = store.snapshot_device("h1").unwrap().nodes["dd44"].clone();
        assert_eq!(node.rssi, Some(-41));
        assert_eq!(node.gateways.len(), 2);

        assert!(
            !store.seed_node_signal("h1", "zz99", Some(-50), None, Vec::new()),
            "a Node the store does not know is skipped"
        );
    }

    #[test]
    fn last_signal_reads_ports_2_3_and_unported_rows_only() {
        assert!(
            last_signal(r#"{"fport":85,"rssi":-50,"snr":1.0}"#).is_none(),
            "command traffic"
        );
        let alarm = last_signal(r#"{"fport":3,"rssi":-60,"snr":2.0,"gateways":[]}"#)
            .expect("an alarm frame has a signal");
        assert_eq!(alarm.rssi, Some(-60));
        let v1 = last_signal(
            r#"{"rssi":-70,"snr":3.0,"gateways":[{"gateway_id":null,"rssi":-70,"snr":3.0}]}"#,
        )
        .expect("a v1 row has no port");
        assert_eq!(v1.gateways.len(), 1);
        assert!(v1.gateways[0].gateway_id.is_none());
        assert!(
            last_signal(r#"{"fport":2,"fields":{}}"#).is_none(),
            "no signal at all"
        );
        assert!(last_signal("not json").is_none());
    }
}
