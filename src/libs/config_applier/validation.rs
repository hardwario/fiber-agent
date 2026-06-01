//! Configuration validation for safe updates

/// Validate temperature threshold ordering
pub struct ConfigValidator;

impl ConfigValidator {
    /// Validate sensor threshold ordering (4-level system)
    ///
    /// Ensures: critical_low < warning_low < warning_high < critical_high
    pub fn validate_threshold_ordering(
        critical_low: f32,
        warning_low: f32,
        warning_high: f32,
        critical_high: f32,
    ) -> Result<(), String> {
        if critical_low >= warning_low {
            return Err(format!(
                "critical_low ({}) must be less than warning_low ({})",
                critical_low, warning_low
            ));
        }

        if warning_low >= warning_high {
            return Err(format!(
                "warning_low ({}) must be less than warning_high ({})",
                warning_low, warning_high
            ));
        }

        if warning_high >= critical_high {
            return Err(format!(
                "warning_high ({}) must be less than critical_high ({})",
                warning_high, critical_high
            ));
        }

        Ok(())
    }

    /// Validate temperature is in reasonable range for medical device
    pub fn validate_temperature_range(temp: f32, field_name: &str) -> Result<(), String> {
        // Medical devices typically operate in range -50°C to 100°C
        if !(-50.0..=100.0).contains(&temp) {
            return Err(format!(
                "{} ({}) is outside valid range (-50 to 100°C)",
                field_name, temp
            ));
        }

        Ok(())
    }

    /// Validate all thresholds for a sensor line
    pub fn validate_sensor_thresholds(
        line: u8,
        critical_low: f32,
        warning_low: f32,
        warning_high: f32,
        critical_high: f32,
    ) -> Result<(), String> {
        // Validate line number
        if line > 7 {
            return Err(format!("Invalid line number: {} (must be 0-7)", line));
        }

        // Validate individual temperatures
        Self::validate_temperature_range(critical_low, "critical_low")?;
        Self::validate_temperature_range(warning_low, "warning_low")?;
        Self::validate_temperature_range(warning_high, "warning_high")?;
        Self::validate_temperature_range(critical_high, "critical_high")?;

        // Validate ordering
        Self::validate_threshold_ordering(
            critical_low,
            warning_low,
            warning_high,
            critical_high,
        )?;

        Ok(())
    }

    /// Validate sensor interval settings
    ///
    /// Ensures:
    /// - sample_interval_ms >= 100ms (minimum sampling rate)
    /// - report_interval_ms <= 24 hours
    /// - sample_interval_ms <= aggregation_interval_ms <= report_interval_ms
    pub fn validate_intervals(
        sample_interval_ms: u64,
        aggregation_interval_ms: u64,
        report_interval_ms: u64,
    ) -> Result<(), String> {
        // Minimum sample interval of 100ms
        if sample_interval_ms < 100 {
            return Err(format!(
                "sample_interval_ms ({}) must be >= 100ms",
                sample_interval_ms
            ));
        }

        // Maximum report interval of 24 hours
        const MAX_INTERVAL_MS: u64 = 24 * 60 * 60 * 1000; // 86,400,000ms
        if report_interval_ms > MAX_INTERVAL_MS {
            return Err(format!(
                "report_interval_ms ({}) must be <= 24 hours ({}ms)",
                report_interval_ms, MAX_INTERVAL_MS
            ));
        }

        // Logical ordering: sample <= aggregation <= report
        if sample_interval_ms > aggregation_interval_ms {
            return Err(format!(
                "sample_interval_ms ({}) must be <= aggregation_interval_ms ({})",
                sample_interval_ms, aggregation_interval_ms
            ));
        }

        if aggregation_interval_ms > report_interval_ms {
            return Err(format!(
                "aggregation_interval_ms ({}) must be <= report_interval_ms ({})",
                aggregation_interval_ms, report_interval_ms
            ));
        }

        Ok(())
    }
}

/// Maximum length of a user-set device label in bytes. Display + MQTT can
/// in principle carry more, but 64 was the historical limit on the MQTT
/// path and we keep that contract.
pub const MAX_DEVICE_LABEL_LEN: usize = 64;

