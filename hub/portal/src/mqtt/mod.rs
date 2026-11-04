//! MQTT ingest client (work package M1-M, refactored for M4-C). Owns the
//! rumqttc connection to the broker: reconnect/backoff, the load-bearing
//! subscribe order from `fiber-protocol::TopicBuilder`, and routing every
//! inbound `Publish` into [`crate::state::StateStore`] via
//! [`routing::route_publish`].
//!
//! M4-C split the module in two: [`connect`] builds the `rumqttc`
//! `AsyncClient`/`EventLoop` pair up front and hands back a cheap-to-clone
//! [`MqttHandle`] (so [`crate::relay::CommandRelay`] can publish signed
//! commands without owning the event loop) alongside an [`IngestTask`] that
//! still owns the same run/backoff/subscribe loop `run_ingest` used to run
//! standalone. `IngestTask::run` is spawned as its own task exactly like the
//! old `run_ingest(store, cfg, persist)` was — nothing about the loop body,
//! backoff, or subscribe order changed, only where the `AsyncClient` gets
//! constructed.
#![allow(dead_code)]

pub mod routing;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use fiber_protocol::TopicBuilder;
use parking_lot::Mutex;
use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS};
use serde::Serialize;
use tokio::sync::mpsc;

use crate::config::PortalConfig;
use crate::persist::PersistMsg;
use crate::state::{IngestEvent, StateStore};

pub use routing::{route_publish, RouteResult};

const KEEP_ALIVE: Duration = Duration::from_secs(30);
const BACKOFF_CAP_SECS: u64 = 30;
const JITTER_MS_MAX: u64 = 250;
/// Request-channel / eventloop capacity — generous for single-digit device
/// fleets and the handful of in-flight subscribes at connect time.
const CLIENT_CAPACITY: usize = 256;
/// Stated rather than inherited: rumqttc's 10 KiB default let one oversized
/// `eye/sensors` snapshot error `eventloop.poll()` and tear down the *shared*
/// ingest+relay client, which is what broke beacon adding in production. The
/// portal is the aggregation endpoint, so its limit has to sit strictly above
/// every producer's outgoing cap — control-core 20 KiB, control-radio 64 KiB,
/// proximos-control 256 KiB — hence the match with
/// `proximos-control/src/broker.rs`'s `MAX_PACKET`.
const MAX_PACKET_SIZE: usize = 256 * 1024;

/// Connection parameters for [`connect`], built from [`PortalConfig`].
#[derive(Debug, Clone)]
pub struct MqttSettings {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub topic_prefix: String,
    pub client_id: String,
}

impl From<&PortalConfig> for MqttSettings {
    fn from(cfg: &PortalConfig) -> Self {
        Self {
            host: cfg.mqtt_host.clone(),
            port: cfg.mqtt_port,
            username: cfg.mqtt_username.clone(),
            password: cfg.mqtt_password.clone(),
            topic_prefix: cfg.topic_prefix.clone(),
            client_id: cfg.mqtt_client_id.clone(),
        }
    }
}

/// Cheap-to-clone publish handle over the shared `rumqttc::AsyncClient`.
/// [`crate::relay::CommandRelay`] publishes signed commands/confirms/pair
/// requests through this rather than owning (or fighting over) the ingest
/// event loop.
#[derive(Clone)]
pub struct MqttHandle {
    client: AsyncClient,
}

/// How long a publish may wait for room in rumqttc's request channel before
/// it is reported as a failure rather than blocked on.
///
/// `AsyncClient::publish` is a bounded channel send ([`CLIENT_CAPACITY`]).
/// rumqttc 0.24 drains that channel only in `EventLoop::clean()`, which runs
/// on a *select* error — not while `poll()` is retrying `connect()` against a
/// broker that is down. So during an outage the channel fills, and every
/// further `publish().await` blocks until the broker is back. Two of this
/// crate's publishers cannot afford that: `CommandRelay::run` publishes the
/// confirm inline in the ingest sink, so one blocked publish stalls every
/// `config/response`, `pair/response` and `DeviceError` behind it; and the
/// alarm ack's `dispatch` publishes inside an HTTP handler — the stall the
/// `execute`/`dispatch` split was introduced to remove. A bounded wait turns
/// "the broker has been unreachable long enough to queue 256 frames" into an
/// error every caller already handles.
const PUBLISH_QUEUE_TIMEOUT: Duration = Duration::from_secs(5);

impl MqttHandle {
    /// QoS 1, retain=false — mqtt.md "Transport & conventions": "Every
    /// viewer publish is QoS 1, retain=false — no exceptions, no retained
    /// publishes from the viewer."
    /// QoS 0, retain=false — the config sync resp/hint only: a QoS 0
    /// message is never queued for an offline persistent session, so a Hub
    /// that reconnects is not replayed stale documents (HIL 2026-10-15).
    pub async fn publish_qos0(&self, topic: String, payload: Vec<u8>) -> anyhow::Result<()> {
        match tokio::time::timeout(
            PUBLISH_QUEUE_TIMEOUT,
            self.client.publish(topic, QoS::AtMostOnce, false, payload),
        )
        .await
        {
            Ok(result) => Ok(result?),
            Err(_) => anyhow::bail!(
                "MQTT publish queue full for {}s — broker unreachable?",
                PUBLISH_QUEUE_TIMEOUT.as_secs()
            ),
        }
    }

    pub async fn publish(&self, topic: String, payload: Vec<u8>) -> anyhow::Result<()> {
        match tokio::time::timeout(
            PUBLISH_QUEUE_TIMEOUT,
            self.client.publish(topic, QoS::AtLeastOnce, false, payload),
        )
        .await
        {
            Ok(result) => Ok(result?),
            Err(_) => anyhow::bail!(
                "MQTT publish queue full for {}s — broker unreachable?",
                PUBLISH_QUEUE_TIMEOUT.as_secs()
            ),
        }
    }
}

