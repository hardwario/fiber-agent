//! EN12830 temperature-recorder client for white Teltonika EYE tags.
//!
//! The recorder GATT service (`e61c00xx-7df8-…`) exposes a tamper-evident,
//! encrypted temperature archive. BlueZ caps the ATT MTU at 23, which truncates
//! the Record Data reads to a single byte, so this module talks to the ATT layer
//! directly over a **raw L2CAP socket** (CID 0x0004) and negotiates a large MTU —
//! the same trick the vendor Android app uses. Ported from the standalone
//! `eye_en12830_dl` tool (`src/raw.rs`), verified live against real tags.
//!
//! The commands are protected by a challenge-response using a vendor CustomXTEA
//! (reverse-engineered and verified): read the plaintext Random value, build
//! `[random_le | cmd_le | params]`, XTEA-encrypt, write to the Command
//! characteristic, read the 1-byte response. Reading Record Info / Record Data
//! additionally requires a plaintext PIN unlock ("123456") written to the config
//! service's password characteristic.
//!
//! All calls are **blocking** (raw libc socket). Callers on an async runtime must
//! wrap them in `spawn_blocking`. They also require exclusive use of the HCI
//! adapter for the duration of the connection (see the monitor: it pauses the
//! bluer scan around a download).

use std::io;
use std::mem;
use std::time::Duration;

const AF_BLUETOOTH: libc::c_int = 31;
const BTPROTO_L2CAP: libc::c_int = 0;
const ATT_CID: u16 = 4;
const BDADDR_LE_PUBLIC: u8 = 1;
const BDADDR_LE_RANDOM: u8 = 2;

/// PIN that unlocks reads of the recorder/SN characteristics (plaintext).
const UNLOCK_PIN: &[u8] = b"123456";

// Recorder protocol commands (u16, little-endian on the wire inside the XTEA blob).
const CMD_START_RECORD: u16 = 0x0001;
const CMD_STOP_RECORD: u16 = 0x0002;
const CMD_START_RECORD_SEND: u16 = 0x0004;
const CMD_SEND_NEXT_CHUNK: u16 = 0x0005;
const CMD_TIME_SYNC: u16 = 0x0006;
const CMD_START_RECORD_SEND_TS: u16 = 0x0007;

const RESP_OK: u8 = 0x00;
const RESP_NO_MORE_CHUNK: u8 = 0x08;

/// One data chunk holds 4 blocks × 15 records = 60 temperature samples.
const RECORDS_PER_CHUNK: u64 = 60;

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct SockaddrL2 {
    l2_family: u16,
    l2_psm: u16,
    l2_bdaddr: [u8; 6],
    l2_cid: u16,
    l2_bdaddr_type: u8,
}

// --- CustomXTEA (reverse-engineered from the vendor app, verified) ---
const KEYS: [i64; 4] = [24, 113, 130, 185];
const DELTA: i64 = 105;
const TIMES: i64 = 5;

fn encrypt(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < data.len() {
        let (mut v0, mut v1) = (data[i] as i64, data[i + 1] as i64);
        let mut s = 0i64;
        for _ in 0..TIMES {
            v0 = (v0 + ((((v1 << 4) ^ (v1 >> 5)) + v1) ^ (s + KEYS[(s & 3) as usize]))) & 0xFF;
            s += DELTA;
            v1 = (v1 + ((((v0 << 4) ^ (v0 >> 5)) + v0) ^ (s + KEYS[((s >> 6) & 3) as usize]))) & 0xFF;
        }
        out.push(v0 as u8);
        out.push(v1 as u8);
        i += 2;
    }
    out
}

