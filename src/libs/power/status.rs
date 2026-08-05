// Battery/Power management module for medical thermometer.
// VBAT: 3100mV = 0%, 3400mV = 100%
// VIN: see DcThresholds — one hysteretic signal, shared by every consumer.

use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// Default VIN at or above which DC power counts as present.
///
/// **Not 12000.** The southbridge reports VIN through two integer truncations
/// (`v_pin_mv = raw * 3300 / 4095`, then `VIN = v_pin_mv * 398 / 68`), so a
/// perfect 12.000 V input reports **11998 mV** and 12000 is not even reachable —
/// pin 2050 gives 11998, pin 2051 gives 12004. Clearing 12000 needs ≈12.008 V of
/// true input, and a nominal 12 V splitter realistically reports anywhere in
/// 11.6–12.4 V once the assumed 3300 mV reference, 1% divider resistors and the
/// 330 kΩ leg's leakage are accounted for.
///
/// A threshold up there therefore strands correctly powered devices. VIN is
/// either the ~12 V rail or near zero on battery, so anything in this range
/// separates the two states; 11000 is the figure `is_on_dc_power()` has used in
/// production.
pub const DEFAULT_DC_CONNECT_MV: u16 = 11000;

/// Default VIN below which DC power counts as gone. Lower than the connect
/// figure so a supply sitting near the boundary cannot chatter.
pub const DEFAULT_DC_DISCONNECT_MV: u16 = 10000;

/// The two VIN figures that define "DC present", as a hysteresis pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DcThresholds {
    /// Rise at or above this.
    pub connect_mv: u16,
    /// Fall below this.
    pub disconnect_mv: u16,
}

impl Default for DcThresholds {
    fn default() -> Self {
        Self {
            connect_mv: DEFAULT_DC_CONNECT_MV,
            disconnect_mv: DEFAULT_DC_DISCONNECT_MV,
        }
    }
}

impl DcThresholds {
    /// Build from configured values, falling back to the defaults if they do not
    /// describe a usable hysteresis band.
    ///
    /// A config that inverts the pair would make the detector either latch on
    /// forever or never rise at all, and this signal gates whether a device in
    /// standby can ever come back. Refusing bad input and saying so is the only
    /// safe reading.
    pub fn new(connect_mv: u16, disconnect_mv: u16) -> Self {
        if disconnect_mv >= connect_mv {
            eprintln!(
                "[power] WARN: dc_disconnect_mv ({disconnect_mv}) must be below \
                 dc_connect_mv ({connect_mv}) — using defaults {DEFAULT_DC_CONNECT_MV}/{DEFAULT_DC_DISCONNECT_MV}"
            );
            return Self::default();
        }
        Self {
            connect_mv,
            disconnect_mv,
        }
    }
}

/// Turns a stream of VIN readings into one debounced "DC is present" answer.
///
/// Stateful on purpose: the whole point is that the answer depends on which side
/// the reading last crossed, not on where a single sample fell. Pure otherwise —
/// no clock, no I/O — so the whole table is unit-testable.
///
/// Every consumer reads this one answer via [`PowerStatus::on_dc_power`]. Before,
/// the field was computed at `> 12000` and `is_on_dc_power()` at `> 11000`, so the
/// LCD and MQTT could say "Bat" while the LED and the alarm logic said DC for any
/// VIN in between.
#[derive(Debug, Clone, Copy)]
pub struct DcDetector {
    thresholds: DcThresholds,
    on_dc: bool,
}

impl DcDetector {
    pub fn new(thresholds: DcThresholds) -> Self {
        Self {
            thresholds,
            on_dc: false,
        }
    }

    /// Seed the state without treating it as a transition — for the first reading
    /// after start-up, where there is no previous side to have crossed from.
    pub fn seeded(thresholds: DcThresholds, vin_mv: u16) -> Self {
        Self {
            thresholds,
            on_dc: vin_mv >= thresholds.connect_mv,
        }
    }

    /// Feed one reading; returns whether DC is present after it.
    pub fn update(&mut self, vin_mv: u16) -> bool {
        if self.on_dc {
            if vin_mv < self.thresholds.disconnect_mv {
                self.on_dc = false;
            }
        } else if vin_mv >= self.thresholds.connect_mv {
            self.on_dc = true;
        }
        self.on_dc
    }

