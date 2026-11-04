//! The central's side of the gateway plane — `plan/control/radio/p2p_cluster_gateway.md` §4.
//!
//! A Client Hub's Northbridge reaches the Server's central as `up` messages on the Server's
//! broker ([`crate::p2p::plane`]). [`PlaneHub`] holds the central's one broker session for the
//! plane: it routes each gateway's `up` frames to that gateway's [`PlaneLink`] and reports every
//! gateway's retained `state`. A [`RemoteModem`] is the same [`FramedModem`] the local UART
//! uses, over a [`PlaneLink`] instead, with a longer `RSP` budget: one network round trip and
//! two broker hops sit between the command and its answer.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
use tokio::sync::mpsc;

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::p2p::gw_board::GatewayBoard;
use crate::p2p::hdlc::Frame;
use crate::p2p::modem_io::{FramedModem, LinkIo};
use crate::p2p::plane::{self, GatewayState, PlaneFrame, PlaneTopic, UpFrame};
use crate::p2p::runtime::GatewayChange;

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

macro_rules! diag {
    ($($arg:tt)*) => { eprintln!("[p2p {}] {}", now_ms(), format_args!($($arg)*)) };
}

/// A remote gateway's `RSP` budget (§4): 800 ms locally, plus the LAN and two broker hops.
pub const REMOTE_RSP_TIMEOUT: Duration = Duration::from_secs(2);

/// How many `up` frames wait for a gateway the central is busy with before new ones are
/// dropped. The ring resends what is dropped (link doc §4.1).
const ROUTE_DEPTH: usize = 256;

/// How often [`follow`] attaches again a gateway the central dropped (a dead link, a failed
/// restore) while its `state` still says it is serviceable.
pub const REATTACH_EVERY: Duration = Duration::from_secs(10);

/// The broker client id of the central's plane session.
pub const CLIENT_ID: &str = "proximos-control-p2p-plane";

/// Where a command to a remote gateway is published.
pub trait DownSink: Send + Sync {
    fn publish_down(&self, gw: &str, payload: Vec<u8>) -> Result<()>;
}

impl DownSink for AsyncClient {
    fn publish_down(&self, gw: &str, payload: Vec<u8>) -> Result<()> {
        self.try_publish(plane::down_topic(gw), QoS::AtMostOnce, false, payload)
            .map_err(|e| {
                Error::new(
                    Kind::ServiceDown,
                    Transport::P2p,
                    Component::MqttBroker,
                    format!("gateway {gw}: command not published: {e}"),
                )
            })
    }
}

/// One remote gateway's link: commands out through the plane, its `up` frames in.
pub struct PlaneLink {
    gw: String,
    sink: Arc<dyn DownSink>,
    rx: mpsc::Receiver<UpFrame>,
    last_host_unix_ms: Option<u64>,
}

impl std::fmt::Debug for PlaneLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaneLink")
            .field("gw", &self.gw)
            .finish_non_exhaustive()
    }
}

impl PlaneLink {
    pub fn new(gw: String, sink: Arc<dyn DownSink>, rx: mpsc::Receiver<UpFrame>) -> Self {
        PlaneLink {
            gw,
            sink,
            rx,
            last_host_unix_ms: None,
        }
    }

    fn detached(&self) -> Error {
        Error::new(
            Kind::ServiceDown,
            Transport::P2p,
            Component::MqttBroker,
            format!("gateway {}: detached from the plane", self.gw),
        )
    }

    fn take(&mut self, first: UpFrame) -> Vec<Frame> {
        let mut out = Vec::new();
        let mut next = Some(first);
        while let Some(up) = next {
            self.last_host_unix_ms = Some(up.host_unix_ms);
            out.push(Frame {
                frame_type: up.frame.frame_type,
                seq: up.frame.seq,
                body: up.frame.body,
            });
            next = self.rx.try_recv().ok();
        }
        out
    }
}

#[async_trait::async_trait]
impl LinkIo for PlaneLink {
    async fn write_frame(&mut self, frame_type: u8, seq: u8, body: &[u8]) -> Result<()> {
        let msg = plane::encode_down(&PlaneFrame {
            frame_type,
            seq,
            body: body.to_vec(),
        });
        self.sink.publish_down(&self.gw, msg)
    }

