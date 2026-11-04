//! The P2P gateway plane — `plan/control/radio/p2p_cluster_gateway.md` §2.
//!
//! A Client Hub's Northbridge is a remote gateway of the Server's central. The link frames
//! of `p2p_tower_gateway.md` §2 cross the Server's broker inside the Client's own topic family,
//! without HDLC framing or CRC (TLS over TCP carries them intact):
//!
//! - `proximos/<gw>/radio/p2p-gateway/up` — Client → central: `v ‖ host_unix_ms(8 LE) ‖ type ‖
//!   seq ‖ body`, every `RSP` and event the Northbridge sent;
//! - `proximos/<gw>/radio/p2p-gateway/down` — central → Client: `v ‖ type ‖ seq ‖ body`, one
//!   command;
//! - `proximos/<gw>/radio/p2p-gateway/state` — the Client's retained JSON state, `offline` as
//!   its last will.
//!
//! Pure codec and naming; the forwarder ([`crate::p2p::gateway_fwd`]) and the central's
//! [`crate::p2p::remote`] move the bytes.

use serde::{Deserialize, Serialize};

use crate::p2p::northbridge::UpdateSummary;

/// The plane's wire version: the first byte of every `up` and `down` message.
pub const VERSION: u8 = 1;

/// The topic family's last segment under `radio/`.
const FAMILY: &str = "p2p-gateway";

/// `proximos/<gw>/radio/p2p-gateway/up`.
pub fn up_topic(gw: &str) -> String {
    format!("{}/{gw}/radio/{FAMILY}/up", crate::northbound::ROOT)
}

/// `proximos/<gw>/radio/p2p-gateway/down`.
pub fn down_topic(gw: &str) -> String {
    format!("{}/{gw}/radio/{FAMILY}/down", crate::northbound::ROOT)
}

/// `proximos/<gw>/radio/p2p-gateway/state`.
pub fn state_topic(gw: &str) -> String {
    format!("{}/{gw}/radio/{FAMILY}/state", crate::northbound::ROOT)
}

/// The central's subscriptions: every gateway's `up` and `state`.
pub fn central_filters() -> [String; 2] {
    let root = crate::northbound::ROOT;
    [
        format!("{root}/+/radio/{FAMILY}/up"),
        format!("{root}/+/radio/{FAMILY}/state"),
    ]
}

/// Which plane topic a message arrived on, and for which gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaneTopic {
    Up(String),
    Down(String),
    State(String),
}

/// Parse a plane topic; `None` for anything else, or a gateway segment that is not a plain
/// hostname (a topic segment cannot carry `/`, `+` or `#`, but an empty one can).
pub fn parse_topic(topic: &str) -> Option<PlaneTopic> {
    let mut parts = topic.split('/');
    let (root, gw, radio, family, leaf) = (
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
    );
    if parts.next().is_some()
        || root != crate::northbound::ROOT
        || radio != "radio"
        || family != FAMILY
        || gw.is_empty()
        || !gw
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return None;
    }
    let gw = gw.to_string();
    match leaf {
        "up" => Some(PlaneTopic::Up(gw)),
        "down" => Some(PlaneTopic::Down(gw)),
        "state" => Some(PlaneTopic::State(gw)),
        _ => None,
    }
}

/// A command type the forwarder may write to its Northbridge (`p2p_tower_gateway.md` §2).
pub fn is_command(frame_type: u8) -> bool {
    (0x01..=0x3F).contains(&frame_type)
}

/// A response or event type the forwarder may send up.
pub fn is_answer_or_event(frame_type: u8) -> bool {
    (0x40..=0xBF).contains(&frame_type)
}

/// One link frame on the plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaneFrame {
    pub frame_type: u8,
    pub seq: u8,
    pub body: Vec<u8>,
}

/// An `up` message: the frame and the Client's clock when it left the UART deframer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpFrame {
    pub host_unix_ms: u64,
    pub frame: PlaneFrame,
}

/// Why a plane message was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaneError {
    /// Shorter than its header.
    Short,
    /// Another plane version than [`VERSION`].
    Version(u8),
    /// A frame type the direction does not carry.
    Type(u8),
}

impl std::fmt::Display for PlaneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlaneError::Short => write!(f, "message shorter than its header"),
            PlaneError::Version(v) => write!(f, "plane version {v}, this build speaks {VERSION}"),
            PlaneError::Type(t) => write!(f, "frame type {t:#04x} not carried in this direction"),
        }
    }
}

/// Encode an `up` message.
pub fn encode_up(up: &UpFrame) -> Vec<u8> {
    let mut b = Vec::with_capacity(11 + up.frame.body.len());
    b.push(VERSION);
    b.extend_from_slice(&up.host_unix_ms.to_le_bytes());
    b.push(up.frame.frame_type);
    b.push(up.frame.seq);
    b.extend_from_slice(&up.frame.body);
    b
}

