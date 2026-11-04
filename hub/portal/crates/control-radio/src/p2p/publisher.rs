//! Northbound publish — the decoded-uplink half of the loop duties `mqtt.rs` defers.
//!
//! `plan/control/radio/plan.md` §9.4 / `p2p.md` §11.2: decoded P2P uplinks are republished on the
//! one per-node topic family `proximos/{hostname}/nodes/{dev_eui}/{area}` via
//! [`crate::northbound::Topics`]. `info` is retained (the inventory a late subscriber needs) and
//! tombstoned on removal; `reading`/`alarm` are QoS 1, not retained. The publisher is a trait so
//! the runtime can be tested against a recording fake, exactly as the LoRaWAN arm is.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::northbound::{Area, Topics, BROKER_ADDR, TOMBSTONE};

/// Where the runtime sends decoded data. `dev_eui` is the node identifier on both carriers
/// since #73 (`p2p.md` §11.2).
#[async_trait::async_trait]
pub trait NorthboundPublisher: Send + Sync {
    /// Publish one decoded area for a node. Retention and QoS follow [`Area`].
    async fn publish(&self, dev_eui: &str, area: Area, payload: &[u8]) -> Result<()>;
    /// Publish one decoded area at QoS 1 and resolve only once the broker has acked it
    /// (`PUBACK`) — what the outbox drainer ([`crate::p2p::outbox`]) deletes an entry on.
    /// An error means the broker did not take it and it may be published again. A double
    /// with no broker behind it acks at once.
    async fn publish_confirmed(&self, dev_eui: &str, area: Area, payload: &[u8]) -> Result<()> {
        self.publish(dev_eui, area, payload).await
    }
    /// Clear the retained `info` topic — the tombstone a `node-remove` leaves so a removed node
    /// cannot linger in a late subscriber's inventory.
    async fn tombstone(&self, dev_eui: &str) -> Result<()>;
    /// How many times the confirmed (outbox) session has connected to the broker, and when it
    /// last did; `None` for a publisher with no broker session. The drainer logs the backlog
    /// it drains after each connect.
    fn broker_connect(&self) -> Option<(u64, std::time::Instant)> {
        None
    }
}

fn broker_err(m: impl std::fmt::Display) -> Error {
    Error::new(
        Kind::ServiceDown,
        Transport::P2p,
        Component::MqttBroker,
        format!("northbound publish: {m}"),
    )
}

/// The real publisher over the local broker (`northbound::BROKER_ADDR`, 127.0.0.1:1883).
///
/// Owns an `AsyncClient` whose event loop is polled by a spawned task; publishes are fire-and-ack
/// at QoS 1. A broker outage surfaces as `ServiceDown`/`MqttBroker` on the publish call — the
/// runtime logs it and carries on (a missed northbound message must never stall the RX path).
///
/// **Non-blocking on purpose.** `AsyncClient::publish` awaits a slot on a bounded channel, and
/// that await only ever resolves to an error if the channel is *closed* — which never happens
/// here, because the spawned task owns the event loop for the life of the process. While the
/// broker is unreachable rumqttc does not drain the channel at all, so the 64 slots fill and the
/// await becomes indefinite. `Central` runs on one task, so that would not merely delay the
/// northbound message: it would stop the central processing anything, with the node
/// unacknowledged and retransmitting into a closed window. `try_publish` fails instead.
///
/// The decoded documents of ring events do not take this best-effort path: they go through the
/// outbox ([`crate::p2p::outbox`]), whose drainer publishes with
/// [`NorthboundPublisher::publish_confirmed`] on a second session (`{client_id}-outbox`) and
/// waits for each `PUBACK` — on its own task, never the central's.
pub struct RumqttcPublisher {
    client: rumqttc::AsyncClient,
    topics: Topics,
    /// The outbox drainer's own broker session, `{client_id}-outbox`: one message in flight at
    /// a time, so each `PUBACK` maps to its publish without guessing.
    confirmed: ConfirmedClient,
}

/// Matches `PUBACK`s to the publishes awaiting them on one client.
///
/// rumqttc reports a QoS 1 publish as `Outgoing::Publish(pkid)` when it writes it and
/// `Incoming::PubAck(pkid)` when the broker acks it, and it writes the requests in the order
/// they were queued. So the waiters queue in that order and take the `pkid` of the next new
/// write. A write of a `pkid` already in flight is rumqttc's own resend after a reconnect: the
/// same publish, the same waiter. The publish is never queued twice by us while it waits —
/// rumqttc keeps it across the reconnect — so a broker that is away costs time, not a
/// duplicate.
#[derive(Debug, Default)]
pub struct AckTracker {
    queued: VecDeque<tokio::sync::oneshot::Sender<()>>,
    in_flight: HashMap<u16, tokio::sync::oneshot::Sender<()>>,
}

