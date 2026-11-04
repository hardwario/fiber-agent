//! The broker client — retained-topic reads for gateway discovery, and the retained writes
//! the CLI itself owns.
//!
//! Deliberately minimal. Two things here cannot wait for the agent's own client:
//!
//! - reading the forwarder's retained gateway-state topics (EUI discovery), and
//! - the per-node `info` document and its tombstone, which `node-add`/`node-remove` publish
//!   **synchronously from the command itself** (`plan.md` §9.4: `node-remove` *must* clear the
//!   retained topic). Routing those through the daemon would mean a removal only takes effect
//!   in the inventory once something else runs, which is precisely the retained-topic hygiene
//!   defect the rule exists to prevent.
//!
//! The streaming northbound publishes — `reading`, `alarm`, `config`, `command` — stay with
//! the loop duties decision #12 moves into `proximos-control`, and are not opened here.

use std::time::Duration;

use rumqttc::{Client, Event, MqttOptions, Packet, QoS, RecvTimeoutError};

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::lorawan::gateway::RetainedTopics;
use crate::northbound::{RetainedPublisher, BROKER_ACCOUNT, BROKER_ADDR};

/// How long the retained replay must stay quiet before discovery calls it done.
const SETTLE: Duration = Duration::from_millis(500);
/// How often the settle condition is re-checked while nothing is arriving. Short enough that
/// the settle window dominates the total, long enough not to spin.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long a retained publish waits for its PUBACKs.
///
/// The broker is on loopback, so this is not a network budget — it is the point at which
/// "mosquitto is not accepting" stops being worth waiting for. `node-add`/`node-remove` run
/// inside a boot unit and from an operator's terminal; both would rather see a `ServiceDown`
/// naming the broker than block. Deliberately shorter than [`crate::lorawan::gateway::
/// DISCOVERY_TIMEOUT`], which waits on a *device* appearing rather than on a local service.
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);

/// The packet cap both broker connections state rather than inherit.
///
/// rumqttc's default is 10 240 B, and a document that grew past it errors the connection
/// instead of the message — the failure that tore v1's client down (`plan.md` §9.4). A
/// retained `info` document is small today, but an oversized one must not be able to take a
/// discovery read or a publish with it, so the limit is written down here at a size no single
/// node document will approach.
const RETAINED_MAX_PACKET: usize = 64 * 1024;

/// Split [`BROKER_ADDR`] once. Not configurable — the endpoint is loopback by contract, like
/// the ChirpStack API address.
fn broker_endpoint() -> (String, u16) {
    BROKER_ADDR
        .split_once(':')
        .and_then(|(h, p)| p.parse().ok().map(|p| (h.to_string(), p)))
        .unwrap_or_else(|| ("127.0.0.1".to_string(), 1883))
}

fn broker_err(kind: Kind, m: String) -> Error {
    Error::new(kind, Transport::Lorawan, Component::MqttBroker, m)
}

pub struct RumqttcRetainedTopics {
    host: String,
    port: u16,
}

impl Default for RumqttcRetainedTopics {
    fn default() -> Self {
        Self::new()
    }
}

impl RumqttcRetainedTopics {
    pub fn new() -> Self {
        let (host, port) = broker_endpoint();
        RumqttcRetainedTopics { host, port }
    }
}

