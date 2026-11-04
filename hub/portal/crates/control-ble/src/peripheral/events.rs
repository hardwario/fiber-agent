//! What the FB00 server tells its host about, so an LCD can show it.
//!
//! v1 had `libs/ble/event_router.rs` — a channel from the GATT server to the display thread, so a
//! phone connecting lit the screen. That router lived on the *server* side of the split, which is
//! why it does not survive: the server moves into this crate, and the display stays in the host.
//! The seam that replaces it is this enum plus a `tokio::sync::broadcast` sender on
//! [`crate::tasks::PeripheralHandle`].
//!
//! **Broadcast, not mpsc, and lossy on purpose.** A host with no LCD (a client-mode Hub, decision
//! #5) subscribes to nothing, and the server must not block or grow a queue because of that. A slow
//! subscriber loses the oldest events (`RecvError::Lagged`) rather than stalling a GATT write —
//! these are display hints, and a stale hint is worse than a dropped one.
//!
//! **Nothing here carries a credential.** The session token never appears: `AuthSucceeded` says a
//! peer authenticated, not what it wrote. That is the same rule
//! `plan/control/ble/peripheral.md` §3 puts on the CLI preview.

/// One thing worth showing on a screen.
///
/// `Serialize` so a host can forward these verbatim (to MQTT, to a log) without re-describing them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PeripheralEvent {
    /// A central connected to the adapter. `addr` is the phone, not this Hub.
    ClientConnected { addr: String },
    /// The connection went away — whether it disconnected, timed out or we tore it down.
    ClientDisconnected { addr: String },
    /// FB01 accepted a token. The peer is now authorised for this session.
    AuthSucceeded { addr: String },
    /// FB01 refused a token. Not fatal, not rate-limited here (the session's lifetime is the limit).
    AuthFailed { addr: String },
    /// FB03 brought Wi-Fi up.
    WifiConnected { ssid: String, ip: String },
    /// FB03 could not bring Wi-Fi up. `error` is the host's own message, already operator-facing.
    WifiFailed { ssid: String, error: String },
    /// FB0A applied a LAN configuration. `mode` is `dhcp` or `static`.
    LanConfigured { mode: String, ip: String },
    /// FB0A refused or failed a LAN configuration.
    LanFailed { mode: String, error: String },
}

impl PeripheralEvent {
    /// A one-line form for a 20×4 character LCD or a log line.
    ///
    /// Deliberately in this crate rather than the host: the wording follows the protocol, and two
    /// hosts should not invent two vocabularies for the same transition.
    pub fn summary(&self) -> String {
        match self {
            PeripheralEvent::ClientConnected { addr } => format!("phone connected {addr}"),
            PeripheralEvent::ClientDisconnected { addr } => format!("phone disconnected {addr}"),
            PeripheralEvent::AuthSucceeded { addr } => format!("phone authenticated {addr}"),
            PeripheralEvent::AuthFailed { addr } => format!("phone auth failed {addr}"),
            PeripheralEvent::WifiConnected { ssid, ip } => format!("wifi {ssid} {ip}"),
            PeripheralEvent::WifiFailed { ssid, error } => format!("wifi {ssid} failed: {error}"),
            PeripheralEvent::LanConfigured { mode, ip } => format!("lan {mode} {ip}"),
            PeripheralEvent::LanFailed { mode, error } => format!("lan {mode} failed: {error}"),
        }
    }
}

/// The sender side, held by the GATT context and cloned into [`crate::tasks::PeripheralHandle`].
///
/// A newtype rather than a bare `broadcast::Sender` so `emit` can be the only way to publish: it
/// swallows the "no subscribers" error, which is the normal case on a headless Hub and not a fault.
#[derive(Debug, Clone)]
pub struct EventBus {
    tx: tokio::sync::broadcast::Sender<PeripheralEvent>,
}

