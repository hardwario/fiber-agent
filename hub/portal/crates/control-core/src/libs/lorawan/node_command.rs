//! Encoding side of the NODE fPort-85 protocol: build `Command` downlinks
//! and the remote-configuration engine (#68).
//!
//! The config-write API takes the same flat `group.field` key space that
//! `node_response::decode_config` produces (#70), so a desired config can be
//! written here and verified with `diff_config` against a `GetParam` read-back.
//!
//! `Command`s are emitted with `seq = 0`; the live sender
//! (`LoRaWANHandle::send_command`) stamps the real sequence number and awaits
//! the correlated `Response` (Ack on success, Error on validation failure).
//!
//! Scope: the `Application` group (sampling/reporting/history) plus the two
//! scalar `Alarms` fields. The `Lorawan` group is intentionally excluded —
//! changing region/activation/keys can permanently disconnect the device and
//! needs a separate guarded flow (#35).

use std::collections::BTreeMap;

use prost::Message;

use control_radio::decode::history::history_column;
use control_radio::decode::sensor_types;

use super::node_alarm;
use super::node_proto::app_config_message::{Alarms, Application, Sensors};
use super::node_proto::{command, Command};
use super::node_response::ConfigValue;

/// EU868 DR0 (SF12) maximum downlink application payload, in bytes. The NODE
/// receives the raw `Command` protobuf on fPort 85 (no proto-version prefix on
/// downlinks), so the whole encoded `Command` must fit this budget at the
/// worst-case data rate.
pub const DR0_COMMAND_BUDGET: usize = 51;

/// A field that failed server-side validation before anything was sent
/// (fail-fast — never burn airtime on a value the firmware would reject).
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigError {
    pub key: String,
    pub reason: String,
}

#[derive(Clone, Copy)]
enum Kind {
    Bool,
    /// inclusive `[min, max]`; `zero_ok` allows the sentinel 0 outside the range.
    Uint {
        min: u64,
        max: u64,
        zero_ok: bool,
    },
    /// A 36-char hex alarm slot (18 bytes, sticker-firmware v2.0.0); validated
    /// by decoding to a rule and checking it against the sensor type registry.
    AlarmHex,
    /// `application.history_channels` (v2.0.0): 24 bytes of `slot << 5 | ch`,
    /// `0xff` = unused, carried as 48 hex chars (see [`history_channels_hex`]).
    HistoryChannels,
    /// `sensors.sensorN_type` (v2.0.0): 0 (none) or a 1-Wire type id of the
    /// sensor type registry.
    W1Type,
    /// `sensors.accel_motion_sensitivity` (off/low/medium/high = 0..3).
    ///
    /// Its own kind rather than a `Uint{0,3}` because reads and writes must agree
    /// on the *representation*, not just the range: `decode_config` emits
    /// `ConfigValue::Enum("MEDIUM")`, so a write that stored `Uint(2)` would make
    /// `diff_config` report a permanent mismatch and the verification loop would
    /// never converge. Everything normalises to `Enum(motion_name(n))`.
    MotionEnum,
    /// A proto enum written by name (sticker-firmware v2.0.0 #498:
    /// `cap_gpio_mode`, `cap_gpio_input_a/_b`): the proto spellings in value
    /// order, normalised to the upper-case name reads emit, like [`Kind::MotionEnum`].
    /// `alias` is the shell spelling of value 0 when it differs (`none` for
    /// `GpioInput.DISABLED`).
    NamedEnum {
        names: &'static [&'static str],
        alias0: Option<&'static str>,
    },
}

/// `Sensors.GpioMode` (sticker-firmware v2.0.0, #498).
pub(crate) const GPIO_MODE_NAMES: &[&str] = &["NONE", "INPUTS", "PIR", "BUZZER"];
/// `Sensors.GpioInput`; the shell calls `DISABLED` `none`.
pub(crate) const GPIO_INPUT_NAMES: &[&str] = &["DISABLED", "DIGITAL", "ANALOG", "PULSE"];
const GPIO_MODE: Kind = Kind::NamedEnum {
    names: GPIO_MODE_NAMES,
    alias0: None,
};
const GPIO_INPUT: Kind = Kind::NamedEnum {
    names: GPIO_INPUT_NAMES,
    alias0: Some("NONE"),
};

/// The proto value of `name` in a [`Kind::NamedEnum`], either case.
fn named_value(names: &[&str], alias0: Option<&str>, name: &str) -> Option<i32> {
    let up = name.trim().to_ascii_uppercase();
    if alias0 == Some(up.as_str()) {
        return Some(0);
    }
    names.iter().position(|n| *n == up).map(|i| i as i32)
}

/// Settable `group.field` → (proto field number within its group, validation).
/// Ranges mirror `app_config.yml` @ the live `origin/v1.4.0` branch (HEAD
/// 6eb111b), which is the single source of truth: `configen` generates
/// `app_config.proto` from it, so a parameter's yml `proto_id` IS its protobuf
/// wire field number within its group submessage (e.g. interval_report = 3).
/// NOTE: the frozen `v1.4.0` *tag* (daec406) predates the #166/#174 field
/// recompaction and uses different numbers (interval_report = 4, history_enable
/// = 49, …); our proto tracks the branch, matching the bench device.
const SETTABLE: &[(&str, u32, Kind)] = &[
    (
        "application.interval_sample",
        2,
        Kind::Uint {
            min: 5,
            max: 3600,
            zero_ok: true,
        },
    ),
    (
        "application.interval_report",
        3,
        Kind::Uint {
            min: 60,
            max: 86400,
            zero_ok: false,
        },
    ),
    ("application.history_enable", 4, Kind::Bool),
    // `application.history_sensors` (5) is `reserved` since sticker-firmware
    // v2.0.0 (#430); `history_channels` (9) names the recorded columns instead.
    (
        "application.battery_level",
        6,
        Kind::Uint {
            min: 1000,
            max: 3600,
            zero_ok: false,
        },
    ),
    // Periodic Info + settings-info re-announce, hours (#445, v2.0.0).
    (
        "application.interval_announce",
        8,
        Kind::Uint {
            min: 1,
            max: 168,
            zero_ok: true,
        },
    ),
    ("application.history_channels", 9, Kind::HistoryChannels),
    // Sensors group. The firmware applies these over LoRaWAN without a transport
    // gate (`app_config_apply_sensors` is ARG_UNUSED(tp)), so they are genuinely
    // writable over the radio — unlike the `lorawan.*` group, which the firmware
    // rejects with NOT_WRITABLE.
    ("sensors.cap_hall_left", 1, Kind::Bool),
    ("sensors.cap_hall_right", 2, Kind::Bool),
    ("sensors.cap_light_sensor", 5, Kind::Bool),
    ("sensors.cap_barometer", 6, Kind::Bool),
    ("sensors.cap_w1_sensors", 8, Kind::Bool),
    ("sensors.cap_accelerometer", 9, Kind::Bool),
    ("sensors.accel_motion_sensitivity", 10, Kind::MotionEnum),
    ("sensors.hall_left_counter", 15, Kind::Bool),
    ("sensors.hall_right_counter", 16, Kind::Bool),
    ("sensors.input_a_counter", 17, Kind::Bool),
    ("sensors.input_b_counter", 18, Kind::Bool),
    // sticker-firmware v2.0.0: the onboard SHT4x (#465) and the configured
    // sensor type of each 1-Wire slot (#430). While a ROM is bound the Node
    // re-derives the type from it at boot, so a retype also needs the ROM
    // cleared, which stays a bench operation (`sensorN_rom` is read-only here).
    ("sensors.cap_sht", 22, Kind::Bool),
    ("sensors.sensor1_type", 23, Kind::W1Type),
    ("sensors.sensor2_type", 24, Kind::W1Type),
    ("sensors.sensor3_type", 25, Kind::W1Type),
    ("sensors.sensor4_type", 26, Kind::W1Type),
    // sticker-firmware v2.0.0 #498: the GP pin owner and the mode of each
    // input. They replace cap_input_a/_b (3/4), cap_pir_detector (7),
    // cap_buzzer (19) and cap_analog_a/_b (20/21), all `reserved` now.
    ("sensors.cap_gpio_mode", 27, GPIO_MODE),
    ("sensors.cap_gpio_input_a", 28, GPIO_INPUT),
    ("sensors.cap_gpio_input_b", 29, GPIO_INPUT),
    (
        "alarms.alarm_limit",
        1,
        Kind::Uint {
            min: 0,
            max: 3600,
            zero_ok: false,
        },
    ),
    // `alarms.alarm_notif_time` (2) and `alarms.alarm_light_confirm_delay` (19)
    // were here until the proto was refreshed from sticker-firmware 7be011e
    // (2026-09-27): v2.0.0 has both `reserved`, so a write of either was a no-op.
    // Alarm rule slots: 18-byte packed rules (v2.0.0, #430), sent/read as 36-char
    // hex. proto field = 3 + N (alarm_0 = 3 … alarm_15 = 18); validated by
    // decoding and checking the target against the sensor type registry.
    ("alarms.alarm_0", 3, Kind::AlarmHex),
    ("alarms.alarm_1", 4, Kind::AlarmHex),
    ("alarms.alarm_2", 5, Kind::AlarmHex),
    ("alarms.alarm_3", 6, Kind::AlarmHex),
    ("alarms.alarm_4", 7, Kind::AlarmHex),
    ("alarms.alarm_5", 8, Kind::AlarmHex),
    ("alarms.alarm_6", 9, Kind::AlarmHex),
    ("alarms.alarm_7", 10, Kind::AlarmHex),
    ("alarms.alarm_8", 11, Kind::AlarmHex),
    ("alarms.alarm_9", 12, Kind::AlarmHex),
    ("alarms.alarm_10", 13, Kind::AlarmHex),
    ("alarms.alarm_11", 14, Kind::AlarmHex),
    ("alarms.alarm_12", 15, Kind::AlarmHex),
    ("alarms.alarm_13", 16, Kind::AlarmHex),
    ("alarms.alarm_14", 17, Kind::AlarmHex),
    ("alarms.alarm_15", 18, Kind::AlarmHex),
    // Alarm frames confirmed on both radios (sticker-firmware 3edb4a4, #460 T2c:
    // `writable: [shell, nfc, radio]`, default false = alarms go unconfirmed).
    ("alarms.radio_alarm_ack", 21, Kind::Bool),
];

