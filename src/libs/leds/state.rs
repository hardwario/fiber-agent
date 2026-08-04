// Shared LED state for communication between monitoring threads and LED controller

use std::sync::{Arc, Mutex, Condvar};
use crate::libs::alarms::color::{BlinkPattern, LedColor, LedState as AlarmLedState};

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

    /// Darken all eight line LEDs.
    ///
    /// Writes `Some(Off)` rather than `None`, and that distinction is the whole
    /// point: `None` means "never written", and
    /// [`crate::libs::leds::monitor`] skips those lines entirely — so clearing to
    /// `None` would leave the LEDs *lit*. `LedColor::Off` is a real value that
    /// travels to the firmware as `LED i O S`.
    pub fn set_all_lines_off(&mut self) {
        let off = AlarmLedState::new(LedColor::Off, BlinkPattern::Steady);
        for line in self.lines.iter_mut() {
            *line = Some(LineLedState::new(off));
        }
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
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Update line LED and notify monitor
    pub fn set_line_led(&self, line_idx: u8, led_state: AlarmLedState) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.set_line_led(line_idx, led_state);
        }
        self.notify.notify_one();
    }

    /// Darken all eight line LEDs and notify the monitor.
    ///
    /// One lock and one notification rather than eight, so the monitor cannot wake
    /// mid-sweep and paint a half-dark set of lines.
    ///
    /// Used on the way into standby: the sensor loop stops running there, and it
    /// is the only writer of line LED state, so without this the LEDs freeze
    /// showing the sensor status of a device that has stopped measuring.
    pub fn set_all_lines_off(&self) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.set_all_lines_off();
        }
        self.notify.notify_one();
    }

    /// Update power LED and notify monitor
    pub fn set_power_leds(&self, color: PowerLedColor, blink: bool) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.set_power_leds(color, blink);
        }
        self.notify.notify_one();
    }

    /// Wait for LED state change notification (with timeout for periodic updates)
    pub fn wait_for_change(&self, timeout: std::time::Duration) {
        let _ = self.notify.wait_timeout(
            self.state.lock().unwrap_or_else(|e| e.into_inner()),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn colours(state: &SharedLedState) -> Vec<Option<(LedColor, BlinkPattern)>> {
        state
            .lines
            .iter()
            .map(|l| l.map(|s| (s.led_state.color, s.led_state.pattern)))
            .collect()
    }

    #[test]
    fn a_fresh_state_has_never_written_lines() {
        // None is "never written", which the LED monitor skips. It is not "off".
        let state = SharedLedState::new();
        assert!(state.lines.iter().all(|l| l.is_none()));
    }

    #[test]
    fn all_lines_off_writes_off_rather_than_clearing_to_none() {
        // THE regression guard. The LED monitor skips `None` lines entirely
        // (`if let Some(..)` in leds/monitor.rs), so clearing to None would leave
        // every LED lit — the exact opposite of the intent. Off must travel as a
        // real value so `LED i O S` reaches the firmware.
        let handle = SharedLedStateWithNotify::new();
        for i in 0..8 {
            handle.set_line_led(i, AlarmLedState::new(LedColor::Green, BlinkPattern::Steady));
        }

        handle.set_all_lines_off();

        let state = handle.read();
        assert_eq!(
            colours(&state),
            vec![Some((LedColor::Off, BlinkPattern::Steady)); 8],
            "every line must carry an explicit Off, not None"
        );
    }

    #[test]
    fn all_lines_off_covers_lines_that_were_never_written() {
        // A line with no sensor is None at boot; standby must still give it an
        // explicit Off so the monitor's cache agrees with the hardware.
        let handle = SharedLedStateWithNotify::new();
        handle.set_all_lines_off();

        assert!(handle.read().lines.iter().all(|l| l.is_some()));
    }

    #[test]
    fn clearing_the_lines_leaves_the_power_led_alone() {
        // Standby keeps the power LED as its own indication (lime, slow blink), so
        // darkening the lines must not touch it.
        let handle = SharedLedStateWithNotify::new();
        handle.set_power_leds(PowerLedColor::Lime, true);

        handle.set_all_lines_off();

        let state = handle.read();
        assert_eq!(state.power.color, PowerLedColor::Lime);
        assert!(state.power.blink);
    }

    #[test]
    fn a_line_index_past_the_end_is_ignored() {
        let handle = SharedLedStateWithNotify::new();
        handle.set_line_led(8, AlarmLedState::new(LedColor::Red, BlinkPattern::Steady));
        assert!(handle.read().lines.iter().all(|l| l.is_none()));
    }

    #[test]
    fn resuming_after_a_clear_is_a_visible_change() {
        // Why the resume needs no cache invalidation: the monitor diffs against
        // what it last *sent*, so a line going Off then back to Green differs both
        // times and is emitted both times.
        let handle = SharedLedStateWithNotify::new();
        let green = AlarmLedState::new(LedColor::Green, BlinkPattern::Steady);

        handle.set_line_led(0, green);
        let before = colours(&handle.read())[0];

        handle.set_all_lines_off();
        let dark = colours(&handle.read())[0];
        assert_ne!(before, dark);

        handle.set_line_led(0, green);
        assert_eq!(colours(&handle.read())[0], before);
        assert_ne!(colours(&handle.read())[0], dark);
    }

    #[test]
    fn power_led_pins_map_as_documented() {
        assert_eq!(PowerLedState::new(PowerLedColor::Green, false).get_pins(), (true, false));
        assert_eq!(PowerLedState::new(PowerLedColor::Yellow, false).get_pins(), (false, true));
        assert_eq!(PowerLedState::new(PowerLedColor::Lime, false).get_pins(), (true, true));
        assert_eq!(PowerLedState::new(PowerLedColor::Off, false).get_pins(), (false, false));
    }
}