/// Decode an `up` message; only `RSP` and event types pass.
pub fn decode_up(b: &[u8]) -> Result<UpFrame, PlaneError> {
    if b.len() < 11 {
        return Err(PlaneError::Short);
    }
    if b[0] != VERSION {
        return Err(PlaneError::Version(b[0]));
    }
    let mut stamp = [0u8; 8];
    stamp.copy_from_slice(&b[1..9]);
    let host_unix_ms = u64::from_le_bytes(stamp);
    let frame_type = b[9];
    if !is_answer_or_event(frame_type) {
        return Err(PlaneError::Type(frame_type));
    }
    Ok(UpFrame {
        host_unix_ms,
        frame: PlaneFrame {
            frame_type,
            seq: b[10],
            body: b[11..].to_vec(),
        },
    })
}

/// Encode a `down` message.
pub fn encode_down(f: &PlaneFrame) -> Vec<u8> {
    let mut b = Vec::with_capacity(3 + f.body.len());
    b.push(VERSION);
    b.push(f.frame_type);
    b.push(f.seq);
    b.extend_from_slice(&f.body);
    b
}

/// Decode a `down` message; only command types pass.
pub fn decode_down(b: &[u8]) -> Result<PlaneFrame, PlaneError> {
    if b.len() < 3 {
        return Err(PlaneError::Short);
    }
    if b[0] != VERSION {
        return Err(PlaneError::Version(b[0]));
    }
    if !is_command(b[1]) {
        return Err(PlaneError::Type(b[1]));
    }
    Ok(PlaneFrame {
        frame_type: b[1],
        seq: b[2],
        body: b[3..].to_vec(),
    })
}

/// The forwarder's connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Online {
    Online,
    Offline,
}

/// The Northbridge link as the forwarder sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NorthbridgeLink {
    Up,
    Down,
    Updating,
}

/// The retained `state` document (`p2p_cluster_gateway.md` §2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayState {
    pub state: Online,
    pub plane: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub northbridge: Option<NorthbridgeLink>,
    /// The Client's Northbridge firmware update, for the Northbridge document the central
    /// publishes for it; absent when the Client's image carries no Northbridge firmware.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<GatewayUpdate>,
}

/// The `update` object of the `state` document: the Client's [`UpdateSummary`] on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayUpdate {
    /// `idle`, `flashing` or `failed`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundled: Option<String>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl From<&UpdateSummary> for GatewayUpdate {
    fn from(u: &UpdateSummary) -> Self {
        GatewayUpdate {
            state: u.state.to_string(),
            bundled: u.bundled.clone(),
            attempts: u.attempts,
            last_error: u.last_error.clone(),
            note: u.note.clone(),
        }
    }
}

impl GatewayUpdate {
    /// Back into the Northbridge document's summary; a state this build does not know reads
    /// as `failed`, so the Portal shows it rather than offering an update over it.
    pub fn summary(&self) -> UpdateSummary {
        UpdateSummary {
            state: match self.state.as_str() {
                "idle" => "idle",
                "flashing" => "flashing",
                _ => "failed",
            },
            bundled: self.bundled.clone(),
            attempts: self.attempts,
            last_error: self.last_error.clone(),
            note: self.note.clone(),
        }
    }
}

impl GatewayState {
    pub fn online(image: Option<String>, northbridge: NorthbridgeLink) -> Self {
        GatewayState {
            state: Online::Online,
            plane: VERSION,
            image,
            northbridge: Some(northbridge),
            update: None,
        }
    }

    /// The same state carrying the Client's Northbridge update `update`.
    #[must_use]
    pub fn with_update(mut self, update: Option<&UpdateSummary>) -> Self {
        self.update = update.map(GatewayUpdate::from);
        self
    }

    /// The last will: what the broker publishes when the forwarder's session drops.
    pub fn offline() -> Self {
        GatewayState {
            state: Online::Offline,
            plane: VERSION,
            image: None,
            northbridge: None,
            update: None,
        }
    }

