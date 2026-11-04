//! LoRaWAN Node field registry — `proximos-v2/plan/v1/viewer/ws.md`
//! §"Field registries" / §"LoRaWAN registry" (`:191-211`), which documents
//! the v1 Python `app/lorawan/field_registry.py` (25 entries).
//!
//! This is the STICKER field registry, not a LoRaWAN-only one — it stays
//! transport-agnostic per `plan/viewer/p2p_nodes.md` §1 (a P2P Node reports
//! the identical field/threshold set) and keeps its module name/path.
//!
//! # Sourcing
//!
//! - `name`, `group`, `kind`, `thresholdable`, `threshold_kind` for the 17
//!   continuous/counter fields are taken from the device agent's own
//!   registry, `application/src/libs/lorawan/registry.rs:37-160`
//!   (`REGISTRY`) — the real enforcement point for what the firmware
//!   accepts, and normative per this deliverable's brief ("field names/
//!   attributes MUST match it").
//! - `label`, `unit`, `chartable`, and the 8 `event`-kind fields (which the
//!   agent's registry does not define at all — see "Cross-check findings"
//!   below) come from ws.md, since the agent has no equivalent to
//!   cross-check them against.
//!
//! ## Cross-check findings (agent `registry.rs` vs. ws.md)
//!
//! *Amended 2026-09-28 (plan/portal/threshold_defaults.md §5):* **`voltage` is not
//! thresholdable**, here or in the agent's registry, which withdrew it first: the Node
//! raises its own low-battery alarm (fPort 3, source `battery`), so a band on the battery
//! only duplicated it. It is not a sensor of the Node either — the portal shows it as the
//! battery (the list's column, the device info card) and in no sensor list. The finding
//! below is what the registries said until then, kept as the record of why the count moved.
//!
//! - **`voltage` is thresholdable in the agent; ws.md says it isn't.** The
//!   agent's `REGISTRY` (`registry.rs:117-123`) defines `voltage` as
//!   `thresholdable: true, threshold_kind: Some(ThresholdKind::LowOnly)`
//!   — "Battery — low_only (no alarm on 'too high')". ws.md's table marks
//!   `voltage` `thresholdable: –, chartable: –`, and its prose states "11
//!   thresholdable (all continuous except `voltage`)" and "`threshold_kind`
//!   also declares `low_only` but no field uses it — every thresholdable
//!   field is `range`." Both of those ws.md claims are contradicted by the
//!   agent's own source, which is the code that actually decides whether a
//!   threshold write against a Node's voltage field does anything. **Per
//!   this deliverable's rule ("application wins"), `voltage` here is
//!   `thresholdable: true, threshold_kind: LowOnly`** — making the true
//!   thresholdable count **12**, not the 11 ws.md states (see
//!   `twelve_thresholdable_per_agent_voltage_disagreement` below). `chartable`
//!   has no equivalent in the agent's registry at all (Rust doesn't track
//!   it), so `voltage`'s `chartable: false` is taken from ws.md as-is —
//!   nothing to cross-check there.
//! - **The 8 `event`-kind fields don't exist in the agent's `REGISTRY` at
//!   all** (`boot`, `orientation`, `tilt_alert_1`, `tilt_alert_2`,
//!   `hall_active`, `hall_notify`, `input_active`, `input_notify`). This
//!   isn't a value disagreement — the agent's `FieldKind::Event` and
//!   `FieldGroup::States` variants exist but are never instantiated in
//!   `REGISTRY`, which matches `rest.md` §"Display-lines catalog": "Events
//!   are excluded because they don't exist in the device's Rust registry —
//!   offering one would produce a command the device rejects." There is
//!   nothing to cross-check for these 8: ws.md is the only source, and its
//!   label column is a placeholder ("(labels as named)"), not a literal
//!   string — the labels below are a best-effort title-case of the field
//!   name, a judgment call flagged here rather than a value pinned by
//!   either source.
//! - All other attributes (name, group, kind, thresholdable,
//!   threshold_kind) for the remaining 16 continuous/counter fields agree
//!   between the two sources.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Continuous,
    Counter,
    Event,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Main,
    ExternalTemp,
    Probe,
    Environmental,
    Battery,
    Counters,
    States,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThresholdKind {
    Range,
    LowOnly,
}

