//! Hardcoded field registry for STICKER LoRaWAN codec.
//! MUST mirror the Python and TypeScript registries.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind { Continuous, Counter, Event }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldGroup { Main, ExternalTemp, Probe, Environmental, Battery, Counters, States }

#[derive(Debug, Clone, Copy)]
pub struct FieldDef {
    pub name: &'static str,
    pub kind: FieldKind,
    pub group: FieldGroup,
    pub thresholdable: bool,
}

pub const REGISTRY: &[FieldDef] = &[
    // Continuous, thresholdable
    FieldDef { name: "temperature",                  kind: FieldKind::Continuous, group: FieldGroup::Main,          thresholdable: true  },
    FieldDef { name: "humidity",                     kind: FieldKind::Continuous, group: FieldGroup::Main,          thresholdable: true  },
    FieldDef { name: "ext_temperature_1",            kind: FieldKind::Continuous, group: FieldGroup::ExternalTemp,  thresholdable: true  },
    FieldDef { name: "ext_temperature_2",            kind: FieldKind::Continuous, group: FieldGroup::ExternalTemp,  thresholdable: true  },
    FieldDef { name: "machine_probe_temperature_1",  kind: FieldKind::Continuous, group: FieldGroup::Probe,         thresholdable: true  },
    FieldDef { name: "machine_probe_temperature_2",  kind: FieldKind::Continuous, group: FieldGroup::Probe,         thresholdable: true  },
    FieldDef { name: "machine_probe_humidity_1",     kind: FieldKind::Continuous, group: FieldGroup::Probe,         thresholdable: true  },
    FieldDef { name: "machine_probe_humidity_2",     kind: FieldKind::Continuous, group: FieldGroup::Probe,         thresholdable: true  },
    FieldDef { name: "illuminance",                  kind: FieldKind::Continuous, group: FieldGroup::Environmental, thresholdable: true  },
    FieldDef { name: "pressure",                     kind: FieldKind::Continuous, group: FieldGroup::Environmental, thresholdable: true  },
    FieldDef { name: "altitude",                     kind: FieldKind::Continuous, group: FieldGroup::Environmental, thresholdable: true  },
    // Continuous, non-thresholdable
    FieldDef { name: "voltage",                      kind: FieldKind::Continuous, group: FieldGroup::Battery,       thresholdable: false },
    // Counters
    FieldDef { name: "motion_count",                 kind: FieldKind::Counter,    group: FieldGroup::Counters,      thresholdable: false },
    FieldDef { name: "hall_left_count",              kind: FieldKind::Counter,    group: FieldGroup::Counters,      thresholdable: false },
    FieldDef { name: "hall_right_count",             kind: FieldKind::Counter,    group: FieldGroup::Counters,      thresholdable: false },
    FieldDef { name: "input_a_count",                kind: FieldKind::Counter,    group: FieldGroup::Counters,      thresholdable: false },
    FieldDef { name: "input_b_count",                kind: FieldKind::Counter,    group: FieldGroup::Counters,      thresholdable: false },
];

pub fn lookup(name: &str) -> Option<&'static FieldDef> {
    REGISTRY.iter().find(|f| f.name == name)
}
