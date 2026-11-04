//! The golden vectors of the Northbridge ↔ Hub link (#470 C4), and a deframer fuzz.
//!
//! `plan/control/radio/p2p_tower_link_vectors.txt` is the single source both sides test
//! against: the Northbridge's ztest `tests/tower_link_vectors` reads the same file. Every line
//! is checked byte for byte both ways here — the central's encoders produce the exact body and
//! wire, and the decoder turns the wire back into the exact frame and fields.

use std::collections::BTreeMap;

use crate::p2p::hdlc::{self, Decoder, Frame};
use crate::p2p::modem::{
    self, BootEvent, Event, FaultEvent, RadioConfig, RxEvent, Status, TxDone, TxSchedule, TxStatus,
};
use crate::p2p::modem_io::{AckCtrlCounters, ModemInfo};
use crate::p2p::tower::link::{
    self, AckTail, NodeListEntry, NodeListPage, TwrCmd, TwrCtrLow, TwrTx, TwrUplink, TxOutcome,
    UplinkAck,
};

const VECTORS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../plan/control/radio/p2p_tower_link_vectors.txt"
));

/// One line of the vector file.
#[derive(Debug, Clone)]
struct Vector {
    name: String,
    dir: String,
    msg: String,
    fields: BTreeMap<String, String>,
    wire: Vec<u8>,
}

fn hex(s: &str) -> Vec<u8> {
    if s == "-" {
        return Vec::new();
    }
    assert!(s.len().is_multiple_of(2), "odd hex {s:?}");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex digit"))
        .collect()
}

fn int(s: &str) -> i128 {
    if let Some(h) = s.strip_prefix("0x") {
        i128::from_str_radix(h, 16).expect("hex int")
    } else {
        s.parse().expect("int")
    }
}

impl Vector {
    fn get(&self, k: &str) -> &str {
        self.fields
            .get(k)
            .unwrap_or_else(|| panic!("{}: no field {k}", self.name))
    }
    fn n<T: TryFrom<i128>>(&self, k: &str) -> T {
        T::try_from(int(self.get(k)))
            .unwrap_or_else(|_| panic!("{}: field {k} out of range", self.name))
    }
    fn bytes(&self, k: &str) -> Vec<u8> {
        hex(self.get(k))
    }
    fn seq(&self) -> u8 {
        self.n("seq")
    }
    fn body(&self) -> Vec<u8> {
        self.bytes("body")
    }
    fn ok(&self) -> bool {
        match self.get("verdict") {
            "ok" => true,
            "bad_param" => false,
            v => panic!("{}: verdict {v}", self.name),
        }
    }
}

fn parse(text: &str) -> Vec<Vector> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let mut tok = l.split_whitespace();
            let name = tok.next().expect("name").to_string();
            let dir = tok.next().expect("dir").to_string();
            let msg = tok.next().expect("msg").to_string();
            let mut fields = BTreeMap::new();
            for t in tok {
                let (k, v) = t
                    .split_once('=')
                    .unwrap_or_else(|| panic!("{name}: token {t}"));
                assert!(
                    fields.insert(k.to_string(), v.to_string()).is_none(),
                    "{name}: {k} twice"
                );
            }
            let wire = hex(fields.get("wire").expect("wire"));
            Vector {
                name,
                dir,
                msg,
                fields,
                wire,
            }
        })
        .collect()
}

fn vectors() -> Vec<Vector> {
    let v = parse(VECTORS);
    let mut names = std::collections::HashSet::new();
    for x in &v {
        assert!(names.insert(x.name.clone()), "duplicate vector {}", x.name);
    }
    v
}

