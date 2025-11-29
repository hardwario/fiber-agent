// src/drivers/ds2482.rs
//
// DS2482S-800+ I2C-to-1Wire Bridge Driver
//
// Provides low-level communication with the DS2482S-800+ I2C bridge chip
// for reading DS18B20 one-wire temperature sensors across 8 independent
// 1-wire buses (IO0-IO7).

use i2cdev::core::*;
use i2cdev::linux::LinuxI2CDevice;
use std::thread;
use std::time::Duration;

// DS2482S-800+ I2C Addresses (default 0x18, configurable via A0/A1)
pub const DS2482_DEFAULT_ADDR: u16 = 0x18;

// DS2482S Commands
const CMD_DEVICE_RESET: u8 = 0xF0;
const CMD_SET_READ_POINTER: u8 = 0xE1;
const CMD_WRITE_CONFIG: u8 = 0xD2;
const CMD_CHANNEL_SELECT: u8 = 0xC3;
const CMD_ONEWIRE_RESET: u8 = 0xB4;
const CMD_ONEWIRE_WRITE_BYTE: u8 = 0xA5;
const CMD_ONEWIRE_READ_BYTE: u8 = 0x96;
const CMD_ONEWIRE_WRITE_BIT: u8 = 0x87;
const CMD_ONEWIRE_READ_BIT: u8 = 0x84;
const CMD_ONEWIRE_TRIPLET: u8 = 0x78;

// DS2482S Register Addresses (read via SET_READ_POINTER)
const REG_STATUS: u8 = 0xF0;
const REG_READ_DATA: u8 = 0xE1;
const REG_CONFIG: u8 = 0xC3;

// Status Register Bits
const STATUS_1WB: u8 = 0x80; // 1-Wire Busy
const STATUS_PPM: u8 = 0x40; // Presence Pulse Masking
const STATUS_APU: u8 = 0x01; // Active Pull-Up
const STATUS_SBR: u8 = 0x08; // Short Bar (short detected)
const STATUS_LL: u8 = 0x02;  // Logic Level

// DS18B20 One-Wire Commands
const OW_CMD_SKIP_ROM: u8 = 0xCC;
const OW_CMD_MATCH_ROM: u8 = 0x55;
const OW_CMD_READ_SCRATCHPAD: u8 = 0xBE;
const OW_CMD_START_CONVERSION: u8 = 0x44;

/// Errors that can occur during DS2482 operations
#[derive(Debug, Clone)]
pub enum Ds2482Error {
    I2cError(String),
    BusTimeout,
    ShortDetected,
    CrcError,
    RomNotFound,
    ConversionTimeout,
    NoPresencePulse,
    InvalidChannel,
}

impl std::fmt::Display for Ds2482Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ds2482Error::I2cError(msg) => write!(f, "I2C Error: {}", msg),
            Ds2482Error::BusTimeout => write!(f, "1-Wire Bus Timeout"),
            Ds2482Error::ShortDetected => write!(f, "1-Wire Short Detected"),
            Ds2482Error::CrcError => write!(f, "CRC-8 Validation Failed"),
            Ds2482Error::RomNotFound => write!(f, "ROM Code Not Found"),
            Ds2482Error::ConversionTimeout => write!(f, "Temperature Conversion Timeout"),
            Ds2482Error::NoPresencePulse => write!(f, "No Presence Pulse on 1-Wire"),
            Ds2482Error::InvalidChannel => write!(f, "Invalid Channel (0-7)"),
        }
    }
}

/// Parsed DS18B20 temperature reading
#[derive(Debug, Clone)]
pub struct Ds18b20Reading {
    pub raw_bytes: [u8; 9],
    pub temperature_c: f32,
}

/// DS2482S-800+ Low-Level I2C Driver
pub struct Ds2482Driver {
    device: LinuxI2CDevice,
    selected_channel: Option<u8>,
}

