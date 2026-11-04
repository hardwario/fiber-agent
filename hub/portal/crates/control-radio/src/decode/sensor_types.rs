//! The STICKER sensor type registry — `node-firmware-v2.0.0.md` §1.
//!
//! sticker-firmware v2.0.0 (#430, PR #431) identifies every value by **(slot, channel)**: slot 0
//! is the motherboard (always type 1), slots 1..4 are the 1-Wire slots, each with a configured
//! type (`sensors.sensorN_type`), and a channel's meaning (name, unit, kind, scales) comes from
//! the slot's type. All of it is generated in the firmware from one file,
//! `app/src/app_sensor_types.yaml`, vendored here as `data/app_sensor_types.v2.0.0.yaml`.
//!
//! The Hub reads the registry through [`TYPES`], a checked-in table rather than the YAML parsed
//! at run time: the decoders are a hot path, and the table is plain `const` data every crate of
//! the workspace can match on. [`tests::the_table_is_the_vendored_registry`] renders the table
//! from the YAML and compares it with the source below, printing the region to paste when the
//! registry moves. Nothing here is hand-copied.
//!
//! [`hub_field`] is the one place a channel gets the field name the Hub publishes, stores and
//! grades (§1.1): telemetry, alarm events, history columns and the Portal's rule targets all go
//! through it, so a replayed record and an alarm land on the same series as the live reading.

/// Sensor type id of the motherboard; slot 0 is always this type.
pub const MOTHERBOARD: u8 = 1;

/// 1-Wire slots are 1..=4.
pub const W1_SLOTS: u8 = 4;

/// What an alarm rule on a channel does (`kind` in the registry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `lo` / `hi` / dwell band.
    Threshold,
    /// `from_state` / `to_state` / dwell transition.
    State,
    /// Counter rate, `hi` / dwell.
    Rate,
    /// Not an alarm target.
    None,
}

impl Kind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Kind::Threshold => "threshold",
            Kind::State => "state",
            Kind::Rate => "rate",
            Kind::None => "none",
        }
    }
}

/// Wire type of a channel's value in telemetry and alarm events (`wire[0]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    S32,
    U32,
}

/// Fixed-width history column encoding (`history[0]`). The top value is "absent".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hist {
    U8,
    I16,
    U16,
    I32,
    U32,
}

impl Hist {
    pub const fn size(self) -> usize {
        match self {
            Hist::U8 => 1,
            Hist::I16 | Hist::U16 => 2,
            Hist::I32 | Hist::U32 => 4,
        }
    }

    /// The "absent" sentinel, as the raw unsigned bytes read little-endian.
    pub const fn sentinel(self) -> u32 {
        match self {
            Hist::U8 => 0xff,
            Hist::I16 => 0x7fff,
            Hist::U16 => 0xffff,
            Hist::I32 => 0x7fff_ffff,
            Hist::U32 => 0xffff_ffff,
        }
    }
}

/// Channel flags.
pub const MOMENTARY: u8 = 1 << 0;
pub const COUNTER: u8 = 1 << 1;
pub const WATCHDOG_ONLY: u8 = 1 << 2;
pub const LIVENESS: u8 = 1 << 3;
pub const RETIRED: u8 = 1 << 4;

/// One channel of a sensor type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Channel {
    pub ch: u8,
    /// The registry's stable identifier (`temperature-aux`); the decoders' output key.
    pub name: &'static str,
    pub label: &'static str,
    pub quantity: &'static str,
    pub unit: &'static str,
    pub kind: Kind,
    pub wire: Wire,
    /// Value on the wire = `round(phys * wire_scale)`.
    pub wire_scale: f64,
    pub hist: Hist,
    pub hist_scale: f64,
    pub range: Option<(f64, f64)>,
    pub cap: Option<&'static str>,
    /// Momentary channels: the counter channel counting their pulses.
    pub pulses: Option<&'static str>,
    /// 1-Wire channels: the chip the channel is read from. The no-data watchdog raises one
    /// alarm per part, on its first channel.
    pub part: Option<&'static str>,
    pub flags: u8,
}