/// The not-yet-running ingest loop, holding the `EventLoop` half of the
/// `AsyncClient`/`EventLoop` pair [`connect`] created. Spawn [`IngestTask::run`]
/// as its own task to actually start polling — mirrors how the old
/// `run_ingest(store, cfg, persist)` was spawned directly.
pub struct IngestTask {
    client: AsyncClient,
    eventloop: EventLoop,
    topics: Vec<String>,
    topic_prefix: String,
    /// Config-sync requests and acks go here instead of the router.
    sync_tx: Option<mpsc::UnboundedSender<crate::sync::SyncInbound>>,
}

/// The v2 radio northbound family — `proximos/+/nodes/+/{reading,alarm,info,device_info,config}`
/// (plan/control/radio/plan.md §9.4), published per node by `proximos-control`.
///
/// **Not built from `topic_prefix`**: this family is rooted at a fixed `proximos/` level that
/// the producer owns, whereas `TopicBuilder`'s topics hang off the configured prefix (default
/// `proximos` since P5; `the_command_and_node_roots_are_the_same_string` pins the two equal).
/// The filters come from `control_radio::northbound::node_filter`, the producer's own
/// builder, so a change to the topic shape cannot leave the portal subscribed — successfully,
/// and to nothing — to the old one.
///
/// One family for both transports (GitLab #73): a P2P node publishes to the same topics once
/// the Hub-side P2P service exists, and its `info` payload says which radio it speaks.
///
/// `device_info` and `config` are the Node's own reports, retained and republished by the Hub
/// on every connect (plan/control/radio/node_reported_state.md §2–§4); their home is the
/// in-memory `StateStore`, like `info`'s. `command` carries the Node's per-frame answers to the
/// portal's own downlinks — acks and refusals, which is how a "Send to device" is confirmed and
/// how a refusal such as `NOT_SUPPORTED` reaches the operator. `history` is a replay's decoded
/// frames, which the persist sink stores and credits to the backfill job that asked
/// (p2p_history_replay.md §14).
fn radio_node_topics() -> Vec<String> {
    use control_radio::northbound::{node_filter, Area};

    [
        Area::Reading,
        Area::Alarm,
        Area::Info,
        Area::DeviceInfo,
        Area::Config,
        Area::Command,
        Area::History,
        Area::DeviceStatus,
    ]
    .into_iter()
    .map(node_filter)
    .collect()
}

/// Every filter the ingest subscribes to, in subscription order.
pub(crate) fn subscribe_list(prefix: &str) -> Vec<String> {
    // Radio topics first, `TopicBuilder`'s list after: that list ends with `+/status` and the
    // order is load-bearing (retained-replay protection), so the new family goes in front of
    // it rather than after.
    //
    // `radio/gateways` (devices.hub.md §12 A2) leads: a fixed, hostname-less topic published
    // retained by `proximos-control`, named by the producer's own constant so a rename cannot
    // leave the portal subscribed — successfully, and to nothing.
    let mut topics = vec![control_radio::northbound::GATEWAYS_TOPIC.to_string()];
    // The radio status documents (radio diagnostics §3/§4a), named by the producers' own
    // builders for the same reason.
    topics.push(control_radio::p2p::northbridge::topic("+"));
    topics.push(control_radio::lorawan::diagnostics::topic("+"));
    topics.extend(radio_node_topics());
    // Config sync req/ack (config_sync.md §5) are NOT subscribed on their own: `+/config/#`
    // already covers them, and an overlapping filter makes the broker deliver every request
    // twice (HIL 2026-10-15: each save built the full document twice). The ingest loop routes
    // them to the sync service by topic.
    topics.extend(TopicBuilder::new(prefix).get_subscribe_topics());
    topics
}

/// Builds the `rumqttc` `AsyncClient`/`EventLoop` pair from `cfg` and
/// returns a publish handle plus the not-yet-started ingest task. Splitting
/// construction from the run loop (previously both lived inside
/// `run_ingest`) is what lets the relay and the ingest loop share one
/// client.
pub fn connect(cfg: MqttSettings) -> (MqttHandle, IngestTask) {
    let topics = subscribe_list(&cfg.topic_prefix);
    let mut options = MqttOptions::new(cfg.client_id.clone(), cfg.host.clone(), cfg.port);
    options.set_keep_alive(KEEP_ALIVE);
    // Persistent session (mqtt/design.md §6, amendment 2026-10-28, #115): with a fixed
    // client id the broker keeps the subscription and queues QoS-1 readings while the
    // portal is away, including across a broker restart (mosquitto `persistence`). A
    // clean session lost everything the P2P outbox drained right after the broker came
    // back, because the portal had not re-subscribed yet.
    options.set_clean_session(false);
    options.set_max_packet_size(MAX_PACKET_SIZE, MAX_PACKET_SIZE);
    if let (Some(username), Some(password)) = (cfg.username.as_ref(), cfg.password.as_ref()) {
        options.set_credentials(username.clone(), password.clone());
    }

    let (client, eventloop) = AsyncClient::new(options, CLIENT_CAPACITY);

    (
        MqttHandle {
            client: client.clone(),
        },
        IngestTask {
            client,
            eventloop,
            topics,
            topic_prefix: cfg.topic_prefix,
            sync_tx: None,
        },
    )
}

impl IngestTask {
    /// Hands `config/sync/{req,ack}` to the sync service.
    pub fn with_sync(mut self, tx: mpsc::UnboundedSender<crate::sync::SyncInbound>) -> Self {
        self.sync_tx = Some(tx);
        self
    }

