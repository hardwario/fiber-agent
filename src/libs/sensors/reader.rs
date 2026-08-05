// W1 (One-Wire) sensor reading and device enumeration

use std::fs;
use std::io;
use std::time::Instant;

/// W1 sensor status
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SensorStatus {
    /// Sensor successfully read with temperature in Celsius
    Connected(f32),
    /// Sensor failed to read (timeout or error)
    Disconnected,
    /// Sensor read error with description
    Error,
}

/// Lowest temperature a DS18B20 can measure, per datasheet
const DS18B20_MIN_CELSIUS: f32 = -55.0;

/// Highest temperature a DS18B20 can measure, per datasheet.
/// Anything above this is a bus artefact, not a measurement — most commonly
/// 127.9375 (raw 0x07FF, an all-ones scratchpad read from a line that is not
/// answering yet: rails just energised, conversion incomplete, bus still
/// settling).
///
/// Note the mirror-image artefact 0xFFFF (-0.0625°C) is deliberately *not*
/// filtered: it is indistinguishable from a genuine reading just below zero,
/// which is squarely in range for the cold-chain this device monitors.
const DS18B20_MAX_CELSIUS: f32 = 125.0;

/// Value the DS18B20 temperature register holds after power-on, before the
/// first conversion completes
const DS18B20_POWER_ON_CELSIUS: f32 = 85.0;

/// Reject readings the hardware cannot have produced, so the caller's failure
/// debouncing handles them instead of the alarm thresholds.
///
/// Returns the temperature unchanged when it is plausible, or an
/// `InvalidData` error naming the rule that rejected it.
fn validate_temperature(temp_c: f32) -> io::Result<f32> {
    if !temp_c.is_finite() {
        // "NaN" and "inf" both parse successfully as f32, so a malformed sysfs
        // read reaches us as a number and would compare false against every
        // threshold.
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Sensor returned a non-finite value ({})", temp_c),
        ));
    }

    if (temp_c - DS18B20_POWER_ON_CELSIUS).abs() < 0.1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Sensor returned power-on default value (85°C)",
        ));
    }

    if temp_c < DS18B20_MIN_CELSIUS || temp_c > DS18B20_MAX_CELSIUS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Sensor returned {:.4}°C, outside the DS18B20 range {}..={}°C",
                temp_c, DS18B20_MIN_CELSIUS, DS18B20_MAX_CELSIUS
            ),
        ));
    }

    Ok(temp_c)
}

/// W1 device reader for enumerating and reading DS18B20 sensors
pub struct W1DeviceReader {
    base_path: String,
}

impl W1DeviceReader {
    /// Create a new W1 device reader
    pub fn new(base_path: &str) -> Self {
        Self {
            base_path: base_path.to_string(),
        }
    }