impl Channel {
    pub const fn momentary(&self) -> bool {
        self.flags & MOMENTARY != 0
    }
    pub const fn counter(&self) -> bool {
        self.flags & COUNTER != 0
    }
    pub const fn watchdog_only(&self) -> bool {
        self.flags & WATCHDOG_ONLY != 0
    }
    pub const fn retired(&self) -> bool {
        self.flags & RETIRED != 0
    }
    /// Whether an alarm rule may target this channel (`app_alarm_rule_valid`).
    pub const fn rule_target(&self) -> bool {
        !matches!(self.kind, Kind::None) && !self.watchdog_only() && !self.retired()
    }
    /// The channel name in snake case, the Hub's spelling.
    pub fn snake(&self) -> String {
        self.name.replace('-', "_")
    }
}

/// One sensor type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SensorType {
    pub id: u8,
    pub name: &'static str,
    pub label: &'static str,
    /// `Some(0)` for the motherboard; 1-Wire types have no fixed slot.
    pub slot: Option<u8>,
    pub w1_family: Option<u8>,
    pub channels: &'static [Channel],
}

impl SensorType {
    pub fn channel(&self, ch: u8) -> Option<&'static Channel> {
        self.channels.iter().find(|c| c.ch == ch)
    }
    pub fn channel_named(&self, name: &str) -> Option<&'static Channel> {
        self.channels
            .iter()
            .find(|c| c.name == name || c.snake() == name)
    }
    /// A type that may sit in a 1-Wire slot.
    pub const fn is_w1(&self) -> bool {
        self.w1_family.is_some()
    }
}

#[allow(clippy::too_many_arguments)]
const fn ch(
    ch: u8,
    name: &'static str,
    label: &'static str,
    quantity: &'static str,
    unit: &'static str,
    kind: Kind,
    wire: (Wire, f64),
    hist: (Hist, f64),
    range: Option<(f64, f64)>,
    cap: Option<&'static str>,
    pulses: Option<&'static str>,
    part: Option<&'static str>,
    flags: u8,
) -> Channel {
    Channel {
        ch,
        name,
        label,
        quantity,
        unit,
        kind,
        wire: wire.0,
        wire_scale: wire.1,
        hist: hist.0,
        hist_scale: hist.1,
        range,
        cap,
        pulses,
        part,
        flags,
    }
}

// Every encoding the generator may emit, used or not by the current registry.
#[allow(unused_imports)]
use Hist::{I16, I32, U16, U32 as H32, U8};
use Kind::{None as NoKind, Rate, State, Threshold};
use Wire::{S32, U32};

