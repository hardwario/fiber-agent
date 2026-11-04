//! The EN12830 archive layouts against `plan/control/ble/beacon-btsmp1.md` §3's vectors.
//!
//! §3 is the **reverse-engineered** layer: unverifiable against any specification, carrying no
//! version byte and no stability commitment. These vectors are therefore the only external check
//! that exists, which is why they are asserted at the public API rather than only in-module.

use control_ble::central::en12830::protocol::{
    interval_s, parse_mtu_response, parse_record_info, parse_session_header, ArchiveDecoder,
    LOGGING_INTERVALS_MIN,
};
use control_ble::central::en12830::xtea;
use control_ble::Kind;

const BLOCKS: usize = 4;
const SLOTS: usize = 15;
const EMPTY: i16 = -1;

/// Build a chunk body: 4 blocks of 15 little-endian i16 samples, each block followed by the u16 CRC
/// the parser skips.
fn chunk_body(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut it = samples.iter().copied();
    for _ in 0..BLOCKS {
        for _ in 0..SLOTS {
            out.extend_from_slice(&it.next().unwrap_or(EMPTY).to_le_bytes());
        }
        out.extend_from_slice(&[0x00, 0x00]);
    }
    out
}

#[test]
fn xtea_round_trips_even_length_inputs() {
    for sample in [
        vec![0x01, 0x00, 0x3C, 0x00, 0x02, 0x00, 0x8E, 0xC6, 0x43, 0x6A],
        (0u8..40).collect::<Vec<u8>>(),
        vec![0xFF; 16],
    ] {
        assert_eq!(
            xtea::decrypt(&xtea::encrypt(&sample)),
            sample,
            "decrypt(encrypt(x)) == x is §3's stated property"
        );
        assert_ne!(
            xtea::encrypt(&sample),
            sample,
            "and the transform is not a no-op"
        );
    }
}

#[test]
fn xtea_drops_an_odd_tail() {
    // §3: "an odd trailing byte is dropped" — not padded, not passed through.
    assert_eq!(xtea::encrypt(&[0x01, 0x02, 0x03]).len(), 2);
    assert_eq!(
        xtea::decrypt(&xtea::encrypt(&[0x01, 0x02, 0x03])),
        vec![0x01, 0x02]
    );
    assert!(xtea::encrypt(&[0x01]).is_empty());
}

#[test]
fn record_info_known_vector() {
    // §3: `01 00 3C 00 02 00 8E C6 43 6A` → recording, 60 s, 2 records, start 0x6A43C68E.
    let info = parse_record_info(&[0x01, 0x00, 0x3C, 0x00, 0x02, 0x00, 0x8E, 0xC6, 0x43, 0x6A])
        .expect("parses");
    assert!(info.is_recording);
    assert_eq!(info.interval_s, 60);
    assert_eq!(info.number_of_records, 2);
    assert_eq!(info.start_ts, 0x6A43_C68E);
}

#[test]
fn the_record_info_vector_survives_the_cipher_it_arrives_under() {
    // On the wire this read is encrypted, so the vector has to hold through the round trip too —
    // the layout and the cipher are only correct *together*.
    let plain = [0x01, 0x00, 0x3C, 0x00, 0x02, 0x00, 0x8E, 0xC6, 0x43, 0x6A];
    let info = parse_record_info(&xtea::decrypt(&xtea::encrypt(&plain))).expect("parses");
    assert_eq!(info.interval_s, 60);
    assert_eq!(info.start_ts, 0x6A43_C68E);
}

#[test]
fn record_info_rejects_short() {
    for len in 0..10 {
        let err = parse_record_info(&vec![0u8; len]).expect_err("must reject");
        assert_eq!(err.kind(), Kind::Transport, "{len} bytes");
    }
}

/// A real session start (2026-09-15T01:11:54Z) and a download time after it.
const T0: u32 = 1_789_434_714;
const NOW: u32 = 1_791_546_103;

