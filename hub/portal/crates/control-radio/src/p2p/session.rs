//! P2P session state — `plan/control/radio/p2p.md` §7-§10, on the gateway link of
//! `plan/control/radio/p2p_tower_gateway.md` §3.
//!
//! The Northbridge opens and ACKs the Nodes' frames itself, so the anti-replay window, the
//! dedup and the ACK routing of the earlier P2P wire are the gateway's now. What the central
//! keeps per Node is what the gateway needs back after a reboot (the key and the last counter
//! it accepted) and what only the central can do: the join channel's anti-replay mark, the
//! downlink queue with its correlation state, and a history replay in progress.

use std::collections::VecDeque;

use crate::error::{Component, Error, Kind, Result, Transport};

/// The Hub's network identifier: 4 bytes, unique per Hub, generated on the first
/// `create --radio p2p` and persisted. **Deliberately not a config value.**
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct NetId(pub u32);

impl NetId {
    /// Build from four random bytes.
    ///
    /// **The plan specifies no generation algorithm** — only "generated, unique per Hub, 4 B BE,
    /// not a config value". Chosen here: 32 bits of OS randomness, with the all-zero and
    /// all-ones values rejected because they are the two most likely values to appear from an
    /// uninitialized buffer or a stuck line, and a Hub that silently adopts one would be
    /// invisible to `net_id` filtering. Recorded in the plan-feedback set.
    pub fn from_bytes(b: [u8; 4]) -> Option<Self> {
        let v = u32::from_be_bytes(b);
        if v == 0 || v == u32::MAX {
            return None;
        }
        Some(NetId(v))
    }

    /// Draw a fresh `net_id` from the OS.
    pub fn generate() -> Result<Self> {
        // Retry rather than accept a rejected value; the rejected set has two members, so this
        // terminates immediately in practice.
        for _ in 0..8 {
            let mut b = [0u8; 4];
            getrandom::fill(&mut b).map_err(|e| {
                Error::new(
                    Kind::StorageError,
                    Transport::P2p,
                    Component::NodeRegistry,
                    format!("cannot draw randomness for net_id: {e}"),
                )
            })?;
            if let Some(id) = NetId::from_bytes(b) {
                return Ok(id);
            }
        }
        Err(Error::new(
            Kind::StorageError,
            Transport::P2p,
            Component::NodeRegistry,
            "the system randomness source returned only reserved net_id values",
        ))
    }

    pub fn to_hex(self) -> String {
        format!("{:08x}", self.0)
    }
}

/// A registered node — the node-registry record, and the P2P join gate itself.
///
/// Written by `node-add --radio p2p` on all three surfaces (CLI, signed MQTT, BLE FB13 — one
/// registration, `p2p.md` §7). **There is no pairing window** (§8, decided 2026-08-27): the
/// central answers a JoinRequest iff the sending serial is in this registry, so registration is
/// the whole admission decision. A registered device's *first* join and every re-join are
/// accepted whenever they arrive — the northbridge modem is in continuous RX, so that may be
/// seconds or hours after registration — and an unregistered device cannot be authenticated at
/// all, since the tag verifies only against an `app_key` the registry already holds.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RegisteredNode {
    pub serial: String,
    pub dev_eui: String,
    pub profile: String,
    /// Hex. The device's own LoRaWAN root key, which P2P now shares.
    pub app_key: String,
    /// The operator's name for the device, `node-add --name` — what the LoRaWAN record keeps
    /// too. Absent from a registry written before 2026-09-27, hence the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl RegisteredNode {
    /// The identity core of the retained `info` document (`northbound` `Area::Info`, §9.4):
    /// the fields a subscriber joins the node by, all known at registration and none of them
    /// changing afterwards. `node-add` publishes exactly this core (adding a `pairing_status`
    /// of `"registered"`); the runtime's join handler builds the richer joined document on the
    /// same core (adding the session fields). Sharing one builder is what keeps the two
    /// publish sites from drifting on the identity — the whole point of a single retained
    /// inventory shape. The `app_key` is deliberately absent: key material never leaves the
    /// registry (§9.7).
    pub fn info_core(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut doc = serde_json::Map::new();
        doc.insert(
            "transport".to_string(),
            serde_json::Value::String(crate::Transport::P2p.as_str().to_string()),
        );
        doc.insert(
            "dev_eui".to_string(),
            serde_json::Value::String(self.dev_eui.clone()),
        );
        doc.insert(
            "serial_number".to_string(),
            serde_json::Value::String(self.serial.clone()),
        );
        doc.insert(
            "profile".to_string(),
            serde_json::Value::String(self.profile.clone()),
        );
        // The LoRaWAN `info` carries the operator's name; without it the portal listed a P2P
        // Node under its DevEUI (night test 2026-09-26, PN-6).
        if let Some(name) = &self.name {
            doc.insert("name".to_string(), serde_json::Value::String(name.clone()));
        }
        doc
    }
}

