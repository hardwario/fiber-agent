use anyhow::Result;
use rppal::gpio::{Gpio, OutputPin};
use std::thread;
use std::time::{Duration, Instant};

const BUZZER_PIN: u8 = 17;
/// PWM carrier frequency for volume control (4kHz - good for piezo buzzers)
const VOLUME_PWM_PERIOD: Duration = Duration::from_micros(250); // 4kHz

pub struct Buzzer {
    pin: OutputPin,
    /// Track whether software PWM is currently active
    pwm_active: bool,
}

impl Buzzer {
    /// Create a new Buzzer using an existing Gpio instance
    pub fn new(gpio: &Gpio) -> Result<Self> {
        let mut pin = gpio.get(BUZZER_PIN)?.into_output();

        // Assuming "inactive" = high
        pin.set_high();

        Ok(Self { pin, pwm_active: false })
    }

    pub fn on(&mut self) {
        self.stop_pwm();
        // Active-low buzzer
        self.pin.set_low();
    }

    pub fn off(&mut self) {
        self.stop_pwm();
        self.pin.set_high();
    }

    /// Set buzzer state directly (non-blocking)
    /// Used for precise timing control in the monitor loop
    pub fn set_state(&mut self, on: bool) {
        if on {
            self.stop_pwm();
            self.pin.set_low();  // Buzzer ON (active-low)
        } else {
            self.stop_pwm();
            self.pin.set_high(); // Buzzer OFF (inactive)
        }
    }

    /// Set buzzer state with volume control using software PWM.
    /// volume 0 = muted, 100 = full volume, 1-99 = PWM duty cycle modulation.
    /// Uses rppal's software PWM at 4kHz carrier frequency.
    /// For active-low buzzer: pin LOW = ON, so we vary LOW time for volume.
    pub fn set_state_with_volume(&mut self, on: bool, volume: u8) {
        if !on || volume == 0 {
            // OFF or muted
            self.stop_pwm();
            self.pin.set_high();
            return;
        }

        if volume >= 100 {
            // Full volume - no PWM overhead, just pin LOW
            self.stop_pwm();
            self.pin.set_low();
            return;
        }

        // Intermediate volume: use software PWM
        // Active-low: pulse_width = time pin is HIGH (buzzer off portion)
        // So pulse_width = period * (100 - volume) / 100
        let off_micros = 250u64 * (100 - volume as u64) / 100;
        let pulse_width = Duration::from_micros(off_micros);
        let _ = self.pin.set_pwm(VOLUME_PWM_PERIOD, pulse_width);
        self.pwm_active = true;
    }

    /// Stop software PWM if active
    fn stop_pwm(&mut self) {
        if self.pwm_active {
            let _ = self.pin.clear_pwm();
            self.pwm_active = false;
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