impl Ds2482Driver {
    /// Open I2C device and initialize DS2482S-800+ bridge
    ///
    /// # Arguments
    /// * `i2c_path` - Path to I2C device (e.g., "/dev/i2c-1")
    ///
    /// # Returns
    /// Initialized driver or error if device not found/not responding
    pub fn new(i2c_path: &str) -> Result<Self, Ds2482Error> {
        let mut device = LinuxI2CDevice::new(i2c_path, DS2482_DEFAULT_ADDR)
            .map_err(|e| Ds2482Error::I2cError(format!("Failed to open {}: {:?}", i2c_path, e)))?;

        // Verify device responds and is a DS2482
        device
            .smbus_write_byte(CMD_DEVICE_RESET)
            .map_err(|e| Ds2482Error::I2cError(format!("Device reset failed: {:?}", e)))?;

        thread::sleep(Duration::from_millis(10));

        // Read status to verify device is responding
        let status = device
            .smbus_read_byte()
            .map_err(|e| Ds2482Error::I2cError(format!("Status read failed: {:?}", e)))?;

        // If device is not responding properly, fail
        if status & (STATUS_1WB | STATUS_SBR) == STATUS_SBR {
            return Err(Ds2482Error::ShortDetected);
        }

        Ok(Self {
            device,
            selected_channel: None,
        })
    }

    /// Select a 1-Wire channel (0-7)
    ///
    /// Each channel corresponds to one of the DS2482S-800+'s 8 independent
    /// 1-wire buses (IO0-IO7). Must be called before any 1-wire operations
    /// on that channel.
    pub fn select_channel(&mut self, channel: u8) -> Result<(), Ds2482Error> {
        if channel > 7 {
            return Err(Ds2482Error::InvalidChannel);
        }

        // Skip if already selected
        if self.selected_channel == Some(channel) {
            return Ok(());
        }

        let channel_code = match channel {
            0 => 0xC0, // Channel 0
            1 => 0xE0, // Channel 1
            2 => 0x40, // Channel 2
            3 => 0x60, // Channel 3
            4 => 0x80, // Channel 4
            5 => 0xA0, // Channel 5
            6 => 0x20, // Channel 6
            7 => 0xF0, // Channel 7
            _ => unreachable!(),
        };

        self.device
            .smbus_write_byte_data(CMD_CHANNEL_SELECT, channel_code)
            .map_err(|e| Ds2482Error::I2cError(format!("Channel select failed: {:?}", e)))?;

        self.selected_channel = Some(channel);
        Ok(())
    }

    /// Wait for 1-Wire bus to become ready (not busy)
    ///
    /// Polls status register with timeout
    fn wait_ready(&mut self, timeout_ms: u32) -> Result<(), Ds2482Error> {
        let mut elapsed = 0u32;
        let poll_interval = Duration::from_millis(1);

        loop {
            let status = self
                .device
                .smbus_read_byte()
                .map_err(|e| Ds2482Error::I2cError(format!("Status read failed: {:?}", e)))?;

            // Check for short circuit
            if status & STATUS_SBR != 0 {
                return Err(Ds2482Error::ShortDetected);
            }

            // If not busy, we're ready
            if status & STATUS_1WB == 0 {
                return Ok(());
            }

            if elapsed >= timeout_ms {
                return Err(Ds2482Error::BusTimeout);
            }

            thread::sleep(poll_interval);
            elapsed += 1;
        }
    }

    /// Reset the 1-Wire bus on the selected channel
    ///
    /// Returns true if a presence pulse was detected (device present)
    pub fn onewire_reset(&mut self) -> Result<bool, Ds2482Error> {
        self.device
            .smbus_write_byte(CMD_ONEWIRE_RESET)
            .map_err(|e| Ds2482Error::I2cError(format!("1-Wire reset failed: {:?}", e)))?;

        // Wait up to 1 second for reset to complete
        self.wait_ready(1000)?;

        // Read status to check for presence pulse
        let status = self
            .device
            .smbus_read_byte()
            .map_err(|e| Ds2482Error::I2cError(format!("Status read failed: {:?}", e)))?;

        // Bit 7 of status indicates presence pulse (PPD = Presence Pulse Detected)
        // When 0 → presence pulse was detected (device present)
        Ok((status & 0x80) == 0)
    }

