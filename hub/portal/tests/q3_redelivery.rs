//! Q3: what a disconnect costs on the command family.
//!
//! plan/portal/mqtt/design.md §6.3 owes two measurements, and §6.2's whole
//! duplicate-suppression design rests on one claim in §6.1:
//!
//! > **rumqttc replays unacked publishes on reconnect regardless of
//! > `clean_session`** — its `EventLoop::clean()` moves in-flight publishes
//! > into `pending` and drains the request channel into it, so a
//! > `config_request` in flight during a disconnect is published twice today.
//!
//! That is a claim about a *library*, so it is measured against `rumqttc`
//! itself — with the portal's own `MqttOptions` (`src/mqtt/mod.rs`: 30 s
//! keep-alive, `clean_session = true`, 256 KiB cap, 256-deep channel) — rather
//! than through a mock in another language whose retry semantics are its own.
//! Driving paho here would have measured paho.
//!
//! **Reproducing the scenario is the hard part, and the reason for the proxy.**
//! The replay lives in one `EventLoop`'s reconnect: `clean()` moves *that
//! loop's* in-flight set into `pending` and the *same* loop republishes. Build
//! a fresh `AsyncClient` after the drop and there is nothing to replay — the
//! first cut of this test did exactly that, measured 1, and would have been
//! read as contradicting §6.1 when it had simply thrown the evidence away. So
//! the client here talks through an in-process TCP proxy that can (a) swallow
//! the broker's replies, so the PUBACK never lands and the publish stays
//! in-flight, and (b) sever the socket, so the one live loop reconnects. That
//! is a disconnect, not a restart, and it is what a dropped link on a Hub is.
//!
//! **These tests need a broker and are `#[ignore]`d.** Run them deliberately:
//!
//! ```bash
//! make mock-up
//! cargo test -p portal --test q3_redelivery -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! `--test-threads=1` is not optional: the test counts what arrives on one
//! broker, and anything else on it would count too.
//!
//! The number it prints goes into plan/portal/status.md with the rumqttc
//! version it was taken against (0.24.0). If the count contradicts §6.1, the
//! rule from handoff.md §3 applies — stop and report, because the seen-set
//! and the nonce ledger are sized by this answer. Q3 (a), the restart case,
//! is not a wire measurement — see the note above its former place below.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

const BROKER: &str = "127.0.0.1";
const PORT: u16 = 1883;
const KEEP_ALIVE: Duration = Duration::from_secs(30);
const MAX_PACKET_SIZE: usize = 256 * 1024;
const CLIENT_CAPACITY: usize = 256;

/// The portal's options, so the measurement is of the portal's client and not
/// of a default-configured one.
fn portal_options(client_id: &str, host: &str, port: u16) -> MqttOptions {
    let mut options = MqttOptions::new(client_id, host, port);
    options.set_keep_alive(KEEP_ALIVE);
    options.set_clean_session(true);
    options.set_max_packet_size(MAX_PACKET_SIZE, MAX_PACKET_SIZE);
    options
}

/// A counting subscriber on a direct connection: stands in for the device, and
/// is the only thing that can answer "how many copies actually reached the
/// broker's subscribers".
async fn spawn_counter(topic: &str) -> Arc<Mutex<Vec<String>>> {
    let (client, mut eventloop) = AsyncClient::new(
        portal_options(
            &format!("q3-counter-{}", uuid::Uuid::new_v4()),
            BROKER,
            PORT,
        ),
        CLIENT_CAPACITY,
    );
    client
        .subscribe(topic, QoS::AtLeastOnce)
        .await
        .expect("subscribe");

    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    tokio::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Packet::Publish(p))) => {
                    sink.lock()
                        .await
                        .push(String::from_utf8_lossy(&p.payload).to_string());
                }
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    });
    // Let the SUBACK land before anybody publishes.
    tokio::time::sleep(Duration::from_millis(500)).await;
    seen
}

/// A severable TCP proxy in front of the broker.
///
/// `swallow` drops everything travelling broker -> client, which is how a
/// PUBACK is made never to arrive while the publish itself still reaches the
/// broker. `sever` closes the live connection; the proxy keeps listening, so
/// the client's own reconnect succeeds.
struct Proxy {
    port: u16,
    swallow: Arc<AtomicBool>,
    sever: Arc<AtomicBool>,
    connections: Arc<AtomicU32>,
}

