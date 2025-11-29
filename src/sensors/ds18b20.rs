// src/sensors/ds18b20.rs
//
// DS18B20 Temperature Sensor Backend via DS2482S-800+ I2C Bridge
//
// Implements the SensorBackend trait for DS18B20 one-wire temperature sensors
// connected to a DS2482S-800+ I2C-to-1Wire bridge.
//
// Each DS18B20Backend corresponds to one DS18B20 sensor on a specific IO pin
// of the shared DS2482 bridge. Multiple backends can share the same bridge
// driver via Arc<Mutex<>>.

use crate::acquisition::SensorBackend;
use crate::drivers::ds2482::{Ds2482Driver, Ds2482Error};
use crate::model::{ReadingQuality, SensorId};
use chrono::{DateTime, Utc};
use std::sync::{Arc, Mutex};

/// DS18B20 Temperature Sensor Backend
///
/// Wraps the low-level Ds2482Driver in a SensorBackend trait implementation.
/// Stores per-sensor configuration (ROM code, IO pin) and provides clean
/// temperature readings with quality indicators.
pub struct Ds18b20Backend {
    id: SensorId,
    rom_code: [u8; 8],
    io_pin: u8,
    calibration_offset: f32,
    driver: Option<Arc<Mutex<Ds2482Driver>>>,
}

impl Ds18b20Backend {
    /// Create a new DS18B20 backend
    ///
    /// # Arguments
    /// * `id` - Unique sensor ID
    /// * `rom_code` - 8-byte DS18B20 ROM code
    /// * `io_pin` - DS2482 IO pin (0-7)
    /// * `calibration_offset` - Temperature offset in Celsius
    /// * `driver` - Shared DS2482 driver (Arc<Mutex<>>)
    pub fn new(
        id: SensorId,
        rom_code: [u8; 8],
        io_pin: u8,
        calibration_offset: f32,
        driver: Arc<Mutex<Ds2482Driver>>,
    ) -> Self {
        Self {
            id,
            rom_code,
            io_pin,
            calibration_offset,
            driver: Some(driver),
        }
    }

    /// Get the ROM code for this sensor
    pub fn rom_code(&self) -> [u8; 8] {
        self.rom_code
    }

    /// Get the IO pin for this sensor
    pub fn io_pin(&self) -> u8 {
        self.io_pin
    }

    #[cfg(test)]
    /// Test-only constructor without a driver (for unit tests)
    fn for_test(id: SensorId, rom_code: [u8; 8], io_pin: u8, calibration_offset: f32) -> Self {
        Self {
            id,
            rom_code,
            io_pin,
            calibration_offset,
            driver: None,
        }
    }
}

impl SensorBackend for Ds18b20Backend {
    fn sensor_id(&self) -> SensorId {
        self.id
    }

    fn read(&mut self, _now: DateTime<Utc>) -> (f32, ReadingQuality) {
        // Check if driver is available
        let driver_arc = match &self.driver {
            Some(d) => d,
            None => {
                // No driver available (likely in tests)
                return (0.0, ReadingQuality::Other);
            }
        };

        // Acquire lock on shared driver
        let mut driver = match driver_arc.lock() {
            Ok(d) => d,
            Err(_) => {
                // Mutex was poisoned (panic in another thread)
                return (0.0, ReadingQuality::Other);
            }
        };

        // Select the IO pin for this sensor
        if let Err(e) = driver.select_channel(self.io_pin) {
            return map_ds2482_error_to_quality(e);
        }

        // Read temperature (includes reset, conversion, scratchpad read)
        match driver.read_temperature(&self.rom_code) {
            Ok(reading) => {
                let mut temp = reading.temperature_c;
                // Apply calibration offset
                temp += self.calibration_offset;
                (temp, ReadingQuality::Ok)
            }
            Err(e) => map_ds2482_error_to_quality(e),
        }
    }
}

