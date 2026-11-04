//! The vendor's "CustomXTEA" — `plan/control/ble/beacon-btsmp1.md` §3.
//!
//! **This is obfuscation, not cryptography, and it is reverse-engineered.** It is not a standard
//! construction and there is nothing to validate it against beyond the beacons it was derived from —
//! which is why the whole archive path sits behind a capability flag. Naming it "encrypt" follows the
//! source document's vocabulary; it protects nothing.
//!
//! Two properties are easy to get subtly wrong and both are asserted below:
//!
//! - **It operates on byte pairs, and an odd trailing byte is dropped.** Not padded, not passed
//!   through — dropped. So `encrypt` of an odd-length input is one byte shorter than its input.
//! - **Every half is masked to 8 bits after every step.** The arithmetic is done in `i64` and
//!   narrowed each time; doing it in `u8` with wrapping arithmetic gives a different answer, because
//!   the round function's shifts are computed on the *unmasked* intermediate.

/// The vendor's key material, such as it is.
pub const KEYS: [i64; 4] = [24, 113, 130, 185];
pub const DELTA: i64 = 105;
pub const TIMES: usize = 5;

/// The round function, on a value already masked to 8 bits.
#[inline]
fn f(v: i64) -> i64 {
    ((v << 4) ^ (v >> 5)) + v
}

/// Encrypt in place, pair by pair. An odd trailing byte is dropped.
pub fn encrypt(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() & !1);
    for pair in data.chunks_exact(2) {
        let (mut v0, mut v1) = (i64::from(pair[0]), i64::from(pair[1]));
        let mut s: i64 = 0;
        for _ in 0..TIMES {
            v0 = (v0 + (f(v1) ^ (s + KEYS[(s & 3) as usize]))) & 0xFF;
            s += DELTA;
            v1 = (v1 + (f(v0) ^ (s + KEYS[((s >> 6) & 3) as usize]))) & 0xFF;
        }
        out.push(v0 as u8);
        out.push(v1 as u8);
    }
    out
}

/// The exact inverse of [`encrypt`], run in the mirrored order with `s` counting down.
pub fn decrypt(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() & !1);
    for pair in data.chunks_exact(2) {
        let (mut v0, mut v1) = (i64::from(pair[0]), i64::from(pair[1]));
        let mut s: i64 = DELTA * TIMES as i64;
        for _ in 0..TIMES {
            v1 = (v1 - (f(v0) ^ (s + KEYS[((s >> 6) & 3) as usize]))) & 0xFF;
            s -= DELTA;
            v0 = (v0 - (f(v1) ^ (s + KEYS[(s & 3) as usize]))) & 0xFF;
        }
        out.push(v0 as u8);
        out.push(v1 as u8);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_round_trip_holds_for_every_even_length_input() {
        // §3's stated property: `decrypt(encrypt(x)) == x` for every even-length x.
        let samples: Vec<Vec<u8>> = vec![
            vec![],
            vec![0x00, 0x00],
            vec![0xFF, 0xFF],
            vec![0x01, 0x00, 0x3C, 0x00, 0x02, 0x00, 0x8E, 0xC6, 0x43, 0x6A],
            (0u8..40).collect(),
            (0u8..=255).collect(),
        ];
        for s in samples {
            assert_eq!(decrypt(&encrypt(&s)), s, "round trip failed for {s:?}");
            // And the other direction, since the archive path decrypts what the beacon encrypted.
            assert_eq!(encrypt(&decrypt(&s)), s, "inverse failed for {s:?}");
        }
    }

    #[test]
    fn every_byte_pair_round_trips() {
        // Exhaustive over the whole 2-byte block space: 65 536 pairs, which is cheap and removes
        // any doubt about the masking.
        for a in 0u8..=255 {
            for b in 0u8..=255 {
                let input = [a, b];
                assert_eq!(
                    decrypt(&encrypt(&input)),
                    input.to_vec(),
                    "pair {a:02X} {b:02X}"
                );
            }
        }
    }

    #[test]
    fn an_odd_tail_is_dropped_not_padded() {
        let odd = [0x01, 0x02, 0x03];
        let enc = encrypt(&odd);
        assert_eq!(enc.len(), 2, "the third byte is dropped, not padded");
        assert_eq!(decrypt(&enc), vec![0x01, 0x02]);
        assert_eq!(encrypt(&[0x01]).len(), 0, "a lone byte encrypts to nothing");
        assert_eq!(decrypt(&[0x01]).len(), 0);
    }

    #[test]
    fn the_output_actually_differs_from_the_input() {
        // A transform that quietly did nothing would pass a round-trip test perfectly.
        let plain = vec![0x01, 0x00, 0x3C, 0x00, 0x02, 0x00, 0x8E, 0xC6, 0x43, 0x6A];
        assert_ne!(encrypt(&plain), plain);
    }

    #[test]
    fn each_block_is_independent_of_its_neighbours() {
        // ECB, in effect: the same pair encrypts to the same bytes wherever it appears. Worth
        // pinning, because it is what lets a chunk be decrypted without the ones before it.
        let one = encrypt(&[0xAB, 0xCD]);
        let two = encrypt(&[0xAB, 0xCD, 0xAB, 0xCD]);
        assert_eq!(two[..2], one[..]);
        assert_eq!(two[2..], one[..]);
    }

    #[test]
    fn the_constants_are_the_documented_ones() {
        assert_eq!(KEYS, [24, 113, 130, 185]);
        assert_eq!(DELTA, 105);
        assert_eq!(TIMES, 5);
    }

    #[test]
    fn the_masking_is_not_the_same_as_wrapping_u8_arithmetic() {
        // If someone "simplifies" this to u8 wrapping arithmetic, the shifts in the round function
        // lose their high bits and the answer changes. Pin one block so that rewrite fails loudly.
        let block = [0x12, 0x34];
        let enc = encrypt(&block);
        // Recompute the documented way, independently of the implementation above.
        let (mut v0, mut v1) = (0x12i64, 0x34i64);
        let mut s = 0i64;
        for _ in 0..5 {
            v0 = (v0 + ((((v1 << 4) ^ (v1 >> 5)) + v1) ^ (s + KEYS[(s & 3) as usize]))) & 0xFF;
            s += DELTA;
            v1 = (v1 + ((((v0 << 4) ^ (v0 >> 5)) + v0) ^ (s + KEYS[((s >> 6) & 3) as usize])))
                & 0xFF;
        }
        assert_eq!(enc, vec![v0 as u8, v1 as u8]);
    }
}