    /// Runs the MQTT ingest loop. Never returns under normal operation:
    /// broker disconnects and I/O errors are absorbed by the
    /// reconnect/backoff below rather than propagated, since there's no
    /// meaningful recovery action for the caller beyond "keep the process
    /// alive". Identical loop/backoff/subscribe semantics to the pre-M4-C
    /// `run_ingest` — only the `AsyncClient`/`EventLoop` construction moved
    /// out to [`connect`].
    ///
    /// `relay_tx` is the third sink (M4-C): `dispatch` forwards only
    /// `ConfigChallenge`/`ConfigResponse`/`PairResponse` — and, since P2,
    /// `DeviceError`, for the relay's duplicate-nonce tolerance — to it,
    /// alongside (not instead of) the `store`/`persist` handling every
    /// event already got.
    pub async fn run(
        mut self,
        store: Arc<StateStore>,
        persist: Option<mpsc::UnboundedSender<PersistMsg>>,
        relay_tx: Option<mpsc::UnboundedSender<IngestEvent>>,
        pair_invite_tx: Option<mpsc::UnboundedSender<IngestEvent>>,
    ) {
        let mut backoff = Backoff::new();
        // Publish what this process listens to, from the list it actually
        // subscribes with below — the diagnostics page answers "is the portal
        // listening where I think it is", and an answer recomputed from config
        // would agree with a wrong config.
        *subscribed_topics().lock() = self.topics.clone();
        // `None` = "never connected yet" (distinct from `Some(false)`, so
        // the very first connection failure still reports once, not just
        // the reconnects that follow it).
        let mut connected: Option<bool> = None;

        loop {
            match self.eventloop.poll().await {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    if connected != Some(true) {
                        store.apply(IngestEvent::MqttConnection {
                            connected: true,
                            reason: None,
                        });
                    }
                    connected = Some(true);
                    backoff.reset();

                    // Sequential, in-order, QoS 1 — see TopicBuilder::get_subscribe_topics:
                    // `+/status` must land last (retained-replay protection).
                    for topic in &self.topics {
                        if let Err(err) =
                            self.client.subscribe(topic.clone(), QoS::AtLeastOnce).await
                        {
                            tracing::warn!(topic = %topic, error = %err, "mqtt subscribe failed");
                        }
                    }
                }

                Ok(Event::Incoming(Packet::Publish(publish))) => {
                    if let Some(tx) = &self.sync_tx {
                        if let Some(msg) = crate::sync::inbound(
                            &publish.topic,
                            &self.topic_prefix,
                            &publish.payload,
                        ) {
                            if tx.send(msg).is_err() {
                                tracing::warn!("sync channel closed — config sync message dropped");
                            }
                            continue;
                        }
                    }
                    dispatch(
                        &store,
                        persist.as_ref(),
                        relay_tx.as_ref(),
                        pair_invite_tx.as_ref(),
                        &publish.topic,
                        &publish.payload,
                        publish.retain,
                        &self.topic_prefix,
                    );
                }

                Ok(Event::Incoming(Packet::Disconnect)) => {
                    if connected != Some(false) {
                        store.apply(IngestEvent::MqttConnection {
                            connected: false,
                            reason: Some("broker sent disconnect".to_string()),
                        });
                    }
                    connected = Some(false);
                }

                Ok(_) => {
                    // Outgoing/PingReq/PingResp/etc — nothing to route.
                }

                Err(err) => {
                    if connected != Some(false) {
                        store.apply(IngestEvent::MqttConnection {
                            connected: false,
                            reason: Some(err.to_string()),
                        });
                    }
                    connected = Some(false);

                    let wait = backoff.next_delay();
                    tracing::warn!(error = %err, wait_ms = wait.as_millis() as u64, "mqtt connection error; backing off");
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }
}

/// Routes one publish and applies its side effects (state mutation, log,
/// counter, relay handoff) — the only caller of [`routing::route_publish`],
/// which stays a pure function for unit testing.
///
/// `retain` is the flag off the rumqttc `Publish`. It matters for exactly one topic — see the
/// `pair/invite` arm below — and is otherwise none of this function's business: every other
/// route is a snapshot of state, which is what retention is for.
// Three sinks and one publish, all four flat: the three sinks are independently optional at the
// call site and the publish is borrowed from rumqttc, so a struct here would be a struct built
// per message to be destructured immediately.
#[allow(clippy::too_many_arguments)]
fn dispatch(
    store: &StateStore,
    persist: Option<&mpsc::UnboundedSender<PersistMsg>>,
    relay_tx: Option<&mpsc::UnboundedSender<IngestEvent>>,
    pair_invite_tx: Option<&mpsc::UnboundedSender<IngestEvent>>,
    topic: &str,
    payload: &[u8],
    retain: bool,
    prefix: &str,
) {
    match route_publish(topic, payload, prefix) {
        RouteResult::Event(event) => {
            // A `pair/invite` is an event, not a state: "the operator just asked for a pairing",
            // true for the seconds it takes to answer (GitLab #61,
            // `plan/control/ble/peripheral.md` §6.5). The agent publishes it with `retain=false`,
            // but that was publisher discipline and nothing else — and a retained one is handed
            // back to this portal on every reconnect and every restart. For the co-located Hub
            // the path needs no code at all, so a redelivery would spend the next pairing window
            // the operator opens at the front panel, attributed to nobody. Refuse the class.
            if retain && is_pair_invite(&event) {
                tracing::warn!(
                    topic,
                    "dropping a retained pair/invite — an invitation is a one-shot event, and a \
                     retained one re-arrives on every reconnect"
                );
                // Keyed on the route name, not the topic: this map is never pruned and the
                // topic carries a hostname (`route_name`).
                record("dropped_retained", route_name(&event));
                return;
            }
            // A `pair/invite` is the one event deliberately kept out of the persist sink. It has
            // no persistent home anyway (`persist::handle` drops it on its catch-all), and the
            // raw publish rides along with every message sent there — so persisting it would
            // write the operator's pairing code into the spill journal, on disk, for a value that
            // lives five minutes and is spent once.
            //
            // A `config/response` stays out for the same reason: since C2 it can carry a
            // verb's `result`, and `cluster-client-add`'s holds a Client Hub's broker password
            // (plan/control/cluster.md §7). It has no persistent home either.
            if let Some(tx) =
                persist.filter(|_| !is_pair_invite(&event) && !is_config_response(&event))
            {
                // The raw publish rides along with the parsed event: if the
                // write has to be deferred (a full VACUUM holds the lock), the
                // persist thread spills these exact bytes and replays them
                // through this same `route_publish`. See `persist::PersistMsg`.
                let msg = PersistMsg::Publish {
                    topic: topic.to_string(),
                    payload: payload.to_vec(),
                    event: Box::new((*event).clone()),
                };
                // The persist thread ending early is survivable (state stays
                // live, history stops) — warn once per failure, don't crash.
                if tx.send(msg).is_err() {
                    tracing::warn!("persistence channel closed — event not persisted");
                }
            }
            if is_relay_event(&event) {
                if let Some(tx) = relay_tx {
                    if tx.send((*event).clone()).is_err() {
                        tracing::warn!("relay channel closed — command handshake event dropped");
                    }
                }
            }
            if is_pair_invite(&event) {
                if let Some(tx) = pair_invite_tx {
                    if tx.send((*event).clone()).is_err() {
                        tracing::warn!("pair-invite channel closed — invite dropped");
                    }
                }
            }
            record("routed", route_name(&event));
            store.apply(*event);
        }
        RouteResult::Unhandled => {
            tracing::debug!(topic, "unhandled topic");
            record("unhandled", topic);
        }
        RouteResult::Ignored => {
            // Non-probe_1m export streams: ledger-only in v1, not this
            // binary's job — no log, no counter.
        }
        RouteResult::Tombstone => {
            tracing::debug!(topic, "empty payload (tombstone) — skipping");
        }
        RouteResult::DecodeError(err) => {
            // One text for both surfaces: redacting what the diagnostics page keeps while the
            // log line beside it prints the original would redact nothing.
            let error = decode_error_text(topic, err.to_string());
            tracing::warn!(topic, error, "failed to decode mqtt payload");
            record_decode_error(topic, &error);
        }
    }
}

/// The relay's third sink (M4-C) is scoped to exactly the three
/// signed-command-handshake / pairing events plus the `errors` topic —
/// everything else keeps flowing only to `store`/`persist` as before.
///
/// `DeviceError` joined them in P2 for the duplicate-error tolerance of
/// design.md §6.2: rumqttc replays an unacked `config_request` across a
/// reconnect, the device refuses the second copy with `NonceAlreadyUsed`,
/// and only the relay knows whether that nonce is one it signed. Forwarding
/// here *adds* a sink; the store still gets the event and still takes its
/// liveness bump from it, so nothing about today's handling moves.
/// `pair/invite` goes to its own sink, not the relay's.
///
/// The relay resolves in-flight command handshakes; this *starts* something, and it needs the key
/// store and the audit log the relay has no business holding.
fn is_pair_invite(event: &IngestEvent) -> bool {
    matches!(event, IngestEvent::PairInvite { .. })
}

/// See the persist filter in [`dispatch`]: a response may carry a secret, so it never reaches
/// the spill journal.
fn is_config_response(event: &IngestEvent) -> bool {
    matches!(event, IngestEvent::ConfigResponse { .. })
}

fn is_relay_event(event: &IngestEvent) -> bool {
    matches!(
        event,
        IngestEvent::ConfigChallenge { .. }
            | IngestEvent::ConfigResponse { .. }
            | IngestEvent::PairResponse { .. }
            | IngestEvent::DeviceError { .. }
    )
}

/// Exponential backoff, 1s doubling up to a 30s cap, plus a small jitter so
/// a fleet of portals reconnecting after a broker restart doesn't thunder.
struct Backoff {
    attempt: u32,
}

impl Backoff {
    fn new() -> Self {
        Self { attempt: 0 }
    }

    fn reset(&mut self) {
        self.attempt = 0;
    }

    fn next_delay(&mut self) -> Duration {
        let secs = 1u64
            .checked_shl(self.attempt)
            .unwrap_or(u64::MAX)
            .min(BACKOFF_CAP_SECS);
        // Cap the exponent itself once it can no longer change the result
        // (1 << 5 == 32 already saturates at the 30s cap).
        self.attempt = self.attempt.saturating_add(1).min(5);
        Duration::from_secs(secs) + Duration::from_millis(jitter_ms(JITTER_MS_MAX))
    }
}

/// Cheap jitter source: no live broker in tests and no need for
/// cryptographic quality, so avoid pulling in a `rand` dependency for a
/// "small jitter" nice-to-have.
fn jitter_ms(max: u64) -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()) % max)
        .unwrap_or(0)
}

