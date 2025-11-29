// src/drivers/buzzer.rs
use anyhow::Result;
use rppal::gpio::{Gpio, OutputPin};

const BUZZER_PIN: u8 = 17;

pub struct Buzzer {
    pin: OutputPin,
}

impl Buzzer {
    pub fn new() -> Result<Self> {
        let gpio = Gpio::new()?;
        let mut pin = gpio.get(BUZZER_PIN)?.into_output();

        // Assuming "inactive" = high (like your previous code)
        pin.set_high();

        Ok(Self { pin })
    }

    pub fn on(&mut self) {
        // Active-low buzzer (like your previous code)
        self.pin.set_low();
    }

    pub fn off(&mut self) {
        self.pin.set_high();
    }
}
