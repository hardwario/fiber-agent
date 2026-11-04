//! On-device alarm-rule slot encoder — the subset the Portal needs to push a
//! **critical** field threshold to a Node so the Node evaluates it itself (PI-A).
//!
//! The decision (2026-09-17): the Node evaluates only the *critical* bounds, so
//! only `critical_low`/`critical_high` become an on-device `alarms.alarm_N`
//! rule; the *warning* bounds stay Hub-side (`set_lorawan_field_threshold`), a
//! deliberate departure from p2p_integration.md §4.4 / OQ2. The threshold UI
//! always requires **both** critical bounds, so a Node rule is a two-sided
//! `[lo, hi]` band by construction (the one-sided arms below are defensive).
//!
//! The last `f32` is the **dwell** time (how long the value must stay out of the
//! band before the Node raises the alarm). `from_state`/`to_state` are 0 for an
//! analog threshold rule. The **enabled** bit carries the threshold's own on/off, so disabling a threshold
//! silences the Node's rule without deleting its bounds.
//!
//! *Amended 2026-09-24 (nodes_phase1.cs.md WP2):* the portal is the source of truth for a
//! Node's alarms and reads none back. Slots follow the order of the Node's active quantities
//! it can watch ([`slot_layout`]), and every write replaces the whole table ([`alarm_table`]
//! with the firmware's `SetParam.alarms_replace`), so a slot changed over NFC cannot survive.
//!
//! *Amended 2026-10-09 (sticker-firmware v2.0.0, #119):* a rule is the 18-byte
//! `(slot, channel, sensor_type)` layout of `control_radio::decode::alarm_rule`, which the
//! Portal now uses directly instead of carrying its own copy; the target of a field comes from
//! the sensor type registry, and a 1-Wire rule names the slot's type (`node-firmware-v2.0.0.md`
//! §2, §7).
//!
//! *Amended 2026-10-12 (MR !129 review, §7):* a rule names the slot's configured
//! `sensorN_type` only, never the detected type; only a field with a band takes a slot; and a
//! table that cannot carry every band (an unknown slot type, more than 16 bands) is refused
//! ([`TableRefusal`]) instead of dropping the band from the Node.

use control_radio::decode::alarm_rule::Rule;
use control_radio::decode::sensor_types::{self, Channel, Kind, W1_SLOTS};

/// Dwell time for a Portal-pushed critical threshold, in seconds. `0.0` = the
/// Node alarms as soon as the value leaves the band, which is the intended
/// default for a critical bound.
const DEFAULT_DWELL: f32 = 0.0;

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// How many alarm rules a Node holds (`alarms.alarm_0`–`alarm_15`).
pub(crate) const NODE_ALARM_SLOTS: usize = 16;

/// The sensor type id of each 1-Wire slot, s1..s4, as a rule is written for it
/// (`node-firmware-v2.0.0.md` §7): `0` = unknown, and a field of that slot gets no rule.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SlotTypes(pub [u8; W1_SLOTS as usize]);

impl SlotTypes {
    /// The type of 1-Wire slot `slot` (1..4), `None` when unknown.
    pub fn of(&self, slot: u8) -> Option<u8> {
        let i = usize::from(slot).checked_sub(1)?;
        self.0.get(i).copied().filter(|&t| t != 0)
    }

    /// A Node's slot types as a rule is written for them (§7, *amended 2026-10-12*): the
    /// configured `sensors.sensorN_type` only. The firmware holds a rule inert whenever its
    /// `sensor_type` differs from the slot's configured type, `0` included
    /// (`app_alarm_rules.c`), so the type the Node detected (`w1_slot_type`) is no target: a
    /// rule written for it would be stale on arrival.
    pub fn of_node(entry: Option<&crate::state::NodeConfigEntry>) -> Self {
        let mut out = [0u8; W1_SLOTS as usize];
        if let Some(entry) = entry {
            for (i, t) in out.iter_mut().enumerate() {
                *t = configured_type(entry, i);
            }
        }
        Self(out)
    }