// ---- Diagnostics -----------------------------------------------------
//
// v1's `MQTTClient.diagnostics()` (surfaced at `GET /api/diagnostics/mqtt`)
// exposed per-route counters and this earns its keep here too. One
// process-wide counter map is enough: this binary runs exactly one ingest
// client, so there's no instance to hang the map off of, and `run_ingest`
// never returns a handle the orchestrator could hold onto instead.

static ROUTE_COUNTERS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();

fn route_counters() -> &'static Mutex<HashMap<String, u64>> {
    ROUTE_COUNTERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record(kind: &str, topic: &str) {
    let key = format!("{kind}:{topic}");
    *route_counters().lock().entry(key).or_insert(0) += 1;
}

/// The route a decoded event came in on — the counter key for the
/// `routed:` half of the map.
///
/// **Not the topic.** `unhandled:`/`decode_error:` key on the full topic
/// because a failure is rare and the operator needs the exact string; a
/// success happens on every message, and a topic carries a hostname and
/// often a `dev_eui`, so keying successes that way would grow the map with
/// the fleet and never shrink — a process-lifetime map on the ingest hot
/// path. The variant is the route, it is what design.md §7 means by
/// "per-route", and there are finitely many of them.
fn route_name(event: &IngestEvent) -> &'static str {
    match event {
        IngestEvent::Status { .. } => "status",
        IngestEvent::SensorsAggregated { .. } => "sensors/aggregated",
        IngestEvent::SensorsLive { .. } => "sensors/summary",
        IngestEvent::SystemInfo { .. } => "system/info",
        IngestEvent::AlarmEvent { .. } => "alarms/events",
        IngestEvent::Standby { .. } => "power/standby",
        IngestEvent::ConfigState { .. } => "config/state",
        IngestEvent::Probe1m { .. } => "export/probe_1m",
        IngestEvent::DeviceError { .. } => "errors",
        IngestEvent::MqttConnection { .. } => "mqtt/connection",
        IngestEvent::ConfigChallenge { .. } => "config/challenge",
        IngestEvent::ConfigResponse { .. } => "config/response",
        IngestEvent::PairResponse { .. } => "pair/response",
        IngestEvent::PairInvite { .. } => "pair/invite",
        IngestEvent::LorawanSensors { .. } => "lorawan/sensors",
        IngestEvent::StickerConfig { .. } => "lorawan/sensors/+/config",
        IngestEvent::StickerInfo { .. } => "lorawan/sensors/+/info",
        IngestEvent::StickerFullConfig { .. } => "lorawan/sensors/+/full-config",
        IngestEvent::StickerCommandResult { .. } => "lorawan/sensors/+/command",
        IngestEvent::StickerHistory { .. } => "lorawan/sensors/+/history",
        IngestEvent::EyeSensors { .. } => "eye/sensors",
        IngestEvent::EyeAggregated { .. } => "eye/aggregated",
        IngestEvent::BeaconHistory { .. } => "eye/history",
        IngestEvent::EyeHomes { .. } => "eye/homes",
        IngestEvent::NodeReading { .. } => "nodes/+/reading",
        IngestEvent::NodeAlarm { .. } => "nodes/+/alarm",
        IngestEvent::NodeInventory { .. } => "nodes/+/info",
        IngestEvent::NodeDeviceInfo { .. } => "nodes/+/device_info",
        IngestEvent::NodeDeviceStatus { .. } => "nodes/+/device_status",
        IngestEvent::NodeDeviceConfig { .. } => "nodes/+/config",
        IngestEvent::NodeCommand { .. } => "nodes/+/command",
        IngestEvent::NodeHistory { .. } => "nodes/+/history",
        IngestEvent::RadioGateways { .. } => "radio/gateways",
        IngestEvent::RadioDiagnostics { .. } => "radio/diagnostics",
    }
}