/// Readable over LoRaWAN but never written from here, so the viewer can display a
/// node's full non-secret configuration without offering to change it.
///
/// The reasons differ per entry and the distinction matters, because a future
/// reader might otherwise "fix" a deliberate policy choice:
///
///   * `lorawan.*` — the FIRMWARE refuses these over the radio (`writable:
///     [shell, nfc]` in app_config.yml → `-EACCES` → `Error{NOT_WRITABLE}`).
///     Not our choice, and not changeable from here. The four session keys
///     (nwkkey/appkey/nwkskey/appskey) are absent even from this list: the device
///     never returns them over LoRaWAN at all.
///   * `sensors.sensorN_rom` — the firmware WOULD accept these (the sensors group
///     has no transport gate). Excluded as FIBER policy: a ROM is per-device
///     identity, normally learned from a `w1_scan`, and a wrong value silently
///     blinds a slot with no error.
///   * `application.calibration` — writable, but setting it reboots the device
///     into calibration mode. That is a bench operation, not a remote one.
///   * `application.vendor_reset_allow` — writable, but setting it false can
///     strand a device whose secret_key was rotated and lost; only a J-Link erase
///     recovers one.
///   * `p2p.*` — the P2P radio tuning (sticker-firmware 7be011e): the FIRMWARE
///     takes it over the shell only (`writable: [shell]`); a downlink on either
///     radio would sever the link carrying it.
const READ_ONLY: &[(&str, u32)] = &[
    ("lorawan.region", 1),
    ("lorawan.sub_band", 2),
    ("lorawan.network", 3),
    ("lorawan.adr", 4),
    ("lorawan.activation", 5),
    ("lorawan.deveui", 6),
    ("lorawan.joineui", 7),
    ("lorawan.devaddr", 10),
    ("lorawan.radio_link_check_interval", 13),
    ("lorawan.radio_link_check_fail_rejoin", 14),
    ("lorawan.radio_mode", 15),
    ("application.calibration", 1),
    ("application.vendor_reset_allow", 7),
    ("sensors.sensor1_rom", 11),
    ("sensors.sensor2_rom", 12),
    ("sensors.sensor3_rom", 13),
    ("sensors.sensor4_rom", 14),
    ("p2p.frequency", 1),
    ("p2p.spreading_factor", 2),
    ("p2p.tx_power", 3),
    ("p2p.modulation", 4),
];

fn spec(key: &str) -> Option<(u32, Kind)> {
    SETTABLE
        .iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, f, k)| (*f, *k))
}

/// The proto field number for any key we can read — settable or read-only.
fn field_number(key: &str) -> Option<u32> {
    spec(key)
        .map(|(f, _)| f)
        .or_else(|| READ_ONLY.iter().find(|(k, _)| *k == key).map(|(_, f)| *f))
}

/// True when a key is readable but deliberately not writable from here.
fn is_read_only(key: &str) -> bool {
    READ_ONLY.iter().any(|(k, _)| *k == key)
}

/// Proto group ids, as the firmware uses them when reporting a fault. Also the
/// dispatch order `set_param` applies groups in, so a fault names the first
/// group that failed. `p2p` is 5 (sticker-firmware 7be011e).
const GROUPS: &[(&str, u32)] = &[
    ("lorawan", 1),
    ("application", 2),
    ("sensors", 3),
    ("alarms", 4),
    ("p2p", 5),
];

/// The group id for a `group.field` key, or `None` for an unknown prefix.
fn group_id(key: &str) -> Option<u32> {
    let prefix = key.split('.').next()?;
    GROUPS.iter().find(|(g, _)| *g == prefix).map(|(_, id)| *id)
}

/// Map an `Error.fault_field` back to the key the engine sent, so the UI/API can
/// name the offending parameter.
///
/// Firmware v1.4.0 encodes `fault_field = group * 100 + tag` (`app_cmd.c:332`,
/// groups per `GROUPS` above), which makes a tag unambiguous across groups —
/// `application.interval_report` faults report 203, not 3. Matching the bare tag
/// (as this did before) therefore never resolved a real v1.4.0 fault, and every
/// rejected write lost the name of the field that caused it.
///
/// Two special cases:
///   * `tag == 0` with a group means "this group, no specific field" — the alarm
///     rule reload reports 400 when a rule fails validation (`app_cmd.c:343`).
///   * `fault_field == 0` is "no fault field set" (the firmware default).
///
/// *Amended 2026-10-14 (v2.0.0 minimum):* a bare tag (`group == 0`, firmware
/// before v1.4.0) is no longer resolved; it names nothing.
pub fn describe_fault<'a>(
    fault_field: u32,
    sent_keys: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    if fault_field == 0 {
        return None; // firmware default: the error carries no field reference
    }
    let (group, tag) = (fault_field / 100, fault_field % 100);
    let group_name = GROUPS.iter().find(|(_, id)| *id == group).map(|(g, _)| *g);
    if tag == 0 {
        return group_name.map(|g| format!("{g} (whole group)"));
    }
    sent_keys
        .into_iter()
        .find(|k| group_id(k) == Some(group) && spec(k).map(|(f, _)| f) == Some(tag))
        .map(|k| k.to_string())
        // The field is not one we sent (or not settable at all) — still name the
        // group and tag rather than silently returning nothing.
        .or_else(|| group_name.map(|g| format!("{g}.<field {tag}>")))
}

/// Validate one value against its field spec.
fn validate_one(key: &str, v: &ConfigValue) -> Result<(), ConfigError> {
    let err = |reason: String| ConfigError {
        key: key.to_string(),
        reason,
    };
    // A read-only key gets its OWN reason, distinct from an unknown one, so the UI
    // can grey the field out instead of flagging it as a typo.
    let (_, kind) = spec(key).ok_or_else(|| {
        if is_read_only(key) {
            err("read-only over LoRaWAN".into())
        } else {
            err("not a remotely settable parameter".into())
        }
    })?;
    match (kind, v) {
        (Kind::Bool, ConfigValue::Bool(_)) => Ok(()),
        (Kind::Bool, _) => Err(err("expected a boolean".into())),
        (Kind::W1Type, ConfigValue::Uint(0)) => Ok(()),
        (Kind::W1Type, ConfigValue::Uint(n)) => {
            let known = u8::try_from(*n)
                .ok()
                .and_then(sensor_types::sensor_type)
                .is_some_and(|t| t.is_w1());
            if known {
                Ok(())
            } else {
                Err(err(format!("{n} is not a 1-Wire sensor type (0 = none)")))
            }
        }
        (Kind::W1Type, _) => Err(err("expected a sensor type id".into())),
        (Kind::HistoryChannels, ConfigValue::Hex(s)) => {
            check_history_channels(s).map_err(|e| err(e))
        }
        (Kind::HistoryChannels, _) => Err(err("expected hex history columns".into())),
        (Kind::Uint { min, max, zero_ok }, ConfigValue::Uint(n)) => {
            if (zero_ok && *n == 0) || (*n >= min && *n <= max) {
                Ok(())
            } else {
                let z = if zero_ok { " (or 0)" } else { "" };
                Err(err(format!("out of range {min}..={max}{z}, got {n}")))
            }
        }
        (Kind::Uint { .. }, _) => Err(err("expected an unsigned integer".into())),
        (Kind::AlarmHex, ConfigValue::Hex(s)) => {
            if s.len() != 2 * node_alarm::SLOT_LEN {
                return Err(err(format!(
                    "expected {} hex chars ({} bytes), got {}",
                    2 * node_alarm::SLOT_LEN,
                    node_alarm::SLOT_LEN,
                    s.len()
                )));
            }
            let rule = node_alarm::decode_slot(s).map_err(|e| err(e))?;
            node_alarm::validate_slot(rule.as_ref()).map_err(|e| err(e))
        }
        (Kind::AlarmHex, _) => Err(err("expected a hex alarm slot".into())),
        // Accepted only in the canonical Enum form that reads emit, so a write and
        // its read-back compare equal.
        (Kind::MotionEnum, ConfigValue::Enum(s)) if motion_value(s).is_some() => Ok(()),
        (Kind::MotionEnum, ConfigValue::Enum(s)) => {
            Err(err(format!("expected off/low/medium/high, got {s:?}")))
        }
        (Kind::MotionEnum, _) => Err(err(
            "expected a motion sensitivity (off/low/medium/high)".into()
        )),
        (Kind::NamedEnum { names, alias0 }, ConfigValue::Enum(s))
            if named_value(names, alias0, s).is_some() =>
        {
            Ok(())
        }
        (Kind::NamedEnum { names, .. }, _) => Err(err(format!(
            "expected one of {}",
            names.join("/").to_ascii_lowercase()
        ))),
    }
}

