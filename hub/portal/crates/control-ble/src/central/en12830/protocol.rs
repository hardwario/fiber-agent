//! The EN12830 archive protocol — `plan/control/ble/beacon-btsmp1.md` §3.
//!
//! Reverse-engineered, unverifiable against any specification, and carrying **no version byte and no
//! stability commitment**. Everything here is a value-in / value-out function so the layouts can be
//! tested against §3's vectors without a beacon, which is the only external check available.

use crate::error::{Component, Error, Kind, Result, Role};

/// Recorder service opcodes.
pub const CMD_START_RECORD: u16 = 0x0001;
pub const CMD_STOP_RECORD: u16 = 0x0002;
pub const CMD_START_RECORD_SEND: u16 = 0x0004;
pub const CMD_SEND_NEXT_CHUNK: u16 = 0x0005;
pub const CMD_TIME_SYNC: u16 = 0x0006;
pub const CMD_START_RECORD_SEND_TS: u16 = 0x0007;

/// Response codes.
pub const RESP_OK: u8 = 0x00;
pub const RESP_NO_MORE_CHUNK: u8 = 0x08;
/// What an empty command read means: nothing answered at all.
pub const RESP_EMPTY: u8 = 0xFF;

/// 4 blocks × 15 slots.
pub const RECORDS_PER_CHUNK: u64 = 60;
const BLOCKS_PER_CHUNK: usize = 4;
const SLOTS_PER_BLOCK: usize = 15;
/// Each block is 15 little-endian i16 samples followed by a u16 CRC we do not check.
const BLOCK_BYTES: usize = SLOTS_PER_BLOCK * 2 + 2;

/// A hard bound on a download, so a beacon that never says NO_MORE_CHUNK cannot loop forever.
pub const MAX_CHUNKS: u16 = 2048;

/// Sentinels for an unwritten slot. Both are skipped — **but they still advance the position**, so
/// the timestamps of the samples after them stay correct.
const EMPTY_SLOT_FFFF: i16 = -1; // 0xFFFF read as i16
const EMPTY_SLOT_MIN: i16 = i16::MIN;

/// Milliseconds between chunk reads, from §3's download procedure.
pub const CHUNK_PACING_MS: u64 = 60;
/// The pause after STOP_RECORD, which finalises the active page.
pub const STOP_SETTLE_MS: u64 = 400;
/// The one retry of the re-START, and how long to wait before it.
pub const RESTART_RETRY_MS: u64 = 250;

fn protocol_err(message: impl Into<String>) -> Error {
    Error::new(Kind::Transport, Role::Central, Component::L2cap, message)
}

/// 10 bytes, all little-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct RecordInfo {
    pub is_recording: bool,
    pub interval_s: u16,
    pub number_of_records: u16,
    pub start_ts: u32,
}

/// Parse the **decrypted** record-info read.
pub fn parse_record_info(dec: &[u8]) -> Result<RecordInfo> {
    if dec.len() < 10 {
        return Err(protocol_err(format!(
            "Record Info shorter than 10 bytes ({} received): the recorder protocol is \
             reverse-engineered and a short read is a protocol failure, not an empty archive",
            dec.len()
        )));
    }
    Ok(RecordInfo {
        is_recording: u16::from_le_bytes([dec[0], dec[1]]) != 0,
        interval_s: u16::from_le_bytes([dec[2], dec[3]]),
        number_of_records: u16::from_le_bytes([dec[4], dec[5]]),
        start_ts: u32::from_le_bytes([dec[6], dec[7], dec[8], dec[9]]),
    })
}

/// The chunk that opens a recording session: 10 bytes, all little-endian.
///
/// The archive is a sequence of sessions, not one run: every START (and every full flash page, about
/// 945 samples) begins a new session with its own header chunk, followed by `ceil(count / 60)` data
/// chunks. Verified against a 177-chunk download of 37 sessions from `7C:D9:F4:10:6E:30` (2026-10-09).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionHeader {
    pub start_ts: u32,
    /// Use it when non-zero; otherwise the requested interval stands.
    pub interval_s: u16,
    /// A per-session word of unknown meaning (likely a check value); carried, not verified.
    pub check: u16,
    /// Samples in this session.
    pub count: u16,
}