/// The most recent decode failure: `(unix seconds, "topic: error")`.
///
/// Half of the **degraded** rule (design.md §7) and the whole of "with the
/// last error text": a counter says a payload failed to decode, and an
/// operator cannot act on a number. Only the latest is kept — the counters
/// carry the history, and a ring buffer of error strings on the ingest hot
/// path would be a memory decision taken for a diagnostics page.
static LAST_DECODE_ERROR: OnceLock<Mutex<Option<(i64, String)>>> = OnceLock::new();

fn last_decode_error() -> &'static Mutex<Option<(i64, String)>> {
    LAST_DECODE_ERROR.get_or_init(|| Mutex::new(None))
}

fn record_decode_error(topic: &str, error: &str) {
    record("decode_error", topic);
    *last_decode_error().lock() = Some((now_unix(), format!("{topic}: {error}")));
}

/// What an operator is told instead of serde's own message for a `pair/invite`.
const PAIR_INVITE_DECODE_ERROR: &str =
    "malformed pair/invite payload (text withheld — the payload carries a pairing code)";

/// The decode failure as it may be written down.
///
/// serde quotes the offending value in a type-mismatch message (`invalid type: integer 123456,
/// expected a string`), and for a `pair/invite` that value is the operator's pairing code —
/// which then sits in the process log and on the diagnostics page for an hour, for a secret
/// that lives five minutes. Same class as 146abfe's spill journal. The topic already names the
/// shape that failed, which is the whole of what an operator can act on here (GitLab #61).
///
/// Decided on the topic rather than on the decoded event, because a decode failure has no event
/// — the last two segments are the whole of it (`routing::route_publish`).
fn decode_error_text(topic: &str, error: String) -> String {
    if topic.ends_with("/pair/invite") {
        PAIR_INVITE_DECODE_ERROR.to_string()
    } else {
        error
    }
}

/// The topics this process subscribed to, published once the ingest loop
/// knows them.
///
/// Read off the live `IngestTask` rather than rebuilt from config in the
/// route: rebuilding is a second implementation of `get_subscribe_topics` +
/// `radio_node_topics`, and the one question this answers — "is the portal
/// actually listening where I think it is" — is worth nothing if the answer
/// is computed the same way the bug would be.
static SUBSCRIBED_TOPICS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn subscribed_topics() -> &'static Mutex<Vec<String>> {
    SUBSCRIBED_TOPICS.get_or_init(|| Mutex::new(Vec::new()))
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// A decode error this recent still counts as degraded (design.md §7).
const DECODE_ERROR_WINDOW: Duration = Duration::from_secs(3600);

/// What an operator is told, in one word (design.md §16 UI-MQ1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MqttHealth {
    /// Connected, and nothing has failed to decode in the last hour.
    Connected,
    /// Down, but for less than one keepalive cycle — a blip, and the client
    /// is already backing off into a retry. Not an alarm.
    Reconnecting,
    /// Down for longer than one keepalive cycle, **or** something failed to
    /// decode within the last hour. Either way a human should look.
    Degraded,
}

#[derive(Debug, Clone, Serialize)]
pub struct MqttDiagnostics {
    /// Keyed `"unhandled:{topic}"` / `"decode_error:{topic}"` for the
    /// failures, and `"routed:{route}"` for the successes — the latter on
    /// the bounded route name, not the topic, because this map is never
    /// pruned (`route_name`).
    pub route_counts: HashMap<String, u64>,
    pub health: MqttHealth,
    pub connected: bool,
    /// Why the link dropped, when it is down.
    pub last_disconnect_reason: Option<String>,
    /// `"topic: error"` of the most recent decode failure, and how long ago.
    pub last_decode_error: Option<String>,
    pub last_decode_error_age_seconds: Option<i64>,
    pub reconnect_attempts: i64,
    /// How long the link has been down, when it is.
    pub disconnected_for_seconds: Option<i64>,
    pub subscribed_topics: Vec<String>,
}

