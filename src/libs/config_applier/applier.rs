//! Configuration applier with atomic updates and rollback

use super::validation::ConfigValidator;
use serde_yaml::{Mapping, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Result of applying a configuration change
#[derive(Debug)]
pub struct ApplyResult {
    /// Whether the change was applied successfully
    pub success: bool,

    /// Path to the modified file
    pub file_path: String,

    /// Path to the backup file (if created)
    pub backup_path: Option<String>,

    /// Error message if failed
    pub error_message: Option<String>,

    /// Timestamp when applied
    pub applied_at: i64,
}

/// Configuration applier with atomic updates
pub struct ConfigApplier {
    /// Base directory for configuration files
    config_dir: PathBuf,

    /// Directory for backups
    backup_dir: PathBuf,
}

impl ConfigApplier {
    /// Create a new configuration applier
    pub fn new(config_dir: &Path) -> Result<Self, String> {
        let config_dir = config_dir.to_path_buf();
        let backup_dir = config_dir.join(".backups");

        // Create backup directory if it doesn't exist
        fs::create_dir_all(&backup_dir)
            .map_err(|e| format!("Failed to create backup directory: {}", e))?;

        Ok(Self {
            config_dir,
            backup_dir,
        })
    }

    /// Apply threshold changes to sensor configuration
    pub fn apply_threshold_change(
        &self,
        line: u8,
        critical_low: f32,
        alarm_low: f32,
        warning_low: f32,
        warning_high: f32,
        alarm_high: f32,
        critical_high: f32,
    ) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // 1. Validate thresholds
        if let Err(e) = ConfigValidator::validate_sensor_thresholds(
            line,
            critical_low,
            alarm_low,
            warning_low,
            warning_high,
            alarm_high,
            critical_high,
        ) {
            return ApplyResult {
                success: false,
                file_path: String::new(),
                backup_path: None,
                error_message: Some(format!("Validation failed: {}", e)),
                applied_at,
            };
        }

        // 2. Determine config file path
        let config_file = self.config_dir.join("fiber.sensors.config.yaml");
        if !config_file.exists() {
            return ApplyResult {
                success: false,
                file_path: config_file.to_string_lossy().to_string(),
                backup_path: None,
                error_message: Some("Config file not found".to_string()),
                applied_at,
            };
        }

        // 3. Read current configuration
        let content = match fs::read_to_string(&config_file) {
            Ok(c) => c,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().to_string(),
                    backup_path: None,
                    error_message: Some(format!("Failed to read config file: {}", e)),
                    applied_at,
                }
            }
        };

        // 4. Parse YAML
        let mut config: Value = match serde_yaml::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().to_string(),
                    backup_path: None,
                    error_message: Some(format!("Failed to parse YAML: {}", e)),
                    applied_at,
                }
            }
        };

        // 5. Create backup
        let backup_path = self.create_backup(&config_file, &content);
        let backup_path_str = backup_path.as_ref().map(|p| p.to_string_lossy().to_string());

        // 6. Modify configuration
        if let Err(e) = self.update_line_thresholds(
            &mut config,
            line,
            critical_low,
            alarm_low,
            warning_low,
            warning_high,
            alarm_high,
            critical_high,
        ) {
            return ApplyResult {
                success: false,
                file_path: config_file.to_string_lossy().to_string(),
                backup_path: backup_path_str,
                error_message: Some(format!("Failed to update thresholds: {}", e)),
                applied_at,
            };
        }

        // 7. Serialize to YAML
        let new_content = match serde_yaml::to_string(&config) {
            Ok(c) => c,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().to_string(),
                    backup_path: backup_path_str,
                    error_message: Some(format!("Failed to serialize YAML: {}", e)),
                    applied_at,
                }
            }
        };

        // 8. Write atomically (write to temp file, then rename)
        if let Err(e) = self.write_atomic(&config_file, &new_content) {
            // Attempt rollback
            if let Some(backup) = &backup_path {
                let _ = self.rollback(&config_file, backup);
            }

            return ApplyResult {
                success: false,
                file_path: config_file.to_string_lossy().to_string(),
                backup_path: backup_path_str,
                error_message: Some(format!("Failed to write config: {}", e)),
                applied_at,
            };
        }

        eprintln!(
            "[ConfigApplier] ✓ Thresholds updated for line {}: {}°C < {}°C < {}°C < {}°C < {}°C < {}°C",
            line, critical_low, alarm_low, warning_low, warning_high, alarm_high, critical_high
        );

        ApplyResult {
            success: true,
            file_path: config_file.to_string_lossy().to_string(),
            backup_path: backup_path_str,
            error_message: None,
            applied_at,
        }
    }

    /// Apply sensor name change
    pub fn apply_name_change(&self, line: u8, name: String) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // 1. Validate line number
        if line > 7 {
            return ApplyResult {
                success: false,
                file_path: String::new(),
                backup_path: None,
                error_message: Some(format!("Invalid line number: {} (must be 0-7)", line)),
                applied_at,
            };
        }

        // 2. Validate name length and characters
        if name.is_empty() || name.len() > 64 {
            return ApplyResult {
                success: false,
                file_path: String::new(),
                backup_path: None,
                error_message: Some("Name must be 1-64 characters".to_string()),
                applied_at,
            };
        }

        // 3. Determine config file path
        let config_file = self.config_dir.join("fiber.sensors.config.yaml");
        if !config_file.exists() {
            return ApplyResult {
                success: false,
                file_path: config_file.to_string_lossy().to_string(),
                backup_path: None,
                error_message: Some("Config file not found".to_string()),
                applied_at,
            };
        }

        // 4. Read current configuration
        let content = match fs::read_to_string(&config_file) {
            Ok(c) => c,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().to_string(),
                    backup_path: None,
                    error_message: Some(format!("Failed to read config file: {}", e)),
                    applied_at,
                }
            }
        };

        // 5. Parse YAML
        let mut config: Value = match serde_yaml::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().to_string(),
                    backup_path: None,
                    error_message: Some(format!("Failed to parse YAML: {}", e)),
                    applied_at,
                }
            }
        };

        // 6. Create backup
        let backup_path = self.create_backup(&config_file, &content);
        let backup_path_str = backup_path.as_ref().map(|p| p.to_string_lossy().to_string());

        // 7. Modify configuration
        if let Err(e) = self.update_line_name(&mut config, line, &name) {
            return ApplyResult {
                success: false,
                file_path: config_file.to_string_lossy().to_string(),
                backup_path: backup_path_str,
                error_message: Some(format!("Failed to update name: {}", e)),
                applied_at,
            };
        }

        // 8. Serialize to YAML
        let new_content = match serde_yaml::to_string(&config) {
            Ok(c) => c,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().to_string(),
                    backup_path: backup_path_str,
                    error_message: Some(format!("Failed to serialize YAML: {}", e)),
                    applied_at,
                }
            }
        };

        // 9. Write atomically (write to temp file, then rename)
        if let Err(e) = self.write_atomic(&config_file, &new_content) {
            // Attempt rollback
            if let Some(backup) = &backup_path {
                let _ = self.rollback(&config_file, backup);
            }

            return ApplyResult {
                success: false,
                file_path: config_file.to_string_lossy().to_string(),
                backup_path: backup_path_str,
                error_message: Some(format!("Failed to write config: {}", e)),
                applied_at,
            };
        }

        eprintln!(
            "[ConfigApplier] ✓ Sensor name updated for line {}: \"{}\"",
            line, name
        );

        ApplyResult {
            success: true,
            file_path: config_file.to_string_lossy().to_string(),
            backup_path: backup_path_str,
            error_message: None,
            applied_at,
        }
    }

    /// Apply sensor interval changes to main configuration
    pub fn apply_interval_change(
        &self,
        sample_interval_ms: u64,
        aggregation_interval_ms: u64,
        report_interval_ms: u64,
    ) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // 1. Validate intervals
        if let Err(e) = ConfigValidator::validate_intervals(
            sample_interval_ms,
            aggregation_interval_ms,
            report_interval_ms,
        ) {
            return ApplyResult {
                success: false,
                file_path: String::new(),
                backup_path: None,
                error_message: Some(e),
                applied_at,
            };
        }

        // 2. Determine config file path (main config, not sensors config)
        let config_file = self.config_dir.join("fiber.config.yaml");
        if !config_file.exists() {
            return ApplyResult {
                success: false,
                file_path: config_file.to_string_lossy().to_string(),
                backup_path: None,
                error_message: Some("Main config file not found".to_string()),
                applied_at,
            };
        }

        // 3. Read current configuration
        let content = match fs::read_to_string(&config_file) {
            Ok(c) => c,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().to_string(),
                    backup_path: None,
                    error_message: Some(format!("Failed to read config file: {}", e)),
                    applied_at,
                }
            }
        };

        // 4. Parse YAML
        let mut config: Value = match serde_yaml::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().to_string(),
                    backup_path: None,
                    error_message: Some(format!("Failed to parse YAML: {}", e)),
                    applied_at,
                }
            }
        };

        // 5. Create backup
        let backup_path = self.create_backup(&config_file, &content);
        let backup_path_str = backup_path.as_ref().map(|p| p.to_string_lossy().to_string());

        // 6. Update intervals in config
        if let Err(e) = self.update_sensor_intervals(
            &mut config,
            sample_interval_ms,
            aggregation_interval_ms,
            report_interval_ms,
        ) {
            return ApplyResult {
                success: false,
                file_path: config_file.to_string_lossy().to_string(),
                backup_path: backup_path_str,
                error_message: Some(e),
                applied_at,
            };
        }

        // 7. Serialize to YAML
        let new_content = match serde_yaml::to_string(&config) {
            Ok(c) => c,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().to_string(),
                    backup_path: backup_path_str,
                    error_message: Some(format!("Failed to serialize YAML: {}", e)),
                    applied_at,
                }
            }
        };

        // 8. Write atomically
        if let Err(e) = self.write_atomic(&config_file, &new_content) {
            // Attempt rollback
            if let Some(backup) = &backup_path {
                let _ = self.rollback(&config_file, backup);
            }

            return ApplyResult {
                success: false,
                file_path: config_file.to_string_lossy().to_string(),
                backup_path: backup_path_str,
                error_message: Some(format!("Failed to write config: {}", e)),
                applied_at,
            };
        }

        eprintln!(
            "[ConfigApplier] ✓ Sensor intervals updated: sample={}ms, aggregation={}ms, report={}ms",
            sample_interval_ms, aggregation_interval_ms, report_interval_ms
        );

        ApplyResult {
            success: true,
            file_path: config_file.to_string_lossy().to_string(),
            backup_path: backup_path_str,
            error_message: None,
            applied_at,
        }
    }

    // --- Private helper methods ---

    /// Update thresholds for a specific sensor line in the YAML structure
    fn update_line_thresholds(
        &self,
        config: &mut Value,
        line: u8,
        critical_low: f32,
        alarm_low: f32,
        warning_low: f32,
        warning_high: f32,
        alarm_high: f32,
        critical_high: f32,
    ) -> Result<(), String> {
        // Get lines array
        let lines = config
            .get_mut("lines")
            .and_then(|v| v.as_sequence_mut())
            .ok_or_else(|| "Missing 'lines' array in config".to_string())?;

        // Find the line entry
        let line_entry = lines
            .iter_mut()
            .find(|entry| {
                entry
                    .get("line")
                    .and_then(|v| v.as_u64())
                    .map(|l| l == line as u64)
                    .unwrap_or(false)
            })
            .ok_or_else(|| format!("Line {} not found in config", line))?;

        // Ensure it's a mapping
        let line_map = line_entry
            .as_mapping_mut()
            .ok_or_else(|| "Line entry is not a mapping".to_string())?;

        // Insert thresholds directly on line config (flat fields, not nested)
        line_map.insert(
            Value::String("critical_low_celsius".to_string()),
            Value::Number(serde_yaml::Number::from(critical_low as f64)),
        );
        line_map.insert(
            Value::String("low_alarm_celsius".to_string()),
            Value::Number(serde_yaml::Number::from(alarm_low as f64)),
        );
        line_map.insert(
            Value::String("warning_low_celsius".to_string()),
            Value::Number(serde_yaml::Number::from(warning_low as f64)),
        );
        line_map.insert(
            Value::String("warning_high_celsius".to_string()),
            Value::Number(serde_yaml::Number::from(warning_high as f64)),
        );
        line_map.insert(
            Value::String("high_alarm_celsius".to_string()),
            Value::Number(serde_yaml::Number::from(alarm_high as f64)),
        );
        line_map.insert(
            Value::String("critical_high_celsius".to_string()),
            Value::Number(serde_yaml::Number::from(critical_high as f64)),
        );

        Ok(())
    }

    /// Update name for a specific sensor line in the YAML structure
    fn update_line_name(&self, config: &mut Value, line: u8, name: &str) -> Result<(), String> {
        // Get lines array
        let lines = config
            .get_mut("lines")
            .and_then(|v| v.as_sequence_mut())
            .ok_or_else(|| "Missing 'lines' array in config".to_string())?;

        // Find the line entry
        let line_entry = lines
            .iter_mut()
            .find(|entry| {
                entry
                    .get("line")
                    .and_then(|v| v.as_u64())
                    .map(|l| l == line as u64)
                    .unwrap_or(false)
            })
            .ok_or_else(|| format!("Line {} not found in config", line))?;

        // Ensure it's a mapping
        let line_map = line_entry
            .as_mapping_mut()
            .ok_or_else(|| "Line entry is not a mapping".to_string())?;

        // Update the name field
        line_map.insert(
            Value::String("name".to_string()),
            Value::String(name.to_string()),
        );

        Ok(())
    }

    /// Update sensor intervals in the main YAML config structure
    fn update_sensor_intervals(
        &self,
        config: &mut Value,
        sample_interval_ms: u64,
        aggregation_interval_ms: u64,
        report_interval_ms: u64,
    ) -> Result<(), String> {
        // Get or create 'sensors' section
        let sensors = config
            .get_mut("sensors")
            .and_then(|v| v.as_mapping_mut())
            .ok_or_else(|| "Missing 'sensors' section in config".to_string())?;

        // Update interval fields
        sensors.insert(
            Value::String("sample_interval_ms".to_string()),
            Value::Number(serde_yaml::Number::from(sample_interval_ms)),
        );
        sensors.insert(
            Value::String("aggregation_interval_ms".to_string()),
            Value::Number(serde_yaml::Number::from(aggregation_interval_ms)),
        );
        sensors.insert(
            Value::String("report_interval_ms".to_string()),
            Value::Number(serde_yaml::Number::from(report_interval_ms)),
        );

        Ok(())
    }

    /// Create a timestamped backup of the config file
    fn create_backup(&self, config_file: &Path, content: &str) -> Option<PathBuf> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let filename = config_file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config.yaml");

        let backup_file = self.backup_dir.join(format!("{}.{}.bak", filename, timestamp));

        match fs::write(&backup_file, content) {
            Ok(_) => {
                eprintln!(
                    "[ConfigApplier] Backup created: {}",
                    backup_file.to_string_lossy()
                );
                Some(backup_file)
            }
            Err(e) => {
                eprintln!("[ConfigApplier] ⚠ Failed to create backup: {}", e);
                None
            }
        }
    }

    /// Write file atomically using temp file + rename
    fn write_atomic(&self, file_path: &Path, content: &str) -> Result<(), String> {
        let temp_file = file_path.with_extension("tmp");

        // Write to temp file
        let mut file = fs::File::create(&temp_file)
            .map_err(|e| format!("Failed to create temp file: {}", e))?;

        file.write_all(content.as_bytes())
            .map_err(|e| format!("Failed to write temp file: {}", e))?;

        file.sync_all()
            .map_err(|e| format!("Failed to sync temp file: {}", e))?;

        drop(file);

        // Atomic rename
        fs::rename(&temp_file, file_path)
            .map_err(|e| format!("Failed to rename temp file: {}", e))?;

        Ok(())
    }

    /// Rollback configuration from backup
    fn rollback(&self, config_file: &Path, backup_file: &Path) -> Result<(), String> {
        eprintln!(
            "[ConfigApplier] Rolling back from backup: {}",
            backup_file.to_string_lossy()
        );

        fs::copy(backup_file, config_file)
            .map_err(|e| format!("Failed to rollback: {}", e))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_result_structure() {
        let result = ApplyResult {
            success: true,
            file_path: "/test/config.yaml".to_string(),
            backup_path: Some("/test/.backups/config.yaml.123456.bak".to_string()),
            error_message: None,
            applied_at: 1702483200,
        };

        assert!(result.success);
        assert!(result.backup_path.is_some());
        assert!(result.error_message.is_none());
    }
}
