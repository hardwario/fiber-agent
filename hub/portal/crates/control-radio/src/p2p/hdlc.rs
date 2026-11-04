//! HDLC-style framing for the CM4 <-> northbridge link — `plan/system/lora/design.md` §3.
//!
//! ```text
//! 0x7E | payload (escaped) | 0x7E
//! escape: 0x7D, escaped byte XOR 0x20   (0x7E -> 0x7D 0x5E, 0x7D -> 0x7D 0x5D)
//!
//! payload (before escaping):
//! +--------+--------+----------------+-----------+
//! | type 1B| seq 1B | body 0..280 B  | crc16 2B  |
//! +--------+--------+----------------+-----------+
//! ```
//!
//! CRC16-CCITT (poly 0x1021, init 0xFFFF) over `type ‖ seq ‖ body`. Multi-byte integers on
//! this link are little-endian — the radio frame it carries is an opaque byte string whose own
//! big-endian layout is `p2p.md`'s concern, not this link's.
//!
//! Framing has to survive line noise and a stray console line: resync on the next `0x7E`, and
//! CRC rejects garbage. That is why decoding is a stream reader rather than a parse of a known
//! buffer.

/// Frame delimiter.
pub const FLAG: u8 = 0x7E;
/// Escape byte.
pub const ESC: u8 = 0x7D;
/// XOR applied to an escaped byte.
pub const ESC_XOR: u8 = 0x20;

/// The largest payload the link carries: type + seq + 280 B body + crc16.
pub const MAX_PAYLOAD: usize = 1 + 1 + 280 + 2;

/// CRC16-CCITT, poly 0x1021, init 0xFFFF — computed over the unescaped `type ‖ seq ‖ body`.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Encode one frame: `type ‖ seq ‖ body ‖ crc16`, byte-stuffed and flag-delimited.
pub fn encode(frame_type: u8, seq: u8, body: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(2 + body.len() + 2);
    payload.push(frame_type);
    payload.push(seq);
    payload.extend_from_slice(body);
    let crc = crc16(&payload);
    payload.extend_from_slice(&crc.to_le_bytes());

    let mut out = Vec::with_capacity(payload.len() + 8);
    out.push(FLAG);
    for &b in &payload {
        if b == FLAG || b == ESC {
            out.push(ESC);
            out.push(b ^ ESC_XOR);
        } else {
            out.push(b);
        }
    }
    out.push(FLAG);
    out
}

/// One decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub frame_type: u8,
    pub seq: u8,
    pub body: Vec<u8>,
}

/// Why a frame on the wire was discarded. Counted rather than propagated: a corrupt frame on a
/// serial line is expected, and the link resyncs rather than failing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecodeStats {
    pub crc_errors: u32,
    pub too_short: u32,
    pub oversized: u32,
}

/// Byte-stream decoder. Feed it whatever arrives; it yields complete, CRC-valid frames.
#[derive(Debug, Default)]
pub struct Decoder {
    /// Bytes since the last opening flag, unescaped.
    buf: Vec<u8>,
    in_frame: bool,
    escaping: bool,
    pub stats: DecodeStats,
}

impl Decoder {
    pub fn new() -> Self {
        Decoder::default()
    }