    async fn read_frames(&mut self) -> Result<Vec<Frame>> {
        match self.rx.recv().await {
            Some(first) => Ok(self.take(first)),
            None => Err(self.detached()),
        }
    }

    async fn read_frames_within(&mut self, deadline: Duration) -> Result<Vec<Frame>> {
        match tokio::time::timeout(deadline, self.rx.recv()).await {
            Ok(Some(first)) => Ok(self.take(first)),
            Ok(None) => Err(self.detached()),
            Err(_) => Err(Error::new(
                Kind::Timeout,
                Transport::P2p,
                Component::NorthbridgeUart,
                format!(
                    "gateway {}: no frame within {} ms",
                    self.gw,
                    deadline.as_millis()
                ),
            )),
        }
    }

    fn last_frame_host_unix_ms(&self) -> Option<u64> {
        self.last_host_unix_ms
    }
}

/// A Client Hub's Northbridge, driven by the central over the plane.
pub type RemoteModem = FramedModem<PlaneLink>;

/// What the plane tells the central about its gateways.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayEvent {
    /// A gateway's retained `state`, as published (or as its last will).
    State(String, GatewayState),
    /// The plane session (re)connected: every retained `state` follows again.
    Connected,
}

type Routes = Arc<Mutex<HashMap<String, mpsc::Sender<UpFrame>>>>;

/// The central's plane session: one broker client, one route per attached gateway.
#[derive(Clone)]
pub struct PlaneHub {
    sink: Arc<dyn DownSink>,
    routes: Routes,
}

impl std::fmt::Debug for PlaneHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaneHub").finish_non_exhaustive()
    }
}

