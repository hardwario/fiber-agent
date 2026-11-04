//! The one threshold grading rule (plan/control/alarm_hysteresis.md §3, decision #41).
//!
//! The portal grades every source with it and the Hub runs the same function, with the
//! portal's bounds and hysteresis, for its LCD, LEDs and buzzer. There is no second copy.
//!
//! A grade steps up at the bound. It steps down only once the value is `hysteresis` back
//! inside the bound it crossed, so a value hovering at a bound raises one alarm instead of
//! one per wobble. With no previous grade (first value, after a reconnect) the value is
//! graded strictly; a hysteresis of 0 is the strict rule.

use serde::{Deserialize, Serialize};

use crate::fields::Quantity;

/// Default hysteresis of a temperature field, °C.
pub const DEFAULT_HYSTERESIS_TEMPERATURE: f64 = 0.5;
/// Default hysteresis of a relative-humidity field, % RH.
pub const DEFAULT_HYSTERESIS_HUMIDITY: f64 = 3.0;
/// Default hysteresis of an illuminance field, lx.
pub const DEFAULT_HYSTERESIS_ILLUMINANCE: f64 = 10.0;

/// The default hysteresis of a field measuring `quantity`; 0 (strict) for anything else.
pub fn default_hysteresis(quantity: Option<Quantity>) -> f64 {
    match quantity {
        Some(Quantity::Temperature) => DEFAULT_HYSTERESIS_TEMPERATURE,
        Some(Quantity::Humidity) => DEFAULT_HYSTERESIS_HUMIDITY,
        Some(Quantity::Illuminance) => DEFAULT_HYSTERESIS_ILLUMINANCE,
        None => 0.0,
    }
}

/// The default hysteresis of a field by its name, for the surfaces that know only the name
/// (the Hub's Beacon fields, a 1-Wire line's `temperature`). A `*temperature*` field is a
/// temperature, a `*humidity*` field a humidity.
pub fn default_hysteresis_for_field(field: &str) -> f64 {
    if field.contains("humidity") {
        DEFAULT_HYSTERESIS_HUMIDITY
    } else if field.contains("temperature") {
        DEFAULT_HYSTERESIS_TEMPERATURE
    } else {
        0.0
    }
}

/// Severity of a value against its bounds, most severe last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Grade {
    Normal,
    Warning,
    Critical,
}

/// The four bounds of one field. A bound that is not set watches nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Bounds {
    pub critical_low: Option<f64>,
    pub warning_low: Option<f64>,
    pub warning_high: Option<f64>,
    pub critical_high: Option<f64>,
}

impl Bounds {
    /// Grade `value` with every bound moved `inset` towards the normal zone.
    fn grade_inset(&self, value: f64, inset: f64) -> Grade {
        let below = |bound: Option<f64>| bound.is_some_and(|b| value < b + inset);
        let above = |bound: Option<f64>| bound.is_some_and(|b| value > b - inset);
        if below(self.critical_low) || above(self.critical_high) {
            Grade::Critical
        } else if below(self.warning_low) || above(self.warning_high) {
            Grade::Warning
        } else {
            Grade::Normal
        }
    }

    /// The strict grade, no hysteresis: outside a critical bound is `Critical`, else outside
    /// a warning bound `Warning`, else `Normal`.
    pub fn classify(&self, value: f64) -> Grade {
        self.grade_inset(value, 0.0)
    }

    /// Grade `value` given the grade it held so far. See the module doc.
    pub fn grade(&self, value: f64, hysteresis: f64, previous: Option<Grade>) -> Grade {
        let strict = self.classify(value);
        let h = if hysteresis.is_finite() {
            hysteresis.max(0.0)
        } else {
            0.0
        };
        match previous {
            Some(prev) if prev > strict && h > 0.0 => {
                strict.max(prev.min(self.grade_inset(value, h)))
            }
            _ => strict,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn band() -> Bounds {
        Bounds {
            critical_low: Some(20.0),
            warning_low: Some(27.0),
            warning_high: Some(38.0),
            critical_high: Some(41.0),
        }
    }

    fn run(values: &[f64], h: f64) -> Vec<Grade> {
        let mut prev = None;
        values
            .iter()
            .map(|&v| {
                let g = band().grade(v, h, prev);
                prev = Some(g);
                g
            })
            .collect()
    }

    #[test]
    fn a_value_hovering_at_a_bound_holds_one_grade() {
        // #110: Sensor 8 at warning_low 27.0 °C opened 593 one-second alarms.
        let grades = run(&[27.3, 26.9, 27.1, 26.9, 27.4, 27.1, 27.5, 27.4], 0.5);
        use Grade::*;
        assert_eq!(
            grades,
            [Normal, Warning, Warning, Warning, Warning, Warning, Normal, Normal]
        );
    }

    #[test]
    fn each_step_down_is_held_and_no_step_up_is() {
        use Grade::*;
        // Up at once: 41.1 is Critical straight from Normal.
        // Down: Critical holds until 40.5, Warning until 37.5.
        let grades = run(&[30.0, 41.1, 40.8, 40.5, 38.2, 37.6, 37.5], 0.5);
        assert_eq!(
            grades,
            [Normal, Critical, Critical, Warning, Warning, Warning, Normal]
        );
        // A jump into the middle of the band goes straight to Normal.
        assert_eq!(run(&[45.0, 30.0], 0.5), [Critical, Normal]);
        // Low side, critical: holds until 20.5.
        assert_eq!(run(&[19.9, 20.3, 20.5], 0.5), [Critical, Critical, Warning]);
    }

    #[test]
    fn zero_or_bad_hysteresis_is_the_strict_rule() {
        use Grade::*;
        for h in [0.0, -1.0, f64::NAN] {
            assert_eq!(
                run(&[26.9, 27.1, 26.9], h),
                [Warning, Normal, Warning],
                "{h}"
            );
        }
    }

    #[test]
    fn no_previous_grade_is_graded_strictly() {
        assert_eq!(band().grade(27.1, 0.5, None), Grade::Normal);
    }

    #[test]
    fn an_unset_bound_watches_nothing() {
        let only_high = Bounds {
            warning_high: Some(30.0),
            ..Bounds::default()
        };
        assert_eq!(
            only_high.grade(-100.0, 0.5, Some(Grade::Warning)),
            Grade::Normal
        );
        assert_eq!(
            only_high.grade(29.8, 0.5, Some(Grade::Warning)),
            Grade::Warning
        );
    }

    #[test]
    fn defaults_follow_the_quantity() {
        assert_eq!(default_hysteresis(Some(Quantity::Temperature)), 0.5);
        assert_eq!(default_hysteresis(Some(Quantity::Humidity)), 3.0);
        assert_eq!(default_hysteresis(Some(Quantity::Illuminance)), 10.0);
        assert_eq!(default_hysteresis(None), 0.0);
        assert_eq!(default_hysteresis_for_field("w1_2_humidity"), 3.0);
        assert_eq!(default_hysteresis_for_field("temperature"), 0.5);
        assert_eq!(default_hysteresis_for_field("battery"), 0.0);
    }
}
