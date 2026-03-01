use anyhow::Result;
use rppal::gpio::{Gpio, OutputPin};
use std::thread;
use std::time::{Duration, Instant};

const BUZZER_PIN: u8 = 17;

pub struct Buzzer {
    pin: OutputPin,
}

impl Buzzer {
    /// Create a new Buzzer using an existing Gpio instance
    pub fn new(gpio: &Gpio) -> Result<Self> {
        let mut pin = gpio.get(BUZZER_PIN)?.into_output();

        // Assuming "inactive" = high
        pin.set_high();

        Ok(Self { pin })
    }

    pub fn on(&mut self) {
        // Active-low buzzer
        self.pin.set_low();
    }

    pub fn off(&mut self) {
        self.pin.set_high();
    }

    /// Set buzzer state directly (non-blocking)
    /// Used for precise timing control in the monitor loop
    pub fn set_state(&mut self, on: bool) {
        if on {
            self.pin.set_low();  // Buzzer ON (active-low)
        } else {
            self.pin.set_high(); // Buzzer OFF (inactive)
        }
    }

    /// Set buzzer state with volume control.
    /// volume 0 = muted (always off), volume 1-100 = active (full on when on=true).
    /// True PWM volume control on a GPIO piezo buzzer is unreliable,
    /// so this implements: 0 = muted, 1-100 = active at full volume.
    pub fn set_state_with_volume(&mut self, on: bool, volume: u8) {
        if !on || volume == 0 {
            self.pin.set_high(); // OFF (inactive)
        } else {
            self.pin.set_low();  // ON (active-low)
        }
    }

    /// Generate PWM (Pulse Width Modulation) beeping pattern
    /// Creates rapid toggling at specified frequency to produce audible beeping
    ///
    /// # Arguments
    /// * `frequency_hz` - Frequency in Hz (100-200 typical for piezo buzzers)
    /// * `duration_ms` - Duration of beeping in milliseconds
    pub fn beep_pwm(&mut self, frequency_hz: u32, duration_ms: u64) {
        if frequency_hz == 0 || duration_ms == 0 {
            return;
        }

        // Calculate period from frequency
        let period_us = 1_000_000 / frequency_hz as u64;
        let half_period_us = period_us / 2;
        let half_period = Duration::from_micros(half_period_us);

        let start = Instant::now();
        let duration = Duration::from_millis(duration_ms);

        // Rapidly toggle pin to create PWM waveform
        while start.elapsed() < duration {
            self.pin.set_low();    // Buzzer ON (active-low)
            thread::sleep(half_period);

            self.pin.set_high();   // Buzzer OFF (inactive)
            thread::sleep(half_period);
        }

        // Ensure pin is high (inactive) after beeping
        self.pin.set_high();
    }
}
