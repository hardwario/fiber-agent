//! The LoRaWAN status document — `plan/control/radio/northbridge_diagnostics.md` §4a.
//!
//! The same shape as the northbridge's (§3), for the LoRaWAN chain on this Hub: the RAK5146
//! card, `chirpstack-concentratord`, `chirpstack-mqtt-forwarder`, the concentrator's own stats
//! and ChirpStack. `proximos-control` gathers the inputs every health tick and publishes the
//! document retained on `proximos/{hostname}/radio/lorawan`.
//!
//! The rules live in [`document`], a pure function over [`Inputs`], so every state is testable
//! without a card, a systemd or a broker; the probes that fill the inputs are thin.

use std::path::Path;
use std::sync::{Arc, Mutex};

use chirpstack_api::prost::Message;
use serde_json::{json, Map, Value};

/// The document's topic for one Hub.
pub fn topic(hostname: &str) -> String {
    format!("{}/{hostname}/radio/lorawan", crate::northbound::ROOT)
}

/// `{prefix}gateway/+/event/stats` — the concentrator's periodic report, via the forwarder.
pub fn stats_topic_filter(prefix: &str) -> String {
    let p = prefix.trim_end_matches('/');
    if p.is_empty() {
        "gateway/+/event/stats".to_string()
    } else {
        format!("{p}/gateway/+/event/stats")
    }
}

/// The concentrator's tty — `com_dev_path` in `chirpstack-concentratord.toml`. A udev link
/// (`99-chirpstack-concentratord.rules`) to whichever `ttyACM<n>` the card's bridge enumerated
/// as, not a fixed `ttyACM0`: any other CDC-ACM device that enumerates first takes that name.
pub const CARD_TTY: &str = "/dev/rak5146";
/// The RAK5146-USB's onboard bridge, the IDs `99-chirpstack-concentratord.rules` matches.
pub const CARD_USB_ID: &str = "0483:5740";
/// The concentrator reports every `stats_interval` (30 s); three missed reports is stale.
pub const STATS_STALE_S: i64 = 90;
/// A card can enumerate late in a boot (#39); before this the document says `starting`.
pub const BOOT_GRACE_S: u64 = 120;

pub const UNIT_CONCENTRATORD: &str = "chirpstack-concentratord.service";
pub const UNIT_FORWARDER: &str = "chirpstack-mqtt-forwarder.service";
pub const UNIT_CHIRPSTACK: &str = "chirpstack.service";

/// What the card looks like from here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub present: bool,
    pub device: String,
    /// `vvvv:pppp` of the tty's USB parent, when sysfs says.
    pub usb_id: Option<String>,
}

/// Look for the card: the tty node, and its USB parent's IDs under `sys_class_tty`
/// (`/sys/class/tty` on a Hub; a directory of a test's choosing otherwise).
///
/// `tty` may be a link, and [`CARD_TTY`] is one. It is resolved before its name is looked up
/// under `sys_class_tty`, which knows only the kernel's `ttyACM<n>`. Looked up by the link's
/// own name, the IDs would read as absent, and a Hub with the card fitted would be taken for
/// one without it (`setmode::hardware_of`, so the renderer's default mode, FB1B and
/// `radio-set-mode`). `device` stays the path asked about, the one the daemon opens.
pub fn probe_card(tty: &Path, sys_class_tty: &Path) -> Card {
    let resolved = std::fs::canonicalize(tty).unwrap_or_else(|_| tty.to_path_buf());
    let name = resolved
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // `device` is the USB interface; its parent is the USB device carrying the IDs.
    let usb = sys_class_tty.join(&name).join("device").join("..");
    let read = |f: &str| {
        std::fs::read_to_string(usb.join(f))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let usb_id = match (read("idVendor"), read("idProduct")) {
        (Some(v), Some(p)) => Some(format!("{v}:{p}")),
        _ => None,
    };
    Card {
        present: tty.exists(),
        device: tty.display().to_string(),
        usb_id,
    }
}

/// One unit's state, as `systemctl show` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub active: String,
    pub sub: String,
    pub result: String,
    pub restarts: u32,
}

impl Unit {
    pub fn is_active(&self) -> bool {
        self.active == "active"
    }
}