#[async_trait::async_trait]
impl RetainedTopics for RumqttcRetainedTopics {
    async fn retained_topic_names(&self, filter: &str, timeout: Duration) -> Result<Vec<String>> {
        let host = self.host.clone();
        let port = self.port;
        let filter = filter.to_string();

        // rumqttc's sync client is a blocking loop; keep it off the async runtime's workers.
        tokio::task::spawn_blocking(move || {
            let broker_err = |m: String| broker_err(Kind::ServiceDown, m);

            let mut opts = MqttOptions::new(
                format!("{BROKER_ACCOUNT}-discovery-{}", std::process::id()),
                &host,
                port,
            );
            opts.set_keep_alive(Duration::from_secs(5));
            // The replay this read is here to receive is retained `info` documents, so an
            // oversized one would error the connection and end discovery rather than be
            // skipped. State the limit instead of inheriting the 10 240 B default.
            opts.set_max_packet_size(RETAINED_MAX_PACKET, RETAINED_MAX_PACKET);
            let (client, mut conn) = Client::new(opts, 32);
            client
                .subscribe(&filter, QoS::AtMostOnce)
                .map_err(|e| broker_err(format!("cannot subscribe to {filter:?}: {e}")))?;

            let deadline = std::time::Instant::now() + timeout;
            let mut topics = Vec::new();
            let mut settled_at: Option<std::time::Instant> = None;

            // Polled, not `conn.iter()`. Iterating blocks until the next notification, so the
            // settle check below only ran when one arrived — and after the retained replay
            // goes quiet the next notification is the keep-alive ping. That made the 500 ms
            // early exit fire at roughly the keep-alive interval instead, so discovery took
            // about 5 s whether or not anything was retained (measured on the bench).
            loop {
                match conn.recv_timeout(POLL_INTERVAL) {
                    Ok(Ok(Event::Incoming(Packet::Publish(p)))) => {
                        // Only retained messages count. A live publish arriving during the
                        // window is a gateway that is talking, not one that was already
                        // registered — and discovery is about the retained inventory.
                        if p.retain {
                            topics.push(p.topic);
                        }
                        settled_at = Some(std::time::Instant::now());
                    }
                    Ok(Ok(Event::Incoming(Packet::SubAck(_)))) => {
                        // The broker replays retained messages right after the SubAck, so
                        // start a short settle timer rather than waiting the whole timeout.
                        settled_at = Some(std::time::Instant::now());
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        return Err(broker_err(format!("broker connection failed: {e}")));
                    }
                    // Nothing arrived within the poll interval; fall through to the checks.
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                // Retained replay is immediate; once it goes quiet for a moment there is
                // nothing more coming, so do not hold the boot for the full deadline.
                if settled_at.is_some_and(|t| t.elapsed() >= SETTLE) {
                    break;
                }
            }
            Ok(topics)
        })
        .await
        .map_err(|e| {
            Error::new(
                Kind::Transport,
                Transport::Lorawan,
                Component::MqttBroker,
                format!("the discovery task did not finish: {e}"),
            )
        })?
    }
}

/// The write half — retained `info` documents and their tombstones.
///
/// Separate from [`RumqttcRetainedTopics`] rather than a second method on it: they are two
/// different capabilities with two different failure meanings (a discovery read that finds
/// nothing is a degraded success; a publish that is not acknowledged is a failure), and
/// dispatch injects them independently so a command that only reads cannot accidentally hold
/// a publish seam.
pub struct RumqttcPublisher {
    host: String,
    port: u16,
}

impl Default for RumqttcPublisher {
    fn default() -> Self {
        Self::new()
    }
}

impl RumqttcPublisher {
    pub fn new() -> Self {
        let (host, port) = broker_endpoint();
        RumqttcPublisher { host, port }
    }
}

/// The retained gateway-state topics a **live subscription** has seen.
///
/// [`RumqttcRetainedTopics`] opens a connection, waits for the replay to go quiet and drops it
/// — right for a one-shot CLI, wrong for a resident process: `proximos-control`'s health tick
/// runs every 60 s, and a connect-subscribe-settle-disconnect cycle at that cadence is both
/// reconnect churn in the broker's log and a fixed sub-second stall in a task that has nothing
/// else to wait for.
///
/// So the daemon subscribes to `{prefix}gateway/+/state/conn` **once**, on the connection it
/// already holds for uplinks, and records what arrives here. The health task then reads it
/// through the same [`RetainedTopics`] seam `create`'s discovery uses, so both paths share one
/// implementation of "which gateways does the local channel know about" and neither has to
/// know how the other got there.
///
/// Retained-only, like the discovery read: a live publish is a gateway that is talking, not one
/// that was registered, and this collection is the inventory.
#[derive(Debug, Default)]
pub struct ObservedRetainedTopics {
    seen: std::sync::Mutex<std::collections::BTreeSet<String>>,
}

impl ObservedRetainedTopics {
    pub fn new() -> Self {
        ObservedRetainedTopics::default()
    }

