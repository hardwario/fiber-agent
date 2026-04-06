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

        // 1. Validate thresholds (4-level system)
        if let Err(e) = ConfigValidator::validate_sensor_thresholds(
            line,
            critical_low,
            warning_low,
            warning_high,
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
            "[ConfigApplier] ✓ Thresholds updated for line {}: {}°C < {}°C < {}°C < {}°C",
            line, critical_low, warning_low, warning_high, critical_high
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

    /// Apply sensor location change
    pub fn apply_location_change(&self, line: u8, location: String) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        if line > 7 {
            return ApplyResult {
                success: false,
                file_path: String::new(),
                backup_path: None,
                error_message: Some(format!("Invalid line number: {} (must be 0-7)", line)),
                applied_at,
            };
        }

        if location.len() > 128 {
            return ApplyResult {
                success: false,
                file_path: String::new(),
                backup_path: None,
                error_message: Some("Location must be 0-128 characters".to_string()),
                applied_at,
            };
        }

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

        let backup_path = self.create_backup(&config_file, &content);
        let backup_path_str = backup_path.as_ref().map(|p| p.to_string_lossy().to_string());

        if let Err(e) = self.update_line_location(&mut config, line, &location) {
            return ApplyResult {
                success: false,
                file_path: config_file.to_string_lossy().to_string(),
                backup_path: backup_path_str,
                error_message: Some(format!("Failed to update location: {}", e)),
                applied_at,
            };
        }

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

        if let Err(e) = self.write_atomic(&config_file, &new_content) {
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
            "[ConfigApplier] ✓ Sensor location updated for line {}: \"{}\"",
            line, location
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

    /// Apply system info report interval change to main configuration
    pub fn apply_system_info_interval_change(&self, interval_seconds: u64) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // 1. Validate interval (minimum 10 seconds, maximum 24 hours)
        if interval_seconds < 10 {
            return ApplyResult {
                success: false,
                file_path: String::new(),
                backup_path: None,
                error_message: Some("System info interval must be at least 10 seconds".to_string()),
                applied_at,
            };
        }
        if interval_seconds > 86400 {
            return ApplyResult {
                success: false,
                file_path: String::new(),
                backup_path: None,
                error_message: Some("System info interval must be at most 86400 seconds (24 hours)".to_string()),
                applied_at,
            };
        }

        // 2. Determine config file path (main config)
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

        // 6. Update system info interval in mqtt section
        if let Err(e) = self.update_system_info_interval(&mut config, interval_seconds) {
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
            "[ConfigApplier] ✓ System info interval updated: {}s",
            interval_seconds
        );

        ApplyResult {
            success: true,
            file_path: config_file.to_string_lossy().to_string(),
            backup_path: backup_path_str,
            error_message: None,
            applied_at,
        }
    }

    /// Apply device label change to main configuration
    pub fn apply_device_label_change(&self, label: String) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // 1. Validate label (1-64 characters, printable)
        if label.is_empty() {
            return ApplyResult {
                success: false,
                file_path: String::new(),
                backup_path: None,
                error_message: Some("Device label cannot be empty".to_string()),
                applied_at,
            };
        }
        if label.len() > 64 {
            return ApplyResult {
                success: false,
                file_path: String::new(),
                backup_path: None,
                error_message: Some("Device label must be at most 64 characters".to_string()),
                applied_at,
            };
        }

        // 2. Determine config file path (main config)
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

        // 6. Update device_label in system section
        if let Err(e) = self.update_device_label(&mut config, &label) {
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
            "[ConfigApplier] ✓ Device label updated: \"{}\"",
            label
        );

        ApplyResult {
            success: true,
            file_path: config_file.to_string_lossy().to_string(),
            backup_path: backup_path_str,
            error_message: None,
            applied_at,
        }
    }

    /// Apply LoRaWAN sensor configuration change to main config
    pub fn apply_lorawan_sensor_config(
        &self,
        dev_eui: String,
        name: Option<String>,
        serial_number: Option<String>,
        temp_critical_low: Option<f32>,
        temp_warning_low: Option<f32>,
        temp_warning_high: Option<f32>,
        temp_critical_high: Option<f32>,
        humidity_critical_low: Option<f32>,
        humidity_warning_low: Option<f32>,
        humidity_warning_high: Option<f32>,
        humidity_critical_high: Option<f32>,
    ) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // Validate dev_eui
        if dev_eui.is_empty() {
            return ApplyResult {
                success: false,
                file_path: String::new(),
                backup_path: None,
                error_message: Some("dev_eui cannot be empty".to_string()),
                applied_at,
            };
        }

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

        let backup_path = self.create_backup(&config_file, &content);
        let backup_path_str = backup_path.as_ref().map(|p| p.to_string_lossy().to_string());

        // Get or create lorawan.sensors array
        if let Err(e) = self.update_lorawan_sensor_config(
            &mut config,
            &dev_eui,
            name.as_deref(),
            serial_number.as_deref(),
            temp_critical_low, temp_warning_low, temp_warning_high, temp_critical_high,
            humidity_critical_low, humidity_warning_low, humidity_warning_high, humidity_critical_high,
        ) {
            return ApplyResult {
                success: false,
                file_path: config_file.to_string_lossy().to_string(),
                backup_path: backup_path_str,
                error_message: Some(e),
                applied_at,
            };
        }

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

        if let Err(e) = self.write_atomic(&config_file, &new_content) {
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
            "[ConfigApplier] ✓ LoRaWAN sensor config updated for {}",
            dev_eui
        );

        ApplyResult {
            success: true,
            file_path: config_file.to_string_lossy().to_string(),
            backup_path: backup_path_str,
            error_message: None,
            applied_at,
        }
    }

    /// Remove a LoRaWAN sensor configuration from main config by dev_eui
    pub fn remove_lorawan_sensor_config(&self, dev_eui: String) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // Validate dev_eui
        if dev_eui.is_empty() {
            return ApplyResult {
                success: false,
                file_path: String::new(),
                backup_path: None,
                error_message: Some("dev_eui cannot be empty".to_string()),
                applied_at,
            };
        }

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

        let backup_path = self.create_backup(&config_file, &content);
        let backup_path_str = backup_path.as_ref().map(|p| p.to_string_lossy().to_string());

        // Remove sensor from lorawan.sensors array
        let removed = (|| -> Result<bool, String> {
            let lorawan = config
                .get_mut("lorawan")
                .and_then(|v| v.as_mapping_mut())
                .ok_or_else(|| "Missing 'lorawan' section in config".to_string())?;

            let sensors_key = Value::String("sensors".to_string());
            let sensors = lorawan
                .get_mut(&sensors_key)
                .and_then(|v| v.as_sequence_mut())
                .ok_or_else(|| "Missing 'lorawan.sensors' array in config".to_string())?;

            let original_len = sensors.len();
            sensors.retain(|s| {
                s.get("dev_eui")
                    .and_then(|v| v.as_str())
                    .map(|e| e != dev_eui)
                    .unwrap_or(true)
            });

            Ok(sensors.len() < original_len)
        })();

        match removed {
            Ok(false) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().to_string(),
                    backup_path: backup_path_str,
                    error_message: Some(format!("Sensor with dev_eui '{}' not found", dev_eui)),
                    applied_at,
                };
            }
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().to_string(),
                    backup_path: backup_path_str,
                    error_message: Some(e),
                    applied_at,
                };
            }
            Ok(true) => {} // Successfully removed, continue to save
        }

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

        if let Err(e) = self.write_atomic(&config_file, &new_content) {
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
            "[ConfigApplier] ✓ LoRaWAN sensor config removed for {}",
            dev_eui
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

    /// Update location for a specific sensor line in the YAML structure
    fn update_line_location(&self, config: &mut Value, line: u8, location: &str) -> Result<(), String> {
        let lines = config
            .get_mut("lines")
            .and_then(|v| v.as_sequence_mut())
            .ok_or_else(|| "Missing 'lines' array in config".to_string())?;

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

        let line_map = line_entry
            .as_mapping_mut()
            .ok_or_else(|| "Line entry is not a mapping".to_string())?;

        if location.is_empty() {
            // Remove location field if empty
            line_map.remove(&Value::String("location".to_string()));
        } else {
            line_map.insert(
                Value::String("location".to_string()),
                Value::String(location.to_string()),
            );
        }

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

    /// Update system info interval in the MQTT section of main config
    fn update_system_info_interval(
        &self,
        config: &mut Value,
        interval_seconds: u64,
    ) -> Result<(), String> {
        // Get or create 'mqtt' section
        let mqtt = config
            .get_mut("mqtt")
            .and_then(|v| v.as_mapping_mut())
            .ok_or_else(|| "Missing 'mqtt' section in config".to_string())?;

        // Update system_info_interval_seconds field
        mqtt.insert(
            Value::String("system_info_interval_seconds".to_string()),
            Value::Number(serde_yaml::Number::from(interval_seconds)),
        );

        Ok(())
    }

    /// Apply LED brightness change to main configuration
    pub fn apply_led_brightness_change(&self, brightness: u8) -> ApplyResult {
        self.apply_system_field_u8_change("led_brightness", brightness, "LED brightness")
    }

    /// Apply screen brightness change to main configuration
    pub fn apply_screen_brightness_change(&self, brightness: u8) -> ApplyResult {
        self.apply_system_field_u8_change("screen_brightness", brightness, "Screen brightness")
    }

    /// Apply buzzer volume change to main configuration
    pub fn apply_buzzer_volume_change(&self, volume: u8) -> ApplyResult {
        self.apply_system_field_u8_change("buzzer_volume", volume, "Buzzer volume")
    }

    /// Generic helper to update a u8 field in the system section of main config
    fn apply_system_field_u8_change(&self, field_name: &str, value: u8, display_name: &str) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

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

        let backup_path = self.create_backup(&config_file, &content);
        let backup_path_str = backup_path.as_ref().map(|p| p.to_string_lossy().to_string());

        // Update field in system section
        if let Some(system) = config
            .get_mut("system")
            .and_then(|v| v.as_mapping_mut())
        {
            system.insert(
                Value::String(field_name.to_string()),
                Value::Number(serde_yaml::Number::from(value as u64)),
            );
        } else {
            return ApplyResult {
                success: false,
                file_path: config_file.to_string_lossy().to_string(),
                backup_path: backup_path_str,
                error_message: Some("Missing 'system' section in config".to_string()),
                applied_at,
            };
        }

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

        if let Err(e) = self.write_atomic(&config_file, &new_content) {
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

        eprintln!("[ConfigApplier] ✓ {} updated: {}%", display_name, value);

        ApplyResult {
            success: true,
            file_path: config_file.to_string_lossy().to_string(),
            backup_path: backup_path_str,
            error_message: None,
            applied_at,
        }
    }

    /// Update device label in the system section of main config
    fn update_device_label(&self, config: &mut Value, label: &str) -> Result<(), String> {
        // Get 'system' section, create if doesn't exist
        let config_map = config
            .as_mapping_mut()
            .ok_or_else(|| "Config root is not a mapping".to_string())?;

        // Get or create 'system' section
        let system_key = Value::String("system".to_string());
        if !config_map.contains_key(&system_key) {
            config_map.insert(system_key.clone(), Value::Mapping(Mapping::new()));
        }

        let system = config_map
            .get_mut(&system_key)
            .and_then(|v| v.as_mapping_mut())
            .ok_or_else(|| "Failed to get/create 'system' section".to_string())?;

        // Update device_label field
        system.insert(
            Value::String("device_label".to_string()),
            Value::String(label.to_string()),
        );

        Ok(())
    }

    /// Update or insert a LoRaWAN sensor config in lorawan.sensors array
    #[allow(clippy::too_many_arguments)]
    fn update_lorawan_sensor_config(
        &self,
        config: &mut Value,
        dev_eui: &str,
        name: Option<&str>,
        serial_number: Option<&str>,
        temp_critical_low: Option<f32>,
        temp_warning_low: Option<f32>,
        temp_warning_high: Option<f32>,
        temp_critical_high: Option<f32>,
        humidity_critical_low: Option<f32>,
        humidity_warning_low: Option<f32>,
        humidity_warning_high: Option<f32>,
        humidity_critical_high: Option<f32>,
    ) -> Result<(), String> {
        let config_map = config
            .as_mapping_mut()
            .ok_or_else(|| "Config root is not a mapping".to_string())?;

        // Get or create 'lorawan' section
        let lorawan_key = Value::String("lorawan".to_string());
        if !config_map.contains_key(&lorawan_key) {
            let mut lorawan = Mapping::new();
            lorawan.insert(Value::String("enabled".to_string()), Value::Bool(true));
            lorawan.insert(Value::String("sensors".to_string()), Value::Sequence(Vec::new()));
            config_map.insert(lorawan_key.clone(), Value::Mapping(lorawan));
        }

        let lorawan = config_map
            .get_mut(&lorawan_key)
            .and_then(|v| v.as_mapping_mut())
            .ok_or_else(|| "Failed to get 'lorawan' section".to_string())?;

        // Get or create 'sensors' array
        let sensors_key = Value::String("sensors".to_string());
        if !lorawan.contains_key(&sensors_key) {
            lorawan.insert(sensors_key.clone(), Value::Sequence(Vec::new()));
        }

        let sensors = lorawan
            .get_mut(&sensors_key)
            .and_then(|v| v.as_sequence_mut())
            .ok_or_else(|| "Failed to get 'lorawan.sensors' array".to_string())?;

        // Find existing entry or create new one
        let entry = sensors.iter_mut().find(|s| {
            s.get("dev_eui")
                .and_then(|v| v.as_str())
                .map(|e| e == dev_eui)
                .unwrap_or(false)
        });

        let sensor_map = if let Some(existing) = entry {
            existing
                .as_mapping_mut()
                .ok_or_else(|| "Sensor entry is not a mapping".to_string())?
        } else {
            // Create new entry
            let mut new_entry = Mapping::new();
            new_entry.insert(
                Value::String("dev_eui".to_string()),
                Value::String(dev_eui.to_string()),
            );
            new_entry.insert(Value::String("enabled".to_string()), Value::Bool(true));
            sensors.push(Value::Mapping(new_entry));
            sensors
                .last_mut()
                .unwrap()
                .as_mapping_mut()
                .ok_or_else(|| "Failed to get new sensor entry".to_string())?
        };

        // Update fields
        if let Some(n) = name {
            sensor_map.insert(Value::String("name".to_string()), Value::String(n.to_string()));
        }
        if let Some(sn) = serial_number {
            sensor_map.insert(Value::String("serial_number".to_string()), Value::String(sn.to_string()));
        }

        // Helper to set or remove optional f32 threshold
        let set_opt_f32 = |map: &mut Mapping, key: &str, val: Option<f32>| {
            let k = Value::String(key.to_string());
            match val {
                Some(v) => { map.insert(k, Value::Number(serde_yaml::Number::from(v as f64))); }
                None => { map.remove(&k); }
            }
        };

        set_opt_f32(sensor_map, "temp_critical_low", temp_critical_low);
        set_opt_f32(sensor_map, "temp_warning_low", temp_warning_low);
        set_opt_f32(sensor_map, "temp_warning_high", temp_warning_high);
        set_opt_f32(sensor_map, "temp_critical_high", temp_critical_high);
        set_opt_f32(sensor_map, "humidity_critical_low", humidity_critical_low);
        set_opt_f32(sensor_map, "humidity_warning_low", humidity_warning_low);
        set_opt_f32(sensor_map, "humidity_warning_high", humidity_warning_high);
        set_opt_f32(sensor_map, "humidity_critical_high", humidity_critical_high);

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
