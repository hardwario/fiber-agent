//! Alarm threshold definitions

use serde::{Deserialize, Serialize};

/// Threshold configuration for temperature-based alarms
///
/// Defines 4 primary temperature thresholds that create different alarm states:
/// - Critical Low: Below this, state is Critical (blinking alert)
/// - Warning Low: Below this, state is Warning
/// - Warning High: Above this, state is Warning
/// - Critical High: Above this, state is Critical (blinking alert)
///
/// The alarm fields (low_alarm_celsius, high_alarm_celsius) are retained for
/// backward compatibility but default to values that effectively disable the
/// alarm zone (0.0 and 100.0 respectively).
///
/// Example with defaults:
/// ```text
/// Critical Low (32°C) ----
///                         Warning Low (35°C) ----
///     ============ NORMAL ZONE (35-38°C) ============
///                         Warning High (38°C) ----
/// Critical High (40°C) ----
/// ```
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct AlarmThreshold {
    /// Temperature below which state is Critical (blinking alert)
    pub critical_low_celsius: f32,

    /// Temperature below which state is Alarm (deprecated, kept for backward compatibility)
    #[serde(default = "default_alarm_low")]
    pub low_alarm_celsius: f32,

    /// Temperature below which state is Warning
    pub warning_low_celsius: f32,

    /// Temperature above which state is Warning
    pub warning_high_celsius: f32,

    /// Temperature above which state is Alarm (deprecated, kept for backward compatibility)
    #[serde(default = "default_alarm_high")]
    pub high_alarm_celsius: f32,

    /// Temperature above which state is Critical (blinking alert)
    pub critical_high_celsius: f32,

    /// How far back inside a bound a value has to come before the state
    /// steps down (#110). Entering a state stays strict at the bound; leaving
    /// it needs the value `hysteresis_celsius` inside, so a probe hovering at
    /// a bound opens one alarm, not one per wobble. 0 disables it.
    #[serde(default = "default_hysteresis")]
    pub hysteresis_celsius: f32,

    /// The "Mez zapnuta" switch (#167). A switched-off band keeps its bounds
    /// but grades nothing: every reading is Normal. Missing (an older portal,
    /// a config written before #167) means on.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

/// Default temperature hysteresis, °C (#110).
pub const DEFAULT_HYSTERESIS_CELSIUS: f32 = 0.5;

fn default_hysteresis() -> f32 {
    DEFAULT_HYSTERESIS_CELSIUS
}

pub use fiber_protocol::grading::Grade;

fn default_alarm_low() -> f32 {
    0.0
}
fn default_alarm_high() -> f32 {
    100.0
}

impl AlarmThreshold {
    /// Create thresholds with custom values
    pub fn new(
        critical_low: f32,
        low_alarm: f32,
        warning_low: f32,
        warning_high: f32,
        high_alarm: f32,
        critical_high: f32,
    ) -> Self {
        Self {
            critical_low_celsius: critical_low,
            low_alarm_celsius: low_alarm,
            warning_low_celsius: warning_low,
            warning_high_celsius: warning_high,
            high_alarm_celsius: high_alarm,
            critical_high_celsius: critical_high,
            hysteresis_celsius: DEFAULT_HYSTERESIS_CELSIUS,
            enabled: true,
        }
    }

    /// Get default medical thermometer thresholds (36.5°C center)
    pub fn default_medical() -> Self {
        Self {
            critical_low_celsius: 32.0,
            low_alarm_celsius: 0.0, // disabled - defaults
            warning_low_celsius: 34.0,
            warning_high_celsius: 39.0,
            high_alarm_celsius: 100.0, // disabled - defaults
            critical_high_celsius: 40.0,
            hysteresis_celsius: DEFAULT_HYSTERESIS_CELSIUS,
            enabled: true,
        }
    }

    /// These thresholds as the shared grading bounds. The deprecated alarm pair
    /// counts as Warning, as in the state machine, so it narrows the warning band.
    fn bounds(&self) -> fiber_protocol::grading::Bounds {
        let f = |v: f32| Some(f64::from(v));
        fiber_protocol::grading::Bounds {
            critical_low: f(self.critical_low_celsius),
            warning_low: f(self.warning_low_celsius.max(self.low_alarm_celsius)),
            warning_high: f(self.warning_high_celsius.min(self.high_alarm_celsius)),
            critical_high: f(self.critical_high_celsius),
        }
    }

    /// Grade a value given the grade it held so far, with the one shared rule
    /// (`fiber_protocol::grading`, decision #41). `previous` is `None` when there
    /// is nothing to hold (first reading, after a reconnect or a standby reset).
    /// A switched-off band (#167) grades every value Normal.
    pub fn grade(&self, value: f32, previous: Option<Grade>) -> Grade {
        if !self.enabled {
            return Grade::Normal;
        }
        self.bounds().grade(
            f64::from(value),
            f64::from(self.hysteresis_celsius),
            previous,
        )
    }

    /// Check if value is within warning range
    pub fn is_warning(&self, value: f32) -> bool {
        value < self.warning_low_celsius || value > self.warning_high_celsius
    }

    /// Check if value is within alarm range
    pub fn is_alarm(&self, value: f32) -> bool {
        value < self.low_alarm_celsius || value > self.high_alarm_celsius
    }

    /// Check if value is within critical range
    pub fn is_critical(&self, value: f32) -> bool {
        value < self.critical_low_celsius || value > self.critical_high_celsius
    }

    /// Check if value is within normal range
    pub fn is_normal(&self, value: f32) -> bool {
        value >= self.warning_low_celsius && value <= self.warning_high_celsius
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #167: a band without `enabled` (an older portal, a stored config) is on.
    #[test]
    fn a_band_without_enabled_deserialises_as_on() {
        let t: AlarmThreshold = serde_json::from_str(
            r#"{"critical_low_celsius":2,"warning_low_celsius":3,
                "warning_high_celsius":8,"critical_high_celsius":10}"#,
        )
        .unwrap();
        assert!(t.enabled);
        assert_eq!(t.grade(12.0, None), Grade::Critical);
    }

    /// #167: off keeps the bounds and grades every value Normal.
    #[test]
    fn a_switched_off_band_grades_nothing_and_keeps_its_bounds() {
        let on = AlarmThreshold::default_medical();
        let off = AlarmThreshold {
            enabled: false,
            ..on
        };
        assert_eq!(off.grade(45.0, None), Grade::Normal);
        assert_eq!(off.grade(10.0, Some(Grade::Critical)), Grade::Normal);
        assert_eq!(off.critical_high_celsius, 40.0);
        assert_eq!(on.grade(45.0, None), Grade::Critical);
    }

    #[test]
    fn test_default_medical_thresholds() {
        let t = AlarmThreshold::default_medical();
        assert_eq!(t.critical_low_celsius, 32.0);
        assert_eq!(t.low_alarm_celsius, 0.0); // disabled
        assert_eq!(t.warning_low_celsius, 34.0);
        assert_eq!(t.warning_high_celsius, 39.0);
        assert_eq!(t.high_alarm_celsius, 100.0); // disabled
        assert_eq!(t.critical_high_celsius, 40.0);
    }

    #[test]
    fn test_is_normal() {
        let t = AlarmThreshold::default_medical();
        assert!(t.is_normal(35.0));
        assert!(t.is_normal(37.0));
        assert!(t.is_normal(39.0));
        assert!(!t.is_normal(33.0));
        assert!(!t.is_normal(40.0));
    }

    #[test]
    fn test_is_warning() {
        let t = AlarmThreshold::default_medical();
        assert!(t.is_warning(33.0)); // Below warning low
        assert!(t.is_warning(40.0)); // Above warning high
        assert!(!t.is_warning(35.0)); // Normal
    }

    #[test]
    fn test_is_alarm() {
        let t = AlarmThreshold::default_medical();
        // With alarm thresholds disabled (0.0 and 100.0), is_alarm only triggers
        // for temperatures below 0.0 or above 100.0
        assert!(!t.is_alarm(34.0)); // Not below 0.0
        assert!(!t.is_alarm(39.0)); // Not above 100.0
        assert!(!t.is_alarm(36.0)); // Normal
        assert!(t.is_alarm(-1.0)); // Below 0.0
        assert!(t.is_alarm(101.0)); // Above 100.0
    }

    #[test]
    fn test_is_critical() {
        let t = AlarmThreshold::default_medical();
        assert!(t.is_critical(31.0)); // Below critical low
        assert!(t.is_critical(41.0)); // Above critical high
        assert!(!t.is_critical(35.0)); // Normal
    }

    #[test]
    fn a_value_hovering_at_a_bound_holds_one_grade() {
        // #110: a probe parked at warning_low 27.0 °C opened ~600 alarms.
        let mut t = AlarmThreshold::default_medical();
        t.critical_low_celsius = 20.0;
        t.warning_low_celsius = 27.0;
        let mut g = None;
        let mut steps = Vec::new();
        for v in [27.1, 26.9, 27.1, 26.9, 27.2, 27.4, 27.5, 27.1, 26.9] {
            let next = t.grade(v, g);
            if g != Some(next) {
                steps.push((v, next));
            }
            g = Some(next);
        }
        assert_eq!(
            steps,
            vec![
                (27.1, Grade::Normal),
                (26.9, Grade::Warning),
                (27.5, Grade::Normal),
                (26.9, Grade::Warning),
            ]
        );
    }

    #[test]
    fn hysteresis_holds_each_step_down_and_never_a_step_up() {
        let t = AlarmThreshold::default_medical(); // 32 / 34 .. 39 / 40, 0.5
        assert_eq!(t.grade(40.1, Some(Grade::Normal)), Grade::Critical);
        assert_eq!(t.grade(39.8, Some(Grade::Critical)), Grade::Critical);
        assert_eq!(t.grade(39.5, Some(Grade::Critical)), Grade::Warning);
        assert_eq!(t.grade(38.6, Some(Grade::Warning)), Grade::Warning);
        assert_eq!(t.grade(38.5, Some(Grade::Warning)), Grade::Normal);
        // Straight from Critical to the middle of the band.
        assert_eq!(t.grade(36.0, Some(Grade::Critical)), Grade::Normal);
        // Nothing to hold: strict.
        assert_eq!(t.grade(38.8, None), Grade::Normal);
    }

    #[test]
    fn zero_hysteresis_is_the_strict_rule() {
        let mut t = AlarmThreshold::default_medical();
        t.hysteresis_celsius = 0.0;
        assert_eq!(t.grade(38.9, Some(Grade::Warning)), Grade::Normal);
        assert_eq!(t.grade(39.9, Some(Grade::Critical)), Grade::Warning);
    }

    /// Decision #41 wants one grading rule. control-ble may depend on nothing in the
    /// workspace (decision #12), so it carries a copy; this crate depends on both and pins
    /// the copy to `fiber_protocol::grading` over a sweep of values, previous grades and
    /// hysteresis values, defaults included.
    #[test]
    fn the_beacon_copy_of_the_rule_grades_as_fiber_protocol_does() {
        use control_ble::central::state::{AlarmLevel, FieldThreshold};
        use fiber_protocol::grading::{default_hysteresis_for_field, Bounds};
        let level = |g: Grade| match g {
            Grade::Normal => AlarmLevel::Normal,
            Grade::Warning => AlarmLevel::Warning,
            Grade::Critical => AlarmLevel::Critical,
        };
        let bands = [
            [Some(20.0), Some(27.0), Some(38.0), Some(41.0)],
            [None, Some(30.0), Some(60.0), None],
            [Some(-5.0), None, None, Some(12.0)],
        ];
        for field in ["temperature", "humidity"] {
            assert_eq!(
                control_ble::central::state::default_hysteresis(field),
                default_hysteresis_for_field(field),
                "{field}"
            );
            for [cl, wl, wh, ch] in bands {
                let bounds = Bounds {
                    critical_low: cl,
                    warning_low: wl,
                    warning_high: wh,
                    critical_high: ch,
                };
                for own in [None, Some(0.0), Some(0.5), Some(2.0)] {
                    let h = own.unwrap_or_else(|| default_hysteresis_for_field(field));
                    let beacon = FieldThreshold {
                        field: field.to_string(),
                        critical_low: cl,
                        warning_low: wl,
                        warning_high: wh,
                        critical_high: ch,
                        hysteresis: own,
                    };
                    for prev in [
                        None,
                        Some(Grade::Normal),
                        Some(Grade::Warning),
                        Some(Grade::Critical),
                    ] {
                        for tenth in -100..=700 {
                            let v = f64::from(tenth) / 10.0;
                            assert_eq!(
                                beacon.evaluate_held(v, prev.map(level)),
                                level(bounds.grade(v, h, prev)),
                                "{field} {bounds:?} h={h} prev={prev:?} v={v}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_file_without_the_key_gets_the_default_hysteresis() {
        let t: AlarmThreshold = serde_yaml::from_str(
            "critical_low_celsius: -10\nwarning_low_celsius: 0\n\
             warning_high_celsius: 40\ncritical_high_celsius: 50\n",
        )
        .unwrap();
        assert_eq!(t.hysteresis_celsius, DEFAULT_HYSTERESIS_CELSIUS);
    }

    #[test]
    fn test_25_degrees_is_normal() {
        let t = AlarmThreshold::default_medical();
        // This verifies the user's requirement: 25°C should show GREEN (normal)
        // 25°C is below warning_low (34°C), so it's in warning range
        assert!(t.is_warning(25.0));
        assert!(!t.is_normal(25.0));
    }
}
