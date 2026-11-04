//! The factory test as a pure state machine: front-panel presses and operator
//! verdicts in, [`Action`]s for the hardware out. No I/O here, so every step
//! transition is unit-tested; `runtime` executes the actions.

use serde::Serialize;

/// A front-panel button as the operator names it (ENTER is the middle one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Btn {
    Up,
    Middle,
    Down,
}

/// The interactive steps in order; the radio scans run beside them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Buttons,
    Display,
    Buzzer,
    Leds,
    Report,
    Done,
}

impl Step {
    pub const INTERACTIVE: [Step; 4] = [Step::Buttons, Step::Display, Step::Buzzer, Step::Leds];

    fn next(self) -> Step {
        match self {
            Step::Buttons => Step::Display,
            Step::Display => Step::Buzzer,
            Step::Buzzer => Step::Leds,
            Step::Leds => Step::Report,
            Step::Report | Step::Done => Step::Done,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Pass,
    Fail,
}

/// What the display test shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Pattern {
    /// All pixels on and off in turn (the step's opening).
    Blink,
    /// Every pixel on ("black").
    AllOn,
    /// Checkerboard ("grey"); also shows shorts between neighbouring pixels.
    Checker,
}

/// Side effects the runtime carries out, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    BuzzerOn {
        volume: u8,
    },
    BuzzerVolume(u8),
    BuzzerOff,
    PlayLeds,
    StopLeds,
    /// The interactive part is over: compute the report.
    Report,
    /// Leave factory test mode; the normal UI takes over again.
    Exit,
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub seq: u64,
    pub step: Step,
    /// `press`, `pass`, `fail`, `volume`, `pattern`, `leds`, `info`.
    pub kind: &'static str,
    pub detail: String,
}

/// Buttons in the order the operator presses them.
pub const BUTTON_ORDER: [Btn; 3] = [Btn::Up, Btn::Middle, Btn::Down];

pub const VOLUME_START: u8 = 50;
pub const VOLUME_STEP: u8 = 10;
pub const VOLUME_MIN: u8 = 10;
pub const VOLUME_MAX: u8 = 100;

#[derive(Debug, Clone)]
pub struct Machine {
    pub step: Step,
    /// Buttons of [`BUTTON_ORDER`] pressed so far.
    pub buttons_done: usize,
    pub pattern: Pattern,
    pub volume: u8,
    pub results: Vec<(Step, Outcome)>,
    pub events: Vec<Event>,
    seq: u64,
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

impl Machine {
    pub fn new() -> Self {
        let mut m = Machine {
            step: Step::Buttons,
            buttons_done: 0,
            pattern: Pattern::Blink,
            volume: VOLUME_START,
            results: Vec::new(),
            events: Vec::new(),
            seq: 0,
        };
        m.event("info", "factory test started".into());
        m
    }

    fn event(&mut self, kind: &'static str, detail: String) {
        self.seq += 1;
        self.events.push(Event {
            seq: self.seq,
            step: self.step,
            kind,
            detail,
        });
    }

    pub fn outcome(&self, step: Step) -> Option<Outcome> {
        self.results
            .iter()
            .find(|(s, _)| *s == step)
            .map(|(_, o)| *o)
    }

    /// True once every interactive step passed.
    pub fn interactive_passed(&self) -> bool {
        Step::INTERACTIVE
            .iter()
            .all(|s| self.outcome(*s) == Some(Outcome::Pass))
    }

    /// The operator's instruction for the current step, as shown in the app.
    pub fn prompt(&self) -> String {
        match self.step {
            Step::Buttons => match BUTTON_ORDER.get(self.buttons_done) {
                Some(Btn::Up) => "Press the UP button".into(),
                Some(Btn::Middle) => "Press the MIDDLE button".into(),
                Some(Btn::Down) => "Press the DOWN button".into(),
                None => String::new(),
            },
            Step::Display => {
                "Check every pixel. UP: all black, DOWN: grey (checkerboard), MIDDLE: confirm"
                    .into()
            }
            Step::Buzzer => format!(
                "The buzzer beeps at {}%. UP: louder, DOWN: quieter, MIDDLE: confirm",
                self.volume
            ),
            Step::Leds => "Watch the LEDs: each in green, red, yellow, then the power LED. \
                 UP or DOWN: play again, MIDDLE: confirm"
                .into(),
            Step::Report => "Results are on the display. MIDDLE: finish".into(),
            Step::Done => "Finished".into(),
        }
    }