    /// Whether the central should serve this gateway: online, on this plane version, with its
    /// Northbridge link up.
    pub fn serviceable(&self) -> bool {
        self.state == Online::Online
            && self.plane == VERSION
            && self.northbridge == Some(NorthbridgeLink::Up)
    }

    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    pub fn parse(b: &[u8]) -> Option<Self> {
        serde_json::from_slice(b).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_name_the_gateway_inside_its_own_family() {
        assert_eq!(
            up_topic("proximos-2200ad33"),
            "proximos/proximos-2200ad33/radio/p2p-gateway/up"
        );
        assert_eq!(
            parse_topic("proximos/proximos-2200ad33/radio/p2p-gateway/state"),
            Some(PlaneTopic::State("proximos-2200ad33".into()))
        );
        assert_eq!(
            parse_topic(&down_topic("hub-1")),
            Some(PlaneTopic::Down("hub-1".into()))
        );
        for bad in [
            "proximos//radio/p2p-gateway/up",
            "proximos/Hub/radio/p2p-gateway/up",
            "proximos/hub/radio/p2p-gateway/up/x",
            "proximos/hub/radio/northbridge",
            "other/hub/radio/p2p-gateway/up",
        ] {
            assert_eq!(parse_topic(bad), None, "{bad}");
        }
    }

    #[test]
    fn up_round_trips_and_refuses_commands() {
        let up = UpFrame {
            host_unix_ms: 1_760_000_000_123,
            frame: PlaneFrame {
                frame_type: 0x84,
                seq: 7,
                body: vec![1, 2, 3],
            },
        };
        let b = encode_up(&up);
        assert_eq!(b[0], VERSION);
        assert_eq!(decode_up(&b), Ok(up));

        let mut cmd = b.clone();
        cmd[9] = 0x11;
        assert_eq!(decode_up(&cmd), Err(PlaneError::Type(0x11)));
        let mut v2 = b.clone();
        v2[0] = 2;
        assert_eq!(decode_up(&v2), Err(PlaneError::Version(2)));
        assert_eq!(decode_up(&b[..10]), Err(PlaneError::Short));
    }

    #[test]
    fn down_round_trips_and_refuses_events() {
        let f = PlaneFrame {
            frame_type: 0x11,
            seq: 200,
            body: vec![0xAA; 30],
        };
        let b = encode_down(&f);
        assert_eq!(decode_down(&b), Ok(f));
        assert_eq!(
            decode_down(&[VERSION, 0x81, 1]),
            Err(PlaneError::Type(0x81))
        );
        assert_eq!(
            decode_down(&[VERSION, 0x41, 1]),
            Err(PlaneError::Type(0x41))
        );
        assert_eq!(decode_down(&[VERSION, 0x01]), Err(PlaneError::Short));
        // An empty body is a command too: GET_INFO.
        assert!(decode_down(&[VERSION, 0x01, 9]).is_ok());
    }

    #[test]
    fn the_state_document_is_serviceable_only_online_with_its_link_up() {
        let on = GatewayState::online(Some("v0.1.0".into()), NorthbridgeLink::Up);
        let json = String::from_utf8(on.to_json()).unwrap();
        assert_eq!(
            json,
            r#"{"state":"online","plane":1,"image":"v0.1.0","northbridge":"up"}"#
        );
        assert_eq!(GatewayState::parse(json.as_bytes()), Some(on.clone()));
        assert!(on.serviceable());
        assert!(!GatewayState::online(None, NorthbridgeLink::Updating).serviceable());
        assert!(!GatewayState::offline().serviceable());
        assert_eq!(
            String::from_utf8(GatewayState::offline().to_json()).unwrap(),
            r#"{"state":"offline","plane":1}"#
        );
        let mut later = on;
        later.plane = 2;
        assert!(!later.serviceable(), "another plane version is not served");
        assert_eq!(GatewayState::parse(b"not json"), None);
    }

    #[test]
    fn the_state_document_carries_the_clients_update() {
        let summary = UpdateSummary {
            state: "flashing",
            bundled: Some("0.4.1+d69b5d68".into()),
            attempts: 1,
            last_error: None,
            note: None,
        };
        let st = GatewayState::online(None, NorthbridgeLink::Updating).with_update(Some(&summary));
        let json = String::from_utf8(st.to_json()).unwrap();
        assert_eq!(
            json,
            r#"{"state":"online","plane":1,"northbridge":"updating","update":{"state":"flashing","bundled":"0.4.1+d69b5d68","attempts":1}}"#
        );
        let back = GatewayState::parse(json.as_bytes()).expect("parses");
        assert_eq!(
            back.update.as_ref().map(GatewayUpdate::summary),
            Some(summary)
        );
        // A Server older than the field ignores it; a state from an older Client has none.
        assert_eq!(
            GatewayState::parse(br#"{"state":"online","plane":1,"northbridge":"up"}"#)
                .and_then(|s| s.update),
            None
        );
        let odd = GatewayUpdate {
            state: "verifying".into(),
            bundled: None,
            attempts: 0,
            last_error: None,
            note: None,
        };
        assert_eq!(
            odd.summary().state,
            "failed",
            "an unknown state is not offered over"
        );
    }
}
