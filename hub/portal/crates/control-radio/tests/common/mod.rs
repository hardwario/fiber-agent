//! Wire helpers shared by the integration tests that drive both carriers.
//!
//! These are duplicated from `p2p::runtime`'s and `tasks::uplink`'s own `#[cfg(test)]` modules
//! rather than exported from the crate: `pub` under `cfg(test)` is not visible to an
//! integration test, and the alternative — a `#[doc(hidden)] pub mod testing` in the shipped
//! library — would put frame-forging helpers into the public API of a binary that ships on a
//! medical device. Everything below is built out of the crate's own *public* wire code
//! (`p2p::tower`, `decode::command`), so a change to the frame format still breaks these at
//! compile time rather than letting them drift into a second encoder.

use control_radio::lorawan::event::{self, UplinkEvent};
use control_radio::p2p::modem::{Event, RxEvent};
use control_radio::p2p::tower::control::{self, JoinReq};
use control_radio::p2p::tower::frame::{self, Header};
use control_radio::p2p::tower::link::{TwrUplink, UplinkAck};
use serde_json::Value;

/// The ChirpStack integration event the LoRaWAN arm is driven with.
const UP_EVENT: &str = include_str!("../fixtures/chirpstack-up-event.json");

pub fn hexbytes(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Minimal base64, so a fixture can be rewritten without a dependency the crate has
/// deliberately not taken.
pub fn base64(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The ChirpStack fixture with its `devEui`, `fPort` and `data` swapped for the frame under
/// test — the LoRaWAN half of "the same bytes down both carriers".
pub fn event(dev_eui: &str, f_port: u32, data: &[u8]) -> UplinkEvent {
    let mut v: Value = serde_json::from_str(UP_EVENT).expect("fixture");
    v["deviceInfo"]["devEui"] = Value::from(dev_eui);
    v["fPort"] = Value::from(f_port);
    v["data"] = Value::from(base64(data));
    event::parse_uplink(&serde_json::to_vec(&v).expect("ser")).expect("parses")
}

/// A JoinReq (`p2p_tower_gateway.md` §3.2): a TOWER data frame to `dest = 0`, sealed under
/// the Node's join key, counter = `dev_nonce`. `dev_eui` is MSB-first, as the Node puts it
/// on the air.
pub fn join_request(app_key: &[u8; 16], dev_eui: &[u8; 8], dev_nonce: u32) -> Vec<u8> {
    let body = JoinReq {
        product_type: 0x01,
        proto_version: 1,
        dev_eui: *dev_eui,
        fw_version: 0x0104_0000,
    }
    .encode();
    let payload = control::encode_control(&[(control::cmd::JOIN_REQ, &body)]).expect("encodes");
    let addr = u32::from_be_bytes([dev_eui[4], dev_eui[5], dev_eui[6], dev_eui[7]]);
    frame::seal(
        &frame::join_key(app_key, dev_eui),
        &Header {
            frame_type: frame::frame_type::DATA,
            flags: 0,
            src: addr,
            dest: 0,
            counter: dev_nonce,
        },
        &payload,
    )
    .expect("seals")
}

/// One application uplink as the gateway hands it to the central: verified, ACKed, plaintext
/// `0x81 ‖ port ‖ body`.
/// An `EVT_TWR_UPLINK` ring event under delivery `seq` (link doc §4.1).
pub fn uplink(seq: u8, addr: u32, counter: u32, t_ms: u64, port: u8, body: &[u8]) -> Event {
    let mut plaintext = vec![control::ENVELOPE_APP, port];
    plaintext.extend_from_slice(body);
    Event::TwrUplink {
        seq,
        up: TwrUplink {
            t_ms,
            rssi_dbm: -58,
            snr_q_db: 36,
            frame_flags: frame::flags::CONFIRMED,
            addr,
            counter,
            ack: UplinkAck::Sent,
            ack_tail: Default::default(),
            plaintext,
        },
    }
}

pub fn rx_event(frame_bytes: Vec<u8>, t_ms: u64) -> Event {
    Event::Rx(RxEvent {
        t_ms,
        rssi_dbm: -58,
        snr_q_db: 36,
        frame: frame_bytes,
    })
}

fn varint(mut n: u32, out: &mut Vec<u8>) {
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// A `Response{seq, <field> = inner}` exactly as a node emits it: the `APP_PROTO_VERSION` byte
/// (`app_cmd.h` prefixes every 0x55, and an fPort-85 uplink carries the same), then the
/// protobuf. `field` is the `ResponseBody` oneof number — 2 ack, 3 info, 4 config_dump,
/// 5 history_frame, 6 error.
pub fn response_with_seq(field: u8, inner: &[u8], seq: u32) -> Vec<u8> {
    let mut resp = vec![control_radio::decode::APP_PROTO_VERSION, 0x08];
    varint(seq, &mut resp);
    resp.push((field << 3) | 2);
    varint(inner.len() as u32, &mut resp);
    resp.extend_from_slice(inner);
    resp
}
