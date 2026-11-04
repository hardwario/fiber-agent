//! Northbound MQTT — `plan/control/radio/plan.md` §9.4.
//!
//! **One family, no transport level:**
//! `proximos/{hostname}/nodes/{dev_eui}/{area}` — both carriers, one tree (decision D3,
//! GitLab #73).
//!
//! The `{transport}` level was redundant once `dev_eui` became the identity on both arms
//! (#73): a node is hardware-bound to one transport, so the DevEUI already determines it, and
//! `reading`/`alarm`/`command` bodies are byte-identical across carriers anyway — nothing in
//! routing or decoding ever needed the level. It moves into the retained `info` document as a
//! `transport` field, which is where a consumer that does care about it now looks.
//!
//! **Per-node retained topics with a tombstone on removal — never a full-array snapshot.**
//! v1 published every node in one array, which exceeded rumqttc's 10 240 B
//! `max_outgoing_packet_size` with a single well-buffered Node and tore down the entire MQTT
//! client (11 reconnects in under 6 minutes, observed). The `@dev` branch raised the cap and
//! added a degradation ladder, but the full-array *shape* survives there, so the per-node rule
//! stands on its own rather than on that defect.
//!
//! No v1 topic name exists in v2: v1's `lorawan/sensors/{dev_eui}/info` becomes
//! `proximos/{hostname}/nodes/{dev_eui}/info`.

use crate::error::Result;

/// The broker account this component connects with.
///
/// Its ACL is new — neither `system/design.md` §7 nor §8 lists a `control-radio` client today,
/// which the plan flags as an amendment. Under decision #12 the account folds into the one
/// `proximos-control` client, but the name is kept here so the ACL rows stay reviewable.
pub const BROKER_ACCOUNT: &str = "control-radio";

/// Local mosquitto. Not configurable, like the ChirpStack endpoint.
pub const BROKER_ADDR: &str = "127.0.0.1:1883";

/// The health topic. Owned by this component and published under every mode.
pub const GATEWAYS_TOPIC: &str = "radio/gateways";

/// Which per-node topic is being addressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    /// Identity and last-known state. **Retained**, with a tombstone on removal.
    Info,
    /// The Node's last fPort-85 `Info` — what it reported about itself. **Retained**
    /// (`node_reported_state.md` §2.1).
    DeviceInfo,
    /// One decoded uplink. QoS 1, not retained.
    Reading,
    /// A decoded fPort-3 batch. QoS 1, not retained.
    Alarm,
    /// The Node's last complete fPort-85 `ConfigDump`, rendered. **Retained**
    /// (`node_reported_state.md` §2.2).
    Config,
    /// fPort-85 command results, one document per frame or page. Not retained.
    Command,
    /// One decoded `HistoryFrame` of a `ReqHistory` replay, in the portal's
    /// `NodeHistoryMsg` shape (`p2p_history_replay.md` §5.3). QoS 1, not retained: a replay
    /// is rows for a database, not state.
    History,
    /// The Node's last `DevStatusAns`: battery, downlink RSSI/SNR, TX power, uptime.
    /// **Retained**, like `device_info` (p2p_control_commands.md §5).
    DeviceStatus,
}

