//! Teltonika EYE Sensor (BTSMP1) BLE advertising parser.
//!
//! The sensor payload travels in BLE manufacturer-specific data under the
//! Teltonika Company ID `0x089A`. After the company id the bytes are:
//!
//! ```text
//! [ protocol version: 1 ][ flags: 1 ][ values... ]
//! ```
//!
//! `flags` is parsed LSB-first; each set bit means the corresponding value is
//! present, and the values are appended in ascending bit order (only for the
//! bits that carry a value):
//!
//! | bit | meaning                          | value bytes (in order) |
//! |-----|----------------------------------|------------------------|
//! | 0   | temperature present              | 2 (i16 big-endian /100 → °C) |
//! | 1   | humidity present                 | 1 (u8 %) |
//! | 2   | magnet sensor present            | — (state is bit 3) |
//! | 3   | magnet field detected (if bit 2) | — |
//! | 4   | movement counter present         | 2 (u16 BE: MSB=moving, low15=count) |
//! | 5   | movement angle present           | 3 (pitch i8, roll i16 BE) |
//! | 6   | low-battery indication           | — |
//! | 7   | battery voltage present          | 1 (u8 → 2000 + v*10 mV) |
//!
//! The parser is intentionally dependency-free and pure so it can be unit
//! tested on any host.

/// Teltonika Bluetooth SIG Company Identifier.
pub const TELTONIKA_COMPANY_ID: u16 = 0x089A;

/// Protocol version byte currently emitted by EYE firmware.
pub const PROTOCOL_VERSION: u8 = 0x01;

/// A decoded EYE sensor advertising frame. Fields are `Option` because the
/// sensor only transmits the values whose flag bit is set (configurable).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EyeReading {
    pub protocol_version: u8,
    pub flags: u8,
    /// Ambient temperature in °C.
    pub temperature_c: Option<f32>,
    /// Relative humidity in %.
    pub humidity_pct: Option<u8>,
    /// Whether a magnet sensor is present/enabled (flags bit 2).
    pub magnet_present: bool,
    /// Magnetic field detected (valid only if `magnet_present`).
    pub magnet_detected: bool,
    /// Movement currently detected.
    pub moving: Option<bool>,
    /// Cumulative movement-event count (15-bit).
    pub movement_count: Option<u16>,
    /// Device pitch in degrees (−90..90).
    pub pitch_deg: Option<i8>,
    /// Device roll in degrees (−180..180).
    pub roll_deg: Option<i16>,
    /// Low-battery indication (flags bit 6).
    pub low_battery: bool,
    /// Battery voltage in millivolts.
    pub battery_mv: Option<u16>,
}

/// Error returned when manufacturer data cannot be parsed as an EYE frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Fewer than the minimum `[version][flags]` bytes.
    TooShort,
    /// Protocol version is not the one this parser understands.
    UnknownVersion(u8),
    /// A flagged value extends past the end of the buffer.
    Truncated,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::TooShort => write!(f, "manufacturer data too short"),
            ParseError::UnknownVersion(v) => write!(f, "unknown EYE protocol version 0x{:02x}", v),
            ParseError::Truncated => write!(f, "EYE payload truncated (flag set but no data)"),
        }
    }
}

impl std::error::Error for ParseError {}

// Flag bit positions.
const F_TEMP: u8 = 0;
const F_HUM: u8 = 1;
const F_MAGNET_PRESENT: u8 = 2;
const F_MAGNET_STATE: u8 = 3;
const F_MOVE_COUNT: u8 = 4;
const F_MOVE_ANGLE: u8 = 5;
const F_LOW_BATT: u8 = 6;
const F_BATT: u8 = 7;

#[inline]
fn bit(flags: u8, n: u8) -> bool {
    (flags >> n) & 1 == 1
}