impl AckTracker {
    /// A publish is about to be queued on the client, under the same lock.
    pub fn queue(&mut self, ack: tokio::sync::oneshot::Sender<()>) {
        self.queued.push_back(ack);
    }

    /// The publish just queued did not get onto the client.
    pub fn unqueue(&mut self) {
        self.queued.pop_back();
    }

    /// rumqttc wrote the publish `pkid`.
    pub fn written(&mut self, pkid: u16) {
        if self.in_flight.contains_key(&pkid) {
            return;
        }
        if let Some(ack) = self.queued.pop_front() {
            self.in_flight.insert(pkid, ack);
        }
    }

    /// The broker acked `pkid`.
    pub fn acked(&mut self, pkid: u16) {
        if let Some(ack) = self.in_flight.remove(&pkid) {
            let _ = ack.send(());
        }
    }
}

struct ConfirmedClient {
    client: rumqttc::AsyncClient,
    acks: Arc<Mutex<AckTracker>>,
    /// One publish in flight.
    gate: tokio::sync::Mutex<()>,
    /// Connects of this session (`CONNACK`s) and the time of the last one.
    connects: Arc<Mutex<Option<(u64, Instant)>>>,
}

fn lock_acks(m: &Mutex<AckTracker>) -> std::sync::MutexGuard<'_, AckTracker> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl ConfirmedClient {
    fn connect(client_id: &str, host: &str, port: u16) -> Self {
        let mut opts = rumqttc::MqttOptions::new(client_id, host, port);
        opts.set_keep_alive(Duration::from_secs(30));
        let (client, mut eventloop) = rumqttc::AsyncClient::new(opts, 8);
        let acks = Arc::new(Mutex::new(AckTracker::default()));
        let tracker = acks.clone();
        let connects: Arc<Mutex<Option<(u64, Instant)>>> = Arc::default();
        let seen = connects.clone();
        tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(_))) => {
                        let mut c = seen.lock().unwrap_or_else(|p| p.into_inner());
                        let n = c.map_or(1, |(n, _)| n + 1);
                        *c = Some((n, Instant::now()));
                    }
                    Ok(rumqttc::Event::Outgoing(rumqttc::Outgoing::Publish(pkid))) => {
                        lock_acks(&tracker).written(pkid)
                    }
                    Ok(rumqttc::Event::Incoming(rumqttc::Packet::PubAck(ack))) => {
                        lock_acks(&tracker).acked(ack.pkid)
                    }
                    Ok(_) => {}
                    // The broker is away: rumqttc keeps the unacked publish and writes it
                    // again once it reconnects.
                    Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
                }
            }
        });
        ConfirmedClient {
            client,
            acks,
            gate: tokio::sync::Mutex::new(()),
            connects,
        }
    }

    async fn publish(&self, topic: String, retain: bool, payload: Vec<u8>) -> Result<()> {
        let _one = self.gate.lock().await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut acks = lock_acks(&self.acks);
            acks.queue(tx);
            if let Err(e) =
                self.client
                    .try_publish(topic, rumqttc::QoS::AtLeastOnce, retain, payload)
            {
                acks.unqueue();
                return Err(broker_err(e));
            }
        }
        // No timeout: while the broker is away the publish sits in rumqttc, which sends it
        // again on reconnect. Giving up here and publishing it anew would send it twice.
        rx.await
            .map_err(|_| broker_err("the broker client stopped before the PUBACK"))
    }
}

impl RumqttcPublisher {
    /// Connect under the bench host's client id, `control-radio-p2p`.
    pub fn connect(hostname: impl Into<String>) -> Self {
        Self::connect_as("control-radio-p2p", hostname)
    }