/// Canonicalise a P2P serial to the on-air form — 8 lowercase hex digits of the 4-byte
/// `serial_number` a JoinRequest carries (`p2p.md` §8).
///
/// The registry key must equal what the runtime derives from an on-air JoinRequest, so both
/// `node-add --radio p2p --serial …` and the join handler funnel through this. Accepts the three
/// ways an operator writes the same number — `0x80e007b2`, bare `80e007b2`, or decimal
/// `2162165682` (as `ats device info` prints it) — and returns the one canonical string. A bare
/// 8-character all-hex-digit token is read as hex (it is the on-air spelling); prefer `0x…` or
/// decimal to avoid the one ambiguous overlap with 8-digit decimals.
pub fn canonical_serial(input: &str) -> Option<String> {
    let t = input.trim();
    let n = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u32::from_str_radix(h, 16).ok()?
    } else if t.len() == 8 && t.chars().all(|c| c.is_ascii_hexdigit()) {
        u32::from_str_radix(t, 16).ok()?
    } else {
        t.parse::<u32>().ok()?
    };
    Some(format!("{n:08x}"))
}

/// The downlink dispatch phase for a session's queue head.
///
/// Only the *front* queue entry is ever in flight, so a single per-session phase tracks the whole
/// downlink handshake rather than tagging each item. The gateway holds the frame and sends it
/// after an ACK that told the Node a downlink is pending; the central only pushes the head and
/// reads the outcome (`p2p_tower_gateway.md` §3.4). See [`crate::p2p::runtime`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownlinkPhase {
    /// Not handed to the gateway: the central pushes the head (if any) on its next pass.
    #[default]
    Idle,
    /// Pushed into the gateway's queue with `TWR_QUEUE_PUSH`; no outcome yet.
    Announced,
    /// Delivered (the gateway reported `DELIVERED` or `SENT`); awaiting the Node's port-85
    /// answer.
    Sent,
}

/// One downlink waiting for a node: the fPort-85 command body, which the central wraps in the
/// `0x81 86` envelope when it pushes it, plus what the central needs to know whether it arrived.
///
/// `seq` is the fPort-85 `Command.seq` inside `body`, kept out here so the correlation check does
/// not have to re-decode what it sent. It is `None` for an operator's `--hex` payload: those bytes
/// are whatever was typed, so there is no allocated sequence to match a `Response` against and the
/// first port-85 answer after delivery clears the head.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueuedDownlink {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u32>,
    pub body: Vec<u8>,
    /// New uplinks seen since this entry was delivered, counted only while the head is `Sent`.
    /// At [`REANNOUNCE_AFTER_UPLINKS`] the head goes back to `Idle` and is pushed again.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub uplinks_since_sent: u8,
    /// Pushes of this entry the gateway put on air and the Node did not ACK (`NOT_DELIVERED`,
    /// `RADIO_ERR`). The gateway makes one attempt per push, so the retries are the central's;
    /// at [`UNDELIVERED_LIMIT`] the entry is given up. Reset once it is delivered.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub undelivered: u8,
    /// `body` is a `0x91` control TLV list (without the envelope byte), not an fPort-85
    /// command: pushed as `0x91 ‖ body`, retired once delivered (p2p_control_commands.md §2).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub control: bool,
    /// The pages of a paged answer to this entry seen so far (#128). A paged answer keeps the
    /// head in flight until every page has arrived: the Node streams one answer at a time, and a
    /// command delivered mid-stream cancels or is refused (`NOT_READY`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<PagesSeen>,
}

/// Which pages of a paged answer have arrived, out of how many.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PagesSeen {
    pub count: u32,
    pub seen: std::collections::BTreeSet<u32>,
}

impl PagesSeen {
    /// Record page `index` of `count`; whether it is new. A different `count` starts over: it is
    /// a fresh answer to the same command.
    pub fn accept(&mut self, index: u32, count: u32) -> bool {
        if count != self.count {
            self.count = count;
            self.seen.clear();
        }
        self.seen.insert(index)
    }

