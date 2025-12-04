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
    pub fn read_temperature(&self, line_num: u8, device_id: &str, timeout_ms: u64) -> io::Result<f32> {
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
                        let temp_c = temp_millic / 1000.0;
                        return Ok(temp_c);
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
}