/// Map a motion-sensitivity name to its proto value. The inverse of
/// `node_response::motion_name`, accepting either case so an operator can type
/// `medium` while reads emit `MEDIUM`.
fn motion_value(s: &str) -> Option<i32> {
    match s.trim().to_ascii_uppercase().as_str() {
        "OFF" => Some(0),
        "LOW" => Some(1),
        "MEDIUM" => Some(2),
        "HIGH" => Some(3),
        _ => None,
    }
}

/// Canonical `ConfigValue` for a motion-sensitivity proto value — the same spelling
/// `decode_config` produces, which is what makes `diff_config` converge.
fn motion_config_value(n: i32) -> ConfigValue {
    ConfigValue::Enum(
        match n {
            0 => "OFF",
            1 => "LOW",
            2 => "MEDIUM",
            3 => "HIGH",
            _ => "unknown",
        }
        .to_string(),
    )
}

/// Validate every key in a desired config. Returns all errors at once (so the
/// UI can show them together) — or the ordered list of validated settable
/// fields, in `SETTABLE` order for deterministic batching.
pub fn validate(
    config: &BTreeMap<String, ConfigValue>,
) -> Result<Vec<(String, ConfigValue)>, Vec<ConfigError>> {
    let mut errors = Vec::new();
    for (k, v) in config {
        if let Err(e) = validate_one(k, v) {
            errors.push(e);
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    // emit in canonical SETTABLE order
    let mut out = Vec::new();
    for (k, _, _) in SETTABLE {
        if let Some(v) = config.get(*k) {
            out.push((k.to_string(), v.clone()));
        }
    }
    Ok(out)
}

/// Set one validated `group.field` value onto a `SetParam`, creating the group
/// submessage on demand.
fn apply(sp: &mut command::SetParam, key: &str, v: &ConfigValue) {
    let group = key.split('.').next().unwrap_or("");
    match (group, key, v) {
        ("application", "application.interval_sample", ConfigValue::Uint(n)) => {
            sp.application
                .get_or_insert_with(Application::default)
                .interval_sample = Some(*n as u32);
        }
        ("application", "application.interval_report", ConfigValue::Uint(n)) => {
            sp.application
                .get_or_insert_with(Application::default)
                .interval_report = Some(*n as u32);
        }
        ("application", "application.history_enable", ConfigValue::Bool(b)) => {
            sp.application
                .get_or_insert_with(Application::default)
                .history_enable = Some(*b);
        }
        ("application", "application.interval_announce", ConfigValue::Uint(n)) => {
            sp.application
                .get_or_insert_with(Application::default)
                .interval_announce = Some(*n as u32);
        }
        ("application", "application.history_channels", ConfigValue::Hex(s)) => {
            if let Ok(bytes) = hex::decode(s) {
                sp.application
                    .get_or_insert_with(Application::default)
                    .history_channels = Some(bytes);
            }
        }
        ("application", "application.battery_level", ConfigValue::Uint(n)) => {
            sp.application
                .get_or_insert_with(Application::default)
                .battery_level = Some(*n as u32);
        }
        ("sensors", k, v) => {
            let s = sp.sensors.get_or_insert_with(Sensors::default);
            match (k, v) {
                ("sensors.cap_hall_left", ConfigValue::Bool(b)) => s.cap_hall_left = Some(*b),
                ("sensors.cap_hall_right", ConfigValue::Bool(b)) => s.cap_hall_right = Some(*b),
                ("sensors.cap_light_sensor", ConfigValue::Bool(b)) => s.cap_light_sensor = Some(*b),
                ("sensors.cap_barometer", ConfigValue::Bool(b)) => s.cap_barometer = Some(*b),
                ("sensors.cap_w1_sensors", ConfigValue::Bool(b)) => s.cap_w1_sensors = Some(*b),
                ("sensors.cap_accelerometer", ConfigValue::Bool(b)) => {
                    s.cap_accelerometer = Some(*b)
                }
                ("sensors.hall_left_counter", ConfigValue::Bool(b)) => {
                    s.hall_left_counter = Some(*b)
                }
                ("sensors.hall_right_counter", ConfigValue::Bool(b)) => {
                    s.hall_right_counter = Some(*b)
                }
                ("sensors.input_a_counter", ConfigValue::Bool(b)) => s.input_a_counter = Some(*b),
                ("sensors.input_b_counter", ConfigValue::Bool(b)) => s.input_b_counter = Some(*b),
                ("sensors.cap_sht", ConfigValue::Bool(b)) => s.cap_sht = Some(*b),
                ("sensors.sensor1_type", ConfigValue::Uint(n)) => s.sensor1_type = Some(*n as u32),
                ("sensors.sensor2_type", ConfigValue::Uint(n)) => s.sensor2_type = Some(*n as u32),
                ("sensors.sensor3_type", ConfigValue::Uint(n)) => s.sensor3_type = Some(*n as u32),
                ("sensors.sensor4_type", ConfigValue::Uint(n)) => s.sensor4_type = Some(*n as u32),
                ("sensors.cap_gpio_mode", ConfigValue::Enum(name)) => {
                    s.cap_gpio_mode = named_value(GPIO_MODE_NAMES, None, name);
                }
                ("sensors.cap_gpio_input_a", ConfigValue::Enum(name)) => {
                    s.cap_gpio_input_a = named_value(GPIO_INPUT_NAMES, Some("NONE"), name);
                }
                ("sensors.cap_gpio_input_b", ConfigValue::Enum(name)) => {
                    s.cap_gpio_input_b = named_value(GPIO_INPUT_NAMES, Some("NONE"), name);
                }
                ("sensors.accel_motion_sensitivity", ConfigValue::Enum(name)) => {
                    if let Some(n) = motion_value(name) {
                        s.accel_motion_sensitivity = Some(n);
                    }
                }
                _ => {}
            }
        }
        ("alarms", "alarms.alarm_limit", ConfigValue::Uint(n)) => {
            sp.alarms.get_or_insert_with(Alarms::default).alarm_limit = Some(*n as u32);
        }
        ("alarms", "alarms.radio_alarm_ack", ConfigValue::Bool(b)) => {
            sp.alarms
                .get_or_insert_with(Alarms::default)
                .radio_alarm_ack = Some(*b);
        }
        ("alarms", k, ConfigValue::Hex(s)) if alarm_slot_index(k).is_some() => {
            if let (Some(n), Ok(bytes)) = (alarm_slot_index(k), hex::decode(s)) {
                set_alarm_slot(sp.alarms.get_or_insert_with(Alarms::default), n, bytes);
            }
        }
        _ => {} // unreachable after validate(); ignore defensively
    }
}

/// Parse an `alarms.alarm_N` key into its slot index (0..15), if it is one.
fn alarm_slot_index(key: &str) -> Option<u8> {
    key.strip_prefix("alarms.alarm_")
        .and_then(|s| s.parse::<u8>().ok())
        .filter(|n| *n < node_alarm::SLOT_COUNT)
}

/// Set the encoded bytes for alarm slot `n` (0..15) on a SetParam's Alarms.
fn set_alarm_slot(al: &mut Alarms, n: u8, bytes: Vec<u8>) {
    match n {
        0 => al.alarm_0 = Some(bytes),
        1 => al.alarm_1 = Some(bytes),
        2 => al.alarm_2 = Some(bytes),
        3 => al.alarm_3 = Some(bytes),
        4 => al.alarm_4 = Some(bytes),
        5 => al.alarm_5 = Some(bytes),
        6 => al.alarm_6 = Some(bytes),
        7 => al.alarm_7 = Some(bytes),
        8 => al.alarm_8 = Some(bytes),
        9 => al.alarm_9 = Some(bytes),
        10 => al.alarm_10 = Some(bytes),
        11 => al.alarm_11 = Some(bytes),
        12 => al.alarm_12 = Some(bytes),
        13 => al.alarm_13 = Some(bytes),
        14 => al.alarm_14 = Some(bytes),
        15 => al.alarm_15 = Some(bytes),
        _ => {}
    }
}

fn set_param_command(sp: command::SetParam) -> Command {
    Command {
        seq: 0,
        body: Some(command::Body::SetParam(sp)),
    }
}

/// Build the `SetParam` downlink(s) for a desired config. Validates first
/// (fail-fast). Fields are greedily packed so each encoded `Command` stays
/// within `max_command_len`.
///
/// `save` controls the COMMIT semantics, and therefore what the device does:
/// - `save = true`  → the **last** batch carries `save=true`, so the device
///   stages every batch and then persists + **reboots** once (destructive).
/// - `save = false` → no batch carries `save`, so the values are only staged in
///   the device's RAM and are reverted on the next reboot (non-destructive dry
///   run / inspect-before-commit).
///
/// Returns `Command`s with `seq = 0` — the sender stamps the real seq.
pub fn build_set_param(
    config: &BTreeMap<String, ConfigValue>,
    max_command_len: usize,
    save: bool,
) -> Result<Vec<Command>, Vec<ConfigError>> {
    let fields = validate(config)?;

    let mut commands: Vec<Command> = Vec::new();
    let mut current = command::SetParam::default();
    let mut current_has = false;

    for (key, value) in &fields {
        // tentatively add the field, then check the encoded size
        let mut trial = current.clone();
        apply(&mut trial, key, value);
        let fits = set_param_command(trial.clone()).encoded_len() <= max_command_len;

        if !fits && current_has {
            // flush the current batch (never the last → save stays unset)
            commands.push(set_param_command(std::mem::take(&mut current)));
            current_has = false;
            apply(&mut current, key, value);
            current_has = true;
        } else {
            current = trial;
            current_has = true;
        }
    }

    // Only the final batch commits, and only when the caller asked to save.
    if save {
        current.save = Some(true);
    }
    commands.push(set_param_command(current));
    Ok(commands)
}

// --- simple no-arg / read command builders (used by read-back + #71) ---

fn cmd(body: command::Body) -> Command {
    Command {
        seq: 0,
        body: Some(body),
    }
}

/// `GetParam` reading back the given `group.field` keys — the read-side partner
/// of `build_set_param`, used to verify a write landed (decode → `diff_config`).
/// Unknown keys are skipped. The full-dump `GetConfig` is avoided on purpose
/// (it overflows the device stack in fw v1.4.0, hardware/node-firmware#176).
pub fn build_get_param(keys: &[&str]) -> Command {
    build_get_param_page(keys, 0)
}

/// Like [`build_get_param`] but requests a specific ConfigDump `page`. The
/// device pages the response: the host reads page 0, learns `page_count` from
/// the `ConfigDump`, then fetches the rest. `page == 0` encodes no page field,
/// so it is wire-identical to a plain [`build_get_param`].
pub fn build_get_param_page(keys: &[&str], page: u32) -> Command {
    let mut gp = command::GetParam::default();
    for k in keys {
        // Driven by the GROUPS table rather than a chain of prefix checks. The old
        // form matched only "application." and "alarms." and dropped everything
        // else in SILENCE, so a `sensors.*` key looked accepted and simply never
        // came back. A group added to GROUPS is now handled here automatically, and
        // an unknown key is logged rather than swallowed.
        let Some(field) = field_number(k) else {
            eprintln!("[node] get_param: ignoring unknown key {k:?}");
            continue;
        };
        match group_id(k) {
            Some(1) => gp.lorawan_field.push(field),
            Some(2) => gp.application_field.push(field),
            Some(3) => gp.sensors_field.push(field),
            Some(4) => gp.alarms_field.push(field),
            // `GetParam.p2p_field` is 6: 5 is `page` (sticker-firmware 7be011e).
            Some(5) => gp.p2p_field.push(field),
            _ => eprintln!("[node] get_param: ignoring key in unknown group {k:?}"),
        }
    }
    if page > 0 {
        gp.page = Some(page);
    }
    cmd(command::Body::GetParam(gp))
}

/// The small scalar set read when a caller selects nothing.
///
/// Kept deliberately narrow — it is what the live-verified default read path has
/// always requested, and widening it would multiply the round trips every
/// unqualified read costs. A Class-A read is chunked six fields at a time with a
/// 180 s timeout each, so the difference between 4 keys and 38 is minutes of
/// airtime. Callers that want more ask for it explicitly via
/// [`all_settable_keys`] or [`all_readable_keys`].
pub fn core_settable_keys() -> Vec<&'static str> {
    // The 16 numbered alarm slots are settable but excluded: requesting all of them
    // in one GetParam overflows the node's request array (it replies bad_request
    // "array overflow"). Alarm slots are read explicitly when needed.
    // `sensors.*`, `battery_level`, `interval_announce` (v2.0.0) and `radio_alarm_ack`
    // (sticker-firmware 3edb4a4) are excluded for the airtime reason above.
    // `history_channels` took `history_sensors`' place (v2.0.0, #430).
    SETTABLE
        .iter()
        .map(|(k, _, _)| *k)
        .filter(|k| alarm_slot_index(k).is_none())
        .filter(|k| {
            !k.starts_with("sensors.")
                && *k != "application.battery_level"
                && *k != "application.interval_announce"
                && *k != "alarms.radio_alarm_ack"
        })
        .collect()
}

/// Every remotely-settable `group.field` key except the 16 alarm slots, in
/// canonical [`SETTABLE`] order.
pub fn all_settable_keys() -> Vec<&'static str> {
    SETTABLE
        .iter()
        .map(|(k, _, _)| *k)
        .filter(|k| alarm_slot_index(k).is_none())
        .collect()
}