impl Area {
    /// The inverse of [`Area::as_str`] — one topic segment back into an area.
    ///
    /// Not `FromStr`: an area is a topic level, not a value a user types, and the trait would
    /// invite `"reading".parse()` in places where the topic shape is not what is being read.
    pub fn from_name(s: &str) -> Option<Self> {
        Area::ALL.into_iter().find(|a| a.as_str() == s)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Area::Info => "info",
            Area::DeviceInfo => "device_info",
            Area::Reading => "reading",
            Area::Alarm => "alarm",
            Area::Config => "config",
            Area::Command => "command",
            Area::History => "history",
            Area::DeviceStatus => "device_status",
        }
    }

    /// The **state** areas are retained, the **event** areas are not.
    ///
    /// `info` is the inventory a late subscriber needs; `device_info` and `config` are the
    /// latest thing the Node reported about itself, correct until it reports something else —
    /// a boot snapshot that may be months old and would otherwise reach only a subscriber
    /// connected in the second it arrived (`node_reported_state.md` §2). Each carries its own
    /// `received_at`, so a consumer presents it as cached state. A retained `reading` would hand
    /// every new subscriber one stale measurement as if it were live, which is why the event
    /// areas stay unretained.
    pub const fn retained(self) -> bool {
        matches!(
            self,
            Area::Info | Area::DeviceInfo | Area::Config | Area::DeviceStatus
        )
    }

    pub const ALL: [Area; 8] = [
        Area::Info,
        Area::DeviceInfo,
        Area::Reading,
        Area::Alarm,
        Area::Config,
        Area::Command,
        Area::History,
        Area::DeviceStatus,
    ];
}

/// Builds the topic family for one Hub.
#[derive(Debug, Clone)]
pub struct Topics {
    hostname: String,
}

impl Topics {
    pub fn new(hostname: impl Into<String>) -> Self {
        Topics {
            hostname: hostname.into(),
        }
    }

    /// `proximos/{hostname}/nodes/{dev_eui}/{area}` — the same topic on both carriers (D3).
    pub fn node(&self, dev_eui: &str, area: Area) -> String {
        format!(
            "proximos/{}/nodes/{}/{}",
            self.hostname,
            dev_eui,
            area.as_str()
        )
    }

    /// Everything a `node-remove` must clear, so removal cannot leave a node visible in an
    /// inventory it has left.
    pub fn tombstones(&self, dev_eui: &str) -> Vec<String> {
        Area::ALL
            .iter()
            .filter(|a| a.retained())
            .map(|a| self.node(dev_eui, *a))
            .collect()
    }
}

/// The first level of every topic this component produces.
pub const ROOT: &str = "proximos";

/// One parsed `proximos/{hostname}/nodes/{dev_eui}/{area}` topic.
///
/// Borrowed from the topic string: a consumer routing thousands of publishes should not have
/// to allocate three strings to find out it is looking at a `reading`.
///
/// **No `transport` field.** The topic no longer carries one (D3); a consumer that needs it
/// reads `transport` from the retained `info` document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeTopic<'a> {
    pub hostname: &'a str,
    /// The `dev_eui`, on both carriers (#73, §9.5). Case as published; consumers lowercase.
    pub id: &'a str,
    pub area: Area,
}

/// The subscription filter for one area, across every Hub and every node.
///
/// `proximos/+/nodes/+/{area}`. Exported so a consumer (`portal`) subscribes to the shape
/// [`Topics::node`] publishes rather than to a hand-written string that can drift from it by
/// one level and silently receive nothing. One filter now covers both carriers (D3).
pub fn node_filter(area: Area) -> String {
    format!("{ROOT}/+/nodes/+/{}", area.as_str())
}

/// Read a topic back into its parts, or `None` when it is not one of ours.
///
/// The inverse of [`Topics::node`], and tested against it: a consumer that parsed the topic
/// with its own `split('/')` would be a second copy of the shape.
pub fn parse_node_topic(topic: &str) -> Option<NodeTopic<'_>> {
    let mut parts = topic.split('/');
    if parts.next()? != ROOT {
        return None;
    }
    let hostname = parts.next().filter(|h| !h.is_empty())?;
    if parts.next()? != "nodes" {
        return None;
    }
    let id = parts.next().filter(|i| !i.is_empty())?;
    let area = Area::from_name(parts.next()?)?;
    // Exactly five levels: a longer topic is a different thing that happens to start the same
    // way, and treating it as a node document would misfile it. That also rejects the old
    // transport-bearing shape, which had six.
    if parts.next().is_some() {
        return None;
    }
    Some(NodeTopic { hostname, id, area })
}