/// The link type of a message vector.
fn frame_type(v: &Vector) -> u8 {
    match v.msg.as_str() {
        "GET_INFO" => modem::cmd::GET_INFO,
        "SET_RADIO_CONFIG" => modem::cmd::SET_RADIO_CONFIG,
        "RX_STOP" => modem::cmd::RX_STOP,
        "TX_SCHEDULE" => modem::cmd::TX_SCHEDULE,
        "TX_CANCEL" => modem::cmd::TX_CANCEL,
        "TWR_START" => link::cmd::TWR_START,
        "TWR_NODE_ADD" => link::cmd::TWR_NODE_ADD,
        "TWR_NODE_REMOVE" => link::cmd::TWR_NODE_REMOVE,
        "TWR_CTR_BLOCK" => link::cmd::TWR_CTR_BLOCK,
        "TWR_QUEUE_PUSH" => link::cmd::TWR_QUEUE_PUSH,
        "TWR_QUEUE_DROP" => link::cmd::TWR_QUEUE_DROP,
        "TWR_NODE_LIST" => link::cmd::TWR_NODE_LIST,
        "TWR_EVT_ACK" => link::cmd::TWR_EVT_ACK,
        "TWR_TIME_SYNC" => link::cmd::TWR_TIME_SYNC,
        "RSP" => modem::RSP_FLAG | v.n::<u8>("cmd"),
        "RSP_GET_INFO" => modem::RSP_FLAG | modem::cmd::GET_INFO,
        "RSP_NODE_LIST" => modem::RSP_FLAG | link::cmd::TWR_NODE_LIST,
        "EVT_BOOT" => modem::evt::BOOT,
        "EVT_RX" => modem::evt::RX,
        "EVT_TX_DONE" => modem::evt::TX_DONE,
        "EVT_LOG" => modem::evt::LOG,
        "EVT_TWR_UPLINK" => link::evt::TWR_UPLINK,
        "EVT_TWR_TX" => link::evt::TWR_TX,
        "EVT_TWR_CTR_LOW" => link::evt::TWR_CTR_LOW,
        "FRAME" => v.n("type"),
        m => panic!("{}: unknown msg {m}", v.name),
    }
}

fn frame_of(v: &Vector) -> Frame {
    Frame {
        frame_type: frame_type(v),
        seq: v.seq(),
        body: v.body(),
    }
}

fn status(name: &str) -> Status {
    match name {
        "ok" => Status::Ok,
        "bad_param" => Status::BadParam,
        "busy" => Status::Busy,
        "invalid_state" => Status::InvalidState,
        "late" => Status::Late,
        "internal" => Status::Internal,
        "no_space" => Status::NoSpace,
        "not_found" => Status::NotFound,
        "unsupported" => Status::Unsupported,
        s => panic!("status {s}"),
    }
}

fn radio_of(v: &Vector, flags: &str) -> RadioConfig {
    RadioConfig {
        freq_hz: v.n("freq_hz"),
        sf: v.n("sf"),
        bw: v.n("bw"),
        cr: v.n("cr"),
        preamble: v.n("preamble"),
        tx_power_dbm: v.n("tx_power_dbm"),
        flags: v.n(flags),
    }
}

/// The central's command for a `TWR_*` vector.
fn twr_cmd(v: &Vector) -> TwrCmd {
    match v.msg.as_str() {
        "TWR_START" => TwrCmd::Start {
            net_id: v.n("net_id"),
            turnaround_ms: v.n("turnaround_ms"),
            dl_gap_ms: v.n("dl_gap_ms"),
            flags: v.n("flags"),
        },
        "TWR_NODE_ADD" => TwrCmd::NodeAdd {
            addr: v.n("addr"),
            session_key: v.bytes("key").try_into().expect("16 B key"),
            last_seen: v.n("last_seen"),
            flags: v.n("flags"),
        },
        "TWR_NODE_REMOVE" => TwrCmd::NodeRemove { addr: v.n("addr") },
        "TWR_CTR_BLOCK" => TwrCmd::CtrBlock {
            first: v.n("first"),
            last: v.n("last"),
        },
        "TWR_QUEUE_PUSH" => TwrCmd::QueuePush {
            addr: v.n("addr"),
            item: v.n("item"),
            ttl_s: v.n("ttl_s"),
            flags: v.n("flags"),
            plaintext: v.bytes("pt"),
        },
        "TWR_QUEUE_DROP" => TwrCmd::QueueDrop {
            addr: v.n("addr"),
            item: v.n("item"),
        },
        "TWR_NODE_LIST" => TwrCmd::NodeList {
            start: v.n("start"),
        },
        "TWR_EVT_ACK" => TwrCmd::EvtAck {
            seq: v.n("ack_seq"),
        },
        "TWR_TIME_SYNC" => TwrCmd::TimeSync {
            now_ms: v.n("now_ms"),
            unix_ms: v.n("unix_ms"),
        },
        m => panic!("{m} is not a TWR_* command"),
    }
}