impl SessionHeader {
    /// Data chunks that follow this header.
    pub fn data_chunks(&self) -> u16 {
        self.count.div_ceil(RECORDS_PER_CHUNK as u16)
    }
}

/// The earliest start a real session can carry (2020-01-01): a smaller value is not a header.
const MIN_PLAUSIBLE_TS: u32 = 1_577_836_800;
/// How far ahead of the Hub's clock a session may start and still be believed.
const FUTURE_SLACK_S: u32 = 86_400;

/// Parse the **decrypted** body of a session header chunk.
pub fn parse_session_header(dec: &[u8]) -> Result<SessionHeader> {
    if dec.len() < 10 {
        return Err(protocol_err(format!(
            "an archive session header is shorter than 10 bytes ({} received)",
            dec.len()
        )));
    }
    Ok(SessionHeader {
        start_ts: u32::from_le_bytes([dec[0], dec[1], dec[2], dec[3]]),
        interval_s: u16::from_le_bytes([dec[4], dec[5]]),
        check: u16::from_le_bytes([dec[6], dec[7]]),
        count: u16::from_le_bytes([dec[8], dec[9]]),
    })
}

#[derive(Debug, Clone, Copy)]
struct Session {
    header_idx: u16,
    header: SessionHeader,
    interval_s: u16,
}

/// Walks the chunk stream of one download and turns it into `(unix_ts, celsius)` pairs.
///
/// A chunk is a header when no session is open or the open one has had all its data chunks; any
/// other chunk is data of the open session. A sample's ordinal within its session comes from the
/// chunk index, so a dropped data chunk leaves a gap rather than shifting time. A chunk expected to
/// be a header that does not look like one is rejected, and the walk resynchronises on the next
/// chunk that does — a lost header loses its session, never re-labels it.
#[derive(Debug)]
pub struct ArchiveDecoder {
    session: Option<Session>,
    fallback_interval_s: u16,
    now: u32,
    /// Session headers accepted.
    pub sessions: u16,
    /// Chunks that were neither data of an open session nor a believable header.
    pub rejected_chunks: u16,
}

impl ArchiveDecoder {
    /// `fallback_interval_s` stands in for a header that reports 0; `now` bounds believable starts.
    pub fn new(fallback_interval_s: u16, now: u32) -> Self {
        ArchiveDecoder {
            session: None,
            fallback_interval_s,
            now,
            sessions: 0,
            rejected_chunks: 0,
        }
    }

    pub fn feed(&mut self, idx: u16, body: &[u8]) -> Vec<(i64, f64)> {
        if let Some(s) = self.session {
            let offset = idx.wrapping_sub(s.header_idx);
            if idx > s.header_idx && offset <= s.header.data_chunks() {
                return self.samples(s, offset - 1, body);
            }
        }
        match parse_session_header(body) {
            Ok(h) if self.plausible(&h) => {
                let interval_s = if h.interval_s != 0 {
                    h.interval_s
                } else {
                    self.fallback_interval_s
                };
                self.session = Some(Session {
                    header_idx: idx,
                    header: h,
                    interval_s,
                });
                self.sessions = self.sessions.saturating_add(1);
            }
            _ => {
                self.session = None;
                self.rejected_chunks = self.rejected_chunks.saturating_add(1);
            }
        }
        Vec::new()
    }

    fn plausible(&self, h: &SessionHeader) -> bool {
        let not_future = self.now == 0 || h.start_ts <= self.now.saturating_add(FUTURE_SLACK_S);
        h.start_ts >= MIN_PLAUSIBLE_TS
            && not_future
            && u64::from(h.count) <= u64::from(MAX_CHUNKS) * RECORDS_PER_CHUNK
    }

