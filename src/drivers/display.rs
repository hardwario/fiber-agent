use anyhow::Result;
use embedded_graphics::{
    pixelcolor::BinaryColor,
    prelude::*,
};
use rppal::gpio::{Gpio, OutputPin};
use rppal::spi::{Bus, Mode, SlaveSelect, Spi};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

// PIN ASSIGNMENTS 
const BL_PIN: u8 = 13;
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
    bl: OutputPin,
    buffer: [u8; BUFFER_SIZE],
}

impl St7920 {
    pub fn new(gpio: Arc<Gpio>) -> Result<Self> {
        let mut cs = gpio.get(CS_PIN)?.into_output();
        let mut rst = gpio.get(RST_PIN)?.into_output();
        let mut bl = gpio.get(BL_PIN)?.into_output();
        let spi = Spi::new(SPI_BUS, SPI_SS, SPI_CLOCK_SPEED, Mode::Mode3)?;

        cs.set_low();
        rst.set_low();
        bl.set_high();

        let mut display = Self {
            spi,
            cs,
            rst,
            bl,
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

        self.send_command(0x30)?; thread::sleep(Duration::from_micros(200));
        self.send_command(0x30)?; thread::sleep(Duration::from_micros(50));
        self.send_command(0x0C)?; thread::sleep(Duration::from_micros(200));
        self.send_command(0x01)?; thread::sleep(Duration::from_millis(20));
        self.send_command(0x06)?;
        self.send_command(0x34)?;
        self.send_command(0x36)?;
        Ok(())
    }

    fn send_command(&mut self, cmd: u8) -> Result<()> { self.write_packet(0xF8, cmd) }
    fn send_data(&mut self, data: u8) -> Result<()> { self.write_packet(0xFA, data) }

    fn write_packet(&mut self, sync: u8, byte: u8) -> Result<()> {
        let buffer = [sync, byte & 0xF0, (byte & 0x0F) << 4];
        self.cs.set_high();
        self.spi.write(&buffer)?;
        self.cs.set_low();
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        for y in 0..32 {
            self.send_command(0x80 + y as u8)?; self.send_command(0x80)?;
            for x in 0..16 { self.send_data(self.buffer[y * 16 + x])?; }
            
            self.send_command(0x80 + y as u8)?; self.send_command(0x88)?;
            for x in 0..16 { self.send_data(self.buffer[(y + 32) * 16 + x])?; }
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
    /// Set backlight brightness using hardware PWM
    ///
    /// GPIO 13 supports hardware PWM (PWM1 channel 1) on Raspberry Pi.
    /// The brightness is specified as a percentage (0-100).
    pub fn set_brightness(&mut self, percent: u8) -> Result<()> {
        let duty = (percent.min(100) as f64) / 100.0;
        self.bl.set_pwm_frequency(1000.0, duty)?;
        Ok(())
    }
}