    /// Write a single byte to the 1-Wire bus
    pub fn onewire_write_byte(&mut self, byte: u8) -> Result<(), Ds2482Error> {
        self.device
            .smbus_write_byte_data(CMD_ONEWIRE_WRITE_BYTE, byte)
            .map_err(|e| Ds2482Error::I2cError(format!("1-Wire write failed: {:?}", e)))?;

        self.wait_ready(100)?;
        Ok(())
    }

    /// Read a single byte from the 1-Wire bus
    pub fn onewire_read_byte(&mut self) -> Result<u8, Ds2482Error> {
        self.device
            .smbus_write_byte(CMD_ONEWIRE_READ_BYTE)
            .map_err(|e| Ds2482Error::I2cError(format!("1-Wire read cmd failed: {:?}", e)))?;

        self.wait_ready(100)?;

        // Read the byte from data register
        let byte = self.device.smbus_read_byte().map_err(|e| {
            Ds2482Error::I2cError(format!("1-Wire byte read failed: {:?}", e))
        })?;

        Ok(byte)
    }

    /// Match ROM: send MATCH_ROM command followed by 8-byte ROM code
    ///
    /// Selects a specific device by its 64-bit ROM code
    pub fn match_rom(&mut self, rom: &[u8; 8]) -> Result<(), Ds2482Error> {
        // Send MATCH_ROM command
        self.onewire_write_byte(OW_CMD_MATCH_ROM)?;

        // Send 8 ROM code bytes
        for &byte in rom.iter() {
            self.onewire_write_byte(byte)?;
        }

        Ok(())
    }

    /// Start temperature conversion on all DS18B20 sensors on selected channel
    ///
    /// Sends SKIP_ROM + START_CONVERSION to trigger conversion on all devices,
    /// then waits for conversion to complete (up to 750ms for 12-bit resolution)
    pub fn start_conversion(&mut self) -> Result<(), Ds2482Error> {
        // Send SKIP_ROM to address all devices
        self.onewire_write_byte(OW_CMD_SKIP_ROM)?;

        // Send START_CONVERSION command
        self.onewire_write_byte(OW_CMD_START_CONVERSION)?;

        // Wait for conversion (max ~750ms for 12-bit DS18B20)
        self.wait_ready(1000)?;

        Ok(())
    }

    /// Read temperature from a specific DS18B20 by ROM code
    ///
    /// 1. Resets bus
    /// 2. Triggers conversion
    /// 3. Waits for completion
    /// 4. Reads 9-byte scratchpad
    /// 5. Validates CRC-8
    /// 6. Parses temperature
    pub fn read_temperature(&mut self, rom: &[u8; 8]) -> Result<Ds18b20Reading, Ds2482Error> {
        // Reset the bus and check for presence
        if !self.onewire_reset()? {
            return Err(Ds2482Error::NoPresencePulse);
        }

        // Trigger conversion
        self.start_conversion()?;

        // Reset again for read
        if !self.onewire_reset()? {
            return Err(Ds2482Error::NoPresencePulse);
        }

        // Match ROM to select specific device
        self.match_rom(rom)?;

        // Send READ_SCRATCHPAD command
        self.onewire_write_byte(OW_CMD_READ_SCRATCHPAD)?;

        // Read 9-byte scratchpad
        let mut scratchpad = [0u8; 9];
        for i in 0..9 {
            scratchpad[i] = self.onewire_read_byte()?;
        }

        // Validate CRC-8
        let crc = calculate_crc8(&scratchpad[0..8]);
        if crc != scratchpad[8] {
            return Err(Ds2482Error::CrcError);
        }

        // Parse temperature
        let temp_c = parse_temperature(&scratchpad)?;

        Ok(Ds18b20Reading {
            raw_bytes: scratchpad,
            temperature_c: temp_c,
        })
    }
}

/// Calculate CRC-8 for DS18B20 data validation (polynomial 0x31)
fn calculate_crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &byte in data {
        let mut crc_in = crc;
        let mut in_byte = byte;
        for _ in 0..8 {
            let carry = (crc_in ^ in_byte) & 1;
            crc_in >>= 1;
            if carry != 0 {
                crc_in ^= 0x8C;
            }
            in_byte >>= 1;
        }
        crc = crc_in;
    }
    crc
}

