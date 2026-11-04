//! Node (STICKER) config-key validation —
//! `proximos-v2/plan/v1/viewer/rest.md` §"LoRaWAN — device router"
//! (`STICKER_SETTABLE`/`STICKER_READ_ONLY`, `:376-377`; the write
//! endpoint's error shapes, `:391` — "422 per-key validation (type /
//! `out of range [min,max]` / unknown / read-only)"), cross-checked
//! against the device agent's own validator,
//! `application/src/libs/lorawan/sticker_command.rs` (`SETTABLE`
//! `:71-167`, `READ_ONLY` `:189-207`, `validate_one` `:289-333`) — the
//! code that actually runs before a downlink is ever sent.
//!
//! # Cross-check findings (agent `sticker_command.rs` vs. rest.md)
//!
//! - **`sensors.accel_motion_sensitivity` is not a `0..3` integer.**
//!   rest.md's prose lists it alongside the other numeric ranges as
//!   "(0..3)". The agent's `Kind::MotionEnum` (`sticker_command.rs:52-60,
//!   325-331`) only accepts the canonical enum spelling
//!   (`off`/`low`/`medium`/`high`, case-insensitive) — explicitly *not* a
//!   raw integer, because `decode_config` reads back
//!   `ConfigValue::Enum("MEDIUM")` and a numeric write would make the
//!   verify-after-write loop never converge (module doc,
//!   `sticker_command.rs:56-59`). **Per "application wins", this module
//!   validates it as [`KeySpec::Enum`]**, not `KeySpec::UintRange(0, 3)` —
//!   a variant this deliverable's brief didn't anticipate, added because
//!   the cross-check required it.
//! - `application.history_sensors` is a free `uint32` bitmask in the agent
//!   (`Kind::Bitmask`, only range-checked to `u32::MAX`, no `min`) —
//!   modeled here as `UintRange(0, u32::MAX)`, which validates
//!   identically.
//! - `alarms.alarm_light_confirm_delay` has `zero_ok: true` in the agent,
//!   but its range already starts at 0 (`0..=3600`), so the flag is inert
//!   — modeled here as a plain `UintRange(0, 3600)`, same shape as
//!   `alarms.alarm_limit`. `UintRangeOrZero` is reserved for
//!   `application.interval_sample`, the one key where 0 is a sentinel
//!   *outside* the normal range (`5..=3600`).
//! - `alarms.alarm_0..alarm_15` are validated here only structurally, as
//!   34-char hex strings (`HexStr(34)`, matching
//!   `sticker_alarm::SLOT_LEN == 17` bytes). The agent additionally
//!   decodes and semantically validates the packed alarm-rule bytes
//!   (`sticker_alarm::decode_slot` + `validate_slot`) — deliberately not
//!   duplicated here, to avoid a second copy of that rule logic drifting
//!   from the agent's; a value that passes this crate's check can still
//!   be rejected by the device.
//! - Every other `STICKER_SETTABLE` range/type and the full
//!   `STICKER_READ_ONLY` list (17 keys) agree between rest.md and the
//!   agent's `SETTABLE`/`READ_ONLY` tables.
//!
//! # Amended 2026-09-27 (sticker-firmware 7be011e)
//!
//! Lorawan 13/14/15 carry the firmware's current names,
//! `radio_link_check_interval`, `radio_link_check_fail_rejoin` and
//! `radio_mode` (the Hub's `control-radio` key table and `ttn.js` agree).
//! The v1 spellings (`link_check_*`, `mode`) stay in [`READ_ONLY`] for one
//! release, so a draft written under them is refused as read-only rather than
//! as unknown. The `p2p` group (`frequency`, `spreading_factor`, `tx_power`)
//! is read-only too: the firmware writes it over the shell only. 17 + 6 = 23.
//!
//! *Amended 2026-10-14 (v2.0.0):* the v1 spellings are dropped; they are
//! now unknown keys. `p2p.modulation` (lora/fsk, shell only) joins the
//! read-only set: 17 + 4 = 21. The boolean GP caps `cap_input_a/_b` and
//! `cap_pir_detector` are replaced by the enums `cap_gpio_mode` and
//! `cap_gpio_input_a/_b` (#499), 42 settable keys as before.
//!
//! # Amended 2026-10-09 (#119, sticker-firmware v2.0.0, 435273c6)
//!
//! The sensor channel model (`plan/control/radio/node-firmware-v2.0.0.md`
//! §2, §5) changes the settable set, 38 → 42:
//!
//! - `alarms.alarm_N` is an 18-byte rule addressed by
//!   `(slot, channel, sensor_type)`, so `HexStr(36)`; the agent decodes and
//!   validates it against the sensor type registry
//!   (`control_radio::decode::alarm_rule`), still not duplicated here.
//! - `application.history_sensors` is `reserved`; its successor
//!   `application.history_channels` is 24 bytes (`slot << 5 | channel`,
//!   `0xff` = unused), `HexStr(48)`.
//! - `alarms.alarm_notif_time` and `alarms.alarm_light_confirm_delay` left
//!   the proto with 7be011e and are gone from the set.
//! - New: `application.interval_announce` (hours, 1..168, 0 = off),
//!   `sensors.cap_sht`, and `sensors.sensor1..4_type`, the expected 1-Wire
//!   type of each slot (0 = any/empty). The type ids are the sensor type
//!   registry's (`control_radio::decode::sensor_types`), which this crate
//!   does not depend on, so they are range-checked to a byte here and the
//!   agent refuses an id the registry does not know.
//!
//! *Note 2026-10-12:* the keys v2.0.0 retires (`application.history_sensors`,
//! `alarms.alarm_notif_time`, `alarms.alarm_light_confirm_delay`) are refused
//! as an **unknown key**, not as read-only: they are in neither table. The
//! read-only grace of the 2026-09-27 amendment covers only the three v1
//! spellings of lorawan 13/14/15. The cross-check bullets above on
//! `history_sensors`, `alarm_light_confirm_delay` and the 17-byte
//! `HexStr(34)` rule describe the v1.4 set and are superseded by this section.

