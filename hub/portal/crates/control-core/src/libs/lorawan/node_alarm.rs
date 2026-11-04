//! NODE alarm-slot codec (fPort-85 `alarms.alarm_0..15`).
//!
//! *Amended 2026-10-09 (sticker-firmware v2.0.0, #430,
//! `plan/control/radio/node-firmware-v2.0.0.md` §2):* a slot is an 18-byte rule
//! addressed by `(slot, channel, sensor_type)` of the sensor channel model, not
//! the 17-byte v1.4 `(source, quantity)` rule. The layout, the firmware's
//! `app_alarm_rule_valid` checks and the channel registry all live in
//! `control_radio::decode::alarm_rule` / `sensor_types`; this module only adapts
//! them to the hex strings the config engine reads and writes, so the legacy
//! stack and the v2 radio cannot disagree on what a rule means.
//!
//! 18-byte layout (LE): flags(1) [bit0 present, bit1 enabled], slot(1),
//! channel(1), sensor_type(1), from_state(1), to_state(1), lo(f32), hi(f32),
//! dwell(f32, seconds).

use control_radio::decode::alarm_rule;
use control_radio::decode::sensor_types;

pub use control_radio::decode::alarm_rule::Rule;

/// Packed size of one rule.
pub const SLOT_LEN: usize = alarm_rule::RULE_LEN;
/// Rule slots `alarm_0 .. alarm_15`.
pub const SLOT_COUNT: u8 = 16;

/// Decode a slot's hex (36 chars, or empty) into its rule; `None` for an empty
/// slot (zero-length, or the `present` flag clear).
pub fn decode_slot(hex_str: &str) -> Result<Option<Rule>, String> {
    let bytes = hex::decode(hex_str.trim()).map_err(|e| format!("invalid hex: {e}"))?;
    Rule::unpack(&bytes)
}

/// Pack a slot into its 18 wire bytes; `None` is the all-zero empty slot.
pub fn encode_slot(rule: Option<&Rule>) -> [u8; SLOT_LEN] {
    rule.map(Rule::pack).unwrap_or([0u8; SLOT_LEN])
}

/// Validate a slot before it is sent. An empty slot is a clear and always
/// allowed; a rule must pass the firmware's `rule_valid` checks (registry
/// target, `hi > lo`, momentary `from != to`, dwell 0..3600 s) with finite
/// numbers. Whether the rule is stale for the slot's current `sensorN_type` is
/// not knowable from the rule alone (see [`Rule::stale`]).
pub fn validate_slot(rule: Option<&Rule>) -> Result<(), String> {
    let Some(r) = rule else {
        return Ok(());
    };
    if !r.lo.is_finite() || !r.hi.is_finite() || !r.dwell.is_finite() {
        return Err("lo/hi/dwell must be finite numbers".into());
    }
    r.validate().map(|_| ())
}

/// The firmware decoder's slot label: `mb`, `s1`..`s4`, else `slot{N}`.
pub fn slot_name(slot: u32) -> String {
    sensor_types::slot_label(slot)
}

/// `AlarmStatus.channel` / `AlarmEvent.channel` 255 on a 1-Wire slot: the whole
/// device stopped answering (`no_data`, sticker-firmware eff819ed).
pub const CHANNEL_WHOLE_DEVICE: u32 = 255;

/// The registry name of `(slot, sensor_type, channel)` (`temperature-aux`),
/// `device` for [`CHANNEL_WHOLE_DEVICE`], or `ch{N}` when the registry does not
/// know it, as `ttn.js` spells it.
pub fn channel_name(slot: u32, sensor_type: Option<u32>, channel: u32) -> String {
    if channel == CHANNEL_WHOLE_DEVICE && slot != 0 {
        return "device".to_string();
    }
    let known = u8::try_from(slot).ok().and_then(|s| {
        let t = sensor_type.and_then(|t| u8::try_from(t).ok());
        sensor_types::channel(s, t, u8::try_from(channel).ok()?)
    });
    known.map_or_else(|| format!("ch{channel}"), |c| c.name.to_string())
}