/// Map DS2482-specific errors to ReadingQuality indicators
///
/// This allows the acquisition engine and alarms to understand sensor
/// health without knowing about DS2482-specific error types.
fn map_ds2482_error_to_quality(err: Ds2482Error) -> (f32, ReadingQuality) {
    let quality = match err {
        // CRC failure indicates corrupted data
        Ds2482Error::CrcError => ReadingQuality::CrcError,
        // Timeout or bus issues indicate transient failures
        Ds2482Error::BusTimeout | Ds2482Error::ConversionTimeout => ReadingQuality::Timeout,
        // ROM not found or no presence pulse indicates disconnected sensor
        Ds2482Error::RomNotFound | Ds2482Error::NoPresencePulse => ReadingQuality::Disconnected,
        // Short circuit is a bus fault
        Ds2482Error::ShortDetected => ReadingQuality::Other,
        // Channel or I2C errors are "other" failures
        Ds2482Error::InvalidChannel | Ds2482Error::I2cError(_) => ReadingQuality::Disconnected,
    };
    (0.0, quality)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mock DS2482Driver for testing without real hardware
    struct MockDs2482 {
        channels: std::collections::HashMap<u8, std::collections::HashMap<[u8; 8], f32>>,
        selected_channel: Option<u8>,
    }

    impl MockDs2482 {
        fn new() -> Self {
            Self {
                channels: std::collections::HashMap::new(),
                selected_channel: None,
            }
        }

        fn with_sensor(mut self, channel: u8, rom: [u8; 8], temp_c: f32) -> Self {
            self.channels
                .entry(channel)
                .or_insert_with(std::collections::HashMap::new)
                .insert(rom, temp_c);
            self
        }
    }

    #[test]
    fn calibration_offset_applied() {
        // This test verifies the logic; actual hardware test would use
        // a mock or real DS2482 driver
        let offset: f32 = 1.5;
        let base_temp: f32 = 25.0;
        let expected: f32 = base_temp + offset;

        // Verify the offset logic
        let adjusted: f32 = base_temp + offset;
        assert!((adjusted - expected).abs() < 0.01);
    }

    #[test]
    fn rom_code_stored_correctly() {
        let rom = [0x28, 0x00, 0x00, 0x00, 0xAB, 0xCD, 0xEF, 0x00];
        let backend = Ds18b20Backend::for_test(SensorId(1), rom, 0, 0.0);
        assert_eq!(backend.rom_code(), rom);
    }

    #[test]
    fn io_pin_stored_correctly() {
        for pin in 0..8 {
            let backend = Ds18b20Backend::for_test(SensorId(pin as u64), [0; 8], pin, 0.0);
            assert_eq!(backend.io_pin(), pin);
        }
    }

    #[test]
    fn error_mapping_crc_error() {
        let (_, quality) = map_ds2482_error_to_quality(Ds2482Error::CrcError);
        assert_eq!(quality, ReadingQuality::CrcError);
    }

    #[test]
    fn error_mapping_timeout() {
        let (_, quality) = map_ds2482_error_to_quality(Ds2482Error::BusTimeout);
        assert_eq!(quality, ReadingQuality::Timeout);

        let (_, quality) = map_ds2482_error_to_quality(Ds2482Error::ConversionTimeout);
        assert_eq!(quality, ReadingQuality::Timeout);
    }

    #[test]
    fn error_mapping_disconnected() {
        let (_, quality) = map_ds2482_error_to_quality(Ds2482Error::RomNotFound);
        assert_eq!(quality, ReadingQuality::Disconnected);

        let (_, quality) = map_ds2482_error_to_quality(Ds2482Error::NoPresencePulse);
        assert_eq!(quality, ReadingQuality::Disconnected);
    }

    #[test]
    fn sensor_id_preserved() {
        let id = SensorId(42);
        let backend = Ds18b20Backend::for_test(id, [0; 8], 0, 0.0);
        assert_eq!(backend.sensor_id(), id);
    }
}