// BEGIN GENERATED SENSOR_TYPES
#[rustfmt::skip]
pub const TYPES: &[SensorType] = &[
    SensorType {
        id: 1,
        name: "motherboard",
        label: "STICKER motherboard",
        slot: Some(0),
        w1_family: None,
        channels: &[
            ch(0, "temperature", "Temperature (SHT4x)", "temperature", "degC", Threshold, (S32, 100.0), (I16, 100.0), Some((-45.0, 130.0)), Some("cap_sht"), None, None, LIVENESS),
            ch(1, "humidity", "Relative humidity (SHT4x)", "humidity", "%RH", Threshold, (U32, 2.0), (U8, 2.0), Some((0.0, 100.0)), Some("cap_sht"), None, None, LIVENESS),
            ch(2, "pressure", "Pressure (MPL3115A2)", "pressure", "hPa", Threshold, (U32, 10.0), (U16, 10.0), Some((0.0, 2000.0)), Some("cap_barometer"), None, None, LIVENESS),
            ch(3, "illuminance", "Illuminance (OPT3001)", "illuminance", "lx", Threshold, (U32, 1.0), (U16, 0.5), Some((0.0, 83865.6)), Some("cap_light_sensor"), None, None, 0),
            ch(4, "temperature-baro", "Temperature (MPL3115A2)", "temperature", "degC", Threshold, (S32, 100.0), (I16, 100.0), Some((-55.0, 125.0)), Some("cap_barometer"), None, None, 0),
            ch(5, "hall-left-state", "Hall left — state", "state", "bool", State, (U32, 1.0), (U8, 1.0), None, Some("cap_hall_left"), None, None, 0),
            ch(6, "hall-left-count", "Hall left — pulse count", "count", "pulses", Rate, (U32, 1.0), (H32, 1.0), None, Some("cap_hall_left"), None, None, COUNTER),
            ch(7, "hall-right-state", "Hall right — state", "state", "bool", State, (U32, 1.0), (U8, 1.0), None, Some("cap_hall_right"), None, None, 0),
            ch(8, "hall-right-count", "Hall right — pulse count", "count", "pulses", Rate, (U32, 1.0), (H32, 1.0), None, Some("cap_hall_right"), None, None, COUNTER),
            ch(9, "input-a-state", "Input A — state", "state", "bool", State, (U32, 1.0), (U8, 1.0), None, Some("cap_gpio_input_a=digital"), None, None, 0),
            ch(10, "input-a-count", "Input A — pulse count (digital / pulse)", "count", "pulses", Rate, (U32, 1.0), (H32, 1.0), None, Some("cap_gpio_input_a=digital|pulse"), None, None, COUNTER),
            ch(11, "input-a-voltage", "Input A — voltage", "voltage", "V", Threshold, (U32, 1000.0), (U16, 1000.0), Some((0.0, 30.0)), Some("cap_gpio_input_a=analog"), None, None, LIVENESS),
            ch(12, "input-b-state", "Input B — state", "state", "bool", State, (U32, 1.0), (U8, 1.0), None, Some("cap_gpio_input_b=digital"), None, None, 0),
            ch(13, "input-b-count", "Input B — pulse count (digital / pulse)", "count", "pulses", Rate, (U32, 1.0), (H32, 1.0), None, Some("cap_gpio_input_b=digital|pulse"), None, None, COUNTER),
            ch(14, "input-b-voltage", "Input B — voltage", "voltage", "V", Threshold, (U32, 1000.0), (U16, 1000.0), Some((0.0, 30.0)), Some("cap_gpio_input_b=analog"), None, None, LIVENESS),
            ch(15, "pir-motion", "PIR — motion", "state", "bool", State, (U32, 1.0), (U8, 1.0), None, Some("cap_gpio_mode=pir"), Some("pir-count"), None, MOMENTARY),
            ch(16, "pir-count", "PIR — motion count", "count", "events", Rate, (U32, 1.0), (H32, 1.0), None, Some("cap_gpio_mode=pir"), None, None, COUNTER),
            ch(17, "accel-motion", "Accelerometer — motion", "state", "bool", State, (U32, 1.0), (U8, 1.0), None, Some("cap_accelerometer"), Some("accel-count"), None, MOMENTARY),
            ch(18, "accel-count", "Accelerometer — motion count", "count", "events", Rate, (U32, 1.0), (H32, 1.0), None, Some("cap_accelerometer"), None, None, COUNTER),
            ch(19, "accel-orientation", "Accelerometer — orientation (1..6)", "orientation", "enum", NoKind, (U32, 1.0), (U8, 1.0), None, Some("cap_accelerometer"), None, None, 0),
            ch(20, "battery-voltage", "Battery voltage", "voltage", "V", Threshold, (U32, 1000.0), (U16, 1000.0), Some((0.0, 6.0)), None, None, None, WATCHDOG_ONLY | LIVENESS),
            ch(21, "altitude", "Altitude (MPL3115A2)", "altitude", "m", NoKind, (S32, 10.0), (I16, 1.0), Some((-700.0, 11000.0)), Some("cap_barometer"), None, None, 0),
        ],
    },
    SensorType {
        id: 2,
        name: "dallas",
        label: "DS18B20 thermometer",
        slot: None,
        w1_family: Some(0x28),
        channels: &[
            ch(0, "temperature", "Temperature", "temperature", "degC", Threshold, (S32, 100.0), (I16, 100.0), Some((-55.0, 125.0)), None, None, Some("ds18b20"), 0),
        ],
    },
    SensorType {
        id: 3,
        name: "machine-probe",
        label: "Machine probe (DS28E17 bridge)",
        slot: None,
        w1_family: Some(0x19),
        channels: &[
            ch(0, "temperature", "Temperature (SHT)", "temperature", "degC", Threshold, (S32, 100.0), (I16, 100.0), Some((-45.0, 130.0)), None, None, Some("sht"), 0),
            ch(1, "humidity", "Relative humidity (SHT)", "humidity", "%RH", Threshold, (U32, 2.0), (U8, 2.0), Some((0.0, 100.0)), None, None, Some("sht"), 0),
            ch(2, "temperature-aux", "Temperature (TMP112)", "temperature", "degC", Threshold, (S32, 100.0), (I16, 100.0), Some((-45.0, 130.0)), None, None, Some("tmp112"), 0),
            ch(3, "illuminance", "Illuminance (OPT3001)", "illuminance", "lx", Threshold, (U32, 1.0), (U16, 0.5), Some((0.0, 83865.6)), None, None, Some("opt3001"), 0),
            ch(4, "magnetic-field", "Magnetic field (Si7210)", "magnetic_field", "mT", Threshold, (S32, 1000.0), (I16, 100.0), Some((-204.7, 204.7)), None, None, Some("si7210"), 0),
            ch(5, "tilt", "Tilt alert (LIS2DH12)", "tilt", "bool", State, (U32, 1.0), (U8, 1.0), None, None, None, Some("lis2dh12"), 0),
            ch(6, "accel-x", "Acceleration X (LIS2DH12)", "acceleration", "m/s2", Threshold, (S32, 100.0), (I16, 100.0), Some((-156.9, 156.9)), None, None, Some("lis2dh12"), 0),
            ch(7, "accel-y", "Acceleration Y (LIS2DH12)", "acceleration", "m/s2", Threshold, (S32, 100.0), (I16, 100.0), Some((-156.9, 156.9)), None, None, Some("lis2dh12"), 0),
            ch(8, "accel-z", "Acceleration Z (LIS2DH12)", "acceleration", "m/s2", Threshold, (S32, 100.0), (I16, 100.0), Some((-156.9, 156.9)), None, None, Some("lis2dh12"), 0),
        ],
    },
];
// END GENERATED SENSOR_TYPES

