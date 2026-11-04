//! The P2P arm of the broker-connect replay — `node_reported_state.md` §3.
//!
//! `proximos-control` republishes every enrolled node's retained documents after each broker
//! connect, because the Hub's mosquitto keeps no retained state across a restart. Until
//! 2026-09-26 it walked only the LoRaWAN enrollments: after a Hub restart a P2P Node had no
//! `info` at all, and a DevEUI enrolled on both carriers got the LoRaWAN record's `info`
//! (`transport: lorawan`) and the LoRaWAN cache's `device_info` and `config` — the portal then
//! labelled the P2P Node "LoRaWAN" and showed a config dump from before it moved to P2P (night
//! test 2026-09-26, PN-1).

use serde_json::{Map, Value};

use super::session::{RegisteredNode, Session, SessionState};
use crate::cache::CacheDocument;
use crate::northbound::{Area, Topics};
use crate::tasks::uplink::{self, Publish};

/// The retained `info` of a node the central holds a session for.
///
/// The one builder of the joined document: the join publishes it (`runtime.rs`) and the
/// replay rebuilds it from the stored session, so the two cannot drift. Built on
/// [`RegisteredNode::info_core`], the part `node-add` publishes at registration.
pub fn joined_info(
    node: &RegisteredNode,
    session: &Session,
    net_id: Option<u32>,
) -> Map<String, Value> {
    let mut info = node.info_core();
    info.insert("pairing_status".to_string(), Value::from("joined"));
    // The Node's P2P address, the low 32 bits of its DevEUI (p2p_tower_gateway.md §3.1),
    // as the DevEUI's low half is written: 8 uppercase hex digits, no prefix (planner
    // 2026-09-28, #470 H4).
    info.insert(
        "dev_addr".to_string(),
        Value::from(format!("{:08X}", session.addr)),
    );
    if let Some(net_id) = net_id {
        info.insert("net_id".to_string(), Value::from(net_id));
    }
    info.insert(
        "session_state".to_string(),
        serde_json::to_value(SessionState::Joined).unwrap_or(Value::Null),
    );
    info.insert(
        "proto_version".to_string(),
        Value::from(session.proto_version),
    );
    if let Some(at) = &session.joined_at {
        info.insert("joined_at".to_string(), Value::from(at.as_str()));
    }
    info
}