    /// Connect under `client_id`. The broker drops an older session when a second client
    /// claims the same id, so the resident host (`proximos-control`) and a bench host must not
    /// share one — each would keep kicking the other off.
    pub fn connect_as(client_id: &str, hostname: impl Into<String>) -> Self {
        let (host, port) = BROKER_ADDR
            .split_once(':')
            .and_then(|(h, p)| p.parse::<u16>().ok().map(|p| (h.to_string(), p)))
            .unwrap_or_else(|| ("127.0.0.1".to_string(), 1883));
        let confirmed = ConfirmedClient::connect(&format!("{client_id}-outbox"), &host, port);
        let mut opts = rumqttc::MqttOptions::new(client_id, host, port);
        opts.set_keep_alive(Duration::from_secs(30));
        let (client, mut eventloop) = rumqttc::AsyncClient::new(opts, 64);
        // Drain the event loop so publishes flush; we never subscribe. A poll error just means
        // the broker is down — back off and let the next publish surface the failure.
        tokio::spawn(async move {
            loop {
                if eventloop.poll().await.is_err() {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        });
        RumqttcPublisher {
            client,
            topics: Topics::new(hostname),
            confirmed,
        }
    }
}

#[async_trait::async_trait]
impl NorthboundPublisher for RumqttcPublisher {
    async fn publish(&self, dev_eui: &str, area: Area, payload: &[u8]) -> Result<()> {
        let topic = self.topics.node(dev_eui, area);
        self.client
            .try_publish(
                topic,
                rumqttc::QoS::AtLeastOnce,
                area.retained(),
                payload.to_vec(),
            )
            .map_err(broker_err)
    }

    async fn publish_confirmed(&self, dev_eui: &str, area: Area, payload: &[u8]) -> Result<()> {
        let topic = self.topics.node(dev_eui, area);
        self.confirmed
            .publish(topic, area.retained(), payload.to_vec())
            .await
    }

    fn broker_connect(&self) -> Option<(u64, Instant)> {
        *self
            .confirmed
            .connects
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    async fn tombstone(&self, dev_eui: &str) -> Result<()> {
        for topic in self.topics.tombstones(dev_eui) {
            self.client
                .try_publish(topic, rumqttc::QoS::AtLeastOnce, true, TOMBSTONE.to_vec())
                .map_err(broker_err)?;
        }
        Ok(())
    }
}

/// Forward through a boxed publisher, so a binary can pick the real one or [`NullPublisher`] at
/// runtime and still hand [`crate::p2p::runtime::Central`] a single concrete `P`.
#[async_trait::async_trait]
impl NorthboundPublisher for Box<dyn NorthboundPublisher> {
    async fn publish(&self, dev_eui: &str, area: Area, payload: &[u8]) -> Result<()> {
        (**self).publish(dev_eui, area, payload).await
    }
    async fn publish_confirmed(&self, dev_eui: &str, area: Area, payload: &[u8]) -> Result<()> {
        (**self).publish_confirmed(dev_eui, area, payload).await
    }
    async fn tombstone(&self, dev_eui: &str) -> Result<()> {
        (**self).tombstone(dev_eui).await
    }
    fn broker_connect(&self) -> Option<(u64, Instant)> {
        (**self).broker_connect()
    }
}

/// Forward through a shared publisher, so a host that restarts the central (PI1's supervisor)
/// hands every new `Central` the one broker client it opened at startup instead of opening a
/// second session under the same client id.
#[async_trait::async_trait]
impl<T: NorthboundPublisher + ?Sized> NorthboundPublisher for std::sync::Arc<T> {
    async fn publish(&self, dev_eui: &str, area: Area, payload: &[u8]) -> Result<()> {
        (**self).publish(dev_eui, area, payload).await
    }
    async fn publish_confirmed(&self, dev_eui: &str, area: Area, payload: &[u8]) -> Result<()> {
        (**self).publish_confirmed(dev_eui, area, payload).await
    }
    async fn tombstone(&self, dev_eui: &str) -> Result<()> {
        (**self).tombstone(dev_eui).await
    }
    fn broker_connect(&self) -> Option<(u64, Instant)> {
        (**self).broker_connect()
    }
}

/// A publisher that does nothing — for a `--no-mqtt` bench run, where the RTT/log trace is the
/// only witness wanted.
pub struct NullPublisher;

#[async_trait::async_trait]
impl NorthboundPublisher for NullPublisher {
    async fn publish(&self, _dev_eui: &str, _area: Area, _payload: &[u8]) -> Result<()> {
        Ok(())
    }
    async fn tombstone(&self, _dev_eui: &str) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::AckTracker;

    /// A publish written, the link lost, written again by rumqttc after the reconnect, then
    /// acked: its waiter resolves once, and the next publish gets the next `pkid`.
    #[tokio::test]
    async fn a_resend_after_a_reconnect_is_the_same_publish() {
        let mut t = AckTracker::default();
        let (a, mut ra) = tokio::sync::oneshot::channel();
        t.queue(a);
        t.written(1);
        t.written(1);
        assert!(ra.try_recv().is_err(), "not acked yet");
        let (b, mut rb) = tokio::sync::oneshot::channel();
        t.queue(b);
        t.acked(1);
        assert!(ra.try_recv().is_ok());
        assert!(rb.try_recv().is_err(), "b is not written yet");
        t.written(2);
        t.acked(2);
        assert!(rb.try_recv().is_ok());
        t.acked(2);
        assert!(t.in_flight.is_empty() && t.queued.is_empty());
    }

    #[test]
    fn a_publish_that_never_got_onto_the_client_takes_no_pkid() {
        let mut t = AckTracker::default();
        let (a, _ra) = tokio::sync::oneshot::channel();
        t.queue(a);
        t.unqueue();
        t.written(5);
        assert!(t.in_flight.is_empty());
    }
}
