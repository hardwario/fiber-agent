//! The one broker connection — subscriptions in, northbound publishes out.
//!
//! `plan/control/radio/plan.md`, "Runtime form": *"the radio tasks ride `proximos-control`'s
//! one broker client — the dedicated `control-radio` account dissolves with the daemon."* So
//! there is a single connection here carrying both subscriptions and every publish the uplink
//! path produces.
//!
//! The **async** rumqttc client, not the sync one `control_radio::mqtt` uses: that module opens
//! a connection per command and blocks for its PUBACKs, which is right for a CLI that lives for
//! milliseconds and wrong for a subscriber that has to stay connected for months.
//!
//! # Waiting for `radio-setup`
//!
//! The uplink filter needs an application id, and that id only exists once `create` has
//! converged (`lorawan.md` §5). `radio-setup.service` is ordered `Before=` this unit, but its
//! own comment is explicit that this is *ordering, not a dependency* — the radio must be able
//! to be absent without the agent failing to start. On a first boot where `create` is still
//! retrying against a ChirpStack that is migrating its database, this process is already up
//! with no `config.json` to read. [`run`] therefore retries with backoff instead of failing,
//! and says so once rather than every second.
//!
//! # Replaying the retained documents
//!
//! The Hub's mosquitto keeps no retained state across a restart, and a Node's `device_info`
//! and `config` may be months old — they arrive once per Node boot. So every connection starts
//! with [`replay_retained`]: each enrolled node's `info`, `device_info` and `config`, rebuilt
//! from the `node-get` cache and published retained (`node_reported_state.md` §3) — the
//! LoRaWAN enrollments from their cache and the P2P registry from its own.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use control_radio::cache::{Cache, CacheDocument};
use control_radio::error::{Component, Error, Kind, Result, Transport};
use control_radio::lorawan::enroll::NodesFile;
use control_radio::lorawan::{event, gateway};
use control_radio::mode::{live_carrier, Heard, Mode};
use control_radio::northbound::{self, Topics};
use control_radio::p2p::db::SessionsDb;
use control_radio::store::{Slot, Store};
use control_radio::tasks::uplink;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};

use crate::supervise::Shutdown;
use crate::Context;

/// The broker account this process connects as.
///
/// One client id, so the ACL rows `system/design.md` §7/§8 owe are one account's — which is
/// what decision #12 meant by the `control-radio` account dissolving into this one.
pub const CLIENT_ID: &str = "proximos-control";

/// How long the daemon waits between checks for a converged `application_id`.
///
/// Not the supervisor's backoff: this is not a failure, it is a boot-order wait, and it must
/// stay short enough that the first uplink after `create` finishes is not lost to a long sleep.
pub const APP_ID_POLL: Duration = Duration::from_secs(5);

/// Well above any single message on this connection.
///
/// The cap that tore v1's client down was a *default* nobody had looked at (§9.4): a single
/// well-buffered Node exceeded rumqttc's 10 240 B limit and took the whole client with it, 11
/// reconnects in under 6 minutes. State the limit rather than inherit it. An uplink event with
/// a dozen `rxInfo[]` entries is a few kilobytes; a reassembled `ConfigDump` is smaller still.
const MAX_PACKET: usize = 256 * 1024;

/// Long enough not to chat on a loopback socket, short enough that a broker restart is noticed
/// in seconds rather than at the next uplink — which on a 15-minute reporting cadence would
/// mean losing a quarter of an hour of readings to a connection nobody knew was dead.
const KEEP_ALIVE: Duration = Duration::from_secs(30);

/// What an incoming topic means to this process.
///
/// A pure classification, split out from the loop so the routing is testable without a broker
/// — every subscription this daemon holds shares one connection, so "which of my two filters
/// did this match" is a real decision and not a formality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// `application/{app_id}/device/+/event/up`.
    Uplink,
    /// `application/{app_id}/device/+/event/txack` — a downlink left the gateway.
    TxAck,
    /// `application/{app_id}/device/+/event/join` — a Node joined, with a new `devAddr`.
    Join,
    /// `application/{app_id}/device/+/event/log` — ChirpStack's report about a device, e.g. a
    /// downlink it discarded at transmit time (#129).
    Log,
    /// `{prefix}gateway/+/state/conn`.
    GatewayState,
    /// `{prefix}gateway/+/event/stats` — the concentrator's 30 s report, which the LoRaWAN
    /// status document (`northbridge_diagnostics.md` §4a) reads for liveness and counters.
    GatewayStats,
    /// Another `event/{kind}`, or a topic neither filter owns. Named so a future subscription
    /// cannot be silently swallowed by an uplink handler.
    Ignored,
}