    /// Ends the current step with `outcome` and enters the next one.
    fn finish(&mut self, outcome: Outcome, why: String) -> Vec<Action> {
        let mut actions = Vec::new();
        match self.step {
            Step::Buzzer => actions.push(Action::BuzzerOff),
            Step::Leds => actions.push(Action::StopLeds),
            _ => {}
        }
        if Step::INTERACTIVE.contains(&self.step) {
            self.results.push((self.step, outcome));
            let kind = if outcome == Outcome::Pass {
                "pass"
            } else {
                "fail"
            };
            self.event(kind, why);
        }
        self.step = self.step.next();
        actions.extend(self.enter());
        actions
    }

    fn enter(&mut self) -> Vec<Action> {
        match self.step {
            Step::Display => {
                self.pattern = Pattern::Blink;
                vec![]
            }
            Step::Buzzer => {
                self.volume = VOLUME_START;
                vec![Action::BuzzerOn {
                    volume: self.volume,
                }]
            }
            Step::Leds => vec![Action::PlayLeds],
            Step::Report => vec![Action::Report],
            Step::Done => vec![Action::Exit],
            Step::Buttons => vec![],
        }
    }

    pub fn press(&mut self, b: Btn) -> Vec<Action> {
        self.event("press", format!("{b:?}").to_lowercase());
        match self.step {
            Step::Buttons => {
                if BUTTON_ORDER.get(self.buttons_done) == Some(&b) {
                    self.buttons_done += 1;
                    if self.buttons_done == BUTTON_ORDER.len() {
                        return self.finish(Outcome::Pass, "UP, MIDDLE and DOWN pressed".into());
                    }
                } else {
                    // A wrong button is noted but not fatal: the operator may
                    // simply have pressed ahead; the expected one still counts.
                    self.event("info", "not the button asked for".into());
                }
                vec![]
            }
            Step::Display => match b {
                Btn::Up => {
                    self.pattern = Pattern::AllOn;
                    self.event("pattern", "all on".into());
                    vec![]
                }
                Btn::Down => {
                    self.pattern = Pattern::Checker;
                    self.event("pattern", "checkerboard".into());
                    vec![]
                }
                Btn::Middle => self.finish(Outcome::Pass, "confirmed on the device".into()),
            },
            Step::Buzzer => match b {
                Btn::Up | Btn::Down => {
                    self.volume = if b == Btn::Up {
                        (self.volume + VOLUME_STEP).min(VOLUME_MAX)
                    } else {
                        self.volume.saturating_sub(VOLUME_STEP).max(VOLUME_MIN)
                    };
                    self.event("volume", self.volume.to_string());
                    vec![Action::BuzzerVolume(self.volume)]
                }
                Btn::Middle => self.finish(Outcome::Pass, "confirmed on the device".into()),
            },
            Step::Leds => match b {
                Btn::Up | Btn::Down => {
                    self.event("leds", "replay".into());
                    vec![Action::PlayLeds]
                }
                Btn::Middle => self.finish(Outcome::Pass, "confirmed on the device".into()),
            },
            Step::Report => match b {
                Btn::Middle => self.finish(Outcome::Pass, String::new()),
                _ => vec![],
            },
            Step::Done => vec![],
        }
    }

    /// The operator marks the current step failed from the app (a dead display
    /// or a button that never registers cannot be confirmed on the device).
    pub fn fail(&mut self, note: &str) -> Vec<Action> {
        match self.step {
            Step::Report | Step::Done => vec![],
            _ => {
                let why = if note.is_empty() {
                    "failed by the operator".into()
                } else {
                    note.to_string()
                };
                self.finish(Outcome::Fail, why)
            }
        }
    }