    /// What a Node's slots measure, for what does not name a type on the wire (the history
    /// columns, `slot << 5 | ch`): the configured type, else the detected one, else unknown.
    pub fn measured(entry: Option<&crate::state::NodeConfigEntry>) -> Self {
        let mut out = [0u8; W1_SLOTS as usize];
        let Some(entry) = entry else {
            return Self(out);
        };
        for (i, t) in out.iter_mut().enumerate() {
            let detected = || {
                entry
                    .w1_slot_type
                    .as_ref()
                    .and_then(|names| names.get(i))
                    .and_then(|name| sensor_types::w1_type_id(name))
            };
            *t = Some(configured_type(entry, i))
                .filter(|&v| v != 0)
                .or_else(detected)
                .unwrap_or(0);
        }
        Self(out)
    }
}

/// `sensors.sensor{i+1}_type` from the Node's held config, `0` when unset or unreadable.
fn configured_type(entry: &crate::state::NodeConfigEntry, i: usize) -> u8 {
    entry
        .config
        .get(&format!("sensors.sensor{}_type", i + 1))
        .and_then(serde_json::Value::as_u64)
        .and_then(|v| u8::try_from(v).ok())
        .unwrap_or(0)
}

/// The marker the stored layout carries the slot types under (`node_alarm_layout`, a JSON list
/// of field names): one trailing entry no field name can be, so a reader that only looks
/// fields up by position never sees it, and the row needs no migration.
const TYPES_MARK: &str = "@sensor_types:";

/// The layout as it is stored once the Node takes the table: the fields in slot order, then
/// the slot types the rules were written for.
pub fn stored_layout(layout: &[String], types: &SlotTypes) -> Vec<String> {
    let mut out = layout.to_vec();
    let types: Vec<String> = types.0.iter().map(u8::to_string).collect();
    out.push(format!("{TYPES_MARK}{}", types.join(",")));
    out
}

/// A stored layout split back into its fields and the slot types it was written for; `None`
/// for the types of a row written before they were recorded.
pub fn parse_stored_layout(stored: Vec<String>) -> (Vec<String>, Option<SlotTypes>) {
    let mut types = None;
    let fields = stored
        .into_iter()
        .filter(|entry| match entry.strip_prefix(TYPES_MARK) {
            Some(list) => {
                let ids: Vec<u8> = list.split(',').filter_map(|v| v.parse().ok()).collect();
                types = <[u8; W1_SLOTS as usize]>::try_from(ids).ok().map(SlotTypes);
                false
            }
            None => true,
        })
        .collect();
    (fields, types)
}

/// Where a Portal-pushed rule for a field lands on the Node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RuleTarget {
    pub slot: u8,
    pub channel: u8,
    pub sensor_type: u8,
}

/// A channel the Portal can push a critical band to: a threshold channel the firmware accepts
/// a rule on (`app_alarm_rule_valid`). State and counter channels take no band; the battery is
/// watchdog-only.
fn band_channel(c: &Channel) -> bool {
    c.kind == Kind::Threshold && c.rule_target()
}

/// The on-device rule target of `field` on a Node whose 1-Wire slots hold `types`, per the
/// sensor type registry (`app_sensor_types.yaml`): a motherboard threshold channel on slot 0, or
/// a threshold channel of the slot's type on s1..s4. `None` for every other field, and for a
/// 1-Wire field whose slot type is unknown: a rule needs the type it is written for.
pub(crate) fn rule_target(field: &str, types: &SlotTypes) -> Option<RuleTarget> {
    let (slot, t, c) = sensor_types::field_channel(field, |slot| types.of(slot))?;
    band_channel(c).then_some(RuleTarget {
        slot,
        channel: c.ch,
        sensor_type: t.id,
    })
}

/// The threshold channel `field` names under any type its slot may hold: the motherboard's for a
/// board field, any 1-Wire type's for `w1_{n}_{channel}`. For checks that hold no slot type.
fn any_band_channel(field: &str) -> Option<&'static Channel> {
    sensor_types::TYPES.iter().find_map(|t| {
        let (_, _, c) = sensor_types::field_channel(field, |_| Some(t.id))?;
        band_channel(c).then_some(c)
    })
}

