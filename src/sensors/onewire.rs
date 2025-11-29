// src/sensors/onewire.rs
use crate::acquisition::SensorBackend;
use crate::model::{ReadingQuality, SensorId};
use chrono::{DateTime, Utc};
use std::fs;
use std::path::{Path, PathBuf};

/// One-Wire temperature sensor backend using Linux sysfs.
///
/// Expects a directory layout like:
///   /sys/bus/w1/devices/<rom_code>/w1_slave
///
/// It:
/// - reads the `w1_slave` file
/// - verifies CRC ("YES")
/// - parses `t=XXXXX` (milli-degrees Celsius)
/// - applies a calibration offset.
pub struct OneWireSysfsBackend {
    id: SensorId,
    w1_slave_path: PathBuf,
    calibration_offset: f32,
}

impl OneWireSysfsBackend {
    /// Create a new backend.
    ///
    /// `device_root` usually is `/sys/bus/w1/devices`
    /// `rom_code` is the 1-Wire ID, e.g. "28-00000abcdef0"
    pub fn new(
        id: SensorId,
        device_root: impl AsRef<Path>,
        rom_code: &str,
        calibration_offset: f32,
    ) -> Self {
        let w1_slave_path = device_root
            .as_ref()
            .join(rom_code)
            .join("w1_slave");

        Self {
            id,
            w1_slave_path,
            calibration_offset,
        }
    }

    /// Parse the contents of a w1_slave file.
    ///
    /// Example:
    ///  8e 01 4b 46 7f ff 0c 10 5e : crc=5e YES
    ///  8e 01 4b 46 7f ff 0c 10 5e t=24625
    fn parse_w1_slave(content: &str, calibration_offset: f32) -> (f32, ReadingQuality) {
        let mut lines = content.lines();

        let first = match lines.next() {
            Some(l) => l,
            None => return (0.0, ReadingQuality::Other),
        };

        // CRC check: line should contain "YES"
        if !first.contains("YES") {
            return (0.0, ReadingQuality::CrcError);
        }

        // Second line: find "t="
        let second = match lines.next() {
            Some(l) => l,
            None => return (0.0, ReadingQuality::Other),
        };

        let pos = match second.rfind("t=") {
            Some(p) => p,
            None => return (0.0, ReadingQuality::Other),
        };

        let milli_str = &second[(pos + 2)..].trim();
        let milli: i32 = match milli_str.parse() {
            Ok(v) => v,
            Err(_) => return (0.0, ReadingQuality::Other),
        };

        let mut temp_c = milli as f32 / 1000.0;
        temp_c += calibration_offset;

        (temp_c, ReadingQuality::Ok)
    }
}

impl SensorBackend for OneWireSysfsBackend {
    fn sensor_id(&self) -> SensorId {
        self.id
    }

    fn read(&mut self, _now: DateTime<Utc>) -> (f32, ReadingQuality) {
        // If the file doesn't exist or can't be read, treat sensor as disconnected.
        let content = match fs::read_to_string(&self.w1_slave_path) {
            Ok(c) => c,
            Err(_) => return (0.0, ReadingQuality::Disconnected),
        };

        Self::parse_w1_slave(&content, self.calibration_offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ReadingQuality;
    use chrono::{TimeZone, Utc};
    use tempfile::tempdir;

    #[test]
    fn parse_ok_content() {
        let content = "\
8e 01 4b 46 7f ff 0c 10 5e : crc=5e YES
8e 01 4b 46 7f ff 0c 10 5e t=24625
";

        let (temp, q) = OneWireSysfsBackend::parse_w1_slave(content, 0.0);
        assert_eq!(q, ReadingQuality::Ok);
        assert!((temp - 24.625).abs() < 0.001);

        // with calibration offset
        let (temp2, q2) = OneWireSysfsBackend::parse_w1_slave(content, 0.5);
        assert_eq!(q2, ReadingQuality::Ok);
        assert!((temp2 - 25.125).abs() < 0.001);
    }

    #[test]
    fn parse_crc_error() {
        let content = "\
8e 01 4b 46 7f ff 0c 10 5e : crc=5e NO
8e 01 4b 46 7f ff 0c 10 5e t=24625
";

        let (_temp, q) = OneWireSysfsBackend::parse_w1_slave(content, 0.0);
        assert_eq!(q, ReadingQuality::CrcError);
    }

    #[test]
    fn read_from_tempfile_structure() {
        let dir = tempdir().unwrap();
        let dev_root = dir.path();

        let rom = "28-000000000000";

        // build backend
        let mut backend = OneWireSysfsBackend::new(
            SensorId(1),
            dev_root,
            rom,
            0.0,
        );

        // create directory and w1_slave file
        let sensor_dir = dev_root.join(rom);
        std::fs::create_dir_all(&sensor_dir).unwrap();
        let w1_slave_path = sensor_dir.join("w1_slave");

        let content = "\
aa bb cc dd ee ff gg hh : crc=12 YES
aa bb cc dd ee ff gg hh t=10000
";
        std::fs::write(&w1_slave_path, content).unwrap();

        let (temp, q) = backend.read(Utc.timestamp_millis_opt(0).unwrap());
        assert_eq!(q, ReadingQuality::Ok);
        assert!((temp - 10.0).abs() < 0.001);
    }
}