/// Parse `systemctl show -p ActiveState -p SubState -p Result -p NRestarts <unit>`.
pub fn parse_systemctl_show(text: &str) -> Option<Unit> {
    let mut active = None;
    let (mut sub, mut result, mut restarts) = (String::new(), String::new(), 0);
    for line in text.lines() {
        match line.split_once('=') {
            Some(("ActiveState", v)) => active = Some(v.trim().to_string()),
            Some(("SubState", v)) => sub = v.trim().to_string(),
            Some(("Result", v)) => result = v.trim().to_string(),
            Some(("NRestarts", v)) => restarts = v.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    Some(Unit {
        active: active?,
        sub,
        result,
        restarts,
    })
}

/// Ask systemd about one unit. `None` when `systemctl` cannot be run or says nothing.
/// Blocking — call it off the async workers.
pub fn query_unit(unit: &str) -> Option<Unit> {
    let out = std::process::Command::new("systemctl")
        .args([
            "show",
            "-p",
            "ActiveState",
            "-p",
            "SubState",
            "-p",
            "Result",
            "-p",
            "NRestarts",
            unit,
        ])
        .output()
        .ok()?;
    parse_systemctl_show(&String::from_utf8_lossy(&out.stdout))
}

/// A binary's version, from the first `x.y.z` in its `--version` output. Blocking.
pub fn tool_version(bin: &str) -> Option<String> {
    let out = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split(|c: char| c.is_whitespace() || c == 'v')
        .find(|w| {
            let parts: Vec<&str> = w.split('.').collect();
            parts.len() >= 2
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        })
        .map(str::to_string)
}

/// The concentrator's last `event/stats`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stats {
    pub at: i64,
    pub gateway_eui: String,
    pub rx_received: u32,
    pub rx_ok: u32,
    pub tx_received: u32,
    pub tx_emitted: u32,
}

/// Decode a forwarder `event/stats` payload (`gw.GatewayStats`, protobuf).
pub fn decode_stats(payload: &[u8], now: i64) -> Option<Stats> {
    let s = chirpstack_api::gw::GatewayStats::decode(payload).ok()?;
    Some(Stats {
        at: now,
        gateway_eui: s.gateway_id,
        rx_received: s.rx_packets_received,
        rx_ok: s.rx_packets_received_ok,
        tx_received: s.tx_packets_received,
        tx_emitted: s.tx_packets_emitted,
    })
}

/// Decode a forwarder `state/conn` payload (`gw.ConnState`): `Some(true)` for `ONLINE`.
pub fn decode_conn(payload: &[u8]) -> Option<bool> {
    let c = chirpstack_api::gw::ConnState::decode(payload).ok()?;
    Some(c.state == chirpstack_api::gw::conn_state::State::Online as i32)
}

/// What the broker task has seen of the concentrator, shared with the health loop.
#[derive(Debug, Clone, Default)]
pub struct Observed(Arc<Mutex<ObservedInner>>);

#[derive(Debug, Clone, Default)]
pub struct ObservedInner {
    pub stats: Option<Stats>,
    /// `ONLINE`/`OFFLINE` and when it was heard.
    pub conn: Option<(bool, i64)>,
}

impl Observed {
    pub fn record_stats(&self, stats: Stats) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).stats = Some(stats);
    }
    pub fn record_conn(&self, online: bool, now: i64) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).conn = Some((online, now));
    }
    pub fn snapshot(&self) -> ObservedInner {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// §4a's states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Disabled,
    Starting,
    Error,
    Degraded,
    Alive,
}

impl State {
    pub const fn as_str(self) -> &'static str {
        match self {
            State::Disabled => "disabled",
            State::Starting => "starting",
            State::Error => "error",
            State::Degraded => "degraded",
            State::Alive => "alive",
        }
    }
}