/// The unreachable bounds a critical band on `field` is completed with (P-3, nodes_phase2.cs.md
/// dodatek 2026-09-25): the open low and high side, for a field the Node watches itself; `None`
/// for any other, whose band the portal alone grades and may leave one-sided.
pub(crate) fn open_band(field: &str) -> Option<(f64, f64)> {
    let c = any_band_channel(field)?;
    let high = match c.quantity {
        "pressure" => 9_999.0,                     // hPa
        "illuminance" | "magnetic_field" => 1.0e6, // lux, mT
        _ => 199.0,                                // temperature, humidity, voltage, ...
    };
    // Below the channel's range, however wide (the magnetic field reaches -204.7 mT).
    let low = c.range.map_or(-99.0, |(lo, _)| (lo - 1.0).min(-99.0));
    Some((low, high))
}

/// The fields of a Node that can hold a slot (nodes_phase1.cs.md WP2): the ones it can watch,
/// in the order of its active quantities (board, `cap_*` sensors, 1-Wire slots).
///
/// *Amended 2026-10-12 (MR !129 review):* not cut at 16 here. Only a field with a band takes a
/// slot ([`alarm_table`]), and a table with more banded fields than slots is refused there
/// rather than truncated.
pub fn slot_layout<'a>(active_fields: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    active_fields
        .into_iter()
        .filter(|f| any_band_channel(f).is_some())
        .map(str::to_string)
        .collect()
}

/// One field's critical band as the portal holds it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CriticalBand {
    pub low: Option<f64>,
    pub high: Option<f64>,
    pub enabled: bool,
}

impl CriticalBand {
    /// Both bounds, when they make a band the firmware takes (`lo < hi`, #203).
    fn bounds(&self) -> Option<(f32, f32)> {
        let (lo, hi) = (self.low? as f32, self.high? as f32);
        (hi > lo).then_some((lo, hi))
    }
}

/// Why the portal does not send a Node's alarm table. The table replaces every slot, so a
/// band it could not carry would be dropped from the Node without a word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableRefusal {
    /// Banded 1-Wire fields whose slot has no configured `sensorN_type` the field's channel
    /// belongs to: a rule for any other type is inert on the Node (§7).
    UnknownType(Vec<String>),
    /// More banded fields than the Node's 16 slots; the ones past the 16th, in slot order.
    TooMany(Vec<String>),
}

/// The alarm table a send carries.
#[derive(Debug, Clone, PartialEq)]
pub struct AlarmTable {
    /// The banded fields in slot order: slot `i` is `alarms.alarm_i`. What the Node holds once
    /// it confirms.
    pub layout: Vec<String>,
    /// `alarms.alarm_i=<36 hex>` (the 18-byte v2.0.0 rule, `control_radio::decode::alarm_rule`)
    /// for each slot of `layout`.
    pub assignments: Vec<String>,
}