    /// The samples of data chunk `nth` (0-based) of session `s`.
    fn samples(&self, s: Session, nth: u16, body: &[u8]) -> Vec<(i64, f64)> {
        let mut out = Vec::new();
        let base = u64::from(nth) * RECORDS_PER_CHUNK;
        let mut pos: u64 = 0;
        for block in 0..BLOCKS_PER_CHUNK {
            let start = block * BLOCK_BYTES;
            for slot in 0..SLOTS_PER_BLOCK {
                let ordinal = base + pos;
                // Past the session's count the slots are filler (0xC80A seen), not samples.
                if ordinal >= u64::from(s.header.count) {
                    return out;
                }
                let at = start + slot * 2;
                // A short body simply ends the chunk: the beacon sent what it had.
                let Some(bytes) = body.get(at..at + 2) else {
                    return out;
                };
                let raw = i16::from_le_bytes([bytes[0], bytes[1]]);
                // Skipped, but `pos` still advances — that is what keeps the later timestamps right.
                if raw != EMPTY_SLOT_FFFF && raw != EMPTY_SLOT_MIN {
                    let ts =
                        i64::from(s.header.start_ts) + ordinal as i64 * i64::from(s.interval_s);
                    out.push((ts, f64::from(raw) / 100.0));
                }
                pos += 1;
            }
            // The block's trailing u16 CRC is deliberately not checked: §3 never pinned its
            // polynomial, and inventing one would reject good data.
        }
        out
    }
}

/// The intervals the beacon accepts, in minutes. `0` means stop and is handled by the caller.
pub const LOGGING_INTERVALS_MIN: [u16; 3] = [1, 5, 15];

/// Convert a logging interval in minutes to seconds.
///
/// A value outside the set is **refused, never snapped to a neighbour**: an operator who asked for a
/// five-minute archive and silently got fifteen has a compliance problem and no way to know.
pub fn interval_s(min: u16) -> Result<u16> {
    if !LOGGING_INTERVALS_MIN.contains(&min) {
        return Err(Error::new(
            Kind::InvalidArgument,
            Role::Central,
            Component::BeaconGatt,
            format!(
                "logging interval {min} minutes is not supported: supported: 1, 5, 15 minutes, or \
                 0 to stop"
            ),
        ));
    }
    Ok(min * 60)
}

/// Parse the response to an Exchange MTU Request. Anything unexpected or short means the default.
pub fn parse_mtu_response(pdu: &[u8]) -> usize {
    const ATT_DEFAULT_MTU: usize = 23;
    const OP_EXCHANGE_MTU_RSP: u8 = 0x03;
    match pdu {
        [OP_EXCHANGE_MTU_RSP, lo, hi, ..] => {
            let mtu = u16::from_le_bytes([*lo, *hi]) as usize;
            // A peer claiming an MTU below the ATT minimum is not to be believed.
            if mtu < ATT_DEFAULT_MTU {
                ATT_DEFAULT_MTU
            } else {
                mtu
            }
        }
        _ => ATT_DEFAULT_MTU,
    }
}

/// Build the plaintext of a command: `random ‖ opcode ‖ params`, all little-endian.
///
/// The `random` value is read from the beacon's own characteristic immediately before, and echoing
/// it back is what the firmware checks.
pub fn command_plaintext(random: u16, opcode: u16, params: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + params.len());
    v.extend_from_slice(&random.to_le_bytes());
    v.extend_from_slice(&opcode.to_le_bytes());
    v.extend_from_slice(params);
    v
}

/// START_RECORD's parameters: interval then the current time, both little-endian.
pub fn start_record_params(interval_s: u16, now_ts: u32) -> Vec<u8> {
    let mut v = interval_s.to_le_bytes().to_vec();
    v.extend_from_slice(&now_ts.to_le_bytes());
    v
}

