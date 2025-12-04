// Shared LED state for communication between monitoring threads and LED controller

use std::sync::{Arc, Mutex, Condvar};
use crate::libs::alarms::color::LedState as AlarmLedState;

/// LED state for a single line (sensor)
#[derive(Debug, Clone, Copy)]
pub struct LineLedState {
    /// LED state from alarm controller
    pub led_state: AlarmLedState,
}

impl LineLedState {
    pub fn new(led_state: AlarmLedState) -> Self {
        Self { led_state }
    }
}

/// Power LED colors
/// For power LEDs (PWRLEDG and PWRLEDY):
/// - Green: PWRLEDG only (AC power)
/// - Yellow: PWRLEDY only (battery OK or low)
/// - Lime: PWRLEDG + PWRLEDY combined (battery OK state on battery power)
/// - Off: Both LEDs off
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerLedColor {
    /// Green LED only (PWRLEDG) - AC power connected
    Green,
    /// Yellow LED only (PWRLEDY) - Battery mode
    Yellow,
    /// Both green and yellow LEDs (PWRLEDG + PWRLEDY) - Battery OK on battery power
    Lime,
    /// Both LEDs off
    Off,
}

/// Shared power LED state
#[derive(Debug, Clone, Copy)]
pub struct PowerLedState {
    pub color: PowerLedColor,
    pub blink: bool,  // Should LED blink?
}

impl PowerLedState {
    pub fn new(color: PowerLedColor, blink: bool) -> Self {
        Self { color, blink }
    }

    /// Get the actual LED pin states (green_on, yellow_on)
    pub fn get_pins(&self) -> (bool, bool) {
        match self.color {
            PowerLedColor::Green => (true, false),
            PowerLedColor::Yellow => (false, true),
            PowerLedColor::Lime => (true, true),
            PowerLedColor::Off => (false, false),
        }
    }
}

/// All LED states shared across threads
#[derive(Debug)]
pub struct SharedLedState {
    /// States for 8 sensor lines
    pub lines: [Option<LineLedState>; 8],
    /// Power LED state
    pub power: PowerLedState,
}

impl Clone for SharedLedState {
    fn clone(&self) -> Self {
        Self {
            lines: self.lines,
            power: self.power,
        }
    }
}

impl SharedLedState {
    pub fn new() -> Self {
        Self {
            lines: [None; 8],
            power: PowerLedState::new(PowerLedColor::Off, false),
        }
    }

    /// Update LED state for a specific line
    pub fn set_line_led(&mut self, line_idx: u8, led_state: AlarmLedState) {
        if (line_idx as usize) < 8 {
            self.lines[line_idx as usize] = Some(LineLedState::new(led_state));
        }
    }

    /// Update power LED state
    pub fn set_power_leds(&mut self, color: PowerLedColor, blink: bool) {
        self.power = PowerLedState::new(color, blink);
    }
}

/// Wrapper for SharedLedState with notification mechanism
pub struct SharedLedStateWithNotify {
    state: Mutex<SharedLedState>,
    notify: Condvar,
}

impl SharedLedStateWithNotify {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(SharedLedState::new()),
            notify: Condvar::new(),
        }
    }

    /// Get current LED state
    pub fn read(&self) -> SharedLedState {
        self.state.lock().unwrap().clone()
    }

    /// Update line LED and notify monitor
    pub fn set_line_led(&self, line_idx: u8, led_state: AlarmLedState) {
        {
            let mut state = self.state.lock().unwrap();
            state.set_line_led(line_idx, led_state);
        }
        self.notify.notify_one();
    }

    /// Update power LED and notify monitor
    pub fn set_power_leds(&self, color: PowerLedColor, blink: bool) {
        {
            let mut state = self.state.lock().unwrap();
            state.set_power_leds(color, blink);
        }
        self.notify.notify_one();
    }

    /// Wait for LED state change notification (with timeout for periodic updates)
    pub fn wait_for_change(&self, timeout: std::time::Duration) {
        let _ = self.notify.wait_timeout(
            self.state.lock().unwrap(),
            timeout
        );
    }
}

impl Default for SharedLedState {
    fn default() -> Self {
        Self::new()
    }
}

/// Arc<SharedLedStateWithNotify> for thread-safe sharing with notification
pub type SharedLedStateHandle = Arc<SharedLedStateWithNotify>;