/// Everything [`document`] judges.
#[derive(Debug, Clone)]
pub struct Inputs {
    pub now: i64,
    pub uptime_s: u64,
    /// `radio.mode` wants the LoRaWAN server (`lorawan`, `dual`).
    pub wants_lorawan: bool,
    /// This Hub is a cluster Client: its forwarder feeds the Server's ChirpStack and it runs
    /// none of its own (cluster.md §2). The renderer drops ChirpStack and Redis on the same
    /// role (§4.2, C3), so a stopped `chirpstack.service` and a silent API are by design,
    /// not a fault, and the document carries no `chirpstack` object.
    pub client: bool,
    pub card: Card,
    pub concentratord: Option<Unit>,
    pub forwarder: Option<Unit>,
    pub chirpstack: Option<Unit>,
    /// The health poll's `api_status`: `ok`, `auth_failed` or `unreachable`.
    pub api: Option<&'static str>,
    pub observed: ObservedInner,
    pub concentratord_version: Option<String>,
    pub chirpstack_version: Option<String>,
}

/// Why a unit is not running, for `last_error`.
fn unit_reason(name: &str, unit: Option<&Unit>) -> String {
    match unit {
        None => format!("{name}: state unknown"),
        Some(u) if u.active == "failed" => format!(
            "{name}: failed ({}), {} restart(s)",
            if u.result.is_empty() {
                "unknown"
            } else {
                &u.result
            },
            u.restarts
        ),
        Some(u) => format!("{name}: {} ({})", u.active, u.sub),
    }
}

/// The state and, when it is not `alive`, the reason — §4a's table, in order.
pub fn judge(i: &Inputs) -> (State, Option<String>) {
    if !i.wants_lorawan {
        return (State::Disabled, None);
    }
    let concentratord_up = i.concentratord.as_ref().is_some_and(Unit::is_active);
    // A Client runs no ChirpStack; for it the chain ends at the forwarder.
    let chirpstack_up = i.client || i.chirpstack.as_ref().is_some_and(Unit::is_active);
    if i.uptime_s < BOOT_GRACE_S && !(i.card.present && concentratord_up && chirpstack_up) {
        return (State::Starting, None);
    }
    if !i.card.present {
        return (
            State::Error,
            Some(format!("card: {} missing", i.card.device)),
        );
    }
    if !concentratord_up {
        return (
            State::Error,
            Some(unit_reason("concentratord", i.concentratord.as_ref())),
        );
    }
    if !chirpstack_up {
        return (
            State::Error,
            Some(unit_reason("chirpstack", i.chirpstack.as_ref())),
        );
    }
    if !i.client && i.api == Some("auth_failed") {
        return (
            State::Error,
            Some("chirpstack: the API refused the credential".into()),
        );
    }
    let stats_age = i
        .observed
        .stats
        .as_ref()
        .map(|s| i.now.saturating_sub(s.at));
    if stats_age.is_none_or(|a| a > STATS_STALE_S) {
        return (
            State::Degraded,
            Some(match stats_age {
                Some(a) => format!("concentrator: no stats for {a} s"),
                None => "concentrator: no stats received yet".into(),
            }),
        );
    }
    if !i.forwarder.as_ref().is_some_and(Unit::is_active) {
        return (
            State::Degraded,
            Some(unit_reason("forwarder", i.forwarder.as_ref())),
        );
    }
    if i.observed.conn.is_some_and(|(online, _)| !online) {
        return (State::Degraded, Some("forwarder: OFFLINE".into()));
    }
    if !i.client && i.api == Some("unreachable") {
        return (
            State::Degraded,
            Some("chirpstack: the API does not answer".into()),
        );
    }
    (State::Alive, None)
}

fn unit_json(u: Option<&Unit>) -> Map<String, Value> {
    let mut m = Map::new();
    match u {
        Some(u) => {
            m.insert("unit".into(), json!(u.active));
            if !u.result.is_empty() {
                m.insert("result".into(), json!(u.result));
            }
            m.insert("restarts".into(), json!(u.restarts));
        }
        None => {
            m.insert("unit".into(), json!("unknown"));
        }
    }
    m
}

