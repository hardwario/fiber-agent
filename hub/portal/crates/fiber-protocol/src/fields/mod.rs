//! Field registries + Node config-key validation (M5-A) — kills v1 quirk
//! #9 (`proximos-v2/plan/v1/viewer/ws.md` §"v1 quirks worth knowing for
//! v2" #9): the LoRaWAN/EYE field registries and the
//! `STICKER_SETTABLE`/`STICKER_READ_ONLY` key tables were each
//! hand-mirrored across Rust/Python/TypeScript, guarded only by
//! convention and one test. This module is the single portal-side source
//! going forward — cross-checked against the device agent's own source
//! (`application/src/libs/lorawan/{registry,sticker_command}.rs`)
//! wherever one exists, since that is the real enforcement point. See
//! each submodule's doc comment for the cross-check findings.
//!
//! - [`lorawan`] — the 25-entry LoRaWAN Node field registry.
//! - [`eye`] — the 4-entry EYE/Beacon field registry + threshold-gating
//!   semantics.
//! - [`sticker_config`] — `STICKER_SETTABLE`/`STICKER_READ_ONLY` key specs
//!   and [`sticker_config::validate_sticker_config`].
//! - [`Quantity`] — what a thresholdable field measures, as far as a default
//!   band is concerned (plan/portal/threshold_defaults.md §1).

pub mod eye;
pub mod lorawan;
pub mod sticker_config;

/// The quantity a thresholdable field measures, for the default bands a new sensor gets
/// (plan/portal/threshold_defaults.md §1, §2).
///
/// Read off the registry's unit rather than the field's name, so a new probe field of a
/// known unit (`w1_3_temperature`, `machine_probe_humidity_2`) needs no new key anywhere:
/// `°C` is a temperature and `%` is a relative humidity in both registries. Every other
/// unit has no default band.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Quantity {
    Temperature,
    Humidity,
    Illuminance,
}

impl Quantity {
    /// The quantity of a field with this unit, if it has a default band at all.
    pub fn of_unit(unit: Option<&str>) -> Option<Self> {
        match unit? {
            "\u{00b0}C" => Some(Self::Temperature),
            "%" => Some(Self::Humidity),
            "lx" => Some(Self::Illuminance),
            _ => None,
        }
    }

    /// The key under `thresholds.defaults` in the rendered config.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Temperature => "temperature",
            Self::Humidity => "humidity",
            Self::Illuminance => "illuminance",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every Node field that gets a default band is a thresholdable continuous field of a
    /// temperature, humidity or (#165) illuminance unit, and every such field gets one: the board, the external
    /// and machine probes, and all four 1-Wire slots (temperature, humidity and, since
    /// v2.0.0, temperature_aux). The barometer's board temperature is the exception.
    #[test]
    fn every_temperature_and_humidity_field_has_a_quantity() {
        let with: Vec<&str> = lorawan::lorawan_registry()
            .iter()
            .filter(|f| f.default_quantity().is_some())
            .map(|f| f.name)
            .collect();
        for name in [
            "temperature",
            "humidity",
            "ext_temperature_1",
            "w1_4_humidity",
            "w1_3_temperature_aux",
            "illuminance",
            "w1_2_illuminance",
        ] {
            assert!(with.contains(&name), "{name}: {with:?}");
        }
        // Board, external, machine probes, the board light sensor, four 1-Wire slots of four.
        assert_eq!(with.len(), 2 + 2 + 4 + 1 + 4 * 4, "{with:?}");
        for name in [
            "voltage",
            "pressure",
            "w1_1_magnetic_field",
            "w1_1_tilt",
            "temperature_baro",
        ] {
            assert!(!with.contains(&name), "{name}");
        }
        assert_eq!(
            lorawan::lorawan_field("w1_2_humidity").and_then(|f| f.default_quantity()),
            Some(Quantity::Humidity)
        );
    }

    /// A Beacon's temperature and humidity, and not its battery or movement counter.
    #[test]
    fn a_beacon_has_a_temperature_and_a_humidity() {
        let with: Vec<(&str, Quantity)> = eye::eye_registry()
            .iter()
            .filter_map(|f| f.default_quantity().map(|q| (f.name, q)))
            .collect();
        assert_eq!(
            with,
            [
                ("temperature", Quantity::Temperature),
                ("humidity", Quantity::Humidity)
            ]
        );
    }
}