impl Proxy {
    async fn start() -> Self {
        let listener = TcpListener::bind((BROKER, 0)).await.expect("bind proxy");
        let port = listener.local_addr().expect("addr").port();
        let swallow = Arc::new(AtomicBool::new(false));
        let sever = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(AtomicU32::new(0));

        let (s, k, c) = (swallow.clone(), sever.clone(), connections.clone());
        tokio::spawn(async move {
            loop {
                let Ok((inbound, _)) = listener.accept().await else {
                    continue;
                };
                c.fetch_add(1, Ordering::SeqCst);
                let Ok(outbound) = TcpStream::connect((BROKER, PORT)).await else {
                    continue;
                };
                // A fresh connection is a fresh chance: clear the sever flag so
                // the reconnect after a cut is a working link.
                k.store(false, Ordering::SeqCst);
                tokio::spawn(pump(inbound, outbound, s.clone(), k.clone()));
            }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        Self {
            port,
            swallow,
            sever,
            connections,
        }
    }
}

/// Shuttles one connection both ways until `sever` is set, swallowing the
/// broker -> client direction while `swallow` is.
async fn pump(
    mut inbound: TcpStream,
    mut outbound: TcpStream,
    swallow: Arc<AtomicBool>,
    sever: Arc<AtomicBool>,
) {
    let (mut cr, mut cw) = inbound.split();
    let (mut br, mut bw) = outbound.split();
    let mut up = [0u8; 4096];
    let mut down = [0u8; 4096];

    loop {
        if sever.load(Ordering::SeqCst) {
            return; // both halves drop -> the client sees a closed socket
        }
        tokio::select! {
            biased;
            n = cr.read(&mut up) => match n {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if bw.write_all(&up[..n]).await.is_err() {
                        return;
                    }
                }
            },
            n = br.read(&mut down) => match n {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if !swallow.load(Ordering::SeqCst)
                        && cw.write_all(&down[..n]).await.is_err()
                    {
                        return;
                    }
                }
            },
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    }
}

/// **Q3 (b): a QoS-1 publish in flight when the connection drops.**
///
/// One `AsyncClient`, one `EventLoop`, driven throughout. The proxy swallows
/// the broker's replies so the PUBACK never lands, the publish goes out and
/// reaches the subscriber once, the socket is then severed, and the same loop
/// reconnects. §6.1 predicts the subscriber sees the frame **twice**.
///
/// Whatever the count is, it is the reason `CommandRelay` carries a nonce
/// ledger and a `challenge_id` seen-set instead of trusting the session.
#[tokio::test]
#[ignore = "needs a broker: make mock-up"]
async fn a_publish_in_flight_across_a_disconnect_is_counted() {
    let topic = format!("q3/{}/inflight", uuid::Uuid::new_v4());
    let seen = spawn_counter(&topic).await;
    let proxy = Proxy::start().await;

    let (client, mut eventloop) = AsyncClient::new(
        portal_options("q3-inflight", BROKER, proxy.port),
        CLIENT_CAPACITY,
    );

    // One loop, polled for the whole test — this is the part that matters.
    let pump = tokio::spawn(async move {
        loop {
            if eventloop.poll().await.is_err() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(700)).await;

    // From here the client hears nothing back: the publish will go out and
    // stay unacked, which is exactly "in flight".
    proxy.swallow.store(true, Ordering::SeqCst);
    client
        .publish(topic.clone(), QoS::AtLeastOnce, false, "config_request-1")
        .await
        .expect("publish");
    tokio::time::sleep(Duration::from_millis(700)).await;
    let delivered_before_cut = seen.lock().await.len();

    // Cut it. The proxy keeps listening, so the reconnect works and the
    // replies flow again.
    proxy.swallow.store(false, Ordering::SeqCst);
    proxy.sever.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(12)).await; // rumqttc's reconnect
    pump.abort();

    let total = seen.lock().await.len();
    let reconnects = proxy.connections.load(Ordering::SeqCst);
    println!(
        "Q3(b) rumqttc 0.24.0: in-flight QoS-1 publish across a disconnect — \
         {delivered_before_cut} delivery before the cut, {total} total after reconnect \
         ({reconnects} proxy connections)"
    );
    assert!(
        reconnects >= 2,
        "the client must actually have reconnected for this to measure anything; \
         saw {reconnects} connection(s)"
    );
    // §6.1's claim is *twice* — once before the cut, once on the reconnect's
    // replay. Asserting `>= 1` (as this did until the second review) could
    // not fail if rumqttc ever stopped replaying, which is the one thing
    // §6.2's seen-set and nonce ledger are sized on; if a rumqttc upgrade
    // makes this read 1, that is the finding this test exists to raise.
    assert!(
        delivered_before_cut >= 1,
        "the publish must reach the broker before the cut; saw {delivered_before_cut}"
    );
    assert!(
        total >= 2,
        "§6.1: the in-flight publish is replayed on reconnect, so the subscriber \
         sees it at least twice; saw {total}"
    );
}

// **Q3 (a): the portal killed between challenge and confirm — not measured
// here, and the 2026-09-15 second review is why.**
//
// A process kill takes `CommandRelay`'s `pending` and `challenges` maps with
// it — they are in-memory by design, and phase 1 has no interactive fallback
// (`relay.rs` module doc) — so a restarted portal cannot answer a challenge it
// no longer has a `PendingCommand` for, and the design says the device sees
// **zero** confirms. That is a property of the relay, and it is pinned where
// the relay lives: `relay.rs::challenge_for_unknown_request_warns_without_publish`
// feeds a fresh `CommandRelay` a challenge for a `request_id` it never saw and
// asserts nothing is published.
//
// This file used to carry a wire version of (a) that published a raw request
// from one `AsyncClient`, aborted it, published a challenge from another, and
// asserted the confirm counter read zero. It did — but no `CommandRelay` was
// running anywhere in that test, so the zero was a property of the test rig,
// not a measurement of anything, and it would have read zero with a relay
// that re-confirmed everything. A `portal` is a binary crate with no library
// target, so an integration test here cannot construct the relay; rather than
// quote a number that measured nothing, the case is cited to the unit test
// that does prove it. status.md's "2 and 0" is therefore "2 measured, 0
// pinned by unit test" — the same fact, honestly labelled.