/// The whole alarm table as `set-config` assignments, to send with `alarms_replace` (the
/// firmware clears every slot first).
///
/// *Amended 2026-10-12 (MR !129 review, node-firmware-v2.0.0.md §7):* the slots go to the
/// fields of `candidates` ([`slot_layout`]) with a band, in that order: both critical bounds set
/// with `low < high`, `enabled` per the threshold's own on/off (a disabled threshold keeps its
/// rule, silenced). A field without a band takes no slot. Every banded field must get its rule:
/// a 1-Wire field whose slot type (`types`, the configured one) does not carry it, or a band
/// past the 16th, refuses the whole table instead of leaving the field out of the replace.
pub fn alarm_table(
    candidates: &[String],
    types: &SlotTypes,
    band: impl Fn(&str) -> Option<CriticalBand>,
) -> Result<AlarmTable, TableRefusal> {
    let mut rules: Vec<(String, Rule)> = Vec::new();
    let mut untyped = Vec::new();
    for field in candidates {
        let Some((b, (lo, hi))) = band(field).and_then(|b| Some((b, b.bounds()?))) else {
            continue;
        };
        let rule = rule_target(field, types).map(|target| Rule {
            enabled: b.enabled,
            slot: target.slot,
            channel: target.channel,
            sensor_type: target.sensor_type,
            from_state: 0,
            to_state: 0,
            lo,
            hi,
            dwell: DEFAULT_DWELL,
        });
        match rule.filter(|r| r.validate().is_ok()) {
            Some(rule) => rules.push((field.clone(), rule)),
            None => untyped.push(field.clone()),
        }
    }
    if !untyped.is_empty() {
        return Err(TableRefusal::UnknownType(untyped));
    }
    if rules.len() > NODE_ALARM_SLOTS {
        return Err(TableRefusal::TooMany(
            rules
                .split_off(NODE_ALARM_SLOTS)
                .into_iter()
                .map(|(f, _)| f)
                .collect(),
        ));
    }
    Ok(AlarmTable {
        assignments: rules
            .iter()
            .enumerate()
            .map(|(i, (_, rule))| format!("alarms.alarm_{i}={}", to_hex(&rule.pack())))
            .collect(),
        layout: rules.into_iter().map(|(f, _)| f).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn band(low: f64, high: f64) -> Option<CriticalBand> {
        Some(CriticalBand {
            low: Some(low),
            high: Some(high),
            enabled: true,
        })
    }

    /// s1 dallas, s2 machine-probe.
    const TYPES: SlotTypes = SlotTypes([2, 3, 0, 0]);

    /// Onboard temperature, present+enabled, lo=15 hi=25, dwell 0: the 18-byte rule names the
    /// motherboard type (1) after slot and channel.
    #[test]
    fn matches_the_firmware_layout() {
        let layout = slot_layout(["temperature"]);
        let table = alarm_table(&layout, &SlotTypes::default(), |_| band(15.0, 25.0)).unwrap();
        assert_eq!(table.layout, ["temperature"]);
        assert_eq!(
            table.assignments,
            ["alarms.alarm_0=030000010000000070410000c84100000000"]
        );
    }

    /// The HIL rule of #119: s1 machine-probe temperature, lo 10, hi 30.
    #[test]
    fn the_hil_rule_is_what_a_machine_probe_band_sends() {
        let layout = slot_layout(["w1_1_temperature"]);
        let table = alarm_table(&layout, &SlotTypes([3, 0, 0, 0]), |_| band(10.0, 30.0)).unwrap();
        assert_eq!(
            table.assignments,
            ["alarms.alarm_0=030100030000000020410000f04100000000"]
        );
    }

    /// Hynek's example: temperature, humidity, Dallas are slots 0-2, in that order; the battery
    /// and a counter get no slot, a machine probe's quantities follow its slot.
    #[test]
    fn slots_follow_the_order_of_the_active_quantities_the_node_watches() {
        let layout = slot_layout([
            "temperature",
            "humidity",
            "voltage",
            "input_a_count",
            "w1_1_temperature",
            "w1_2_temperature",
            "w1_2_humidity",
            "w1_2_temperature_aux",
            "w1_2_illuminance",
            "w1_2_magnetic_field",
            "w1_2_tilt",
        ]);
        assert_eq!(
            layout,
            [
                "temperature",
                "humidity",
                "w1_1_temperature",
                "w1_2_temperature",
                "w1_2_humidity",
                "w1_2_temperature_aux",
                "w1_2_illuminance",
                "w1_2_magnetic_field",
            ]
        );
        let t = |slot, channel, sensor_type| {
            Some(RuleTarget {
                slot,
                channel,
                sensor_type,
            })
        };
        assert_eq!(
            rule_target("w1_1_temperature", &TYPES),
            t(1, 0, 2),
            "s1 dallas"
        );
        assert_eq!(rule_target("w1_2_magnetic_field", &TYPES), t(2, 4, 3));
        assert_eq!(rule_target("w1_2_temperature_aux", &TYPES), t(2, 2, 3));
        assert_eq!(rule_target("pressure", &TYPES), t(0, 2, 1));
        assert_eq!(
            rule_target("w1_1_humidity", &TYPES),
            None,
            "dallas has none"
        );
        assert_eq!(
            rule_target("w1_3_temperature", &TYPES),
            None,
            "s3 type unknown"
        );
        assert_eq!(rule_target("w1_2_tilt", &TYPES), None, "a state, no band");
        assert_eq!(rule_target("voltage", &TYPES), None, "watchdog-only");
        assert_eq!(rule_target("altitude", &TYPES), None);
        assert_eq!(rule_target("w1_5_temperature", &TYPES), None, "four slots");
    }

    /// MR !129 review: only a banded field takes a slot, so a field without a band leaves no
    /// hole; the table is replaced whole, and the slots follow the candidates' order.
    #[test]
    fn only_banded_fields_take_a_slot_in_candidate_order() {
        let layout = slot_layout(["temperature", "humidity", "w1_1_temperature"]);
        let table = alarm_table(&layout, &TYPES, |f| match f {
            "temperature" => band(15.0, 25.0),
            "w1_1_temperature" => band(2.0, 8.0),
            _ => None,
        })
        .unwrap();
        assert_eq!(table.layout, ["temperature", "w1_1_temperature"]);
        assert_eq!(table.assignments.len(), 2);
        assert!(table.assignments[0].starts_with("alarms.alarm_0=03"));
        let dallas = &table.assignments[1];
        assert!(
            dallas.starts_with("alarms.alarm_1="),
            "humidity has no band and takes no slot"
        );
        let hex = dallas.split_once('=').unwrap().1;
        assert_eq!(&hex[2..4], "01", "slot byte = s1");
        assert_eq!(&hex[4..6], "00", "channel byte = temperature");
        assert_eq!(&hex[6..8], "02", "sensor type byte = dallas");
    }

    /// MR !129 review: a banded 1-Wire field whose slot has no configured type (or one that
    /// does not carry the field) would get no rule, and the replace would drop it from the Node
    /// silently. The table is refused, naming the fields.
    #[test]
    fn a_banded_field_of_an_unknown_slot_type_refuses_the_table() {
        let layout = slot_layout(["temperature", "w1_1_temperature", "w1_2_humidity"]);
        assert_eq!(
            alarm_table(&layout, &SlotTypes::default(), |_| band(2.0, 8.0)),
            Err(TableRefusal::UnknownType(vec![
                "w1_1_temperature".into(),
                "w1_2_humidity".into()
            ]))
        );
        // s2 configured as Dallas: it has no humidity.
        assert_eq!(
            alarm_table(&layout, &SlotTypes([2, 2, 0, 0]), |_| band(2.0, 8.0)),
            Err(TableRefusal::UnknownType(vec!["w1_2_humidity".into()]))
        );
        // Without a band the field needs no rule, and nothing is refused.
        let table = alarm_table(&layout, &SlotTypes::default(), |f| {
            (f == "temperature").then(|| band(2.0, 8.0)).flatten()
        })
        .unwrap();
        assert_eq!(table.layout, ["temperature"]);
    }

    #[test]
    fn a_disabled_threshold_sends_a_present_but_disabled_rule() {
        let layout = slot_layout(["temperature"]);
        let table = alarm_table(&layout, &TYPES, |_| {
            Some(CriticalBand {
                low: Some(15.0),
                high: Some(25.0),
                enabled: false,
            })
        })
        .unwrap();
        let hex = table.assignments[0].split_once('=').unwrap().1;
        // flags byte: present (bit0) set, enabled (bit1) clear -> 0x01.
        assert_eq!(&hex[0..2], "01");
    }

    /// One-sided, inverted and empty bands: the firmware refuses `hi <= lo` (#203).
    #[test]
    fn a_one_sided_inverted_or_empty_band_fills_no_slot() {
        let layout = slot_layout(["temperature"]);
        for (low, high) in [
            (None, Some(25.0)),
            (Some(25.0), Some(15.0)),
            (Some(5.0), Some(5.0)),
        ] {
            let b = CriticalBand {
                low,
                high,
                enabled: true,
            };
            let table = alarm_table(&layout, &TYPES, |_| Some(b)).unwrap();
            assert!(table.assignments.is_empty() && table.layout.is_empty());
        }
    }

    /// MR !129 review: the 16 slots go to banded fields only, and more than 16 banded fields
    /// refuse the table instead of dropping the rest.
    #[test]
    fn sixteen_slots_go_to_banded_fields_and_more_are_refused() {
        let fields: Vec<String> = ["temperature".to_string(), "humidity".to_string()]
            .into_iter()
            .chain((1..=4).flat_map(|n| {
                ["temperature", "humidity", "illuminance", "magnetic_field"]
                    .map(|q| format!("w1_{n}_{q}"))
            }))
            .collect();
        let layout = slot_layout(fields.iter().map(String::as_str));
        assert_eq!(layout.len(), 18, "candidates are not cut");
        let probes = SlotTypes([3, 3, 3, 3]);

        // 18 candidates, the two board fields without a band: 16 banded, all slotted.
        let table = alarm_table(&layout, &probes, |f| {
            f.starts_with("w1_").then(|| band(1.0, 2.0)).flatten()
        })
        .unwrap();
        assert_eq!(table.layout.len(), NODE_ALARM_SLOTS);
        assert_eq!(table.layout[0], "w1_1_temperature");
        assert!(table.assignments[15].starts_with("alarms.alarm_15="));

        // All 18 banded: refused, naming the two past the 16th.
        assert_eq!(
            alarm_table(&layout, &probes, |_| band(1.0, 2.0)),
            Err(TableRefusal::TooMany(vec![
                "w1_4_illuminance".into(),
                "w1_4_magnetic_field".into()
            ]))
        );
    }

    #[test]
    fn open_bands_lie_outside_every_channel_range() {
        assert_eq!(open_band("temperature"), Some((-99.0, 199.0)));
        assert_eq!(open_band("pressure"), Some((-99.0, 9_999.0)));
        let (low, high) = open_band("w1_2_magnetic_field").expect("machine-probe");
        assert!(low < -204.7 && high > 204.7);
        assert_eq!(open_band("w1_1_temperature"), Some((-99.0, 199.0)));
        assert_eq!(open_band("voltage"), None, "watchdog-only");
        assert_eq!(open_band("w1_1_tilt"), None);
    }

    /// MR !129 review: a rule names the configured `sensorN_type` only; the detected type
    /// would make it inert on the Node. The history columns may still read the detected one.
    #[test]
    fn rule_slot_types_are_the_configured_ones_only() {
        let mut entry = crate::state::NodeConfigEntry {
            w1_slot_type: Some(
                ["dallas", "machine-probe", "empty", "type9"]
                    .map(String::from)
                    .to_vec(),
            ),
            ..Default::default()
        };
        assert_eq!(SlotTypes::of_node(Some(&entry)), SlotTypes::default());
        assert_eq!(SlotTypes::measured(Some(&entry)), SlotTypes([2, 3, 0, 0]));
        entry
            .config
            .insert("sensors.sensor1_type".into(), serde_json::json!(3));
        entry
            .config
            .insert("sensors.sensor2_type".into(), serde_json::json!(0));
        assert_eq!(SlotTypes::of_node(Some(&entry)), SlotTypes([3, 0, 0, 0]));
        assert_eq!(SlotTypes::measured(Some(&entry)), SlotTypes([3, 3, 0, 0]));
        assert_eq!(SlotTypes::of_node(None), SlotTypes::default());
    }

    #[test]
    fn the_stored_layout_carries_the_slot_types_it_was_written_for() {
        let layout = vec!["temperature".to_string(), "w1_1_temperature".to_string()];
        let stored = stored_layout(&layout, &SlotTypes([2, 0, 3, 0]));
        assert_eq!(stored.len(), 3);
        assert_eq!(
            parse_stored_layout(stored),
            (layout.clone(), Some(SlotTypes([2, 0, 3, 0])))
        );
        assert_eq!(
            parse_stored_layout(layout.clone()),
            (layout, None),
            "an older row"
        );
    }
}
