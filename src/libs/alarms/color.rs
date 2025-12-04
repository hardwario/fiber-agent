//! LED color and blink pattern definitions

use std::fmt;

/// LED color states
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedColor {
    /// Green LED on, red LED off
    Green,
    /// Red LED on, green LED off
    Red,
    /// Both red and green LEDs on (produces yellow)
    Yellow,
    /// Both LEDs off
    Off,
}

impl fmt::Display for LedColor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LedColor::Green => write!(f, "GREEN"),
            LedColor::Red => write!(f, "RED"),
            LedColor::Yellow => write!(f, "YELLOW"),
            LedColor::Off => write!(f, "OFF"),
        }
    }
}

/// Blink pattern for LED animation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlinkPattern {
    /// Steady: always on (or off if color is Off)
    Steady,
    /// Slow blink: 2 cycles on, 2 cycles off (8-cycle period)
    BlinkSlow,
    /// Fast blink: 1 cycle on, 1 cycle off (8-cycle period)
    BlinkFast,
}

impl BlinkPattern {
    /// Determine if LED should be on at this blink cycle (0-7)
    /// Blink cycle wraps at 8
    pub fn is_on(&self, blink_cycle: u8) -> bool {
        let cycle = blink_cycle % 8;
        match self {
            BlinkPattern::Steady => true,
            BlinkPattern::BlinkSlow => cycle < 4,      // 4 on, 4 off
            BlinkPattern::BlinkFast => cycle % 2 == 0, // 1 on, 1 off (alternating)
        }
    }
}

/// Combination of color and blink pattern for LED control
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedState {
    pub color: LedColor,
    pub pattern: BlinkPattern,
}

impl LedState {
    /// Create a new LED state
    pub fn new(color: LedColor, pattern: BlinkPattern) -> Self {
        Self { color, pattern }
    }

    /// Get the actual LED on/off states (green_on, red_on) for a given blink cycle
    pub fn get_led_pins(&self, blink_cycle: u8) -> (bool, bool) {
        let is_on = self.pattern.is_on(blink_cycle);

        match (self.color, is_on) {
            (LedColor::Green, true) => (true, false),
            (LedColor::Red, true) => (false, true),
            (LedColor::Yellow, true) => (true, true),   // Both on for orange
            (LedColor::Off, true) => (false, false),    // Off color stays off even if pattern says on
            (_, false) => (false, false),               // Off regardless of color when pattern says off
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_led_color_display() {
        assert_eq!(LedColor::Green.to_string(), "GREEN");
        assert_eq!(LedColor::Red.to_string(), "RED");
        assert_eq!(LedColor::Yellow.to_string(), "ORANGE");
        assert_eq!(LedColor::Off.to_string(), "OFF");
    }

    #[test]
    fn test_blink_pattern_steady() {
        for cycle in 0..8 {
            assert!(BlinkPattern::Steady.is_on(cycle), "Steady should always be on");
        }
    }

    #[test]
    fn test_blink_pattern_slow() {
        // 4 on, 4 off
        assert!(BlinkPattern::BlinkSlow.is_on(0));
        assert!(BlinkPattern::BlinkSlow.is_on(3));
        assert!(!BlinkPattern::BlinkSlow.is_on(4));
        assert!(!BlinkPattern::BlinkSlow.is_on(7));
        // Wraps at 8
        assert!(BlinkPattern::BlinkSlow.is_on(8));
    }

    #[test]
    fn test_blink_pattern_fast() {
        // 1 on, 1 off (alternating)
        assert!(BlinkPattern::BlinkFast.is_on(0));
        assert!(!BlinkPattern::BlinkFast.is_on(1));
        assert!(BlinkPattern::BlinkFast.is_on(2));
        assert!(!BlinkPattern::BlinkFast.is_on(3));
        assert!(BlinkPattern::BlinkFast.is_on(4));
    }

    #[test]
    fn test_led_state_green_steady() {
        let state = LedState::new(LedColor::Green, BlinkPattern::Steady);
        let (green, red) = state.get_led_pins(0);
        assert!(green && !red);
    }

    #[test]
    fn test_led_state_orange_blink() {
        let state = LedState::new(LedColor::Yellow, BlinkPattern::BlinkSlow);
        let (green1, red1) = state.get_led_pins(0); // On cycle
        let (green2, red2) = state.get_led_pins(4); // Off cycle
        assert!(green1 && red1);                     // Both on
        assert!(!green2 && !red2);                   // Both off
    }

    #[test]
    fn test_led_state_off() {
        let state = LedState::new(LedColor::Off, BlinkPattern::BlinkFast);
        for cycle in 0..8 {
            let (green, red) = state.get_led_pins(cycle);
            assert!(!green && !red, "Off should always be off");
        }
    }
}
