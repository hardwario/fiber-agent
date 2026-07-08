//! STICKER alarm-slot codec (fPort-85 `alarms.alarm_0..15`).
//!
//! Each slot is a 17-byte little-endian packed rule; this module is the single
//! source of truth for that layout and the source/quantity tables (mirroring
//! the sticker firmware's app_alarm). It decodes a slot for display and encodes
//! an edited slot back to bytes for `SetParam`. Source/quantity are kept as raw
//! `u8` with name lookups (rather than closed enums) so a firmware that adds a
//! value never breaks a read — unknown values surface as "unknown".
//!
//! 17-byte layout (LE): flags(1) [bit0 present, bit1 enabled], source(1),
//! quantity(1), from_state(1), to_state(1), lo(f32), hi(f32), hst(f32).

pub const SLOT_LEN: usize = 17;
pub const SLOT_COUNT: u8 = 16;

/// A decoded alarm rule slot.
#[derive(Debug, Clone, PartialEq)]
pub struct AlarmSlot {
    pub present: bool,
    pub enabled: bool,
    pub source: u8,
    pub quantity: u8,
    pub from_state: u8,
    pub to_state: u8,
    pub lo: f32,
    pub hi: f32,
    pub hst: f32,
}

/// Symbolic name for an alarm source discriminant (see the v1.4 doc §7).
pub fn source_name(v: u8) -> &'static str {
    match v {
        0 => "onboard",
        1 => "s1",
        2 => "s2",
        3 => "s3",
        4 => "s4",
        5 => "hall_left",
        6 => "hall_right",
        7 => "input_a",
        8 => "input_b",
        9 => "pir",
        10 => "accel",
        11 => "battery",
        _ => "unknown",
    }
}

/// Symbolic name for an alarm quantity discriminant (see the v1.4 doc §7).
pub fn quantity_name(v: u8) -> &'static str {
    match v {
        0 => "temperature",
        1 => "humidity",
        2 => "pressure",
        3 => "illuminance",
        4 => "magnetic_field",
        5 => "tilt",
        6 => "state",
        7 => "count",
        8 => "voltage",
        _ => "unknown",
    }
}

/// True for analog quantities whose rule is a `[lo, hi]` band with hysteresis.
pub fn is_threshold(quantity: u8) -> bool {
    matches!(quantity, 0..=4) // temperature, humidity, pressure, illuminance, magnetic_field
}

/// Which quantities each source can raise. Mirrors the sticker firmware
/// authority `app_alarm_rules.c::app_alarm_rule_valid` (v1.4.0): onboard does
/// temp/hum/pressure; the 1-Wire slots s1..s4 do temp/hum/illuminance/magnetic/
/// tilt; the digital sources (hall l/r, input a/b, pir, accel) do state/count.
/// battery/voltage is watchdog-only and intentionally excluded.
pub fn valid_source_quantity(source: u8, quantity: u8) -> bool {
    match quantity {
        0 | 1 => matches!(source, 0..=4),     // temperature, humidity: onboard + s1..s4
        2 => source == 0,                     // pressure: onboard
        3 | 4 | 5 => matches!(source, 1..=4), // illuminance, magnetic_field, tilt: 1-Wire slots s1..s4
        6 | 7 => matches!(source, 5..=10),    // state, count: hall l/r, input a/b, pir, accel
        _ => false,                           // voltage/unknown: not user-settable
    }
}

/// Decode a 34-char hex string (17 bytes) into an `AlarmSlot`.
pub fn decode_slot(hex_str: &str) -> Result<AlarmSlot, String> {
    let bytes = hex::decode(hex_str.trim()).map_err(|e| format!("invalid hex: {e}"))?;
    if bytes.len() != SLOT_LEN {
        return Err(format!("expected {SLOT_LEN} bytes, got {}", bytes.len()));
    }
    let flags = bytes[0];
    let f32le = |o: usize| f32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
    Ok(AlarmSlot {
        present: flags & 0x01 != 0,
        enabled: flags & 0x02 != 0,
        source: bytes[1],
        quantity: bytes[2],
        from_state: bytes[3],
        to_state: bytes[4],
        lo: f32le(5),
        hi: f32le(9),
        hst: f32le(13),
    })
}

/// Pack an `AlarmSlot` into its 17 wire bytes.
pub fn encode_slot(s: &AlarmSlot) -> [u8; SLOT_LEN] {
    let mut b = [0u8; SLOT_LEN];
    b[0] = (s.present as u8) | ((s.enabled as u8) << 1);
    b[1] = s.source;
    b[2] = s.quantity;
    b[3] = s.from_state;
    b[4] = s.to_state;
    b[5..9].copy_from_slice(&s.lo.to_le_bytes());
    b[9..13].copy_from_slice(&s.hi.to_le_bytes());
    b[13..17].copy_from_slice(&s.hst.to_le_bytes());
    b
}