/// The ChirpStack integration event carrying an uplink — the LoRaWAN entry point of the decode
/// path.
pub fn chirpstack_uplink_topic(app_id: &str, dev_eui: &str) -> String {
    format!("application/{app_id}/device/{dev_eui}/event/up")
}

pub fn chirpstack_txack_topic(app_id: &str, dev_eui: &str) -> String {
    format!("application/{app_id}/device/{dev_eui}/event/txack")
}

/// Subscribe to every device's events for one application.
pub fn chirpstack_event_filter(app_id: &str) -> String {
    format!("application/{app_id}/device/+/event/+")
}

/// A retained publish with an empty payload — the tombstone that removes a node from every
/// subscriber's inventory.
pub const TOMBSTONE: &[u8] = b"";

/// Where `{hostname}` in the topic family comes from.
///
/// `PROXIMOS_HOSTNAME` first, for the dev loop and the integration stack — the same override
/// convention `PROXIMOS_CONFIG`/`PROXIMOS_RADIO_DATA` use. Then `/etc/hostname`, which is what
/// the image sets from the machine id and what every other ProXimos topic is named after. The
/// final fallback is a literal rather than a failure: a Hub whose hostname cannot be read must
/// still publish its inventory somewhere reachable, and a wrong-but-present topic is
/// diagnosable while a dropped publish is not.
pub fn hostname() -> String {
    // Lowercased on every path (decision #21): one spelling of a Hub on the broker, whatever
    // `/etc/hostname` or an override says.
    if let Ok(h) = std::env::var("PROXIMOS_HOSTNAME") {
        let h = h.trim().to_lowercase();
        if !h.is_empty() {
            return h;
        }
    }
    match std::fs::read_to_string("/etc/hostname") {
        Ok(raw) if !raw.trim().is_empty() => raw.trim().to_lowercase(),
        _ => "proximos".to_string(),
    }
}

/// Publishes retained documents on the northbound family.
///
/// A trait for the same reason [`crate::lorawan::gateway::RetainedTopics`] is one: the
/// retained *read* side is already injected into dispatch, and the write side has to be
/// testable the same way — a unit test that had to stand up a broker to prove `node-remove`
/// tombstones its topic would either be skipped in CI or be a broker test wearing a dispatch
/// test's name.
///
/// One call carries every message because the LoRaWAN tombstone set is
/// [`Topics::tombstones`] — `info`, `device_info` and `config` — and they must go out over one
/// connection, not one connection each.
#[async_trait::async_trait]
pub trait RetainedPublisher: Send + Sync {
    /// Publish every `(topic, payload)` **retained**, at QoS 1, and do not return until the
    /// broker has acknowledged all of them.
    ///
    /// Waiting for the acks is the whole point: a fire-and-forget retained publish followed by
    /// process exit is a publish the broker may never have stored, and `node-remove`'s
    /// tombstone is exactly the message whose loss is invisible until an operator wonders why
    /// a deleted Node is still in the inventory.
    async fn publish_retained(&self, messages: &[(String, Vec<u8>)]) -> Result<()>;

    /// Publish every `(topic, payload)` **not** retained, at QoS 1, and wait for the broker's
    /// acks the same way: an event a command implies rather than state, such as the
    /// `superseded` `command` document of a coalesced `node-send` (plan.md §5, amended
    /// 2026-09-28). Retained, it would reach every later subscriber as though it had just
    /// happened.
    async fn publish_events(&self, messages: &[(String, Vec<u8>)]) -> Result<()>;
}