    pub fn complete(&self) -> bool {
        self.seen.len() as u32 >= self.count
    }

    pub fn missing(&self) -> Vec<u32> {
        (0..self.count).filter(|i| !self.seen.contains(i)).collect()
    }
}

fn is_zero(n: &u8) -> bool {
    *n == 0
}

impl QueuedDownlink {
    /// An operator-supplied `--hex` payload: no allocated sequence to correlate against.
    pub fn raw(body: Vec<u8>) -> Self {
        Self {
            seq: None,
            body,
            uplinks_since_sent: 0,
            undelivered: 0,
            control: false,
            pages: None,
        }
    }

    /// A central control command (p2p_control_commands.md §2).
    pub fn control(cmd: crate::p2p::tower::control::ControlDownlink) -> Self {
        Self {
            seq: None,
            body: cmd.tlv(),
            uplinks_since_sent: 0,
            undelivered: 0,
            control: true,
            pages: None,
        }
    }

    /// A structured `node-send`, carrying the sequence the encoder wrote into `body`.
    pub fn with_seq(seq: u32, body: Vec<u8>) -> Self {
        Self {
            seq: Some(seq),
            body,
            uplinks_since_sent: 0,
            undelivered: 0,
            control: false,
            pages: None,
        }
    }
}

/// Confirmed pushes of one queue head that may go unACKed before the central gives it up
/// (#470 C3). The gateway attempts each push once, in the receive window after a Node uplink,
/// so this is five chances over five uplinks. An `EXPIRED` push (never on air: the Node sent
/// nothing within its TTL) and a `NO_COUNTER` one (never sealed) spend no chance.
pub const UNDELIVERED_LIMIT: u8 = 5;

/// The `status` (and `kind`) of the `command` document for a head given up after
/// [`UNDELIVERED_LIMIT`] unACKed pushes.
pub const UNDELIVERED: &str = "undelivered";

/// New uplinks the central tolerates in `Sent` before it pushes the head again (README §W2).
/// Three is enough to ride out a lost downlink or a lost answer without waiting out a sleepy
/// node, and re-delivery is safe because the node only executes a deferred action once its own
/// answer has been acknowledged (STICKER S3).
pub const REANNOUNCE_AFTER_UPLINKS: u8 = 3;

/// The one session state the central publishes (`session_state` in the retained `info`,
/// `node-info`, `node-list`). The earlier P2P wire also had `detach_pending`, `rejoin_pending`
/// and `stale`; the Detach and the rejoin request come back with #470 P3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Joined,
}

/// A Node's P2P session, set up by a JoinReq/JoinAccept exchange (`p2p_tower_gateway.md` §3.2)
/// and kept, parked, while the Node is on another carrier (node_registry.md §4).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Session {
    /// The node's identity, canonical 16 lowercase hex (#73) — the key of both
    /// [`crate::p2p::db::SessionsDb`] maps, and what the session key was derived from.
    pub dev_eui: String,
    /// The Node's address on the air: the low 32 bits of `dev_eui` (#470 D6). Stored rather
    /// than derived so the field that tells a session of this link from an older one is
    /// explicit ([`crate::p2p::db::SessionsDb::load_file`]).
    pub addr: u32,
    /// Hex. Handed to the gateway with `TWR_NODE_ADD`; never published.
    pub session_key: String,
    /// The last JoinReq counter accepted: the next one must be greater.
    pub dev_nonce: u32,
    /// The last uplink counter the gateway accepted under this session, 0 before the first.
    /// Given back to the gateway at every resync, so a rebooted gateway cannot take a replay.
    pub last_seen: u32,
    /// When the join was committed, RFC 3339 — the `joined_at` of the retained `info`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub joined_at: Option<String>,
    /// The JoinReq's `proto_version`: the Node's `APP_PROTO_VERSION` (the app/protobuf payload
    /// version, the byte NFC and LoRaWAN `Info` carry), reported in the retained `info` as
    /// before. It is not the TOWER wire version and selects nothing on the Hub.
    #[serde(default)]
    pub proto_version: u8,
    /// The Node's `0x91` `Capabilities`, as last received.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<crate::p2p::tower::control::Capabilities>,
    /// The Node's last `0x91` `Hello`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hello: Option<crate::p2p::tower::control::Hello>,
    /// Pending downlinks for this node, FIFO. `node-send` appends; the runtime drains from the
    /// front. See [`QueuedDownlink`].
    #[serde(default)]
    pub downlink_queue: VecDeque<QueuedDownlink>,
    /// Dispatch phase of the queue head — see [`DownlinkPhase`].
    #[serde(default)]
    pub downlink_phase: DownlinkPhase,
    /// The history replay currently answering the queue head, if any — see [`HistoryCollect`].
    /// Lives on the session so it survives a runtime restart (a replay outlives one drain pass)
    /// and is dropped with the node by `remove_node`.
    #[serde(default)]
    pub history: Option<HistoryCollect>,
    /// Set when `node-remove` took the Node's registration while this session still had to
    /// deliver a `Detach` (p2p_control_commands.md §3). A detaching session stays on the
    /// gateway until the Detach is delivered, given up or past [`DETACH_MAX_S`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detach: Option<Detaching>,
    /// The gateway that ACKs this Node and sends its downlinks — its **home**
    /// (`p2p_cluster_gateway.md` §5): a Client Hub's hostname, or `None` for this Hub's own
    /// Northbridge. Every other gateway holds the peer without `HOME`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
}