impl PlaneHub {
    /// A hub over any sink, routing what [`PlaneHub::route`] is handed — the seam the tests
    /// drive without a broker.
    pub fn with_sink(sink: Arc<dyn DownSink>) -> Self {
        PlaneHub {
            sink,
            routes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Connect to the local broker (`northbound::BROKER_ADDR`) and start routing. Gateway events
    /// arrive on the returned receiver; `own` (this Hub's hostname) is never a remote gateway.
    pub fn connect(own: String) -> (Self, mpsc::Receiver<GatewayEvent>) {
        let (host, port) = crate::northbound::BROKER_ADDR
            .split_once(':')
            .map(|(h, p)| (h.to_string(), p.parse().unwrap_or(1883)))
            .unwrap_or(("127.0.0.1".into(), 1883));
        let mut o = MqttOptions::new(CLIENT_ID, host, port);
        o.set_keep_alive(Duration::from_secs(30));
        o.set_clean_session(true);
        o.set_max_packet_size(64 * 1024, 64 * 1024);
        let (client, mut eventloop) = AsyncClient::new(o, 256);
        let hub = PlaneHub::with_sink(Arc::new(client.clone()));
        let (tx, rx) = mpsc::channel(64);
        let routing = hub.clone();
        // The session ends with the central run that reads `rx`, so the next run's session,
        // under the same client id, never meets this one on the broker.
        tokio::spawn(async move {
            let mut failures = 0u32;
            loop {
                let polled = tokio::select! {
                    () = tx.closed() => break,
                    polled = eventloop.poll() => polled,
                };
                match polled {
                    Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                        failures = 0;
                        for f in plane::central_filters() {
                            let _ = client.subscribe(f, QoS::AtMostOnce).await;
                        }
                        if tx.send(GatewayEvent::Connected).await.is_err() {
                            break;
                        }
                    }
                    Ok(Event::Incoming(Incoming::Publish(p))) => {
                        if let Some(ev) = routing.route(&own, &p.topic, &p.payload) {
                            if tx.send(ev).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        failures = failures.saturating_add(1);
                        if failures == 1 || failures.is_multiple_of(30) {
                            diag!("gateway plane: broker {e} (attempt {failures}); retrying");
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        });
        (hub, rx)
    }

    /// Handle one plane message: an `up` frame goes to its gateway's route (dropped when none
    /// is attached or it is full), a `state` becomes a [`GatewayEvent`].
    pub fn route(&self, own: &str, topic: &str, payload: &[u8]) -> Option<GatewayEvent> {
        match plane::parse_topic(topic)? {
            PlaneTopic::Up(gw) if gw != own => {
                match plane::decode_up(payload) {
                    Ok(up) => {
                        let tx = self
                            .routes
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .get(&gw)
                            .cloned();
                        if let Some(tx) = tx {
                            let _ = tx.try_send(up);
                        }
                    }
                    Err(e) => diag!("gateway {gw}: up message refused: {e}"),
                }
                None
            }
            PlaneTopic::State(gw) if gw != own => match GatewayState::parse(payload) {
                Some(state) => Some(GatewayEvent::State(gw, state)),
                // An empty retained message clears the topic: the gateway is gone.
                None if payload.is_empty() => {
                    Some(GatewayEvent::State(gw, GatewayState::offline()))
                }
                None => {
                    diag!("gateway {gw}: state document not understood");
                    None
                }
            },
            _ => None,
        }
    }

    /// Attach a gateway: a fresh route (replacing any earlier one, whose modem then reads
    /// `detached`) and a modem over it.
    pub fn attach(&self, gw: &str) -> RemoteModem {
        self.attach_within(gw, REMOTE_RSP_TIMEOUT)
    }

    fn attach_within(&self, gw: &str, rsp_timeout: Duration) -> RemoteModem {
        let (tx, rx) = mpsc::channel(ROUTE_DEPTH);
        self.routes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(gw.to_string(), tx);
        FramedModem::with_timeout(
            PlaneLink::new(gw.to_string(), self.sink.clone(), rx),
            rsp_timeout,
        )
    }

    /// Whether `gw` has a route a modem still reads: `false` once the central dropped the modem.
    pub fn is_attached(&self, gw: &str) -> bool {
        self.routes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(gw)
            .is_some_and(|tx| !tx.is_closed())
    }

    /// Detach a gateway: its modem's next read is `detached`.
    pub fn detach(&self, gw: &str) {
        self.routes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(gw);
    }
}

/// Turn the plane's gateway events into the central's [`GatewayChange`]s (§4): a gateway is
/// attached while its `state` says it is serviceable, detached when it stops being, and
/// attached again every [`REATTACH_EVERY`] when the central dropped it while its `state` still
/// says it is serviceable. An online `state` also hands the Client's Northbridge update to
/// `board`, for the Northbridge document the central publishes for it (§8). Ends when either
/// side goes away.
pub async fn follow(
    plane: PlaneHub,
    events: mpsc::Receiver<GatewayEvent>,
    changes: mpsc::Sender<GatewayChange>,
    board: GatewayBoard,
) {
    follow_every(plane, events, changes, board, REATTACH_EVERY).await;
}

async fn follow_every(
    plane: PlaneHub,
    mut events: mpsc::Receiver<GatewayEvent>,
    changes: mpsc::Sender<GatewayChange>,
    board: GatewayBoard,
    every: Duration,
) {
    let mut serviceable: BTreeMap<String, bool> = BTreeMap::new();
    let mut retry = tokio::time::interval(every);
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let due: Vec<String> = tokio::select! {
            () = changes.closed() => return,
            ev = events.recv() => match ev {
                None => return,
                Some(GatewayEvent::Connected) => Vec::new(),
                Some(GatewayEvent::State(gw, st)) => {
                    // Before a detach, which reads whether the Northbridge is being flashed.
                    board.on_state(&gw, &st, crate::lorawan::gateway::now_unix());
                    let ok = st.serviceable();
                    let was = serviceable.insert(gw.clone(), ok).unwrap_or(false);
                    if !ok && (was || plane.is_attached(&gw)) {
                        diag!(
                            "gateway {gw}: {:?}, Northbridge {:?} — detaching",
                            st.state,
                            st.northbridge
                        );
                        plane.detach(&gw);
                        if changes.send(GatewayChange::Offline(gw.clone())).await.is_err() {
                            return;
                        }
                    }
                    if ok && !plane.is_attached(&gw) {
                        vec![gw]
                    } else {
                        Vec::new()
                    }
                }
            },
            _ = retry.tick() => serviceable
                .iter()
                .filter(|(gw, ok)| **ok && !plane.is_attached(gw))
                .map(|(gw, _)| gw.clone())
                .collect(),
        };
        for gw in due {
            diag!("gateway {gw}: online — attaching");
            let modem = plane.attach(&gw);
            if changes
                .send(GatewayChange::Online(gw, Box::new(modem)))
                .await
                .is_err()
            {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::p2p::modem::{self, Event as ModemEvent};
    use crate::p2p::modem_io::Modem;

    /// Records what the central publishes, and lets the test answer it.
    #[derive(Default)]
    struct Sink(Mutex<Vec<(String, Vec<u8>)>>);

    impl DownSink for Sink {
        fn publish_down(&self, gw: &str, payload: Vec<u8>) -> Result<()> {
            self.0.lock().unwrap().push((gw.to_string(), payload));
            Ok(())
        }
    }

    fn up(gw: &str, frame_type: u8, seq: u8, body: Vec<u8>, at: u64) -> (String, Vec<u8>) {
        (
            plane::up_topic(gw),
            plane::encode_up(&UpFrame {
                host_unix_ms: at,
                frame: PlaneFrame {
                    frame_type,
                    seq,
                    body,
                },
            }),
        )
    }

    #[tokio::test]
    async fn a_command_matches_its_rsp_by_seq_and_keeps_events_in_order() {
        let sink = Arc::new(Sink::default());
        let hub = PlaneHub::with_sink(sink.clone());
        let mut m = hub.attach("gw-b");

        // An event, a stale RSP and then the right one arrive while GET_INFO waits.
        let mut info = vec![0u8]; // status OK
        info.extend(vec![0u8; crate::p2p::modem_io::ModemInfo::LEN]);
        let boot = vec![0, 4, 0, 1, 2, 3, 4, 0];
        let replies = vec![
            up("gw-b", modem::evt::BOOT, 9, boot.clone(), 100),
            up(
                "gw-b",
                modem::RSP_FLAG | modem::cmd::GET_INFO,
                0,
                vec![0],
                101,
            ),
            up("gw-b", modem::RSP_FLAG | modem::cmd::GET_INFO, 1, info, 102),
        ];
        let feeder = {
            let hub = hub.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                for (t, p) in replies {
                    assert_eq!(hub.route("gw-a", &t, &p), None);
                }
            })
        };
        let i = m.get_info().await.expect("answered");
        feeder.await.unwrap();
        assert_eq!(i.now_ms, 0);
        assert_eq!(
            m.last_rsp_host_unix_ms(),
            Some(102),
            "the Client's stamp of the RSP"
        );

        let sent = sink.0.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "gw-b");
        assert_eq!(
            plane::decode_down(&sent[0].1).unwrap(),
            PlaneFrame {
                frame_type: modem::cmd::GET_INFO,
                seq: 1,
                body: vec![]
            }
        );
        match m.next_event().await.expect("the stashed event") {
            ModemEvent::Boot(b) => assert_eq!(b.fw_ver, [0, 4, 0]),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_silent_gateway_times_out_and_a_detached_one_is_a_dead_link() {
        let hub = PlaneHub::with_sink(Arc::new(Sink::default()));
        let mut m = hub.attach_within("gw-b", Duration::from_millis(50));
        let err = m.get_info().await.expect_err("no answer");
        assert_eq!(err.kind(), Kind::Timeout);
        hub.detach("gw-b");
        let err = m.next_event().await.expect_err("detached");
        assert_eq!(err.kind(), Kind::ServiceDown);
    }

    #[test]
    fn states_are_reported_for_other_hubs_only() {
        let hub = PlaneHub::with_sink(Arc::new(Sink::default()));
        let on = GatewayState::online(None, plane::NorthbridgeLink::Up);
        assert_eq!(
            hub.route("own", &plane::state_topic("gw-b"), &on.to_json()),
            Some(GatewayEvent::State("gw-b".into(), on.clone()))
        );
        assert_eq!(
            hub.route("own", &plane::state_topic("own"), &on.to_json()),
            None
        );
        assert_eq!(
            hub.route("own", &plane::state_topic("gw-b"), b""),
            Some(GatewayEvent::State("gw-b".into(), GatewayState::offline()))
        );
        // An up frame for a gateway nobody attached is dropped quietly.
        let (t, p) = up("gw-c", 0x84, 1, vec![], 1);
        assert_eq!(hub.route("own", &t, &p), None);
    }

    fn up_state() -> GatewayState {
        GatewayState::online(None, plane::NorthbridgeLink::Up)
    }

    async fn next_change(rx: &mut mpsc::Receiver<GatewayChange>) -> GatewayChange {
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("a change in time")
            .expect("open")
    }

    fn quiet(rx: &mut mpsc::Receiver<GatewayChange>) -> bool {
        matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty))
    }

    /// `online` attaches once, `offline` detaches; a Northbridge that is down is not served.
    #[tokio::test]
    async fn a_gateway_is_attached_while_its_state_is_serviceable() {
        let plane = PlaneHub::with_sink(Arc::new(Sink::default()));
        let board = GatewayBoard::default();
        let (ev_tx, ev_rx) = mpsc::channel(8);
        let (ch_tx, mut ch_rx) = mpsc::channel(8);
        tokio::spawn(follow_every(
            plane.clone(),
            ev_rx,
            ch_tx,
            board.clone(),
            Duration::from_secs(60),
        ));

        ev_tx
            .send(GatewayEvent::State("gw-b".into(), up_state()))
            .await
            .expect("send");
        let modem = match next_change(&mut ch_rx).await {
            GatewayChange::Online(gw, m) => {
                assert_eq!(gw, "gw-b");
                m
            }
            GatewayChange::Offline(_) => panic!("online expected"),
        };
        assert!(plane.is_attached("gw-b"));

        // The same state again (a reconnect's retained replay) changes nothing.
        ev_tx
            .send(GatewayEvent::State("gw-b".into(), up_state()))
            .await
            .expect("send");
        ev_tx.send(GatewayEvent::Connected).await.expect("send");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(quiet(&mut ch_rx));

        ev_tx
            .send(GatewayEvent::State(
                "gw-b".into(),
                GatewayState::online(None, plane::NorthbridgeLink::Updating).with_update(Some(
                    &crate::p2p::northbridge::UpdateSummary {
                        state: "flashing",
                        ..Default::default()
                    },
                )),
            ))
            .await
            .expect("send");
        assert!(matches!(
            next_change(&mut ch_rx).await,
            GatewayChange::Offline(gw) if gw == "gw-b"
        ));
        assert!(!plane.is_attached("gw-b"));
        drop(modem);
        // The will that follows keeps the update the Client last reported.
        ev_tx
            .send(GatewayEvent::State("gw-b".into(), GatewayState::offline()))
            .await
            .expect("send");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let status = crate::p2p::northbridge::NorthbridgeStatus::default();
        board.online(
            "gw-b",
            status.clone(),
            crate::p2p::runtime::Liveness::default(),
        );
        assert_eq!(
            status.snapshot().update.map(|u| u.state),
            Some("flashing"),
            "the board took the Client's update"
        );

        ev_tx
            .send(GatewayEvent::State(
                "gw-c".into(),
                GatewayState::online(None, plane::NorthbridgeLink::Down),
            ))
            .await
            .expect("send");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            quiet(&mut ch_rx),
            "a gateway without its Northbridge is not served"
        );
    }

    /// A gateway the central dropped is attached again while its state still says online.
    #[tokio::test]
    async fn a_dropped_gateway_is_attached_again() {
        let plane = PlaneHub::with_sink(Arc::new(Sink::default()));
        let (ev_tx, ev_rx) = mpsc::channel(8);
        let (ch_tx, mut ch_rx) = mpsc::channel(8);
        tokio::spawn(follow_every(
            plane.clone(),
            ev_rx,
            ch_tx,
            GatewayBoard::default(),
            Duration::from_millis(50),
        ));
        ev_tx
            .send(GatewayEvent::State("gw-b".into(), up_state()))
            .await
            .expect("send");
        let first = next_change(&mut ch_rx).await;
        assert!(matches!(&first, GatewayChange::Online(gw, _) if gw == "gw-b"));
        drop(first);
        assert!(!plane.is_attached("gw-b"), "the central dropped its modem");
        assert!(matches!(
            next_change(&mut ch_rx).await,
            GatewayChange::Online(gw, _) if gw == "gw-b"
        ));
    }

    /// The bridge ends with the central run that reads its changes.
    #[tokio::test]
    async fn the_bridge_ends_with_the_central() {
        let plane = PlaneHub::with_sink(Arc::new(Sink::default()));
        let (_ev_tx, ev_rx) = mpsc::channel(8);
        let (ch_tx, ch_rx) = mpsc::channel(8);
        let task = tokio::spawn(follow_every(
            plane,
            ev_rx,
            ch_tx,
            GatewayBoard::default(),
            Duration::from_secs(60),
        ));
        drop(ch_rx);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("ended")
            .expect("joined");
    }
}
