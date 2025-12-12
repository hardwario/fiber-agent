// Battery/Power management module for medical thermometer.
// VBAT: 3100mV = 0%, 3400mV = 100%
// VIN: >12000mV = AC Power, <12000mV = Battery mode

use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// Power supply information
#[derive(Debug, Clone, Copy)]
pub struct PowerStatus {
    /// Battery voltage in millivolts
    pub vbat_mv: u16,
    /// Calculated battery percentage (0-100)
    pub battery_percent: u8,
    /// Main input voltage (VIN) in millivolts
    pub vin_mv: u16,
    /// Whether on AC power (VIN > 12000mV)
    pub on_ac_power: bool,
    /// Timestamp of last AC power loss event
    pub last_ac_loss_time: Option<SystemTime>,
}

impl PowerStatus {
    /// Create power status from VBAT and VIN voltages in millivolts
    /// Maps: 3100mV → 0%, 3400mV → 100% (VBAT)
    /// VIN > 12000mV = AC power, < 12000mV = Battery mode
    pub fn new(vbat_mv: u16, vin_mv: u16) -> Self {
        let percent = Self::calculate_battery_percent(vbat_mv);
        let on_ac_power = vin_mv > 12000;
        Self {
            vbat_mv,
            battery_percent: percent,
            vin_mv,
            on_ac_power,
            last_ac_loss_time: None,
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

    /// Check if battery is low (3100mV ≤ VBAT ≤ 3200mV)
    pub fn is_low(&self) -> bool {
        self.vbat_mv >= 3100 && self.vbat_mv <= 3200
    }

    /// Check if battery is critical (VBAT < 3100mV)
    pub fn is_critical(&self) -> bool {
        self.vbat_mv < 3100
    }

    /// Check if battery is normal (VBAT > 3200mV)
    pub fn is_normal_battery(&self) -> bool {
        self.vbat_mv > 3200
    }

    /// Check if on AC power (VIN > 11000mV)
    pub fn is_on_ac_power(&self) -> bool {
        self.vin_mv > 11000
    }

    /// Check if on battery power (VIN ≤ 11000mV)
    pub fn is_on_battery(&self) -> bool {
        self.vin_mv <= 11000
    }

    /// Get LED control state for power indicator (PWRLEDG / PWRLEDY)
    /// Returns (color, blink) using PowerLedColor enum
    /// - AC Power: GREEN (steady)
    /// - Battery OK (>3200mV): LIME (steady)
    /// - Battery Low (3100-3200mV): YELLOW (steady)
    /// - Battery Critical (<3100mV): YELLOW (blinking)
    pub fn get_pwr_led_state(&self) -> (crate::libs::leds::state::PowerLedColor, bool) {
        use crate::libs::leds::state::PowerLedColor;

        if self.is_on_ac_power() {
            // AC Power connected: GREEN (steady)
            (PowerLedColor::Green, false)
        } else if self.is_critical() {
            // Battery critical: YELLOW (blinking)
            (PowerLedColor::Yellow, true)
        } else if self.is_low() {
            // Battery low: YELLOW (steady)
            (PowerLedColor::Yellow, false)
        } else if self.is_normal_battery() {
            // Battery OK on battery power: LIME (steady)
            (PowerLedColor::Lime, false)
        } else {
            // Fallback
            (PowerLedColor::Off, false)
        }
    }

    /// Record AC power loss event
    pub fn record_ac_loss(&mut self) {
        self.last_ac_loss_time = Some(SystemTime::now());
    }
}

impl Default for PowerStatus {
    fn default() -> Self {
        // Default to full battery (3400mV) and AC power (15000mV)
        Self::new(3400, 15000)
    }
}

pub type SharedPowerStatus = Arc<Mutex<PowerStatus>>;

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_pwr_led_ac_power() {
        // AC Power (VIN > 12000mV): GREEN on, YELLOW off
        let ac = PowerStatus::new(3400, 15000);
        let (green, yellow) = ac.get_pwr_led_state();
        assert!(green && !yellow, "AC power should have GREEN on, YELLOW off");
        assert!(!ac.should_yellow_blink());
    }

    #[test]
    fn test_pwr_led_battery_ok() {
        // Battery mode, not low: GREEN off, YELLOW on
        let battery_ok = PowerStatus::new(3300, 5000);
        let (green, yellow) = battery_ok.get_pwr_led_state();
        assert!(!green && yellow, "Battery OK should have GREEN off, YELLOW on");
        assert!(!battery_ok.should_yellow_blink());
    }

    #[test]
    fn test_pwr_led_battery_low() {
        // Battery low: YELLOW should blink
        let battery_low = PowerStatus::new(3120, 5000); // ~7%
        let (green, yellow) = battery_low.get_pwr_led_state();
        assert!(!green && yellow, "Battery low should have GREEN off, YELLOW on (blinking)");
        assert!(battery_low.should_yellow_blink());
    }
}
