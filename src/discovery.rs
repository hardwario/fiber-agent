/// Sensor Discovery Module
///
/// Auto-discovers DS18B20 sensors by scanning the Linux sysfs 1-Wire bus.
/// Reads from /sys/bus/w1/devices/ to find connected sensors and their temperatures.

use std::fs;
use std::path::Path;

/// Discovered sensor information with channel tracking
#[derive(Debug, Clone)]
pub struct SensorOnChannel {
    pub io_pin: u8,           // 0-7 corresponding to w1_bus_master 1-8
    pub rom: String,          // e.g., "28-000000b372c1"
    pub temperature_c: f32,   // in Celsius
}

/// Read temperature from a DS18B20 sensor via sysfs
///
/// Reads from /sys/bus/w1/devices/{rom}/temperature
/// Temperature is returned in millidegrees Celsius, converted to Celsius
pub fn read_temperature_from_sysfs(rom: &str) -> Result<f32, Box<dyn std::error::Error>> {
    let temp_path = format!("/sys/bus/w1/devices/{}/temperature", rom);
    let temp_file = Path::new(&temp_path);

    if !temp_file.exists() {
        return Err(format!("Sensor file not found: {}", temp_path).into());
    }

    let temp_str = fs::read_to_string(temp_file)?
        .trim()
        .to_string();

    let temp_raw: i32 = temp_str.parse()
        .map_err(|_| format!("Failed to parse temperature: {}", temp_str))?;

    // DS18B20 returns temperature in millidegrees Celsius
    // Convert to Celsius
    let temperature_c = (temp_raw as f32) / 1000.0;

    Ok(temperature_c)
}

/// Scan for sensors on a specific w1_bus_master channel
///
/// Returns a list of sensors found on that channel with io_pin mapping
pub fn scan_channel(channel: u8) -> Result<Vec<SensorOnChannel>, Box<dyn std::error::Error>> {
    if channel < 1 || channel > 8 {
        return Err("Channel must be 1-8".into());
    }

    let io_pin = channel - 1; // w1_bus_master1 = io_pin 0
    let mut sensors = Vec::new();

    let channel_path = format!("/sys/bus/w1/devices/w1_bus_master{}", channel);
    let channel_dir = Path::new(&channel_path);

    if !channel_dir.exists() {
        eprintln!("[discovery] Channel path does not exist: {}", channel_path);
        return Ok(sensors);
    }

    eprintln!("[discovery] Scanning channel {} at {}", channel, channel_path);

    if let Ok(entries) = fs::read_dir(channel_dir) {
        let mut found_sensor = false;
        for entry in entries {
            if let Ok(entry) = entry {
                let filename = entry.file_name();
                let rom_str = filename.to_string_lossy();

                if rom_str.starts_with("28-") {
                    found_sensor = true;
                    let rom = rom_str.to_string();
                    eprintln!("[discovery] Found sensor {} on channel {}", rom, channel);

                    // Quick retry logic for transient read failures (sensor initializing)
                    // No blocking sleep - discovery runs every 3 seconds, so transient failures will retry naturally
                    let max_attempts = 2;
                    let mut read_result = None;

                    for attempt in 0..max_attempts {
                        match read_temperature_from_sysfs(&rom) {
                            Ok(temp) => {
                                if attempt > 0 {
                                    eprintln!("[discovery] ✓ Read temperature for {} (attempt {}): {:.2}°C", rom, attempt + 1, temp);
                                } else {
                                    eprintln!("[discovery] ✓ Read temperature for {}: {:.2}°C", rom, temp);
                                }
                                read_result = Some(temp);
                                break;
                            }
                            Err(e) => {
                                if attempt < max_attempts - 1 {
                                    eprintln!("[discovery] ⚠ Read failed (attempt {}): {}, retrying immediately...", attempt + 1, e);
                                    // Quick retry without blocking sleep (no std::thread::sleep in main loop!)
                                } else {
                                    eprintln!("[discovery] ✗ Read failed after {} attempts: {}", max_attempts, e);
                                }
                            }
                        }
                    }

                    if let Some(temp) = read_result {
                        sensors.push(SensorOnChannel {
                            io_pin,
                            rom,
                            temperature_c: temp,
                        });
                    }
                }
            }
        }
        if !found_sensor {
            eprintln!("[discovery] No sensors found on channel {}", channel);
        }
    }

    Ok(sensors)
}

/// Scan all 8 channels and return sensors with their io_pin mappings
pub fn scan_all_channels() -> Result<Vec<SensorOnChannel>, Box<dyn std::error::Error>> {
    eprintln!("[discovery] ========== Scanning all channels ==========");
    let mut all_sensors = Vec::new();

    for channel in 1..=8 {
        if let Ok(sensors) = scan_channel(channel) {
            all_sensors.extend(sensors);
        }
    }

    eprintln!("[discovery] Total sensors found: {}", all_sensors.len());
    for sensor in &all_sensors {
        eprintln!("[discovery]   - Pin {} (master{}): {} at {:.2}°C",
            sensor.io_pin, sensor.io_pin + 1, sensor.rom, sensor.temperature_c);
    }
    eprintln!("[discovery] =========================================");

    Ok(all_sensors)
}

/// Trigger 1-wire bus search to detect newly connected sensors
///
/// Writes "1" to /sys/bus/w1/devices/w1_bus_master{n}/w1_master_search
/// for all 8 possible bus masters (IO0-IO7)
pub fn trigger_w1_search() -> Result<usize, Box<dyn std::error::Error>> {
    eprintln!("[discovery] Triggering 1-wire bus search on all channels...");
    let mut search_count = 0;

    for channel in 1..=8 {
        let search_path = format!(
            "/sys/bus/w1/devices/w1_bus_master{}/w1_master_search",
            channel
        );
        let search_file = Path::new(&search_path);

        if search_file.exists() {
            match fs::write(search_file, "1") {
                Ok(_) => {
                    search_count += 1;
                    eprintln!("[discovery] ✓ Triggered search on w1_bus_master{}", channel);
                }
                Err(e) => {
                    eprintln!(
                        "[discovery] ✗ Failed to trigger search on w1_bus_master{}: {}",
                        channel, e
                    );
                }
            }
        } else {
            eprintln!("[discovery] ⚠ w1_bus_master{} not found", channel);
        }
    }

    eprintln!("[discovery] Search triggered on {} channels", search_count);
    Ok(search_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    #[ignore] // TODO: detect_changes function was removed as unused
    fn test_detect_changes() {
        /*
        let mut prev = HashMap::new();
        prev.insert("28-aaa".to_string(), 25.0);
        prev.insert("28-bbb".to_string(), 26.0);

        let mut curr = HashMap::new();
        curr.insert("28-bbb".to_string(), 26.5);
        curr.insert("28-ccc".to_string(), 27.0);

        let (connected, disconnected) = detect_changes(&prev, &curr);

        assert_eq!(connected, vec!["28-ccc"]);
        assert_eq!(disconnected, vec!["28-aaa"]);
        */
    }
}
