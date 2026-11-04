//! The concentrator's own gateway EUI, asked of concentratord over its command socket
//! (plan/control/cluster.md, amendment 2026-10-28, #164).
//!
//! A Client Hub runs no ChirpStack and its forwarder publishes to the Server's broker, so the
//! retained `state/conn` read that `create` uses ([radio/lorawan.md] §6) finds nothing locally.
//! concentratord answers `gw.Command{get_gateway_id}` on its REP socket instead. This is one
//! ZMTP 3.0 REQ exchange with the NULL mechanism over a Unix socket, written out by hand: the
//! two messages are a few bytes each, and a ZeroMQ dependency for them is not worth carrying.
//!
//! Wire (verified against concentratord 4.7.1 on 2026-10-28):
//! - request: `gw.Command` with field 3 (`get_gateway_id`) an empty message → `1a 00`;
//! - reply: `GetGatewayIdResponse` with field 1 (`gateway_id`) a string → `0a 10 <16 hex>`.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// Where the shipped concentratord binds its command socket (`command_bind` in
/// `chirpstack-concentratord.toml`).
pub const COMMAND_SOCKET: &str = "/run/chirpstack-concentratord/command";

/// How long one exchange may take. concentratord answers from memory; a daemon that does not
/// answer within this is treated as absent.
pub const TIMEOUT: Duration = Duration::from_secs(2);

/// `gw.Command { get_gateway_id: GetGatewayIdRequest {} }`.
const GET_GATEWAY_ID: &[u8] = &[0x1a, 0x00];

/// The concentrator's gateway EUI, lowercase, from the socket at `path`. `Err` says why not:
/// no socket, no answer within `timeout`, or an answer that is not a gateway EUI.
pub fn gateway_id(path: &Path, timeout: Duration) -> Result<String, String> {
    let mut s = UnixStream::connect(path).map_err(|e| format!("{}: {e}", path.display()))?;
    s.set_read_timeout(Some(timeout)).map_err(io)?;
    s.set_write_timeout(Some(timeout)).map_err(io)?;

    // Greeting: signature, version 3.0, mechanism NULL, as-server 0, filler.
    let mut greeting = [0u8; 64];
    greeting[0] = 0xff;
    greeting[9] = 0x7f;
    greeting[10] = 3;
    greeting[12..16].copy_from_slice(b"NULL");
    s.write_all(&greeting).map_err(io)?;
    let mut peer = [0u8; 64];
    s.read_exact(&mut peer).map_err(io)?;
    if peer[0] != 0xff || peer[9] != 0x7f || peer[10] < 3 {
        return Err("not a ZMTP 3 peer".to_string());
    }

    // READY with Socket-Type REQ, then the peer's READY.
    let mut ready = b"\x05READY\x0bSocket-Type".to_vec();
    ready.extend_from_slice(&3u32.to_be_bytes());
    ready.extend_from_slice(b"REQ");
    s.write_all(&frame(&ready, false, true)).map_err(io)?;
    let (flags, _) = read_frame(&mut s)?;
    if flags & 0x04 == 0 {
        return Err("the peer did not answer READY".to_string());
    }

    // REQ envelope: an empty delimiter, then the command.
    let mut request = frame(&[], true, false);
    request.extend(frame(GET_GATEWAY_ID, false, false));
    s.write_all(&request).map_err(io)?;

    // The reply: delimiter frame(s), the last one is the body.
    let body = loop {
        let (flags, body) = read_frame(&mut s)?;
        if flags & 0x01 == 0 {
            break body;
        }
    };
    parse_reply(&body)
}

/// [`gateway_id`] against the shipped socket, `None` on any failure.
pub fn current_gateway_id() -> Option<String> {
    gateway_id(Path::new(COMMAND_SOCKET), TIMEOUT).ok()
}

fn io(e: std::io::Error) -> String {
    e.to_string()
}

fn frame(body: &[u8], more: bool, command: bool) -> Vec<u8> {
    let flags = u8::from(more) | if command { 0x04 } else { 0 };
    let mut out = Vec::with_capacity(body.len() + 9);
    if body.len() > 255 {
        out.push(flags | 0x02);
        out.extend_from_slice(&(body.len() as u64).to_be_bytes());
    } else {
        out.push(flags);
        out.push(body.len() as u8);
    }
    out.extend_from_slice(body);
    out
}