/// The retained documents one registered P2P node owes a subscriber after a broker connect.
///
/// - `info` always: the joined document when the central holds a session, the registration
///   document (`pairing_status: registered`, what `node-add` publishes) when it does not.
///   `last_seen` is the newest cache slot, as on the LoRaWAN arm.
/// - `device_info` and `config` from the P2P cache, by the rule both arms share
///   ([`uplink::replay_reported`]).
pub fn replay(
    topics: &Topics,
    node: &RegisteredNode,
    session: Option<&Session>,
    net_id: Option<u32>,
    cache: Option<&CacheDocument>,
) -> Vec<Publish> {
    let mut info = match session {
        Some(s) => joined_info(node, s, net_id),
        None => {
            let mut info = node.info_core();
            info.insert("pairing_status".to_string(), Value::from("registered"));
            info
        }
    };
    if let Some(last_seen) = cache.and_then(uplink::newest_slot_at) {
        info.insert("last_seen".to_string(), Value::from(last_seen));
    }
    let mut out = vec![Publish {
        topic: topics.node(&node.dev_eui, Area::Info),
        payload: serde_json::to_vec(&Value::Object(info)).unwrap_or_default(),
        retained: Area::Info.retained(),
    }];
    out.extend(uplink::replay_reported(topics, &node.dev_eui, cache));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{Cache, CacheSlot};
    use crate::error::Transport;
    use crate::p2p::session::DownlinkPhase;
    use crate::store::Store;

    fn node() -> RegisteredNode {
        RegisteredNode {
            serial: "80e0684d".into(),
            dev_eui: "5876070000000413".into(),
            profile: "default".into(),
            app_key: "00".repeat(16),
            name: None,
        }
    }

    fn session() -> Session {
        Session {
            dev_eui: "5876070000000413".into(),
            addr: 0x0000_0413,
            session_key: "00".repeat(16),
            dev_nonce: 3,
            last_seen: 17,
            joined_at: Some("2026-09-26T12:32:29Z".into()),
            proto_version: 1,
            capabilities: None,
            hello: None,
            downlink_queue: Default::default(),
            downlink_phase: DownlinkPhase::Idle,
            history: None,
            detach: None,
            home: None,
        }
    }

    fn topics() -> Topics {
        Topics::new("fiber-4e970f4a")
    }

    fn json(p: &Publish) -> Value {
        serde_json::from_slice(&p.payload).unwrap()
    }

    /// PN-1: the replayed `info` is the P2P one — `transport: p2p`, the session's fields and
    /// the stored join time — never a key.
    #[test]
    fn a_joined_node_replays_its_p2p_info_document() {
        let out = replay(
            &topics(),
            &node(),
            Some(&session()),
            Some(0x6d24_8a62),
            None,
        );
        assert_eq!(out.len(), 1, "no cache, so info only");
        assert_eq!(
            out[0].topic,
            "proximos/fiber-4e970f4a/nodes/5876070000000413/info"
        );
        assert!(out[0].retained);
        let v = json(&out[0]);
        assert_eq!(v["transport"], "p2p");
        assert_eq!(v["serial_number"], "80e0684d");
        assert_eq!(v["pairing_status"], "joined");
        assert_eq!(v["session_state"], "joined");
        assert_eq!(v["dev_addr"], "00000413", "the low 32 bits of the DevEUI");
        assert_eq!(v["net_id"], 0x6d24_8a62u32);
        assert_eq!(v["proto_version"], 1);
        assert_eq!(v["joined_at"], "2026-09-26T12:32:29Z");
        assert!(v.get("app_key").is_none() && v.get("session_key").is_none());
    }

    /// PN-6: the operator's name rides the replayed `info`, as on LoRaWAN.
    #[test]
    fn the_replayed_info_carries_the_name() {
        let named = RegisteredNode {
            name: Some("Cold room 2".into()),
            ..node()
        };
        let v = json(&replay(&topics(), &named, Some(&session()), None, None)[0]);
        assert_eq!(v["name"], "Cold room 2");
        assert!(json(&replay(&topics(), &node(), None, None, None)[0])
            .get("name")
            .is_none());
    }

    #[test]
    fn a_node_without_a_session_replays_as_registered() {
        let v = json(&replay(&topics(), &node(), None, Some(1), None)[0]);
        assert_eq!(v["transport"], "p2p");
        assert_eq!(v["pairing_status"], "registered");
        assert!(v.get("session_state").is_none() && v.get("dev_addr").is_none());
    }

    /// PN-1: `device_info` and `config` come from the **P2P** cache, with the P2P-era stamps.
    #[test]
    fn reported_state_comes_from_the_p2p_cache() {
        struct TempDir(std::path::PathBuf);
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = TempDir(
            std::env::temp_dir().join(format!("control-radio-p2p-replay-{}", std::process::id())),
        );
        let _ = std::fs::remove_dir_all(&dir.0);
        let store = Store::new(&dir.0);
        let cache = Cache::new(&store, Transport::P2p);
        let id = "5876070000000413";
        cache
            .merge_write(
                id,
                CacheSlot::DeviceInfo,
                "2026-09-26T20:22:28Z",
                serde_json::json!({"fw_version": "1.5.0"}),
            )
            .unwrap();
        cache
            .merge_write(
                id,
                CacheSlot::Config,
                "2026-09-26T20:23:29Z",
                serde_json::json!({
                    "dev_eui": id, "received_at": "2026-09-26T20:23:29Z", "seq": 5,
                    "page_count": 0, "config": {"application.interval_report": 60},
                    "body_hex": ""
                }),
            )
            .unwrap();
        let doc = cache.read(id).unwrap();

        let out = replay(&topics(), &node(), Some(&session()), None, doc.as_ref());
        let topics: Vec<&str> = out.iter().map(|p| p.topic.as_str()).collect();
        assert_eq!(
            topics,
            [
                "proximos/fiber-4e970f4a/nodes/5876070000000413/info",
                "proximos/fiber-4e970f4a/nodes/5876070000000413/device_info",
                "proximos/fiber-4e970f4a/nodes/5876070000000413/config",
            ]
        );
        assert_eq!(json(&out[0])["last_seen"], "2026-09-26T20:23:29Z");
        assert_eq!(json(&out[1])["received_at"], "2026-09-26T20:22:28Z");
        let config = json(&out[2]);
        assert_eq!(config["received_at"], "2026-09-26T20:23:29Z");
        assert_eq!(config["config"]["application.interval_report"], 60);
    }
}