/// Deep enough that a burst of connect + auth + wifi cannot outrun a display thread that ticks at
/// human speed; shallow enough that a subscriber which stopped reading cannot pin much memory.
pub const EVENT_BUFFER: usize = 64;

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> EventBus {
        let (tx, _rx) = tokio::sync::broadcast::channel(EVENT_BUFFER);
        EventBus { tx }
    }

    /// Publish. Never fails, never blocks: with no subscribers the event is dropped.
    pub fn emit(&self, event: PeripheralEvent) {
        let _ = self.tx.send(event);
    }

    /// Subscribe. Only events emitted *after* this call arrive.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<PeripheralEvent> {
        self.tx.subscribe()
    }

    /// How many receivers are listening — used by tests and by a host deciding whether to spawn a
    /// forwarding task at all.
    pub fn subscribers(&self) -> usize {
        self.tx.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emitting_with_no_subscribers_is_not_an_error() {
        // The headless-Hub case (decision #5). If this ever panics or blocks, a client-mode Hub
        // cannot serve FB00 at all.
        let bus = EventBus::new();
        assert_eq!(bus.subscribers(), 0);
        for _ in 0..EVENT_BUFFER * 4 {
            bus.emit(PeripheralEvent::ClientDisconnected {
                addr: "AA:BB:CC:DD:EE:FF".into(),
            });
        }
    }

    #[test]
    fn a_subscriber_sees_events_emitted_after_it_subscribed() {
        let bus = EventBus::new();
        bus.emit(PeripheralEvent::AuthFailed {
            addr: "before".into(),
        });
        let mut rx = bus.subscribe();
        assert_eq!(bus.subscribers(), 1);
        bus.emit(PeripheralEvent::AuthSucceeded {
            addr: "after".into(),
        });
        assert_eq!(
            rx.try_recv().expect("one event"),
            PeripheralEvent::AuthSucceeded {
                addr: "after".into()
            },
            "the pre-subscription event is not replayed"
        );
        assert!(rx.try_recv().is_err(), "and there is only the one");
    }

    #[test]
    fn two_subscribers_both_see_every_event() {
        // The display thread and (later) an MQTT forwarder are two independent consumers.
        let bus = EventBus::new();
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        bus.emit(PeripheralEvent::WifiConnected {
            ssid: "lab".into(),
            ip: "10.0.0.5".into(),
        });
        assert_eq!(a.try_recv().expect("a"), b.try_recv().expect("b"));
    }

    #[test]
    fn a_lagging_subscriber_loses_events_rather_than_stalling_the_server() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        for i in 0..EVENT_BUFFER + 10 {
            bus.emit(PeripheralEvent::AuthFailed {
                addr: format!("{i}"),
            });
        }
        // First read reports the loss; the channel then continues from the oldest survivor.
        let err = rx.try_recv().expect_err("lagged");
        assert!(
            matches!(err, tokio::sync::broadcast::error::TryRecvError::Lagged(n) if n == 10),
            "{err:?}"
        );
        assert!(rx.try_recv().is_ok(), "and reading resumes");
    }

    #[test]
    fn summaries_are_one_line_and_name_the_peer() {
        let cases = [
            (
                PeripheralEvent::ClientConnected {
                    addr: "AA:BB:CC:DD:EE:FF".into(),
                },
                "phone connected AA:BB:CC:DD:EE:FF",
            ),
            (
                PeripheralEvent::LanFailed {
                    mode: "static".into(),
                    error: "gateway unreachable".into(),
                },
                "lan static failed: gateway unreachable",
            ),
        ];
        for (event, expected) in cases {
            let s = event.summary();
            assert_eq!(s, expected);
            assert!(!s.contains('\n'), "one line: {s}");
        }
    }

    #[test]
    fn the_serialised_form_is_tagged_and_carries_no_credential() {
        let json = serde_json::to_string(&PeripheralEvent::AuthSucceeded {
            addr: "AA:BB:CC:DD:EE:FF".into(),
        })
        .expect("serialize");
        assert_eq!(
            json,
            r#"{"event":"auth_succeeded","addr":"AA:BB:CC:DD:EE:FF"}"#
        );
        for forbidden in ["tok", "token", "pin", "password"] {
            assert!(!json.contains(forbidden), "{forbidden} in {json}");
        }
        // The Wi-Fi password is the other credential in reach of these handlers.
        let json = serde_json::to_string(&PeripheralEvent::WifiFailed {
            ssid: "lab".into(),
            error: "auth failed".into(),
        })
        .expect("serialize");
        assert!(!json.contains("password"), "{json}");
    }
}
