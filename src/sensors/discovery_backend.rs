/// Discovery-based DS18B20 Sensor Backend
///
/// Reads temperature from /sys/bus/w1/devices/{rom}/temperature
/// ROM code is discovered dynamically, allowing hot-plug functionality

use crate::acquisition::SensorBackend;
use crate::model::{ReadingQuality, SensorId};
use chrono::{DateTime, Utc};
use std::sync::{Arc, Mutex, atomic::{AtomicU64, Ordering}};
use std::sync::OnceLock;

/// Global counter for sensor reading line numbers
static READING_LINE_COUNTER: OnceLock<AtomicU64> = OnceLock::new();

fn get_next_reading_line() -> u64 {
    let counter = READING_LINE_COUNTER.get_or_init(|| AtomicU64::new(1));
    let line_no = counter.fetch_add(1, Ordering::SeqCst);
    line_no
}

/// A sensor backend that uses discovered ROM codes
/// The ROM is updated dynamically by the discovery system
pub struct DiscoverySensorBackend {
    id: SensorId,
    io_pin: u8,
    calibration_offset: f32,
    current_rom: Arc<Mutex<Option<String>>>, // Shared with discovery system
    last_good_reading: std::sync::Mutex<Option<f32>>, // Cache for graceful degradation during hot-swap
}

impl DiscoverySensorBackend {
    /// Create with an externally managed ROM Arc (shared with discovery system)
    pub fn new_with_rom(
        id: SensorId,
        io_pin: u8,
        calibration_offset: f32,
        rom_arc: Arc<Mutex<Option<String>>>,
    ) -> Self {
        Self {
            id,
            io_pin,
            calibration_offset,
            current_rom: rom_arc,
            last_good_reading: std::sync::Mutex::new(None),
        }
    }

    /// Get last-known-good temperature reading (for graceful degradation during hot-swap)
    fn get_last_good_reading(&self) -> Option<f32> {
        self.last_good_reading.lock().ok().and_then(|r| *r)
    }

    /// Cache a successful temperature reading
    fn set_last_good_reading(&self, value: f32) {
        if let Ok(mut r) = self.last_good_reading.lock() {
            *r = Some(value);
        }
    }

    /// Create with its own ROM Arc (legacy, not recommended)
    pub fn new(
        id: SensorId,
        io_pin: u8,
        calibration_offset: f32,
    ) -> Self {
        Self::new_with_rom(id, io_pin, calibration_offset, Arc::new(Mutex::new(None)))
    }

    /// Update the ROM code (called by discovery system)
    pub fn set_rom(&self, rom: Option<String>) {
        if let Ok(mut r) = self.current_rom.lock() {
            match &rom {
                Some(r) => eprintln!("[discovery] Sensor {} (pin {}): ROM set to {}", self.id.0, self.io_pin, r),
                None => eprintln!("[discovery] Sensor {} (pin {}): ROM cleared", self.id.0, self.io_pin),
            }
            *r = rom;
        }
    }

    /// Get the current ROM code
    pub fn get_rom(&self) -> Option<String> {
        self.current_rom.lock().ok().and_then(|r| r.clone())
    }

    pub fn io_pin(&self) -> u8 {
        self.io_pin
    }