/// Classify one incoming topic.
pub fn route(topic: &str, prefix: &str) -> Route {
    if let Some(t) = event::parse_event_topic(topic) {
        return match t.kind {
            event::EventKind::Up => Route::Uplink,
            event::EventKind::TxAck => Route::TxAck,
            event::EventKind::Join => Route::Join,
            event::EventKind::Log => Route::Log,
            event::EventKind::Other(_) => Route::Ignored,
        };
    }
    // The state topic's shape, checked through the same helper discovery uses so the two
    // cannot disagree about what a gateway topic looks like.
    let matches = |filter: String| {
        let want: Vec<&str> = filter.split('/').collect();
        let got: Vec<&str> = topic.split('/').collect();
        got.len() == want.len() && got.iter().zip(&want).all(|(g, w)| *w == "+" || g == w)
    };
    if matches(gateway::state_topic_filter(prefix)) {
        return Route::GatewayState;
    }
    if matches(control_radio::lorawan::diagnostics::stats_topic_filter(
        prefix,
    )) {
        return Route::GatewayStats;
    }
    Route::Ignored
}

fn broker_err(kind: Kind, message: String) -> Error {
    Error::new(kind, Transport::Lorawan, Component::MqttBroker, message)
}

/// Connect, subscribe, and pump events until shutdown or a connection failure.
///
/// Returning `Err` hands the connection back to the supervisor, which rebuilds it on the
/// backoff ladder — a broker connection cannot be resumed, so a retry has to be a fresh one.
pub async fn run(
    ctx: Arc<Context>,
    observed: Arc<control_radio::mqtt::ObservedRetainedTopics>,
    shutdown: Shutdown,
) -> Result<()> {
    let Some(app_id) = wait_for_application_id(&ctx, &shutdown).await else {
        return Ok(());
    };

    let mut opts = MqttOptions::new(CLIENT_ID, broker_host(), broker_port());
    opts.set_keep_alive(KEEP_ALIVE);
    opts.set_max_packet_size(MAX_PACKET, MAX_PACKET);
    // A fresh session every connect. The alternative — a persistent session — would have the
    // broker queue uplinks while this process is down, and replaying a backlog of readings on
    // restart puts hours-old measurements on `reading` as though they had just arrived. What
    // survives a restart is the cache and the retained inventory, both of which carry their own
    // timestamps.
    opts.set_clean_session(true);

    let (client, mut eventloop) = AsyncClient::new(opts, 64);

    let uplink_filter = northbound::chirpstack_event_filter(&app_id);
    let state_filter = gateway::state_topic_filter(&ctx.prefix);
    // QoS 1 for events, so a broker hiccup does not silently drop a reading; QoS 0 for the
    // retained gateway state, which is a fact the broker re-sends on every reconnect anyway.
    client
        .subscribe(&uplink_filter, QoS::AtLeastOnce)
        .await
        .map_err(|e| {
            broker_err(
                Kind::ServiceDown,
                format!("cannot subscribe to {uplink_filter:?}: {e}"),
            )
        })?;
    client
        .subscribe(&state_filter, QoS::AtMostOnce)
        .await
        .map_err(|e| {
            broker_err(
                Kind::ServiceDown,
                format!("cannot subscribe to {state_filter:?}: {e}"),
            )
        })?;
    let stats_filter = control_radio::lorawan::diagnostics::stats_topic_filter(&ctx.prefix);
    // QoS 0 like the state: a missed report is replaced 30 s later.
    client
        .subscribe(&stats_filter, QoS::AtMostOnce)
        .await
        .map_err(|e| {
            broker_err(
                Kind::ServiceDown,
                format!("cannot subscribe to {stats_filter:?}: {e}"),
            )
        })?;
    eprintln!(
        "proximos-control: subscribed to {uplink_filter:?}, {state_filter:?} and {stats_filter:?}"
    );

    // Its own task, so this loop keeps polling while it runs: the client's request channel is
    // bounded, and a publish awaited here with the event loop idle would never be sent. Aborted
    // with the connection — a dead client's replay is the next connection's to redo.
    let _replay = AbortOnDrop(tokio::spawn(replay_retained(ctx.clone(), client.clone())));

    let cache = control_radio::cache::Cache::new(&ctx.store, Transport::Lorawan);
    let mut state = uplink::State::new();

    loop {
        let event = tokio::select! {
            e = eventloop.poll() => e,
            _ = shutdown.wait() => {
                // A clean DISCONNECT, so the broker drops the session immediately instead of
                // waiting out the keep-alive. Best effort: the process is stopping either way.
                let _ = client.disconnect().await;
                return Ok(());
            }
        };

        match event {
            Ok(Event::Incoming(Packet::Publish(p))) => {
                handle(&ctx, &observed, &client, &cache, &mut state, &p).await;
            }
            Ok(_) => {}
            Err(e) => {
                return Err(broker_err(
                    Kind::ServiceDown,
                    format!("the broker connection failed: {e}"),
                ));
            }
        }
    }
}

