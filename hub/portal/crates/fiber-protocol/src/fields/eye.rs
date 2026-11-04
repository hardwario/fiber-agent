//! EYE/Beacon field registry — `proximos-v2/plan/v1/viewer/ws.md`
//! §"Field registries" / §"EYE/Beacon registry" (`:213-224`), which
//! documents the v1 Python `app/eye/field_registry.py` (a static table of
//! 4 fields, not served over HTTP). EYE tags are BLE beacons, not LoRaWAN
//! Nodes — there is no equivalent registry in `application/` to
//! cross-check against, unlike [`crate::fields::lorawan`].
//!
//! Semantics (ws.md): [`is_thresholdable`] gates **setting** a threshold
//! (400 otherwise); [`is_known`] (mere existence in the registry) gates
//! **deleting** — deliberately looser, so a stored threshold on a field
//! that stopped being thresholdable stays removable. `thresholdable:
//! false` does not mean hidden — all four fields are always displayed.

use crate::fields::lorawan::ThresholdKind;

#[derive(Debug, Clone, Copy)]
pub struct EyeFieldDef {
    pub name: &'static str,
    pub label: &'static str,
    pub unit: Option<&'static str>,
    pub thresholdable: bool,
    pub threshold_kind: Option<ThresholdKind>,
}

impl EyeFieldDef {
    /// The quantity whose default band this field gets when its Beacon is registered
    /// (plan/portal/threshold_defaults.md §3).
    pub fn default_quantity(&self) -> Option<crate::fields::Quantity> {
        if !self.thresholdable {
            return None;
        }
        crate::fields::Quantity::of_unit(self.unit)
    }
}

pub const REGISTRY: &[EyeFieldDef] = &[
    EyeFieldDef {
        name: "temperature",
        label: "Temperature",
        unit: Some("\u{00b0}C"),
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    EyeFieldDef {
        name: "humidity",
        label: "Humidity",
        unit: Some("%"),
        thresholdable: true,
        threshold_kind: Some(ThresholdKind::Range),
    },
    // Not thresholdable: the tag asserts its own low-battery flag — a
    // portal-side threshold on raw mV would duplicate device logic
    // (ws.md).
    EyeFieldDef {
        name: "battery",
        label: "Battery",
        unit: Some("mV"),
        thresholdable: false,
        threshold_kind: None,
    },
    // Not thresholdable: a monotonic counter — a band would fire once and
    // then stay fired forever (ws.md).
    EyeFieldDef {
        name: "movement",
        label: "Movement",
        unit: None,
        thresholdable: false,
        threshold_kind: None,
    },
];

/// Look up one field by name.
pub fn eye_field(name: &str) -> Option<&'static EyeFieldDef> {
    REGISTRY.iter().find(|f| f.name == name)
}

/// The full 4-entry registry.
pub fn eye_registry() -> &'static [EyeFieldDef] {
    REGISTRY
}

/// Gates **setting** a threshold — the caller should reject with 400 when
/// this is `false` (ws.md).
pub fn is_thresholdable(name: &str) -> bool {
    eye_field(name).is_some_and(|f| f.thresholdable)
}

/// Gates **deleting** a threshold. Deliberately looser than
/// [`is_thresholdable`]: mere existence in the registry is enough, so a
/// stored threshold on a field that stopped being thresholdable stays
/// removable (ws.md).
pub fn is_known(name: &str) -> bool {
    eye_field(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_4_entries() {
        assert_eq!(REGISTRY.len(), 4, "ws.md: EYE registry — 4 fields");
    }

    #[test]
    fn temperature_and_humidity_are_thresholdable_range() {
        for name in ["temperature", "humidity"] {
            assert!(is_thresholdable(name), "{name}");
            assert_eq!(
                eye_field(name).unwrap().threshold_kind,
                Some(ThresholdKind::Range)
            );
        }
    }

    #[test]
    fn battery_and_movement_are_not_thresholdable() {
        for name in ["battery", "movement"] {
            assert!(!is_thresholdable(name), "{name}");
            assert_eq!(eye_field(name).unwrap().threshold_kind, None);
        }
    }

    #[test]
    fn is_known_is_looser_than_is_thresholdable() {
        // battery is known (deletable) but not thresholdable (not settable).
        assert!(is_known("battery"));
        assert!(!is_thresholdable("battery"));
        assert!(!is_known("not_a_field"));
        assert!(!is_thresholdable("not_a_field"));
    }
}