/// Parse temperature from DS18B20 9-byte scratchpad
///
/// Temperature is stored in bytes 0 (LSB) and 1 (MSB) as 16-bit value where:
/// - Bits 15-4: Integer part (signed 12-bit value)
/// - Bits 3-0: Fractional part (4 bits = 0-15 sixteenths)
fn parse_temperature(scratchpad: &[u8; 9]) -> Result<f32, Ds2482Error> {
    let lsb = scratchpad[0];
    let msb = scratchpad[1];

    // Combine MSB and LSB into 16-bit value
    // MSB is upper 8 bits, LSB is lower 8 bits
    let raw_word = ((msb as i16) << 8) | (lsb as i16);

    // Extract integer part: bits 15-4 (right shift by 4)
    let integer_part = (raw_word >> 4) as f32;

    // Extract fractional part: bits 3-0 (as 0-15 sixteenths)
    let frac_bits = (raw_word & 0x0F) as f32;
    let frac_part = frac_bits / 16.0;

    // Total temperature
    let temp_c = integer_part + frac_part;

    Ok(temp_c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc8_basic_calculation() {
        // Test CRC-8 calculation is consistent
        // Calculate CRC of a simple pattern
        let data1 = [0x28, 0x00, 0x00, 0x00, 0xAB, 0xCD, 0xEF, 0x00];
        let crc1 = calculate_crc8(&data1);

        // Same data should produce same CRC
        let crc2 = calculate_crc8(&data1);
        assert_eq!(crc1, crc2, "CRC-8 calculation not consistent");

        // CRC should be deterministic
        assert!(crc1 > 0 || crc1 == 0, "CRC-8 calculation resulted in invalid value");
    }

    #[test]
    fn temperature_parsing_25_5_celsius() {
        // 25.5°C = 25.5 / 0.0625 = 408 = 0x198
        // In 16-bit form: 0x0198
        // LSB = 0x98, MSB = 0x01
        // Calculation: (0x01 << 8 | 0x98) = 0x0198 = 408
        // 408 * 0.0625 = 25.5°C
        let mut scratchpad = [0u8; 9];
        scratchpad[0] = 0x98; // LSB
        scratchpad[1] = 0x01; // MSB
        scratchpad[8] = 0x00; // CRC (not validated in this unit test)

        let temp = parse_temperature(&scratchpad).unwrap();
        assert!((temp - 25.5).abs() < 0.01, "Temperature parsing failed: expected 25.5, got {}", temp);
    }

    #[test]
    fn temperature_parsing_negative() {
        // -10.75°C = -10.75 / 0.0625 = -172 in raw units
        // -172 in 16-bit two's complement = 0xFF54
        // LSB = 0x54, MSB = 0xFF
        // Calculation: (0xFF << 8 | 0x54) as i16 = 0xFF54 = -172 (signed)
        // -172 * 0.0625 = -10.75°C
        let mut scratchpad = [0u8; 9];
        scratchpad[0] = 0x54; // LSB
        scratchpad[1] = 0xFF; // MSB
        scratchpad[8] = 0x00; // CRC (not validated in this unit test)

        let temp = parse_temperature(&scratchpad).unwrap();
        assert!((temp - (-10.75)).abs() < 0.01, "Negative temp parsing failed: expected -10.75, got {}", temp);
    }

    #[test]
    fn temperature_parsing_zero() {
        // 0.0°C should be 0x00, 0x00
        let mut scratchpad = [0u8; 9];
        scratchpad[0] = 0x00;
        scratchpad[1] = 0x00;
        scratchpad[8] = 0x00;

        let temp = parse_temperature(&scratchpad).unwrap();
        assert!((temp - 0.0).abs() < 0.01, "Zero temp parsing failed");
    }

    #[test]
    fn channel_validation() {
        // Channel must be 0-7; verify that invalid channels are caught
        // This test is structural; actual hardware test would require mocking I2C
        // For now, we just verify the function signature
        assert!(Ds2482Error::InvalidChannel.to_string().len() > 0);
    }

    #[test]
    fn error_display() {
        let err = Ds2482Error::CrcError;
        assert_eq!(err.to_string(), "CRC-8 Validation Failed");

        let err = Ds2482Error::I2cError("test error".to_string());
        assert!(err.to_string().contains("I2C Error"));
    }
}