    pub fn is_on_dc(&self) -> bool {
        self.on_dc
    }

    pub fn thresholds(&self) -> DcThresholds {
        self.thresholds
    }
}

/// Power supply information
#[derive(Debug, Clone, Copy)]
pub struct PowerStatus {
    /// Battery voltage in millivolts
    pub vbat_mv: u16,
    /// Calculated battery percentage (0-100)
    pub battery_percent: u8,
    /// Main input voltage (VIN) in millivolts
    pub vin_mv: u16,
    /// Whether DC power is present. The single DC answer in the process — see
    /// [`DcDetector`]. Set from the detector by [`super::controller::PowerController`];
    /// [`PowerStatus::new`] falls back to a stateless comparison for the callers
    /// (tests, defaults, seeds) that have no detector to hand.
    pub on_dc_power: bool,
    /// Timestamp of last DC power loss event
    pub last_dc_loss_time: Option<SystemTime>,
}

impl PowerStatus {
    /// Create power status from VBAT and VIN voltages in millivolts
    /// Maps: 3100mV → 0%, 3400mV → 100% (VBAT)
    ///
    /// DC presence is decided statelessly against [`DEFAULT_DC_CONNECT_MV`]. Use
    /// [`PowerStatus::with_dc`] where a [`DcDetector`] exists, so the answer
    /// carries the hysteresis.
    pub fn new(vbat_mv: u16, vin_mv: u16) -> Self {
        let on_dc_power = vin_mv >= DEFAULT_DC_CONNECT_MV;
        Self::with_dc(vbat_mv, vin_mv, on_dc_power)
    }

    /// Create power status with DC presence supplied by a [`DcDetector`].
    pub fn with_dc(vbat_mv: u16, vin_mv: u16, on_dc_power: bool) -> Self {
        let percent = Self::calculate_battery_percent(vbat_mv);
        Self {
            vbat_mv,
            battery_percent: percent,
            vin_mv,
            on_dc_power,
            last_dc_loss_time: None,
        }
    }

    /// Create power status from VBAT only (VIN assumed 0)
    pub fn from_vbat(vbat_mv: u16) -> Self {
        Self::new(vbat_mv, 0)
    }

    /// Create power status from VIN only (VBAT assumed 0)
    pub fn from_vin(vin_mv: u16) -> Self {
        Self::new(0, vin_mv)
    }

    /// Calculate battery percentage from voltage
    /// Linear mapping: 3100mV = 0%, 3400mV = 100%
    fn calculate_battery_percent(vbat_mv: u16) -> u8 {
        const MIN_VBAT: u16 = 3100; // 0%
        const MAX_VBAT: u16 = 3400; // 100%

        if vbat_mv <= MIN_VBAT {
            0
        } else if vbat_mv >= MAX_VBAT {
            100
        } else {
            let range = (MAX_VBAT - MIN_VBAT) as u16;
            let used = (vbat_mv - MIN_VBAT) as u16;
            ((used * 100) / range) as u8
        }
    }

    /// Check if battery is low (< 20%, matches Config's low_threshold_percent)
    pub fn is_low(&self) -> bool {
        self.battery_percent < 20
    }

    /// Check if battery is critical (< 5%, matches Config's critical_threshold_percent)
    pub fn is_critical(&self) -> bool {
        self.battery_percent < 5
    }

    /// Check if battery is normal (>= 20%)
    pub fn is_normal_battery(&self) -> bool {
        self.battery_percent >= 20
    }

    /// Whether DC power is present.
    ///
    /// A shim over the field rather than its own comparison, which is the point:
    /// this used to test `vin_mv > 11000` while the field was set at `> 12000`, so
    /// the LED and the POWER_DISCONNECT alarm edge could disagree with the LCD,
    /// MQTT and `fiberctl` for any VIN in between.
    pub fn is_on_dc_power(&self) -> bool {
        self.on_dc_power
    }

    /// Whether the device is running from the battery.
    pub fn is_on_battery(&self) -> bool {
        !self.on_dc_power
    }