/// Snapshot of MQTT ingest state for the settings page (design.md §7, §16
/// UI-MQ1).
///
/// Takes the connection half from the caller rather than reading a global:
/// that half lives in the [`StateStore`], because the SSE hub renders it too,
/// and a second copy here would be a second thing to keep in step. The
/// *verdict* is computed here, once, next to the keepalive constant it
/// depends on — a template deciding "degraded" from four fields would put the
/// rule in a place no test looks.
pub fn diagnostics(mqtt: &crate::state::MqttConnectionState) -> MqttDiagnostics {
    let now = now_unix();
    let disconnected_for_seconds = (!mqtt.connected)
        .then_some(mqtt.last_disconnected_at)
        .flatten()
        .map(|at| (now - at).max(0));

    let (last_decode_error, last_decode_error_age_seconds) = match &*last_decode_error().lock() {
        Some((at, text)) => (Some(text.clone()), Some((now - at).max(0))),
        None => (None, None),
    };
    let decode_error_is_recent =
        last_decode_error_age_seconds.is_some_and(|age| age < DECODE_ERROR_WINDOW.as_secs() as i64);

    let health = if !mqtt.connected {
        // A drop with no timestamp is one we have not seen go down — treat it
        // as the fresh case rather than inventing an outage.
        if disconnected_for_seconds.unwrap_or(0) > KEEP_ALIVE.as_secs() as i64 {
            MqttHealth::Degraded
        } else {
            MqttHealth::Reconnecting
        }
    } else if decode_error_is_recent {
        MqttHealth::Degraded
    } else {
        MqttHealth::Connected
    };

    MqttDiagnostics {
        route_counts: route_counters().lock().clone(),
        health,
        connected: mqtt.connected,
        last_disconnect_reason: mqtt.last_disconnect_reason.clone(),
        last_decode_error,
        last_decode_error_age_seconds,
        reconnect_attempts: mqtt.reconnect_attempts,
        disconnected_for_seconds,
        subscribed_topics: subscribed_topics().lock().clone(),
    }
}

#[cfg(test)]
mod tests {
    /// MQTT filter match (`+` one level, `#` the rest).
    fn filter_matches(filter: &str, topic: &str) -> bool {
        let (mut f, mut t) = (filter.split('/'), topic.split('/'));
        loop {
            match (f.next(), t.next()) {
                (Some("#"), _) => return true,
                (Some(a), Some(b)) if a == "+" || a == b => {}
                (None, None) => return true,
                _ => return false,
            }
        }
    }

    #[test]
    fn each_sync_topic_is_subscribed_exactly_once() {
        let filters = subscribe_list("proximos");
        for kind in [
            fiber_protocol::SyncTopic::Req,
            fiber_protocol::SyncTopic::Ack,
        ] {
            let topic = kind.topic("proximos", "hub-a");
            let n = filters.iter().filter(|f| filter_matches(f, &topic)).count();
            assert_eq!(n, 1, "{topic} matched by {n} filters: duplicate delivery");
        }
    }

    use super::*;

    /// **The two halves agree on one root, by value** (plan/portal/mqtt/design.md §9.3).
    ///
    /// This test is load-bearing rather than tidy. The command family's root comes from
    /// `topic_prefix`, rendered; the radio family's is the literal `control_radio::
    /// northbound::ROOT`, because #73 owns that string and P5 deliberately does not touch
    /// it. Two roots that must match and only one of which is configurable is exactly the
    /// shape that drifts — and the drift is **silent**: `extract_hostname` drops every
    /// topic whose root does not match and a subscribe succeeds against a prefix nothing
    /// publishes, so a half-flip raises no error anywhere and the readings simply stop
    /// arriving. Nothing else in the tree would notice.
    ///
    /// The config default is what is checked, not a literal, so flipping the default in
    /// `config.rs` without the radio side fails here.
    #[test]
    fn the_command_and_node_roots_are_the_same_string() {
        let default_prefix = crate::config::load_from(&[(
            std::path::PathBuf::from("/definitely/does/not/exist/portal-config.yaml"),
            crate::config::Source::Production,
        )])
        .expect("the defaults load with no file present")
        .topic_prefix;
        assert_eq!(
            fiber_protocol::topics::TopicBuilder::new(&default_prefix)
                .prefix()
                .to_string(),
            control_radio::northbound::ROOT.to_string(),
            "the portal's rendered topic_prefix default and control-radio's northbound \
             root have to be one string; a half-flip is silent data loss"
        );

        // …and the globs the ingest loop actually subscribes to are under it, which is
        // the property the equality above exists to give.
        for topic in radio_node_topics() {
            assert!(
                topic.starts_with(&format!("{default_prefix}/")),
                "node glob {topic} is not under the configured root {default_prefix}"
            );
        }
    }

    /// **The degraded rule** (design.md §7, UI-MQ1), tested where it lives
    /// rather than through the template that shows it.
    ///
    /// Two clauses, and the boundary of each matters: a drop shorter than one
    /// keepalive cycle is a blip the client is already retrying, and calling
    /// that "degraded" would train an operator to ignore the word by the time
    /// it means something.
    #[test]
    fn degraded_is_a_long_disconnect_or_a_recent_decode_error() {
        let now = now_unix();
        let keepalive = KEEP_ALIVE.as_secs() as i64;
        let state = |connected: bool, down_at: Option<i64>| crate::state::MqttConnectionState {
            connected,
            last_disconnect_reason: None,
            reconnect_attempts: 0,
            last_connected_at: None,
            last_disconnected_at: down_at,
        };

        // Clause one: how long we have been down.
        *last_decode_error().lock() = None;
        assert_eq!(
            diagnostics(&state(true, None)).health,
            MqttHealth::Connected
        );
        assert_eq!(
            diagnostics(&state(false, Some(now))).health,
            MqttHealth::Reconnecting,
            "a fresh drop is a retry in progress, not an outage"
        );
        assert_eq!(
            diagnostics(&state(false, Some(now - keepalive))).health,
            MqttHealth::Reconnecting,
            "exactly one keepalive cycle is still inside the grace"
        );
        assert_eq!(
            diagnostics(&state(false, Some(now - keepalive - 1))).health,
            MqttHealth::Degraded
        );
        assert_eq!(
            diagnostics(&state(false, None)).health,
            MqttHealth::Reconnecting,
            "a drop we never saw go down is the fresh case, not an invented outage"
        );

        // Clause two: a decode error inside the hour degrades a *connected*
        // link — the broker is fine and the data still is not arriving, which
        // is the case a connection-only indicator would call green.
        *last_decode_error().lock() = Some((now, "proximos/h/status: bad json".to_string()));
        let diag = diagnostics(&state(true, None));
        assert_eq!(diag.health, MqttHealth::Degraded);
        assert_eq!(
            diag.last_decode_error.as_deref(),
            Some("proximos/h/status: bad json"),
            "the text is what the operator acts on, not the count"
        );

        // …and ages out of it.
        let stale = now - DECODE_ERROR_WINDOW.as_secs() as i64;
        *last_decode_error().lock() = Some((stale, "old".to_string()));
        let diag = diagnostics(&state(true, None));
        assert_eq!(diag.health, MqttHealth::Connected);
        assert!(
            diag.last_decode_error.is_some(),
            "still reported, just no longer degrading — the counters keep the history"
        );

        *last_decode_error().lock() = None;
    }