    /// Feed bytes; returns every complete frame they finished.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Frame> {
        let mut out = Vec::new();
        for &b in bytes {
            if b == FLAG {
                if self.in_frame && !self.buf.is_empty() {
                    if let Some(f) = self.finish() {
                        out.push(f);
                    }
                }
                // A flag always starts a fresh frame, which is what makes resync work: a
                // truncated frame is abandoned rather than merged into the next one.
                self.buf.clear();
                self.in_frame = true;
                self.escaping = false;
                continue;
            }
            if !self.in_frame {
                continue; // Pre-sync noise, or a stray console line.
            }
            if self.escaping {
                self.buf.push(b ^ ESC_XOR);
                self.escaping = false;
            } else if b == ESC {
                self.escaping = true;
            } else {
                self.buf.push(b);
            }
            if self.buf.len() > MAX_PAYLOAD {
                self.stats.oversized += 1;
                self.buf.clear();
                self.in_frame = false;
            }
        }
        out
    }

    fn finish(&mut self) -> Option<Frame> {
        let payload = std::mem::take(&mut self.buf);
        // type + seq + crc16
        if payload.len() < 4 {
            self.stats.too_short += 1;
            return None;
        }
        let split = payload.len() - 2;
        let (head, crc_bytes) = payload.split_at(split);
        let want = u16::from_le_bytes([crc_bytes[0], crc_bytes[1]]);
        if crc16(head) != want {
            self.stats.crc_errors += 1;
            return None;
        }
        Some(Frame {
            frame_type: head[0],
            seq: head[1],
            body: head[2..].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips() {
        let wire = encode(0x01, 42, &[1, 2, 3]);
        assert_eq!(wire.first(), Some(&FLAG));
        assert_eq!(wire.last(), Some(&FLAG));

        let mut d = Decoder::new();
        let frames = d.push(&wire);
        assert_eq!(
            frames,
            vec![Frame {
                frame_type: 0x01,
                seq: 42,
                body: vec![1, 2, 3]
            }]
        );
    }

    #[test]
    fn an_empty_body_round_trips() {
        let mut d = Decoder::new();
        let frames = d.push(&encode(0x03, 7, &[]));
        assert_eq!(frames.len(), 1);
        assert!(frames[0].body.is_empty());
    }

    /// Both special bytes must survive a round trip, or a frame containing 0x7E in its payload
    /// would terminate itself.
    #[test]
    fn the_delimiter_and_escape_bytes_survive_stuffing() {
        for body in [
            vec![FLAG],
            vec![ESC],
            vec![FLAG, ESC, FLAG],
            vec![0x00, FLAG, 0xff, ESC, 0x7f],
        ] {
            let wire = encode(0x05, 1, &body);
            // Neither special byte may appear raw inside the frame.
            assert!(
                !wire[1..wire.len() - 1].contains(&FLAG),
                "raw delimiter inside the frame for {body:?}"
            );
            let mut d = Decoder::new();
            let frames = d.push(&wire);
            assert_eq!(frames[0].body, body, "round trip for {body:?}");
        }
    }

    /// The reason framing exists: a corrupt frame is dropped and the next one still decodes.
    #[test]
    fn a_corrupted_frame_is_dropped_and_the_link_resyncs() {
        let good = encode(0x01, 1, b"hello");
        let mut bad = encode(0x01, 2, b"world");
        // Flip a payload byte so the CRC fails.
        let mid = bad.len() / 2;
        bad[mid] ^= 0xff;

        let mut d = Decoder::new();
        let mut frames = d.push(&bad);
        assert!(frames.is_empty(), "a bad CRC must not yield a frame");
        assert_eq!(d.stats.crc_errors, 1);

        frames = d.push(&good);
        assert_eq!(frames.len(), 1, "the next frame still decodes");
        assert_eq!(frames[0].body, b"hello");
    }

    /// A stray console line before the first flag must not corrupt the frame after it.
    #[test]
    fn noise_before_the_first_flag_is_ignored() {
        let mut d = Decoder::new();
        let mut wire = b"[00:00:01.234] boot: hello\r\n".to_vec();
        wire.extend_from_slice(&encode(0x02, 9, &[0xaa]));
        let frames = d.push(&wire);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].body, vec![0xaa]);
    }

    /// A truncated frame is abandoned at the next flag rather than merged into its successor.
    #[test]
    fn a_truncated_frame_does_not_swallow_the_next_one() {
        let full = encode(0x01, 1, b"abcdef");
        let truncated = &full[..full.len() / 2]; // no closing flag
        let next = encode(0x01, 2, b"xyz");

        let mut d = Decoder::new();
        let mut wire = truncated.to_vec();
        wire.extend_from_slice(&next);
        let frames = d.push(&wire);
        assert_eq!(frames.len(), 1, "only the intact frame decodes");
        assert_eq!(frames[0].body, b"xyz");
    }

    /// Bytes may arrive one at a time; the decoder is a stream reader, not a buffer parser.
    #[test]
    fn frames_decode_across_arbitrary_read_boundaries() {
        let wire = encode(0x05, 3, &[FLAG, ESC, 0x11, 0x22]);
        let mut d = Decoder::new();
        let mut got = Vec::new();
        for b in &wire {
            got.extend(d.push(&[*b]));
        }
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, vec![FLAG, ESC, 0x11, 0x22]);
    }

    #[test]
    fn two_frames_in_one_read_both_decode() {
        let mut wire = encode(0x01, 1, b"a");
        wire.extend_from_slice(&encode(0x02, 2, b"b"));
        let mut d = Decoder::new();
        let frames = d.push(&wire);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].seq, 1);
        assert_eq!(frames[1].seq, 2);
    }

    #[test]
    fn a_short_frame_is_counted_not_returned() {
        let mut d = Decoder::new();
        // Flag, two bytes, flag — shorter than type+seq+crc.
        let frames = d.push(&[FLAG, 0x01, 0x02, FLAG]);
        assert!(frames.is_empty());
        assert_eq!(d.stats.too_short, 1);
    }

    /// CRC16-CCITT with init 0xFFFF, against the standard check vector.
    #[test]
    fn crc16_matches_the_ccitt_false_check_value() {
        assert_eq!(crc16(b"123456789"), 0x29B1);
        assert_eq!(crc16(&[]), 0xFFFF);
    }

    #[test]
    fn an_oversized_frame_is_abandoned() {
        let mut d = Decoder::new();
        let mut wire = vec![FLAG];
        wire.extend(std::iter::repeat_n(0xAA, MAX_PAYLOAD + 10));
        wire.push(FLAG);
        let frames = d.push(&wire);
        assert!(frames.is_empty());
        assert_eq!(d.stats.oversized, 1);
    }
}