#[derive(Debug, Clone, Copy)]
pub struct FieldDef {
    pub name: &'static str,
    pub label: &'static str,
    pub unit: Option<&'static str>,
    pub group: Group,
    pub kind: Kind,
    pub thresholdable: bool,
    pub chartable: bool,
    pub threshold_kind: Option<ThresholdKind>,
}

/// The one display conversion the portal applies to a Node field: a value
/// reported in Pa is shown in hPa (UX-038, decided 2026-09-26), because
/// "101328.8 Pa" is not how anyone reads air pressure. `unit` stays the wire
/// unit; the wire, the database, the stored alarm row, the webhook and CSV
/// exports keep it. Since the v2 decoder `pressure` arrives in hPa already,
/// so no registry field takes the conversion today; it stays for a field
/// that reports Pa. Every other unit is shown as sent.
impl FieldDef {
    /// Wire units per displayed unit.
    fn display_divisor(&self) -> f64 {
        match self.unit {
            Some("Pa") => 100.0,
            _ => 1.0,
        }
    }

    /// The unit the portal shows this field in.
    pub fn display_unit(&self) -> Option<&'static str> {
        match self.unit {
            Some("Pa") => Some("hPa"),
            unit => unit,
        }
    }

    /// A wire value (a reading or a threshold bound) as displayed.
    pub fn to_display(&self, wire: f64) -> f64 {
        wire / self.display_divisor()
    }

    /// A displayed value (what an operator typed) back in wire units.
    /// Rounded to a thousandth of a wire unit when scaled, so 1013.3 hPa
    /// goes out as 101330, not 101330.00000000001.
    pub fn from_display(&self, shown: f64) -> f64 {
        let divisor = self.display_divisor();
        if divisor == 1.0 {
            return shown;
        }
        (shown * divisor * 1000.0).round() / 1000.0
    }

    /// The quantity whose default band this field gets when its Node is registered
    /// (plan/portal/threshold_defaults.md §3): a thresholdable continuous field of a
    /// temperature or humidity unit.
    pub fn default_quantity(&self) -> Option<crate::fields::Quantity> {
        if !self.thresholdable || self.kind != Kind::Continuous {
            return None;
        }
        // The barometer's temperature reads the same air as `temperature` on the same
        // board; a second default band would double every board temperature alarm.
        if self.name == "temperature_baro" {
            return None;
        }
        crate::fields::Quantity::of_unit(self.unit)
    }
}

/// A thresholdable, chartable continuous field (the common case — every
/// continuous field except `voltage`, see "Cross-check findings").
const fn continuous(
    name: &'static str,
    label: &'static str,
    unit: &'static str,
    group: Group,
) -> FieldDef {
    FieldDef {
        name,
        label,
        unit: Some(unit),
        group,
        kind: Kind::Continuous,
        thresholdable: true,
        chartable: true,
        threshold_kind: Some(ThresholdKind::Range),
    }
}

/// A chartable, never-thresholdable counter.
const fn counter(name: &'static str, label: &'static str) -> FieldDef {
    FieldDef {
        name,
        label,
        unit: None,
        group: Group::Counters,
        kind: Kind::Counter,
        thresholdable: false,
        chartable: true,
        threshold_kind: None,
    }
}

/// A never-chartable, never-thresholdable discrete event. Not present in
/// the agent's registry — see "Cross-check findings".
const fn event(name: &'static str, label: &'static str) -> FieldDef {
    FieldDef {
        name,
        label,
        unit: None,
        group: Group::States,
        kind: Kind::Event,
        thresholdable: false,
        chartable: false,
        threshold_kind: None,
    }
}

/// A chartable, never-thresholdable state (0/1) reported as a value, like the
/// 1-Wire tilt state (v2.0.0). A rule on it is a transition, not a band.
const fn state(name: &'static str, label: &'static str) -> FieldDef {
    FieldDef {
        name,
        label,
        unit: None,
        group: Group::States,
        kind: Kind::Continuous,
        thresholdable: false,
        chartable: true,
        threshold_kind: None,
    }
}