use serde_json::Value;

/// One key's validation shape. Mirrors the agent's private `Kind`
/// (`sticker_command.rs:40-61`), plus [`KeySpec::Enum`] — added here, see
/// module doc "Cross-check findings".
#[derive(Debug, Clone, Copy)]
pub enum KeySpec {
    Bool,
    /// Inclusive `[min, max]`.
    UintRange(u32, u32),
    /// Inclusive `[min, max]`, plus the sentinel `0` even when it falls
    /// outside that range (agent: `Kind::Uint { zero_ok: true, .. }`).
    UintRangeOrZero(u32, u32),
    /// Exactly `len` hex characters (case-insensitive); no semantic
    /// decode — see module doc.
    HexStr(usize),
    /// One of the given values, case-insensitive
    /// (agent: `sticker_command.rs::motion_value`).
    Enum(&'static [&'static str]),
}

/// `STICKER_SETTABLE` (rest.md `:376`) — 42 keys since sticker-firmware
/// v2.0.0 (module doc, amended 2026-10-09), cross-checked against the
/// agent's `SETTABLE` (`control-core` `node_command.rs`).
pub const SETTABLE: &[(&str, KeySpec)] = &[
    (
        "application.interval_sample",
        KeySpec::UintRangeOrZero(5, 3600),
    ),
    ("application.interval_report", KeySpec::UintRange(60, 86400)),
    ("application.history_enable", KeySpec::Bool),
    (
        "application.interval_announce",
        KeySpec::UintRangeOrZero(1, 168),
    ),
    ("application.history_channels", KeySpec::HexStr(48)),
    ("application.battery_level", KeySpec::UintRange(1000, 3600)),
    ("sensors.cap_hall_left", KeySpec::Bool),
    ("sensors.cap_hall_right", KeySpec::Bool),
    ("sensors.cap_light_sensor", KeySpec::Bool),
    ("sensors.cap_barometer", KeySpec::Bool),
    ("sensors.cap_w1_sensors", KeySpec::Bool),
    ("sensors.cap_accelerometer", KeySpec::Bool),
    ("sensors.cap_sht", KeySpec::Bool),
    // v2.0.0 (#499): the GP pins, in the firmware's shell spelling.
    (
        "sensors.cap_gpio_mode",
        KeySpec::Enum(&["none", "inputs", "pir", "buzzer"]),
    ),
    (
        "sensors.cap_gpio_input_a",
        KeySpec::Enum(&["none", "digital", "analog", "pulse"]),
    ),
    (
        "sensors.cap_gpio_input_b",
        KeySpec::Enum(&["none", "digital", "analog", "pulse"]),
    ),
    // Expected 1-Wire type per slot; the registry check is the agent's.
    ("sensors.sensor1_type", KeySpec::UintRange(0, 255)),
    ("sensors.sensor2_type", KeySpec::UintRange(0, 255)),
    ("sensors.sensor3_type", KeySpec::UintRange(0, 255)),
    ("sensors.sensor4_type", KeySpec::UintRange(0, 255)),
    // Cross-check: not a 0..3 integer — see module doc.
    (
        "sensors.accel_motion_sensitivity",
        KeySpec::Enum(&["off", "low", "medium", "high"]),
    ),
    ("sensors.hall_left_counter", KeySpec::Bool),
    ("sensors.hall_right_counter", KeySpec::Bool),
    ("sensors.input_a_counter", KeySpec::Bool),
    ("sensors.input_b_counter", KeySpec::Bool),
    ("alarms.alarm_limit", KeySpec::UintRange(0, 3600)),
    ("alarms.alarm_0", KeySpec::HexStr(36)),
    ("alarms.alarm_1", KeySpec::HexStr(36)),
    ("alarms.alarm_2", KeySpec::HexStr(36)),
    ("alarms.alarm_3", KeySpec::HexStr(36)),
    ("alarms.alarm_4", KeySpec::HexStr(36)),
    ("alarms.alarm_5", KeySpec::HexStr(36)),
    ("alarms.alarm_6", KeySpec::HexStr(36)),
    ("alarms.alarm_7", KeySpec::HexStr(36)),
    ("alarms.alarm_8", KeySpec::HexStr(36)),
    ("alarms.alarm_9", KeySpec::HexStr(36)),
    ("alarms.alarm_10", KeySpec::HexStr(36)),
    ("alarms.alarm_11", KeySpec::HexStr(36)),
    ("alarms.alarm_12", KeySpec::HexStr(36)),
    ("alarms.alarm_13", KeySpec::HexStr(36)),
    ("alarms.alarm_14", KeySpec::HexStr(36)),
    ("alarms.alarm_15", KeySpec::HexStr(36)),
];

