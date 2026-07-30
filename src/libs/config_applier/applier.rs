//! Configuration applier with atomic updates and rollback

use super::validation::{validate_device_label, validate_display_custom_lines, ConfigValidator};
use crate::libs::config::DisplayLine;
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

    /// Optional storage handle for save-and-feed side effects (e.g. appending
    /// `sticker_removed` markers on sensor removal). `None` in test or
    /// pre-storage-init contexts; `Some` in the live runtime.
    storage: Option<crate::libs::storage::StorageHandle>,
}

impl ConfigApplier {
    /// Create a new configuration applier without storage hook. Tests and
    /// callers that don't care about save-and-feed side effects use this.
    pub fn new(config_dir: &Path) -> Result<Self, String> {
        Self::new_with_storage(config_dir, None)
    }

    /// Create a new configuration applier wired to the storage thread so
    /// removal-style commands can record their side effects to the firmware
    /// DB (sticker_removed markers, etc.).
    pub fn new_with_storage(
        config_dir: &Path,
        storage: Option<crate::libs::storage::StorageHandle>,
    ) -> Result<Self, String> {
        let config_dir = config_dir.to_path_buf();
        let backup_dir = config_dir.join(".backups");

        // Create backup directory if it doesn't exist
        fs::create_dir_all(&backup_dir)
            .map_err(|e| format!("Failed to create backup directory: {}", e))?;

        Ok(Self {
            config_dir,
            backup_dir,
            storage,
        })
    }