/// A session header chunk: start, interval, an unverified check word, sample count.
fn header_body(start_ts: u32, interval_s: u16, count: u16) -> Vec<u8> {
    let mut out = start_ts.to_le_bytes().to_vec();
    out.extend_from_slice(&interval_s.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.resize(BLOCKS * (SLOTS * 2 + 2), 0xFF);
    out
}

/// Decode one session: its header at idx 0, then `chunks` as idx 1, 2, ….
fn one_session(interval: u16, count: u16, chunks: &[Vec<u8>]) -> Vec<(i64, f64)> {
    let mut d = ArchiveDecoder::new(60, NOW);
    assert!(d.feed(0, &header_body(T0, interval, count)).is_empty());
    let mut rows = Vec::new();
    for (i, c) in chunks.iter().enumerate() {
        rows.extend(d.feed(i as u16 + 1, c));
    }
    rows
}

#[test]
fn session_header_layout() {
    let h = parse_session_header(&header_body(T0, 300, 945)).expect("parses");
    assert_eq!((h.start_ts, h.interval_s, h.count), (T0, 300, 945));
    assert_eq!(h.data_chunks(), 16);
}

#[test]
fn data_chunk_timestamps_and_values() {
    let t = i64::from(T0);
    assert_eq!(
        one_session(300, 3, &[chunk_body(&[2500, 2550, -1800])]),
        vec![(t, 25.0), (t + 300, 25.5), (t + 600, -18.0)]
    );
}

#[test]
fn a_later_data_chunk_continues_the_session_ordinal() {
    // The second data chunk of a session starts at ordinal 60.
    let rows = one_session(60, 61, &[chunk_body(&[EMPTY; 60]), chunk_body(&[100])]);
    assert_eq!(rows, vec![(i64::from(T0) + 3_600, 1.0)]);
}

#[test]
fn empty_slots_advance_position() {
    // −1 then 2000 at interval 10 → the sample is at ordinal 1, so it lands 10 s after the start.
    assert_eq!(
        one_session(10, 2, &[chunk_body(&[EMPTY, 2000])]),
        vec![(i64::from(T0) + 10, 20.0)]
    );
    assert_eq!(
        one_session(10, 2, &[chunk_body(&[i16::MIN, 2000])]),
        vec![(i64::from(T0) + 10, 20.0)],
        "i16::MIN is the other sentinel"
    );
}

#[test]
fn mtu_response_parsing_guards_short_pdus() {
    assert_eq!(parse_mtu_response(&[0x03, 0x00, 0x02]), 512);
    assert_eq!(parse_mtu_response(&[0x03]), 23);
    assert_eq!(parse_mtu_response(&[0x03, 0x00]), 23);
    assert_eq!(parse_mtu_response(&[]), 23);
    assert_eq!(parse_mtu_response(&[0x02, 0x00, 0x02]), 23);
}

#[test]
fn logging_interval_set_is_1_5_15() {
    assert_eq!(LOGGING_INTERVALS_MIN, [1, 5, 15]);
    assert_eq!(interval_s(1).expect("ok"), 60);
    assert_eq!(interval_s(5).expect("ok"), 300);
    assert_eq!(interval_s(15).expect("ok"), 900);
    for bad in [0u16, 2, 7, 10, 30] {
        assert_eq!(
            interval_s(bad).expect_err("must refuse").kind(),
            Kind::InvalidArgument,
            "{bad} must never be snapped to a neighbour"
        );
    }
}

#[test]
fn a_full_chunk_is_sixty_samples_and_the_block_stride_includes_the_crc() {
    // Getting the block stride wrong (forgetting the 2 CRC bytes) shifts every sample after the
    // first block, which is the kind of error that produces a plausible-looking wrong archive.
    let samples: Vec<i16> = (0..60).collect();
    let rows = one_session(1, 60, &[chunk_body(&samples)]);
    assert_eq!(rows.len(), 60);
    for (i, (ts, _)) in rows.iter().enumerate() {
        assert_eq!(
            *ts - i64::from(T0),
            i as i64,
            "sample {i} must land on its own ordinal"
        );
    }
}