/// The §4a document.
pub fn document(i: &Inputs) -> Value {
    let (state, reason) = judge(i);
    let rfc = crate::lorawan::gateway::format_rfc3339;
    let mut doc = Map::new();
    doc.insert("timestamp".into(), json!(rfc(i.now)));
    doc.insert("state".into(), json!(state.as_str()));
    // The card is reported in every mode: the portal's Connection rows show whether it is
    // fitted even while the radio runs P2P (devices.hub.md, amendment 2026-10-30).
    let mut card = Map::new();
    card.insert("present".into(), json!(i.card.present));
    card.insert("device".into(), json!(i.card.device));
    if let Some(id) = &i.card.usb_id {
        card.insert("usb_id".into(), json!(id));
    }
    doc.insert("card".into(), Value::Object(card));
    if state == State::Disabled {
        return Value::Object(doc);
    }

    let mut c = unit_json(i.concentratord.as_ref());
    if let Some(v) = &i.concentratord_version {
        c.insert("version".into(), json!(v));
    }
    if let Some(s) = &i.observed.stats {
        if !s.gateway_eui.is_empty() {
            c.insert("gateway_eui".into(), json!(s.gateway_eui));
        }
        c.insert("last_stats".into(), json!(rfc(s.at)));
        c.insert(
            "stats".into(),
            json!({ "rx_received": s.rx_received, "rx_ok": s.rx_ok,
                    "tx_received": s.tx_received, "tx_emitted": s.tx_emitted }),
        );
    }
    doc.insert("concentratord".into(), Value::Object(c));

    let mut f = unit_json(i.forwarder.as_ref());
    if let Some((online, _)) = i.observed.conn {
        f.insert(
            "conn".into(),
            json!(if online { "online" } else { "offline" }),
        );
    }
    doc.insert("forwarder".into(), Value::Object(f));

    if !i.client {
        let mut cs = unit_json(i.chirpstack.as_ref());
        if let Some(api) = i.api {
            cs.insert("api".into(), json!(api));
        }
        if let Some(v) = &i.chirpstack_version {
            cs.insert("version".into(), json!(v));
        }
        doc.insert("chirpstack".into(), Value::Object(cs));
    }

    doc.insert(
        "last_error".into(),
        match reason {
            Some(message) => json!({ "at": rfc(i.now), "message": message }),
            None => Value::Null,
        },
    );
    Value::Object(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_790_400_000;

    fn active() -> Option<Unit> {
        Some(Unit {
            active: "active".into(),
            sub: "running".into(),
            result: "success".into(),
            restarts: 0,
        })
    }

    fn healthy() -> Inputs {
        Inputs {
            now: NOW,
            uptime_s: 3600,
            wants_lorawan: true,
            client: false,
            card: Card {
                present: true,
                device: CARD_TTY.into(),
                usb_id: Some(CARD_USB_ID.into()),
            },
            concentratord: active(),
            forwarder: active(),
            chirpstack: active(),
            api: Some("ok"),
            observed: ObservedInner {
                stats: Some(Stats {
                    at: NOW - 20,
                    gateway_eui: "0016c001f11b34a6".into(),
                    rx_received: 10,
                    rx_ok: 9,
                    tx_received: 3,
                    tx_emitted: 3,
                }),
                conn: Some((true, NOW - 3000)),
            },
            concentratord_version: Some("4.7.1".into()),
            chirpstack_version: Some("4.19.1".into()),
        }
    }

    #[test]
    fn a_healthy_chain_is_alive_and_reports_everything() {
        let doc = document(&healthy());
        assert_eq!(doc["state"], "alive");
        assert_eq!(doc["card"]["usb_id"], CARD_USB_ID);
        assert_eq!(doc["concentratord"]["gateway_eui"], "0016c001f11b34a6");
        assert_eq!(doc["concentratord"]["stats"]["rx_ok"], 9);
        assert_eq!(doc["forwarder"]["conn"], "online");
        assert_eq!(doc["chirpstack"]["api"], "ok");
        assert_eq!(doc["chirpstack"]["version"], "4.19.1");
        assert_eq!(doc["last_error"], Value::Null);
    }

    #[test]
    fn a_p2p_hub_is_disabled_and_reports_only_its_card() {
        let mut i = healthy();
        i.wants_lorawan = false;
        let doc = document(&i);
        assert_eq!(doc["state"], "disabled");
        assert_eq!(doc["card"]["present"], true);
        assert!(doc.get("concentratord").is_none());
        assert!(doc.get("forwarder").is_none());
        assert!(doc.get("chirpstack").is_none());
    }

    #[test]
    fn a_missing_card_is_an_error_after_the_boot_grace_and_starting_before_it() {
        let mut i = healthy();
        i.card.present = false;
        assert_eq!(judge(&i).0, State::Error);
        assert_eq!(judge(&i).1.as_deref(), Some("card: /dev/rak5146 missing"));
        i.uptime_s = 30;
        assert_eq!(judge(&i).0, State::Starting);
    }

    #[test]
    fn a_failed_unit_names_its_result_and_restarts() {
        let mut i = healthy();
        i.concentratord = Some(Unit {
            active: "failed".into(),
            sub: "failed".into(),
            result: "exit-code".into(),
            restarts: 3,
        });
        let (state, reason) = judge(&i);
        assert_eq!(state, State::Error);
        assert_eq!(
            reason.as_deref(),
            Some("concentratord: failed (exit-code), 3 restart(s)")
        );
    }

    #[test]
    fn a_stopped_chirpstack_is_an_error() {
        let mut i = healthy();
        i.chirpstack = Some(Unit {
            active: "inactive".into(),
            sub: "dead".into(),
            result: "success".into(),
            restarts: 0,
        });
        assert_eq!(judge(&i).0, State::Error);
        assert_eq!(judge(&i).1.as_deref(), Some("chirpstack: inactive (dead)"));
    }

    #[test]
    fn stale_stats_offline_forwarder_or_a_silent_api_are_degraded() {
        let mut i = healthy();
        i.observed.stats.as_mut().expect("stats").at = NOW - 200;
        assert_eq!(judge(&i).0, State::Degraded);

        let mut i = healthy();
        i.observed.conn = Some((false, NOW));
        assert_eq!(judge(&i).0, State::Degraded);

        let mut i = healthy();
        i.api = Some("unreachable");
        assert_eq!(judge(&i).0, State::Degraded);

        let mut i = healthy();
        i.api = Some("auth_failed");
        assert_eq!(
            judge(&i).0,
            State::Error,
            "a refused credential is not transient"
        );
    }

    /// cluster.md §2: a Client runs no ChirpStack, so a stopped unit and an unanswered API
    /// leave it alive; the forwarder and concentratord checks still apply.
    #[test]
    fn a_client_is_alive_without_a_chirpstack() {
        let mut i = healthy();
        i.client = true;
        i.chirpstack = Some(Unit {
            active: "inactive".into(),
            sub: "dead".into(),
            result: "success".into(),
            restarts: 0,
        });
        i.api = Some("unreachable");
        let doc = document(&i);
        assert_eq!(doc["state"], "alive");
        assert!(
            doc.get("chirpstack").is_none(),
            "no chirpstack object on a Client"
        );
        assert_eq!(doc["forwarder"]["unit"], "active");

        i.api = Some("auth_failed");
        assert_eq!(judge(&i).0, State::Alive);

        // Inside the boot grace a Client waits only for the card and concentratord.
        i.uptime_s = 10;
        assert_eq!(judge(&i).0, State::Alive);

        i.uptime_s = 3600;
        i.forwarder = None;
        assert_eq!(
            judge(&i).0,
            State::Degraded,
            "the forwarder is still checked"
        );
        i.forwarder = active();
        i.concentratord = None;
        assert_eq!(judge(&i).0, State::Error, "concentratord is still checked");

        // The same Hub as a Server is in error for the stopped ChirpStack.
        let mut server = healthy();
        server.chirpstack = i.chirpstack.clone();
        assert_eq!(judge(&server).0, State::Error);
    }

    #[test]
    fn systemctl_show_parses() {
        let u = parse_systemctl_show(
            "ActiveState=failed\nSubState=failed\nResult=exit-code\nNRestarts=2\n",
        )
        .expect("parses");
        assert_eq!(u.active, "failed");
        assert_eq!(u.result, "exit-code");
        assert_eq!(u.restarts, 2);
        assert!(parse_systemctl_show("").is_none());
    }

    #[test]
    fn the_card_is_found_by_its_tty_and_usb_ids() {
        let root = std::env::temp_dir().join(format!("lrw-card-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dev = root.join("dev");
        let usbdev = root.join("sys/usbdev");
        let iface = usbdev.join("iface");
        std::fs::create_dir_all(&iface).expect("dirs");
        std::fs::create_dir_all(root.join("sys/class/tty/ttyACM0")).expect("dirs");
        std::fs::write(usbdev.join("idVendor"), "0483\n").expect("vid");
        std::fs::write(usbdev.join("idProduct"), "5740\n").expect("pid");
        std::os::unix::fs::symlink(&iface, root.join("sys/class/tty/ttyACM0/device"))
            .expect("link");
        std::fs::create_dir_all(&dev).expect("dev");
        let tty = dev.join("ttyACM0");

        let missing = probe_card(&tty, &root.join("sys/class/tty"));
        assert!(!missing.present);
        std::fs::write(&tty, "").expect("tty");
        let card = probe_card(&tty, &root.join("sys/class/tty"));
        assert!(card.present);
        assert_eq!(card.usb_id.as_deref(), Some(CARD_USB_ID));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The shipped shape: `/dev/rak5146` is udev's relative link to the card's `ttyACM<n>`,
    /// and sysfs knows the card only by that kernel name. `ttyACM1` here, the case the link
    /// exists for — something else enumerated first and took `ttyACM0`.
    #[test]
    fn the_card_is_found_through_the_udev_link() {
        let root = std::env::temp_dir().join(format!("lrw-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dev = root.join("dev");
        let sys_class_tty = root.join("sys/class/tty");
        let usbdev = root.join("sys/usbdev");
        let iface = usbdev.join("iface");
        std::fs::create_dir_all(&iface).expect("dirs");
        std::fs::create_dir_all(sys_class_tty.join("ttyACM1")).expect("dirs");
        std::fs::write(usbdev.join("idVendor"), "0483\n").expect("vid");
        std::fs::write(usbdev.join("idProduct"), "5740\n").expect("pid");
        std::os::unix::fs::symlink(&iface, sys_class_tty.join("ttyACM1/device")).expect("link");
        std::fs::create_dir_all(&dev).expect("dev");
        std::fs::write(dev.join("ttyACM1"), "").expect("tty");
        let link = dev.join("rak5146");
        std::os::unix::fs::symlink("ttyACM1", &link).expect("udev link");

        let card = probe_card(&link, &sys_class_tty);
        assert!(card.present);
        assert_eq!(
            card.usb_id.as_deref(),
            Some(CARD_USB_ID),
            "the link must be resolved to ttyACM1 before sysfs is asked"
        );
        assert_eq!(
            card.device,
            link.display().to_string(),
            "the path the daemon opens"
        );

        // The card pulled: udev removes the node, and a link left behind dangles. Neither is a
        // card.
        std::fs::remove_file(dev.join("ttyACM1")).expect("unplug");
        let gone = probe_card(&link, &sys_class_tty);
        assert!(!gone.present);
        std::fs::remove_file(&link).expect("udev removes the link");
        let gone = probe_card(&link, &sys_class_tty);
        assert!(!gone.present);
        assert_eq!(gone.usb_id, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stats_and_conn_state_decode_from_the_forwarders_protobuf() {
        let stats = chirpstack_api::gw::GatewayStats {
            gateway_id: "0016c001f11b34a6".into(),
            rx_packets_received: 12,
            rx_packets_received_ok: 11,
            tx_packets_received: 2,
            tx_packets_emitted: 2,
            ..Default::default()
        };
        let s = decode_stats(&stats.encode_to_vec(), NOW).expect("decodes");
        assert_eq!(s.gateway_eui, "0016c001f11b34a6");
        assert_eq!((s.rx_ok, s.tx_emitted), (11, 2));

        let online = chirpstack_api::gw::ConnState {
            state: chirpstack_api::gw::conn_state::State::Online as i32,
            ..Default::default()
        };
        assert_eq!(decode_conn(&online.encode_to_vec()), Some(true));
        let offline = chirpstack_api::gw::ConnState::default();
        assert_eq!(decode_conn(&offline.encode_to_vec()), Some(false));
    }

    #[test]
    fn the_topic_is_per_hub() {
        assert_eq!(
            topic("fiber-4e970f4a"),
            "proximos/fiber-4e970f4a/radio/lorawan"
        );
    }
}
