use anyhow::Result;
use rppal::gpio::{Gpio, InputPin};
use std::time::{Duration, Instant};

// Pins for buttons
const BTN_UP: u8 = 23;
const BTN_ENTER: u8 = 24;
const BTN_DOWN: u8 = 25;

/// How long a raw GPIO level must hold steady before it is accepted as real.
///
/// Mechanical bounce and electrical noise on these lines (weak RPi internal
/// pull-ups, unshielded panel wiring) can otherwise register as a spurious edge.
/// 30ms is comfortably longer than typical switch bounce (<20ms) and short enough
/// to keep a brisk tap intact — the double-click window is 400ms.
///
/// Measured in time rather than in consecutive samples on purpose. A sample count
/// silently inherits the poll loop's cadence: three polls delayed by lock
/// contention or a slow LCD flush can span half a second, and the filter would
/// then demand a contact that long before admitting it happened.
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(30);

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

/// Confirms a raw boolean reading only once it has been stable for
/// [`DEBOUNCE_WINDOW`], filtering out switch bounce and electrical glitches.
struct Debouncer {
    confirmed: bool,
    candidate: bool,
    /// When the candidate level was first seen. Every flip restarts it.
    candidate_since: Instant,
}

impl Debouncer {
    fn new(initial: bool, now: Instant) -> Self {
        Self {
            confirmed: initial,
            candidate: initial,
            candidate_since: now,
        }
    }

    /// Feed one raw sample; returns the debounced, confirmed level.
    fn sample(&mut self, raw: bool, now: Instant) -> bool {
        if raw != self.candidate {
            self.candidate = raw;
            self.candidate_since = now;
        }

        if self.candidate != self.confirmed
            && now.saturating_duration_since(self.candidate_since) >= DEBOUNCE_WINDOW
        {
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

        // Seed from what the pins actually read rather than assuming every button
        // is up. A button held (or a line stuck low) as the thread starts would
        // otherwise look like a fresh press moments later, which is enough to open
        // a menu on its own — including after a supervisor restart, when the
        // operator may well have a finger on the panel.
        let now = Instant::now();
        let (up_level, down_level, enter_level) = (up.is_high(), down.is_high(), enter.is_high());

        Ok(Self {
            up,
            down,
            enter,
            up_debounce: Debouncer::new(up_level, now),
            down_debounce: Debouncer::new(down_level, now),
            enter_debounce: Debouncer::new(enter_level, now),
            last_up: up_level,
            last_down: down_level,
            last_enter: enter_level,
        })
    }

    /// Whether a button is currently held down, per the debounced level.
    ///
    /// Reports the same confirmed level [`Self::poll`] derives its edges from, so
    /// a caller that mixes the two cannot see them disagree. Consumers use this
    /// rather than edge bookkeeping to decide whether a hold is still in progress:
    /// an edge can be interfered with, a level cannot.
    pub fn is_pressed(&self, button: Button) -> bool {
        // Active-low: a confirmed high reading means the button is up.
        match button {
            Button::Up => !self.last_up,
            Button::Down => !self.last_down,
            Button::Enter => !self.last_enter,
        }
    }

    /// Poll buttons and return any edge events since last call.
    ///
    /// `now` is passed in so the caller's tick shares one clock reading with the
    /// state machine it feeds.
    pub fn poll(&mut self, now: Instant) -> Vec<ButtonEvent> {
        let mut events = Vec::new();

        let curr_up = self.up_debounce.sample(self.up.is_high(), now);
        let curr_down = self.down_debounce.sample(self.down.is_high(), now);
        let curr_enter = self.enter_debounce.sample(self.enter.is_high(), now);

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
    use super::{Debouncer, DEBOUNCE_WINDOW};
    use std::time::{Duration, Instant};

    /// Poll cadence of the button thread, for tests that care about it.
    const POLL: Duration = Duration::from_millis(50);

    #[test]
    fn a_glitch_shorter_than_the_window_is_rejected() {
        let t0 = Instant::now();
        let mut d = Debouncer::new(true, t0);

        assert!(d.sample(false, t0 + Duration::from_millis(1)));
        assert!(d.sample(true, t0 + Duration::from_millis(10)));
    }

    #[test]
    fn a_level_held_past_the_window_is_confirmed() {
        let t0 = Instant::now();
        let mut d = Debouncer::new(true, t0);

        assert!(d.sample(false, t0), "not yet — the window has not elapsed");
        assert!(!d.sample(false, t0 + DEBOUNCE_WINDOW));
    }

    #[test]
    fn a_tap_spanning_two_polls_registers() {
        // The regression the sample-counting filter introduced: it demanded three
        // consecutive polls (100-150ms), so a brisk tap produced no event at all.
        // Two polls is the floor for any debounce — one to see the change, one to
        // confirm it — and at a 50ms cadence that is comfortably inside a tap.
        let t0 = Instant::now();
        let mut d = Debouncer::new(true, t0);

        assert!(d.sample(false, t0 + POLL));
        assert!(
            !d.sample(false, t0 + POLL * 2),
            "a ~100ms contact must register as a press"
        );
    }

    #[test]
    fn the_window_does_not_stretch_when_the_poll_loop_stalls() {
        // Counting samples tied the guarantee to the loop's cadence: three polls
        // delayed by lock contention or a slow LCD flush could span half a second,
        // and the filter would silently demand a contact that long. Measuring time
        // decouples the two.
        let t0 = Instant::now();
        let mut d = Debouncer::new(true, t0);

        assert!(d.sample(false, t0));
        assert!(
            !d.sample(false, t0 + Duration::from_millis(500)),
            "one stalled poll later, the level has clearly been stable long enough"
        );
    }

    #[test]
    fn mechanical_bounce_does_not_confirm_early() {
        let t0 = Instant::now();
        let mut d = Debouncer::new(true, t0);
        let ms = |n| t0 + Duration::from_millis(n);

        // Contact chatter: every flip restarts the window.
        for (raw, at) in [(false, 0), (true, 5), (false, 10), (true, 15), (false, 20)] {
            assert!(d.sample(raw, ms(at)), "still bouncing at {}ms", at);
        }

        // Settled low for a full window from the last flip.
        assert!(!d.sample(false, ms(20 + DEBOUNCE_WINDOW.as_millis() as u64)));
    }

    #[test]
    fn a_glitch_during_a_hold_is_not_reported_as_a_release() {
        let t0 = Instant::now();
        let mut d = Debouncer::new(true, t0);
        d.sample(false, t0);
        assert!(!d.sample(false, t0 + DEBOUNCE_WINDOW), "settled pressed");

        assert!(
            !d.sample(true, t0 + DEBOUNCE_WINDOW + Duration::from_millis(5)),
            "a single stray high sample is not a release"
        );
        assert!(!d.sample(false, t0 + DEBOUNCE_WINDOW + Duration::from_millis(10)));
    }
}
