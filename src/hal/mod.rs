// src/hal/mod.rs
pub mod real;

/// Abstract interface for a buzzer.
pub trait BuzzerHal {
    fn set_on(&mut self);
    fn set_off(&mut self);
}

/// Logical LED state for **global** alarm indication (PWRLED).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedState {
    Off,
    Green,
    Yellow,
    Red,
}

/// Abstract interface for the global alarm LED (PWRLED).
pub trait LedHal {
    fn set_led_state(&mut self, state: LedState);
}

/// Logical LED state for **per-sensor** LEDs (8 lines).
///
/// Each line has a green and red LED; “Both” = turn both on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensorLedState {
    Off,
    Green,
    Red,
    Both,
}

/// Abstract interface for per-sensor LEDs.
pub trait SensorLedHal {
    /// `index` is the physical LED index: 0..7 for your 8 lines.
    fn set_sensor_led(&mut self, index: u8, state: SensorLedState);
}