pub const REGISTRY: &[FieldDef] = &[
    continuous("temperature", "Temperature", "\u{00b0}C", Group::Main),
    continuous("humidity", "Humidity", "%", Group::Main),
    // The battery: reported and shown, never a threshold — the Node alarms on it itself
    // (module doc, amendment 2026-09-28). Group::Battery is what keeps it out of the Node's
    // sensor lists.
    FieldDef {
        name: "voltage",
        label: "Battery",
        unit: Some("V"),
        group: Group::Battery,
        kind: Kind::Continuous,
        thresholdable: false,
        chartable: false,
        threshold_kind: None,
    },
    continuous(
        "ext_temperature_1",
        "External probe 1",
        "\u{00b0}C",
        Group::ExternalTemp,
    ),
    continuous(
        "ext_temperature_2",
        "External probe 2",
        "\u{00b0}C",
        Group::ExternalTemp,
    ),
    continuous(
        "machine_probe_temperature_1",
        "Machine probe 1 \u{2014} Temp",
        "\u{00b0}C",
        Group::Probe,
    ),
    continuous(
        "machine_probe_temperature_2",
        "Machine probe 2 \u{2014} Temp",
        "\u{00b0}C",
        Group::Probe,
    ),
    continuous(
        "machine_probe_humidity_1",
        "Machine probe 1 \u{2014} Hum",
        "%",
        Group::Probe,
    ),
    continuous(
        "machine_probe_humidity_2",
        "Machine probe 2 \u{2014} Hum",
        "%",
        Group::Probe,
    ),
    continuous("illuminance", "Illuminance", "lx", Group::Environmental),
    // hPa: the v2 decoder scales the firmware's hPa x10 (sticker-firmware v2.0.0 only,
    // nodes_phase1.cs.md Q12).
    continuous("pressure", "Pressure", "hPa", Group::Environmental),
    continuous("altitude", "Altitude", "m", Group::Environmental),
    counter("motion_count", "Motion"),
    counter("hall_left_count", "Hall left"),
    counter("hall_right_count", "Hall right"),
    counter("input_a_count", "Input A"),
    counter("input_b_count", "Input B"),
    // v2.0.0 (#396, #499): an analog GP input's voltage, telemetry 30/31 (mV on the wire,
    // V here). Thresholdable like any continuous field.
    continuous(
        "input_a_voltage",
        "Input voltage A",
        "V",
        Group::Environmental,
    ),
    continuous(
        "input_b_voltage",
        "Input voltage B",
        "V",
        Group::Environmental,
    ),
    // Event fields: absent from the agent's registry — see "Cross-check
    // findings". Labels are a best-effort title-case, not a pinned value.
    event("boot", "Boot"),
    // The Node's device-status alarm byte from `system_flags` (sticker-firmware #409, #90).
    event("alarm_status", "Alarm status"),
    event("orientation", "Orientation"),
    event("tilt_alert_1", "Tilt alert 1"),
    event("tilt_alert_2", "Tilt alert 2"),
    event("hall_active", "Hall active"),
    event("hall_notify", "Hall notify"),
    event("input_active", "Input active"),
    event("input_notify", "Input notify"),
    // v2 (sticker-firmware v2.0.0): the accelerometer's any-motion counter, beside PIR's
    // `motion_count`.
    counter("accel_motion_count", "Accelerometer motion"),
    // v2 1-Wire slots (`w1_{N}_{channel}`, control-radio `decode::reading`). The detail page
    // labels them with the slot's type from the Node's config info; these are the fallback
    // labels and what a chart needs (unit, chartable).
    //
    // *Amended 2026-10-09 (#119, node-firmware-v2.0.0.md §1.1):* every channel of every
    // 1-Wire type of the sensor type registry (control-radio `decode::sensor_types`, dallas
    // and machine-probe), which this crate does not depend on, so the list is written out
    // here; `temperature_aux`, `tilt` (the state, 0/1) and `accel_{x,y,z}` joined. The
    // `w1_{N}_tilt_alert` event and the pre-v2.0.0 names above stay for stored lines and
    // older firmware.
    continuous(
        "w1_1_temperature",
        "1W slot 1 \u{2014} Temperature",
        "\u{00b0}C",
        Group::Probe,
    ),
    continuous(
        "w1_1_humidity",
        "1W slot 1 \u{2014} Humidity",
        "%",
        Group::Probe,
    ),
    continuous(
        "w1_1_temperature_aux",
        "1W slot 1 \u{2014} Temperature (aux)",
        "\u{00b0}C",
        Group::Probe,
    ),
    continuous(
        "w1_1_illuminance",
        "1W slot 1 \u{2014} Illuminance",
        "lx",
        Group::Probe,
    ),
    continuous(
        "w1_1_magnetic_field",
        "1W slot 1 \u{2014} Magnetic field",
        "mT",
        Group::Probe,
    ),
    state("w1_1_tilt", "1W slot 1 \u{2014} Tilt state"),
    continuous(
        "w1_1_accel_x",
        "1W slot 1 \u{2014} Acceleration X",
        "m/s\u{00b2}",
        Group::Probe,
    ),
    continuous(
        "w1_1_accel_y",
        "1W slot 1 \u{2014} Acceleration Y",
        "m/s\u{00b2}",
        Group::Probe,
    ),
    continuous(
        "w1_1_accel_z",
        "1W slot 1 \u{2014} Acceleration Z",
        "m/s\u{00b2}",
        Group::Probe,
    ),
    event("w1_1_tilt_alert", "1W slot 1 \u{2014} Tilt"),
    continuous(
        "w1_2_temperature",
        "1W slot 2 \u{2014} Temperature",
        "\u{00b0}C",
        Group::Probe,
    ),
    continuous(
        "w1_2_humidity",
        "1W slot 2 \u{2014} Humidity",
        "%",
        Group::Probe,
    ),
    continuous(
        "w1_2_temperature_aux",
        "1W slot 2 \u{2014} Temperature (aux)",
        "\u{00b0}C",
        Group::Probe,
    ),
    continuous(
        "w1_2_illuminance",
        "1W slot 2 \u{2014} Illuminance",
        "lx",
        Group::Probe,
    ),
    continuous(
        "w1_2_magnetic_field",
        "1W slot 2 \u{2014} Magnetic field",
        "mT",
        Group::Probe,
    ),
    state("w1_2_tilt", "1W slot 2 \u{2014} Tilt state"),
    continuous(
        "w1_2_accel_x",
        "1W slot 2 \u{2014} Acceleration X",
        "m/s\u{00b2}",
        Group::Probe,
    ),
    continuous(
        "w1_2_accel_y",
        "1W slot 2 \u{2014} Acceleration Y",
        "m/s\u{00b2}",
        Group::Probe,
    ),
    continuous(
        "w1_2_accel_z",
        "1W slot 2 \u{2014} Acceleration Z",
        "m/s\u{00b2}",
        Group::Probe,
    ),
    event("w1_2_tilt_alert", "1W slot 2 \u{2014} Tilt"),
    continuous(
        "w1_3_temperature",
        "1W slot 3 \u{2014} Temperature",
        "\u{00b0}C",
        Group::Probe,
    ),
    continuous(
        "w1_3_humidity",
        "1W slot 3 \u{2014} Humidity",
        "%",
        Group::Probe,
    ),
    continuous(
        "w1_3_temperature_aux",
        "1W slot 3 \u{2014} Temperature (aux)",
        "\u{00b0}C",
        Group::Probe,
    ),
    continuous(
        "w1_3_illuminance",
        "1W slot 3 \u{2014} Illuminance",
        "lx",
        Group::Probe,
    ),
    continuous(
        "w1_3_magnetic_field",
        "1W slot 3 \u{2014} Magnetic field",
        "mT",
        Group::Probe,
    ),
    state("w1_3_tilt", "1W slot 3 \u{2014} Tilt state"),
    continuous(
        "w1_3_accel_x",
        "1W slot 3 \u{2014} Acceleration X",
        "m/s\u{00b2}",
        Group::Probe,
    ),
    continuous(
        "w1_3_accel_y",
        "1W slot 3 \u{2014} Acceleration Y",
        "m/s\u{00b2}",
        Group::Probe,
    ),
    continuous(
        "w1_3_accel_z",
        "1W slot 3 \u{2014} Acceleration Z",
        "m/s\u{00b2}",
        Group::Probe,
    ),
    event("w1_3_tilt_alert", "1W slot 3 \u{2014} Tilt"),
    continuous(
        "w1_4_temperature",
        "1W slot 4 \u{2014} Temperature",
        "\u{00b0}C",
        Group::Probe,
    ),
    continuous(
        "w1_4_humidity",
        "1W slot 4 \u{2014} Humidity",
        "%",
        Group::Probe,
    ),
    continuous(
        "w1_4_temperature_aux",
        "1W slot 4 \u{2014} Temperature (aux)",
        "\u{00b0}C",
        Group::Probe,
    ),
    continuous(
        "w1_4_illuminance",
        "1W slot 4 \u{2014} Illuminance",
        "lx",
        Group::Probe,
    ),
    continuous(
        "w1_4_magnetic_field",
        "1W slot 4 \u{2014} Magnetic field",
        "mT",
        Group::Probe,
    ),
    state("w1_4_tilt", "1W slot 4 \u{2014} Tilt state"),
    continuous(
        "w1_4_accel_x",
        "1W slot 4 \u{2014} Acceleration X",
        "m/s\u{00b2}",
        Group::Probe,
    ),
    continuous(
        "w1_4_accel_y",
        "1W slot 4 \u{2014} Acceleration Y",
        "m/s\u{00b2}",
        Group::Probe,
    ),
    continuous(
        "w1_4_accel_z",
        "1W slot 4 \u{2014} Acceleration Z",
        "m/s\u{00b2}",
        Group::Probe,
    ),
    event("w1_4_tilt_alert", "1W slot 4 \u{2014} Tilt"),
    // Board temperature of the barometer (motherboard channel 4, `temperature-baro`), the
    // Hub field the v2 radio publishes beside `temperature` (v2.0.0, #430).
    continuous(
        "temperature_baro",
        "Temperature (barometer)",
        "\u{00b0}C",
        Group::Environmental,
    ),
];

