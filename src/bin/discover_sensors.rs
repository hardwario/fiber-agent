/// Discover DS18B20 sensors on DS2482 I2C-to-1Wire bridges
///
/// This standalone tool scans all 8 IO pins (channels) of the DS2482
/// and reports which ones have DS18B20 sensors connected, along with
/// their current temperatures.
///
/// Usage (from fiber directory):
///   cargo run --bin discover_sensors -- /dev/i2c-10
///   cargo run --bin discover_sensors  # defaults to /dev/i2c-10

use i2cdev::core::*;
use i2cdev::linux::LinuxI2CDevice;
use std::env;
use std::thread;
use std::time::Duration;
use std::io;

const DS2482_ADDR: u16 = 0x18;

// DS2482 Commands
const CMD_DEVICE_RESET: u8 = 0xF0;
const CMD_CHANNEL_SELECT: u8 = 0xC3;
const CMD_ONEWIRE_RESET: u8 = 0xB4;
const CMD_ONEWIRE_WRITE_BYTE: u8 = 0xA5;
const CMD_ONEWIRE_READ_BYTE: u8 = 0x96;

// 1-Wire Commands
const OW_SKIP_ROM: u8 = 0xCC;
const OW_READ_SCRATCHPAD: u8 = 0xBE;

fn main() {
    let i2c_path = env::args()
        .nth(1)
        .unwrap_or_else(|| "/dev/i2c-10".to_string());

    println!("[discovery] Starting DS18B20 sensor enumeration on {}", i2c_path);
    println!("[discovery] Scanning 8 channels (IO0-IO7)...\n");

    match scan_all_channels(&i2c_path) {
        Ok(found_count) => {
            println!("\n[discovery] ✓ Enumeration complete. Found {} device(s).", found_count);
            if found_count > 0 {
                println!("[discovery] Next steps:");
                println!("[discovery]   1. Update fiber.yaml with discovered io_pin values");
                println!("[discovery]   2. Use 'fiber.yaml ROM code discovery' guide for exact ROM codes");
                println!("[discovery]   3. Or use SKIP_ROM with only one sensor per channel");
            }
        }
        Err(e) => {
            eprintln!("[discovery] ✗ Error during enumeration: {}", e);
            std::process::exit(1);
        }
    }
}

fn scan_all_channels(i2c_path: &str) -> io::Result<usize> {
    let mut device = LinuxI2CDevice::new(i2c_path, DS2482_ADDR)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to open i2c device: {}", e)))?;

    // Reset DS2482
    device.write(&[CMD_DEVICE_RESET])
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to reset DS2482: {}", e)))?;
    thread::sleep(Duration::from_millis(10));

    let mut found_count = 0;

    for channel in 0..8 {
        print!("[discovery] Channel {}: ", channel);

        // Select channel
        if let Err(e) = device.write(&[CMD_CHANNEL_SELECT, channel]) {
            println!("✗ Failed to select ({})", e);
            continue;
        }
        thread::sleep(Duration::from_millis(5));

        // Try 1-wire reset
        match device.write(&[CMD_ONEWIRE_RESET]) {
            Ok(_) => {
                thread::sleep(Duration::from_millis(10));

                // Read status to check for presence pulse
                let mut status = [0u8; 1];
                if let Ok(_) = device.read(&mut status) {
                    let has_device = (status[0] & 0x80) == 0; // 1WB bit clear = device present

                    if has_device {
                        println!("✓ Device detected");

                        // Try to read temperature
                        match read_channel_temperature(&mut device) {
                            Ok(temp) => {
                                println!("  └─ Temperature: {:.2}°C", temp);
                                println!("  └─ io_pin: {} (for fiber.yaml)", channel);
                                found_count += 1;
                            }
                            Err(e) => {
                                println!("  └─ Warning: Could not read temp ({})", e);
                                println!("  └─ io_pin: {} (for fiber.yaml)", channel);
                                found_count += 1;
                            }
                        }
                    } else {
                        println!("(no device)");
                    }
                }
            }
            Err(e) => {
                println!("✗ Reset failed: {}", e);
            }
        }

        thread::sleep(Duration::from_millis(100));
    }

    Ok(found_count)
}

fn read_channel_temperature(device: &mut LinuxI2CDevice) -> io::Result<f32> {
    // Send SKIP_ROM
    device.write(&[CMD_ONEWIRE_WRITE_BYTE, OW_SKIP_ROM])?;
    thread::sleep(Duration::from_millis(2));

    // Send READ_SCRATCHPAD
    device.write(&[CMD_ONEWIRE_WRITE_BYTE, OW_READ_SCRATCHPAD])?;
    thread::sleep(Duration::from_millis(2));

    // Read temperature bytes
    device.write(&[CMD_ONEWIRE_READ_BYTE])?;
    thread::sleep(Duration::from_millis(2));
    let mut temp_low = [0u8; 1];
    device.read(&mut temp_low)?;

    device.write(&[CMD_ONEWIRE_READ_BYTE])?;
    thread::sleep(Duration::from_millis(2));
    let mut temp_high = [0u8; 1];
    device.read(&mut temp_high)?;

    // Parse temperature (12-bit, 0.0625°C resolution)
    let raw = ((temp_high[0] as i16) << 8) | (temp_low[0] as i16);
    let temperature = (raw as f32) * 0.0625;

    Ok(temperature)
}
