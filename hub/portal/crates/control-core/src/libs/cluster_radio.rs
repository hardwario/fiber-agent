//! The cluster's radio mode, one value published to every Client (decision #48,
//! `plan/control/cluster.md` amendment 2026-10-29 "the Server publishes its radio mode").
//!
//! - On a **Server** a thread publishes, retained, `<prefix>/<client>/cluster/radio`
//!   `{"mode": …}` for every Client it has a broker account for. The value is this Hub's own
//!   effective `radio.mode` (the chosen one, else the rendered one), so the one place the mode
//!   lives is the Server's `fiber.config.yaml`. A change is published at the next [`TICK`]; an
//!   unchanged value is repeated every [`REPUBLISH_S`] so a broker restart loses nothing.
//! - On a **Client** the agent subscribes to its own `cluster/radio` on the Server's broker and
//!   applies a value that differs through [`radio_mode::apply`], origin `cluster`, like the signed
//!   `radio-set-mode`. No pairing or signature: the topic is in the Client's own family, which
//!   the broker ACL lets no other peer write (cluster.md §5), and the Server's agent writes it on
//!   the loopback listener.
//!
//! A removed Client's copy goes with the rest of its retained family (`cluster_retained`).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use crate::libs::config_applier::ConfigApplier;
use crate::libs::mqtt::messages::MqttMessage;
use crate::libs::radio_mode::{self, Changed, Machine, ThisHub};

/// How often the Server compares what it published with its mode.
const TICK: Duration = Duration::from_secs(10);
/// An unchanged value is republished at least this often.
const REPUBLISH_S: i64 = 600;
/// The audit origin of a mode taken from the Server.
pub const ORIGIN: &str = "cluster";

/// The document's payload.
pub fn payload(mode: &str) -> String {
    serde_json::json!({ "mode": mode }).to_string()
}

#[derive(Deserialize)]
struct Doc {
    mode: String,
}

/// The mode a payload carries. `None` for a tombstone or anything unreadable.
pub fn parse(payload: &[u8]) -> Option<String> {
    if payload.is_empty() {
        return None;
    }
    serde_json::from_slice::<Doc>(payload)
        .ok()
        .map(|d| d.mode.trim().to_ascii_lowercase())
        .filter(|m| !m.is_empty())
}

/// What the Server last published per Client: the mode and when.
#[derive(Default)]
pub struct Publisher(BTreeMap<String, (String, i64)>);

impl Publisher {
    /// The Clients due a publish of `mode` at `now`: new, changed, or older than
    /// [`REPUBLISH_S`]. Forgets Clients no longer listed.
    pub fn due(&mut self, mode: &str, clients: &[String], now: i64) -> Vec<String> {
        self.0.retain(|c, _| clients.contains(c));
        let mut out = Vec::new();
        for client in clients {
            let fresh = matches!(self.0.get(client),
                Some((m, at)) if m == mode && now - at < REPUBLISH_S);
            if !fresh {
                self.0.insert(client.clone(), (mode.to_string(), now));
                out.push(client.clone());
            }
        }
        out
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// This Hub's effective mode: chosen, else rendered.
fn own_mode(applier: &ConfigApplier) -> Option<String> {
    radio_mode::effective_mode(applier, || ThisHub.rendered_mode())
        .ok()
        .map(|m| m.as_str().to_string())
}

/// Start the Server's publisher. Inert on a Client (re-checked every tick, so a join or a
/// removal takes effect without a restart).
pub fn spawn(applier: Arc<ConfigApplier>, mqtt_tx: crossbeam::channel::Sender<MqttMessage>) {
    let spawned = std::thread::Builder::new()
        .name("cluster-radio".into())
        .spawn(move || {
            let mut publisher = Publisher::default();
            loop {
                std::thread::sleep(TICK);
                if crate::libs::beacon_cluster::is_client() {
                    publisher = Publisher::default();
                    continue;
                }
                let Some(mode) = own_mode(&applier) else {
                    continue;
                };
                let clients = crate::libs::beacon_cluster::known_clients();
                for hub in publisher.due(&mode, &clients, now_unix()) {
                    let msg = MqttMessage::PublishClusterRadio {
                        hub,
                        payload: payload(&mode),
                    };
                    if mqtt_tx.send(msg).is_err() {
                        return;
                    }
                }
            }
        });
    if let Err(e) = spawned {
        eprintln!("[cluster-radio] not started: {e}");
    }
}

/// A Client's incoming publish. `true` when it was this Hub's `cluster/radio` and needs nothing
/// else. Applies a mode that differs from this Hub's; a refusal is logged and retried on the
/// next publish.
pub fn on_publish(
    own_topic: &str,
    topic: &str,
    payload: &[u8],
    applier: &Option<Arc<ConfigApplier>>,
) -> bool {
    if topic != own_topic {
        return false;
    }
    if !crate::libs::beacon_cluster::is_client() {
        return true;
    }
    let (Some(mode), Some(applier)) = (parse(payload), applier.as_ref()) else {
        return true;
    };
    take(&mode, applier, &ThisHub);
    true
}

/// Apply the Server's mode on this Client.
pub fn take(mode: &str, applier: &ConfigApplier, machine: &dyn Machine) {
    match radio_mode::apply(mode, applier, ORIGIN, machine) {
        Ok(Changed::Unchanged(_)) => {}
        Ok(changed) => eprintln!("[cluster-radio] the Server's mode: {}", changed.message()),
        Err(e) => eprintln!("[cluster-radio] the Server's mode {mode} refused: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_payload_round_trips_and_a_tombstone_is_nothing() {
        assert_eq!(parse(payload("p2p").as_bytes()).as_deref(), Some("p2p"));
        assert_eq!(
            parse(br#"{"mode":" LoRaWAN "}"#).as_deref(),
            Some("lorawan")
        );
        assert_eq!(parse(b""), None);
        assert_eq!(parse(b"nonsense"), None);
    }

    #[test]
    fn a_client_is_due_when_new_changed_or_old() {
        let mut p = Publisher::default();
        let clients = vec!["proximos-a".to_string(), "proximos-b".to_string()];
        assert_eq!(p.due("lorawan", &clients, 100), clients);
        assert!(p.due("lorawan", &clients, 110).is_empty());
        assert_eq!(p.due("p2p", &clients, 120), clients);
        assert!(p.due("p2p", &clients[..1], 130).is_empty());
        // Forgotten when unlisted, so it is due again when it comes back.
        assert_eq!(p.due("p2p", &clients, 140), vec!["proximos-b".to_string()]);
        assert_eq!(
            p.due("p2p", &clients[..1], 120 + REPUBLISH_S),
            vec!["proximos-a".to_string()]
        );
    }
}