/// Look up one field by name.
pub fn lorawan_field(name: &str) -> Option<&'static FieldDef> {
    REGISTRY.iter().find(|f| f.name == name)
}

/// The full registry: v1's 25 entries plus the v2 fields sticker-firmware v2.0.0 adds.
pub fn lorawan_registry() -> &'static [FieldDef] {
    REGISTRY
}

#[cfg(test)]
mod tests {
    use super::*;

    /// UX-038 (2026-09-26): pressure is shown in hPa. The v2 decoder sends
    /// hPa, so it is shown as sent, like every other field; a field on the
    /// wire in Pa would be scaled both ways without float noise.
    #[test]
    fn pressure_is_displayed_in_hpa_and_converts_both_ways() {
        let p = lorawan_field("pressure").expect("pressure in registry");
        assert_eq!(p.unit, Some("hPa"), "the v2 decoder sends hPa");
        assert_eq!(p.display_unit(), Some("hPa"));
        assert_eq!(p.to_display(1013.288), 1013.288);
        assert_eq!(p.from_display(1013.3), 1013.3);

        let pa = FieldDef {
            unit: Some("Pa"),
            ..*p
        };
        assert_eq!(pa.display_unit(), Some("hPa"));
        assert!((pa.to_display(101_328.8) - 1013.288).abs() < 1e-9);
        assert_eq!(
            pa.from_display(1013.3),
            101_330.0,
            "no float noise on the wire"
        );

        let t = lorawan_field("temperature").expect("temperature in registry");
        assert_eq!(t.display_unit(), Some("\u{00b0}C"));
        assert_eq!(t.to_display(4.25), 4.25);
        assert_eq!(t.from_display(4.25), 4.25);
        assert_eq!(lorawan_field("motion_count").unwrap().display_unit(), None);
    }

