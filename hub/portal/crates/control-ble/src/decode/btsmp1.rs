//! The Teltonika BTSMP1 advertisement parser — `plan/control/ble/beacon-btsmp1.md` §1.
//!
//! ```text
//! manufacturer data value (after company id 0x089A):
//! +---------+---------+-----------------------------------+
//! | version | flags   | values, in flag-bit order          |
//! | 1 B     | 1 B     | variable                           |
//! +---------+---------+-----------------------------------+
//! ```
//!
//! **Values follow in bit order**, so the frame is variable length and field offsets depend on which
//! flags are set. There is no fixed layout to index — a parser must walk the flags. Getting this
//! wrong does not error: it reads a battery code as a humidity percentage.

use super::{ParseError, Reading};

/// Flag bit → field, from §1's table.
mod bit {
    pub const TEMPERATURE: u8 = 1 << 0;
    pub const HUMIDITY: u8 = 1 << 1;
    pub const MAGNET_PRESENT: u8 = 1 << 2;
    pub const MAGNET_DETECTED: u8 = 1 << 3;
    pub const MOVEMENT: u8 = 1 << 4;
    pub const ORIENTATION: u8 = 1 << 5;
    pub const LOW_BATTERY: u8 = 1 << 6;
    pub const BATTERY: u8 = 1 << 7;
}

/// The only application protocol version this parser speaks.
pub const PROTOCOL_VERSION: u8 = 0x01;

/// Battery encoding: millivolts = `2000 + 10 × value`.
const BATTERY_BASE_MV: u16 = 2000;
const BATTERY_STEP_MV: u16 = 10;

/// The MSB of the movement word is "currently moving"; the low 15 bits are the counter.
const MOVING_MASK: u16 = 0x8000;
const MOVEMENT_COUNT_MASK: u16 = 0x7FFF;

/// A cursor that yields `Truncated` rather than panicking or padding.
struct Cursor<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Cursor<'a> {
        Cursor { buf, at: 0 }
    }
    fn take(&mut self, n: usize) -> std::result::Result<&'a [u8], ParseError> {
        let end = self.at.checked_add(n).ok_or(ParseError::Truncated)?;
        let slice = self.buf.get(self.at..end).ok_or(ParseError::Truncated)?;
        self.at = end;
        Ok(slice)
    }
    fn u8(&mut self) -> std::result::Result<u8, ParseError> {
        Ok(self.take(1)?[0])
    }
    fn i16_be(&mut self) -> std::result::Result<i16, ParseError> {
        let b = self.take(2)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }
    fn u16_be(&mut self) -> std::result::Result<u16, ParseError> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
}