/// Characters not allowed inside a device label:
/// - `/`, `+`, `#`: MQTT topic wildcards / separators. Embedding any of
///   these in `device_label` would either split the topic or get
///   interpreted as a wildcard subscription.
/// - Everything outside `0x20..=0x7E`: anything that's not printable
///   ASCII. The LCD renderer only ships an ASCII font and Unicode
///   characters render as boxes; control bytes / null / newline could
///   also corrupt log lines and downstream parsers.
///
/// Note: space (`0x20`) is allowed so users can write "Ward 3 Freezer".
pub fn validate_device_label(label: &str) -> Result<(), String> {
    if label.is_empty() {
        return Err("Device label cannot be empty".to_string());
    }
    if label.len() > MAX_DEVICE_LABEL_LEN {
        return Err(format!(
            "Device label must be at most {} characters (got {})",
            MAX_DEVICE_LABEL_LEN,
            label.len(),
        ));
    }

    for (idx, b) in label.bytes().enumerate() {
        match b {
            // MQTT-breaking characters within the otherwise-allowed range.
            b'/' | b'+' | b'#' => {
                return Err(format!(
                    "Device label contains MQTT-reserved character {:?} at byte {}",
                    b as char, idx,
                ));
            }
            // Printable ASCII (space through tilde).
            0x20..=0x7E => continue,
            // Anything else: control byte, null, or non-ASCII (Unicode).
            other => {
                return Err(format!(
                    "Device label contains non-printable-ASCII byte 0x{:02X} at byte {}",
                    other, idx,
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_threshold_ordering() {
        let result = ConfigValidator::validate_threshold_ordering(
            32.0, 35.0, 39.0, 42.0,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_invalid_threshold_ordering() {
        // critical_low >= warning_low
        let result = ConfigValidator::validate_threshold_ordering(
            36.0, 35.0, 39.0, 42.0,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("critical_low"));
    }

    #[test]
    fn test_temperature_range_validation() {
        assert!(ConfigValidator::validate_temperature_range(36.5, "test").is_ok());
        assert!(ConfigValidator::validate_temperature_range(-50.0, "test").is_ok());
        assert!(ConfigValidator::validate_temperature_range(100.0, "test").is_ok());
        assert!(ConfigValidator::validate_temperature_range(-51.0, "test").is_err());
        assert!(ConfigValidator::validate_temperature_range(101.0, "test").is_err());
    }

    #[test]
    fn test_sensor_thresholds_validation() {
        // Valid thresholds
        assert!(ConfigValidator::validate_sensor_thresholds(
            0, 32.0, 35.0, 39.0, 42.0
        )
        .is_ok());

        // Invalid line number
        assert!(ConfigValidator::validate_sensor_thresholds(
            99, 32.0, 35.0, 39.0, 42.0
        )
        .is_err());

        // Temperature out of range
        assert!(ConfigValidator::validate_sensor_thresholds(
            0, 32.0, 35.0, 39.0, 150.0
        )
        .is_err());

        // Invalid ordering
        assert!(ConfigValidator::validate_sensor_thresholds(
            0, 32.0, 38.0, 36.0, 42.0
        )
        .is_err());
    }

    // ---- device label validation ------------------------------------------------

    #[test]
    fn device_label_accepts_simple_ascii() {
        assert!(validate_device_label("FIBER-001").is_ok());
        assert!(validate_device_label("Ward 3 Freezer").is_ok());
        assert!(validate_device_label("a").is_ok(), "single char ok");
    }

    #[test]
    fn device_label_rejects_empty() {
        let err = validate_device_label("").unwrap_err();
        assert!(err.to_lowercase().contains("empty"), "got: {err}");
    }

    #[test]
    fn device_label_rejects_too_long() {
        let s = "A".repeat(MAX_DEVICE_LABEL_LEN + 1);
        let err = validate_device_label(&s).unwrap_err();
        assert!(err.contains(&MAX_DEVICE_LABEL_LEN.to_string()), "got: {err}");
    }

    #[test]
    fn device_label_accepts_exact_max_length() {
        let s = "A".repeat(MAX_DEVICE_LABEL_LEN);
        assert!(validate_device_label(&s).is_ok());
    }

    #[test]
    fn device_label_rejects_mqtt_breakers() {
        for bad in ["a/b", "a+b", "a#b", "/leading", "trailing#", "with + sign"] {
            let err = validate_device_label(bad).unwrap_err();
            assert!(
                err.to_lowercase().contains("mqtt"),
                "expected MQTT mention for {:?}, got: {}",
                bad,
                err,
            );
        }
    }

    #[test]
    fn device_label_rejects_unicode() {
        // Includes accented (Portuguese), emoji, and CJK to be thorough.
        for bad in ["Câmara", "Ward 🥶", "病房", "naïve"] {
            assert!(
                validate_device_label(bad).is_err(),
                "should reject non-ASCII: {:?}",
                bad,
            );
        }
    }

    #[test]
    fn device_label_rejects_control_and_null() {
        for bad in ["a\0b", "a\nb", "a\tb", "\x7Fdel"] {
            assert!(
                validate_device_label(bad).is_err(),
                "should reject control byte in {:?}",
                bad,
            );
        }
    }

    #[test]
    fn device_label_accepts_all_other_punctuation() {
        // Verify common safe symbols round-trip — these are well within
        // printable ASCII and not MQTT-reserved.
        for ok in ["Lab-A_3", "v1.0", "@home", "(spare)", "PASS!", "100%"] {
            assert!(validate_device_label(ok).is_ok(), "should accept {:?}", ok);
        }
    }
}