/// Parse the **value portion** of Teltonika manufacturer data — i.e. the bytes
/// that follow the Company ID (`0x089A`). On Linux/BlueZ this is exactly the
/// `value` of the `ManufacturerData` entry keyed by `0x089a`.
///
/// `data` layout: `[version][flags][values...]`.
pub fn parse_manufacturer_value(data: &[u8]) -> Result<EyeReading, ParseError> {
    if data.len() < 2 {
        return Err(ParseError::TooShort);
    }
    let version = data[0];
    if version != PROTOCOL_VERSION {
        return Err(ParseError::UnknownVersion(version));
    }
    let flags = data[1];

    let mut r = EyeReading {
        protocol_version: version,
        flags,
        magnet_present: bit(flags, F_MAGNET_PRESENT),
        magnet_detected: bit(flags, F_MAGNET_PRESENT) && bit(flags, F_MAGNET_STATE),
        low_battery: bit(flags, F_LOW_BATT),
        ..Default::default()
    };

    let mut i = 2usize;
    let mut take = |n: usize| -> Result<&[u8], ParseError> {
        let end = i.checked_add(n).ok_or(ParseError::Truncated)?;
        let slice = data.get(i..end).ok_or(ParseError::Truncated)?;
        i = end;
        Ok(slice)
    };

    if bit(flags, F_TEMP) {
        let b = take(2)?;
        let raw = i16::from_be_bytes([b[0], b[1]]);
        r.temperature_c = Some(raw as f32 / 100.0);
    }
    if bit(flags, F_HUM) {
        r.humidity_pct = Some(take(1)?[0]);
    }
    if bit(flags, F_MOVE_COUNT) {
        let b = take(2)?;
        let raw = u16::from_be_bytes([b[0], b[1]]);
        r.moving = Some(raw & 0x8000 != 0);
        r.movement_count = Some(raw & 0x7FFF);
    }
    if bit(flags, F_MOVE_ANGLE) {
        let b = take(3)?;
        r.pitch_deg = Some(b[0] as i8);
        r.roll_deg = Some(i16::from_be_bytes([b[1], b[2]]));
    }
    if bit(flags, F_BATT) {
        let v = take(1)?[0] as u16;
        r.battery_mv = Some(2000 + v * 10);
    }

    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real frame captured from tag 7C:D9:F4:13:10:DE (Sensors / Temp+Hum profile).
    #[test]
    fn parses_temp_hum_battery() {
        let r = parse_manufacturer_value(&[0x01, 0x83, 0x09, 0xab, 0x3f, 0x6a]).unwrap();
        assert_eq!(r.flags, 0x83);
        assert_eq!(r.temperature_c, Some(24.75));
        assert_eq!(r.humidity_pct, Some(63));
        assert!(!r.magnet_present);
        assert_eq!(r.battery_mv, Some(3060)); // 2000 + 106*10
        assert_eq!(r.moving, None);
    }

    // Real frame after enabling the magnet sensor (flags 0x87).
    #[test]
    fn parses_with_magnet_present_not_detected() {
        let r = parse_manufacturer_value(&[0x01, 0x87, 0x09, 0xb5, 0x40, 0x6b]).unwrap();
        assert_eq!(r.temperature_c, Some(24.85));
        assert_eq!(r.humidity_pct, Some(64));
        assert!(r.magnet_present);
        assert!(!r.magnet_detected);
        assert_eq!(r.battery_mv, Some(3070));
    }

    // Wiki worked example: flags 0xB7 → temp, hum, magnet, movement count, battery.
    #[test]
    fn parses_wiki_example_movement() {
        // 0xB7 = 1011 0111 → bits 0,1,2,4,5,7 (magnet present, no state bit3=0)
        // temp 08B4, hum 12, move 0CCB, angle 0BFFC7, batt 67
        let r = parse_manufacturer_value(&[
            0x01, 0xb7, 0x08, 0xb4, 0x12, 0x0c, 0xcb, 0x0b, 0xff, 0xc7, 0x67,
        ])
        .unwrap();
        assert_eq!(r.temperature_c, Some(22.28));
        assert_eq!(r.humidity_pct, Some(18));
        assert!(r.magnet_present);
        assert!(!r.magnet_detected);
        assert_eq!(r.moving, Some(false));
        assert_eq!(r.movement_count, Some(3275));
        assert_eq!(r.pitch_deg, Some(11));
        assert_eq!(r.roll_deg, Some(-57));
        assert_eq!(r.battery_mv, Some(2000 + 0x67 * 10));
    }

    #[test]
    fn moving_bit_sets_flag_and_masks_count() {
        // movement raw 0x8005 → moving=true, count=5
        let r = parse_manufacturer_value(&[0x01, 0x10, 0x80, 0x05]).unwrap();
        assert_eq!(r.moving, Some(true));
        assert_eq!(r.movement_count, Some(5));
    }

    #[test]
    fn negative_temperature() {
        // -5.00 °C = -500 = 0xFE0C
        let r = parse_manufacturer_value(&[0x01, 0x01, 0xfe, 0x0c]).unwrap();
        assert_eq!(r.temperature_c, Some(-5.0));
    }

    #[test]
    fn low_battery_flag() {
        let r = parse_manufacturer_value(&[0x01, 0x40]).unwrap();
        assert!(r.low_battery);
    }

    #[test]
    fn rejects_short_and_bad_version() {
        assert_eq!(parse_manufacturer_value(&[0x01]), Err(ParseError::TooShort));
        assert_eq!(
            parse_manufacturer_value(&[0x02, 0x00]),
            Err(ParseError::UnknownVersion(2))
        );
    }

    #[test]
    fn rejects_truncated_value() {
        // flag says temperature present but only 1 byte follows
        assert_eq!(
            parse_manufacturer_value(&[0x01, 0x01, 0x09]),
            Err(ParseError::Truncated)
        );
    }
}