/// Parse the manufacturer-data **value** — the bytes after the `0x089A` company id.
///
/// Any field the flags claim but the frame does not carry rejects the **whole** frame
/// (`Truncated`): a half-decoded reading published as fact is worse than a dropped one, and §1 makes
/// that a contract rather than a nicety.
pub fn parse(value: &[u8]) -> std::result::Result<Reading, ParseError> {
    if value.len() < 2 {
        return Err(ParseError::TooShort);
    }
    let version = value[0];
    if version != PROTOCOL_VERSION {
        return Err(ParseError::UnknownVersion(version));
    }
    let flags = value[1];
    let mut r = Reading::empty(version, flags);
    let mut c = Cursor::new(&value[2..]);

    // Bit order is the wire order. Do not reorder these blocks.
    if flags & bit::TEMPERATURE != 0 {
        r.temperature_c = Some(f64::from(c.i16_be()?) / 100.0);
    }
    if flags & bit::HUMIDITY != 0 {
        r.humidity_pct = Some(c.u8()?);
    }
    // Bits 2 and 3 carry no bytes. Bit 3 is "only meaningful when bit 2 is also set", so a device
    // claiming a magnet reading without claiming a magnet sensor does not get to assert one.
    r.magnet_present = flags & bit::MAGNET_PRESENT != 0;
    r.magnet_detected = r.magnet_present && (flags & bit::MAGNET_DETECTED != 0);
    if flags & bit::MOVEMENT != 0 {
        let raw = c.u16_be()?;
        r.moving = Some(raw & MOVING_MASK != 0);
        r.movement_count = Some(raw & MOVEMENT_COUNT_MASK);
    }
    if flags & bit::ORIENTATION != 0 {
        // Three bytes, and they are not interchangeable: pitch is a signed byte, roll a signed
        // big-endian word.
        r.pitch_deg = Some(c.u8()? as i8);
        r.roll_deg = Some(c.i16_be()?);
    }
    r.low_battery = flags & bit::LOW_BATTERY != 0;
    if flags & bit::BATTERY != 0 {
        r.battery_mv = Some(BATTERY_BASE_MV + u16::from(c.u8()?) * BATTERY_STEP_MV);
    }
    // A trailing byte the flags do not claim is left alone: the flags are authoritative about what
    // the frame carries, and a longer frame is a firmware that added something we do not read.
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_documented_worked_example_decodes() {
        // beacon-btsmp1.md §1 "Worked example".
        let r = parse(&[0x01, 0x83, 0x09, 0xAB, 0x3F, 0x6A]).expect("decodes");
        assert_eq!(r.temperature_c, Some(24.75));
        assert_eq!(r.humidity_pct, Some(63));
        assert_eq!(r.battery_mv, Some(3060));
    }

    #[test]
    fn a_frame_of_one_byte_is_too_short_and_a_bumped_version_is_unknown() {
        assert_eq!(parse(&[]).unwrap_err(), ParseError::TooShort);
        assert_eq!(parse(&[0x01]).unwrap_err(), ParseError::TooShort);
        assert_eq!(
            parse(&[0x02, 0x00]).unwrap_err(),
            ParseError::UnknownVersion(0x02)
        );
        // The version check happens before any field walk, so a bumped version never even attempts
        // to read the remainder — that is the §7 rule.
        assert_eq!(
            parse(&[0xFF, 0xFF, 0x09, 0xAB]).unwrap_err(),
            ParseError::UnknownVersion(0xFF)
        );
    }

    #[test]
    fn zero_flags_is_a_valid_frame_that_carries_nothing() {
        let r = parse(&[0x01, 0x00]).expect("decodes");
        assert_eq!(r.flags, 0);
        assert_eq!(r.temperature_c, None);
        assert!(!r.low_battery);
    }

    #[test]
    fn every_single_flag_consumes_exactly_the_documented_byte_count() {
        // The table's byte counts, one flag at a time. A frame one byte short must be Truncated and
        // a frame of exactly the right length must decode — this is what pins each width.
        for (flag, width) in [
            (bit::TEMPERATURE, 2usize),
            (bit::HUMIDITY, 1),
            (bit::MAGNET_PRESENT, 0),
            (bit::MAGNET_DETECTED, 0),
            (bit::MOVEMENT, 2),
            (bit::ORIENTATION, 3),
            (bit::LOW_BATTERY, 0),
            (bit::BATTERY, 1),
        ] {
            let mut exact = vec![0x01, flag];
            exact.extend(std::iter::repeat_n(0u8, width));
            parse(&exact).unwrap_or_else(|e| panic!("flag {flag:#04X} width {width}: {e:?}"));

            if width > 0 {
                let short = exact[..exact.len() - 1].to_vec();
                assert_eq!(
                    parse(&short).unwrap_err(),
                    ParseError::Truncated,
                    "flag {flag:#04X} must need all {width} bytes"
                );
            }
        }
    }

    #[test]
    fn temperature_is_signed_big_endian_hundredths() {
        assert_eq!(
            parse(&[0x01, 0x01, 0xFE, 0x0C])
                .expect("decodes")
                .temperature_c,
            Some(-5.0)
        );
        assert_eq!(
            parse(&[0x01, 0x01, 0x00, 0x00])
                .expect("decodes")
                .temperature_c,
            Some(0.0)
        );
        // Byte order matters: the same bytes swapped are a different (and wrong) temperature.
        assert_eq!(
            parse(&[0x01, 0x01, 0x0C, 0xFE])
                .expect("decodes")
                .temperature_c,
            Some(33.26)
        );
    }

    #[test]
    fn the_movement_word_splits_into_a_flag_and_a_fifteen_bit_counter() {
        let r = parse(&[0x01, 0x10, 0x80, 0x05]).expect("decodes");
        assert_eq!(r.moving, Some(true));
        assert_eq!(r.movement_count, Some(5));
        // The counter saturates at 15 bits, and the top bit never leaks into it.
        let r = parse(&[0x01, 0x10, 0xFF, 0xFF]).expect("decodes");
        assert_eq!(r.moving, Some(true));
        assert_eq!(r.movement_count, Some(0x7FFF));
        let r = parse(&[0x01, 0x10, 0x7F, 0xFF]).expect("decodes");
        assert_eq!(r.moving, Some(false));
        assert_eq!(r.movement_count, Some(0x7FFF));
    }

    #[test]
    fn the_battery_code_is_an_offset_not_a_voltage() {
        assert_eq!(
            parse(&[0x01, 0x80, 0x00]).expect("decodes").battery_mv,
            Some(2000)
        );
        assert_eq!(
            parse(&[0x01, 0x80, 0xFF]).expect("decodes").battery_mv,
            Some(2000 + 2550)
        );
    }

    #[test]
    fn orientation_reads_pitch_then_roll_in_that_order() {
        let r = parse(&[0x01, 0x20, 0xF6, 0xFF, 0xC7]).expect("decodes");
        assert_eq!(r.pitch_deg, Some(-10), "pitch is a signed byte");
        assert_eq!(r.roll_deg, Some(-57), "roll is a signed big-endian word");
    }
}