fn decrypt(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < data.len() {
        let (mut v0, mut v1) = (data[i] as i64, data[i + 1] as i64);
        let mut s = DELTA * TIMES;
        for _ in 0..TIMES {
            v1 = (v1 - ((((v0 << 4) ^ (v0 >> 5)) + v0) ^ (s + KEYS[((s >> 6) & 3) as usize]))) & 0xFF;
            s -= DELTA;
            v0 = (v0 - ((((v1 << 4) ^ (v1 >> 5)) + v1) ^ (s + KEYS[(s & 3) as usize]))) & 0xFF;
        }
        out.push(v0 as u8);
        out.push(v1 as u8);
        i += 2;
    }
    out
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

// --- raw ATT socket ---

struct Att {
    fd: libc::c_int,
}

impl Att {
    fn send(&self, pdu: &[u8]) -> io::Result<()> {
        let n = unsafe { libc::send(self.fd, pdu.as_ptr() as *const _, pdu.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn recv(&self) -> io::Result<Vec<u8>> {
        let mut buf = [0u8; 1024];
        let n = unsafe { libc::recv(self.fd, buf.as_mut_ptr() as *mut _, buf.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(buf[..n as usize].to_vec())
    }

    fn req(&self, pdu: &[u8]) -> io::Result<Vec<u8>> {
        self.send(pdu)?;
        self.recv()
    }

    fn write_req(&self, handle: u16, value: &[u8]) -> io::Result<Vec<u8>> {
        let mut p = vec![0x12u8, (handle & 0xff) as u8, (handle >> 8) as u8];
        p.extend_from_slice(value);
        self.req(&p)
    }

    fn read_req(&self, handle: u16) -> io::Result<Vec<u8>> {
        let r = self.req(&[0x0a, (handle & 0xff) as u8, (handle >> 8) as u8])?;
        if r.first() == Some(&0x0b) {
            Ok(r[1..].to_vec())
        } else {
            Ok(r) // includes ATT error PDU for the caller to inspect
        }
    }

    /// Read By Type (0x08) over `[start, end]` for characteristic declarations
    /// (UUID 0x2803). Returns `(decl_handle, value_handle, char_uuid_le_bytes)`.
    fn read_by_type_chars(&self, mut start: u16, end: u16) -> io::Result<Vec<(u16, u16, Vec<u8>)>> {
        let mut out = Vec::new();
        loop {
            let p = [
                0x08,
                (start & 0xff) as u8,
                (start >> 8) as u8,
                (end & 0xff) as u8,
                (end >> 8) as u8,
                0x03,
                0x28,
            ];
            let r = self.req(&p)?;
            if r.first() != Some(&0x09) {
                break; // 0x01 error => done
            }
            let len = r[1] as usize;
            if len < 5 {
                break;
            }
            let mut i = 2;
            let mut last = start;
            while i + len <= r.len() {
                let rec = &r[i..i + len];
                let decl = u16::from_le_bytes([rec[0], rec[1]]);
                let vhandle = u16::from_le_bytes([rec[3], rec[4]]);
                let uuid = rec[5..].to_vec();
                out.push((decl, vhandle, uuid));
                last = decl;
                i += len;
            }
            if last >= end {
                break;
            }
            start = last + 1;
        }
        Ok(out)
    }

    /// Read a possibly-long value: Read Request then Read Blob at increasing offsets.
    fn read_long(&self, handle: u16, mtu: usize) -> io::Result<Vec<u8>> {
        let first = self.req(&[0x0a, (handle & 0xff) as u8, (handle >> 8) as u8])?;
        if first.first() != Some(&0x0b) {
            return Ok(first);
        }
        let mut val = first[1..].to_vec();
        while val.len() >= mtu - 1 {
            let off = val.len() as u16;
            let blob = self.req(&[
                0x0c,
                (handle & 0xff) as u8,
                (handle >> 8) as u8,
                (off & 0xff) as u8,
                (off >> 8) as u8,
            ])?;
            if blob.first() != Some(&0x0b) || blob.len() <= 1 {
                break;
            }
            val.extend_from_slice(&blob[1..]);
        }
        Ok(val)
    }
}

impl Drop for Att {
    fn drop(&mut self) {
        // Always release the L2CAP socket, including on error paths.
        unsafe {
            libc::close(self.fd);
        }
    }
}

fn parse_mac(s: &str) -> [u8; 6] {
    let mut b = [0u8; 6];
    for (i, p) in s.split(':').enumerate().take(6) {
        b[5 - i] = u8::from_str_radix(p, 16).unwrap_or(0); // little-endian in sockaddr
    }
    b
}

fn connect_att(mac: &str, addr_type: u8) -> io::Result<Att> {
    let fd = unsafe { libc::socket(AF_BLUETOOTH, libc::SOCK_SEQPACKET, BTPROTO_L2CAP) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let att = Att { fd }; // owns fd now → Drop closes it on any early return
    let src = SockaddrL2 {
        l2_family: AF_BLUETOOTH as u16,
        l2_psm: 0,
        l2_bdaddr: [0; 6],
        l2_cid: ATT_CID,
        l2_bdaddr_type: BDADDR_LE_PUBLIC,
    };
    let r = unsafe {
        libc::bind(
            fd,
            &src as *const _ as *const libc::sockaddr,
            mem::size_of::<SockaddrL2>() as u32,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    let dst = SockaddrL2 {
        l2_family: AF_BLUETOOTH as u16,
        l2_psm: 0,
        l2_bdaddr: parse_mac(mac),
        l2_cid: ATT_CID,
        l2_bdaddr_type: addr_type,
    };
    let r = unsafe {
        libc::connect(
            fd,
            &dst as *const _ as *const libc::sockaddr,
            mem::size_of::<SockaddrL2>() as u32,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(att)
}

// --- characteristic discovery ---

/// Value handles for the recorder flow, discovered dynamically (the ATT layout
/// differs between tag models, so hard-coded handles are not safe).
struct RecorderHandles {
    password: u16,    // e61c0008-7df2 (config service) — plaintext PIN unlock
    record_info: u16, // e61c0001-7df8
    random: u16,      // e61c0002-7df8
    record_data: u16, // e61c0003-7df8
    command: u16,     // e61c0004-7df8
}

/// True for an `e61c00xx-<svc>-4d4e-8e6d-c611745b92e9` UUID (little-endian bytes)
/// with the given `xx` suffix and service discriminator byte (`0xf8` recorder,
/// `0xf2` config).
fn is_e61c(uuid: &[u8], suffix: u8, svc: u8) -> bool {
    uuid.len() == 16
        && uuid[15] == 0xe6
        && uuid[14] == 0x1c
        && uuid[13] == 0x00
        && uuid[12] == suffix
        && uuid[11] == 0x7d
        && uuid[10] == svc
}

fn discover(att: &Att) -> io::Result<RecorderHandles> {
    let chars = att.read_by_type_chars(0x0001, 0xffff)?;
    let find = |suffix: u8, svc: u8| -> Option<u16> {
        chars
            .iter()
            .find(|(_, _, u)| is_e61c(u, suffix, svc))
            .map(|t| t.1)
    };
    let missing = |what: &str| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("EN12830 recorder characteristic not found: {what}"),
        )
    };
    Ok(RecorderHandles {
        password: find(0x08, 0xf2).ok_or_else(|| missing("password e61c0008-7df2"))?,
        record_info: find(0x01, 0xf8).ok_or_else(|| missing("record_info e61c0001-7df8"))?,
        random: find(0x02, 0xf8).ok_or_else(|| missing("random e61c0002-7df8"))?,
        record_data: find(0x03, 0xf8).ok_or_else(|| missing("record_data e61c0003-7df8"))?,
        command: find(0x04, 0xf8).ok_or_else(|| missing("command e61c0004-7df8"))?,
    })
}

/// Decoded Record Info metadata (10-byte layout on the current tag firmware).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordInfo {
    pub is_recording: bool,
    pub interval_s: u16,
    pub number_of_records: u16,
    pub start_ts: u32,
}

fn parse_record_info(dec: &[u8]) -> io::Result<RecordInfo> {
    if dec.len() < 10 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Record Info shorter than 10 bytes",
        ));
    }
    Ok(RecordInfo {
        is_recording: le16(dec, 0) != 0,
        interval_s: le16(dec, 2),
        number_of_records: le16(dec, 4),
        start_ts: le32(dec, 6),
    })
}

/// Parse one decrypted data-chunk body into `(ts, temp_c)` records.
///
/// `chunk_index` is the plaintext page index (1-based for data chunks). The
/// absolute ordinal of a slot is `(chunk_index-1)*60 + slot_pos`, so its
/// timestamp is `start_ts + ordinal*interval`. Empty slots (`0xFFFF` /
/// `i16::MIN`) are trailing padding and are skipped, but still advance the slot
/// position so filled records keep their correct time.
fn parse_data_chunk(chunk_index: u16, body: &[u8], start_ts: u32, interval_s: u16) -> Vec<(i64, f32)> {
    let mut out = Vec::new();
    let base = (chunk_index.saturating_sub(1)) as u64 * RECORDS_PER_CHUNK;
    let mut off = 0usize;
    let mut pos: u64 = 0;
    for _ in 0..4 {
        for _ in 0..15 {
            if off + 2 > body.len() {
                return out;
            }
            let raw = le16(body, off);
            off += 2;
            let t = raw as i16;
            if raw != 0xffff && t != i16::MIN {
                let ordinal = base + pos;
                let ts = start_ts as i64 + ordinal as i64 * interval_s as i64;
                out.push((ts, t as f32 / 100.0));
            }
            pos += 1;
        }
        off += 2; // skip per-block CRC
    }
    out
}

// --- connected recorder session ---

struct Recorder {
    att: Att,
    handles: RecorderHandles,
    mtu: usize,
}

impl Recorder {
    /// Connect, negotiate MTU, discover handles, and unlock reads with the PIN.
    fn connect(mac: &str) -> io::Result<Recorder> {
        let att = match connect_att(mac, BDADDR_LE_PUBLIC) {
            Ok(a) => a,
            Err(_) => connect_att(mac, BDADDR_LE_RANDOM)?,
        };
        // Exchange MTU (request 517 like the vendor app; tag replies with its max).
        let r = att.req(&[0x02, 0x05, 0x02])?;
        let mtu = if r.first() == Some(&0x03) {
            le16(&r, 1) as usize
        } else {
            23
        };
        let handles = discover(&att)?;
        // Unlock recorder/SN reads with the plaintext PIN (Write Request).
        att.write_req(handles.password, UNLOCK_PIN)?;
        Ok(Recorder { att, handles, mtu })
    }

    /// Build `[random_le | cmd_le | params]`, XTEA-encrypt, write to Command, and
    /// return the 1-byte response code.
    fn send_cmd(&self, command: u16, params: &[u8]) -> io::Result<u8> {
        let rnd = self.att.read_req(self.handles.random)?;
        let r = if rnd.len() >= 2 { le16(&rnd, 0) } else { 0 };
        let mut plain = r.to_le_bytes().to_vec();
        plain.extend_from_slice(&command.to_le_bytes());
        plain.extend_from_slice(params);
        let enc = encrypt(&plain);
        self.att.write_req(self.handles.command, &enc)?;
        let resp = self.att.read_req(self.handles.command)?;
        Ok(*resp.first().unwrap_or(&0xff))
    }

    fn read_record_info(&self) -> io::Result<RecordInfo> {
        let ri = self.att.read_long(self.handles.record_info, self.mtu)?;
        parse_record_info(&decrypt(&ri))
    }

    /// Download temperature records with a timestamp `>= since_ts`.
    ///
    /// Stops recording (to finalize the active page), requests the archive from
    /// `since_ts` (partial via `START_RECORD_SEND_TS`, falling back to a full
    /// send if unsupported), walks the chunks, and finally restarts a clean
    /// recording with the FIBER's clock so the tag keeps logging.
    fn download_since(&self, since_ts: u32, interval_s: u16, now_ts: u32) -> io::Result<Vec<(i64, f32)>> {
        // Finalize the active page so its data chunks become readable.
        self.send_cmd(CMD_STOP_RECORD, &[])?;
        std::thread::sleep(Duration::from_millis(400));

        // Prefer a partial send from `since_ts`; fall back to a full send.
        let req_ts = since_ts.max(1);
        let mut resp = self.send_cmd(CMD_START_RECORD_SEND_TS, &req_ts.to_le_bytes())?;
        if resp != RESP_OK {
            resp = self.send_cmd(CMD_START_RECORD_SEND, &[])?;
        }
        let mut header: Option<(u32, u16)> = None; // (start_ts, interval)
        let mut out: Vec<(i64, f32)> = Vec::new();
        if resp == RESP_OK {
            for chunk_no in 0..2048u32 {
                if chunk_no > 0 {
                    let r = self.send_cmd(CMD_SEND_NEXT_CHUNK, &[])?;
                    if r == RESP_NO_MORE_CHUNK || r != RESP_OK {
                        break;
                    }
                }
                let raw = self.att.read_long(self.handles.record_data, self.mtu)?;
                if raw.len() < 2 {
                    break;
                }
                let idx = le16(&raw, 0);
                let body = decrypt(&raw[2..]);
                if idx == 0 {
                    if body.len() >= 6 {
                        header = Some((le32(&body, 0), le16(&body, 4)));
                    }
                } else if let Some((start_ts, hdr_interval)) = header {
                    let step = if hdr_interval > 0 { hdr_interval } else { interval_s };
                    out.extend(parse_data_chunk(idx, &body, start_ts, step));
                }
                std::thread::sleep(Duration::from_millis(60));
            }
        }

        // Restart a clean recording with the correct clock.
        let _ = self.send_cmd(CMD_TIME_SYNC, &now_ts.to_le_bytes());
        let mut p = interval_s.to_le_bytes().to_vec();
        p.extend_from_slice(&now_ts.to_le_bytes());
        let _ = self.send_cmd(CMD_START_RECORD, &p);

        // Client-side guard against page-alignment overshoot (device returns
        // whole pages, so a few records before `since_ts` may come back).
        out.retain(|(ts, _)| *ts >= since_ts as i64);
        Ok(out)
    }
}

// --- public API (blocking; wrap in spawn_blocking on an async runtime) ---

/// Enable temperature recording on the tag: sync its clock to `now_ts` and start
/// a recording at `interval_s`.
pub fn enable_recording(mac: &str, interval_s: u16, now_ts: u32) -> io::Result<()> {
    let rec = Recorder::connect(mac)?;
    rec.send_cmd(CMD_TIME_SYNC, &now_ts.to_le_bytes())?;
    let mut p = interval_s.to_le_bytes().to_vec();
    p.extend_from_slice(&now_ts.to_le_bytes());
    let resp = rec.send_cmd(CMD_START_RECORD, &p)?;
    if resp != RESP_OK {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("START_RECORD rejected: resp=0x{resp:02x}"),
        ));
    }
    Ok(())
}

/// Read the tag's current Record Info (recording state, interval, count, start).
pub fn read_record_info(mac: &str) -> io::Result<RecordInfo> {
    Recorder::connect(mac)?.read_record_info()
}

/// Download archived temperature records with `ts >= since_ts`, then restart a
/// clean recording at `interval_s` synced to `now_ts`. Returns `(unix_ts, °C)`.
pub fn download_since(
    mac: &str,
    since_ts: i64,
    interval_s: u16,
    now_ts: u32,
) -> io::Result<Vec<(i64, f32)>> {
    let since = since_ts.max(0) as u32;
    Recorder::connect(mac)?.download_since(since, interval_s, now_ts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xtea_roundtrip() {
        for sample in [
            vec![0x00u8, 0x01, 0x02, 0x03, 0x04, 0x05],
            vec![0xff, 0xfe, 0x10, 0x27, 0x2a, 0x00, 0x99, 0x42],
            (0u8..40).collect::<Vec<_>>(),
        ] {
            assert_eq!(decrypt(&encrypt(&sample)), sample, "roundtrip {sample:?}");
        }
    }

    #[test]
    fn xtea_operates_on_2byte_blocks_dropping_odd_tail() {
        // Odd-length input drops the trailing byte (2-byte block cipher).
        let enc = encrypt(&[1, 2, 3]);
        assert_eq!(enc.len(), 2);
    }

    #[test]
    fn parse_record_info_known_layout() {
        // is_recording=1, interval=60s, number_of_records=2, start_ts=0x6a43c68e
        let dec = [0x01, 0x00, 0x3c, 0x00, 0x02, 0x00, 0x8e, 0xc6, 0x43, 0x6a];
        let ri = parse_record_info(&dec).unwrap();
        assert!(ri.is_recording);
        assert_eq!(ri.interval_s, 60);
        assert_eq!(ri.number_of_records, 2);
        assert_eq!(ri.start_ts, 0x6a43c68e);
    }

    #[test]
    fn parse_record_info_rejects_short() {
        assert!(parse_record_info(&[0x01, 0x00]).is_err());
    }

    fn chunk_body(temps: &[i16]) -> Vec<u8> {
        // 4 blocks × (15 × int16 + u16 CRC). Fill from `temps`, pad with 0xFFFF.
        let mut body = Vec::new();
        let mut it = temps.iter();
        for _ in 0..4 {
            for _ in 0..15 {
                let v = it.next().copied().unwrap_or(-1); // 0xFFFF as i16 == -1 == empty
                body.extend_from_slice(&(v as u16).to_le_bytes());
            }
            body.extend_from_slice(&[0x00, 0x00]); // CRC placeholder
        }
        body
    }

    #[test]
    fn parse_data_chunk_timestamps_and_values() {
        let start_ts = 1_000_000u32;
        let interval = 300u16; // 5 min
                               // chunk_index=1 → ordinals 0,1,2; temps 25.00, 25.50, -18.00 °C
        let body = chunk_body(&[2500, 2550, -1800]);
        let recs = parse_data_chunk(1, &body, start_ts, interval);
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0], (1_000_000, 25.00));
        assert_eq!(recs[1], (1_000_300, 25.50));
        assert_eq!(recs[2], (1_000_600, -18.00));
    }

    #[test]
    fn parse_data_chunk_uses_absolute_ordinal_from_index() {
        let start_ts = 0u32;
        let interval = 60u16;
        // chunk_index=3 → first slot ordinal = (3-1)*60 = 120 → ts = 120*60 = 7200
        let body = chunk_body(&[100]); // 1.00 °C
        let recs = parse_data_chunk(3, &body, start_ts, interval);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0], (7200, 1.00));
    }

    #[test]
    fn parse_data_chunk_skips_empty_but_advances_position() {
        let start_ts = 0u32;
        let interval = 10u16;
        // slot0 empty (0xFFFF), slot1 = 2000 (20°C) → ts must be ordinal 1 = 10
        let body = chunk_body(&[-1, 2000]);
        let recs = parse_data_chunk(1, &body, start_ts, interval);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0], (10, 20.00));
    }
}