    /// Read temperature from sysfs
    fn read_from_sysfs(rom: &str) -> Result<f32, String> {
        use std::fs;
        use std::path::Path;

        let temp_path = format!("/sys/bus/w1/devices/{}/temperature", rom);
        let temp_file = Path::new(&temp_path);

        // Log the file path we're about to check
        eprintln!("[discovery-diag] sysfs checking file: {}", temp_path);

        if !temp_file.exists() {
            let err = format!("Sensor file not found: {}", temp_path);
            eprintln!("[discovery-diag] ✗ File does NOT exist: {}", temp_path);
            eprintln!("[discovery] sysfs read failed for {}: {}", rom, err);
            return Err(err);
        }

        eprintln!("[discovery-diag] ✓ File exists: {}", temp_path);

        match fs::read_to_string(temp_file) {
            Ok(temp_str) => {
                let trimmed = temp_str.trim();

                eprintln!("[discovery-diag] Read raw data from file (len={})", temp_str.len());

                // Check for empty data (sensor initializing)
                if trimmed.is_empty() {
                    let err = "Empty temperature data (sensor initializing)".to_string();
                    eprintln!("[discovery-diag] ✗ Empty data - sensor may be initializing");
                    eprintln!("[discovery] sysfs empty data for {}: {}", rom, err);
                    return Err(err);
                }

                let temp_raw: i32 = trimmed
                    .parse()
                    .map_err(|_| {
                        eprintln!("[discovery-diag] ✗ Parse failed, data: '{}'", trimmed);
                        format!("Failed to parse temperature: {}", trimmed)
                    })?;

                // DS18B20 returns temperature in millidegrees Celsius
                let temperature_c = (temp_raw as f32) / 1000.0;
                eprintln!("[discovery-diag] ✓ Parse success: {} ms = {:.2}°C", temp_raw, temperature_c);
                let line_no = get_next_reading_line();
                eprintln!("[discovery] [reading line {}] sysfs read success for {}: {:.2}°C (raw: {})", line_no, rom, temperature_c, temp_raw);
                Ok(temperature_c)
            }
            Err(e) => {
                let err = format!("Failed to read temperature: {}", e);
                eprintln!("[discovery-diag] ✗ File read failed: {}", e);
                eprintln!("[discovery] sysfs read error for {}: {}", rom, err);
                Err(err)
            }
        }
    }
}

impl SensorBackend for DiscoverySensorBackend {
    fn sensor_id(&self) -> SensorId {
        self.id
    }

    fn read(&mut self, _now: DateTime<Utc>) -> (f32, ReadingQuality) {
        // Try to get current ROM
        let rom = match self.get_rom() {
            Some(r) => {
                eprintln!("[discovery-diag] Sensor {} (pin {}): ROM found in Arc: {}", self.id.0, self.io_pin, r);
                r
            }
            None => {
                // Sensor not yet discovered - waiting for discovery system to find it
                // (Don't spam logs - only log once per sensor on first startup)
                return (0.0, ReadingQuality::Other);
            }
        };

        // Read from sysfs
        eprintln!("[discovery-diag] Sensor {} (pin {}): Attempting sysfs read for ROM {}", self.id.0, self.io_pin, rom);
        match Self::read_from_sysfs(&rom) {
            Ok(temp) => {
                let adjusted = temp + self.calibration_offset;
                // Cache the successful reading for graceful degradation during hot-swap
                self.set_last_good_reading(adjusted);
                eprintln!("[discovery-diag] Sensor {} (pin {}): ✓ RETURNING Ok({:.2}°C) after calibration", self.id.0, self.io_pin, adjusted);
                eprintln!("[discovery] Sensor {} (pin {}): Read OK, adjusted temp: {:.2}°C", self.id.0, self.io_pin, adjusted);
                (adjusted, ReadingQuality::Ok)
            }
            Err(e) => {
                eprintln!("[discovery-diag] Sensor {} (pin {}): sysfs read error: {}", self.id.0, self.io_pin, e);

                // Check if it's a parse/empty error (temporary) or actual disconnection
                if e.contains("parse temperature") || e.contains("Empty temperature data") {
                    // Temporary read failure - sensor might be initializing
                    // Return last-known-good value if available, otherwise use 0.0
                    let cached = self.get_last_good_reading().unwrap_or(0.0);
                    eprintln!("[discovery-diag] Sensor {} (pin {}): ✓ RETURNING Timeout({:.2}°C) - parse/empty error", self.id.0, self.io_pin, cached);
                    eprintln!("[discovery] Sensor {} (pin {}): Read failed (initializing), using cached: {:.2}°C",
                        self.id.0, self.io_pin, cached);
                    (cached, ReadingQuality::Timeout)
                } else {
                    // Actual disconnection or file not found
                    eprintln!("[discovery-diag] Sensor {} (pin {}): ✓ RETURNING Other(0.0) - file not found/disconnected", self.id.0, self.io_pin);
                    eprintln!("[discovery] Sensor {} (pin {}): Read failed (disconnected)", self.id.0, self.io_pin);
                    (0.0, ReadingQuality::Other)
                }
            }
        }
    }
}