/// Everything readable over LoRaWAN: the settable surface plus the read-only
/// groups, for a viewer that wants to show a node's whole non-secret config.
///
/// Excludes the alarm slots (array-overflow, as above) and never includes the four
/// LoRaWAN session keys — the device does not return those over the radio at all.
pub fn all_readable_keys() -> Vec<&'static str> {
    let mut keys = all_settable_keys();
    keys.extend(READ_ONLY.iter().map(|(k, _)| *k));
    keys
}

pub fn build_get_info() -> Command {
    cmd(command::Body::GetInfo(command::GetInfo::default()))
}

pub fn build_reboot() -> Command {
    cmd(command::Body::Reboot(command::Reboot::default()))
}

pub fn build_force_send() -> Command {
    cmd(command::Body::ForceSend(command::ForceSend::default()))
}

/// Selective `ResetCounters`: clear exactly the channels flagged `true`.
///
/// The firmware clears a channel only when its flag is **present and true**
/// (`rc->has_hall_left && rc->hall_left`, `app_cmd.c:662-664`), so every flag has
/// to be sent explicitly. An all-absent message clears nothing while still
/// answering `Ack` — see [`build_reset_counters`].
///
/// Only these four counters are resettable, and they are the four that persist
/// across reboot (`counters/totals` in NVS). `motion_count` (PIR) and
/// `accel_motion_count` are RAM-only on the device and cannot be cleared by this
/// command at all; they zero themselves on reboot.
pub fn build_reset_counters_selective(
    hall_left: bool,
    hall_right: bool,
    input_a: bool,
    input_b: bool,
) -> Command {
    cmd(command::Body::ResetCounters(command::ResetCounters {
        hall_left: Some(hall_left),
        hall_right: Some(hall_right),
        input_a: Some(input_a),
        input_b: Some(input_b),
    }))
}

/// Reset every resettable pulse counter (all four channels).
///
/// NOTE: this used to send an *empty* `ResetCounters`, documented as "reset every
/// channel". That was wrong in a way the device could not report: the firmware
/// treats an absent flag as "leave this counter alone", so the command cleared
/// nothing yet still replied `Ack` and still scheduled the counters-save — so
/// `fiberctl lorawan send <eui> reset-counters --force` reported success and did
/// nothing. The flags are now set explicitly.
pub fn build_reset_counters() -> Command {
    build_reset_counters_selective(true, true, true, true)
}

/// `FactoryReset` (proto id 23).
///
/// **Rejected over LoRaWAN by design.** `app_config.yml` declares this command
/// `transports: [nfc, shell]`, so the generated dispatch answers
/// `Error{NOT_READY, "transport not allowed"}` and does nothing. It exists here so
/// the rejection can be demonstrated from the bench rather than asserted from
/// documentation, and so nobody reaches for `send_node_raw` with hand-written
/// hex to find that out.
///
/// The reachable reset over the radio is `device_reset` (id 8): it restores
/// defaults but keeps identity and the LoRaWAN keys, so the node stays joined.
pub fn build_factory_reset() -> Command {
    cmd(command::Body::FactoryReset(command::FactoryReset::default()))
}

/// `DeviceReset` (proto id 8): restore defaults, keeping identity and the full
/// LoRaWAN configuration, then cold-reboot. The device stays joined, so no
/// re-provisioning is needed — but every sensor capability, alarm rule, interval
/// and history setting is lost.
pub fn build_device_reset() -> Command {
    cmd(command::Body::DeviceReset(command::DeviceReset::default()))
}

/// `ClockSync` carrying an explicit wall-clock (Unix seconds) to push to the device.
pub fn build_clock_sync(unix_time: u32) -> Command {
    cmd(command::Body::ClockSync(command::ClockSync {
        unix_time: Some(unix_time),
    }))
}

/// `ClockSync` with an empty body: ask the device to re-sync from the network
/// instead of pushing a wall-clock.
///
/// The firmware branches on whether `unix_time` is present, not on the transport
/// (`app_cmd_handle_clock_sync` is `ARG_UNUSED(tp)`). With it absent the device
/// calls `app_clock_force_resync()` and **sends no immediate reply** — the answer
/// is a deferred `Info` uplink once `LORAWAN_TIME_UPDATED` lands. So a caller must
/// not wait for a correlated response to this one.
pub fn build_clock_sync_from_network() -> Command {
    cmd(command::Body::ClockSync(command::ClockSync {
        unix_time: None,
    }))
}