    /// Get LED control state for power indicator (PWRLEDG / PWRLEDY)
    /// Returns (color, blink) using PowerLedColor enum
    /// - DC Power: GREEN (steady)
    /// - Battery OK (>= 20%): YELLOW (blinking)
    /// - Battery Low (5-19%): YELLOW (steady)
    /// - Battery Critical (< 5%): YELLOW (blinking)
    pub fn get_pwr_led_state(&self) -> (crate::libs::leds::state::PowerLedColor, bool) {
        use crate::libs::leds::state::PowerLedColor;

        if self.is_on_dc_power() {
            // DC Power connected: GREEN (steady)
            (PowerLedColor::Green, false)
        } else if self.is_critical() {
            // Battery critical: YELLOW (blinking)
            (PowerLedColor::Yellow, true)
        } else if self.is_low() {
            // Battery low: YELLOW (steady)
            (PowerLedColor::Yellow, false)
        } else if self.is_normal_battery() {
            // Battery OK on battery power: YELLOW (blinking) - immediate PoE loss indicator
            (PowerLedColor::Yellow, true)
        } else {
            // Fallback
            (PowerLedColor::Off, false)
        }
    }

    /// Record DC power loss event
    pub fn record_dc_loss(&mut self) {
        self.last_dc_loss_time = Some(SystemTime::now());
    }
}

impl Default for PowerStatus {
    fn default() -> Self {
        // Default to full battery (3400mV) and DC power (15000mV)
        Self::new(3400, 15000)
    }
}

pub type SharedPowerStatus = Arc<Mutex<PowerStatus>>;

#[cfg(test)]
mod tests {
    use super::*;

    // --- DcDetector / DcThresholds ----------------------------------------

    #[test]
    fn an_ideal_twelve_volt_input_counts_as_dc_power() {
        // THE regression guard. The southbridge truncates twice, so a perfect
        // 12.000 V input reports 11998 mV. The old thresholds tested `> 12000`,
        // which this fails — and a device that fails it can never resume from
        // standby and re-enters standby on every boot while on mains.
        let mut d = DcDetector::new(DcThresholds::default());
        assert!(
            d.update(11_998),
            "a nominal 12 V supply must read as DC power"
        );

        // The neighbouring reachable values either side, for good measure:
        // 12000 itself is not producible by the firmware's integer maths.
        assert!(DcDetector::new(DcThresholds::default()).update(11_992));
        assert!(DcDetector::new(DcThresholds::default()).update(12_004));
    }

    #[test]
    fn dc_detection_does_not_chatter_anywhere_in_the_band() {
        let t = DcThresholds::default(); // 11000 / 10000
        let mut d = DcDetector::new(t);

        // Rising: nothing below connect_mv may latch on.
        for mv in [9_500u16, 10_000, 10_500, 10_999] {
            assert!(!d.update(mv), "{mv} mV is below the connect threshold");
        }
        assert!(d.update(11_000), "exactly at connect_mv counts as DC");

        // Falling: once on, it stays on all the way down to disconnect_mv.
        for mv in [11_000u16, 10_500, 10_000] {
            assert!(
                d.update(mv),
                "{mv} mV is still above the disconnect threshold"
            );
        }
        assert!(!d.update(9_999), "just below disconnect_mv drops out");
    }

    #[test]
    fn a_supply_hovering_on_the_boundary_holds_its_state() {
        // The reason for hysteresis: ripple around one figure must not produce a
        // stream of connect/disconnect alarm events and LED changes.
        let mut d = DcDetector::new(DcThresholds::default());
        d.update(12_000);
        for mv in [10_400u16, 10_600, 10_400, 10_600] {
            assert!(d.update(mv), "still DC while inside the band");
        }
    }

    #[test]
    fn seeding_adopts_the_state_without_calling_it_a_transition() {
        let t = DcThresholds::default();
        assert!(DcDetector::seeded(t, 12_100).is_on_dc());
        assert!(!DcDetector::seeded(t, 0).is_on_dc());
    }

