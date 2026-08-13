use anyhow::Result;
use rppal::gpio::{Gpio, InputPin};

// Pins for buttons
const BTN_UP: u8 = 23;
const BTN_ENTER: u8 = 24;
const BTN_DOWN: u8 = 25;

// Number of consecutive matching 50ms polls (see poll_interval in
// libs/display/buttons.rs) required before a raw GPIO level change is
// accepted as real. Mechanical bounce and electrical noise on these lines
// (weak RPi internal pull-ups, unshielded panel wiring) can otherwise
// register as a spurious edge — and hold-based actions (2s countdowns)
// treat any edge on another button as a deliberate cancel, so a single
// glitch mid-hold silently aborts it. 3 samples = 100-150ms, comfortably
// longer than typical switch bounce (<20ms) without adding perceptible
// input lag.
const DEBOUNCE_SAMPLES: u8 = 3;

#[derive(Debug, Clone, Copy)]
pub enum Button {
    Up,
    Down,
    Enter,
}

#[derive(Debug, Clone, Copy)]
pub enum ButtonEvent {
    Press(Button),
    Release(Button),
}

/// Confirms a raw boolean reading only after it has been stable for
/// `DEBOUNCE_SAMPLES` consecutive `sample()` calls, filtering out
/// switch bounce / electrical noise glitches.
struct Debouncer {
    confirmed: bool,
    candidate: bool,
    candidate_count: u8,
}

impl Debouncer {
    fn new(initial: bool) -> Self {
        Self {
            confirmed: initial,
            candidate: initial,
            candidate_count: DEBOUNCE_SAMPLES,
        }
    }

    /// Feed one raw sample; returns the debounced, confirmed level.
    fn sample(&mut self, raw: bool) -> bool {
        if raw == self.candidate {
            if self.candidate_count < DEBOUNCE_SAMPLES {
                self.candidate_count += 1;
            }
        } else {
            self.candidate = raw;
            self.candidate_count = 1;
        }

        if self.candidate_count >= DEBOUNCE_SAMPLES {
            self.confirmed = self.candidate;
        }

        self.confirmed
    }
}

pub struct Buttons {
    up: InputPin,
    down: InputPin,
    enter: InputPin,
    up_debounce: Debouncer,
    down_debounce: Debouncer,
    enter_debounce: Debouncer,
    last_up: bool,
    last_down: bool,
    last_enter: bool,
}

impl Buttons {
    pub fn new() -> Result<Self> {
        let gpio = Gpio::new()?;

        let up = gpio.get(BTN_UP)?.into_input_pullup();
        let down = gpio.get(BTN_DOWN)?.into_input_pullup();
        let enter = gpio.get(BTN_ENTER)?.into_input_pullup();

        Ok(Self {
            up,
            down,
            enter,
            up_debounce: Debouncer::new(true),
            down_debounce: Debouncer::new(true),
            enter_debounce: Debouncer::new(true),
            last_up: true,
            last_down: true,
            last_enter: true,
        })
    }

    /// Poll buttons and return any edge events since last call.
    pub fn poll(&mut self) -> Vec<ButtonEvent> {
        let mut events = Vec::new();

        let curr_up = self.up_debounce.sample(self.up.is_high());
        let curr_down = self.down_debounce.sample(self.down.is_high());
        let curr_enter = self.enter_debounce.sample(self.enter.is_high());

        // Falling edge = press (active-low)
        if self.last_up && !curr_up {
            events.push(ButtonEvent::Press(Button::Up));
        } else if !self.last_up && curr_up {
            events.push(ButtonEvent::Release(Button::Up));
        }

        if self.last_down && !curr_down {
            events.push(ButtonEvent::Press(Button::Down));
        } else if !self.last_down && curr_down {
            events.push(ButtonEvent::Release(Button::Down));
        }

        if self.last_enter && !curr_enter {
            events.push(ButtonEvent::Press(Button::Enter));
        } else if !self.last_enter && curr_enter {
            events.push(ButtonEvent::Release(Button::Enter));
        }

        self.last_up = curr_up;
        self.last_down = curr_down;
        self.last_enter = curr_enter;

        events
    }
}

#[cfg(test)]
mod debounce_tests {
    use super::Debouncer;

    #[test]
    fn ignores_a_single_glitch_sample() {
        let mut d = Debouncer::new(true);
        // One stray low sample (simulates a noise glitch mid-hold) must not
        // flip the confirmed level.
        assert!(d.sample(false));
        assert!(d.sample(true));
    }

    #[test]
    fn confirms_a_sustained_change_after_debounce_window() {
        let mut d = Debouncer::new(true);
        assert!(d.sample(false)); // 1st low sample: not yet confirmed
        assert!(d.sample(false)); // 2nd
        assert!(!d.sample(false)); // 3rd (== DEBOUNCE_SAMPLES): now confirmed low
        assert!(!d.sample(false)); // stays low
    }

    #[test]
    fn mechanical_bounce_pattern_does_not_produce_a_premature_press() {
        // Typical switch bounce: rapid true/false chatter for a few samples
        // before settling low (button held down, active-low).
        let mut d = Debouncer::new(true);
        let bounce = [false, true, false, true, false, false, false, false];
        let mut confirmed = Vec::new();
        for raw in bounce {
            confirmed.push(d.sample(raw));
        }
        // Only settles to `false` once the raw signal has been low for
        // DEBOUNCE_SAMPLES consecutive samples in a row — never earlier,
        // and never bounces back to `true` after that.
        assert_eq!(
            confirmed,
            vec![true, true, true, true, true, true, false, false]
        );
    }

    #[test]
    fn recovers_after_a_dropped_hold_window_glitch() {
        // A hold in progress (confirmed false/pressed) sees one noise blip
        // back to `true` — must not be reported as a real release.
        let mut d = Debouncer::new(true);
        for raw in [false, false, false] {
            d.sample(raw);
        }
        assert!(!d.sample(false)); // settled pressed
        assert!(!d.sample(true)); // single glitch sample
        assert!(!d.sample(false)); // back to normal — still pressed, no flip
    }
}