/// A type by id. `None` for 0 (no type) and for an id this build does not know.
pub fn sensor_type(id: u8) -> Option<&'static SensorType> {
    TYPES.iter().find(|t| t.id == id)
}

/// The type a slot is read with: the motherboard for slot 0, else `w1_type`.
pub fn slot_type(slot: u8, w1_type: Option<u8>) -> Option<&'static SensorType> {
    if slot == 0 {
        sensor_type(MOTHERBOARD)
    } else {
        w1_type.and_then(sensor_type).filter(|t| t.is_w1())
    }
}

/// A channel of `(slot, type)`; the type is ignored for slot 0.
pub fn channel(slot: u8, w1_type: Option<u8>, ch: u8) -> Option<&'static Channel> {
    slot_type(slot, w1_type)?.channel(ch)
}

/// The label a 1-Wire slot type id carries on the wire: `empty` for 0, the type's name, and the
/// firmware decoder's `type{N}` for anything else (the motherboard id included: it is never a
/// slot type).
pub fn w1_type_label(id: u32) -> String {
    if id == 0 {
        return "empty".to_string();
    }
    match u8::try_from(id).ok().and_then(sensor_type) {
        Some(t) if t.is_w1() => t.name.to_string(),
        _ => format!("type{id}"),
    }
}

/// The type id of a 1-Wire label (`dallas` → 2). The inverse of [`w1_type_label`] for a known
/// type; `None` for `empty` and anything unknown.
pub fn w1_type_id(label: &str) -> Option<u8> {
    TYPES
        .iter()
        .find(|t| t.is_w1() && t.name == label)
        .map(|t| t.id)
}

/// `Info.w1_slot_state` (enum `app_w1_slot_state`), by wire value.
pub const W1_SLOT_STATES: &[&str] = &["none", "ok", "absent", "replaced", "mismatch"];