/// Aborts the task it holds when dropped, so a task tied to one connection ends with it.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Republish every enrolled node's retained documents from the `node-get` cache —
/// `node_reported_state.md` §3.
///
/// Each node's cache is read immediately before that node is published, so what goes out is
/// the freshest the Hub holds. One race is accepted: a fresh `Info`/`ConfigDump` for the same
/// node, handled by the uplink loop in the moment between this read and this publish, can be
/// overtaken by the older document. It exists only in the first moments after a connect, both
/// documents carry `received_at`, the cache (and so `node-get`) stays right, and the next
/// connect or Node boot corrects the topic.
async fn replay_retained(ctx: Arc<Context>, client: AsyncClient) {
    let (documents, nodes) = retained_documents(ctx.config.mode, &ctx.store, &ctx.topics);
    let mut sent = 0usize;
    for p in documents {
        if let Err(e) = client
            .publish(&p.topic, QoS::AtLeastOnce, p.retained, p.payload)
            .await
        {
            eprintln!("proximos-control: replay stopped at {}: {e}", p.topic);
            return;
        }
        sent += 1;
    }
    eprintln!("proximos-control: replayed {sent} retained documents for {nodes} enrolled nodes");
}

/// Every retained document the replay owes, in publish order, and how many nodes they
/// describe: the LoRaWAN enrollments from the LoRaWAN cache, then — when `radio.mode` runs
/// P2P — the P2P registry from the P2P cache.
///
/// The `nodes/{dev_eui}/*` topics are shared by both carriers (#73), so a DevEUI both
/// registries hold is described by one arm only. Replaying both let the LoRaWAN record win
/// after every restart — the portal labelled a P2P Node "LoRaWAN" and showed its config from
/// before it moved (night test 2026-09-26, PN-1). Which arm is the portal seam's rule
/// (p2p_integration.md §11): the one `radio.mode` names under `p2p` and `lorawan`, and under
/// `dual` the one the node was last heard on ([`live_carrier`] over the two caches), P2P when
/// it has been heard on neither.
///
/// An unreadable registry or cache is logged and skipped: the other arm, and the other nodes,
/// still get their documents back.
fn retained_documents(mode: Mode, store: &Store, topics: &Topics) -> (Vec<uplink::Publish>, usize) {
    let p2p = if mode.allows(Transport::P2p) {
        match SessionsDb::load(store) {
            Ok(db) => Some(db),
            Err(e) => {
                eprintln!("proximos-control: cannot read the P2P registry to replay: {e}");
                None
            }
        }
    } else {
        None
    };
    let lorawan = match NodesFile::load(store) {
        Ok(file) => Some(file),
        // An unreadable enrollment file still lets the P2P registry replay.
        Err(e) => {
            eprintln!("proximos-control: cannot read the enrollment records to replay: {e}");
            None
        }
    };
    // Every node's cache is read once, up front: the arm a node on both carriers is replayed
    // by and the documents it is replayed with come from the same read.
    let lorawan_cache = Cache::new(store, Transport::Lorawan);
    let lorawan_docs: Vec<_> = lorawan
        .iter()
        .flat_map(|file| file.by_serial.values())
        .map(|record| (record, read_cache(&lorawan_cache, record.dev_eui.as_str())))
        .collect();
    let p2p_cache = Cache::new(store, Transport::P2p);
    let p2p_docs: BTreeMap<&str, _> = p2p
        .iter()
        .flat_map(|db| db.nodes.values())
        .map(|node| {
            let doc = read_cache(&p2p_cache, &node.dev_eui);
            (node.dev_eui.as_str(), (node, doc))
        })
        .collect();

    // The arm of each DevEUI both registries hold: the mode's, and under `dual` the one it was
    // last heard on. (`lorawan` and `off` never load the P2P registry, so nothing is held by
    // both there.)
    let heard = |doc: &Option<CacheDocument>| Heard {
        registered: true,
        last: doc.as_ref().and_then(CacheDocument::last_heard),
    };
    let both: BTreeMap<&str, Transport> = lorawan_docs
        .iter()
        .filter_map(|(record, lorawan_doc)| {
            let id = record.dev_eui.as_str();
            let (_, p2p_doc) = p2p_docs.get(id)?;
            let arm = match mode {
                Mode::Dual => live_carrier(&heard(p2p_doc), &heard(lorawan_doc)),
                Mode::P2p => Transport::P2p,
                Mode::Lorawan | Mode::Off => Transport::Lorawan,
            };
            Some((id, arm))
        })
        .collect();
    let replayed_by = |id: &str, t: Transport| both.get(id).is_none_or(|arm| *arm == t);

    let mut out = Vec::new();
    let mut nodes = 0usize;
    for (record, doc) in &lorawan_docs {
        if !replayed_by(record.dev_eui.as_str(), Transport::Lorawan) {
            continue;
        }
        nodes += 1;
        out.extend(uplink::replay(topics, record, doc.as_ref()));
    }

    if let Some(db) = &p2p {
        let net_id = store
            .read(Slot::P2pNetId)
            .ok()
            .flatten()
            .and_then(|hex| u32::from_str_radix(hex.trim(), 16).ok());
        for (id, (node, doc)) in &p2p_docs {
            if !replayed_by(id, Transport::P2p) {
                continue;
            }
            nodes += 1;
            out.extend(control_radio::p2p::replay::replay(
                topics,
                node,
                db.sessions.get(*id),
                net_id,
                doc.as_ref(),
            ));
        }
    }
    (out, nodes)
}

