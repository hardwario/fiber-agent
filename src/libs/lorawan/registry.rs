//! Hardcoded field registry for NODE LoRaWAN codec.
//! MUST mirror the Python and TypeScript registries.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Continuous,
    Counter,
    Event,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldGroup {
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
    pub kind: FieldKind,
    pub group: FieldGroup,
    pub thresholdable: bool,
    pub threshold_kind: Option<ThresholdKind>,
}

pub const REGISTRY: &[FieldDef] = &[
    // Continuous, thresholdable (range)
    FieldDef {
        name: "temperature",
        kind: FieldKind::Continuous,
        group: FieldGroup::Main,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "humidity",
        kind: FieldKind::Continuous,
        group: FieldGroup::Main,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "ext_temperature_1",
        kind: FieldKind::Continuous,
        group: FieldGroup::ExternalTemp,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "ext_temperature_2",
        kind: FieldKind::Continuous,
        group: FieldGroup::ExternalTemp,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "machine_probe_temperature_1",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "machine_probe_temperature_2",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "machine_probe_humidity_1",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "machine_probe_humidity_2",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "illuminance",
        kind: FieldKind::Continuous,
        group: FieldGroup::Environmental,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "pressure",
        kind: FieldKind::Continuous,
        group: FieldGroup::Environmental,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "altitude",
        kind: FieldKind::Continuous,
        group: FieldGroup::Environmental,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    // Battery — reported and displayed, but NOT thresholdable: the node
    // evaluates its own low-battery condition and reports it as a native alarm on
    // fPort 3, so a threshold here is a duplicate of an alarm the device already
    // raises. It was armed by default via `common_lorawan_field_thresholds`, so
    // every node had one without anyone configuring it.
    FieldDef {
        name: "voltage",
        kind: FieldKind::Continuous,
        group: FieldGroup::Battery,
        thresholdable: false,
        threshold_kind: None,
    },
    // Counters
    FieldDef {
        name: "motion_count",
        kind: FieldKind::Counter,
        group: FieldGroup::Counters,
        thresholdable: false,
        threshold_kind: None,
    },
    FieldDef {
        name: "hall_left_count",
        kind: FieldKind::Counter,
        group: FieldGroup::Counters,
        thresholdable: false,
        threshold_kind: None,
    },
    FieldDef {
        name: "hall_right_count",
        kind: FieldKind::Counter,
        group: FieldGroup::Counters,
        thresholdable: false,
        threshold_kind: None,
    },
    FieldDef {
        name: "input_a_count",
        kind: FieldKind::Counter,
        group: FieldGroup::Counters,
        thresholdable: false,
        threshold_kind: None,
    },
    FieldDef {
        name: "input_b_count",
        kind: FieldKind::Counter,
        group: FieldGroup::Counters,
        thresholdable: false,
        threshold_kind: None,
    },
];

pub fn lookup(name: &str) -> Option<&'static FieldDef> {
    REGISTRY.iter().find(|f| f.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voltage_is_reported_but_not_thresholdable() {
        // Phase 1: the node raises its own low-battery alarm on fPort 3, so the
        // viewer must not offer (or auto-arm) a second one. The field itself stays
        // in the registry because the value is still reported and displayed.
        let v = lookup("voltage").expect("voltage in registry");
        assert!(!v.thresholdable);
        assert_eq!(v.threshold_kind, None);
    }

    #[test]
    fn temperature_is_range() {
        let t = lookup("temperature").expect("temperature in registry");
        assert!(t.thresholdable);
        assert_eq!(t.threshold_kind, Some(ThresholdKind::Range));
    }

    #[test]
    fn motion_count_is_not_thresholdable() {
        let m = lookup("motion_count").expect("motion_count in registry");
        assert!(!m.thresholdable);
        assert_eq!(m.threshold_kind, None);
    }
}
