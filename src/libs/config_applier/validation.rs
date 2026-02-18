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
}