/// A node's `node-get` cache; an unreadable one still gets the node its inventory back.
fn read_cache(cache: &Cache, id: &str) -> Option<CacheDocument> {
    cache.read(id).unwrap_or_else(|e| {
        eprintln!("proximos-control: cannot read the node-get cache of {id}: {e}");
        None
    })
}

/// Poll for the id `create` writes, announcing the wait once.
///
/// `None` means shutdown arrived first.
async fn wait_for_application_id(ctx: &Context, shutdown: &Shutdown) -> Option<String> {
    let mut announced = false;
    loop {
        if shutdown.is_shutting_down() {
            return None;
        }
        if let Some(id) = ctx.application_id() {
            if announced {
                eprintln!("proximos-control: radio-setup has converged; application {id}");
            }
            return Some(id);
        }
        if !announced {
            eprintln!(
                "proximos-control: no converged application id in {} yet — waiting for \
                 radio-setup (this is normal on a first boot)",
                ctx.store
                    .path(control_radio::store::Slot::LorawanConfig)
                    .display()
            );
            announced = true;
        }
        shutdown.sleep(APP_ID_POLL).await;
    }
}

/// One incoming message.
async fn handle(
    ctx: &Context,
    observed: &control_radio::mqtt::ObservedRetainedTopics,
    client: &AsyncClient,
    cache: &control_radio::cache::Cache,
    state: &mut uplink::State,
    p: &rumqttc::Publish,
) {
    match route(&p.topic, &ctx.prefix) {
        Route::GatewayState => {
            // An empty retained payload is the forwarder saying a gateway is gone.
            if p.retain && p.payload.is_empty() {
                observed.record_tombstone(&p.topic);
            } else {
                observed.record(&p.topic, p.retain);
                if let Some(online) = control_radio::lorawan::diagnostics::decode_conn(&p.payload) {
                    ctx.lrw
                        .record_conn(online, control_radio::lorawan::gateway::now_unix());
                }
            }
        }
        Route::GatewayStats => {
            let now = gateway::now_unix();
            match control_radio::lorawan::diagnostics::decode_stats(&p.payload, now) {
                Some(stats) => ctx.lrw.record_stats(stats),
                None => eprintln!("proximos-control: {} — not a gw.GatewayStats", p.topic),
            }
        }
        Route::Uplink => {
            let ev = match event::parse_uplink(&p.payload) {
                Ok(ev) => ev,
                Err(e) => {
                    eprintln!("proximos-control: {} — {e}", p.topic);
                    return;
                }
            };

            // Re-read per frame rather than cached: `node-add` and `node-remove` run in a
            // different process (the CLI), so a cached copy would decode with an inventory
            // that had moved on — a device enrolled a minute ago would be skipped as unknown
            // until this daemon was restarted. The file is small and uplinks are minutes
            // apart per device.
            let nodes = match NodesFile::load(&ctx.store) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("proximos-control: cannot read the enrollment records: {e}");
                    return;
                }
            };

            let mut outcome = uplink::process(
                &ev,
                &uplink::Context {
                    config: &ctx.config,
                    nodes: &nodes,
                    topics: &ctx.topics,
                },
                state,
            );
            if let Some(reason) = &outcome.skipped {
                eprintln!("proximos-control: {}", reason.describe());
            }
            // A completed dump joins what the Hub already holds before it goes anywhere
            // (node_reported_state.md §2.2, "The Hub accumulates"). Read here, before `commit`,
            // and only for an outcome that has a config to fold. An ack has none: it changes
            // nothing in the config (§2.2, the ack fold withdrawn 2026-09-28).
            uplink::fold_prior(&mut outcome, &ctx.topics, |id| {
                cache.read(id).unwrap_or_else(|e| {
                    eprintln!("proximos-control: cannot read the node-get cache of {id}: {e}");
                    None
                })
            });

            // The uplink's `info` refresh keeps the session the Node's last join set up
            // (node_registry.md §5, amended 2026-09-28).
            uplink::fold_session(&mut outcome, |id| read_cache(cache, id));

            deliver(client, cache, &outcome).await;
        }
        Route::Join => {
            let ev = match event::parse_join(&p.payload) {
                Ok(ev) => ev,
                Err(e) => {
                    eprintln!("proximos-control: {} — {e}", p.topic);
                    return;
                }
            };
            // Re-read per event, as for an uplink.
            let nodes = match NodesFile::load(&ctx.store) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("proximos-control: cannot read the enrollment records: {e}");
                    return;
                }
            };
            let outcome = uplink::process_join(
                &ev,
                &uplink::Context {
                    config: &ctx.config,
                    nodes: &nodes,
                    topics: &ctx.topics,
                },
            );
            if let Some(reason) = &outcome.skipped {
                eprintln!("proximos-control: join: {}", reason.describe());
            }
            if let Some(w) = &outcome.session {
                eprintln!(
                    "proximos-control: {} joined over LoRaWAN as {} at {}",
                    w.id, w.session.dev_addr, w.session.joined_at
                );
            }
            deliver(client, cache, &outcome).await;
        }
        Route::Log => {
            let ev = match event::parse_log(&p.payload) {
                Ok(ev) => ev,
                Err(e) => {
                    eprintln!("proximos-control: {} — {e}", p.topic);
                    return;
                }
            };
            // Re-read per event, as for an uplink: the serial the document names.
            let nodes = match NodesFile::load(&ctx.store) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("proximos-control: cannot read the enrollment records: {e}");
                    return;
                }
            };
            let report = uplink::process_log(
                &ev,
                &uplink::Context {
                    config: &ctx.config,
                    nodes: &nodes,
                    topics: &ctx.topics,
                },
                |id| read_cache(cache, id),
            );
            eprintln!("proximos-control: {}", report.line);
            deliver(client, cache, &report.outcome).await;
            // #166: the next set-config to this Node packs into what the NS said it carries.
            if ev.code == event::LOG_CODE_DOWNLINK_PAYLOAD_SIZE {
                if let Some(max) = ev.context_u64("max_payload_size") {
                    let now = gateway::now_unix();
                    if let Err(e) = cache.note_payload_cap(ev.dev_eui.as_str(), max, now) {
                        eprintln!("proximos-control: cannot note the payload cap: {e}");
                    }
                }
            }
        }
        // `txack` says a downlink left the gateway. The fPort-85 engine's own seq-correlated
        // `Response` is what confirms a *command*, so this is not part of any acknowledgement
        // path — it is wired here because the subscription is `event/+` and a future
        // downlink-queue view will want it, not because anything consumes it yet.
        Route::TxAck => {}
        Route::Ignored => {}
    }
}