    /// Enumerate available DS18B20 sensors from /sys/bus/w1/devices/
    /// Searches through all w1_bus_master* directories to find 28-* devices
    /// Returns Vec of (line_number, device_id) tuples
    /// Line number is derived from w1_bus_master{N} (w1_bus_master1 = line 0, w1_bus_master2 = line 1, etc.)
    pub fn enum_devices(&self) -> io::Result<Vec<(u8, String)>> {
        let mut devices = Vec::new();

        // Read the base W1 devices directory
        let entries = fs::read_dir(&self.base_path)?;

        for entry_result in entries {
            let entry = entry_result?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            // Look for w1_bus_master directories (e.g., w1_bus_master1, w1_bus_master2, etc.)
            if name_str.starts_with("w1_bus_master") {
                // Extract the line number from w1_bus_master{N}
                if let Some(line_str) = name_str.strip_prefix("w1_bus_master") {
                    if let Ok(line_num) = line_str.parse::<u8>() {
                        // Line number is 1-based from w1_bus_master, convert to 0-based
                        let line_idx = line_num.saturating_sub(1);

                        // Now search inside this w1_bus_master directory for 28-* devices
                        let bus_path = format!("{}/{}", self.base_path, name_str);
                        if let Ok(bus_entries) = fs::read_dir(&bus_path) {
                            for bus_entry_result in bus_entries {
                                if let Ok(bus_entry) = bus_entry_result {
                                    let device_name = bus_entry.file_name();
                                    let device_str = device_name.to_string_lossy();

                                    // Found a DS18B20 sensor
                                    if device_str.starts_with("28-") {
                                        devices.push((line_idx, device_str.to_string()));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Sort by line number for consistent ordering
        devices.sort_by_key(|d| d.0);

        Ok(devices)
    }

    /// Read temperature from a specific DS18B20 sensor
    /// device_id: device identifier like "28-000000afb5c3"
    /// line_num: which w1_bus_master line (0-based)
    /// timeout_ms: timeout in milliseconds for the read operation
    /// Returns temperature in Celsius or error
    pub fn read_temperature(
        &self,
        line_num: u8,
        device_id: &str,
        timeout_ms: u64,
    ) -> io::Result<f32> {
        self.read_temperature_with_callback(line_num, device_id, timeout_ms, &mut |_| {})
    }

    /// Read temperature with a callback that's invoked during polling waits
    /// This allows other operations (like buzzer updates) to happen during sensor read timeouts
    pub fn read_temperature_with_callback<F>(
        &self,
        line_num: u8,
        device_id: &str,
        timeout_ms: u64,
        on_polling_wait: &mut F,
    ) -> io::Result<f32>
    where
        F: FnMut(u64) -> (),
    {
        let start = Instant::now();
        // Build path: /sys/bus/w1/devices/w1_bus_master{line+1}/{device_id}/temperature
        let temp_path = format!(
            "{}/w1_bus_master{}/{}/temperature",
            self.base_path,
            line_num + 1,
            device_id
        );

        // Attempt to read the temperature file
        loop {
            match fs::read_to_string(&temp_path) {
                Ok(content) => {
                    // Temperature file contains a single integer in millidegrees Celsius
                    // e.g., "25125" means 25.125°C
                    let temp_str = content.trim();
                    if let Ok(temp_millic) = temp_str.parse::<f32>() {
                        // Anything implausible is returned as an error so the
                        // caller's failure debouncing handles it, rather than
                        // reaching the alarm thresholds as a real reading.
                        return validate_temperature(temp_millic / 1000.0);
                    } else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("Failed to parse temperature value: {}", temp_str),
                        ));
                    }
                }
                Err(_e) => {
                    // Check timeout
                    if start.elapsed().as_millis() as u64 > timeout_ms {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("Sensor read timeout: {}", device_id),
                        ));
                    }

                    // Invoke callback before sleeping (allows buzzer/LED updates during polling)
                    let elapsed = start.elapsed().as_millis() as u64;
                    on_polling_wait(elapsed);

                    // Sleep briefly and retry
                    std::thread::sleep(std::time::Duration::from_millis(10));

                    // Continue if still within timeout
                    if start.elapsed().as_millis() as u64 > timeout_ms {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("Sensor read timeout: {}", device_id),
                        ));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sensor_status_connected() {
        let status = SensorStatus::Connected(37.5);
        assert_eq!(status, SensorStatus::Connected(37.5));
    }

    #[test]
    fn test_sensor_status_disconnected() {
        let status = SensorStatus::Disconnected;
        assert_eq!(status, SensorStatus::Disconnected);
    }

    #[test]
    fn plausible_temperatures_pass_through_unchanged() {
        for temp in [
            25.0_f32,
            -40.0,
            0.0,
            36.6,
            DS18B20_MIN_CELSIUS,
            DS18B20_MAX_CELSIUS,
        ] {
            assert_eq!(
                validate_temperature(temp).expect("should accept"),
                temp,
                "{}°C is within the DS18B20 range and must be accepted",
                temp
            );
        }
    }

    #[test]
    fn all_ones_scratchpad_is_rejected() {
        // 0x07FF * 0.0625 = 127.9375 — the value a line reports at power-on
        // before it is really answering. Would otherwise be a CRITICAL alarm.
        assert!(validate_temperature(127.9375).is_err());
        // Same reading as it arrives through the sysfs millidegree file (127937)
        assert!(validate_temperature(127.937).is_err());
    }

    #[test]
    fn power_on_default_is_rejected() {
        assert!(validate_temperature(DS18B20_POWER_ON_CELSIUS).is_err());
        assert!(validate_temperature(85.0).is_err());
    }

    #[test]
    fn values_outside_the_sensor_range_are_rejected() {
        assert!(validate_temperature(-55.1).is_err());
        assert!(validate_temperature(125.1).is_err());
        assert!(validate_temperature(-273.0).is_err());
    }

    #[test]
    fn sub_zero_readings_are_kept() {
        // The 0xFFFF artefact reads out as -0.0625°C, but so does a real probe
        // just below freezing — and this device monitors the cold chain, so the
        // range check must not reach up and swallow it.
        assert!(validate_temperature(-0.0625).is_ok());
        assert!(validate_temperature(-18.0).is_ok());
    }

    #[test]
    fn non_finite_values_are_rejected() {
        // "NaN" and "inf" both parse as f32, so they reach validation as numbers
        assert!(validate_temperature(f32::NAN).is_err());
        assert!(validate_temperature(f32::INFINITY).is_err());
        assert!(validate_temperature(f32::NEG_INFINITY).is_err());
    }

    #[test]
    fn rejections_are_invalid_data_so_callers_debounce_them() {
        // SensorMonitor treats any Err as a read failure and debounces it; the
        // kind matters only for logging, but it must not be a TimedOut lookalike.
        let err = validate_temperature(127.9375).expect_err("must reject");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