/// Validate a slot before it is sent. An absent slot (`present == false`) is a
/// clear and always allowed; a present slot must use a valid source×quantity
/// combo and finite, ordered thresholds.
pub fn validate_slot(s: &AlarmSlot) -> Result<(), String> {
    if !s.present {
        return Ok(());
    }
    if s.source == 11 || s.quantity == 8 {
        return Err("battery/voltage is watchdog-only, not a user-settable rule".into());
    }
    if !valid_source_quantity(s.source, s.quantity) {
        return Err(format!(
            "quantity {} is not valid for source {}",
            quantity_name(s.quantity),
            source_name(s.source)
        ));
    }
    if !s.lo.is_finite() || !s.hi.is_finite() || !s.hst.is_finite() {
        return Err("lo/hi/hst must be finite numbers".into());
    }
    if s.hst < 0.0 {
        return Err("hysteresis (hst) must be >= 0".into());
    }
    if is_threshold(s.quantity) && s.lo > s.hi {
        return Err("lo must be <= hi for a threshold rule".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Onboard temperature threshold, present+enabled, lo=15.0 hi=25.0 hst=0.5.
    const KNOWN_HEX: &str = "0300000000000070410000c8410000003f";

    #[test]
    fn decodes_known_vector() {
        let s = decode_slot(KNOWN_HEX).unwrap();
        assert!(s.present && s.enabled);
        assert_eq!(s.source, 0);
        assert_eq!(s.quantity, 0);
        assert_eq!(s.lo, 15.0);
        assert_eq!(s.hi, 25.0);
        assert_eq!(s.hst, 0.5);
    }

    #[test]
    fn encode_decode_round_trips() {
        let s = AlarmSlot {
            present: true,
            enabled: true,
            source: 5, // hall_left
            quantity: 7, // count
            from_state: 0,
            to_state: 0,
            lo: 0.0,
            hi: 100.0,
            hst: 0.0,
        };
        assert_eq!(decode_slot(&hex::encode(encode_slot(&s))).unwrap(), s);
        // and the known vector encodes back to its exact bytes
        assert_eq!(hex::encode(encode_slot(&decode_slot(KNOWN_HEX).unwrap())), KNOWN_HEX);
    }

    #[test]
    fn empty_slot_is_all_zero_and_valid() {
        let empty = decode_slot(&"00".repeat(SLOT_LEN)).unwrap();
        assert!(!empty.present);
        assert!(validate_slot(&empty).is_ok()); // clearing a slot is allowed
    }

    #[test]
    fn rejects_bad_length_and_hex() {
        assert!(decode_slot("00").is_err());
        assert!(decode_slot("zz").is_err());
    }

    #[test]
    fn validates_source_quantity_matrix() {
        let mk = |source, quantity| AlarmSlot {
            present: true, enabled: true, source, quantity,
            from_state: 0, to_state: 0, lo: 0.0, hi: 1.0, hst: 0.0,
        };
        assert!(validate_slot(&mk(0, 0)).is_ok()); // onboard temperature
        assert!(validate_slot(&mk(1, 0)).is_ok()); // s1 temperature
        assert!(validate_slot(&mk(1, 2)).is_err()); // pressure only onboard
        assert!(validate_slot(&mk(5, 7)).is_ok()); // hall_left count
        assert!(validate_slot(&mk(11, 8)).is_err()); // battery/voltage excluded
        // Mirror app_alarm_rules.c: illuminance/magnetic/tilt live on the 1-Wire
        // slots s1..s4 (not onboard/hall/accel); state+count on the digital sources.
        assert!(validate_slot(&mk(1, 3)).is_ok()); // s1 illuminance
        assert!(validate_slot(&mk(0, 3)).is_err()); // onboard has no illuminance rule
        assert!(validate_slot(&mk(2, 4)).is_ok()); // s2 magnetic_field
        assert!(validate_slot(&mk(9, 7)).is_ok()); // pir count (motion)
        assert!(validate_slot(&mk(10, 6)).is_ok()); // accel state
    }

    #[test]
    fn rejects_inverted_threshold() {
        let s = AlarmSlot {
            present: true, enabled: true, source: 0, quantity: 0,
            from_state: 0, to_state: 0, lo: 25.0, hi: 15.0, hst: 0.0,
        };
        assert!(validate_slot(&s).is_err());
    }
}