    /// ws.md's 25 v1 fields, plus v2 (sticker-firmware v2.0.0): `accel_motion_count`,
    /// `alarm_status` (#90), `temperature_baro` and, per 1-Wire slot (four), the nine
    /// channels of the sensor type registry plus the `tilt_alert` event.
    #[test]
    fn the_v1_registry_plus_the_v2_fields() {
        assert_eq!(REGISTRY.len(), 25 + 2 + 1 + 2 + 4 * 10);
        for n in 1..=4 {
            for q in [
                "temperature",
                "humidity",
                "temperature_aux",
                "illuminance",
                "magnetic_field",
                "accel_x",
                "accel_y",
                "accel_z",
            ] {
                let f = lorawan_field(&format!("w1_{n}_{q}")).expect("slot quantity");
                assert!(f.chartable && f.thresholdable, "w1_{n}_{q}");
            }
            let tilt = lorawan_field(&format!("w1_{n}_tilt")).expect("tilt state");
            assert!(tilt.chartable && !tilt.thresholdable);
            assert!(lorawan_field(&format!("w1_{n}_tilt_alert")).is_some());
        }
        assert!(lorawan_field("temperature_baro").unwrap().thresholdable);
    }

    #[test]
    fn no_duplicate_names() {
        let mut seen = std::collections::HashSet::new();
        for f in REGISTRY {
            assert!(seen.insert(f.name), "duplicate: {}", f.name);
        }
    }

