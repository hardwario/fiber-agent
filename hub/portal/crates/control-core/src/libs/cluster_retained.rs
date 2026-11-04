//! Clear a removed Client's retained documents from this Server's broker
//! (plan/control/cluster.md, amendment 2026-10-28, #168).
//!
//! A Client publishes its topic family `<prefix>/<hostname>/…` on the Server's broker, much of it
//! retained (`system/info`, `config/state`, node info, …). After `cluster-client-remove` the
//! account is gone, but those documents stay, and a restarted portal reads the Hub back in from
//! them. The agent clears them here: the portal never publishes retained (mqtt.md).
//!
//! The broker cannot list retained topics, so they are discovered: subscribe to the family on
//! the loopback listener, collect every topic that arrives with a non-empty retained payload
//! until the stream has been quiet for [`QUIET`] (at most [`MAX_COLLECT`]), then publish an empty
//! retained payload to each. The Client's account no longer exists, so nothing republishes them.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use rumqttc::{Client, Event, MqttOptions, Packet, QoS};

/// The loopback listener: anonymous, no ACL (cluster.md §5 #4).
pub const LOOPBACK_HOST: &str = "127.0.0.1";
pub const LOOPBACK_PORT: u16 = 1883;

/// The retained burst after a subscribe is over once nothing arrives for this long.
pub const QUIET: Duration = Duration::from_secs(1);
/// Upper bound on the whole collection, however chatty the family is.
pub const MAX_COLLECT: Duration = Duration::from_secs(5);
/// Upper bound on waiting for the tombstones' acknowledgements.
const MAX_FLUSH: Duration = Duration::from_secs(5);

/// The subscription filter for one Hub's family. `hostname` is validated by the caller (it is
/// a topic segment and an ACL line already), so it holds no wildcard or separator.
pub fn family_filter(prefix: &str, hostname: &str) -> String {
    format!("{prefix}/{hostname}/#")
}

/// Whether a delivered message is a retained document to tombstone: retained, non-empty (an
/// empty one is already a tombstone), and inside the family.
fn is_retained_document(filter_root: &str, topic: &str, retain: bool, payload: &[u8]) -> bool {
    retain && !payload.is_empty() && topic.starts_with(filter_root)
}

/// Clear the retained documents under `<prefix>/<hostname>/#` on the broker at `host:port`.
/// Returns how many topics were tombstoned.
pub fn clear(host: &str, port: u16, prefix: &str, hostname: &str) -> Result<usize, String> {
    let filter = family_filter(prefix, hostname);
    let root = format!("{prefix}/{hostname}/");
    let mut opts = MqttOptions::new(format!("cluster-clear-{hostname}"), host, port);
    opts.set_keep_alive(Duration::from_secs(10));
    opts.set_clean_session(true);
    opts.set_max_packet_size(256 * 1024, 256 * 1024);
    let (client, mut connection) = Client::new(opts, 64);
    client
        .subscribe(filter.as_str(), QoS::AtLeastOnce)
        .map_err(|e| format!("subscribe {filter}: {e}"))?;

    let mut topics = BTreeSet::new();
    let started = Instant::now();
    let mut subscribed = false;
    let mut last = Instant::now();
    while started.elapsed() < MAX_COLLECT {
        // Before the SubAck the broker has sent nothing yet, so only wait out the quiet after it.
        if subscribed && last.elapsed() >= QUIET {
            break;
        }
        match connection.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(Event::Incoming(Packet::SubAck(_)))) => {
                subscribed = true;
                last = Instant::now();
            }
            Ok(Ok(Event::Incoming(Packet::Publish(p)))) => {
                last = Instant::now();
                if is_retained_document(&root, &p.topic, p.retain, &p.payload) {
                    topics.insert(p.topic.clone());
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(format!("broker {host}:{port}: {e}")),
            Err(_) => {}
        }
    }
    if !subscribed {
        return Err(format!("broker {host}:{port}: no answer to the subscribe"));
    }
    let _ = client.unsubscribe(filter.as_str());

    for topic in &topics {
        client
            .publish(topic.as_str(), QoS::AtLeastOnce, true, Vec::<u8>::new())
            .map_err(|e| format!("tombstone {topic}: {e}"))?;
    }
    let mut acked = 0usize;
    let started = Instant::now();
    while acked < topics.len() && started.elapsed() < MAX_FLUSH {
        match connection.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(Event::Incoming(Packet::PubAck(_)))) => acked += 1,
            Ok(Ok(_)) | Err(_) => {}
            Ok(Err(e)) => return Err(format!("broker {host}:{port}: {e}")),
        }
    }
    let _ = client.disconnect();
    if acked < topics.len() {
        return Err(format!(
            "{acked} of {} tombstones acknowledged",
            topics.len()
        ));
    }
    Ok(topics.len())
}

/// [`clear`] against the loopback listener.
pub fn clear_loopback(prefix: &str, hostname: &str) -> Result<usize, String> {
    clear(LOOPBACK_HOST, LOOPBACK_PORT, prefix, hostname)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_filter_is_the_hubs_family() {
        assert_eq!(
            family_filter("proximos", "proximos-c23e465c"),
            "proximos/proximos-c23e465c/#"
        );
    }

    #[test]
    fn only_non_empty_retained_documents_of_the_family_are_cleared() {
        let root = "proximos/proximos-c23e465c/";
        assert!(is_retained_document(
            root,
            "proximos/proximos-c23e465c/system/info",
            true,
            b"{}"
        ));
        assert!(!is_retained_document(
            root,
            "proximos/proximos-c23e465c/system/info",
            false,
            b"{}"
        ));
        assert!(!is_retained_document(
            root,
            "proximos/proximos-c23e465c/system/info",
            true,
            b""
        ));
        assert!(!is_retained_document(
            root,
            "proximos/proximos-c23e465cd/system/info",
            true,
            b"{}"
        ));
    }

    #[test]
    fn no_broker_is_an_error_not_a_hang() {
        // Port 1 on loopback refuses at once.
        let started = Instant::now();
        assert!(clear("127.0.0.1", 1, "proximos", "proximos-00000001").is_err());
        assert!(started.elapsed() <= MAX_COLLECT + Duration::from_secs(1));
    }
}