/// When a detaching session was removed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Detaching {
    /// RFC 3339.
    pub since: String,
    pub since_unix: u64,
}

/// How long a detaching session waits for its Detach to reach the Node: a Node silent this
/// long is gone, and its session goes without one.
pub const DETACH_MAX_S: u64 = 6 * 3600;

/// State of one in-flight history replay.
///
/// A `req_history` is answered by N `HistoryFrame` uplinks that all echo the request's `seq`, so
/// the queue head cannot retire on the first answer the way every other command does. This
/// tracks how far the stream has got.
///
/// `seen` holds only the frame indices, not the pages: each frame is published northbound as it
/// arrives (§5.3 — the portal credits a backfill job per frame, so coalescing would lose the
/// accounting), which means nothing downstream needs the records kept here, and a long replay
/// would otherwise put hundreds of decoded records into `sessions.db`.
/// The largest `frame_count` a frame may claim.
///
/// `frame_count` arrives as a protobuf `uint32` varint, but the firmware counts frames into a
/// `uint16_t` (`app_history_count_frames`, `app_history.h`), so a larger value is corruption or
/// hostile. It matters because [`HistoryCollect::missing_indices`] walks `0..frame_count` while
/// the session lock is held.
pub const MAX_FRAME_COUNT: u32 = u16::MAX as u32;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HistoryCollect {
    /// The `ReqHistory` seq this stream answers.
    pub seq: u32,
    /// Learned from the first frame that carries it. Grows only: a node that discovers more
    /// records mid-stream raises it, and a lower value in a later frame is ignored rather than
    /// truncating the stream.
    pub frame_count: Option<u32>,
    /// Frame indices already seen. First wins, so a retransmitted frame is not double-counted.
    pub seen: std::collections::BTreeSet<u32>,
    /// When the last frame arrived, for the inter-frame timeout.
    pub last_frame_at: String,
    /// The same instant in Unix seconds — what the timeout actually compares. Kept beside the
    /// RFC3339 string rather than parsed back out of it: that string is the published form and
    /// this is the arithmetic one. `default` so a bench database written before the timeout
    /// existed still loads (its streams then read as stalled and are retired once, which is the
    /// right answer for a replay nobody is still feeding).
    #[serde(default)]
    pub last_frame_at_unix: u64,
}

impl HistoryCollect {
    pub fn new(seq: u32, at: &str, at_unix: u64) -> Self {
        HistoryCollect {
            seq,
            frame_count: None,
            seen: std::collections::BTreeSet::new(),
            last_frame_at: at.to_string(),
            last_frame_at_unix: at_unix,
        }
    }

    /// Record a frame. Returns false if this index was already seen (first wins).
    pub fn accept(&mut self, frame_index: u32, frame_count: u32, at: &str, at_unix: u64) -> bool {
        self.last_frame_at = at.to_string();
        self.last_frame_at_unix = at_unix;
        // Bounded by the firmware's own counter type — see [`MAX_FRAME_COUNT`].
        let frame_count = frame_count.min(MAX_FRAME_COUNT);
        if frame_count > 0 {
            self.frame_count = Some(match self.frame_count {
                Some(c) => c.max(frame_count),
                None => frame_count,
            });
        }
        self.seen.insert(frame_index)
    }

    /// Whether every frame the stream promised has arrived.
    ///
    /// Counts what was *seen*, not the highest index: frames can arrive out of order, and
    /// `frame_index + 1 >= frame_count` on an early-arriving last frame would end the stream
    /// while the middle was still in flight.
    pub fn complete(&self) -> bool {
        match self.frame_count {
            Some(c) => self.seen.len() as u32 >= c,
            None => false,
        }
    }