/// Publish an outcome, then commit its cache writes.
async fn deliver(client: &AsyncClient, cache: &Cache, outcome: &uplink::Outcome) {
    for publish in &outcome.publishes {
        // QoS 1 throughout (§9.4). A dropped reading is a gap in a medical-grade temperature
        // record, and a dropped `info` is an inventory that disagrees with the device tree.
        if let Err(e) = client
            .publish(
                &publish.topic,
                QoS::AtLeastOnce,
                publish.retained,
                publish.payload.clone(),
            )
            .await
        {
            eprintln!("proximos-control: cannot publish {}: {e}", publish.topic);
        }
    }
    // After the publishes: the cache is what `node-get` reads, and a write that succeeded while
    // the publish failed would leave the CLI reporting a reading the portal never received.
    // This order makes the two disagree in the recoverable direction — the next uplink refills
    // both.
    if let Err(e) = uplink::commit(cache, outcome) {
        eprintln!("proximos-control: cannot write the node-get cache: {e}");
    }
}

fn broker_host() -> String {
    endpoint().0
}

fn broker_port() -> u16 {
    endpoint().1
}

/// Split [`northbound::BROKER_ADDR`]. Not configurable — the endpoint is loopback by contract,
/// like the ChirpStack API address.
fn endpoint() -> (String, u16) {
    northbound::BROKER_ADDR
        .split_once(':')
        .and_then(|(h, p)| p.parse().ok().map(|p| (h.to_string(), p)))
        .unwrap_or_else(|| ("127.0.0.1".to_string(), 1883))
}

