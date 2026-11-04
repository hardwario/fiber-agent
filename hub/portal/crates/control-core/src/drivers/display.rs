use anyhow::Result;
use embedded_graphics::{pixelcolor::BinaryColor, prelude::*};
use rppal::gpio::{Gpio, OutputPin};
use rppal::spi::{Bus, Mode, SlaveSelect, Spi};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

// PIN ASSIGNMENTS
pub(crate) const BL_PIN: u8 = 13;
const RST_PIN: u8 = 16;
const CS_PIN: u8 = 12;

// SPI
const SPI_BUS: Bus = Bus::Spi6;
const SPI_SS: SlaveSelect = SlaveSelect::Ss0;
const SPI_CLOCK_SPEED: u32 = 400_000;

// Display Geometry
const DISPLAY_WIDTH: usize = 128;
const DISPLAY_HEIGHT: usize = 64;
const BUFFER_SIZE: usize = (DISPLAY_WIDTH * DISPLAY_HEIGHT) / 8;

pub struct St7920 {
    spi: Spi,
    cs: OutputPin,
    rst: OutputPin,
    bl: Backlight,
    buffer: [u8; BUFFER_SIZE],
}

impl St7920 {
    pub fn new(gpio: Arc<Gpio>) -> Result<Self> {
        let mut cs = gpio.get(CS_PIN)?.into_output();
        let mut rst = gpio.get(RST_PIN)?.into_output();
        let bl = gpio.get(BL_PIN)?.into_output();
        let spi = Spi::new(SPI_BUS, SPI_SS, SPI_CLOCK_SPEED, Mode::Mode3)?;

        cs.set_low();
        rst.set_low();

        let mut display = Self {
            spi,
            cs,
            rst,
            bl: Backlight::start(bl)?,
            buffer: [0; BUFFER_SIZE],
        };
        display.init()?;
        Ok(display)
    }

    fn init(&mut self) -> Result<()> {
        self.rst.set_low();
        thread::sleep(Duration::from_millis(50));
        self.rst.set_high();
        thread::sleep(Duration::from_millis(100));

        self.send_command(0x30)?;
        thread::sleep(Duration::from_micros(200));
        self.send_command(0x30)?;
        thread::sleep(Duration::from_micros(50));
        self.send_command(0x0C)?;
        thread::sleep(Duration::from_micros(200));
        self.send_command(0x01)?;
        thread::sleep(Duration::from_millis(20));
        self.send_command(0x06)?;
        self.send_command(0x34)?;
        self.send_command(0x36)?;
        Ok(())
    }

    fn send_command(&mut self, cmd: u8) -> Result<()> {
        self.write_packet(0xF8, cmd)
    }
    fn send_data(&mut self, data: u8) -> Result<()> {
        self.write_packet(0xFA, data)
    }

    fn write_packet(&mut self, sync: u8, byte: u8) -> Result<()> {
        let buffer = [sync, byte & 0xF0, (byte & 0x0F) << 4];
        self.cs.set_high();
        self.spi.write(&buffer)?;
        self.cs.set_low();
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        for y in 0..32 {
            self.send_command(0x80 + y as u8)?;
            self.send_command(0x80)?;
            for x in 0..16 {
                self.send_data(self.buffer[y * 16 + x])?;
            }

            self.send_command(0x80 + y as u8)?;
            self.send_command(0x88)?;
            for x in 0..16 {
                self.send_data(self.buffer[(y + 32) * 16 + x])?;
            }
        }
        Ok(())
    }

    pub fn clear_buffer(&mut self) {
        self.buffer.fill(0);
    }
}

impl DrawTarget for St7920 {
    type Color = BinaryColor;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(point, color) in pixels.into_iter() {
            if point.x >= 0 && point.x < 128 && point.y >= 0 && point.y < 64 {
                let x = (127 - point.x) as usize; // Rotated 180
                let y = (63 - point.y) as usize;
                let idx = y * 16 + (x / 8);
                match color {
                    BinaryColor::On => self.buffer[idx] |= 0x80 >> (x % 8),
                    BinaryColor::Off => self.buffer[idx] &= !(0x80 >> (x % 8)),
                }
            }
        }
        Ok(())
    }
}

impl OriginDimensions for St7920 {
    fn size(&self) -> Size {
        Size::new(128, 64)
    }
}

impl St7920 {
    /// Set backlight brightness, 0-100 %.
    pub fn set_brightness(&mut self, percent: u8) -> Result<()> {
        self.bl.set(percent);
        Ok(())
    }
}

/// The backlight PWM period: 500 Hz, above visible flicker.
const BL_PERIOD: Duration = Duration::from_millis(2);
/// How often a steady (0 % or 100 %) backlight looks for a new level.
const BL_IDLE: Duration = Duration::from_millis(50);

/// What the backlight thread does for one brightness.
#[derive(Debug, PartialEq, Eq)]
enum BlPhase {
    /// The pin held at one level: no PWM, no wake-ups but the idle poll.
    Steady(bool),
    /// One PWM period: high for `on`, low for `off`.
    Pwm { on: Duration, off: Duration },
}

fn bl_phase(percent: u8) -> BlPhase {
    match percent.min(100) {
        0 => BlPhase::Steady(false),
        100 => BlPhase::Steady(true),
        p => {
            let on = BL_PERIOD * u32::from(p) / 100;
            BlPhase::Pwm {
                on,
                off: BL_PERIOD - on,
            }
        }
    }
}

/// Software PWM on GPIO 13 by plain sleeps. GPIO 13 can be hardware PWM, but the image loads no
/// `pwm` overlay, and rppal's software PWM busy-waits each edge: at 1 kHz that kept one core
/// about 40 % busy on every Hub (bench, 2026-10-29). A plain sleep is a few µs of CPU per edge;
/// its jitter is invisible on a backlight.
struct Backlight {
    percent: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Backlight {
    fn start(mut pin: OutputPin) -> Result<Self> {
        pin.set_high();
        let percent = Arc::new(AtomicU8::new(100));
        let stop = Arc::new(AtomicBool::new(false));
        let (p, s) = (percent.clone(), stop.clone());
        let thread = thread::Builder::new()
            .name("backlight".into())
            .spawn(move || {
                while !s.load(Ordering::Relaxed) {
                    match bl_phase(p.load(Ordering::Relaxed)) {
                        BlPhase::Steady(level) => {
                            if level {
                                pin.set_high();
                            } else {
                                pin.set_low();
                            }
                            thread::sleep(BL_IDLE);
                        }
                        BlPhase::Pwm { on, off } => {
                            pin.set_high();
                            thread::sleep(on);
                            pin.set_low();
                            thread::sleep(off);
                        }
                    }
                }
            })?;
        Ok(Self {
            percent,
            stop,
            thread: Some(thread),
        })
    }

    fn set(&self, percent: u8) {
        self.percent.store(percent.min(100), Ordering::Relaxed);
    }
}

impl Drop for Backlight {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod backlight_tests {
    use super::*;

    #[test]
    fn the_ends_hold_the_pin_and_the_middle_splits_the_period() {
        assert_eq!(bl_phase(0), BlPhase::Steady(false));
        assert_eq!(bl_phase(100), BlPhase::Steady(true));
        assert_eq!(bl_phase(250), BlPhase::Steady(true));
        assert_eq!(
            bl_phase(50),
            BlPhase::Pwm {
                on: Duration::from_millis(1),
                off: Duration::from_millis(1)
            }
        );
        assert_eq!(
            bl_phase(25),
            BlPhase::Pwm {
                on: Duration::from_micros(500),
                off: Duration::from_micros(1500)
            }
        );
    }
}