    /// Abort: whatever runs is stopped and the test ends.
    pub fn abort(&mut self, why: &str) -> Vec<Action> {
        let mut actions = match self.step {
            Step::Buzzer => vec![Action::BuzzerOff],
            Step::Leds => vec![Action::StopLeds],
            _ => vec![],
        };
        if self.step != Step::Done {
            self.event("info", format!("aborted: {why}"));
            self.step = Step::Done;
            actions.push(Action::Exit);
        }
        actions
    }

    pub fn events_since(&self, seq: u64) -> Vec<Event> {
        self.events
            .iter()
            .filter(|e| e.seq > seq)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass_buttons(m: &mut Machine) {
        for b in BUTTON_ORDER {
            m.press(b);
        }
    }

    #[test]
    fn buttons_must_come_in_order_and_a_wrong_one_is_not_fatal() {
        let mut m = Machine::new();
        assert!(m.press(Btn::Down).is_empty());
        assert_eq!(m.buttons_done, 0);
        m.press(Btn::Up);
        m.press(Btn::Middle);
        assert_eq!(m.step, Step::Buttons);
        m.press(Btn::Down);
        assert_eq!(m.step, Step::Display);
        assert_eq!(m.outcome(Step::Buttons), Some(Outcome::Pass));
    }

    #[test]
    fn display_switches_patterns_and_middle_confirms() {
        let mut m = Machine::new();
        pass_buttons(&mut m);
        assert_eq!(m.pattern, Pattern::Blink);
        m.press(Btn::Up);
        assert_eq!(m.pattern, Pattern::AllOn);
        m.press(Btn::Down);
        assert_eq!(m.pattern, Pattern::Checker);
        let a = m.press(Btn::Middle);
        assert_eq!(m.step, Step::Buzzer);
        assert_eq!(
            a,
            vec![Action::BuzzerOn {
                volume: VOLUME_START
            }]
        );
    }

    #[test]
    fn buzzer_volume_is_clamped_and_middle_silences_it() {
        let mut m = Machine::new();
        pass_buttons(&mut m);
        m.press(Btn::Middle);
        for _ in 0..10 {
            m.press(Btn::Up);
        }
        assert_eq!(m.volume, VOLUME_MAX);
        for _ in 0..20 {
            m.press(Btn::Down);
        }
        assert_eq!(m.press(Btn::Down), vec![Action::BuzzerVolume(VOLUME_MIN)]);
        let a = m.press(Btn::Middle);
        assert_eq!(a, vec![Action::BuzzerOff, Action::PlayLeds]);
        assert_eq!(m.step, Step::Leds);
    }

    #[test]
    fn leds_replay_then_report_then_exit() {
        let mut m = Machine::new();
        pass_buttons(&mut m);
        m.press(Btn::Middle);
        m.press(Btn::Middle);
        assert_eq!(m.press(Btn::Down), vec![Action::PlayLeds]);
        assert_eq!(m.press(Btn::Middle), vec![Action::StopLeds, Action::Report]);
        assert!(m.interactive_passed());
        assert_eq!(m.press(Btn::Up), vec![]);
        assert_eq!(m.press(Btn::Middle), vec![Action::Exit]);
        assert_eq!(m.step, Step::Done);
    }

    #[test]
    fn operator_fail_records_and_moves_on() {
        let mut m = Machine::new();
        pass_buttons(&mut m);
        m.fail("no pixels");
        assert_eq!(m.outcome(Step::Display), Some(Outcome::Fail));
        assert_eq!(m.step, Step::Buzzer);
        assert!(!m.interactive_passed());
    }

    #[test]
    fn abort_stops_the_buzzer() {
        let mut m = Machine::new();
        pass_buttons(&mut m);
        m.press(Btn::Middle);
        assert_eq!(m.abort("timeout"), vec![Action::BuzzerOff, Action::Exit]);
        assert!(m.abort("again").is_empty());
    }

    #[test]
    fn events_since_returns_only_newer() {
        let mut m = Machine::new();
        let first = m.events.last().unwrap().seq;
        m.press(Btn::Up);
        let newer = m.events_since(first);
        assert_eq!(newer.len(), 1);
        assert_eq!(newer[0].kind, "press");
    }
}