/// A host → Northbridge vector: the central encodes exactly `body`, or refuses a `bad_param`
/// body before it reaches the link (§7.1).
fn check_command(v: &Vector) {
    let body = v.body();
    let encoded: Result<Vec<u8>, String> = match v.msg.as_str() {
        "GET_INFO" | "RX_STOP" => Ok(Vec::new()),
        "SET_RADIO_CONFIG" => {
            let cfg = radio_of(v, "flags");
            // The SF is the one field the central range-checks on its own encode path.
            cfg.with_spreading_factor(cfg.sf)
                .map(|c| c.encode())
                .map_err(|e| e.to_string())
        }
        "TX_SCHEDULE" => TxSchedule {
            tx_id: v.n("tx_id"),
            t_ms: v.n("t_ms"),
            frame: v.bytes("frame"),
        }
        .encode()
        .map_err(|e| e.to_string()),
        "TX_CANCEL" => Ok(vec![v.n("tx_id")]),
        _ => {
            let c = twr_cmd(v);
            assert_eq!(c.cmd(), frame_type(v), "{}: command type", v.name);
            assert_eq!(
                c.expects_rsp(),
                v.msg != "TWR_EVT_ACK",
                "{}: RSP expected",
                v.name
            );
            c.encode().map_err(|e| e.to_string())
        }
    };
    if v.ok() {
        assert_eq!(
            encoded.as_deref().map(hex_str),
            Ok(hex_str(&body)),
            "{}: body",
            v.name
        );
    } else {
        assert!(
            encoded.is_err(),
            "{}: the Northbridge answers BAD_PARAM, the central encoded {:?}",
            v.name,
            encoded.map(|b| hex_str(&b))
        );
    }
}