    /// ws.md states "11 thresholdable", and since the battery left (threshold_defaults.md
    /// §5) the agent's registry agrees: every continuous v1 field except `voltage`.
    #[test]
    fn eleven_thresholdable_v1_fields() {
        let n = REGISTRY
            .iter()
            .filter(|f| f.thresholdable && !f.name.starts_with("w1_"))
            .filter(|f| f.name != "temperature_baro")
            .filter(|f| !f.name.ends_with("_voltage"))
            .count();
        assert_eq!(n, 11, "v1 fields; the v2 fields come on top");
    }

    /// The Node's own low-battery alarm is the battery's alarm (threshold_defaults.md §5).
    #[test]
    fn voltage_is_the_battery_and_not_thresholdable() {
        let v = lorawan_field("voltage").expect("voltage in registry");
        assert!(!v.thresholdable);
        assert_eq!(v.threshold_kind, None);
        assert_eq!(v.group, Group::Battery);
        assert!(REGISTRY
            .iter()
            .filter(|f| f.group == Group::Battery)
            .all(|f| f.name == "voltage"));
    }

    #[test]
    fn temperature_is_thresholdable_range() {
        let t = lorawan_field("temperature").expect("temperature in registry");
        assert!(t.thresholdable);
        assert_eq!(t.threshold_kind, Some(ThresholdKind::Range));
    }

    #[test]
    fn counters_are_chartable_not_thresholdable() {
        let counters: Vec<_> = REGISTRY
            .iter()
            .filter(|f| f.kind == Kind::Counter)
            .collect();
        assert_eq!(counters.len(), 6, "v1's five + accel_motion_count");
        for f in counters {
            assert!(f.chartable, "{}", f.name);
            assert!(!f.thresholdable, "{}", f.name);
            assert_eq!(f.threshold_kind, None, "{}", f.name);
        }
    }

    #[test]
    fn events_are_neither_chartable_nor_thresholdable() {
        let events: Vec<_> = REGISTRY.iter().filter(|f| f.kind == Kind::Event).collect();
        assert_eq!(
            events.len(),
            13,
            "v1's eight + alarm_status + one tilt per 1-Wire slot"
        );
        for f in events {
            assert!(!f.chartable, "{}", f.name);
            assert!(!f.thresholdable, "{}", f.name);
            assert_eq!(f.threshold_kind, None, "{}", f.name);
        }
    }

    #[test]
    fn unknown_field_is_none() {
        assert!(lorawan_field("not_a_field").is_none());
    }
}