    /// The gaps, for an accept-incomplete retirement.
    pub fn missing_indices(&self) -> Vec<u32> {
        match self.frame_count {
            Some(c) => (0..c).filter(|i| !self.seen.contains(i)).collect(),
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_serial_canonicalises_across_the_three_spellings() {
        // 0x80e007b2 == 2162165682 (the campaign Sticker, `ats device info` decimal form).
        assert_eq!(canonical_serial("0x80e007b2").as_deref(), Some("80e007b2"));
        assert_eq!(canonical_serial("80e007b2").as_deref(), Some("80e007b2"));
        assert_eq!(canonical_serial("2162165682").as_deref(), Some("80e007b2"));
        assert_eq!(
            canonical_serial("  0X80E007B2 ").as_deref(),
            Some("80e007b2")
        );
        assert_eq!(canonical_serial("").as_deref(), None);
        assert_eq!(canonical_serial("nope").as_deref(), None);
        // The on-air value the runtime derives from a JoinRequest matches the stored key.
        assert_eq!(format!("{:08x}", 0x80e007b2u32), "80e007b2");
    }

    #[test]
    fn reserved_net_ids_are_refused() {
        assert!(
            NetId::from_bytes([0, 0, 0, 0]).is_none(),
            "all-zero is reserved"
        );
        assert!(
            NetId::from_bytes([0xff; 4]).is_none(),
            "all-ones is reserved"
        );
        let id = NetId::from_bytes([0xa1, 0xb2, 0xc3, 0xd4]).expect("valid");
        assert_eq!(id.to_hex(), "a1b2c3d4");
    }

    #[test]
    fn a_generated_net_id_is_never_reserved() {
        for _ in 0..32 {
            let id = NetId::generate().expect("generates");
            assert_ne!(id.0, 0);
            assert_ne!(id.0, u32::MAX);
        }
    }

    /// The registry record is the §7 tuple, spelled as all three registration surfaces write it
    /// — and it is now the whole join gate, so its field set is a contract, not an internal
    /// detail. Round-trips, because the registry is persisted and read back.
    #[test]
    fn the_registry_record_is_the_documented_tuple() {
        let n = RegisteredNode {
            serial: "SN125".into(),
            dev_eui: "70b3d57ed0000abe".into(),
            profile: "default".into(),
            app_key: "00112233445566778899aabbccddeeff".into(),
            name: None,
        };
        let json = serde_json::to_value(&n).expect("serializes");
        let mut keys: Vec<&String> = json.as_object().expect("an object").keys().collect();
        keys.sort_unstable();
        assert_eq!(keys, ["app_key", "dev_eui", "profile", "serial"]);
        // No window state rides along: registration is not time-boxed (§8).
        assert!(json.get("opened_at_ms").is_none());
        assert!(json.get("window_s").is_none());

        let back: RegisteredNode = serde_json::from_value(json).expect("round-trips");
        assert_eq!(back, n);
    }

    #[test]
    fn the_session_state_is_spelled_as_the_portal_reads_it() {
        assert_eq!(
            serde_json::to_value(SessionState::Joined).expect("ser"),
            "joined"
        );
    }

    /// A queued entry round-trips and leaves its counter out while it is zero.
    #[test]
    fn a_queued_downlink_round_trips() {
        let mut q = QueuedDownlink::with_seq(7, vec![0x08, 0x07]);
        let bare = serde_json::to_value(&q).expect("ser");
        assert!(bare.get("uplinks_since_sent").is_none());
        q.uplinks_since_sent = 2;
        let back: QueuedDownlink =
            serde_json::from_value(serde_json::to_value(&q).expect("ser")).expect("de");
        assert_eq!(back, q);
    }

    /// `frame_count` is a protobuf `uint32` off the air; the firmware counts frames in a
    /// `uint16_t`. `missing_indices` walks `0..frame_count` under the session lock, so an
    /// unbounded value is a lock-held allocation of up to 16 GiB.
    #[test]
    fn a_frame_count_past_the_firmware_type_is_clamped() {
        let mut c = HistoryCollect::new(7, "2026-09-21T00:00:00Z", 1_758_412_800);
        c.accept(0, u32::MAX, "2026-09-21T00:00:00Z", 1_758_412_800);
        assert_eq!(c.frame_count, Some(u32::from(u16::MAX)));
        assert_eq!(c.missing_indices().len(), 65_534);
    }
}