fn hex_str(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn info_of(v: &Vector) -> ModemInfo {
    let counters: Vec<u32> = v
        .get("counters")
        .split(',')
        .map(|c| u32::try_from(int(c)).expect("u32 counter"))
        .collect();
    ModemInfo {
        fw: v.bytes("fw_ver").try_into().expect("fw_ver"),
        git_sha: v.bytes("git_sha").try_into().expect("git_sha"),
        reset_cause: v.n("reset_cause"),
        build_flags: v.n("build_flags"),
        now_ms: v.n("now_ms"),
        state: v.n("state"),
        net_id: v.n("net_id"),
        turnaround_ms: v.n("turnaround_ms"),
        dl_gap_ms: v.n("dl_gap_ms"),
        modulation: v.n("modulation"),
        radio: radio_of(v, "radio_flags"),
        err_flags: v.n("err_flags"),
        last_err_code: v.n("last_err_code"),
        last_err_age_s: v.n("last_err_age_s"),
        counters: counters.try_into().expect("25 counters"),
        ack_turnaround_last_us: v.n("ack_turnaround_last_us"),
        ack_turnaround_max_us: v.n("ack_turnaround_max_us"),
        airtime_ms_last_hour: v.n("airtime_ms_last_hour"),
        peers: v.n("peers"),
        peers_max: v.n("peers_max"),
        queue_used: v.n("queue_used"),
        queue_max: v.n("queue_max"),
        evt_ring_used: v.n("evt_ring_used"),
        evt_ring_max: v.n("evt_ring_max"),
        ctr_next: v.n("ctr_next"),
        ctr_last: v.n("ctr_last"),
        evt_head_seq: Some(v.n("evt_head_seq")),
        // The 180 B status (#470 §13.5) names both ACK-tail counters; a 172 B one neither.
        ack_ctrl: v
            .fields
            .contains_key("ack_ctrl_sent")
            .then(|| AckCtrlCounters {
                sent: v.n("ack_ctrl_sent"),
                time_no_anchor: v.n("ack_time_no_anchor"),
            }),
    }
}

/// A Northbridge → host vector: the central decodes the fields, and re-encodes the body where
/// it has an encoder (its fakes and tests use them).
fn check_reply(v: &Vector) {
    let f = frame_of(v);
    let body = v.body();
    let name = &v.name;
    match v.msg.as_str() {
        "RSP" => {
            assert_eq!(body.len(), 1, "{name}: a status-only RSP");
            assert_eq!(
                Status::from_byte(body[0]),
                status(v.get("status")),
                "{name}"
            );
        }
        "RSP_GET_INFO" => {
            assert_eq!(Status::from_byte(body[0]), Status::Ok, "{name}");
            let want = info_of(v);
            let len = if want.ack_ctrl.is_some() {
                ModemInfo::LEN_ACK_CTRL
            } else {
                ModemInfo::LEN_HEAD
            };
            assert_eq!(body.len(), 1 + len, "{name}: {len} B after the status");
            let got = ModemInfo::parse(&body[1..]).expect("GET_INFO parses");
            assert_eq!(got, want, "{name}: decoded");
            assert_eq!(want.encode(), body[1..], "{name}: encoded");
        }
        "RSP_NODE_LIST" => {
            assert_eq!(Status::from_byte(body[0]), Status::Ok, "{name}");
            let entries = match v.get("entries") {
                "-" => Vec::new(),
                e => e
                    .split(',')
                    .map(|x| {
                        let p: Vec<&str> = x.split(':').collect();
                        NodeListEntry {
                            addr: u32::try_from(int(p[0])).expect("addr"),
                            last_seen: u32::try_from(int(p[1])).expect("last_seen"),
                            flags: u8::try_from(int(p[2])).expect("flags"),
                        }
                    })
                    .collect(),
            };
            let next: u16 = v.n("next");
            let want = NodeListPage {
                total: v.n("total"),
                next: (next != u16::MAX).then_some(next),
                entries,
            };
            assert_eq!(NodeListPage::parse(&body[1..]), Some(want), "{name}");
        }
        "EVT_BOOT" => {
            let boot = BootEvent {
                fw_ver: v.bytes("fw_ver").try_into().expect("fw_ver"),
                git_sha: v.bytes("git_sha").try_into().expect("git_sha"),
                reset_cause: v.n("reset_cause"),
            };
            assert_eq!(Event::from_frame(&f), Some(Event::Boot(boot)), "{name}");
            assert_eq!(boot.encode(), body, "{name}: encoded");
        }
        "EVT_RX" => {
            let rx = RxEvent {
                t_ms: v.n("t_ms"),
                rssi_dbm: v.n("rssi_dbm"),
                snr_q_db: v.n("snr_q_db"),
                frame: v.bytes("frame"),
            };
            assert_eq!(Event::from_frame(&f), Some(Event::Rx(rx)), "{name}");
        }
        "EVT_TX_DONE" => {
            let done = TxDone {
                tx_id: v.n("tx_id"),
                status: match v.n::<u8>("tx_status") {
                    0 => TxStatus::Ok,
                    1 => TxStatus::Late,
                    2 => TxStatus::RadioErr,
                    3 => TxStatus::Cancelled,
                    o => TxStatus::Unknown(o),
                },
                t_actual_ms: v.n("t_actual_ms"),
            };
            assert_eq!(Event::from_frame(&f), Some(Event::TxDone(done)), "{name}");
        }
        "EVT_LOG" => {
            let fault = FaultEvent {
                level: v.n("level"),
                code: v.n("code"),
                text: String::from_utf8(v.bytes("text")).expect("ASCII detail"),
            };
            assert_eq!(Event::from_frame(&f), Some(Event::Fault(fault)), "{name}");
        }
        "EVT_TWR_UPLINK" => {
            let up = TwrUplink {
                t_ms: v.n("t_ms"),
                rssi_dbm: v.n("rssi_dbm"),
                snr_q_db: v.n("snr_q_db"),
                frame_flags: v.n("frame_flags"),
                addr: v.n("addr"),
                counter: v.n("counter"),
                // The code in bits 0..5, the ACK tail in bits 6 and 7 (#470 §13.5).
                ack: match v.n::<u8>("ack") & 0x3F {
                    0 => UplinkAck::NotConfirmed,
                    1 => UplinkAck::Sent,
                    2 => UplinkAck::SentPending,
                    3 => UplinkAck::Late,
                    4 => UplinkAck::NoCounter,
                    5 => UplinkAck::SuppressedRingFull,
                    o => UplinkAck::Unknown(o),
                },
                ack_tail: AckTail {
                    sent: v.n::<u8>("ack") & 0x80 != 0,
                    time: v.n::<u8>("ack") & 0x40 != 0,
                },
                plaintext: v.bytes("pt"),
            };
            assert_eq!(up.encode(), body, "{name}: encoded");
            assert_eq!(
                Event::from_frame(&f),
                Some(Event::TwrUplink { seq: v.seq(), up }),
                "{name}"
            );
        }
        "EVT_TWR_TX" => {
            let rssi: i8 = v.n("ack_rssi");
            let tx = TwrTx {
                addr: v.n("addr"),
                item: v.n("item"),
                outcome: match v.n::<u8>("outcome") {
                    0 => TxOutcome::Delivered,
                    1 => TxOutcome::Sent,
                    2 => TxOutcome::NotDelivered,
                    3 => TxOutcome::Expired,
                    4 => TxOutcome::RadioErr,
                    5 => TxOutcome::NoCounter,
                    o => TxOutcome::Unknown(o),
                },
                gw_counter: v.n("gw_counter"),
                node_ack_counter: v.n("node_ack_counter"),
                ack_rssi: (rssi != 0x7F).then_some(rssi),
            };
            assert_eq!(tx.encode(), body, "{name}: encoded");
            assert_eq!(
                Event::from_frame(&f),
                Some(Event::TwrTx { seq: v.seq(), tx }),
                "{name}"
            );
        }
        "EVT_TWR_CTR_LOW" => {
            let low = TwrCtrLow {
                next: v.n("next"),
                last: v.n("last"),
            };
            assert_eq!(low.encode(), body, "{name}: encoded");
            assert_eq!(Event::from_frame(&f), Some(Event::TwrCtrLow(low)), "{name}");
        }
        m => panic!("{name}: unknown n2h msg {m}"),
    }
}

fn decode_all(wire: &[u8]) -> (Vec<Frame>, hdlc::DecodeStats) {
    let mut d = Decoder::new();
    let frames = d.push(wire);
    (frames, d.stats)
}

/// Every message vector: the wire is the frame, byte for byte both ways, whole and one byte at
/// a time; then the message itself (fields ↔ body).
#[test]
fn every_link_vector_encodes_and_decodes_byte_for_byte() {
    let all = vectors();
    let by_name: BTreeMap<&str, &Vector> = all.iter().map(|v| (v.name.as_str(), v)).collect();
    let mut counted = BTreeMap::<&str, usize>::new();
    for v in &all {
        *counted.entry(v.dir.as_str()).or_default() += 1;
        match v.msg.as_str() {
            "STREAM" => {
                let want: Vec<Frame> = match v.get("expect") {
                    "-" => Vec::new(),
                    e => e.split(',').map(|n| frame_of(by_name[n])).collect(),
                };
                assert_eq!(decode_all(&v.wire).0, want, "{}", v.name);
                let mut d = Decoder::new();
                let bytewise: Vec<Frame> = v.wire.iter().flat_map(|b| d.push(&[*b])).collect();
                assert_eq!(bytewise, want, "{}: byte by byte", v.name);
            }
            "REJECT" => {
                let (frames, stats) = decode_all(&v.wire);
                assert!(frames.is_empty(), "{}: decoded {frames:?}", v.name);
                let (crc, short, over) = match v.get("why") {
                    "crc" => (1, 0, 0),
                    "short" => (0, 1, 0),
                    "oversized" => (0, 0, 1),
                    w => panic!("why {w}"),
                };
                assert_eq!(
                    (stats.crc_errors, stats.too_short, stats.oversized),
                    (crc, short, over),
                    "{}: counted",
                    v.name
                );
            }
            _ => {
                let f = frame_of(v);
                assert_eq!(
                    hex_str(&hdlc::encode(f.frame_type, f.seq, &f.body)),
                    hex_str(&v.wire),
                    "{}: wire",
                    v.name
                );
                assert_eq!(
                    decode_all(&v.wire).0,
                    vec![f.clone()],
                    "{}: deframed",
                    v.name
                );
                let mut d = Decoder::new();
                let bytewise: Vec<Frame> = v.wire.iter().flat_map(|b| d.push(&[*b])).collect();
                assert_eq!(bytewise, vec![f], "{}: byte by byte", v.name);
                match v.dir.as_str() {
                    "h2n" => check_command(v),
                    "n2h" => check_reply(v),
                    "raw" => {}
                    d => panic!("{}: dir {d}", v.name),
                }
            }
        }
    }
    // Every opcode both sides exchange has a vector.
    let msgs: std::collections::BTreeSet<&str> = all.iter().map(|v| v.msg.as_str()).collect();
    for m in [
        "GET_INFO",
        "SET_RADIO_CONFIG",
        "RX_STOP",
        "TX_SCHEDULE",
        "TX_CANCEL",
        "TWR_START",
        "TWR_NODE_ADD",
        "TWR_NODE_REMOVE",
        "TWR_CTR_BLOCK",
        "TWR_QUEUE_PUSH",
        "TWR_QUEUE_DROP",
        "TWR_NODE_LIST",
        "TWR_EVT_ACK",
        "TWR_TIME_SYNC",
        "RSP",
        "RSP_GET_INFO",
        "RSP_NODE_LIST",
        "EVT_BOOT",
        "EVT_RX",
        "EVT_TX_DONE",
        "EVT_LOG",
        "EVT_TWR_UPLINK",
        "EVT_TWR_TX",
        "EVT_TWR_CTR_LOW",
        "FRAME",
        "STREAM",
        "REJECT",
    ] {
        assert!(msgs.contains(m), "no vector for {m}");
    }
    // Every message kind also has a hand-built vector (hb_*), typed from the link doc's
    // tables rather than produced by a codec.
    for m in msgs
        .iter()
        .filter(|m| !["FRAME", "STREAM", "REJECT"].contains(m))
    {
        assert!(
            all.iter().any(|v| v.msg == *m && v.name.starts_with("hb_")),
            "no hand-built vector for {m}"
        );
    }
    assert_eq!(all.len(), 130, "vector count: {counted:?}");
    // Both GET_INFO lengths the central reads with a ring head are pinned, hand-built too.
    for len in [ModemInfo::LEN_HEAD, ModemInfo::LEN_ACK_CTRL] {
        assert!(
            all.iter().any(|v| v.msg == "RSP_GET_INFO"
                && v.name.starts_with("hb_")
                && v.body().len() == 1 + len),
            "no hand-built {len} B GET_INFO"
        );
    }
}

/// GET_INFO body offsets after the status byte, straight from the link doc's §3 table
/// (hynek/p2p-tower-link-doc): (offset, field, width, signed). Checked against the raw bytes
/// of every RSP_GET_INFO vector, without the codec.
const GET_INFO_OFFSETS: &[(usize, &str, usize, bool)] = &[
    (0, "fw_ver", 3, false),
    (3, "git_sha", 4, false),
    (7, "reset_cause", 1, false),
    (8, "build_flags", 1, false),
    (9, "now_ms", 8, false),
    (17, "state", 1, false),
    (18, "net_id", 4, false),
    (22, "turnaround_ms", 1, false),
    (23, "dl_gap_ms", 1, false),
    (24, "modulation", 1, false),
    (25, "freq_hz", 4, false),
    (29, "sf", 1, false),
    (30, "bw", 1, false),
    (31, "cr", 1, false),
    (32, "preamble", 2, false),
    (34, "tx_power_dbm", 1, true),
    (35, "radio_flags", 1, false),
    (36, "err_flags", 4, false),
    (40, "last_err_code", 1, false),
    (41, "last_err_age_s", 4, false),
    (45, "counters", 100, false),
    (145, "ack_turnaround_last_us", 2, false),
    (147, "ack_turnaround_max_us", 2, false),
    (149, "airtime_ms_last_hour", 4, false),
    (153, "peers", 2, false),
    (155, "peers_max", 2, false),
    (157, "queue_used", 2, false),
    (159, "queue_max", 2, false),
    (161, "evt_ring_used", 1, false),
    (162, "evt_ring_max", 1, false),
    (163, "ctr_next", 4, false),
    (167, "ctr_last", 4, false),
    (171, "evt_head_seq", 1, false),
    (172, "ack_ctrl_sent", 4, false),
    (176, "ack_time_no_anchor", 4, false),
];

fn le(b: &[u8], signed: bool) -> i128 {
    let mut v: i128 = 0;
    for (i, x) in b.iter().enumerate() {
        v |= i128::from(*x) << (8 * i);
    }
    if signed && b.last().is_some_and(|x| x & 0x80 != 0) {
        v -= 1i128 << (8 * b.len());
    }
    v
}

#[test]
fn get_info_vectors_match_the_link_doc_offsets_field_by_field() {
    let mut at = 0;
    for (off, name, width, _) in GET_INFO_OFFSETS {
        assert_eq!(*off, at, "§3 table: {name} not contiguous");
        at += width;
    }
    assert_eq!(at, 180, "§3: 180 B after the status byte");
    let infos: Vec<Vector> = vectors()
        .into_iter()
        .filter(|v| v.msg == "RSP_GET_INFO")
        .collect();
    assert!(infos.iter().any(|v| v.name == "hb_rsp_get_info"));
    assert!(infos.iter().any(|v| v.name == "hb_rsp_get_info_180"));
    for v in &infos {
        let body = v.body();
        // 180 B, or the 172 B of Northbridge 0.3.1 final without the ACK-tail counters.
        assert!(
            [1 + 172, 1 + 180].contains(&body.len()),
            "{}: {} B",
            v.name,
            body.len()
        );
        assert_eq!(body[0], 0, "{}: status OK", v.name);
        let data = &body[1..];
        for (off, name, width, signed) in GET_INFO_OFFSETS
            .iter()
            .filter(|(off, _, width, _)| off + width <= data.len())
        {
            let raw = &data[*off..off + width];
            match *name {
                "fw_ver" | "git_sha" => assert_eq!(hex_str(raw), v.get(name), "{}: {name}", v.name),
                "counters" => {
                    let want: Vec<i128> = v.get(name).split(',').map(int).collect();
                    let got: Vec<i128> = raw.chunks(4).map(|c| le(c, false)).collect();
                    assert_eq!(got, want, "{}: counters at 45", v.name);
                }
                _ => assert_eq!(
                    le(raw, *signed),
                    int(v.get(name)),
                    "{}: {name} at {off}",
                    v.name
                ),
            }
        }
    }
}

/// xorshift64*: deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// A random but valid frame: bodies biased towards the two special bytes.
fn random_frame(r: &mut Rng) -> Frame {
    let len = match r.below(8) {
        0 => 0,
        1 => 280,
        _ => r.below(281),
    };
    let body = (0..len)
        .map(|_| match r.below(4) {
            0 => hdlc::FLAG,
            1 => hdlc::ESC,
            _ => r.next() as u8,
        })
        .collect();
    Frame {
        frame_type: r.next() as u8,
        seq: r.next() as u8,
        body,
    }
}

/// Garbage that may hold flags, escapes and fragments of real frames.
fn garbage(r: &mut Rng, vector_wires: &[Vec<u8>]) -> Vec<u8> {
    match r.below(4) {
        0 => {
            let n = r.below(64);
            r.bytes(n)
        }
        1 => {
            // A truncated real frame (no closing flag).
            let w = &vector_wires[r.below(vector_wires.len())];
            w[..w.len() - 1 - r.below(w.len() - 1)].to_vec()
        }
        2 => (0..r.below(16))
            .map(|_| [hdlc::FLAG, hdlc::ESC, 0x5E, 0x5D][r.below(4)])
            .collect(),
        _ => b"[00:00:01.234] <err> radio: rearm -5\r\n".to_vec(),
    }
}

/// Split `wire` at random points and feed the decoder piece by piece.
fn feed_split(d: &mut Decoder, r: &mut Rng, wire: &[u8]) -> Vec<Frame> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < wire.len() {
        let n = 1 + r.below(wire.len() - i);
        out.extend(d.push(&wire[i..i + n]));
        i += n;
    }
    out
}

