### FIBER MEDICAL PROJECT

Target : Raspberry CM4

Peripherals: /drivers
Libraries: /libs

We'll create an app to medical thermo sensors reading
it has a display, buzzer, buttons, one wire sensors connected
- Display: /drivers/display.rs
- Buzzer: /drivers/buzzer.rs
- STM32: /drivers/stm.rs
- Buttons: /drivers/buttons.rs
- Accelerometer: /drivers/lis2dh12.rs
- W1 sensors: in target (CM4) on /sys/bus/w1/devices/:
  - each w1 line has in stm32 two related LEDs, green and red, so w1_bus_master_1 should be related in stm32.rs with LED1G and LED1R
- Power LEDS: in stm32.rs PWRLEDY and PWRLEDG
- Battery and voltage in: in stm32.rs we have analog VIN and VBAT

The main idea is:
3) Discover sensors connected (we have 8 lines of W1 sensors DS18B20)
2) Thread for reading them (the time to trigger the reading should be defined in a fiber.config.yaml) (in this moment just read, not saving)
1) develop in /libs alarms to:
    - power (VIN > 12000mV -> LEDPWRG ON && LEDPWRY OFF)
        (VIN < 11000mV PWRLEDY ON on PWRLEDG off)
        (VBAT < 3100mV PWRLEDY blinking)
example:
```bash
// src/power.rs
//
// Battery/Power management module for medical thermometer.
// VBAT: 3100mV = 0%, 3400mV = 100%
// VIN: >12000mV = AC Power, <12000mV = Battery mode

use std::sync::{Arc, Mutex};

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

    /// Check if battery is low (< 20%)
    pub fn is_low(&self) -> bool {
        self.battery_percent < 20
    }

    /// Check if battery is critical (< 5%)
    pub fn is_critical(&self) -> bool {
        self.battery_percent < 5
    }

    /// Get LED control state for power indicator (PWRLEDG / PWRLEDY)
    /// Returns (green_on, yellow_on)
    /// - AC Power: GREEN on, YELLOW off
    /// - Battery OK: GREEN off, YELLOW on
    /// - Battery Low: GREEN off, YELLOW blinking (controlled by caller)
    pub fn get_pwr_led_state(&self) -> (bool, bool) {
        if self.on_ac_power {
            // AC Power connected: GREEN on, YELLOW off
            (true, false)
        } else if self.is_low() {
            // Battery low: YELLOW should blink (return as on, caller will blink)
            (false, true)
        } else {
            // Battery mode, not low: GREEN off, YELLOW on
            (false, true)
        }
    }

    /// Check if YELLOW LED should blink (low battery on battery power)
    pub fn should_yellow_blink(&self) -> bool {
        !self.on_ac_power && self.is_low()
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

```

echo ds2482 0x18 | sudo tee /sys/bus/i2c/devices/i2c-10/new_device

 cargo build --release --target aarch64-unknown-linux-gnu --features dev-platform