/// `ReqHistory` requesting the device's on-device history buffer (#39).
/// `from_unix`/`to_unix` bound the window (Unix seconds); `None` = whole buffer.
/// The device replies with one or more `HistoryFrame` responses that share this
/// command's seq (collected by the history backfill path), each expanded with
/// [`super::node_response::expand_history_frame`].
pub fn build_req_history(from_unix: Option<u32>, to_unix: Option<u32>) -> Command {
    cmd(command::Body::ReqHistory(command::ReqHistory {
        from_unix,
        to_unix,
    }))
}

/// Parse a raw `key=value` string into a [`ConfigValue`] of the type the field
/// expects (bool vs unsigned), per the [`SETTABLE`] spec. Range validation
/// happens later in [`build_set_param`]/[`validate`]; this only fixes the type.
pub fn parse_value(key: &str, raw: &str) -> Result<ConfigValue, ConfigError> {
    let err = |reason: String| ConfigError {
        key: key.to_string(),
        reason,
    };
    let (_, kind) = spec(key).ok_or_else(|| {
        if is_read_only(key) {
            err("read-only over LoRaWAN".into())
        } else {
            err("not a remotely settable parameter".into())
        }
    })?;
    match kind {
        Kind::Bool => match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "on" | "yes" => Ok(ConfigValue::Bool(true)),
            "false" | "0" | "off" | "no" => Ok(ConfigValue::Bool(false)),
            _ => Err(err(format!("expected a boolean (true/false), got {raw:?}"))),
        },
        Kind::Uint { .. } | Kind::W1Type => raw
            .trim()
            .parse::<u64>()
            .map(ConfigValue::Uint)
            .map_err(|_| err(format!("expected an unsigned integer, got {raw:?}"))),
        // Slot semantics are checked in validate_one; keep the raw hex here.
        Kind::AlarmHex => Ok(ConfigValue::Hex(raw.trim().to_ascii_lowercase())),
        // Hex columns, or Hub field names separated by commas
        // (`temperature,humidity,w1_1_temperature`); either way normalised to
        // the 24-byte padded hex the device reads back, so diff_config converges.
        Kind::HistoryChannels => {
            let t = raw.trim();
            let columns = if !t.is_empty() && t.chars().all(|c| c.is_ascii_hexdigit()) {
                hex::decode(t).map_err(|e| err(format!("invalid hex: {e}")))?
            } else {
                let fields: Vec<&str> = t
                    .split(',')
                    .map(str::trim)
                    .filter(|f| !f.is_empty())
                    .collect();
                history_columns_of(&fields).map_err(|e| err(e))?
            };
            history_channels_hex(&columns)
                .map(ConfigValue::Hex)
                .map_err(|e| err(e))
        }
        // Accept a name OR the numeric proto value, and normalise both to the Enum
        // spelling reads emit. Without this normalisation a write of "2" would be
        // stored as Uint(2), diff_config would compare it against Enum("MEDIUM")
        // forever, and the node would look permanently out of sync on the one
        // field that was just written successfully.
        Kind::NamedEnum { names, alias0 } => match named_value(names, alias0, raw) {
            Some(n) => Ok(ConfigValue::Enum(names[n as usize].to_string())),
            None => Err(err(format!(
                "expected {}, got {raw:?}",
                names.join("/").to_ascii_lowercase()
            ))),
        },
        Kind::MotionEnum => {
            let t = raw.trim();
            if let Ok(n) = t.parse::<i32>() {
                if (0..=3).contains(&n) {
                    return Ok(motion_config_value(n));
                }
                return Err(err(format!(
                    "motion sensitivity out of range 0..=3, got {n}"
                )));
            }
            match motion_value(t) {
                Some(n) => Ok(motion_config_value(n)),
                None => Err(err(format!(
                    "expected off/low/medium/high or 0..=3, got {raw:?}"
                ))),
            }
        }
    }
}

/// Size of `application.history_channels` (sticker-firmware v2.0.0, #430).
pub const HISTORY_CHANNELS_LEN: usize = 24;
/// An unused `history_channels` column (the firmware's `fill: 0xff`).
pub const HISTORY_CHANNEL_UNUSED: u8 = 0xff;

/// The 24 `history_channels` bytes for a list of columns (`slot << 5 | ch`):
/// the columns in order, padded with [`HISTORY_CHANNEL_UNUSED`].
pub fn history_channels_bytes(columns: &[u8]) -> Vec<u8> {
    let mut out = columns.to_vec();
    out.resize(
        HISTORY_CHANNELS_LEN.max(columns.len()),
        HISTORY_CHANNEL_UNUSED,
    );
    out
}

/// The canonical `history_channels` value for a list of columns: 48 lowercase
/// hex chars, checked like [`check_history_channels`].
pub fn history_channels_hex(columns: &[u8]) -> Result<String, String> {
    if columns.len() > HISTORY_CHANNELS_LEN {
        return Err(format!(
            "at most {HISTORY_CHANNELS_LEN} columns, got {}",
            columns.len()
        ));
    }
    let s = hex::encode(history_channels_bytes(columns));
    check_history_channels(&s)?;
    Ok(s)
}