/// TIME_SYNC / START_RECORD_SEND_TS take a single little-endian u32.
pub fn u32_params(v: u32) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_info_known_vector() {
        // §3: `01 00 3C 00 02 00 8E C6 43 6A` → recording, 60 s, 2 records, start 0x6A43C68E.
        let v = [0x01, 0x00, 0x3C, 0x00, 0x02, 0x00, 0x8E, 0xC6, 0x43, 0x6A];
        let info = parse_record_info(&v).expect("parses");
        assert!(info.is_recording);
        assert_eq!(info.interval_s, 60);
        assert_eq!(info.number_of_records, 2);
        assert_eq!(info.start_ts, 0x6A43_C68E);
    }

    #[test]
    fn record_info_rejects_short() {
        for len in 0..10 {
            let err = parse_record_info(&vec![0u8; len]).expect_err("must reject");
            assert_eq!(err.kind(), Kind::Transport);
            assert_eq!(err.component(), Component::L2cap);
        }
        // Extra trailing bytes are fine — the layout is a prefix.
        assert!(parse_record_info(&[0u8; 20]).is_ok());
    }

    #[test]
    fn record_info_is_recording_is_any_non_zero() {
        let mut v = [0u8; 10];
        assert!(!parse_record_info(&v).expect("parses").is_recording);
        v[1] = 0x01; // the high byte of the LE u16
        assert!(
            parse_record_info(&v).expect("parses").is_recording,
            "the field is a u16 != 0, not a byte flag"
        );
    }

    /// Build a chunk body from a list of raw i16 samples, padding the rest with the empty sentinel
    /// and inserting the per-block CRC bytes the parser skips.
    fn chunk_body(samples: &[i16]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut it = samples.iter().copied();
        for _ in 0..BLOCKS_PER_CHUNK {
            for _ in 0..SLOTS_PER_BLOCK {
                let raw = it.next().unwrap_or(EMPTY_SLOT_FFFF);
                out.extend_from_slice(&raw.to_le_bytes());
            }
            out.extend_from_slice(&[0x00, 0x00]); // the CRC we deliberately ignore
        }
        out
    }

    /// A session header chunk body.
    fn header_body(start_ts: u32, interval_s: u16, count: u16) -> Vec<u8> {
        let mut out = start_ts.to_le_bytes().to_vec();
        out.extend_from_slice(&interval_s.to_le_bytes());
        out.extend_from_slice(&0x5134u16.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.resize(4 * BLOCK_BYTES, 0xFF);
        out
    }

    /// Seen on hardware: past the session's count the slots hold this, not empty sentinels.
    const FILLER: i16 = 0xC80Au16 as i16;

    fn filled(samples: &[i16]) -> Vec<u8> {
        let mut v = samples.to_vec();
        v.resize(60, FILLER);
        chunk_body(&v)
    }

    const T0: u32 = 1_789_434_714; // 2026-09-15T01:11:54Z, a real session start
    const NOW: u32 = 1_791_546_103; // 2026-10-09T11:41:43Z

    #[test]
    fn header_layout_is_start_interval_check_count() {
        let h = parse_session_header(&header_body(T0, 300, 945)).expect("parses");
        assert_eq!(
            h,
            SessionHeader {
                start_ts: T0,
                interval_s: 300,
                check: 0x5134,
                count: 945
            }
        );
        assert_eq!(h.data_chunks(), 16);
        assert!(parse_session_header(&[0u8; 9]).is_err());
    }

    #[test]
    fn data_timestamps_and_values() {
        let mut d = ArchiveDecoder::new(60, NOW);
        assert!(d.feed(0, &header_body(T0, 300, 3)).is_empty());
        let t = i64::from(T0);
        assert_eq!(
            d.feed(1, &filled(&[2500, 2550, -1800])),
            vec![(t, 25.0), (t + 300, 25.5), (t + 600, -18.0)]
        );
    }

    #[test]
    fn a_second_session_restarts_time_from_its_own_header() {
        // The bug this replaces: every chunk after idx 0 was read as data on one clock, so the
        // second header became two "temperatures" (its timestamp halves, 273.xx °C) and all later
        // samples drifted into the future.
        let mut d = ArchiveDecoder::new(60, NOW);
        let t1 = T0 + 4_000;
        let mut rows = Vec::new();
        rows.extend(d.feed(0, &header_body(T0, 300, 2)));
        rows.extend(d.feed(1, &filled(&[2119, 2118])));
        rows.extend(d.feed(2, &header_body(t1, 900, 1)));
        rows.extend(d.feed(3, &filled(&[2201])));
        assert_eq!(
            rows,
            vec![
                (i64::from(T0), 21.19),
                (i64::from(T0) + 300, 21.18),
                (i64::from(t1), 22.01)
            ]
        );
        assert_eq!(d.sessions, 2);
        assert_eq!(d.rejected_chunks, 0);
    }

    #[test]
    fn filler_past_the_count_is_never_a_sample() {
        let mut d = ArchiveDecoder::new(60, NOW);
        d.feed(0, &header_body(T0, 300, 2));
        let rows = d.feed(1, &filled(&[2119, 2118]));
        assert_eq!(
            rows.len(),
            2,
            "0xC80A (−143.26 °C) after the count is filler"
        );
    }

    #[test]
    fn a_session_spans_several_chunks_and_keeps_its_ordinal() {
        let mut d = ArchiveDecoder::new(60, NOW);
        d.feed(10, &header_body(T0, 60, 61));
        assert_eq!(d.feed(11, &filled(&[100; 60])).len(), 60);
        assert_eq!(
            d.feed(12, &filled(&[200])),
            vec![(i64::from(T0) + 60 * 60, 2.0)]
        );
        // The session is complete, so the next chunk must be a header again.
        assert!(d.feed(13, &filled(&[300])).is_empty());
        assert_eq!(d.rejected_chunks, 1);
    }

    #[test]
    fn a_dropped_data_chunk_leaves_a_gap_instead_of_shifting_time() {
        let mut d = ArchiveDecoder::new(60, NOW);
        d.feed(0, &header_body(T0, 60, 120));
        // idx 1 lost on the air.
        let rows = d.feed(2, &chunk_body(&[500]));
        assert_eq!(rows, vec![(i64::from(T0) + 60 * 60, 5.0)]);
    }

    #[test]
    fn empty_slots_advance_position() {
        let mut d = ArchiveDecoder::new(60, NOW);
        d.feed(0, &header_body(T0, 10, 2));
        assert_eq!(
            d.feed(1, &filled(&[EMPTY_SLOT_FFFF, 2000])),
            vec![(i64::from(T0) + 10, 20.0)]
        );
        d.feed(2, &header_body(T0, 10, 2));
        assert_eq!(
            d.feed(3, &filled(&[EMPTY_SLOT_MIN, 2000])),
            vec![(i64::from(T0) + 10, 20.0)]
        );
    }

    #[test]
    fn a_sample_spanning_a_block_boundary_keeps_its_ordinal() {
        // Slot 15 is the first slot of the second block, and the block's CRC bytes sit between them.
        // Getting the block stride wrong shifts every later sample in time.
        let mut samples = vec![EMPTY_SLOT_FFFF; 15];
        samples.push(1_234);
        let mut d = ArchiveDecoder::new(60, NOW);
        d.feed(0, &header_body(T0, 100, 16));
        assert_eq!(
            d.feed(1, &chunk_body(&samples)),
            vec![(i64::from(T0) + 1_500, 12.34)],
            "ordinal 15 × 100 s"
        );
    }

    #[test]
    fn a_short_body_ends_the_chunk_instead_of_reading_past_it() {
        let mut d = ArchiveDecoder::new(60, NOW);
        d.feed(0, &header_body(T0, 60, 60));
        let body = chunk_body(&[100, 200])[..6].to_vec();
        assert_eq!(d.feed(1, &body).len(), 2);
    }

    #[test]
    fn an_implausible_header_is_rejected_and_the_walk_resynchronises() {
        let mut d = ArchiveDecoder::new(60, NOW);
        // A data chunk where a header was expected: its first word is a temperature, not a time.
        assert!(d.feed(0, &filled(&[2119, 2118, 2117])).is_empty());
        // A start far in the future is not believed either.
        assert!(d
            .feed(1, &header_body(NOW + 30 * 86_400, 300, 1))
            .is_empty());
        assert_eq!(d.rejected_chunks, 2);
        d.feed(2, &header_body(T0, 300, 1));
        assert_eq!(d.feed(3, &filled(&[2000])), vec![(i64::from(T0), 20.0)]);
    }

    #[test]
    fn a_zero_header_interval_falls_back_to_the_requested_one() {
        let mut d = ArchiveDecoder::new(900, NOW);
        d.feed(0, &header_body(T0, 0, 2));
        assert_eq!(d.feed(1, &filled(&[1, 2]))[1].0, i64::from(T0) + 900);
    }

    #[test]
    fn mtu_response_parsing_guards_short_pdus() {
        assert_eq!(parse_mtu_response(&[0x03, 0x00, 0x02]), 512);
        assert_eq!(parse_mtu_response(&[0x03]), 23);
        assert_eq!(parse_mtu_response(&[0x03, 0x00]), 23);
        assert_eq!(parse_mtu_response(&[]), 23);
        assert_eq!(
            parse_mtu_response(&[0x02, 0x00, 0x02]),
            23,
            "a request opcode is not a response"
        );
        // A peer claiming less than the ATT minimum is not believed.
        assert_eq!(parse_mtu_response(&[0x03, 0x10, 0x00]), 23);
        // Trailing bytes are tolerated.
        assert_eq!(parse_mtu_response(&[0x03, 0x05, 0x02, 0xFF]), 517);
    }

    #[test]
    fn logging_interval_set_is_1_5_15() {
        assert_eq!(interval_s(1).expect("1 min"), 60);
        assert_eq!(interval_s(5).expect("5 min"), 300);
        assert_eq!(interval_s(15).expect("15 min"), 900);
        for bad in [2u16, 3, 7, 10, 20, 30, 60] {
            let err = interval_s(bad).expect_err("must refuse");
            assert_eq!(err.kind(), Kind::InvalidArgument);
            assert!(
                err.message().contains("1, 5, 15"),
                "the message names the set: {}",
                err.message()
            );
        }
        // 0 is the caller's business (it means "stop"), so it is not in the set either.
        assert!(interval_s(0).is_err());
    }

    #[test]
    fn a_command_plaintext_is_random_then_opcode_then_params() {
        let p = command_plaintext(
            0xBEEF,
            CMD_START_RECORD,
            &start_record_params(300, 1_000_000),
        );
        assert_eq!(
            p,
            vec![
                0xEF, 0xBE, // random, LE
                0x01, 0x00, // opcode 0x0001, LE
                0x2C, 0x01, // interval 300, LE
                0x40, 0x42, 0x0F, 0x00, // now 1_000_000, LE
            ]
        );
        assert_eq!(u32_params(1).len(), 4);
    }

    #[test]
    fn the_opcodes_are_the_documented_ones() {
        assert_eq!(CMD_START_RECORD, 0x0001);
        assert_eq!(CMD_STOP_RECORD, 0x0002);
        assert_eq!(CMD_START_RECORD_SEND, 0x0004);
        assert_eq!(CMD_SEND_NEXT_CHUNK, 0x0005);
        assert_eq!(CMD_TIME_SYNC, 0x0006);
        assert_eq!(CMD_START_RECORD_SEND_TS, 0x0007);
        assert_eq!(RESP_OK, 0x00);
        assert_eq!(RESP_NO_MORE_CHUNK, 0x08);
        // 0x0003 is deliberately absent: nothing in §3 assigns it.
        assert_ne!(CMD_START_RECORD_SEND, 0x0003);
    }
}