/// `STICKER_READ_ONLY` (rest.md `:377`) — 17 keys, identical to the
/// agent's `READ_ONLY` (`sticker_command.rs:189-207`); no disagreement
/// found. Amended 2026-09-27 (module doc): the 7be011e names of lorawan
/// 13/14/15 and the `p2p` group. Amended 2026-10-14 (v2.0.0): the v1 names
/// are gone, `p2p.modulation` is new.
pub const READ_ONLY: &[&str] = &[
    "lorawan.region",
    "lorawan.sub_band",
    "lorawan.network",
    "lorawan.adr",
    "lorawan.activation",
    "lorawan.deveui",
    "lorawan.joineui",
    "lorawan.devaddr",
    "lorawan.radio_link_check_interval",
    "lorawan.radio_link_check_fail_rejoin",
    "lorawan.radio_mode",
    "application.calibration",
    "application.vendor_reset_allow",
    "sensors.sensor1_rom",
    "sensors.sensor2_rom",
    "sensors.sensor3_rom",
    "sensors.sensor4_rom",
    "p2p.frequency",
    "p2p.spreading_factor",
    "p2p.tx_power",
    "p2p.modulation",
];

fn spec(key: &str) -> Option<KeySpec> {
    SETTABLE.iter().find(|(k, _)| *k == key).map(|(_, s)| *s)
}

fn is_read_only(key: &str) -> bool {
    READ_ONLY.contains(&key)
}

/// One key that failed validation, v1-style (rest.md: "type /
/// `out of range [min,max]` / unknown / read-only").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyError {
    pub key: String,
    pub reason: String,
}

fn as_u32(v: &Value) -> Option<u32> {
    v.as_u64().and_then(|n| u32::try_from(n).ok())
}

fn validate_one(spec: KeySpec, value: &Value) -> Result<(), String> {
    match spec {
        KeySpec::Bool => {
            if value.is_boolean() {
                Ok(())
            } else {
                Err("expected a boolean".to_string())
            }
        }
        KeySpec::UintRange(min, max) => match as_u32(value) {
            Some(n) if n >= min && n <= max => Ok(()),
            Some(n) => Err(format!("out of range [{min},{max}], got {n}")),
            None => Err("expected an unsigned integer".to_string()),
        },
        KeySpec::UintRangeOrZero(min, max) => match as_u32(value) {
            Some(0) => Ok(()),
            Some(n) if n >= min && n <= max => Ok(()),
            Some(n) => Err(format!("out of range [{min},{max}] or 0, got {n}")),
            None => Err("expected an unsigned integer".to_string()),
        },
        KeySpec::HexStr(len) => match value.as_str() {
            Some(s) if s.len() == len && s.chars().all(|c| c.is_ascii_hexdigit()) => Ok(()),
            Some(s) if s.len() != len => Err(format!(
                "expected a {len}-char hex string, got {} chars",
                s.len()
            )),
            Some(_) => Err(format!("expected a {len}-char hex string")),
            None => Err(format!("expected a {len}-char hex string")),
        },
        KeySpec::Enum(options) => match value.as_str() {
            Some(s) if options.iter().any(|o| o.eq_ignore_ascii_case(s.trim())) => Ok(()),
            Some(s) => Err(format!("expected one of {options:?}, got {s:?}")),
            None => Err(format!("expected one of {options:?}")),
        },
    }
}

