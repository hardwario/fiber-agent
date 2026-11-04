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
    // 1-Wire slots, sticker-firmware v2.0.0 (#430): `w1_{slot}_{channel}`,
    // the Hub field names of node-firmware-v2.0.0.md §1.1 that both the
    // legacy stack (`node_payload`) and the v2 radio publish. Every channel of
    // every 1-Wire type in `control_radio::decode::sensor_types` (dallas,
    // machine-probe) on slots 1..4; the test below keeps the lists in step.
    // The pre-v2.0.0 names above stay for the JS codec (fPort 1) and for
    // display lines and thresholds already stored under them.
    FieldDef {
        name: "w1_1_temperature",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_1_humidity",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_1_temperature_aux",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_1_illuminance",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_1_magnetic_field",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_1_tilt",
        kind: FieldKind::Continuous,
        group: FieldGroup::States,
        thresholdable: false,
        threshold_kind: None,
    },
    FieldDef {
        name: "w1_1_accel_x",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_1_accel_y",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_1_accel_z",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_2_temperature",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_2_humidity",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_2_temperature_aux",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_2_illuminance",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_2_magnetic_field",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_2_tilt",
        kind: FieldKind::Continuous,
        group: FieldGroup::States,
        thresholdable: false,
        threshold_kind: None,
    },
    FieldDef {
        name: "w1_2_accel_x",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_2_accel_y",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_2_accel_z",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_3_temperature",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_3_humidity",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_3_temperature_aux",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_3_illuminance",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_3_magnetic_field",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_3_tilt",
        kind: FieldKind::Continuous,
        group: FieldGroup::States,
        thresholdable: false,
        threshold_kind: None,
    },
    FieldDef {
        name: "w1_3_accel_x",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_3_accel_y",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_3_accel_z",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_4_temperature",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_4_humidity",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_4_temperature_aux",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_4_illuminance",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_4_magnetic_field",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_4_tilt",
        kind: FieldKind::Continuous,
        group: FieldGroup::States,
        thresholdable: false,
        threshold_kind: None,
    },
    FieldDef {
        name: "w1_4_accel_x",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_4_accel_y",
        kind: FieldKind::Continuous,
        group: FieldGroup::Probe,
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    FieldDef {
        name: "w1_4_accel_z",
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

    #[test]
    fn every_w1_channel_of_the_radio_registry_is_here() {
        use control_radio::decode::sensor_types;
        let names = sensor_types::w1_channel_names();
        for slot in 1..=sensor_types::W1_SLOTS {
            for ch in &names {
                let f = format!("w1_{slot}_{ch}");
                assert!(lookup(&f).is_some(), "{f} missing from the registry");
            }
        }
        let ours = REGISTRY
            .iter()
            .filter(|d| d.name.starts_with("w1_"))
            .count();
        assert_eq!(ours, names.len() * usize::from(sensor_types::W1_SLOTS));
        // A state channel is shown, never a threshold.
        assert!(!lookup("w1_3_tilt").unwrap().thresholdable);
        assert!(lookup("w1_2_temperature_aux").unwrap().thresholdable);
    }
}