    fn drain_unbounded<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> Vec<T> {
        let mut out = Vec::new();
        while let Ok(item) = rx.try_recv() {
            out.push(item);
        }
        out
    }

    /// The subscribe list: the radio family and the cluster-wide gateway report are present,
    /// both spelled by the producer's own builder/constant, and `+/status` still lands last
    /// (retained-replay protection — `TopicBuilder`'s order is load-bearing and neither may
    /// displace it).
    #[test]
    fn the_radio_family_is_subscribed_and_status_still_lands_last() {
        let (_handle, task) = connect(MqttSettings {
            host: "127.0.0.1".into(),
            port: 1883,
            username: None,
            password: None,
            topic_prefix: "proximos".into(),
            client_id: "test".into(),
        });

        assert!(
            task.topics
                .contains(&control_radio::northbound::GATEWAYS_TOPIC.to_string()),
            "radio/gateways missing from {:?}",
            task.topics
        );
        for area in [
            "reading",
            "alarm",
            "info",
            "device_info",
            "config",
            "history",
        ] {
            let expected = format!("proximos/+/nodes/+/{area}");
            assert!(
                task.topics.contains(&expected),
                "{expected} missing from {:?}",
                task.topics
            );
        }
        assert!(
            task.topics.iter().any(|t| t.ends_with("/nodes/+/command")),
            "command results confirm the portal's downlinks"
        );
        assert!(
            task.topics
                .last()
                .expect("some topics")
                .ends_with("/status"),
            "status must stay last: {:?}",
            task.topics
        );
    }

    /// The default the shared ingest+relay client used to inherit is what tore it down. State it.
    #[test]
    fn the_packet_size_is_stated_not_inherited() {
        // A const block, so this is a compile-time fact rather than a runtime one — the
        // number is a constant and clippy is right that asserting it at runtime proves
        // nothing about the build that shipped.
        const {
            assert!(
                MAX_PACKET_SIZE > 10_240,
                "above the rumqttc default this client used to inherit"
            );
            assert!(
                MAX_PACKET_SIZE >= 64 * 1024,
                "and above every producer's outgoing cap"
            );
        }
    }

    /// The end-to-end ingest hop for a v2 uplink: a `proximos/…/reading` publish reaches both
    /// sinks and lands in the store, with no relay involvement.
    #[test]
    fn dispatch_routes_a_radio_reading_into_the_store_and_persist() {
        let store = StateStore::new();
        let (persist_tx, mut persist_rx) = mpsc::unbounded_channel();
        let (relay_tx, mut relay_rx) = mpsc::unbounded_channel();

        let payload = br#"{"message_id":"m-1","dev_eui":"58760700c0668fc7","fields":{"temperature":21.5},"counters":{},"events":[],"gateways":[{"gateway_id":"24e124fffefd3bda","rssi":-41,"snr":13.5}],"received_at":"2026-08-27T09:59:19Z","fport":2}"#;
        dispatch(
            &store,
            Some(&persist_tx),
            Some(&relay_tx),
            None,
            "proximos/proximos-a1b2c3d4/nodes/58760700c0668fc7/reading",
            payload,
            false,
            "proximos",
        );

        assert_eq!(drain_unbounded(&mut persist_rx).len(), 1, "persist sees it");
        assert!(
            drain_unbounded(&mut relay_rx).is_empty(),
            "telemetry is not a relay event"
        );
        let device = store
            .snapshot_device("proximos-a1b2c3d4")
            .expect("device created");
        assert_eq!(device.nodes["58760700c0668fc7"].fields["temperature"], 21.5);
    }

    /// C5: `cluster-client-add`'s response carries a Client Hub's broker password in its
    /// `result`. The relay needs it; the persist sink must not see it, because a deferred write
    /// spills the raw publish to disk.
    #[test]
    fn a_config_response_reaches_the_relay_and_never_the_persist_sink() {
        let store = StateStore::new();
        let (persist_tx, mut persist_rx) = mpsc::unbounded_channel();
        let (relay_tx, mut relay_rx) = mpsc::unbounded_channel();

        dispatch(
            &store,
            Some(&persist_tx),
            Some(&relay_tx),
            None,
            "proximos/proximos-a1b2c3d4/config/response",
            br#"{"challenge_id":"c-1","request_id":"r-1","status":"SUCCESS","applied_at":1,
                "effective_at":1,"message":"ok","result":{"account":"ok","gateway":"ok",
                "password":"pw-pw-pw-pw-pw-pw-pw-pw"}}"#,
            false,
            "proximos",
        );