#[cfg(test)]
mod tests {
    use super::*;
    use control_radio::lorawan::enroll::EnrollmentRecord;
    use control_radio::lorawan::eui::Eui;
    use control_radio::p2p::session::RegisteredNode;

    struct TempStore(std::path::PathBuf);
    impl Drop for TempStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 0413 enrolled on LoRaWAN and then registered on P2P, as on the bench Hub — with the one
    /// registry that moved it to P2P; 1111 on LoRaWAN only.
    fn both_registries(tag: &str) -> (Store, TempStore) {
        let dir = std::env::temp_dir().join(format!(
            "proximos-control-replay-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::new(&dir);
        let record = |serial: &str, dev_eui: &str| EnrollmentRecord {
            serial_number: serial.into(),
            dev_eui: Eui::parse(dev_eui).unwrap(),
            profile: "default".into(),
            activation: "otaa".into(),
            join_eui: None,
            name: None,
            app_key: None,
        };
        let mut file = NodesFile::default();
        file.by_serial
            .insert("33333".into(), record("33333", "5876070000000413"));
        file.by_serial
            .insert("2222".into(), record("2222", "1111111111111111"));
        file.save(&store).unwrap();
        let mut db = SessionsDb::default();
        db.upsert_node(RegisteredNode {
            serial: "80e0684d".into(),
            dev_eui: "5876070000000413".into(),
            profile: "default".into(),
            app_key: "00".repeat(16),
            name: None,
        });
        db.save(&store).unwrap();
        (store, TempStore(dir))
    }

    /// The `transport` of every replayed `info`, by DevEUI, in publish order.
    fn info_transports(mode: Mode, store: &Store) -> Vec<(String, String)> {
        let topics = Topics::new("fiber-4e970f4a");
        let (docs, _) = retained_documents(mode, store, &topics);
        docs.iter()
            .filter(|p| p.topic.ends_with("/info"))
            .map(|p| {
                let v: serde_json::Value = serde_json::from_slice(&p.payload).unwrap();
                (
                    v["dev_eui"].as_str().unwrap().to_string(),
                    v["transport"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    /// PN-1, since the one Node registry (node_registry.md): a Node is replayed once, by its
    /// registry carrier. 0413 was enrolled on LoRaWAN, then registered on P2P, which moved it;
    /// a LoRaWAN-only Node keeps its document under every mode. Under `lorawan` the P2P arm is
    /// off and 0413 is not replayed.
    #[test]
    fn a_node_is_replayed_by_its_registry_carrier() {
        let (store, _dir) = both_registries("mode");
        let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
        for mode in [Mode::P2p, Mode::Dual] {
            assert_eq!(
                info_transports(mode, &store),
                [
                    pair("1111111111111111", "lorawan"),
                    pair("5876070000000413", "p2p"),
                ],
                "{mode:?}"
            );
        }
        assert_eq!(
            info_transports(Mode::Lorawan, &store),
            [pair("1111111111111111", "lorawan")]
        );
    }

    /// Every `device_info` replayed for 0413, as `(received_at, carrier)`.
    fn device_infos_of_0413(mode: Mode, store: &Store) -> Vec<(String, String)> {
        let topics = Topics::new("fiber-4e970f4a");
        let (docs, _) = retained_documents(mode, store, &topics);
        docs.iter()
            .filter(|p| p.topic.ends_with("5876070000000413/device_info"))
            .map(|p| {
                let v: serde_json::Value = serde_json::from_slice(&p.payload).unwrap();
                (
                    v["received_at"].as_str().unwrap().to_string(),
                    v["device_info"]["carrier"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    /// p2p_integration.md §11 (bench 2026-09-27) replayed a Node both carriers held by the one
    /// it was last heard on. With the one registry no Node is held by both: the registry
    /// carrier decides, whichever carrier heard it last.
    #[test]
    fn under_dual_the_registry_carrier_decides_not_the_newest_frame() {
        let (store, _dir) = both_registries("recency");
        let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
        let heard = |t: Transport, at: &str| {
            Cache::new(&store, t)
                .merge_write(
                    "5876070000000413",
                    control_radio::cache::CacheSlot::DeviceInfo,
                    at,
                    serde_json::json!({ "carrier": t.as_str() }),
                )
                .unwrap();
        };

        heard(Transport::P2p, "2026-09-27T11:10:00Z");
        heard(Transport::Lorawan, "2026-09-27T11:17:15.204311Z");
        let mut dual = info_transports(Mode::Dual, &store);
        dual.sort();
        assert_eq!(
            dual,
            [
                pair("1111111111111111", "lorawan"),
                pair("5876070000000413", "p2p"),
            ]
        );
        // One cache per Node (R5): the Node's newest device_info, whichever carrier brought it.
        assert_eq!(
            device_infos_of_0413(Mode::Dual, &store),
            [pair("2026-09-27T11:17:15.204311Z", "lorawan")],
            "one device_info, the newest"
        );
    }

    /// Q-N2: after a Hub or broker restart a LoRaWAN Node's `info` still carries the session
    /// its last join set up, read back from the cache.
    #[test]
    fn a_restart_replays_the_lorawan_session_from_the_cache() {
        let (store, _dir) = both_registries("session");
        Cache::new(&store, Transport::Lorawan)
            .set_lorawan_session(
                "1111111111111111",
                &control_radio::cache::LorawanSession {
                    dev_addr: "01ab23cd".into(),
                    joined_at: "2026-09-27T21:04:11.482161Z".into(),
                },
            )
            .unwrap();
        let topics = Topics::new("fiber-4e970f4a");
        let (docs, _) = retained_documents(Mode::Lorawan, &store, &topics);
        let info = docs
            .iter()
            .find(|p| p.topic == topics.node("1111111111111111", northbound::Area::Info))
            .expect("the Node's info");
        let v: serde_json::Value = serde_json::from_slice(&info.payload).unwrap();
        assert_eq!(v["dev_addr"], "01ab23cd");
        assert_eq!(v["joined_at"], "2026-09-27T21:04:11.482161Z");
        assert_eq!(v["pairing_status"], "joined");
    }

    #[test]
    fn the_two_subscriptions_route_to_their_own_handlers() {
        assert_eq!(
            route(
                "application/app-1/device/70b3d57ed80051b2/event/up",
                "eu868"
            ),
            Route::Uplink
        );
        assert_eq!(
            route(
                "application/app-1/device/70b3d57ed80051b2/event/txack",
                "eu868"
            ),
            Route::TxAck
        );
        assert_eq!(
            route("eu868/gateway/24e124fffefd3bda/state/conn", "eu868"),
            Route::GatewayState
        );
        assert_eq!(
            route(
                "application/app-1/device/70b3d57ed80051b2/event/join",
                "eu868"
            ),
            Route::Join
        );
        assert_eq!(
            route("application/app-1/device/aa/event/log", "eu868"),
            Route::Log
        );
    }

    /// A future subscription must not be silently swallowed by the uplink handler, so anything
    /// unrecognised is `Ignored` by name.
    #[test]
    fn everything_else_is_ignored_by_name() {
        for topic in [
            "application/app-1/device/aa/event/status",
            "eu868/gateway/24e124fffefd3bda/event/up",
            "proximos/hub/nodes/aa/reading",
            "radio/gateways",
            "",
        ] {
            assert_eq!(route(topic, "eu868"), Route::Ignored, "{topic}");
        }
    }

    /// The concentrator's stats feed the LoRaWAN status document (§4a).
    #[test]
    fn gateway_stats_are_routed() {
        assert_eq!(
            route("eu868/gateway/24e124fffefd3bda/event/stats", "eu868"),
            Route::GatewayStats
        );
        assert_eq!(
            route("gateway/24e124fffefd3bda/event/stats", ""),
            Route::GatewayStats
        );
    }

    /// The forwarder's prefix is configuration, not a constant (`lorawan.md` §6) — including
    /// the empty prefix upstream renders as no prefix at all.
    #[test]
    fn the_gateway_prefix_is_configuration() {
        assert_eq!(
            route("gateway/24e124fffefd3bda/state/conn", ""),
            Route::GatewayState
        );
        assert_eq!(
            route("gateway/24e124fffefd3bda/state/conn", "eu868"),
            Route::Ignored,
            "the wrong prefix must not match by accident"
        );
        assert_eq!(
            route("us915/gateway/24e124fffefd3bda/state/conn", "eu868"),
            Route::Ignored
        );
    }

    /// One client id, so the ACL rows `system/design.md` §7/§8 owe are one account's.
    #[test]
    fn the_client_id_is_the_one_account_decision_12_left() {
        assert_eq!(CLIENT_ID, "proximos-control");
        assert_eq!(endpoint(), ("127.0.0.1".to_string(), 1883));
        assert_eq!(northbound::BROKER_ADDR, "127.0.0.1:1883");
    }

    /// The cap that tore v1's client down was a default nobody had looked at. State it.
    #[test]
    fn the_packet_size_is_stated_not_inherited() {
        // A const block, so this is a compile-time fact rather than a runtime one — the
        // number is a constant and clippy is right that asserting it at runtime proves
        // nothing about the build that shipped.
        const {
            assert!(
                MAX_PACKET > 10_240,
                "above the default that took v1's client down"
            );
            assert!(MAX_PACKET >= 64 * 1024, "and with room for a whole event");
        }
    }
}
