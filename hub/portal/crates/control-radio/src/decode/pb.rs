//! A minimal protobuf wire reader — enough for the Node's uplink messages.
//!
//! **Why not prost.** `plan.md` §4 says to generate Rust types from the pinned repo's
//! `proto/` at build time. There is no `proto/` directory at `sticker-firmware@v1.4.0`: the
//! committed file is `app/src/app_config.proto`, and the rest is generated from
//! `app/src/app_config.yml` by a west command at build time. Codegen "from the pinned
//! `proto/`" is therefore not implementable as written. What *is* available, and is the
//! stronger contract, is the reference decoder `app/decoder/ttn.js` and its 64 HW-verified
//! vectors — which the plan already makes the parity acceptance gate. So the wire is read
//! directly, against the field tables in `node-firmware-v1.4.0.md` §3-§5, and proven by
//! parity. Carried to the plan-feedback set.
//!
//! **The 32-bit semantics are deliberate.** The reference decoder is JavaScript and its
//! varint reader uses 32-bit bitwise operators: it accumulates into a 32-bit value and stops
//! once the shift reaches 32. Reading these as 64-bit would decode some malformed frames
//! *differently* from the device's own decoder, and parity would fail on exactly the inputs
//! that matter — the malformed ones. So this mirrors the truncation rather than improving on
//! it.

/// A varint read: the 32-bit value and the offset just past it.
pub struct Varint {
    pub value: u32,
    pub next: usize,
}

/// Read a base-128 varint, truncating at 32 bits exactly as the reference does.
///
/// Running off the end of the buffer yields what was accumulated so far rather than an error
/// — again mirroring the reference, whose truncated-frame tests assert a partial decode and
/// never a `NaN`.
pub fn read_varint(bytes: &[u8], offset: usize) -> Varint {
    let mut result: u32 = 0;
    let mut shift: u32 = 0;
    let mut pos = offset;
    while pos < bytes.len() {
        let b = bytes[pos];
        pos += 1;
        // `<<` past 31 is UB-adjacent in Rust and a no-op-ish in JS; the shift>=32 guard
        // below is what actually terminates, so mask defensively.
        if shift < 32 {
            result |= ((b & 0x7f) as u32) << shift;
        }
        if b & 0x80 == 0 {
            return Varint {
                value: result,
                next: pos,
            };
        }
        shift += 7;
        if shift >= 32 {
            return Varint {
                value: result,
                next: pos,
            };
        }
    }
    Varint {
        value: result,
        next: pos,
    }
}

/// Zigzag-decode, in 32-bit space: `(n >>> 1) ^ -(n & 1)`.
pub fn zigzag(n: u32) -> i32 {
    ((n >> 1) as i32) ^ -((n & 1) as i32)
}

/// A protobuf tag: field number and wire type.
pub struct Tag {
    pub field: u32,
    pub wire: u32,
    pub next: usize,
}

pub fn read_tag(bytes: &[u8], offset: usize) -> Tag {
    let v = read_varint(bytes, offset);
    Tag {
        field: v.value >> 3,
        wire: v.value & 0x7,
        next: v.next,
    }
}

/// Skip a field of a wire type this decoder does not handle.
///
/// Returns `None` when the wire type is unknown, which the reference treats as "stop parsing"
/// rather than "skip" — a distinction that shows up on corrupt frames.
pub fn skip(bytes: &[u8], wire: u32, pos: usize) -> Option<usize> {
    match wire {
        2 => {
            let l = read_varint(bytes, pos);
            Some(l.next + l.value as usize)
        }
        5 => Some(pos + 4),
        1 => Some(pos + 8),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_byte_varints() {
        assert_eq!(read_varint(&[0x00], 0).value, 0);
        assert_eq!(read_varint(&[0x64], 0).value, 100);
        assert_eq!(read_varint(&[0x7f], 0).value, 127);
    }

    #[test]
    fn multi_byte_varints_match_the_reference_examples() {
        // From the captured vectors: 0xa801 -> 168 (voltage), 0xfa24 -> 4730 (temperature raw)
        assert_eq!(read_varint(&[0xa8, 0x01], 0).value, 168);
        assert_eq!(read_varint(&[0xfa, 0x24], 0).value, 4730);
        assert_eq!(read_varint(&[0x97, 0x4f], 0).value, 10135);
        assert_eq!(read_varint(&[0xcd, 0x0e], 0).value, 1869);
    }

    #[test]
    fn zigzag_matches_the_reference() {
        assert_eq!(zigzag(0), 0);
        assert_eq!(zigzag(1), -1);
        assert_eq!(zigzag(2), 1);
        assert_eq!(zigzag(4730), 2365);
        assert_eq!(zigzag(1869), -935);
        // The INT32_MIN sentinel: UINT32_MAX zigzags to INT32_MIN.
        assert_eq!(zigzag(u32::MAX), i32::MIN);
    }

    /// The truncation the reference performs, mirrored rather than improved on.
    #[test]
    fn varints_truncate_at_32_bits_like_the_reference() {
        // Five continuation bytes: the reference stops once shift reaches 32.
        let v = read_varint(&[0xff, 0xff, 0xff, 0xff, 0xff, 0x01], 0);
        assert_eq!(v.value, u32::MAX);
        assert_eq!(v.next, 5, "stops after the fifth byte, not the sixth");
    }

    /// Running off the end yields a partial value, never a panic — the reference's
    /// truncated-frame tests depend on this.
    #[test]
    fn a_truncated_varint_does_not_panic() {
        let v = read_varint(&[0xff], 0);
        assert_eq!(v.next, 1);
        let v = read_varint(&[], 0);
        assert_eq!(v.value, 0);
    }

    #[test]
    fn tags_split_into_field_and_wire_type() {
        // 0x08 -> field 1, varint
        let t = read_tag(&[0x08], 0);
        assert_eq!((t.field, t.wire), (1, 0));
        // 0xda 0x01 -> field 27, length-delimited
        let t = read_tag(&[0xda, 0x01], 0);
        assert_eq!((t.field, t.wire), (27, 2));
        // 0xd0 0x01 -> field 26, varint
        let t = read_tag(&[0xd0, 0x01], 0);
        assert_eq!((t.field, t.wire), (26, 0));
    }

    #[test]
    fn unknown_wire_types_stop_rather_than_skip() {
        assert_eq!(skip(&[0x05, 0xaa], 2, 0), Some(6));
        assert_eq!(skip(&[], 5, 0), Some(4));
        assert_eq!(skip(&[], 1, 0), Some(8));
        assert_eq!(skip(&[], 3, 0), None, "group wire types are not skippable");
        assert_eq!(skip(&[], 7, 0), None);
    }
}