/// One frame: its flags and body. Bodies are capped well above anything concentratord sends.
fn read_frame(s: &mut impl Read) -> Result<(u8, Vec<u8>), String> {
    let mut b = [0u8; 1];
    s.read_exact(&mut b).map_err(io)?;
    let flags = b[0];
    let len = if flags & 0x02 != 0 {
        let mut l = [0u8; 8];
        s.read_exact(&mut l).map_err(io)?;
        u64::from_be_bytes(l)
    } else {
        s.read_exact(&mut b).map_err(io)?;
        u64::from(b[0])
    };
    if len > 4096 {
        return Err(format!("a {len}-byte frame is not a gateway ID reply"));
    }
    let mut body = vec![0u8; len as usize];
    s.read_exact(&mut body).map_err(io)?;
    Ok((flags, body))
}

/// `GetGatewayIdResponse.gateway_id` (field 1, string), validated as 16 hex digits.
fn parse_reply(body: &[u8]) -> Result<String, String> {
    match body {
        [0x0a, n, rest @ ..] if usize::from(*n) == rest.len() => {
            let id = std::str::from_utf8(rest).map_err(|_| "gateway_id is not UTF-8")?;
            if id.len() == 16 && id.bytes().all(|c| c.is_ascii_hexdigit()) {
                Ok(id.to_ascii_lowercase())
            } else {
                Err(format!("gateway_id {id:?} is not a gateway EUI"))
            }
        }
        _ => Err("not a GetGatewayIdResponse".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A REP peer that answers the first request with `reply`, and returns what it was sent.
    fn fake_concentratord(reply: Vec<u8>) -> (tempfile::TempDir, std::thread::JoinHandle<Vec<u8>>) {
        let dir = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(dir.path().join("command")).unwrap();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut greeting = [0u8; 64];
            s.read_exact(&mut greeting).unwrap();
            let mut ours = [0u8; 64];
            ours[0] = 0xff;
            ours[9] = 0x7f;
            ours[10] = 3;
            ours[11] = 1;
            ours[12..16].copy_from_slice(b"NULL");
            s.write_all(&ours).unwrap();
            let (flags, ready) = read_frame(&mut s).unwrap();
            assert_eq!(flags, 0x04);
            assert!(ready.ends_with(b"REQ"));
            let mut rep = b"\x05READY\x0bSocket-Type".to_vec();
            rep.extend_from_slice(&3u32.to_be_bytes());
            rep.extend_from_slice(b"REP");
            s.write_all(&frame(&rep, false, true)).unwrap();
            let (more, delimiter) = read_frame(&mut s).unwrap();
            assert_eq!((more, delimiter.len()), (0x01, 0));
            let (_, request) = read_frame(&mut s).unwrap();
            let mut answer = frame(&[], true, false);
            answer.extend(frame(&reply, false, false));
            s.write_all(&answer).unwrap();
            request
        });
        (dir, handle)
    }

    #[test]
    fn reads_the_gateway_id_as_concentratord_4_answers_it() {
        let mut reply = vec![0x0a, 0x10];
        reply.extend_from_slice(b"0016C001F136C66A");
        let (dir, peer) = fake_concentratord(reply);
        let eui = gateway_id(&dir.path().join("command"), TIMEOUT).unwrap();
        assert_eq!(eui, "0016c001f136c66a");
        assert_eq!(peer.join().unwrap(), GET_GATEWAY_ID);
    }

    #[test]
    fn a_reply_that_is_not_an_eui_is_refused() {
        let (dir, peer) = fake_concentratord(vec![0x0a, 0x03, b'a', b'b', b'c']);
        assert!(gateway_id(&dir.path().join("command"), TIMEOUT).is_err());
        peer.join().unwrap();
    }

    #[test]
    fn no_socket_is_an_error_not_a_hang() {
        let dir = tempfile::tempdir().unwrap();
        assert!(gateway_id(&dir.path().join("command"), TIMEOUT).is_err());
    }
}