/// The Hub field (`node-firmware-v2.0.0.md` §1.1) of `(slot, sensor_type,
/// channel)`: `temperature`, `w1_2_temperature_aux`. `None` when the registry
/// does not know the channel.
pub fn channel_field(slot: u32, sensor_type: Option<u32>, channel: u32) -> Option<String> {
    let slot = u8::try_from(slot).ok()?;
    let t = sensor_type.and_then(|t| u8::try_from(t).ok());
    sensor_types::hub_field(slot, t, u8::try_from(channel).ok()?)
}

/// `AlarmEvent.type` / `AlarmStatus.type` by wire value, as the v2 radio and
/// `ttn.js` spell them (`none` past the table).
const ALARM_TYPES: &[&str] = &[
    "none",
    "low",
    "high",
    "trigger",
    "no_data",
    "sensor_mismatch",
];
/// `AlarmEvent.Type.NO_DATA`.
pub const ALARM_TYPE_NO_DATA: u32 = 4;
/// `AlarmEvent.Type.SENSOR_MISMATCH` (v2.0.0, #430).
pub const ALARM_TYPE_SENSOR_MISMATCH: u32 = 5;

/// The alarm type by name, `none` past the table (the v2 radio's
/// `alarm_type_label`).
pub fn alarm_type_label(kind: u32) -> &'static str {
    ALARM_TYPES.get(kind as usize).copied().unwrap_or("none")
}

/// The channel an alarm on `(slot, channel)` is about: the motherboard type on
/// slot 0, else the type the alarm names (any registry type, as `ttn.js`).
pub fn alarm_channel(
    slot: u32,
    sensor_type: Option<u32>,
    ch: u32,
) -> Option<&'static sensor_types::Channel> {
    let id = if slot == 0 {
        u32::from(sensor_types::MOTHERBOARD)
    } else {
        sensor_type?
    };
    sensor_types::sensor_type(u8::try_from(id).ok()?)?.channel(u8::try_from(ch).ok()?)
}