/// A `w1_slot_state` wire value by name; past the table, the firmware decoder's `state{N}`.
pub fn w1_slot_state_label(v: u32) -> String {
    W1_SLOT_STATES
        .get(v as usize)
        .map(|s| (*s).to_string())
        .unwrap_or_else(|| format!("state{v}"))
}

/// Alarm slot labels of the firmware decoder: `mb`, `s1`..`s4`, else `slot{N}`.
pub fn slot_label(slot: u32) -> String {
    match slot {
        0 => "mb".to_string(),
        1..=4 => format!("s{slot}"),
        _ => format!("slot{slot}"),
    }
}

/// The slot number of a [`slot_label`].
pub fn slot_of_label(label: &str) -> Option<u8> {
    match label {
        "mb" => Some(0),
        _ => label
            .strip_prefix('s')
            .and_then(|n| n.parse().ok())
            .filter(|n| (1..=W1_SLOTS).contains(n)),
    }
}

/// Motherboard channels whose Hub field is the live fPort-2 name rather than the channel's own
/// (`node-firmware-v2.0.0.md` §1.1). v2.0.0 leaves the motherboard telemetry unchanged, so these
/// keep the series the Portal already holds.
const MOTHERBOARD_FIELDS: &[(&str, &str)] = &[
    ("hall-left-state", "hall_left_is_active"),
    ("hall-right-state", "hall_right_is_active"),
    ("input-a-state", "input_a_is_active"),
    ("input-b-state", "input_b_is_active"),
    ("pir-count", "motion_count"),
    ("accel-count", "accel_motion_count"),
    ("accel-orientation", "orientation"),
    ("battery-voltage", "voltage"),
];

/// The Hub field of a channel: slot 0 by its live name, a 1-Wire slot as
/// `w1_{slot}_{channel}`.
pub fn hub_field_of(slot: u8, c: &Channel) -> String {
    if slot == 0 {
        MOTHERBOARD_FIELDS
            .iter()
            .find(|(name, _)| *name == c.name)
            .map(|(_, field)| (*field).to_string())
            .unwrap_or_else(|| c.snake())
    } else {
        format!("w1_{slot}_{}", c.snake())
    }
}

/// The Hub field of `(slot, type, ch)`, `None` when the registry does not know the channel.
pub fn hub_field(slot: u8, w1_type: Option<u8>, ch: u8) -> Option<String> {
    channel(slot, w1_type, ch).map(|c| hub_field_of(slot, c))
}

/// The slot and channel a Hub field names, reading a 1-Wire field with type `w1_type(slot)`.
/// `None` for a field no channel carries, or a 1-Wire field whose slot type is unknown.
pub fn field_channel(
    field: &str,
    w1_type: impl Fn(u8) -> Option<u8>,
) -> Option<(u8, &'static SensorType, &'static Channel)> {
    if let Some(rest) = field.strip_prefix("w1_") {
        let (slot, name) = rest.split_once('_')?;
        let slot: u8 = slot.parse().ok().filter(|n| (1..=W1_SLOTS).contains(n))?;
        let t = slot_type(slot, w1_type(slot))?;
        return t.channel_named(name).map(|c| (slot, t, c));
    }
    let t = sensor_type(MOTHERBOARD)?;
    t.channels
        .iter()
        .find(|c| hub_field_of(0, c) == field)
        .map(|c| (0, t, c))
}