#[cfg(test)]
mod tests {
    /// Every other publisher in the tree resolves its Hub name through this function, which
    /// honours `PROXIMOS_HOSTNAME` before `/etc/hostname`. A second implementation that read the
    /// file only — as the bench P2P host used to — puts the runtime's `info` and the CLI's
    /// tombstones under two different Hubs on any bench that sets the variable, and the portal
    /// matches hubs by name.
    ///
    /// This documents the contract rather than reproducing a failure: a private function in a
    /// binary cannot be reached from a test, which is exactly why the duplicate survived.
    #[test]
    fn the_env_override_wins_over_etc_hostname() {
        std::env::set_var("PROXIMOS_HOSTNAME", "  bench-x  ");
        assert_eq!(super::hostname(), "bench-x", "trimmed, and it wins");

        // An empty value is not an instruction — fall through to the file or the default.
        std::env::set_var("PROXIMOS_HOSTNAME", "");
        assert!(!super::hostname().is_empty());

        std::env::remove_var("PROXIMOS_HOSTNAME");
    }

    use super::*;

    /// D3: one family, five levels, no transport anywhere in it. Two nodes on different
    /// carriers differ only by their DevEUI — which is the point: the DevEUI already
    /// determines the transport, so the level was redundant.
    #[test]
    fn the_topic_family_has_no_transport_level() {
        let t = Topics::new("proximos-a1b2c3d4");
        assert_eq!(
            t.node("0011223344556677", Area::Reading),
            "proximos/proximos-a1b2c3d4/nodes/0011223344556677/reading"
        );

        for area in Area::ALL {
            let topic = t.node("70b3d57ed0000abe", area);
            let levels: Vec<&str> = topic.split('/').collect();
            assert_eq!(levels.len(), 5, "{topic}");
            assert_eq!(levels[2], "nodes", "{topic}");
            for word in ["lorawan", "p2p"] {
                assert!(
                    !levels.contains(&word),
                    "the transport level is gone: {topic}"
                );
            }
        }
    }

    /// The state areas are retained, the event areas are not. A retained reading would hand
    /// every new subscriber one stale measurement as though it were live.
    #[test]
    fn only_the_state_areas_are_retained() {
        for a in [Area::Info, Area::DeviceInfo, Area::Config] {
            assert!(a.retained(), "{a:?} is state and must be retained");
        }
        for a in [Area::Reading, Area::Alarm, Area::Command, Area::History] {
            assert!(!a.retained(), "{a:?} must not be retained");
        }
    }

    /// Removal clears every retained topic, so a removed node cannot linger in an inventory.
    #[test]
    fn removal_tombstones_every_retained_topic() {
        let t = Topics::new("hub");
        let stones = t.tombstones("0011223344556677");
        assert_eq!(
            stones,
            vec![
                "proximos/hub/nodes/0011223344556677/info",
                "proximos/hub/nodes/0011223344556677/device_info",
                "proximos/hub/nodes/0011223344556677/config",
                "proximos/hub/nodes/0011223344556677/device_status",
            ]
        );
        assert_eq!(TOMBSTONE, b"", "a tombstone is an empty retained payload");

        // Every retained area is covered, so adding one later cannot silently skip cleanup.
        let retained: Vec<_> = Area::ALL.iter().filter(|a| a.retained()).collect();
        assert_eq!(stones.len(), retained.len());
    }

    /// No v1 topic name survives into v2 — the v1 shape was `lorawan/sensors/...`.
    #[test]
    fn no_v1_topic_name_survives() {
        let t = Topics::new("hub");
        for area in Area::ALL {
            let topic = t.node("x", area);
            assert!(!topic.contains("sensors"), "v1 vocabulary leaked: {topic}");
            assert!(!topic.contains("sticker"), "v1 vocabulary leaked: {topic}");
            assert!(!topic.contains("fiber"), "v1 vocabulary leaked: {topic}");
        }
    }

    /// The filter a consumer subscribes to and the topic this component publishes have to be
    /// the same shape, level for level — one wrong level subscribes successfully and receives
    /// nothing, which looks exactly like a Hub with no traffic.
    #[test]
    fn the_subscription_filter_matches_what_is_published() {
        for area in Area::ALL {
            let filter = node_filter(area);
            let published = Topics::new("proximos-a1b2c3d4").node("id-1", area);
            let f: Vec<&str> = filter.split('/').collect();
            let p: Vec<&str> = published.split('/').collect();
            assert_eq!(f.len(), p.len(), "{filter} vs {published}");
            for (fl, pl) in f.iter().zip(p.iter()) {
                assert!(*fl == "+" || fl == pl, "{filter} vs {published}");
            }
        }
        assert_eq!(node_filter(Area::Reading), "proximos/+/nodes/+/reading");
    }