        let relayed = drain_unbounded(&mut relay_rx);
        assert_eq!(relayed.len(), 1, "the relay resolves the command with it");
        assert!(
            drain_unbounded(&mut persist_rx).is_empty(),
            "the raw publish would ride to the spill journal"
        );
        assert!(
            !format!("{relayed:?}").contains("pw-pw"),
            "and a debug print of the event shows no password"
        );
    }

    #[test]
    fn dispatch_forwards_a_pair_invite_to_its_own_sink_and_not_to_the_relay() {
        // GitLab #61. The invite *starts* a pairing and needs the key store and the audit log, so
        // it goes to the sink `http::app` spawns, not to the relay that resolves handshakes.
        let store = StateStore::new();
        let (persist_tx, mut persist_rx) = mpsc::unbounded_channel();
        let (relay_tx, mut relay_rx) = mpsc::unbounded_channel();
        let (invite_tx, mut invite_rx) = mpsc::unbounded_channel();

        dispatch(
            &store,
            Some(&persist_tx),
            Some(&relay_tx),
            Some(&invite_tx),
            "proximos/FIBER-001/pair/invite",
            br#"{"code":"23456X","origin":"ble"}"#,
            false,
            "proximos",
        );

        assert_eq!(
            drain_unbounded(&mut invite_rx).len(),
            1,
            "the invite sink sees it"
        );
        assert!(
            drain_unbounded(&mut relay_rx).is_empty(),
            "an invite is not a handshake event"
        );
        assert!(
            drain_unbounded(&mut persist_rx).is_empty(),
            "and it is not persisted: the raw publish rides along to that sink, so the operator's \
             pairing code would land in the spill journal for nothing"
        );
    }

    /// A retained invite is not an invitation, it is a recording of one — and the broker hands
    /// it back on every reconnect (GitLab #61, `plan/control/ble/peripheral.md` §6.5).
    ///
    /// The same payload is dispatched twice, once each way, so what the test pins is the retain
    /// flag and nothing else about the message.
    #[test]
    fn a_retained_pair_invite_never_reaches_the_sink_but_a_fresh_one_does() {
        let store = StateStore::new();
        let (invite_tx, mut invite_rx) = mpsc::unbounded_channel();

        for (retain, expected) in [(true, 0), (false, 1)] {
            dispatch(
                &store,
                None,
                None,
                Some(&invite_tx),
                "proximos/FIBER-001/pair/invite",
                br#"{"code":"23456X","origin":"ble"}"#,
                retain,
                "proximos",
            );
            assert_eq!(
                drain_unbounded(&mut invite_rx).len(),
                expected,
                "retain={retain}"
            );
        }
    }

    /// Retention is what the other routes are *for*: `status` and `system/info` arrive retained
    /// on every connect and that replay is the portal's cold start. The drop is scoped to the
    /// one topic that carries an intent rather than a state.
    #[test]
    fn a_retained_publish_on_any_other_topic_is_routed_as_usual() {
        let store = StateStore::new();
        let (persist_tx, mut persist_rx) = mpsc::unbounded_channel();

        dispatch(
            &store,
            Some(&persist_tx),
            None,
            None,
            "proximos/FIBER-001/status",
            b"online",
            true,
            "proximos",
        );

        assert_eq!(drain_unbounded(&mut persist_rx).len(), 1);
        assert!(
            store.snapshot_device("FIBER-001").expect("device").online,
            "a retained status is exactly how the portal learns a device is up"
        );
    }

    /// serde names the value it choked on, and on this one topic that value is the operator's
    /// pairing code — which would then sit in the log and on the diagnostics page for an hour.
    #[test]
    fn a_pair_invite_decode_failure_is_written_down_without_the_payload() {
        let serde_said = r#"invalid type: integer `234561`, expected a string at line 1 column 15"#;

        let text = decode_error_text("proximos/FIBER-001/pair/invite", serde_said.to_string());
        assert!(
            !text.contains("234561"),
            "the code must not survive into the diagnostic: {text}"
        );
        assert_eq!(text, PAIR_INVITE_DECODE_ERROR);

        assert_eq!(
            decode_error_text("proximos/FIBER-001/system/info", serde_said.to_string()),
            serde_said,
            "every other topic keeps serde's message — it is the only thing that says what broke"
        );
    }

    #[test]
    fn dispatch_forwards_only_relay_events_to_relay_tx() {
        let store = StateStore::new();
        let (persist_tx, mut persist_rx) = mpsc::unbounded_channel();
        let (relay_tx, mut relay_rx) = mpsc::unbounded_channel();

        // A Status event: goes to persist, never to relay_tx.
        dispatch(
            &store,
            Some(&persist_tx),
            Some(&relay_tx),
            None,
            "proximos/FIBER-001/status",
            b"online",
            false,
            "proximos",
        );
        // A config/challenge event: goes to both.
        dispatch(
            &store,
            Some(&persist_tx),
            Some(&relay_tx),
            None,
            "proximos/FIBER-001/config/challenge",
            br#"{"challenge_id":"chal-1","request_id":"req-1","signer_id":null,"expires_at":null,"preview":null}"#,
            false,
            "proximos",
        );

        let persisted = drain_unbounded(&mut persist_rx);
        assert_eq!(persisted.len(), 2, "persist sees every routed event");

        let relayed = drain_unbounded(&mut relay_rx);
        assert_eq!(relayed.len(), 1, "relay_tx sees only the challenge event");
        match &relayed[0] {
            IngestEvent::ConfigChallenge { hostname, payload } => {
                assert_eq!(hostname, "fiber-001");
                assert_eq!(payload.challenge_id, "chal-1");
            }
            other => panic!("expected ConfigChallenge, got {other:?}"),
        }

        // Store still applied the Status event normally.
        let device = store.snapshot_device("FIBER-001").expect("device created");
        assert!(device.online);
    }

    /// **Every route has a name, and no name is a topic** (design.md §7's
    /// "per-route counters", `UI-MQ1`).
    ///
    /// Until the P0–P6 audit, `record` was called for `unhandled:` and
    /// `decode_error:` only — the exact counter set the section's own §1
    /// listed as the *defect* it was closing — so the page could say what
    /// had failed and never what was working. A success counter has one
    /// design constraint the failure counters do not: it fires on every
    /// message, and the map is process-wide and never pruned, so the key
    /// must not carry a hostname or a `dev_eui`.
    ///
    /// This pins the constraint rather than the list: a new `IngestEvent`
    /// variant makes `route_name`'s match fail to compile, and a route name
    /// that smuggles an id in fails here.
    #[test]
    fn every_route_name_is_bounded_and_carries_no_identity() {
        let status = IngestEvent::Status {
            hostname: "FIBER-001".into(),
            status: fiber_protocol::payloads::status::DeviceStatus::Online,
        };
        let name = route_name(&status);

        assert_eq!(name, "status");
        assert!(
            !name.contains("FIBER-001"),
            "a route name must not carry the hostname: the counter map is never pruned, \
             so a per-topic success key grows with the fleet for the process's lifetime"
        );
        assert!(
            !name.starts_with('/') && !name.contains("proximos/"),
            "a route name is the route, not the topic it arrived on: {name}"
        );

        // And the counter it produces is keyed under `routed:`, so the
        // fragment can tell a success from a failure without a second map.
        record("routed", name);
        let counters = route_counters().lock().clone();
        assert!(
            counters.contains_key("routed:status"),
            "successes are counted under `routed:`, next to `unhandled:` and \
             `decode_error:`; got {counters:?}"
        );
    }
}