/// 1-Wire channel names of every type, snake case, for "could this be a 1-Wire field" checks
/// that have no slot type to hand (field registries, display labels).
pub fn w1_channel_names() -> Vec<String> {
    let mut names: Vec<String> = TYPES
        .iter()
        .filter(|t| t.is_w1())
        .flat_map(|t| t.channels.iter().map(Channel::snake))
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    names.retain(|n| seen.insert(n.clone()));
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    const YAML: &str = include_str!("../../data/app_sensor_types.v2.0.0.yaml");
    const SOURCE: &str = include_str!("sensor_types.rs");

    fn s(v: &serde_yaml::Value) -> String {
        format!("{:?}", v.as_str().expect("string"))
    }

    fn opt_s(v: Option<&serde_yaml::Value>) -> String {
        v.map(|v| format!("Some({})", s(v)))
            .unwrap_or_else(|| "None".into())
    }

    fn f(v: &serde_yaml::Value) -> String {
        let x = v.as_f64().expect("number");
        format!("{x:?}")
    }

    /// The table as the registry renders it, in the source's own layout.
    fn render(yaml: &str) -> String {
        let doc: serde_yaml::Value = serde_yaml::from_str(yaml).expect("registry parses");
        let mut out = String::new();
        out.push_str("#[rustfmt::skip]\npub const TYPES: &[SensorType] = &[\n");
        for t in doc["types"].as_sequence().expect("types") {
            out.push_str("    SensorType {\n");
            out.push_str(&format!("        id: {},\n", t["id"].as_u64().expect("id")));
            out.push_str(&format!("        name: {},\n", s(&t["name"])));
            out.push_str(&format!("        label: {},\n", s(&t["label"])));
            out.push_str(&format!(
                "        slot: {},\n",
                t.get("slot")
                    .and_then(serde_yaml::Value::as_u64)
                    .map(|v| format!("Some({v})"))
                    .unwrap_or_else(|| "None".into())
            ));
            out.push_str(&format!(
                "        w1_family: {},\n",
                t.get("w1_family")
                    .and_then(serde_yaml::Value::as_u64)
                    .map(|v| format!("Some(0x{v:02x})"))
                    .unwrap_or_else(|| "None".into())
            ));
            out.push_str("        channels: &[\n");
            for c in t["channels"].as_sequence().expect("channels") {
                let kind = match c["kind"].as_str().expect("kind") {
                    "threshold" => "Threshold",
                    "state" => "State",
                    "rate" => "Rate",
                    "none" => "NoKind",
                    other => panic!("unknown kind {other}"),
                };
                let wire = &c["wire"];
                let wire_t = match wire[0].as_str().expect("wire type") {
                    "sint32" => "S32",
                    "uint32" => "U32",
                    other => panic!("unknown wire type {other}"),
                };
                let hist = &c["history"];
                let hist_t = match hist[0].as_str().expect("history enc") {
                    "u8" => "U8",
                    "i16" => "I16",
                    "u16" => "U16",
                    "i32" => "I32",
                    "u32" => "H32",
                    other => panic!("unknown history enc {other}"),
                };
                let range = c
                    .get("range")
                    .map(|r| format!("Some(({}, {}))", f(&r[0]), f(&r[1])))
                    .unwrap_or_else(|| "None".into());
                let flag = |key: &str, name: &str| {
                    c.get(key)
                        .and_then(serde_yaml::Value::as_bool)
                        .unwrap_or(false)
                        .then(|| name.to_string())
                };
                let flags: Vec<String> = [
                    flag("momentary", "MOMENTARY"),
                    flag("counter", "COUNTER"),
                    flag("alarm_only_watchdog", "WATCHDOG_ONLY"),
                    flag("liveness", "LIVENESS"),
                    flag("retired", "RETIRED"),
                ]
                .into_iter()
                .flatten()
                .collect();
                let flags = if flags.is_empty() {
                    "0".to_string()
                } else {
                    flags.join(" | ")
                };
                out.push_str(&format!(
                    "            ch({}, {}, {}, {}, {}, {kind}, ({wire_t}, {}), ({hist_t}, {}), {range}, {}, {}, {}, {flags}),\n",
                    c["ch"].as_u64().expect("ch"),
                    s(&c["name"]),
                    s(&c["label"]),
                    s(&c["quantity"]),
                    s(&c["unit"]),
                    f(&wire[1]),
                    f(&hist[1]),
                    opt_s(c.get("cap")),
                    opt_s(c.get("pulses")),
                    opt_s(c.get("part")),
                ));
            }
            out.push_str("        ],\n    },\n");
        }
        out.push_str("];\n");
        out
    }

    #[test]
    fn the_table_is_the_vendored_registry() {
        let begin = "// BEGIN GENERATED SENSOR_TYPES\n";
        let start = SOURCE.find(begin).expect("begin marker") + begin.len();
        let end = SOURCE
            .find("// END GENERATED SENSOR_TYPES")
            .expect("end marker");
        let rendered = render(YAML);
        assert!(
            SOURCE[start..end] == rendered,
            "the table differs from data/app_sensor_types.v2.0.0.yaml; paste this between the \
             markers:\n{rendered}"
        );
    }

    #[test]
    fn the_registry_header_names_the_version_the_table_was_built_for() {
        let doc: serde_yaml::Value = serde_yaml::from_str(YAML).expect("parses");
        assert_eq!(doc["version"].as_u64(), Some(1));
    }

    /// v2.0.0 renumbered the 1-Wire types against v1.4.0's `app_w1_slot_type`.
    #[test]
    fn type_ids_moved_and_label_like_the_firmware_decoder() {
        assert_eq!(w1_type_label(0), "empty");
        assert_eq!(w1_type_label(2), "dallas");
        assert_eq!(w1_type_label(3), "machine-probe");
        assert_eq!(
            w1_type_label(1),
            "type1",
            "the motherboard is never a slot type"
        );
        assert_eq!(w1_type_label(9), "type9");
        assert_eq!(w1_type_id("machine-probe"), Some(3));
        assert_eq!(w1_type_id("empty"), None);
    }

    #[test]
    fn hub_fields_keep_the_live_names() {
        assert_eq!(hub_field(0, None, 0).as_deref(), Some("temperature"));
        assert_eq!(
            hub_field(0, None, 5).as_deref(),
            Some("hall_left_is_active")
        );
        assert_eq!(hub_field(0, None, 16).as_deref(), Some("motion_count"));
        assert_eq!(hub_field(0, None, 20).as_deref(), Some("voltage"));
        assert_eq!(hub_field(0, None, 4).as_deref(), Some("temperature_baro"));
        assert_eq!(
            hub_field(1, Some(2), 0).as_deref(),
            Some("w1_1_temperature")
        );
        assert_eq!(
            hub_field(2, Some(3), 2).as_deref(),
            Some("w1_2_temperature_aux")
        );
        assert_eq!(hub_field(3, Some(3), 5).as_deref(), Some("w1_3_tilt"));
        assert_eq!(hub_field(1, Some(2), 1), None, "dallas has one channel");
        assert_eq!(hub_field(1, None, 0), None, "a 1-Wire slot needs its type");
        assert_eq!(hub_field(1, Some(1), 0), None, "never the motherboard type");
    }

    #[test]
    fn a_field_names_its_slot_and_channel_back() {
        let mp = |_| Some(3);
        let (slot, t, c) = field_channel("w1_2_temperature_aux", mp).expect("known");
        assert_eq!((slot, t.id, c.ch), (2, 3, 2));
        let (slot, t, c) = field_channel("voltage", mp).expect("known");
        assert_eq!((slot, t.id, c.ch), (0, 1, 20));
        assert!(field_channel("w1_2_humidity", |_| Some(2)).is_none());
        assert!(field_channel("w1_5_temperature", mp).is_none());
        assert!(field_channel("w1_1_temperature", |_| None).is_none());
        assert!(field_channel("no_such_field", mp).is_none());
        // Every motherboard channel round-trips.
        for c in sensor_type(MOTHERBOARD).expect("mb").channels {
            let f = hub_field_of(0, c);
            assert_eq!(
                field_channel(&f, mp).map(|(_, _, c)| c.ch),
                Some(c.ch),
                "{f}"
            );
        }
    }

    #[test]
    fn rule_targets_exclude_none_and_watchdog_channels() {
        let mb = sensor_type(MOTHERBOARD).expect("mb");
        assert!(mb.channel(0).expect("temperature").rule_target());
        assert!(!mb.channel(19).expect("orientation").rule_target());
        assert!(!mb.channel(20).expect("battery").rule_target());
        assert!(!mb.channel(21).expect("altitude").rule_target());
        assert!(mb.channel(17).expect("accel-motion").momentary());
    }

    #[test]
    fn slot_labels_round_trip() {
        for slot in 0..=4u8 {
            assert_eq!(slot_of_label(&slot_label(u32::from(slot))), Some(slot));
        }
        assert_eq!(slot_label(7), "slot7");
        assert_eq!(slot_of_label("s5"), None);
    }
}
