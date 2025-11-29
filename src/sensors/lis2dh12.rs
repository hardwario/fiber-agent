// src/sensors/lis2dh12.rs
use anyhow::Result;
use i2cdev::core::*;
use i2cdev::linux::LinuxI2CDevice;
use std::thread;
use std::time::Duration;

// --- Registers ---
const LIS2DH12_ADDR: u16 = 0x19;
const REG_WHO_AM_I: u8 = 0x0F;
const REG_CTRL_REG1: u8 = 0x20;
const REG_OUT_X_L: u8 = 0x28;

// 0x57 = 0b01010111 = 100Hz, normal mode, XYZ enabled
const CTRL_REG1_CONFIG: u8 = 0x57;

#[derive(Debug, Clone, Copy)]
pub struct AccelData {
    pub x_g: f32,
    pub y_g: f32,
    pub z_g: f32,
}

pub struct Lis2dh12 {
    dev: LinuxI2CDevice,
}

impl Lis2dh12 {
    pub fn new(i2c_path: &str) -> Result<Self> {
        let mut dev = LinuxI2CDevice::new(i2c_path, LIS2DH12_ADDR)?;

        // 1. Verify Device ID
        let who_am_i = dev.smbus_read_byte_data(REG_WHO_AM_I)?;
        println!("LIS2DH12 WHO_AM_I: 0x{:02X}", who_am_i);

        if who_am_i != 0x33 {
            println!("WARNING: Unexpected LIS2DH12 Device ID! (Expected 0x33)");
        } else {
            println!("LIS2DH12 identified successfully.");
        }

        // 2. Initialize Sensor
        dev.smbus_write_byte_data(REG_CTRL_REG1, CTRL_REG1_CONFIG)?;
        println!("LIS2DH12 initialized (100Hz, Normal Mode, XYZ).");

        // Wait briefly for boot
        thread::sleep(Duration::from_millis(100));

        Ok(Self { dev })
    }

    pub fn read(&mut self) -> Result<AccelData> {
        // Read 6 bytes starting at OUT_X_L with auto-increment
        let mut buf = [0u8; 6];
        self.dev.write(&[REG_OUT_X_L | 0x80])?;
        self.dev.read(&mut buf)?;

        let x_raw = (buf[0] as i16) | ((buf[1] as i16) << 8);
        let y_raw = (buf[2] as i16) | ((buf[3] as i16) << 8);
        let z_raw = (buf[4] as i16) | ((buf[5] as i16) << 8);

        // 10-bit left-aligned, ±2g, ~4 mg/LSB -> shift >> 6
        let x_g = (x_raw >> 6) as f32 * 0.004;
        let y_g = (y_raw >> 6) as f32 * 0.004;
        let z_g = (z_raw >> 6) as f32 * 0.004;

        Ok(AccelData { x_g, y_g, z_g })
    }
}