/// A `history_channels` value as the device takes it: exactly 24 bytes, every
/// used column on slot 0..4 and listed once. A slot-0 column must be a
/// motherboard channel of the registry; a 1-Wire column is checked by slot
/// only, its type being the slot's configuration at sample time (as the v2
/// radio's `command_encode::check_history_channels`).
fn check_history_channels(hex_str: &str) -> Result<(), String> {
    let raw = hex::decode(hex_str).map_err(|e| format!("invalid hex: {e}"))?;
    if raw.len() != HISTORY_CHANNELS_LEN {
        return Err(format!(
            "expected {HISTORY_CHANNELS_LEN} bytes, got {}",
            raw.len()
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for &b in raw.iter().filter(|b| **b != HISTORY_CHANNEL_UNUSED) {
        if b >> 5 > sensor_types::W1_SLOTS {
            return Err(format!("column {b:#04x} names slot {}, not 0..4", b >> 5));
        }
        if b >> 5 == 0 && sensor_types::channel(0, None, b & 0x1f).is_none() {
            return Err(format!(
                "column {b:#04x} names motherboard channel {}, which does not exist",
                b & 0x1f
            ));
        }
        if !seen.insert(b) {
            return Err(format!("column {b:#04x} is listed twice"));
        }
    }
    Ok(())
}

/// The `history_channels` columns of Hub fields (`temperature`,
/// `w1_1_temperature`), in order. A 1-Wire field resolves without knowing the
/// slot's type when every 1-Wire type that has the channel puts it on the same
/// channel number (`w1_N_temperature` is channel 0 of both dallas and the
/// machine probe); a field no type carries, or one that is ambiguous, is an error.
pub fn history_columns_of(fields: &[&str]) -> Result<Vec<u8>, String> {
    fields
        .iter()
        .map(|field| {
            let candidates: std::collections::BTreeSet<u8> = sensor_types::TYPES
                .iter()
                .filter(|t| t.is_w1())
                .filter_map(|t| history_column(field, |_| Some(t.id)))
                .collect();
            let mut it = candidates.into_iter();
            match (it.next(), it.next()) {
                (Some(col), None) => Ok(col),
                (None, _) => Err(format!("{field:?} is not a recordable field")),
                (Some(_), Some(_)) => Err(format!(
                    "{field:?} differs between 1-Wire sensor types; give the column as hex"
                )),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::lorawan::node_proto::{command, Command};

    fn cfg(pairs: &[(&str, ConfigValue)]) -> BTreeMap<String, ConfigValue> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn decode_set_param(c: &Command) -> command::SetParam {
        match &c.body {
            Some(command::Body::SetParam(sp)) => sp.clone(),
            other => panic!("expected SetParam, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_out_of_range_and_wrong_type() {
        let errs = validate(&cfg(&[
            ("application.interval_report", ConfigValue::Uint(10)), // < 60
            ("alarms.alarm_limit", ConfigValue::Uint(3601)),        // > 3600
            ("application.history_enable", ConfigValue::Uint(1)),   // wrong type (bool)
            ("application.bogus", ConfigValue::Uint(1)),            // unknown
        ]))
        .unwrap_err();
        assert_eq!(errs.len(), 4);
        assert!(errs.iter().any(|e| e.key == "application.interval_report"));
        assert!(errs.iter().any(|e| e.key == "application.bogus"));
    }

    #[test]
    fn validate_allows_zero_sentinel_for_interval_sample() {
        // interval_sample has zero_allowed (0 = precede report), range 5..3600
        assert!(validate(&cfg(&[(
            "application.interval_sample",
            ConfigValue::Uint(0)
        )]))
        .is_ok());
        assert!(validate(&cfg(&[(
            "application.interval_sample",
            ConfigValue::Uint(4)
        )]))
        .is_err());
    }

    #[test]
    fn single_batch_sets_fields_and_save() {
        let cmds = build_set_param(
            &cfg(&[
                ("application.interval_report", ConfigValue::Uint(1200)),
                ("application.history_enable", ConfigValue::Bool(true)),
                ("alarms.alarm_limit", ConfigValue::Uint(300)),
            ]),
            DR0_COMMAND_BUDGET,
            true,
        )
        .unwrap();
        assert_eq!(cmds.len(), 1); // small config → one downlink
        let sp = decode_set_param(&cmds[0]);
        assert_eq!(sp.save, Some(true));
        let app = sp.application.unwrap();
        assert_eq!(app.interval_report, Some(1200));
        assert_eq!(app.history_enable, Some(true));
        assert_eq!(sp.alarms.unwrap().alarm_limit, Some(300));
    }

    /// sticker-firmware 3edb4a4 (#460 T2c): `radio_alarm_ack` is alarms field 21, a bool the
    /// firmware takes over the radio. It is written like any other scalar, but stays out of
    /// the default read.
    #[test]
    fn radio_alarm_ack_is_a_settable_alarms_bool() {
        let cmds = build_set_param(
            &cfg(&[("alarms.radio_alarm_ack", ConfigValue::Bool(true))]),
            DR0_COMMAND_BUDGET,
            false,
        )
        .unwrap();
        let al = decode_set_param(&cmds[0]).alarms.expect("the alarms group");
        assert_eq!(al.radio_alarm_ack, Some(true));
        // Field 21, varint: tag (21 << 3) = 0xa8 0x01, then true.
        assert_eq!(al.encode_to_vec(), [0xa8, 0x01, 0x01]);

        let err = validate(&cfg(&[("alarms.radio_alarm_ack", ConfigValue::Uint(1))])).unwrap_err();
        assert_eq!(err[0].reason, "expected a boolean");

        assert!(all_settable_keys().contains(&"alarms.radio_alarm_ack"));
        assert!(!core_settable_keys().contains(&"alarms.radio_alarm_ack"));
        let get = build_get_param(&["alarms.radio_alarm_ack"]);
        let Some(command::Body::GetParam(gp)) = get.body else {
            panic!("a GetParam");
        };
        assert_eq!(gp.alarms_field, [21]);
    }

    #[test]
    fn save_false_does_not_commit() {
        // save=false → values staged only, no batch carries save (no reboot).
        let cmds = build_set_param(
            &cfg(&[("application.interval_report", ConfigValue::Uint(600))]),
            DR0_COMMAND_BUDGET,
            false,
        )
        .unwrap();
        assert_eq!(cmds.len(), 1);
        let sp = decode_set_param(&cmds[0]);
        assert_eq!(sp.save, None, "save=false must not set the commit flag");
        assert_eq!(sp.application.unwrap().interval_report, Some(600));
    }

    #[test]
    fn batching_puts_save_on_last_only() {
        // Tiny budget so single fields fit alone but pairs don't → forced split.
        let budget = 8;
        let cmds = build_set_param(
            &cfg(&[
                ("application.interval_report", ConfigValue::Uint(1200)),
                ("application.interval_sample", ConfigValue::Uint(60)),
                ("alarms.alarm_limit", ConfigValue::Uint(300)),
            ]),
            budget,
            true,
        )
        .unwrap();
        assert!(
            cmds.len() >= 2,
            "expected multiple batches, got {}",
            cmds.len()
        );
        for (i, c) in cmds.iter().enumerate() {
            let sp = decode_set_param(c);
            let is_last = i == cmds.len() - 1;
            assert_eq!(
                sp.save,
                if is_last { Some(true) } else { None },
                "save placement at #{i}"
            );
            // non-final batches must respect the budget; the final one may exceed
            // it only by the 2-byte save flag (unavoidable on the commit message).
            if !is_last {
                assert!(
                    c.encoded_len() <= budget,
                    "batch #{i} = {} B > {budget}",
                    c.encoded_len()
                );
            }
        }
        // round-trip: every requested field is present across the batches
        let mut report = None;
        let mut sample = None;
        let mut limit = None;
        for c in &cmds {
            let sp = decode_set_param(c);
            if let Some(a) = &sp.application {
                report = report.or(a.interval_report);
                sample = sample.or(a.interval_sample);
            }
            if let Some(al) = &sp.alarms {
                limit = limit.or(al.alarm_limit);
            }
        }
        assert_eq!((report, sample, limit), (Some(1200), Some(60), Some(300)));
    }

    #[test]
    fn describe_fault_v140_group_encoded() {
        // Firmware v1.4.0 sends group * 100 + tag (app_cmd.c:332), so
        // application (group 2) field 3 arrives as 203 — NOT as a bare 3.
        let sent = ["application.interval_report", "application.history_enable"];
        assert_eq!(
            describe_fault(203, sent),
            Some("application.interval_report".to_string())
        );
        assert_eq!(
            describe_fault(204, sent),
            Some("application.history_enable".to_string())
        );

        // A tag we did not send is still attributed to its group rather than lost.
        assert_eq!(
            describe_fault(205, sent),
            Some("application.<field 5>".to_string())
        );

        // The same tag in a different group must not be confused with ours: 103 is
        // lorawan field 3 (network), which is not settable over LoRaWAN at all.
        assert_eq!(
            describe_fault(103, sent),
            Some("lorawan.<field 3>".to_string())
        );

        // Group-scoped fault with no specific field: the alarm-rule reload reports
        // 400 when a rule fails validation (app_cmd.c:343).
        assert_eq!(
            describe_fault(400, sent),
            Some("alarms (whole group)".to_string())
        );

        // sensors is group 3. The group resolves even though no `sensors.*` key is
        // settable yet (that arrives with the #69 surface), so a NOT_WRITABLE on a
        // capability is already attributable instead of silently unnamed.
        assert_eq!(
            describe_fault(306, sent),
            Some("sensors.<field 6>".to_string())
        );
    }

    #[test]
    fn describe_fault_bare_tag_names_nothing() {
        // A bare proto tag (group id 0) is pre-v1.4.0 firmware's; v2.0.0 is the
        // minimum, so it is not guessed among the sent keys.
        let sent = ["application.interval_report", "application.history_enable"];
        assert_eq!(describe_fault(3, sent), None);
        assert_eq!(describe_fault(99, sent), None);
    }

    #[test]
    fn describe_fault_zero_is_no_field() {
        // The firmware initialises fault_field to 0 (app_cmd.c:270) for errors that
        // reference no field at all — never invent one.
        assert_eq!(describe_fault(0, ["application.interval_report"]), None);
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{:02x}", x)).collect()
    }

    #[test]
    fn reset_counters_sends_every_flag_explicitly() {
        // REGRESSION: build_reset_counters used to send an EMPTY ResetCounters and
        // document it as "reset every channel". The firmware requires
        // `has_x && x` per channel (app_cmd.c:662-664), so an empty message resets
        // nothing — while still returning Ack and still scheduling the counters
        // save. The bug was invisible from the gateway: it reported success.
        let mut all = build_reset_counters();
        all.seq = 7;
        // 0807 seq, 5208 = field 10 len 8, then four (tag,true) pairs.
        assert_eq!(hex(&all.encode_to_vec()), "080752080801100118012001");

        // An empty body must never be what we send, so assert it differs.
        let mut empty = cmd(command::Body::ResetCounters(
            command::ResetCounters::default(),
        ));
        empty.seq = 7;
        assert_ne!(
            hex(&all.encode_to_vec()),
            hex(&empty.encode_to_vec()),
            "an all-channels reset must not encode as an empty message"
        );
    }

    #[test]
    fn reset_counters_selective_sets_only_requested_channels() {
        // Selective reset is real: an operator can clear hall_left without losing
        // the other three totalisers.
        let mut c = build_reset_counters_selective(true, false, false, false);
        c.seq = 7;
        // hall_left=true, the other three explicitly false (present-and-false is
        // what tells the device "leave it alone" unambiguously).
        assert_eq!(hex(&c.encode_to_vec()), "080752080801100018002000");

        let Some(command::Body::ResetCounters(rc)) = &c.body else {
            panic!("expected ResetCounters");
        };
        assert_eq!(rc.hall_left, Some(true));
        assert_eq!(rc.hall_right, Some(false));
        assert_eq!(rc.input_a, Some(false));
        assert_eq!(rc.input_b, Some(false));
    }

    #[test]
    fn v140_reset_ladder_encodes_distinct_command_ids() {
        // v1.4.0 renamed proto id 8 factory_reset -> device_reset and introduced a
        // NEW, narrower factory_reset at id 23. They are different operations and
        // must never be confused:
        //   device_reset  (8)  restores defaults but KEEPS identity + LoRaWAN keys,
        //                      so the node stays joined. Reachable over LoRaWAN.
        //   factory_reset (23) also drops the LoRaWAN session/keys, and is
        //                      transports [nfc, shell] — over LoRaWAN the device
        //                      answers Error{NOT_READY,"transport not allowed"}.
        //
        // The id-8 encoding is byte-identical to the old factory_reset, so the
        // rename cannot change anything already on the wire.
        let mut dr = cmd(command::Body::DeviceReset(command::DeviceReset::default()));
        dr.seq = 7;
        assert_eq!(hex(&dr.encode_to_vec()), "08074200");

        let mut fr = cmd(command::Body::FactoryReset(command::FactoryReset::default()));
        fr.seq = 7;
        assert_eq!(hex(&fr.encode_to_vec()), "0807ba0100");
    }

    #[test]
    fn real_hw_set_param_accepted_with_ack() {
        // GOLDEN: the NODE (fw v1.4.0) accepted exactly these bytes over the
        // shell-inject path and replied Response{seq, Ack} (action 1=save was
        // recognised, just not executed from a shell inject). Ties our builder
        // output to bytes real firmware parses, plus the decode of its Ack.
        use crate::libs::lorawan::node_response::{decode_response, DecodedResponse, ResponseKind};
        let config = cfg(&[
            ("application.interval_report", ConfigValue::Uint(1200)),
            ("application.history_enable", ConfigValue::Bool(true)),
        ]);
        let mut cmds = build_set_param(&config, DR0_COMMAND_BUDGET, true).unwrap();
        assert_eq!(cmds.len(), 1);
        cmds[0].seq = 9; // sender stamps the seq
        assert_eq!(hex(&cmds[0].encode_to_vec()), "08091209120518b00920011801");

        // the real Ack the device returned (on-wire, incl. 0x01 version byte)
        let resp = [0x01u8, 0x08, 0x09, 0x12, 0x00];
        let d = decode_response(&resp[1..]).unwrap();
        assert_eq!(
            d,
            DecodedResponse {
                seq: 9,
                kind: ResponseKind::Ack
            }
        );
    }

    // The #119 HIL rule (matches the node_alarm vector): enabled, s1
    // machine-probe temperature, lo 10, hi 30, dwell 0.
    const ALARM_HEX: &str = "030100030000000020410000f04100000000";

    #[test]
    fn alarm_slot_parsed_validated_and_encoded() {
        assert_eq!(
            parse_value("alarms.alarm_3", ALARM_HEX).unwrap(),
            ConfigValue::Hex(ALARM_HEX.to_string())
        );
        let config = cfg(&[("alarms.alarm_3", ConfigValue::Hex(ALARM_HEX.to_string()))]);
        assert!(validate(&config).is_ok());
        let cmds = build_set_param(&config, DR0_COMMAND_BUDGET, false).unwrap();
        assert_eq!(cmds.len(), 1);
        let sp = decode_set_param(&cmds[0]);
        let alarms = sp.alarms.expect("alarms group present");
        assert_eq!(
            alarms.alarm_3.as_deref().map(hex),
            Some(ALARM_HEX.to_string())
        );
    }

    #[test]
    fn alarm_invalid_target_rejected() {
        // present+enabled s1 dallas (2) channel 2: dallas has temperature only.
        let bad = "030102020000000000000000803f00000000";
        let errs = validate(&cfg(&[(
            "alarms.alarm_0",
            ConfigValue::Hex(bad.to_string()),
        )]))
        .unwrap_err();
        assert!(errs.iter().any(|e| e.key == "alarms.alarm_0"));
        // A v1.4 17-byte rule is refused by its length, not misread.
        let v14 = "0300000000000070410000c8410000003f";
        let errs = validate(&cfg(&[("alarms.alarm_0", ConfigValue::Hex(v14.into()))])).unwrap_err();
        assert!(
            errs[0].reason.contains("36 hex chars"),
            "{}",
            errs[0].reason
        );
        // Clearing a slot (all zero) is always allowed.
        assert!(validate(&cfg(&[(
            "alarms.alarm_0",
            ConfigValue::Hex("00".repeat(18))
        )]))
        .is_ok());
    }

    #[test]
    fn history_channels_take_hex_or_hub_fields() {
        let want = format!("0001202122{}", "ff".repeat(19));
        // Board temperature + humidity, then s1 temperature, humidity, aux.
        let from_fields = parse_value(
            "application.history_channels",
            "temperature, humidity,w1_1_temperature,w1_1_humidity,w1_1_temperature_aux",
        )
        .unwrap();
        assert_eq!(from_fields, ConfigValue::Hex(want.clone()));
        let from_hex = parse_value("application.history_channels", "0001202122").unwrap();
        assert_eq!(from_hex, from_fields, "a short list is padded with 0xff");

        let cmds = build_set_param(
            &cfg(&[("application.history_channels", from_hex)]),
            DR0_COMMAND_BUDGET,
            false,
        )
        .unwrap();
        let app = decode_set_param(&cmds[0]).application.unwrap();
        assert_eq!(app.history_channels.as_deref().map(hex), Some(want));

        assert!(parse_value("application.history_channels", "bogus_field").is_err());
        assert!(
            parse_value("application.history_channels", "0000").is_err(),
            "listed twice"
        );
        assert!(
            parse_value("application.history_channels", "a0").is_err(),
            "slot 5"
        );
        assert!(
            parse_value("application.history_channels", "1f").is_err(),
            "no motherboard channel 31"
        );
        assert!(
            parse_value("application.history_channels", "3f").is_ok(),
            "a 1-Wire column is checked by slot only"
        );
        let short = validate(&cfg(&[(
            "application.history_channels",
            ConfigValue::Hex("0001".into()),
        )]));
        assert!(short.is_err(), "the canonical value is the full 24 bytes");
    }

    #[test]
    fn v200_settable_keys_reach_the_wire() {
        let cmds = build_set_param(
            &cfg(&[
                ("application.interval_announce", ConfigValue::Uint(0)),
                ("sensors.cap_sht", ConfigValue::Bool(false)),
                ("sensors.sensor2_type", ConfigValue::Uint(3)),
            ]),
            DR0_COMMAND_BUDGET,
            false,
        )
        .unwrap();
        let sp = decode_set_param(&cmds[0]);
        assert_eq!(sp.application.unwrap().interval_announce, Some(0));
        let sensors = sp.sensors.unwrap();
        assert_eq!(sensors.cap_sht, Some(false));
        assert_eq!(sensors.sensor2_type, Some(3));

        let errs = validate(&cfg(&[
            ("application.interval_announce", ConfigValue::Uint(169)),
            ("sensors.sensor1_type", ConfigValue::Uint(1)), // the board is no slot type
            ("application.history_sensors", ConfigValue::Uint(3)), // reserved in v2.0.0
        ]))
        .unwrap_err();
        assert_eq!(errs.len(), 3);
        assert!(validate(&cfg(&[("sensors.sensor1_type", ConfigValue::Uint(0))])).is_ok());
    }

    #[test]
    fn alarm_slots_split_across_dr0_batches() {
        let config = cfg(&[
            ("alarms.alarm_0", ConfigValue::Hex(ALARM_HEX.to_string())),
            ("alarms.alarm_1", ConfigValue::Hex(ALARM_HEX.to_string())),
            ("alarms.alarm_2", ConfigValue::Hex(ALARM_HEX.to_string())),
        ]);
        let cmds = build_set_param(&config, DR0_COMMAND_BUDGET, true).unwrap();
        assert!(
            cmds.len() >= 2,
            "3 slots should not fit one DR0 command, got {}",
            cmds.len()
        );
        for c in &cmds {
            assert!(c.encode_to_vec().len() <= DR0_COMMAND_BUDGET);
        }
    }

    #[test]
    fn all_settable_keys_excludes_alarm_slots() {
        // Numbered alarm slots must not be in a bulk read (they overflow a single
        // GetParam); the scalar params, incl. the alarm scalars, stay.
        let keys = all_settable_keys();
        assert!(keys.contains(&"alarms.alarm_limit"));
        assert!(keys.contains(&"application.interval_report"));
        assert!(!keys.contains(&"alarms.alarm_3"));
        assert!(keys.iter().all(|k| alarm_slot_index(k).is_none()));
    }

    #[test]
    fn core_read_set_is_unchanged_by_the_widened_settable_surface() {
        // The default read (read_config with no keys) must stay exactly what the
        // live-verified path always requested. A Class-A read is chunked six fields
        // at a time with a 180 s timeout each, so silently widening this default
        // would turn every unqualified read into minutes of airtime.
        let core = core_settable_keys();
        assert_eq!(
            core,
            vec![
                "application.interval_sample",
                "application.interval_report",
                "application.history_enable",
                "application.history_channels",
                "alarms.alarm_limit",
                // `alarms.alarm_notif_time` left the set with the 7be011e proto
                // refresh: v2.0.0 has it `reserved`, so it never came back anyway.
            ]
        );
        // The wider surface exists, it is just opt-in.
        assert!(all_settable_keys().contains(&"sensors.cap_barometer"));
        assert!(all_settable_keys().contains(&"application.battery_level"));
        assert!(all_readable_keys().contains(&"lorawan.region"));
    }

    #[test]
    fn get_param_selects_the_sensors_group_instead_of_dropping_it() {
        // REGRESSION: build_get_param_page matched only "application." and
        // "alarms." and dropped every other key in SILENCE, so a sensors.* read
        // looked accepted and simply never came back.
        let c = build_get_param_page(
            &[
                "sensors.cap_barometer",
                "sensors.accel_motion_sensitivity",
                "lorawan.region",
            ],
            0,
        );
        let Some(command::Body::GetParam(gp)) = &c.body else {
            panic!("expected GetParam")
        };
        assert_eq!(
            gp.sensors_field,
            vec![6, 10],
            "cap_barometer=6, accel_motion_sensitivity=10"
        );
        assert_eq!(
            gp.lorawan_field,
            vec![1],
            "region=1, read-only but readable"
        );
        assert!(gp.application_field.is_empty());
    }

    #[test]
    fn read_only_keys_are_rejected_with_their_own_reason() {
        // A read-only key and an unknown key must not produce the same message: the
        // UI greys out the former and flags the latter as a typo.
        let e = validate(&cfg(&[("lorawan.adr", ConfigValue::Bool(true))])).unwrap_err();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].key, "lorawan.adr");
        assert_eq!(e[0].reason, "read-only over LoRaWAN");

        let e = validate(&cfg(&[(
            "sensors.sensor1_rom",
            ConfigValue::Hex("00".into()),
        )]))
        .unwrap_err();
        assert_eq!(e[0].reason, "read-only over LoRaWAN");

        let e = validate(&cfg(&[("application.nonsense", ConfigValue::Bool(true))])).unwrap_err();
        assert_eq!(e[0].reason, "not a remotely settable parameter");
    }

    #[test]
    fn motion_sensitivity_write_and_read_back_agree() {
        use crate::libs::lorawan::node_proto::app_config_message::Sensors as PSensors;
        use crate::libs::lorawan::node_proto::response;
        use crate::libs::lorawan::node_response::{decode_config, diff_config};

        // A write accepts a name or the numeric proto value, and BOTH normalise to
        // the Enum spelling reads emit.
        let from_num = parse_value("sensors.accel_motion_sensitivity", "2").unwrap();
        let from_name = parse_value("sensors.accel_motion_sensitivity", "medium").unwrap();
        assert_eq!(from_num, ConfigValue::Enum("MEDIUM".to_string()));
        assert_eq!(from_num, from_name);

        // It reaches the wire as the numeric proto value.
        let cmds = build_set_param(
            &cfg(&[("sensors.accel_motion_sensitivity", from_num.clone())]),
            DR0_COMMAND_BUDGET,
            false,
        )
        .unwrap();
        let sp = decode_set_param(&cmds[0]);
        assert_eq!(
            sp.sensors.as_ref().unwrap().accel_motion_sensitivity,
            Some(2)
        );

        // THE POINT: the device's read-back of that write must diff clean. Storing
        // Uint(2) instead would leave diff_config reporting a mismatch forever, on
        // the one field that was just written successfully.
        let dump = response::ConfigDump {
            sensors: Some(PSensors {
                accel_motion_sensitivity: Some(2),
                ..Default::default()
            }),
            ..Default::default()
        };
        let actual = decode_config(&dump);
        let desired = cfg(&[("sensors.accel_motion_sensitivity", from_num)]);
        assert!(
            diff_config(&desired, &actual).is_empty(),
            "write/read-back must converge, got {:?}",
            diff_config(&desired, &actual)
        );
    }

    #[test]
    fn every_settable_key_is_decodable() {
        // Guard for a whole class of bug: a key that can be WRITTEN but not DECODED
        // reads back as absent forever, so diff_config reports it permanently
        // unverified and the UI can never show it as applied. battery_level was
        // exactly this until decode_config learned it.
        use crate::libs::lorawan::node_proto::app_config_message::{
            Alarms as PAlarms, Application as PApplication, Lorawan as PLorawan, P2p as PP2p,
            Sensors as PSensors,
        };
        use crate::libs::lorawan::node_proto::response;
        use crate::libs::lorawan::node_response::decode_config;

        let dump = response::ConfigDump {
            lorawan: Some(PLorawan {
                region: Some(0),
                sub_band: Some(2),
                network: Some(0),
                adr: Some(false),
                activation: Some(0),
                deveui: Some(vec![1; 8]),
                joineui: Some(vec![2; 8]),
                devaddr: Some(vec![3; 4]),
                radio_link_check_interval: Some(5),
                radio_link_check_fail_rejoin: Some(5),
                radio_mode: Some(1),
                ..Default::default()
            }),
            application: Some(PApplication {
                calibration: Some(false),
                interval_sample: Some(0),
                interval_report: Some(900),
                history_enable: Some(true),
                battery_level: Some(2400),
                vendor_reset_allow: Some(true),
                interval_announce: Some(24),
                history_channels: Some(history_channels_bytes(&[0x00, 0x01])),
            }),
            sensors: Some(PSensors {
                cap_hall_left: Some(true),
                cap_hall_right: Some(true),
                cap_light_sensor: Some(true),
                cap_barometer: Some(true),
                cap_w1_sensors: Some(true),
                cap_accelerometer: Some(true),
                accel_motion_sensitivity: Some(3),
                sensor1_rom: Some(vec![4; 8]),
                sensor2_rom: Some(vec![5; 8]),
                sensor3_rom: Some(vec![6; 8]),
                sensor4_rom: Some(vec![7; 8]),
                hall_left_counter: Some(true),
                hall_right_counter: Some(true),
                input_a_counter: Some(true),
                input_b_counter: Some(true),
                cap_sht: Some(true),
                sensor1_type: Some(2),
                sensor2_type: Some(3),
                sensor3_type: Some(0),
                sensor4_type: Some(0),
                cap_gpio_mode: Some(1),
                cap_gpio_input_a: Some(3),
                cap_gpio_input_b: Some(2),
            }),
            alarms: Some(PAlarms {
                alarm_limit: Some(0),
                radio_alarm_ack: Some(false),
                ..Default::default()
            }),
            p2p: Some(PP2p {
                frequency: Some(868_100_000),
                spreading_factor: Some(10),
                tx_power: Some(14),
                modulation: Some(0),
            }),
            ..Default::default()
        };
        let decoded = decode_config(&dump);
        for key in all_readable_keys() {
            assert!(
                decoded.contains_key(key),
                "{key} is readable/settable but decode_config does not surface it"
            );
        }
    }

    /// Sticker-firmware 7be011e: the `p2p` group reads through `GetParam.p2p_field`
    /// (6 — 5 is `page`) and is never writable from here. v2.0.0: the pre-rename
    /// lorawan names are unknown keys.
    #[test]
    fn the_p2p_group_resolves_and_the_old_link_check_names_are_gone() {
        let c = build_get_param(&[
            "p2p.frequency",
            "p2p.tx_power",
            "lorawan.radio_link_check_interval",
            "lorawan.radio_link_check_fail_rejoin",
            "lorawan.radio_mode",
            "lorawan.link_check_interval",
            "lorawan.mode",
        ]);
        let Some(command::Body::GetParam(gp)) = &c.body else {
            panic!("expected GetParam")
        };
        assert_eq!(gp.p2p_field, vec![1, 3]);
        assert_eq!(gp.lorawan_field, vec![13, 14, 15]);
        // Command { 3 get_param { 1 lorawan_field [13,14,15], 6 p2p_field [1,3] } }
        assert_eq!(hex(&c.encode_to_vec()), "1a090a030d0e0f32020103");

        for key in ["p2p.spreading_factor", "lorawan.radio_link_check_interval"] {
            let e = validate(&cfg(&[(key, ConfigValue::Uint(7))])).unwrap_err();
            assert_eq!(e[0].reason, "read-only over LoRaWAN", "{key}");
        }
        let e = validate(&cfg(&[(
            "lorawan.link_check_interval",
            ConfigValue::Uint(7),
        )]))
        .unwrap_err();
        assert_ne!(e[0].reason, "read-only over LoRaWAN");
    }

    #[test]
    fn get_param_selects_proto_fields_per_group() {
        let c = build_get_param(&["application.interval_report", "alarms.alarm_limit", "bogus"]);
        match c.body {
            Some(command::Body::GetParam(gp)) => {
                assert_eq!(gp.application_field, vec![3]);
                assert_eq!(gp.alarms_field, vec![1]);
            }
            other => panic!("expected GetParam, got {other:?}"),
        }
    }

    #[test]
    fn req_history_encodes_range() {
        let mut c = build_req_history(Some(1000), Some(2000));
        c.seq = 7; // sender stamps the seq
                   // seq(1)=7, req_history(11)={ from_unix(1)=1000, to_unix(2)=2000 }
        assert_eq!(hex(&c.encode_to_vec()), "08075a0608e80710d00f");
        match c.body {
            Some(command::Body::ReqHistory(r)) => {
                assert_eq!((r.from_unix, r.to_unix), (Some(1000), Some(2000)));
            }
            other => panic!("expected ReqHistory, got {other:?}"),
        }
    }

    #[test]
    fn req_history_empty_requests_whole_buffer() {
        let c = build_req_history(None, None);
        match &c.body {
            Some(command::Body::ReqHistory(r)) => {
                assert_eq!((r.from_unix, r.to_unix), (None, None));
            }
            other => panic!("expected ReqHistory, got {other:?}"),
        }
        // seq=0 omitted (proto3 default); req_history(11) with an empty body
        // encodes to tag 0x5a + length 0x00.
        assert_eq!(hex(&c.encode_to_vec()), "5a00");
    }
}