    /// Fire-and-forget audit-log helper used by every apply_* method that
    /// mutates the YAML. EU MDR Annex I §17.1 requires a trail of any
    /// configuration change that affects device behaviour; before this
    /// helper only `apply_device_label_change` actually wrote a row, so
    /// the trail had silent gaps for thresholds, names, locations,
    /// intervals, LoRaWAN sensor/threshold/sticker changes, etc.
    ///
    /// Failure to log is reported on stderr but never rolls back the
    /// just-committed YAML write — same trade-off as `apply_device_label`.
    fn log_audit(&self, operation: &str, details: String) {
        if let Some(storage) = self.storage.as_ref() {
            if let Err(e) = storage.log_audit_event(
                operation.to_string(),
                Some("config".to_string()),
                Some(details),
            ) {
                eprintln!(
                    "[ConfigApplier] audit log_audit_event({}) failed: {}",
                    operation, e,
                );
            }
        }
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

        self.log_audit(
            "SET_SENSOR_THRESHOLD",
            format!(
                r#"{{"line":{},"critical_low":{},"alarm_low":{},"warning_low":{},"warning_high":{},"alarm_high":{},"critical_high":{}}}"#,
                line, critical_low, alarm_low, warning_low, warning_high, alarm_high, critical_high,
            ),
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

        self.log_audit(
            "SET_SENSOR_NAME",
            format!(r#"{{"line":{},"name":{:?}}}"#, line, name),
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

        self.log_audit(
            "SET_SENSOR_LOCATION",
            format!(r#"{{"line":{},"location":{:?}}}"#, line, location),
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

        self.log_audit(
            "SET_SENSOR_INTERVAL",
            format!(
                r#"{{"sample_interval_ms":{},"aggregation_interval_ms":{},"report_interval_ms":{}}}"#,
                sample_interval_ms, aggregation_interval_ms, report_interval_ms,
            ),
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

        self.log_audit(
            "SET_SYSTEM_INFO_INTERVAL",
            format!(r#"{{"interval_seconds":{}}}"#, interval_seconds),
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

        // 1. Validate label. We intentionally enforce the same policy on
        //    every entry path (BLE FB0A and MQTT SetDeviceLabel) so that
        //    nobody can sneak a malformed label in through one channel and
        //    break the other — most importantly, MQTT topic publishing.
        if let Err(msg) = validate_device_label(&label) {
            return ApplyResult {
                success: false,
                file_path: String::new(),
                backup_path: None,
                error_message: Some(msg),
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

        // 9. Audit. Fire-and-forget — if the storage thread isn't wired up
        //    (tests, early boot) the failure is logged but doesn't roll
        //    back the on-disk change, which has already succeeded.
        if let Some(storage) = self.storage.as_ref() {
            let details = format!(r#"{{"new_label":{:?}}}"#, label);
            if let Err(e) = storage.log_audit_event(
                "SET_DEVICE_LABEL".to_string(),
                Some("config".to_string()),
                Some(details),
            ) {
                eprintln!(
                    "[ConfigApplier] audit log_audit_event(SET_DEVICE_LABEL) failed: {}",
                    e
                );
            }
        }

        ApplyResult {
            success: true,
            file_path: config_file.to_string_lossy().to_string(),
            backup_path: backup_path_str,
            error_message: None,
            applied_at,
        }
    }

    /// Replace the configured physical-display lines (`display.custom_lines`).
    ///
    /// Whole-list replacement rather than per-line add/remove: the Viewer owns
    /// the ordered list and re-sends it in full, which makes the operation
    /// idempotent and makes reordering expressible (a per-line API cannot
    /// express "move row 3 above row 1").
    ///
    /// An empty list removes the key, restoring the built-in overview layout.
    pub fn apply_display_custom_lines(&self, lines: Vec<DisplayLine>) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // 1. Validate the whole list before touching the file, so a single bad
        //    entry can't leave a partially-applied layout on disk.
        if let Err(msg) = validate_display_custom_lines(&lines) {
            return ApplyResult {
                success: false,
                file_path: String::new(),
                backup_path: None,
                error_message: Some(msg),
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

        // 6. Update display.custom_lines
        if let Err(e) = self.update_display_custom_lines(&mut config, &lines) {
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

        if lines.is_empty() {
            eprintln!("[ConfigApplier] ✓ Display lines cleared (built-in layout restored)");
        } else {
            eprintln!("[ConfigApplier] ✓ Display lines updated ({} lines)", lines.len());
        }

        // 9. Audit. Count only — labels and DevEUIs would put sensor identity
        //    into every audit row for no investigative benefit.
        if let Some(storage) = self.storage.as_ref() {
            let details = format!(r#"{{"count":{}}}"#, lines.len());
            if let Err(e) = storage.log_audit_event(
                "SET_DISPLAY_LINES".to_string(),
                Some("config".to_string()),
                Some(details),
            ) {
                eprintln!(
                    "[ConfigApplier] audit log_audit_event(SET_DISPLAY_LINES) failed: {}",
                    e
                );
            }
        }

        ApplyResult {
            success: true,
            file_path: config_file.to_string_lossy().to_string(),
            backup_path: backup_path_str,
            error_message: None,
            applied_at,
        }
    }

    /// Apply LoRaWAN sensor metadata change (name/serial/location) to main config.
    /// Per-field thresholds use `apply_lorawan_field_threshold` / `delete_lorawan_field_threshold`.
    pub fn apply_lorawan_sensor_config(
        &self,
        dev_eui: String,
        name: Option<String>,
        serial_number: Option<String>,
        location: Option<String>,
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
            location.as_deref(),
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

        self.log_audit(
            "SET_LORAWAN_SENSOR_CONFIG",
            format!(
                r#"{{"dev_eui":{:?},"name":{:?},"serial_number":{:?},"location":{:?}}}"#,
                dev_eui, name, serial_number, location,
            ),
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

        // Save-and-feed: record a `sticker_removed` marker in the firmware DB
        // so downstream destinations (replaying via the export drain loop)
        // can see the deprovisioning event and avoid mis-attributing a later
        // re-provisioned incarnation to the old epoch.
        if let Some(storage) = self.storage.as_ref() {
            if let Err(e) = storage.append_sticker_removed(dev_eui.clone(), applied_at) {
                eprintln!(
                    "[ConfigApplier] WARN: failed to append sticker_removed for {}: {}",
                    dev_eui, e
                );
            }
        }

        self.log_audit(
            "REMOVE_LORAWAN_SENSOR_CONFIG",
            format!(r#"{{"dev_eui":{:?}}}"#, dev_eui),
        );

        ApplyResult {
            success: true,
            file_path: config_file.to_string_lossy().to_string(),
            backup_path: backup_path_str,
            error_message: None,
            applied_at,
        }
    }

    /// Apply an external LoRaWAN gateway entry (eui/name) to the main config's
    /// `lorawan.gateways` array. Mirrors `apply_lorawan_sensor_config`.
    pub fn apply_external_gateway(&self, gateway_eui: String, name: Option<String>) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        if gateway_eui.is_empty() {
            return ApplyResult {
                success: false,
                file_path: String::new(),
                backup_path: None,
                error_message: Some("gateway_eui cannot be empty".to_string()),
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

        if let Err(e) = self.update_external_gateway(&mut config, &gateway_eui, name.as_deref()) {
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
            "[ConfigApplier] ✓ External LoRaWAN gateway config updated for {}",
            gateway_eui
        );

        self.log_audit(
            "ADD_EXTERNAL_GATEWAY",
            format!(r#"{{"gateway_eui":{:?},"name":{:?}}}"#, gateway_eui, name),
        );

        ApplyResult {
            success: true,
            file_path: config_file.to_string_lossy().to_string(),
            backup_path: backup_path_str,
            error_message: None,
            applied_at,
        }
    }

    /// Remove an external LoRaWAN gateway from the main config by gateway_eui.
    /// Mirrors `remove_lorawan_sensor_config` (no sticker_removed marker — a
    /// gateway is not a save-and-feed sticker).
    pub fn remove_external_gateway(&self, gateway_eui: String) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        if gateway_eui.is_empty() {
            return ApplyResult {
                success: false,
                file_path: String::new(),
                backup_path: None,
                error_message: Some("gateway_eui cannot be empty".to_string()),
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

        let removed = (|| -> Result<bool, String> {
            let lorawan = config
                .get_mut("lorawan")
                .and_then(|v| v.as_mapping_mut())
                .ok_or_else(|| "Missing 'lorawan' section in config".to_string())?;

            let gateways_key = Value::String("gateways".to_string());
            let gateways = lorawan
                .get_mut(&gateways_key)
                .and_then(|v| v.as_sequence_mut())
                .ok_or_else(|| "Missing 'lorawan.gateways' array in config".to_string())?;

            let original_len = gateways.len();
            gateways.retain(|g| {
                g.get("gateway_eui")
                    .and_then(|v| v.as_str())
                    .map(|e| e != gateway_eui)
                    .unwrap_or(true)
            });

            Ok(gateways.len() < original_len)
        })();

        match removed {
            Ok(false) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().to_string(),
                    backup_path: backup_path_str,
                    error_message: Some(format!("Gateway with gateway_eui '{}' not found", gateway_eui)),
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
            Ok(true) => {}
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
            "[ConfigApplier] ✓ External LoRaWAN gateway config removed for {}",
            gateway_eui
        );

        self.log_audit(
            "REMOVE_EXTERNAL_GATEWAY",
            format!(r#"{{"gateway_eui":{:?}}}"#, gateway_eui),
        );

        ApplyResult {
            success: true,
            file_path: config_file.to_string_lossy().to_string(),
            backup_path: backup_path_str,
            error_message: None,
            applied_at,
        }
    }

    /// Register/update an EYE BLE tag in the main config (`eye.tags[]`).
    /// `mac` is stored uppercase; `name` is optional. Auto-provisioning still
    /// discovers unknown tags — this pins an explicit, named entry.
    pub fn apply_eye_tag_config(&self, mac: String, name: Option<String>) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let mac = mac.to_uppercase();
        if mac.is_empty() {
            return ApplyResult {
                success: false,
                file_path: String::new(),
                backup_path: None,
                error_message: Some("mac cannot be empty".to_string()),
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

        let created = match self.update_eye_tag_config(&mut config, &mac, name.as_deref()) {
            Ok(created) => created,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().to_string(),
                    backup_path: backup_path_str,
                    error_message: Some(e),
                    applied_at,
                };
            }
        };

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
            "[ConfigApplier] ✓ EYE tag {} for {}",
            if created { "added" } else { "updated" },
            mac
        );

        self.log_audit(
            "ADD_EYE_TAG",
            format!(r#"{{"mac":{:?},"name":{:?}}}"#, mac, name),
        );

        ApplyResult {
            success: true,
            file_path: config_file.to_string_lossy().to_string(),
            backup_path: backup_path_str,
            error_message: None,
            applied_at,
        }
    }

    /// Persist an EYE tag's recording on/off + interval into `eye.tags[mac]`.
    /// `interval_min == 0` disables recording so it survives a restart and the
    /// gap/fallback sync stops re-enabling it (H1). The tag must already exist.
    pub fn apply_eye_recording(&self, mac: String, interval_min: u16) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let mac = mac.to_uppercase();
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

        if let Err(e) = self.update_eye_recording_config(&mut config, &mac, interval_min) {
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
            "[ConfigApplier] ✓ EYE recording {} for {}",
            if interval_min == 0 { "off".to_string() } else { format!("{interval_min}min") },
            mac
        );
        self.log_audit(
            "SET_EYE_RECORDING",
            format!(r#"{{"mac":{:?},"interval_min":{}}}"#, mac, interval_min),
        );

        ApplyResult {
            success: true,
            file_path: config_file.to_string_lossy().to_string(),
            backup_path: backup_path_str,
            error_message: None,
            applied_at,
        }
    }

    /// Remove an EYE BLE tag from the main config (`eye.tags[]`) by MAC.
    pub fn remove_eye_tag_config(&self, mac: String) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let mac = mac.to_uppercase();
        if mac.is_empty() {
            return ApplyResult {
                success: false,
                file_path: String::new(),
                backup_path: None,
                error_message: Some("mac cannot be empty".to_string()),
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

        // Remove tag from eye.tags array (MAC compared case-insensitively).
        let removed = (|| -> Result<bool, String> {
            let eye = config
                .get_mut("eye")
                .and_then(|v| v.as_mapping_mut())
                .ok_or_else(|| "Missing 'eye' section in config".to_string())?;

            let tags_key = Value::String("tags".to_string());
            let tags = eye
                .get_mut(&tags_key)
                .and_then(|v| v.as_sequence_mut())
                .ok_or_else(|| "Missing 'eye.tags' array in config".to_string())?;

            let original_len = tags.len();
            tags.retain(|t| {
                t.get("mac")
                    .and_then(|v| v.as_str())
                    .map(|m| m.to_uppercase() != mac)
                    .unwrap_or(true)
            });

            Ok(tags.len() < original_len)
        })();

        match removed {
            Ok(false) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().to_string(),
                    backup_path: backup_path_str,
                    error_message: Some(format!("EYE tag with mac '{}' not found", mac)),
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
            Ok(true) => {}
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

        eprintln!("[ConfigApplier] ✓ EYE tag config removed for {}", mac);

        self.log_audit("REMOVE_EYE_TAG", format!(r#"{{"mac":{:?}}}"#, mac));

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

    /// Apply screen idle-timeout change (seconds) to main configuration
    pub fn apply_screen_timeout_change(&self, secs: u32) -> ApplyResult {
        self.apply_system_field_u32_change("screen_timeout_secs", secs, "Screen timeout")
    }

    /// Update a `u8` field in the `system` section (brightness/volume; logged with `%`).
    fn apply_system_field_u8_change(&self, field_name: &str, value: u8, display_name: &str) -> ApplyResult {
        self.apply_system_field_num_change(field_name, u64::from(value), display_name, "%")
    }

    /// Update a `u32` field in the `system` section (e.g. `screen_timeout_secs`,
    /// which exceeds a `u8`; logged without a `%` suffix).
    fn apply_system_field_u32_change(&self, field_name: &str, value: u32, display_name: &str) -> ApplyResult {
        self.apply_system_field_num_change(field_name, u64::from(value), display_name, "")
    }

    /// Shared implementation for numeric `system` fields: atomic YAML rewrite with
    /// timestamped backup, rollback on failure, and an audit log entry. `unit` is
    /// appended to the success log line (e.g. `"%"` for percentages, `""` otherwise).
    fn apply_system_field_num_change(&self, field_name: &str, value: u64, display_name: &str, unit: &str) -> ApplyResult {
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
                Value::Number(serde_yaml::Number::from(value)),
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

        eprintln!("[ConfigApplier] ✓ {} updated: {}{}", display_name, value, unit);

        self.log_audit(
            "SET_SYSTEM_FIELD",
            format!(r#"{{"field":{:?},"value":{}}}"#, field_name, value),
        );

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

    /// Write `display.custom_lines` into the untyped config tree, creating the
    /// `display` section if it isn't there yet.
    ///
    /// An empty list removes the key entirely rather than writing `[]`, so the
    /// on-disk config goes back to exactly the shape a device that never used
    /// this feature has.
    fn update_display_custom_lines(
        &self,
        config: &mut Value,
        lines: &[DisplayLine],
    ) -> Result<(), String> {
        let config_map = config
            .as_mapping_mut()
            .ok_or_else(|| "Config root is not a mapping".to_string())?;

        let display_key = Value::String("display".to_string());
        if !config_map.contains_key(&display_key) {
            config_map.insert(display_key.clone(), Value::Mapping(Mapping::new()));
        }

        let display = config_map
            .get_mut(&display_key)
            .and_then(|v| v.as_mapping_mut())
            .ok_or_else(|| "Failed to get/create 'display' section".to_string())?;

        let lines_key = Value::String("custom_lines".to_string());
        if lines.is_empty() {
            display.remove(&lines_key);
        } else {
            let value = serde_yaml::to_value(lines)
                .map_err(|e| format!("Failed to serialize display lines: {}", e))?;
            display.insert(lines_key, value);
        }

        Ok(())
    }

    /// Update or insert a LoRaWAN sensor config in lorawan.sensors array (metadata only)
    fn update_lorawan_sensor_config(
        &self,
        config: &mut Value,
        dev_eui: &str,
        name: Option<&str>,
        serial_number: Option<&str>,
        location: Option<&str>,
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
        if let Some(loc) = location {
            sensor_map.insert(Value::String("location".to_string()), Value::String(loc.to_string()));
        }

        Ok(())
    }

    /// Update or insert an external gateway in the lorawan.gateways array.
    fn update_external_gateway(
        &self,
        config: &mut Value,
        gateway_eui: &str,
        name: Option<&str>,
    ) -> Result<(), String> {
        let config_map = config
            .as_mapping_mut()
            .ok_or_else(|| "Config root is not a mapping".to_string())?;

        // Get or create 'lorawan' section
        let lorawan_key = Value::String("lorawan".to_string());
        if !config_map.contains_key(&lorawan_key) {
            let mut lorawan = Mapping::new();
            lorawan.insert(Value::String("enabled".to_string()), Value::Bool(true));
            lorawan.insert(Value::String("gateways".to_string()), Value::Sequence(Vec::new()));
            config_map.insert(lorawan_key.clone(), Value::Mapping(lorawan));
        }

        let lorawan = config_map
            .get_mut(&lorawan_key)
            .and_then(|v| v.as_mapping_mut())
            .ok_or_else(|| "Failed to get 'lorawan' section".to_string())?;

        // Get or create 'gateways' array
        let gateways_key = Value::String("gateways".to_string());
        if !lorawan.contains_key(&gateways_key) {
            lorawan.insert(gateways_key.clone(), Value::Sequence(Vec::new()));
        }

        let gateways = lorawan
            .get_mut(&gateways_key)
            .and_then(|v| v.as_sequence_mut())
            .ok_or_else(|| "Failed to get 'lorawan.gateways' array".to_string())?;

        // Find existing entry or create new one
        let entry = gateways.iter_mut().find(|g| {
            g.get("gateway_eui")
                .and_then(|v| v.as_str())
                .map(|e| e == gateway_eui)
                .unwrap_or(false)
        });

        let gateway_map = if let Some(existing) = entry {
            existing
                .as_mapping_mut()
                .ok_or_else(|| "Gateway entry is not a mapping".to_string())?
        } else {
            let mut new_entry = Mapping::new();
            new_entry.insert(
                Value::String("gateway_eui".to_string()),
                Value::String(gateway_eui.to_string()),
            );
            new_entry.insert(Value::String("enabled".to_string()), Value::Bool(true));
            gateways.push(Value::Mapping(new_entry));
            gateways
                .last_mut()
                .unwrap()
                .as_mapping_mut()
                .ok_or_else(|| "Failed to get new gateway entry".to_string())?
        };

        // Update fields
        if let Some(n) = name {
            gateway_map.insert(Value::String("name".to_string()), Value::String(n.to_string()));
        }

        Ok(())
    }

    /// Upsert a per-field threshold inside lorawan.sensors[*].field_thresholds.
    /// Upsert an EYE tag entry in `eye.tags[]` (get-or-create the `eye` section
    /// and `tags` array, then match on `mac` case-insensitively). `mac` is
    /// expected already uppercased by the caller.
    /// Set `recording` (+ `logging_interval_min` when on) on an existing
    /// `eye.tags[mac]` entry. Errors if the tag is not present.
    fn update_eye_recording_config(
        &self,
        config: &mut Value,
        mac: &str,
        interval_min: u16,
    ) -> Result<(), String> {
        let not_found = || format!("EYE tag with mac '{mac}' not found");
        let config_map = config
            .as_mapping_mut()
            .ok_or_else(|| "Config root is not a mapping".to_string())?;
        let eye = config_map
            .get_mut(&Value::String("eye".to_string()))
            .and_then(|v| v.as_mapping_mut())
            .ok_or_else(not_found)?;
        let tags = eye
            .get_mut(&Value::String("tags".to_string()))
            .and_then(|v| v.as_sequence_mut())
            .ok_or_else(not_found)?;
        let tag = tags
            .iter_mut()
            .find(|t| {
                t.get("mac")
                    .and_then(|v| v.as_str())
                    .map(|m| m.to_uppercase() == mac)
                    .unwrap_or(false)
            })
            .ok_or_else(not_found)?
            .as_mapping_mut()
            .ok_or_else(|| "Tag entry is not a mapping".to_string())?;
        tag.insert(
            Value::String("recording".to_string()),
            Value::Bool(interval_min != 0),
        );
        if interval_min != 0 {
            tag.insert(
                Value::String("logging_interval_min".to_string()),
                Value::Number((interval_min as u64).into()),
            );
        }
        Ok(())
    }

    fn update_eye_tag_config(
        &self,
        config: &mut Value,
        mac: &str,
        name: Option<&str>,
    ) -> Result<bool, String> {
        let config_map = config
            .as_mapping_mut()
            .ok_or_else(|| "Config root is not a mapping".to_string())?;

        // Get or create 'eye' section
        let eye_key = Value::String("eye".to_string());
        if !config_map.contains_key(&eye_key) {
            let mut eye = Mapping::new();
            eye.insert(Value::String("enabled".to_string()), Value::Bool(true));
            eye.insert(Value::String("tags".to_string()), Value::Sequence(Vec::new()));
            config_map.insert(eye_key.clone(), Value::Mapping(eye));
        }

        let eye = config_map
            .get_mut(&eye_key)
            .and_then(|v| v.as_mapping_mut())
            .ok_or_else(|| "Failed to get 'eye' section".to_string())?;

        // Get or create 'tags' array
        let tags_key = Value::String("tags".to_string());
        if !eye.contains_key(&tags_key) {
            eye.insert(tags_key.clone(), Value::Sequence(Vec::new()));
        }

        let tags = eye
            .get_mut(&tags_key)
            .and_then(|v| v.as_sequence_mut())
            .ok_or_else(|| "Failed to get 'eye.tags' array".to_string())?;

        // Find existing entry (case-insensitive MAC) or create a new one.
        let entry = tags.iter_mut().find(|t| {
            t.get("mac")
                .and_then(|v| v.as_str())
                .map(|m| m.to_uppercase() == mac)
                .unwrap_or(false)
        });
        let created = entry.is_none();

        let tag_map = if let Some(existing) = entry {
            existing
                .as_mapping_mut()
                .ok_or_else(|| "Tag entry is not a mapping".to_string())?
        } else {
            let mut new_entry = Mapping::new();
            new_entry.insert(
                Value::String("mac".to_string()),
                Value::String(mac.to_string()),
            );
            new_entry.insert(Value::String("enabled".to_string()), Value::Bool(true));
            tags.push(Value::Mapping(new_entry));
            tags.last_mut()
                .unwrap()
                .as_mapping_mut()
                .ok_or_else(|| "Failed to get new tag entry".to_string())?
        };

        if let Some(n) = name {
            tag_map.insert(Value::String("name".to_string()), Value::String(n.to_string()));
        }

        Ok(created)
    }

    /// Upsert a per-field threshold inside lorawan.sensors[*].field_thresholds.
    /// Upsert a per-field threshold inside lorawan.sensors[*].field_thresholds.
    pub fn apply_lorawan_field_threshold(
        &self,
        dev_eui: String,
        field: String,
        critical_low: Option<f64>,
        warning_low: Option<f64>,
        warning_high: Option<f64>,
        critical_high: Option<f64>,
    ) -> ApplyResult {
        let applied_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
        if dev_eui.is_empty() {
            return ApplyResult { success: false, file_path: String::new(), backup_path: None,
                error_message: Some("dev_eui cannot be empty".into()), applied_at };
        }
        let config_file = self.config_dir.join("fiber.config.yaml");
        let content = match fs::read_to_string(&config_file) {
            Ok(c) => c,
            Err(e) => return ApplyResult { success: false, file_path: config_file.to_string_lossy().into(),
                backup_path: None, error_message: Some(format!("Failed to read config: {}", e)), applied_at },
        };
        let mut cfg: Value = match serde_yaml::from_str(&content) {
            Ok(c) => c,
            Err(e) => return ApplyResult { success: false, file_path: config_file.to_string_lossy().into(),
                backup_path: None, error_message: Some(format!("Failed to parse YAML: {}", e)), applied_at },
        };
        let backup_path = self.create_backup(&config_file, &content);
        let backup_str = backup_path.as_ref().map(|p| p.to_string_lossy().to_string());

        if let Err(e) = self.upsert_field_threshold(&mut cfg, &dev_eui, &field,
            critical_low, warning_low, warning_high, critical_high)
        {
            return ApplyResult { success: false, file_path: config_file.to_string_lossy().into(),
                backup_path: backup_str, error_message: Some(e), applied_at };
        }

        let new_content = match serde_yaml::to_string(&cfg) {
            Ok(c) => c,
            Err(e) => return ApplyResult { success: false, file_path: config_file.to_string_lossy().into(),
                backup_path: backup_str, error_message: Some(e.to_string()), applied_at },
        };
        // Atomic write + rollback on failure, matching every other apply_*
        // method. fs::write directly would truncate the YAML mid-write on
        // power loss, leaving the device with an unparseable config — and
        // there's no rollback path back to the backup we just took.
        if let Err(e) = self.write_atomic(&config_file, &new_content) {
            if let Some(b) = backup_path.as_ref() {
                let _ = self.rollback(&config_file, b);
            }
            return ApplyResult { success: false, file_path: config_file.to_string_lossy().into(),
                backup_path: backup_str, error_message: Some(e), applied_at };
        }
        self.log_audit(
            "SET_LORAWAN_FIELD_THRESHOLD",
            format!(
                r#"{{"dev_eui":{:?},"field":{:?},"critical_low":{:?},"warning_low":{:?},"warning_high":{:?},"critical_high":{:?}}}"#,
                dev_eui, field, critical_low, warning_low, warning_high, critical_high,
            ),
        );
        ApplyResult { success: true, file_path: config_file.to_string_lossy().into(),
            backup_path: backup_str, error_message: None, applied_at }
    }

    /// Remove a per-field threshold from lorawan.sensors[*].field_thresholds.
    pub fn delete_lorawan_field_threshold(&self, dev_eui: String, field: String) -> ApplyResult {
        let applied_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
        let config_file = self.config_dir.join("fiber.config.yaml");
        let content = match fs::read_to_string(&config_file) {
            Ok(c) => c,
            Err(e) => return ApplyResult { success: false, file_path: config_file.to_string_lossy().into(),
                backup_path: None, error_message: Some(format!("Failed to read config: {}", e)), applied_at },
        };
        let mut cfg: Value = match serde_yaml::from_str(&content) {
            Ok(c) => c,
            Err(e) => return ApplyResult { success: false, file_path: config_file.to_string_lossy().into(),
                backup_path: None, error_message: Some(format!("Failed to parse YAML: {}", e)), applied_at },
        };
        let backup_path = self.create_backup(&config_file, &content);
        let backup_str = backup_path.as_ref().map(|p| p.to_string_lossy().to_string());

        // Surface whether anything was actually removed so callers can tell
        // a real delete from a misspelled-dev_eui/no-op. The YAML rewrite
        // still happens either way because re-serialising is harmless.
        let removed = self.remove_field_threshold(&mut cfg, &dev_eui, &field).unwrap_or(false);
        let new_content = match serde_yaml::to_string(&cfg) {
            Ok(c) => c,
            Err(e) => return ApplyResult { success: false, file_path: config_file.to_string_lossy().into(),
                backup_path: backup_str, error_message: Some(e.to_string()), applied_at },
        };
        if let Err(e) = self.write_atomic(&config_file, &new_content) {
            if let Some(b) = backup_path.as_ref() {
                let _ = self.rollback(&config_file, b);
            }
            return ApplyResult { success: false, file_path: config_file.to_string_lossy().into(),
                backup_path: backup_str, error_message: Some(e), applied_at };
        }
        if !removed {
            eprintln!(
                "[ConfigApplier] delete_lorawan_field_threshold: no threshold matched (dev_eui={}, field={}) — wrote yaml anyway",
                dev_eui, field,
            );
        }
        self.log_audit(
            "DELETE_LORAWAN_FIELD_THRESHOLD",
            format!(r#"{{"dev_eui":{:?},"field":{:?},"removed":{}}}"#, dev_eui, field, removed),
        );
        ApplyResult { success: true, file_path: config_file.to_string_lossy().into(),
            backup_path: backup_str, error_message: None, applied_at }
    }

    fn upsert_field_threshold(
        &self, config: &mut Value, dev_eui: &str, field: &str,
        critical_low: Option<f64>, warning_low: Option<f64>,
        warning_high: Option<f64>, critical_high: Option<f64>,
    ) -> Result<(), String> {
        let lorawan_key = Value::String("lorawan".to_string());
        let sensors_key = Value::String("sensors".to_string());
        let ft_key = Value::String("field_thresholds".to_string());

        let lorawan = config.as_mapping_mut()
            .ok_or_else(|| "Config root is not a mapping".to_string())?
            .entry(lorawan_key.clone())
            .or_insert_with(|| {
                let mut m = Mapping::new();
                m.insert(Value::String("enabled".to_string()), Value::Bool(true));
                m.insert(sensors_key.clone(), Value::Sequence(Vec::new()));
                Value::Mapping(m)
            });
        let lorawan_map = lorawan.as_mapping_mut().ok_or_else(|| "lorawan is not a mapping".to_string())?;
        let sensors = lorawan_map.entry(sensors_key.clone())
            .or_insert_with(|| Value::Sequence(Vec::new()))
            .as_sequence_mut().ok_or_else(|| "sensors is not a sequence".to_string())?;

        // Find or create sensor entry
        let idx = sensors.iter().position(|s| s.get("dev_eui").and_then(|v| v.as_str()) == Some(dev_eui));
        let sensor_map = if let Some(i) = idx {
            sensors[i].as_mapping_mut().ok_or_else(|| "sensor entry is not a mapping".to_string())?
        } else {
            let mut m = Mapping::new();
            m.insert(Value::String("dev_eui".to_string()), Value::String(dev_eui.to_string()));
            m.insert(Value::String("enabled".to_string()), Value::Bool(true));
            sensors.push(Value::Mapping(m));
            sensors.last_mut().unwrap().as_mapping_mut().unwrap()
        };

        let thresholds = sensor_map.entry(ft_key.clone())
            .or_insert_with(|| Value::Sequence(Vec::new()))
            .as_sequence_mut().ok_or_else(|| "field_thresholds is not a sequence".to_string())?;

        let make_entry = || -> Value {
            let mut m = Mapping::new();
            m.insert(Value::String("field".to_string()), Value::String(field.to_string()));
            for (k, v) in [("critical_low", critical_low), ("warning_low", warning_low),
                           ("warning_high", warning_high), ("critical_high", critical_high)] {
                if let Some(v) = v {
                    m.insert(Value::String(k.to_string()),
                        Value::Number(serde_yaml::Number::from(v)));
                }
            }
            Value::Mapping(m)
        };

        if let Some(existing) = thresholds.iter_mut()
            .find(|t| t.get("field").and_then(|v| v.as_str()) == Some(field))
        {
            *existing = make_entry();
        } else {
            thresholds.push(make_entry());
        }
        Ok(())
    }

    /// Returns Ok(true) if a threshold was actually removed, Ok(false) if
    /// the dev_eui/field pair didn't match anything (no-op).
    fn remove_field_threshold(&self, config: &mut Value, dev_eui: &str, field: &str) -> Result<bool, String> {
        let lorawan = config.as_mapping_mut()
            .and_then(|m| m.get_mut(&Value::String("lorawan".into())))
            .and_then(|v| v.as_mapping_mut());
        let Some(lorawan_map) = lorawan else { return Ok(false); };
        let Some(sensors) = lorawan_map.get_mut(&Value::String("sensors".into()))
            .and_then(|v| v.as_sequence_mut()) else { return Ok(false); };
        let mut removed = false;
        for s in sensors.iter_mut() {
            if s.get("dev_eui").and_then(|v| v.as_str()) != Some(dev_eui) { continue; }
            let sm = match s.as_mapping_mut() { Some(m) => m, None => continue };
            if let Some(thresholds) = sm.get_mut(&Value::String("field_thresholds".into()))
                .and_then(|v| v.as_sequence_mut())
            {
                let before = thresholds.len();
                thresholds.retain(|t| t.get("field").and_then(|v| v.as_str()) != Some(field));
                if thresholds.len() != before {
                    removed = true;
                }
            }
        }
        Ok(removed)
    }

    /// Upsert a per-field alarm threshold on an EYE tag (`eye.tags[mac].field_thresholds[]`).
    pub fn apply_eye_field_threshold(
        &self,
        mac: String,
        field: String,
        critical_low: Option<f64>,
        warning_low: Option<f64>,
        warning_high: Option<f64>,
        critical_high: Option<f64>,
    ) -> ApplyResult {
        let applied_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
        let mac = mac.to_uppercase();
        if mac.is_empty() {
            return ApplyResult { success: false, file_path: String::new(), backup_path: None,
                error_message: Some("mac cannot be empty".into()), applied_at };
        }
        let config_file = self.config_dir.join("fiber.config.yaml");
        let content = match fs::read_to_string(&config_file) {
            Ok(c) => c,
            Err(e) => return ApplyResult { success: false, file_path: config_file.to_string_lossy().into(),
                backup_path: None, error_message: Some(format!("Failed to read config: {}", e)), applied_at },
        };
        let mut cfg: Value = match serde_yaml::from_str(&content) {
            Ok(c) => c,
            Err(e) => return ApplyResult { success: false, file_path: config_file.to_string_lossy().into(),
                backup_path: None, error_message: Some(format!("Failed to parse YAML: {}", e)), applied_at },
        };
        let backup_path = self.create_backup(&config_file, &content);
        let backup_str = backup_path.as_ref().map(|p| p.to_string_lossy().to_string());
        if let Err(e) = self.upsert_eye_field_threshold(&mut cfg, &mac, &field,
            critical_low, warning_low, warning_high, critical_high)
        {
            return ApplyResult { success: false, file_path: config_file.to_string_lossy().into(),
                backup_path: backup_str, error_message: Some(e), applied_at };
        }
        let new_content = match serde_yaml::to_string(&cfg) {
            Ok(c) => c,
            Err(e) => return ApplyResult { success: false, file_path: config_file.to_string_lossy().into(),
                backup_path: backup_str, error_message: Some(e.to_string()), applied_at },
        };
        if let Err(e) = self.write_atomic(&config_file, &new_content) {
            if let Some(b) = backup_path.as_ref() { let _ = self.rollback(&config_file, b); }
            return ApplyResult { success: false, file_path: config_file.to_string_lossy().into(),
                backup_path: backup_str, error_message: Some(e), applied_at };
        }
        self.log_audit("SET_EYE_FIELD_THRESHOLD",
            format!(r#"{{"mac":{:?},"field":{:?},"critical_low":{:?},"warning_low":{:?},"warning_high":{:?},"critical_high":{:?}}}"#,
                mac, field, critical_low, warning_low, warning_high, critical_high));
        ApplyResult { success: true, file_path: config_file.to_string_lossy().into(),
            backup_path: backup_str, error_message: None, applied_at }
    }

    /// Remove a per-field EYE alarm threshold. Returns success even on no-op.
    pub fn delete_eye_field_threshold(&self, mac: String, field: String) -> ApplyResult {
        let applied_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
        let mac = mac.to_uppercase();
        let config_file = self.config_dir.join("fiber.config.yaml");
        let content = match fs::read_to_string(&config_file) {
            Ok(c) => c,
            Err(e) => return ApplyResult { success: false, file_path: config_file.to_string_lossy().into(),
                backup_path: None, error_message: Some(format!("Failed to read config: {}", e)), applied_at },
        };
        let mut cfg: Value = match serde_yaml::from_str(&content) {
            Ok(c) => c,
            Err(e) => return ApplyResult { success: false, file_path: config_file.to_string_lossy().into(),
                backup_path: None, error_message: Some(format!("Failed to parse YAML: {}", e)), applied_at },
        };
        let backup_path = self.create_backup(&config_file, &content);
        let backup_str = backup_path.as_ref().map(|p| p.to_string_lossy().to_string());
        let removed = self.remove_eye_field_threshold(&mut cfg, &mac, &field).unwrap_or(false);
        let new_content = match serde_yaml::to_string(&cfg) {
            Ok(c) => c,
            Err(e) => return ApplyResult { success: false, file_path: config_file.to_string_lossy().into(),
                backup_path: backup_str, error_message: Some(e.to_string()), applied_at },
        };
        if let Err(e) = self.write_atomic(&config_file, &new_content) {
            if let Some(b) = backup_path.as_ref() { let _ = self.rollback(&config_file, b); }
            return ApplyResult { success: false, file_path: config_file.to_string_lossy().into(),
                backup_path: backup_str, error_message: Some(e), applied_at };
        }
        self.log_audit("DELETE_EYE_FIELD_THRESHOLD",
            format!(r#"{{"mac":{:?},"field":{:?},"removed":{}}}"#, mac, field, removed));
        ApplyResult { success: true, file_path: config_file.to_string_lossy().into(),
            backup_path: backup_str, error_message: None, applied_at }
    }

    fn upsert_eye_field_threshold(
        &self, config: &mut Value, mac: &str, field: &str,
        critical_low: Option<f64>, warning_low: Option<f64>,
        warning_high: Option<f64>, critical_high: Option<f64>,
    ) -> Result<(), String> {
        let eye_key = Value::String("eye".to_string());
        let tags_key = Value::String("tags".to_string());
        let ft_key = Value::String("field_thresholds".to_string());
        let eye = config.as_mapping_mut()
            .ok_or_else(|| "Config root is not a mapping".to_string())?
            .entry(eye_key.clone())
            .or_insert_with(|| {
                let mut m = Mapping::new();
                m.insert(Value::String("enabled".to_string()), Value::Bool(true));
                m.insert(tags_key.clone(), Value::Sequence(Vec::new()));
                Value::Mapping(m)
            });
        let eye_map = eye.as_mapping_mut().ok_or_else(|| "eye is not a mapping".to_string())?;
        let tags = eye_map.entry(tags_key.clone())
            .or_insert_with(|| Value::Sequence(Vec::new()))
            .as_sequence_mut().ok_or_else(|| "tags is not a sequence".to_string())?;
        let idx = tags.iter().position(|t| {
            t.get("mac").and_then(|v| v.as_str()).map(|m| m.to_uppercase() == mac).unwrap_or(false)
        });
        let tag_map = if let Some(i) = idx {
            tags[i].as_mapping_mut().ok_or_else(|| "tag entry is not a mapping".to_string())?
        } else {
            let mut m = Mapping::new();
            m.insert(Value::String("mac".to_string()), Value::String(mac.to_string()));
            m.insert(Value::String("enabled".to_string()), Value::Bool(true));
            tags.push(Value::Mapping(m));
            tags.last_mut().unwrap().as_mapping_mut().unwrap()
        };
        let thresholds = tag_map.entry(ft_key.clone())
            .or_insert_with(|| Value::Sequence(Vec::new()))
            .as_sequence_mut().ok_or_else(|| "field_thresholds is not a sequence".to_string())?;
        let make_entry = || -> Value {
            let mut m = Mapping::new();
            m.insert(Value::String("field".to_string()), Value::String(field.to_string()));
            for (k, v) in [("critical_low", critical_low), ("warning_low", warning_low),
                           ("warning_high", warning_high), ("critical_high", critical_high)] {
                if let Some(v) = v {
                    m.insert(Value::String(k.to_string()), Value::Number(serde_yaml::Number::from(v)));
                }
            }
            Value::Mapping(m)
        };
        if let Some(existing) = thresholds.iter_mut()
            .find(|t| t.get("field").and_then(|v| v.as_str()) == Some(field))
        {
            *existing = make_entry();
        } else {
            thresholds.push(make_entry());
        }
        Ok(())
    }

    fn remove_eye_field_threshold(&self, config: &mut Value, mac: &str, field: &str) -> Result<bool, String> {
        let eye = config.as_mapping_mut()
            .and_then(|m| m.get_mut(&Value::String("eye".into())))
            .and_then(|v| v.as_mapping_mut());
        let Some(eye_map) = eye else { return Ok(false); };
        let Some(tags) = eye_map.get_mut(&Value::String("tags".into()))
            .and_then(|v| v.as_sequence_mut()) else { return Ok(false); };
        let mut removed = false;
        for t in tags.iter_mut() {
            let is_match = t.get("mac").and_then(|v| v.as_str())
                .map(|m| m.to_uppercase() == mac).unwrap_or(false);
            if !is_match { continue; }
            let tm = match t.as_mapping_mut() { Some(m) => m, None => continue };
            if let Some(thresholds) = tm.get_mut(&Value::String("field_thresholds".into()))
                .and_then(|v| v.as_sequence_mut())
            {
                let before = thresholds.len();
                thresholds.retain(|x| x.get("field").and_then(|v| v.as_str()) != Some(field));
                if thresholds.len() != before { removed = true; }
            }
        }
        Ok(removed)
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

    #[test]
    fn remove_lorawan_sensor_config_appends_sticker_removed_event() {
        use crate::libs::storage::db::Database;
        use crate::libs::storage::thread::StorageThread;

        let tmp_db = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp_db.path().to_str().unwrap().to_string();

        let (storage, join) = StorageThread::spawn(&db_path, 1).unwrap();

        let tmp_config_dir = tempfile::tempdir().unwrap();
        let config_file = tmp_config_dir.path().join("fiber.config.yaml");
        std::fs::write(
            &config_file,
            "lorawan:\n  sensors:\n    - dev_eui: '70b3d5'\n      name: test\n      enabled: true\n",
        )
        .unwrap();

        let applier = ConfigApplier::new_with_storage(tmp_config_dir.path(), Some(storage.clone())).unwrap();
        let result = applier.remove_lorawan_sensor_config("70b3d5".to_string());
        assert!(result.success, "removal should succeed: {:?}", result.error_message);

        storage.flush().unwrap();
        storage.shutdown().unwrap();
        join.join().unwrap();

        let db = Database::new(&db_path, 1).unwrap();
        let conn = db.connect().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sticker_readings WHERE dev_eui = '70b3d5' AND event_type = 'sticker_removed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(n >= 1, "expected a sticker_removed row to be appended");
    }

    // ---- EYE tag config apply/remove tests --------------------------------------

    #[test]
    fn apply_eye_tag_config_creates_section_and_entry() {
        let tmp_config_dir = tempfile::tempdir().unwrap();
        // No `eye:` section yet — the helper must create it.
        std::fs::write(
            tmp_config_dir.path().join("fiber.config.yaml"),
            "system:\n  device_label: \"X\"\n",
        )
        .unwrap();

        let applier = ConfigApplier::new(tmp_config_dir.path()).unwrap();
        let result = applier
            .apply_eye_tag_config("aa:bb:cc:dd:ee:ff".to_string(), Some("Freezer".to_string()));
        assert!(result.success, "{:?}", result.error_message);

        let contents =
            std::fs::read_to_string(tmp_config_dir.path().join("fiber.config.yaml")).unwrap();
        let parsed: Value = serde_yaml::from_str(&contents).unwrap();
        let tags = parsed["eye"]["tags"].as_sequence().unwrap();
        assert_eq!(tags.len(), 1);
        // MAC is stored uppercase.
        assert_eq!(tags[0]["mac"].as_str().unwrap(), "AA:BB:CC:DD:EE:FF");
        assert_eq!(tags[0]["name"].as_str().unwrap(), "Freezer");
        assert_eq!(tags[0]["enabled"].as_bool().unwrap(), true);
    }

    #[test]
    fn apply_eye_tag_config_updates_existing_name() {
        let tmp_config_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp_config_dir.path().join("fiber.config.yaml"),
            "eye:\n  enabled: true\n  tags:\n    - mac: 'AA:BB:CC:DD:EE:FF'\n      enabled: true\n      name: Old\n",
        )
        .unwrap();

        let applier = ConfigApplier::new(tmp_config_dir.path()).unwrap();
        // Same MAC in lowercase must match the existing (uppercase) entry.
        let result = applier
            .apply_eye_tag_config("aa:bb:cc:dd:ee:ff".to_string(), Some("New".to_string()));
        assert!(result.success, "{:?}", result.error_message);

        let contents =
            std::fs::read_to_string(tmp_config_dir.path().join("fiber.config.yaml")).unwrap();
        let parsed: Value = serde_yaml::from_str(&contents).unwrap();
        let tags = parsed["eye"]["tags"].as_sequence().unwrap();
        assert_eq!(tags.len(), 1, "must upsert, not duplicate");
        assert_eq!(tags[0]["name"].as_str().unwrap(), "New");
    }

    #[test]
    fn remove_eye_tag_config_removes_entry() {
        let tmp_config_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp_config_dir.path().join("fiber.config.yaml"),
            "eye:\n  tags:\n    - mac: 'AA:BB:CC:DD:EE:FF'\n      enabled: true\n",
        )
        .unwrap();

        let applier = ConfigApplier::new(tmp_config_dir.path()).unwrap();
        let result = applier.remove_eye_tag_config("aa:bb:cc:dd:ee:ff".to_string());
        assert!(result.success, "{:?}", result.error_message);

        let contents =
            std::fs::read_to_string(tmp_config_dir.path().join("fiber.config.yaml")).unwrap();
        let parsed: Value = serde_yaml::from_str(&contents).unwrap();
        assert_eq!(parsed["eye"]["tags"].as_sequence().unwrap().len(), 0);
    }

    #[test]
    fn remove_eye_tag_config_missing_is_error() {
        let tmp_config_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp_config_dir.path().join("fiber.config.yaml"),
            "eye:\n  tags: []\n",
        )
        .unwrap();

        let applier = ConfigApplier::new(tmp_config_dir.path()).unwrap();
        let result = applier.remove_eye_tag_config("AA:BB:CC:DD:EE:FF".to_string());
        assert!(!result.success);
        assert!(result.error_message.unwrap().contains("not found"));
    }

    #[test]
    fn apply_eye_recording_persists_off_and_interval() {
        let tmp_config_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp_config_dir.path().join("fiber.config.yaml"),
            "eye:\n  tags:\n    - mac: 'AA:BB:CC:DD:EE:FF'\n      enabled: true\n",
        )
        .unwrap();
        let applier = ConfigApplier::new(tmp_config_dir.path()).unwrap();

        // interval 5 -> recording on + interval persisted
        assert!(applier.apply_eye_recording("aa:bb:cc:dd:ee:ff".to_string(), 5).success);
        let parsed: Value = serde_yaml::from_str(
            &std::fs::read_to_string(tmp_config_dir.path().join("fiber.config.yaml")).unwrap(),
        )
        .unwrap();
        let tag = &parsed["eye"]["tags"][0];
        assert_eq!(tag["recording"].as_bool(), Some(true));
        assert_eq!(tag["logging_interval_min"].as_u64(), Some(5));

        // interval 0 -> recording off persisted (H1: must survive restart)
        assert!(applier.apply_eye_recording("AA:BB:CC:DD:EE:FF".to_string(), 0).success);
        let parsed: Value = serde_yaml::from_str(
            &std::fs::read_to_string(tmp_config_dir.path().join("fiber.config.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(parsed["eye"]["tags"][0]["recording"].as_bool(), Some(false));

        // unknown MAC -> error
        let r = applier.apply_eye_recording("11:22:33:44:55:66".to_string(), 1);
        assert!(!r.success);
        assert!(r.error_message.unwrap().contains("not found"));
    }

    #[test]
    fn apply_and_delete_eye_field_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("fiber.config.yaml"),
            "eye:\n  tags:\n    - mac: 'AA:BB:CC:DD:EE:FF'\n      enabled: true\n",
        )
        .unwrap();
        let applier = ConfigApplier::new(tmp.path()).unwrap();

        // upsert (lowercase MAC must match the uppercase entry)
        assert!(applier
            .apply_eye_field_threshold(
                "aa:bb:cc:dd:ee:ff".into(), "temperature".into(),
                Some(-20.0), Some(0.0), Some(8.0), Some(12.0),
            )
            .success);
        let parsed: Value = serde_yaml::from_str(
            &std::fs::read_to_string(tmp.path().join("fiber.config.yaml")).unwrap(),
        )
        .unwrap();
        let ft = &parsed["eye"]["tags"][0]["field_thresholds"][0];
        assert_eq!(ft["field"].as_str(), Some("temperature"));
        assert_eq!(ft["critical_high"].as_f64(), Some(12.0));

        // delete
        assert!(applier
            .delete_eye_field_threshold("AA:BB:CC:DD:EE:FF".into(), "temperature".into())
            .success);
        let parsed: Value = serde_yaml::from_str(
            &std::fs::read_to_string(tmp.path().join("fiber.config.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            parsed["eye"]["tags"][0]["field_thresholds"].as_sequence().unwrap().len(),
            0
        );
    }

    // ---- device label apply-path tests -----------------------------------------

    fn write_minimal_main_config(dir: &std::path::Path) {
        let yaml = "system:\n  device_label: \"OLD-LABEL\"\n";
        std::fs::write(dir.join("fiber.config.yaml"), yaml).unwrap();
    }

    #[test]
    fn apply_device_label_change_persists_new_value() {
        let tmp_config_dir = tempfile::tempdir().unwrap();
        write_minimal_main_config(tmp_config_dir.path());

        let applier = ConfigApplier::new(tmp_config_dir.path()).unwrap();
        let result = applier.apply_device_label_change("Ward 3 Freezer".to_string());
        assert!(result.success, "{:?}", result.error_message);

        let contents = std::fs::read_to_string(tmp_config_dir.path().join("fiber.config.yaml")).unwrap();
        assert!(contents.contains("Ward 3 Freezer"), "got: {contents}");
        assert!(!contents.contains("OLD-LABEL"), "old value should be gone: {contents}");
    }

    #[test]
    fn apply_screen_timeout_change_persists_u32_value() {
        let tmp_config_dir = tempfile::tempdir().unwrap();
        // A value that exceeds u8, exercising the u32 write path.
        std::fs::write(
            tmp_config_dir.path().join("fiber.config.yaml"),
            "system:\n  screen_timeout_secs: 60\n",
        )
        .unwrap();

        let applier = ConfigApplier::new(tmp_config_dir.path()).unwrap();
        let result = applier.apply_screen_timeout_change(3600);
        assert!(result.success, "{:?}", result.error_message);

        // Reload the YAML and confirm the persisted value round-trips.
        let contents = std::fs::read_to_string(tmp_config_dir.path().join("fiber.config.yaml")).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&contents).unwrap();
        assert_eq!(parsed["system"]["screen_timeout_secs"].as_u64(), Some(3600));
    }

    #[test]
    fn apply_device_label_change_rejects_empty() {
        let tmp_config_dir = tempfile::tempdir().unwrap();
        write_minimal_main_config(tmp_config_dir.path());
        let applier = ConfigApplier::new(tmp_config_dir.path()).unwrap();

        let result = applier.apply_device_label_change(String::new());
        assert!(!result.success);
        assert!(
            result.error_message.unwrap().to_lowercase().contains("empty"),
        );
    }

    #[test]
    fn apply_device_label_change_rejects_mqtt_chars() {
        let tmp_config_dir = tempfile::tempdir().unwrap();
        write_minimal_main_config(tmp_config_dir.path());
        let applier = ConfigApplier::new(tmp_config_dir.path()).unwrap();

        let result = applier.apply_device_label_change("bad/label".to_string());
        assert!(!result.success);
        assert!(result.error_message.unwrap().to_lowercase().contains("mqtt"));

        // And the on-disk file is untouched.
        let contents = std::fs::read_to_string(tmp_config_dir.path().join("fiber.config.yaml")).unwrap();
        assert!(contents.contains("OLD-LABEL"));
    }

    #[test]
    fn apply_device_label_change_rejects_unicode() {
        let tmp_config_dir = tempfile::tempdir().unwrap();
        write_minimal_main_config(tmp_config_dir.path());
        let applier = ConfigApplier::new(tmp_config_dir.path()).unwrap();

        let result = applier.apply_device_label_change("Câmara".to_string());
        assert!(!result.success);
    }

    #[test]
    fn apply_device_label_change_writes_audit_entry() {
        use crate::libs::storage::db::Database;
        use crate::libs::storage::thread::StorageThread;

        let tmp_db = tempfile::NamedTempFile::new().unwrap();
        let db_path = tmp_db.path().to_str().unwrap().to_string();
        let (storage, join) = StorageThread::spawn(&db_path, 1).unwrap();

        let tmp_config_dir = tempfile::tempdir().unwrap();
        write_minimal_main_config(tmp_config_dir.path());
        let applier = ConfigApplier::new_with_storage(
            tmp_config_dir.path(),
            Some(storage.clone()),
        )
        .unwrap();

        let result = applier.apply_device_label_change("New Label".to_string());
        assert!(result.success, "{:?}", result.error_message);

        storage.flush().unwrap();
        storage.shutdown().unwrap();
        join.join().unwrap();

        let db = Database::new(&db_path, 1).unwrap();
        let conn = db.connect().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audit_log WHERE operation = 'SET_DEVICE_LABEL'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(n >= 1, "expected an SET_DEVICE_LABEL audit row to be written");

        let details: Option<String> = conn
            .query_row(
                "SELECT details FROM audit_log WHERE operation = 'SET_DEVICE_LABEL' ORDER BY timestamp DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let details = details.expect("details should be Some");
        assert!(details.contains("New Label"), "details should carry new label: {details}");
    }
}

#[cfg(test)]
mod display_lines_tests {
    use super::*;
    use crate::libs::config::{DisplayLineFormat, DisplayLineSource};

    const EUI: &str = "70b3d57ed0051f2a";

    fn ds_line(idx: u8) -> DisplayLine {
        DisplayLine {
            source: DisplayLineSource::Ds18b20,
            line: Some(idx),
            dev_eui: None,
            field: "temperature".to_string(),
            label: None,
            format: DisplayLineFormat::default(),
        }
    }

    fn sticker_line(field: &str) -> DisplayLine {
        DisplayLine {
            source: DisplayLineSource::Sticker,
            line: None,
            dev_eui: Some(EUI.to_string()),
            field: field.to_string(),
            label: Some("Chiller".to_string()),
            format: DisplayLineFormat::default(),
        }
    }

    /// A main config with a few unrelated sections, so the tests can check that
    /// the untyped-Value round trip leaves them alone.
    fn write_config(dir: &std::path::Path) -> std::path::PathBuf {
        let yaml = "\
system:
  device_label: \"KEEP-ME\"
  screen_brightness: 50
mqtt:
  broker:
    host: \"example.invalid\"
    port: 8883
";
        let path = dir.join("fiber.config.yaml");
        std::fs::write(&path, yaml).unwrap();
        path
    }

    fn read_yaml(path: &std::path::Path) -> Value {
        serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn creates_display_section_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path());
        let applier = ConfigApplier::new(dir.path()).unwrap();

        let result = applier.apply_display_custom_lines(vec![sticker_line("voltage"), ds_line(0)]);
        assert!(result.success, "{:?}", result.error_message);

        let parsed = read_yaml(&path);
        let lines = parsed["display"]["custom_lines"].as_sequence().unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["field"].as_str(), Some("voltage"));
        assert_eq!(lines[0]["dev_eui"].as_str(), Some(EUI));
        assert_eq!(lines[0]["source"].as_str(), Some("sticker"));
        assert_eq!(lines[1]["line"].as_u64(), Some(0));
        assert_eq!(lines[1]["source"].as_str(), Some("ds18b20"));
    }

    #[test]
    fn replaces_existing_list_wholesale() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path());
        let applier = ConfigApplier::new(dir.path()).unwrap();

        applier.apply_display_custom_lines(vec![ds_line(0), ds_line(1), ds_line(2)]);
        let result = applier.apply_display_custom_lines(vec![sticker_line("humidity")]);
        assert!(result.success, "{:?}", result.error_message);

        let parsed = read_yaml(&path);
        let lines = parsed["display"]["custom_lines"].as_sequence().unwrap();
        assert_eq!(lines.len(), 1, "old entries must be gone, not merged");
        assert_eq!(lines[0]["field"].as_str(), Some("humidity"));
    }

    #[test]
    fn empty_list_removes_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path());
        let applier = ConfigApplier::new(dir.path()).unwrap();

        applier.apply_display_custom_lines(vec![ds_line(0)]);
        let result = applier.apply_display_custom_lines(Vec::new());
        assert!(result.success, "{:?}", result.error_message);

        let parsed = read_yaml(&path);
        assert!(
            parsed["display"].get("custom_lines").is_none(),
            "empty list should remove the key, not write [] : {:?}",
            parsed["display"],
        );
    }

    #[test]
    fn preserves_unrelated_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path());
        let applier = ConfigApplier::new(dir.path()).unwrap();

        let result = applier.apply_display_custom_lines(vec![ds_line(3)]);
        assert!(result.success, "{:?}", result.error_message);

        let parsed = read_yaml(&path);
        assert_eq!(parsed["system"]["device_label"].as_str(), Some("KEEP-ME"));
        assert_eq!(parsed["system"]["screen_brightness"].as_u64(), Some(50));
        assert_eq!(parsed["mqtt"]["broker"]["host"].as_str(), Some("example.invalid"));
        assert_eq!(parsed["mqtt"]["broker"]["port"].as_u64(), Some(8883));
    }

    #[test]
    fn rejects_invalid_line_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path());
        let before = std::fs::read_to_string(&path).unwrap();
        let applier = ConfigApplier::new(dir.path()).unwrap();

        let result = applier.apply_display_custom_lines(vec![ds_line(0), ds_line(9)]);
        assert!(!result.success);
        let err = result.error_message.unwrap();
        assert!(err.contains("display line 1"), "should name the index: {err}");

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "file must be untouched when validation fails",
        );
    }

    #[test]
    fn writes_a_backup_before_changing_the_file() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path());
        let applier = ConfigApplier::new(dir.path()).unwrap();

        let result = applier.apply_display_custom_lines(vec![ds_line(0)]);
        assert!(result.success, "{:?}", result.error_message);
        let backup = result.backup_path.expect("a backup path");
        let backup_contents = std::fs::read_to_string(&backup).unwrap();
        assert!(
            !backup_contents.contains("custom_lines"),
            "backup must hold the pre-change content: {backup_contents}",
        );
    }

    #[test]
    fn round_trips_back_through_the_typed_config_parser() {
        // The full write -> migrate -> parse chain, which is what the device
        // actually does on the next config read.
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path());
        let applier = ConfigApplier::new(dir.path()).unwrap();

        let mut line = sticker_line("ext_temperature_1");
        line.format.decimals = Some(2);
        line.format.status_char = false;
        assert!(applier.apply_display_custom_lines(vec![line, ds_line(5)]).success);

        // `Config::from_file` needs every non-defaulted section, so merge the
        // written display section onto a full default config and re-read it.
        let written = read_yaml(&path);
        let mut base = match serde_yaml::to_value(crate::libs::config::Config::default_config()).unwrap() {
            Value::Mapping(m) => m,
            other => panic!("expected mapping, got {other:?}"),
        };
        base.insert(Value::String("display".into()), written["display"].clone());
        let merged = dir.path().join("merged.yaml");
        std::fs::write(&merged, serde_yaml::to_string(&Value::Mapping(base)).unwrap()).unwrap();

        let cfg = crate::libs::config::Config::from_file(&merged).expect("must parse");
        assert_eq!(cfg.display.custom_lines.len(), 2);
        let first = &cfg.display.custom_lines[0];
        assert_eq!(first.field, "ext_temperature_1");
        assert_eq!(first.dev_eui.as_deref(), Some(EUI));
        assert_eq!(first.format.decimals, Some(2));
        assert!(!first.format.status_char);
        assert_eq!(cfg.display.custom_lines[1].line, Some(5));
    }
}