    #[test]
    fn a_published_topic_parses_back_into_its_parts() {
        let topics = Topics::new("hub-1");
        for area in Area::ALL {
            let topic = topics.node("0011223344556677", area);
            let parsed = parse_node_topic(&topic).expect("round trips");
            assert_eq!(parsed.hostname, "hub-1");
            assert_eq!(parsed.id, "0011223344556677");
            assert_eq!(parsed.area, area);
        }
    }

    /// The old six-level shape must not parse. It is not backward compatibility we are
    /// dropping — nothing is deployed — but a consumer still publishing it would otherwise
    /// have its `lorawan` segment read as the hostname and `nodes` as the node id.
    #[test]
    fn parse_node_topic_rejects_the_old_transport_shaped_topic() {
        for topic in [
            "proximos/h/lorawan/nodes/70b3d57ed0000abe/reading",
            "proximos/h/p2p/nodes/70b3d57ed0000abe/info",
        ] {
            assert!(parse_node_topic(topic).is_none(), "{topic}");
        }
    }

    /// Anything that is not one of ours is `None` rather than a guess — a consumer must not
    /// file another component's topic as a node document.
    #[test]
    fn a_foreign_topic_is_not_parsed_as_a_node_document() {
        for topic in [
            "fiber/hub-1/lorawan/sensors",
            "proximos/hub-1/lorawan/gateways",
            "proximos/hub-1/zigbee/nodes/aa/reading",
            // The pre-#73 shape with a transport level. Nothing publishes it any more, and a
            // consumer must not quietly accept both.
            "proximos/hub-1/lorawan/nodes/aa/reading",
            "proximos/hub-1/p2p/nodes/aa/info",
            "proximos/hub-1/p2p/scan",
            "proximos/hub-1/nodes/aa/unknown-area",
            "proximos/hub-1/nodes//reading",
            "proximos//nodes/aa/reading",
            "proximos/hub-1/nodes/aa/reading/extra",
            "proximos/hub-1/nodes/aa",
            "radio/gateways",
        ] {
            assert!(parse_node_topic(topic).is_none(), "{topic}");
        }
    }

    #[test]
    fn the_chirpstack_event_topics_are_the_documented_ones() {
        assert_eq!(
            chirpstack_uplink_topic("app-1", "0011223344556677"),
            "application/app-1/device/0011223344556677/event/up"
        );
        assert_eq!(
            chirpstack_txack_topic("app-1", "0011223344556677"),
            "application/app-1/device/0011223344556677/event/txack"
        );
        assert_eq!(
            chirpstack_event_filter("app-1"),
            "application/app-1/device/+/event/+"
        );
    }

    /// Whatever the host is called, it has to be usable as one topic level: an empty or
    /// slash-bearing hostname would silently re-shape every topic in the family.
    #[test]
    fn the_hostname_is_always_one_usable_topic_level() {
        let h = hostname();
        assert!(!h.is_empty());
        assert!(
            !h.contains('/'),
            "hostname must not add topic levels: {h:?}"
        );
        assert!(!h.contains('+') && !h.contains('#'), "no wildcards: {h:?}");
    }

    #[test]
    fn the_health_topic_is_component_owned_and_transport_free() {
        // `radio/gateways` carries both arms' rows, so it is deliberately not under the
        // per-transport tree.
        assert_eq!(GATEWAYS_TOPIC, "radio/gateways");
        assert!(!GATEWAYS_TOPIC.contains("lorawan"));
        assert!(!GATEWAYS_TOPIC.contains("p2p"));
    }
}