fn crc_ok(f: &Frame) -> bool {
    // A frame the decoder yields carries a CRC that matched; re-encoding it and decoding again
    // must give the same frame, and its length must be within the link's bounds.
    let w = hdlc::encode(f.frame_type, f.seq, &f.body);
    f.body.len() <= 280 && decode_all(&w).0 == vec![f.clone()]
}

/// The deframer fuzz (#470 C4): random, truncated, corrupted and byte-split streams. The decoder
/// never panics, never yields a frame whose CRC did not match, resyncs after garbage, and every
/// valid frame between two pieces of garbage is recovered.
#[test]
fn deframer_fuzz_resyncs_and_never_yields_a_bad_frame() {
    const ROUNDS: usize = 20_000;
    let wires: Vec<Vec<u8>> = vectors()
        .into_iter()
        .filter(|v| !matches!(v.msg.as_str(), "STREAM" | "REJECT"))
        .map(|v| v.wire)
        .collect();
    let mut r = Rng(0x0470_C4C4_7E7D_0001);
    let mut recovered = 0usize;
    for round in 0..ROUNDS {
        let mut d = Decoder::new();
        let mut stream = garbage(&mut r, &wires);
        let mut want = Vec::new();
        for _ in 0..1 + r.below(4) {
            let f = random_frame(&mut r);
            stream.extend(hdlc::encode(f.frame_type, f.seq, &f.body));
            want.push(f);
            if r.below(2) == 0 {
                stream.extend(garbage(&mut r, &wires));
                // Garbage ends inside a frame more often than not: the next frame's opening
                // flag is what resyncs, so a flag closes it before the next valid frame.
                stream.push(hdlc::FLAG);
            }
        }
        let got = feed_split(&mut d, &mut r, &stream);
        // Garbage may hold whole valid frames of its own (a vector wire, or a lucky CRC):
        // the wanted frames must appear in order, as a subsequence.
        let mut it = got.iter();
        for w in &want {
            assert!(
                it.any(|g| g == w),
                "round {round}: frame lost\nwant {w:?}\ngot {got:?}"
            );
            recovered += 1;
        }
        assert!(
            got.iter().all(crc_ok),
            "round {round}: a bad frame came out"
        );

        // Corruption: flip one bit or drop/insert one byte of a valid wire. Whatever comes out
        // still passed a CRC; a following valid frame is always recovered.
        let f = random_frame(&mut r);
        let mut w = hdlc::encode(f.frame_type, f.seq, &f.body);
        let at = 1 + r.below(w.len() - 2);
        match r.below(3) {
            0 => w[at] ^= 1 << r.below(8),
            1 => {
                w.remove(at);
            }
            _ => w.insert(at, r.next() as u8),
        }
        let next = random_frame(&mut r);
        w.extend(hdlc::encode(next.frame_type, next.seq, &next.body));
        let got = feed_split(&mut Decoder::new(), &mut r, &w);
        assert!(
            got.iter().all(crc_ok),
            "round {round}: corrupted frame came out bad"
        );
        assert_eq!(
            got.last(),
            Some(&next),
            "round {round}: no resync after corruption"
        );
    }
    assert!(recovered >= ROUNDS, "{recovered} frames recovered");

    // Pure noise never panics and anything it yields passed a CRC.
    let mut d = Decoder::new();
    for _ in 0..2_000 {
        let n = r.below(600);
        let chunk = r.bytes(n);
        for f in d.push(&chunk) {
            assert!(crc_ok(&f));
        }
    }
}