    #[test]
    fn an_inverted_threshold_pair_falls_back_to_the_defaults() {
        // Taking these at face value would give a detector that never rises, on a
        // signal that decides whether a standby device can come back at all.
        let t = DcThresholds::new(10_000, 11_000);
        assert_eq!(t, DcThresholds::default());

        // Equal values are just as unusable.
        assert_eq!(DcThresholds::new(11_000, 11_000), DcThresholds::default());

        // A sane pair is kept.
        let ok = DcThresholds::new(9_000, 8_000);
        assert_eq!(ok.connect_mv, 9_000);
        assert_eq!(ok.disconnect_mv, 8_000);
    }

    #[test]
    fn the_field_and_the_method_can_no_longer_disagree() {
        // 11500 mV used to be reported as battery by the field (>12000) and as DC
        // by the method (>11000) at the same time.
        let s = PowerStatus::new(3_400, 11_500);
        assert_eq!(s.on_dc_power, s.is_on_dc_power());
        assert_ne!(s.is_on_dc_power(), s.is_on_battery());
        assert!(s.is_on_dc_power(), "11500 mV is a powered device");
    }

    #[test]
    fn with_dc_lets_the_detector_override_the_stateless_reading() {
        // Mid-band on the way down: the detector says still DC, and the status
        // must carry that rather than recomputing it.
        let s = PowerStatus::with_dc(3_400, 10_500, true);
        assert!(s.is_on_dc_power());
        assert!(!PowerStatus::new(3_400, 10_500).is_on_dc_power());
    }

    #[test]
    fn test_battery_calculation_bounds() {
        let min = PowerStatus::from_vbat(3100);
        assert_eq!(min.battery_percent, 0);

        let max = PowerStatus::from_vbat(3400);
        assert_eq!(max.battery_percent, 100);

        let over = PowerStatus::from_vbat(3500);
        assert_eq!(over.battery_percent, 100);

        let under = PowerStatus::from_vbat(3000);
        assert_eq!(under.battery_percent, 0);
    }

    #[test]
    fn test_battery_calculation_midpoint() {
        let mid = PowerStatus::from_vbat(3250);
        assert_eq!(mid.battery_percent, 50);
    }

    #[test]
    fn test_battery_low_critical() {
        let low = PowerStatus::from_vbat(3150); // ~16%
        assert!(low.is_low());

        let critical = PowerStatus::from_vbat(3110); // ~3%
        assert!(critical.is_critical());
    }

    #[test]
    fn test_pwr_led_dc_power() {
        use crate::libs::leds::state::PowerLedColor;
        // DC Power (VIN > 12000mV): GREEN steady
        let dc = PowerStatus::new(3400, 15000);
        let (color, blink) = dc.get_pwr_led_state();
        assert_eq!(color, PowerLedColor::Green, "DC power should be GREEN");
        assert!(!blink, "DC power should not blink");
    }

    #[test]
    fn test_pwr_led_battery_ok() {
        use crate::libs::leds::state::PowerLedColor;
        // Battery mode, not low: YELLOW blinking (PoE loss indicator)
        let battery_ok = PowerStatus::new(3300, 5000);
        let (color, blink) = battery_ok.get_pwr_led_state();
        assert_eq!(color, PowerLedColor::Yellow, "Battery OK should be YELLOW");
        assert!(blink, "Battery OK should blink to indicate PoE loss");
    }

    #[test]
    fn test_pwr_led_battery_low() {
        use crate::libs::leds::state::PowerLedColor;
        // Battery low: YELLOW steady
        let battery_low = PowerStatus::new(3150, 5000);
        let (color, blink) = battery_low.get_pwr_led_state();
        assert_eq!(color, PowerLedColor::Yellow, "Battery low should be YELLOW");
        assert!(!blink, "Battery low should not blink (critical blinks)");
    }

    #[test]
    fn test_pwr_led_battery_critical() {
        use crate::libs::leds::state::PowerLedColor;
        // Battery critical: YELLOW blinking
        let battery_critical = PowerStatus::new(3050, 5000);
        let (color, blink) = battery_critical.get_pwr_led_state();
        assert_eq!(
            color,
            PowerLedColor::Yellow,
            "Battery critical should be YELLOW"
        );
        assert!(blink, "Battery critical should blink");
    }
}