/// The target of an alarm, the same map the v2 radio's `alarm_target` builds
/// (`node-firmware-v2.0.0.md` §4): `slot` label, `channel` name (`null` for a
/// sensor mismatch, `device` for [`CHANNEL_WHOLE_DEVICE`], `ch{N}` when
/// unknown), `part` for a no-data on a 1-Wire chip, and on a 1-Wire slot the
/// `sensor_type` name.
pub fn alarm_target(
    slot: u32,
    ch: u32,
    sensor_type: Option<u32>,
    kind: u32,
) -> serde_json::Map<String, serde_json::Value> {
    use serde_json::Value;
    let mut o = serde_json::Map::new();
    o.insert("slot".into(), Value::from(sensor_types::slot_label(slot)));
    let c = alarm_channel(slot, sensor_type, ch);
    o.insert(
        "channel".into(),
        if kind == ALARM_TYPE_SENSOR_MISMATCH {
            Value::Null
        } else if ch == CHANNEL_WHOLE_DEVICE {
            Value::from("device")
        } else {
            Value::from(c.map_or_else(|| format!("ch{ch}"), |c| c.name.to_string()))
        },
    );
    if let (true, Some(part)) = (kind == ALARM_TYPE_NO_DATA, c.and_then(|c| c.part)) {
        o.insert("part".into(), Value::from(part));
    }
    if let (Some(t), true) = (sensor_type, slot != 0) {
        o.insert(
            "sensor_type".into(),
            Value::from(sensor_types::w1_type_label(t)),
        );
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The HIL rule of #119: enabled, s1 machine-probe temperature, lo 10,
    /// hi 30, dwell 0.
    const HIL_HEX: &str = "030100030000000020410000f04100000000";

    #[test]
    fn the_hil_rule_round_trips() {
        let r = decode_slot(HIL_HEX).unwrap().expect("present");
        assert!(r.enabled);
        assert_eq!((r.slot, r.channel, r.sensor_type), (1, 0, 3));
        assert_eq!((r.lo, r.hi, r.dwell), (10.0, 30.0, 0.0));
        assert!(validate_slot(Some(&r)).is_ok());
        assert_eq!(hex::encode(encode_slot(Some(&r))), HIL_HEX);
        assert_eq!(
            channel_field(1, Some(3), 0).as_deref(),
            Some("w1_1_temperature")
        );
    }

    #[test]
    fn an_empty_slot_is_all_zero_and_valid() {
        assert_eq!(decode_slot(&"00".repeat(SLOT_LEN)).unwrap(), None);
        assert_eq!(decode_slot("").unwrap(), None);
        assert_eq!(encode_slot(None), [0u8; SLOT_LEN]);
        assert!(validate_slot(None).is_ok());
    }

    #[test]
    fn rejects_bad_length_and_hex() {
        assert!(decode_slot("00").is_err());
        assert!(decode_slot("zz").is_err());
        // A v1.4 17-byte rule is not an 18-byte one.
        assert!(decode_slot("0300000000000070410000c8410000003f").is_err());
    }

    fn rule(slot: u8, channel: u8, sensor_type: u8) -> Rule {
        Rule {
            enabled: true,
            slot,
            channel,
            sensor_type,
            from_state: 0,
            to_state: 0,
            lo: 0.0,
            hi: 1.0,
            dwell: 0.0,
        }
    }

    #[test]
    fn validates_targets_through_the_registry() {
        let ok = |r: Rule| validate_slot(Some(&r)).is_ok();
        assert!(ok(rule(0, 0, 1))); // board temperature
        assert!(ok(rule(0, 2, 1))); // board pressure
        assert!(ok(rule(1, 0, 2))); // s1 dallas temperature
        assert!(ok(rule(2, 2, 3))); // s2 machine-probe temperature-aux
        assert!(ok(rule(3, 6, 3))); // s3 machine-probe accel-x
        assert!(ok(rule(0, 6, 1))); // hall-left-count (rate)
        assert!(!ok(rule(0, 20, 1)), "battery-voltage is watchdog-only");
        assert!(!ok(rule(0, 19, 1)), "orientation is kind none");
        assert!(!ok(rule(1, 2, 2)), "dallas has no channel 2");
        assert!(!ok(rule(1, 0, 1)), "a 1-Wire slot cannot hold the board");
        assert!(!ok(rule(0, 0, 3)), "slot 0 is always the board");
        assert!(!ok(rule(5, 0, 2)), "slot 5 does not exist");
    }

    #[test]
    fn rejects_an_empty_band_a_bad_dwell_and_non_finite_numbers() {
        let mut r = rule(0, 0, 1);
        r.lo = 25.0;
        r.hi = 15.0;
        assert!(validate_slot(Some(&r)).is_err());
        let mut r = rule(0, 0, 1);
        r.dwell = 3601.0;
        assert!(validate_slot(Some(&r)).is_err());
        let mut r = rule(0, 0, 1);
        r.hi = f32::INFINITY;
        assert!(validate_slot(Some(&r)).is_err());
        // v2.0.0 accepts a momentary state rule with from == to (any event fires).
        let r = rule(0, 15, 1);
        assert!(validate_slot(Some(&r)).is_ok());
    }

    #[test]
    fn names_follow_the_firmware_decoder() {
        assert_eq!(slot_name(0), "mb");
        assert_eq!(slot_name(4), "s4");
        assert_eq!(slot_name(9), "slot9");
        assert_eq!(channel_name(0, None, 4), "temperature-baro");
        assert_eq!(channel_name(2, Some(3), 2), "temperature-aux");
        assert_eq!(channel_name(2, Some(2), 2), "ch2");
        assert_eq!(channel_name(1, Some(3), CHANNEL_WHOLE_DEVICE), "device");
        assert_eq!(channel_field(0, None, 20).as_deref(), Some("voltage"));
        assert_eq!(channel_field(2, Some(3), 5).as_deref(), Some("w1_2_tilt"));
        assert_eq!(channel_field(2, None, 0), None);
    }
}