/// Validate a `NodeDeviceConfigUpdate.config` map (rest.md `:373`) against
/// [`SETTABLE`]/[`READ_ONLY`]. Returns every failing key at once (rest.md:
/// "422 per-key validation"), not just the first.
pub fn validate_sticker_config(
    config: &serde_json::Map<String, Value>,
) -> Result<(), Vec<KeyError>> {
    let mut errors = Vec::new();
    for (key, value) in config {
        let reason = match spec(key) {
            Some(s) => validate_one(s, value).err(),
            None if is_read_only(key) => Some("read-only key".to_string()),
            None => Some("unknown key".to_string()),
        };
        if let Some(reason) = reason {
            errors.push(KeyError {
                key: key.clone(),
                reason,
            });
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(pairs: &[(&str, Value)]) -> serde_json::Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn settable_has_42_entries() {
        assert_eq!(SETTABLE.len(), 42, "rest.md: STICKER_SETTABLE (v2.0.0)");
    }

    #[test]
    fn read_only_has_21_entries() {
        assert_eq!(
            READ_ONLY.len(),
            21,
            "rest.md's 17 plus the p2p group (v2.0.0)"
        );
    }

    /// Sticker-firmware 7be011e: the renamed lorawan keys and
    /// the shell-only P2P radio tuning are all refused as read-only, never as
    /// unknown — a UI greys a read-only key out and flags an unknown one.
    #[test]
    fn the_7be011e_names_and_the_p2p_group_are_read_only() {
        for key in [
            "lorawan.radio_link_check_interval",
            "lorawan.radio_link_check_fail_rejoin",
            "p2p.frequency",
            "p2p.modulation",
            "p2p.spreading_factor",
            "p2p.tx_power",
        ] {
            let err = validate_sticker_config(&cfg(&[(key, json!(5))])).unwrap_err();
            assert_eq!(err[0].reason, "read-only key", "{key}");
        }
    }

    /// v2.0.0: the v1 lorawan spellings and the old boolean GP caps are
    /// unknown keys; the GP enums take the firmware's shell names only.
    #[test]
    fn v2_0_0_drops_the_v1_names_and_takes_the_gp_enums() {
        for key in [
            "lorawan.link_check_interval",
            "lorawan.mode",
            "sensors.cap_input_a",
            "sensors.cap_pir_detector",
        ] {
            let err = validate_sticker_config(&cfg(&[(key, json!(true))])).unwrap_err();
            assert_eq!(err[0].reason, "unknown key", "{key}");
        }
        assert!(validate_sticker_config(&cfg(&[("sensors.cap_gpio_mode", json!("pir"))])).is_ok());
        assert!(
            validate_sticker_config(&cfg(&[("sensors.cap_gpio_input_b", json!("pulse"))])).is_ok()
        );
        assert!(
            validate_sticker_config(&cfg(&[("sensors.cap_gpio_input_a", json!("inputs"))]))
                .is_err()
        );
    }

    #[test]
    fn no_key_is_both_settable_and_read_only() {
        for (k, _) in SETTABLE {
            assert!(
                !is_read_only(k),
                "{k} listed as both settable and read-only"
            );
        }
    }

    #[test]
    fn no_duplicate_settable_keys() {
        let mut seen = std::collections::HashSet::new();
        for (k, _) in SETTABLE {
            assert!(seen.insert(*k), "duplicate: {k}");
        }
    }

    #[test]
    fn valid_config_accepted() {
        let c = cfg(&[
            ("application.interval_sample", json!(0)),
            ("application.interval_report", json!(300)),
            ("application.history_enable", json!(true)),
            ("sensors.cap_hall_left", json!(false)),
            ("sensors.accel_motion_sensitivity", json!("medium")),
            ("alarms.alarm_limit", json!(60)),
            (
                "alarms.alarm_0",
                json!("030100030000000020410000f04100000000"),
            ),
            ("application.interval_announce", json!(24)),
            (
                "application.history_channels",
                json!(format!("0001{}", "ff".repeat(22))),
            ),
            ("sensors.cap_sht", json!(true)),
            ("sensors.sensor1_type", json!(3)),
        ]);
        assert!(validate_sticker_config(&c).is_ok());
    }

    #[test]
    fn interval_sample_allows_zero_sentinel_outside_range() {
        let c = cfg(&[("application.interval_sample", json!(0))]);
        assert!(validate_sticker_config(&c).is_ok());
    }

    #[test]
    fn interval_sample_rejects_below_min_nonzero() {
        let c = cfg(&[("application.interval_sample", json!(4))]);
        let err = validate_sticker_config(&c).unwrap_err();
        assert_eq!(err[0].reason, "out of range [5,3600] or 0, got 4");
    }

    #[test]
    fn unknown_key_reason() {
        let c = cfg(&[("not.a.key", json!(1))]);
        let err = validate_sticker_config(&c).unwrap_err();
        assert_eq!(err[0].reason, "unknown key");
    }

    #[test]
    fn read_only_key_reason() {
        let c = cfg(&[("lorawan.region", json!("EU868"))]);
        let err = validate_sticker_config(&c).unwrap_err();
        assert_eq!(err[0].reason, "read-only key");
    }

    #[test]
    fn out_of_range_reason_format() {
        let c = cfg(&[("alarms.alarm_limit", json!(3601))]);
        let err = validate_sticker_config(&c).unwrap_err();
        assert_eq!(err[0].reason, "out of range [0,3600], got 3601");
    }

    #[test]
    fn bool_type_error() {
        let c = cfg(&[("application.history_enable", json!("yes"))]);
        let err = validate_sticker_config(&c).unwrap_err();
        assert_eq!(err[0].reason, "expected a boolean");
    }

    /// Cross-check regression: this must NOT be validated as `0..3` — see
    /// module doc "Cross-check findings".
    #[test]
    fn accel_motion_sensitivity_rejects_numeric_write() {
        let c = cfg(&[("sensors.accel_motion_sensitivity", json!(2))]);
        let err = validate_sticker_config(&c).unwrap_err();
        assert_eq!(err[0].key, "sensors.accel_motion_sensitivity");
    }

    #[test]
    fn accel_motion_sensitivity_accepts_case_insensitive_enum() {
        for v in ["off", "LOW", "Medium", "HIGH"] {
            let c = cfg(&[("sensors.accel_motion_sensitivity", json!(v))]);
            assert!(validate_sticker_config(&c).is_ok(), "{v}");
        }
    }

    #[test]
    fn accel_motion_sensitivity_rejects_unknown_variant() {
        let c = cfg(&[("sensors.accel_motion_sensitivity", json!("extreme"))]);
        assert!(validate_sticker_config(&c).is_err());
    }

    #[test]
    fn alarm_slot_requires_exactly_36_hex_chars() {
        let too_short = cfg(&[("alarms.alarm_0", json!("aabb"))]);
        let err = validate_sticker_config(&too_short).unwrap_err();
        assert_eq!(err[0].reason, "expected a 36-char hex string, got 4 chars");

        // A v1.4 17-byte rule is refused by length.
        let v14 = cfg(&[(
            "alarms.alarm_0",
            json!("0300000000000070410000c8410000003f"),
        )]);
        assert!(validate_sticker_config(&v14).is_err());

        let not_hex = cfg(&[("alarms.alarm_0", json!(format!("zz{}", "1".repeat(34))))]);
        assert!(validate_sticker_config(&not_hex).is_err());

        // The #119 HIL rule: s1 machine-probe temperature, 10..30.
        let valid = cfg(&[(
            "alarms.alarm_0",
            json!("030100030000000020410000f04100000000"),
        )]);
        assert!(validate_sticker_config(&valid).is_ok());
    }

    #[test]
    fn v200_keys_replace_the_retired_ones() {
        for gone in [
            "application.history_sensors",
            "alarms.alarm_notif_time",
            "alarms.alarm_light_confirm_delay",
        ] {
            let c = cfg(&[(gone, json!(1))]);
            let err = validate_sticker_config(&c).unwrap_err();
            assert_eq!(err[0].reason, "unknown key", "{gone}");
        }
        let short = cfg(&[("application.history_channels", json!("0001"))]);
        assert!(validate_sticker_config(&short).is_err());
        let off = cfg(&[("application.interval_announce", json!(0))]);
        assert!(validate_sticker_config(&off).is_ok());
        let week = cfg(&[("application.interval_announce", json!(169))]);
        assert!(validate_sticker_config(&week).is_err());
        let ty = cfg(&[("sensors.sensor4_type", json!(256))]);
        assert!(validate_sticker_config(&ty).is_err());
    }

    #[test]
    fn multiple_errors_reported_together() {
        let c = cfg(&[("bad.key", json!(1)), ("lorawan.region", json!("x"))]);
        let err = validate_sticker_config(&c).unwrap_err();
        assert_eq!(err.len(), 2);
    }
}