    /// Note one incoming message. Non-retained messages are ignored — see the type's note.
    pub fn record(&self, topic: &str, retained: bool) {
        if !retained {
            return;
        }
        if let Ok(mut set) = self.seen.lock() {
            set.insert(topic.to_string());
        }
    }

    /// Forget a topic whose retained message was cleared.
    ///
    /// A tombstone — a retained publish with an empty payload — is how the forwarder says a
    /// gateway is gone, and it arrives as an ordinary retained message with no body. Without
    /// this the daemon would keep reporting a concentrator that has been unplugged for weeks.
    pub fn record_tombstone(&self, topic: &str) {
        if let Ok(mut set) = self.seen.lock() {
            set.remove(topic);
        }
    }

    pub fn len(&self) -> usize {
        self.seen.lock().map(|s| s.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait::async_trait]
impl RetainedTopics for ObservedRetainedTopics {
    /// Answers from what the live subscription has already seen, so the `timeout` is unused:
    /// there is nothing to wait for, and blocking the health tick to re-confirm a fact the
    /// connection already delivered would be the churn this type exists to avoid.
    async fn retained_topic_names(&self, filter: &str, _timeout: Duration) -> Result<Vec<String>> {
        let want: Vec<&str> = filter.split('/').collect();
        let set = self.seen.lock().map_err(|_| {
            broker_err(
                Kind::StorageError,
                "the observed-topic set is poisoned".into(),
            )
        })?;
        Ok(set
            .iter()
            .filter(|t| {
                let got: Vec<&str> = t.split('/').collect();
                got.len() == want.len() && got.iter().zip(&want).all(|(g, w)| *w == "+" || g == w)
            })
            .cloned()
            .collect())
    }
}

#[async_trait::async_trait]
impl RetainedPublisher for RumqttcPublisher {
    async fn publish_retained(&self, messages: &[(String, Vec<u8>)]) -> Result<()> {
        self.publish(messages, true).await
    }

    async fn publish_events(&self, messages: &[(String, Vec<u8>)]) -> Result<()> {
        self.publish(messages, false).await
    }
}

impl RumqttcPublisher {
    /// One connection for `messages`, QoS 1, held until the broker has acked every one.
    async fn publish(&self, messages: &[(String, Vec<u8>)], retained: bool) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let host = self.host.clone();
        let port = self.port;
        let messages = messages.to_vec();
        let what = if retained { "retained" } else { "event" };

        // Same discipline as the discovery read above: rumqttc's sync client is a blocking
        // loop, so it stays off the async runtime's workers.
        tokio::task::spawn_blocking(move || {
            let mut opts = MqttOptions::new(
                format!("{BROKER_ACCOUNT}-publish-{}", std::process::id()),
                &host,
                port,
            );
            opts.set_keep_alive(Duration::from_secs(5));
            // The retained `info` document is small, but the cap that tore v1's client down was
            // a *default* nobody had looked at (plan.md §9.4). State the limit rather than
            // inherit it, and keep it well above any single-node document.
            opts.set_max_packet_size(RETAINED_MAX_PACKET, RETAINED_MAX_PACKET);
            let (client, mut conn) = Client::new(opts, 32);

            let expected = messages.len();
            for (topic, payload) in &messages {
                // QoS 1. AtMostOnce would give nothing to wait for, and "the tombstone was
                // probably delivered" is not a property a removal can rest on.
                client
                    .publish(topic, QoS::AtLeastOnce, retained, payload.clone())
                    .map_err(|e| {
                        broker_err(
                            Kind::ServiceDown,
                            format!("cannot publish the {what} topic {topic:?}: {e}"),
                        )
                    })?;
            }

            let deadline = std::time::Instant::now() + PUBLISH_TIMEOUT;
            let mut acked = 0usize;
            while acked < expected {
                match conn.recv_timeout(POLL_INTERVAL) {
                    Ok(Ok(Event::Incoming(Packet::PubAck(_)))) => acked += 1,
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        return Err(broker_err(
                            Kind::ServiceDown,
                            format!("broker connection failed: {e}"),
                        ));
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        return Err(broker_err(
                            Kind::ServiceDown,
                            format!(
                                "the broker closed the connection after {acked} of {expected} \
                                 {what} publishes were acknowledged"
                            ),
                        ));
                    }
                }
                if std::time::Instant::now() >= deadline && acked < expected {
                    // `Timeout` (exit 21), not `ServiceDown` (20): the broker was reachable —
                    // it just did not acknowledge in time. Both are in the retryable band, so
                    // the boot unit's policy is the same, but the operator's next step is not.
                    return Err(broker_err(
                        Kind::Timeout,
                        format!(
                            "only {acked} of {expected} {what} publishes were acknowledged \
                             within {}s",
                            PUBLISH_TIMEOUT.as_secs()
                        ),
                    ));
                }
            }

            // Acknowledged, so the broker has stored them; the disconnect is politeness for
            // the broker's log, and a failure to send it changes nothing about the outcome.
            let _ = client.disconnect();
            Ok(())
        })
        .await
        .map_err(|e| {
            Error::new(
                Kind::Transport,
                Transport::Lorawan,
                Component::MqttBroker,
                format!("the {what}-publish task did not finish: {e}"),
            )
        })?
    }
}

#[cfg(test)]
mod observed_tests {
    use super::*;

