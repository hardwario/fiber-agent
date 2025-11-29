// src/buttons.rs
use anyhow::Result;
use rppal::gpio::{Gpio, InputPin};

// Pins for buttons
const BTN_UP: u8 = 23;
const BTN_ENTER: u8 = 24;
const BTN_DOWN: u8 = 25;

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

pub struct Buttons {
    up: InputPin,
    down: InputPin,
    enter: InputPin,
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
            last_up: true,
            last_down: true,
            last_enter: true,
        })
    }

    /// Poll buttons and return any edge events since last call.
    pub fn poll(&mut self) -> Vec<ButtonEvent> {
        let mut events = Vec::new();

        let curr_up = self.up.is_high();
        let curr_down = self.down.is_high();
        let curr_enter = self.enter.is_high();

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