    /// The cap is the point of the constant; a later edit that shrinks it back towards
    /// rumqttc's 10 240 B default reintroduces the failure it exists to prevent.
    #[test]
    fn the_packet_cap_stays_well_above_the_inherited_default() {
        const { assert!(RETAINED_MAX_PACKET >= 64 * 1024) };
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    /// The same filter semantics the discovery read has, so the health task cannot get a
    /// different answer depending on which implementation it happens to hold.
    #[test]
    fn the_single_level_wildcard_matches_the_way_it_does_on_the_wire() {
        let o = ObservedRetainedTopics::new();
        o.record("eu868/gateway/0011223344556677/state/conn", true);
        o.record("eu868/gateway/24e124fffefd3bda/state/conn", true);
        // Neighbouring topics on the same connection must not leak into the answer.
        o.record("eu868/gateway/0011223344556677/event/stats", true);
        o.record("application/app-1/device/aa/event/up", true);

        let got = rt()
            .block_on(o.retained_topic_names("eu868/gateway/+/state/conn", Duration::from_secs(0)))
            .expect("reads");
        assert_eq!(
            got,
            vec![
                "eu868/gateway/0011223344556677/state/conn".to_string(),
                "eu868/gateway/24e124fffefd3bda/state/conn".to_string(),
            ]
        );
    }

    /// A live publish is a gateway that is talking, not one that was registered — and this
    /// collection is the inventory.
    #[test]
    fn only_retained_messages_count() {
        let o = ObservedRetainedTopics::new();
        o.record("eu868/gateway/0011223344556677/state/conn", false);
        assert!(o.is_empty());
        o.record("eu868/gateway/0011223344556677/state/conn", true);
        assert_eq!(o.len(), 1);
    }

    /// Without the tombstone the daemon would keep reporting a concentrator that has been
    /// unplugged for weeks.
    #[test]
    fn a_cleared_retained_topic_leaves_the_inventory() {
        let o = ObservedRetainedTopics::new();
        o.record("eu868/gateway/0011223344556677/state/conn", true);
        o.record_tombstone("eu868/gateway/0011223344556677/state/conn");
        assert!(o.is_empty());
        let got = rt()
            .block_on(o.retained_topic_names("eu868/gateway/+/state/conn", Duration::from_secs(0)))
            .expect("reads");
        assert!(got.is_empty());
    }

    /// The same retained topic republished is one gateway, not two.
    #[test]
    fn a_republished_topic_does_not_duplicate() {
        let o = ObservedRetainedTopics::new();
        for _ in 0..3 {
            o.record("eu868/gateway/0011223344556677/state/conn", true);
        }
        assert_eq!(o.len(), 1);
    }
}
