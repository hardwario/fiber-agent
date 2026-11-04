//! Configuration applier with atomic updates and rollback

use super::validation::{validate_device_label, validate_display_custom_lines, ConfigValidator};
use crate::libs::config::{DisplayLine, SetupRole};
use serde_yaml::{Mapping, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The settled client contract for a Hub↔Hub export: TLS, and only this port
/// (`plan/control/ble/peripheral.md` §6.4). Not a parameter anywhere.
const EXPORT_TLS_PORT: u16 = 8883;

/// `map[key]` as a mapping, creating it when absent and replacing an explicit
/// `null` (the shape an operator gets by deleting a section's body but leaving
/// its key) rather than refusing over it.
fn or_insert_mapping<'a>(map: &'a mut Mapping, key: &str) -> Result<&'a mut Mapping, String> {
    let slot = map
        .entry(Value::String(key.to_string()))
        .or_insert_with(|| Value::Mapping(Mapping::new()));
    if slot.is_null() {
        *slot = Value::Mapping(Mapping::new());
    }
    slot.as_mapping_mut()
        .ok_or_else(|| format!("`{key}:` is not a mapping"))
}

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

    /// `/data/proximos/cluster`, whose role marker [`Self::apply_setup_complete`] checks the
    /// requested role against (`plan/control/cluster.md` §3). Injectable for tests.
    cluster_dir: PathBuf,
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
            cluster_dir: PathBuf::from(crate::libs::cluster_state::CLUSTER_DIR),
        })
    }

    /// The same applier against another cluster directory — a test's.
    pub fn with_cluster_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cluster_dir = dir.into();
        self
    }

    /// The cluster directory this applier checks the role marker in.
    pub fn cluster_dir(&self) -> &Path {
        &self.cluster_dir
    }

    /// Fire-and-forget audit-log helper used by every apply_* method that
    /// mutates the YAML. EU MDR Annex I §17.1 requires a trail of any
    /// configuration change that affects device behaviour; before this
    /// helper only `apply_device_label_change` actually wrote a row, so
    /// the trail had silent gaps for thresholds, names, locations,
    /// intervals, LoRaWAN sensor/threshold/node changes, etc.
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
        hysteresis: Option<f32>,
        enabled: bool,
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
        let backup_path_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

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
            hysteresis,
            enabled,
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
                r#"{{"line":{},"critical_low":{},"alarm_low":{},"warning_low":{},"warning_high":{},"alarm_high":{},"critical_high":{},"hysteresis":{},"enabled":{}}}"#,
                line, critical_low, alarm_low, warning_low, warning_high, alarm_high, critical_high,
                hysteresis.map_or_else(|| "null".to_string(), |h| h.to_string()),
                enabled,
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
        let backup_path_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

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
        let backup_path_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

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

    /// Apply sensor interval changes to main configuration.
    ///
    /// The signed command carries milliseconds (wire contract); the file holds
    /// seconds (plan/control/storage_intervals.md §3). `report_interval_ms` is
    /// retired and ignored.
    pub fn apply_interval_change(
        &self,
        sample_interval_ms: u64,
        aggregation_interval_ms: u64,
        _report_interval_ms: u64,
    ) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let sample_interval_s = sample_interval_ms.div_ceil(1000);
        let aggregation_interval_s = aggregation_interval_ms.div_ceil(1000);

        // 1. Validate intervals
        if let Err(e) =
            ConfigValidator::validate_intervals(sample_interval_s, aggregation_interval_s)
        {
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
        let backup_path_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        // 6. Update intervals in config
        if let Err(e) =
            self.update_sensor_intervals(&mut config, sample_interval_s, aggregation_interval_s)
        {
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
            "[ConfigApplier] ✓ Sensor intervals updated: sample={}s, aggregation={}s",
            sample_interval_s, aggregation_interval_s
        );

        self.log_audit(
            "SET_SENSOR_INTERVAL",
            format!(
                r#"{{"sample_interval_s":{},"aggregation_interval_s":{}}}"#,
                sample_interval_s, aggregation_interval_s,
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
                error_message: Some(
                    "System info interval must be at most 86400 seconds (24 hours)".to_string(),
                ),
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
        let backup_path_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

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
        let backup_path_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

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

        eprintln!("[ConfigApplier] ✓ Device label updated: \"{}\"", label);

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

    /// Read → back up → edit → serialize → write atomically → audit, once.
    ///
    /// Every mutation above spells this sequence out inline. That is how the v1
    /// import arrived and it is left alone on purpose (decision #3 — rewriting
    /// it would bury the import's provenance under a refactor diff). Mutations
    /// added *after* the import share this copy instead, because the sequence is
    /// identical every time and a hand-copied step is exactly where a missing
    /// backup or a missing rollback would hide.
    ///
    /// `edit` returns `Err(message)` to refuse, and then **nothing on disk is
    /// touched at all** — not the config, not a backup. The backup used to be
    /// taken before the closure ran, which made a refusal cost a
    /// `fiber.config.yaml.<secs>.bak` on `/data`; BLE FB18 is not rate-limited,
    /// so an authenticated peer looping on a payload the applier refuses filled
    /// the partition one backup per second. The closure only ever mutates the
    /// parsed document, never the file, so validating first costs nothing — and
    /// it puts this helper where the pre-existing `apply_*` methods already are,
    /// which judge their input before they touch anything on disk.
    fn edit_config_yaml(
        &self,
        audit_action: &str,
        audit_details: Option<String>,
        edit: impl FnOnce(&mut Value) -> Result<(), String>,
    ) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let config_file = self.config_dir.join("fiber.config.yaml");
        let fail = |file: &Path, backup: Option<String>, message: String| ApplyResult {
            success: false,
            file_path: file.to_string_lossy().to_string(),
            backup_path: backup,
            error_message: Some(message),
            applied_at,
        };

        if !config_file.exists() {
            return fail(&config_file, None, "Main config file not found".to_string());
        }

        let content = match fs::read_to_string(&config_file) {
            Ok(c) => c,
            Err(e) => {
                return fail(
                    &config_file,
                    None,
                    format!("Failed to read config file: {}", e),
                )
            }
        };

        let mut config: Value = match serde_yaml::from_str(&content) {
            Ok(c) => c,
            Err(e) => return fail(&config_file, None, format!("Failed to parse YAML: {}", e)),
        };

        // Validate, then back up. See the docstring: a refused write must leave
        // no trace on a partition the caller can reach without a rate limit.
        if let Err(e) = edit(&mut config) {
            return fail(&config_file, None, e);
        }

        let backup_path = self.create_backup(&config_file, &content);
        let backup_path_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        let new_content = match serde_yaml::to_string(&config) {
            Ok(c) => c,
            Err(e) => {
                return fail(
                    &config_file,
                    backup_path_str,
                    format!("Failed to serialize YAML: {}", e),
                )
            }
        };

        if let Err(e) = self.write_atomic(&config_file, &new_content) {
            if let Some(backup) = &backup_path {
                let _ = self.rollback(&config_file, backup);
            }
            return fail(
                &config_file,
                backup_path_str,
                format!("Failed to write config: {}", e),
            );
        }

        // Fire-and-forget, as everywhere else here: the on-disk change has
        // already succeeded, so a storage thread that is not wired up (tests,
        // early boot) logs and does not roll anything back.
        if let Some(storage) = self.storage.as_ref() {
            if let Err(e) = storage.log_audit_event(
                audit_action.to_string(),
                Some("config".to_string()),
                audit_details,
            ) {
                eprintln!(
                    "[ConfigApplier] audit log_audit_event({}) failed: {}",
                    audit_action, e
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

    /// Record the Hub's runtime radio mode — `radio.mode` of this file, the key
    /// the renderer reads over the hardware default and the device overlay
    /// (`plan/control/radio/node_registry.md` §11 amended R7, §12). Written by
    /// `libs::radio_mode::apply` only, for the signed `radio-set-mode` and BLE
    /// FB1B alike; the restart that applies it is the caller's.
    ///
    /// `mode` must already be one of `off`/`lorawan`/`p2p`/`dual`
    /// (`control_radio::setmode::parse_mode`); it is re-checked here so no
    /// caller can put a value in the file that fails the next render. Only
    /// `radio.mode` is touched: anything else under `radio:` rides through, and
    /// no struct in this crate models the section, so a key a newer build adds
    /// is not dropped by an older one's next write. `from` and `origin` (the
    /// surface that asked: `signed-mqtt`, `ble`) go into the audit row only.
    pub fn apply_radio_mode(&self, mode: &str, from: &str, origin: &str) -> ApplyResult {
        let details =
            serde_json::json!({ "mode": mode, "from": from, "origin": origin }).to_string();
        let result = self.edit_config_yaml("SET_RADIO_MODE", Some(details), |config| {
            if control_radio::setmode::parse_mode(mode).is_err() {
                return Err(format!(
                    "{mode:?} is not a radio mode: expected off, lorawan, p2p or dual"
                ));
            }
            let map = config
                .as_mapping_mut()
                .ok_or_else(|| "Config root is not a mapping".to_string())?;
            let radio = map
                .entry(Value::String("radio".to_string()))
                .or_insert_with(|| Value::Mapping(serde_yaml::Mapping::new()));
            if radio.is_null() {
                *radio = Value::Mapping(serde_yaml::Mapping::new());
            }
            let radio = radio
                .as_mapping_mut()
                .ok_or_else(|| "`radio:` is not a mapping".to_string())?;
            radio.insert(
                Value::String("mode".to_string()),
                Value::String(mode.to_string()),
            );
            Ok(())
        });
        if result.success {
            eprintln!("[ConfigApplier] ✓ Radio mode set to {mode} (was {from})");
        }
        result
    }

    /// `radio.mode` as this file holds it: the mode chosen at provisioning
    /// (BLE FB1B) or by `radio-set-mode`, verbatim. `Ok(None)` when nothing
    /// chose one — no `radio:` section, or no `mode:` in it — and the Hub runs
    /// the overlay's mode or its hardware default (node_registry.md §12).
    ///
    /// Read through the file rather than a cached `Config`, as FB16 reads
    /// `setup:`: a write from the other surface lands while this process runs.
    /// An unreadable or unparseable file is `Err`, never "nothing chosen".
    pub fn chosen_radio_mode(&self) -> Result<Option<String>, String> {
        let config_file = self.config_dir.join("fiber.config.yaml");
        let text = fs::read_to_string(&config_file)
            .map_err(|e| format!("cannot read {}: {e}", config_file.display()))?;
        let doc: Value = serde_yaml::from_str(&text)
            .map_err(|e| format!("cannot parse {}: {e}", config_file.display()))?;
        match doc.get("radio").and_then(|r| r.get("mode")) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(mode)) => Ok(Some(mode.clone())),
            Some(other) => Ok(Some(
                serde_yaml::to_string(other)
                    .unwrap_or_default()
                    .trim_end()
                    .to_string(),
            )),
        }
    }

    /// Mark setup complete and record the role — the `setup:` section of
    /// `plan/control/ble/peripheral.md` §6.3, written by BLE FB17 and by the
    /// signed `setup-complete` command through the same seam.
    ///
    /// Idempotent by construction: it writes the state the caller asked for
    /// rather than toggling, so the app re-running its wizard finish (a retried
    /// write, a second phone) lands on the same file. Clearing it is not
    /// offered here — decision #4 gives that job to the factory reset, which
    /// takes the whole file, and an "un-provision" write would be a way to lie
    /// about a Hub that is in fact configured.
    ///
    /// **The role is fixed once `provisioned: true`.** Re-writing the same role
    /// stays idempotent; asking for a *different* one is refused, on every
    /// surface that reaches this method (BLE FB17, the signed `setup-complete`
    /// command). Until the 2026-09-16 review the last write simply won, and that
    /// made FB19's "client Hubs may not read the server's connection info" gate
    /// a formality: the same authenticated peer could write FB17 `server` and
    /// then read the credential it had just been refused. A role change is also
    /// not a config edit in the ordinary sense — it decides whether the portal
    /// runs at all — so `peripheral.md` §6.3's documented way back is the
    /// factory reset, and the refusal says so rather than leaving the app to
    /// guess.
    ///
    /// A role the *file* carries that this build does not know still counts as
    /// a role: a stale value must not be the one thing that makes a provisioned
    /// Hub re-rolable.
    ///
    /// **The role must be the one the Hub runs** (`plan/control/cluster.md` §3,
    /// milestone C4). `client` is accepted only when the role marker in
    /// `/data/proximos/cluster/role` says `client` — a Hub becomes a Client by
    /// joining a Server (BLE FB1C, `proximosctl control.radio create --role
    /// client`), and the join writes this section itself. `server` is refused
    /// on a Hub whose marker says `client`. Otherwise a Hub could claim a role
    /// whose service set it does not run. On a joined Hub, `client` again is the
    /// idempotent same-role write.
    pub fn apply_setup_complete(&self, role: SetupRole) -> ApplyResult {
        let details = format!(r#"{{"provisioned":true,"role":"{}"}}"#, role.as_str());
        let marker = crate::libs::cluster_state::ClusterDir::new(&self.cluster_dir).is_client();
        let result = self.edit_config_yaml("SET_SETUP_COMPLETE", Some(details), |config| {
            let joined = marker.map_err(|e| {
                format!("cannot read the cluster role marker, so the role cannot be checked: {e}")
            })?;
            match (role, joined) {
                (SetupRole::Client, false) => {
                    return Err(
                        "this Hub has not joined a Server: a Hub becomes a client only by \
                                joining one (the Manager app's cluster join, or proximosctl \
                                control.radio create --role client), which completes setup \
                                itself"
                            .to_string(),
                    )
                }
                (SetupRole::Server, true) => {
                    return Err(
                        "this Hub has joined a Server as a client and cannot be set up \
                                as \"server\"; a factory reset is the only way back"
                            .to_string(),
                    )
                }
                _ => {}
            }
            let map = config
                .as_mapping_mut()
                .ok_or_else(|| "Config root is not a mapping".to_string())?;

            let setup = map
                .entry(Value::String("setup".to_string()))
                .or_insert_with(|| Value::Mapping(serde_yaml::Mapping::new()));

            // A `setup:` key that is present but null (the shape an operator
            // gets by deleting the two lines under it) is a mapping waiting to
            // happen, not an error worth refusing a setup over.
            if setup.is_null() {
                *setup = Value::Mapping(serde_yaml::Mapping::new());
            }
            let setup = setup
                .as_mapping_mut()
                .ok_or_else(|| "`setup:` is not a mapping".to_string())?;

            // Read the existing state before writing over it. Both halves are
            // required: a Hub provisioned with no role at all (a hand-edited
            // file, or one written before the role existed) is still free to
            // pick one, because there is no earlier choice to contradict.
            let already_provisioned = setup
                .get(&Value::String("provisioned".to_string()))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let existing_role = setup
                .get(&Value::String("role".to_string()))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty());
            if already_provisioned {
                if let Some(existing) = existing_role {
                    if existing != role.as_str() {
                        return Err(format!(
                            "the role is fixed once setup is complete: this Hub is provisioned \
                             as \"{existing}\" and cannot be re-roled to \"{}\"; a factory reset \
                             is the only way back",
                            role.as_str()
                        ));
                    }
                }
            }

            setup.insert(Value::String("provisioned".to_string()), Value::Bool(true));
            setup.insert(
                Value::String("role".to_string()),
                Value::String(role.as_str().to_string()),
            );
            Ok(())
        });

        if result.success {
            eprintln!(
                "[ConfigApplier] ✓ Setup marked complete, role {}",
                role.as_str()
            );
        }
        result
    }

    /// `setup:` as this file holds it: `(provisioned, role)`, read through the
    /// file like FB16. An absent section is `(false, None)`; an unreadable or
    /// unparseable file is `Err`, never "not provisioned".
    pub fn setup_status(&self) -> Result<(bool, Option<String>), String> {
        let config_file = self.config_dir.join("fiber.config.yaml");
        let text = fs::read_to_string(&config_file)
            .map_err(|e| format!("cannot read {}: {e}", config_file.display()))?;
        let doc: Value = serde_yaml::from_str(&text)
            .map_err(|e| format!("cannot parse {}: {e}", config_file.display()))?;
        let Some(setup) = doc.get("setup").filter(|s| !s.is_null()) else {
            return Ok((false, None));
        };
        let provisioned = setup
            .get("provisioned")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let role = setup
            .get("role")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Ok((provisioned, role))
    }

    /// The cluster join's setup half (`plan/control/cluster.md` §8.3 step 5):
    /// `setup: {provisioned: true, role: client}`, written by the join and by
    /// nothing else, with one `CLUSTER_JOIN` audit row naming the surface
    /// (`ble`, `cli`) and the Server. The peer password is never an argument
    /// here, so it cannot reach the row.
    ///
    /// Refused when setup is already complete, whatever the role: the join's
    /// own precondition, re-checked against the file in the same write so two
    /// surfaces racing cannot both land. It does not consult the role marker:
    /// the join writes the marker just before this (decided 2026-10-02), and
    /// [`crate::libs::cluster_join::complete_interrupted_join`] calls it on a
    /// Hub whose join stopped between the two.
    pub fn apply_cluster_join_setup(
        &self,
        origin: &str,
        server_host: &str,
        peer_username: &str,
    ) -> ApplyResult {
        let details = serde_json::json!({
            "origin": origin,
            "server_host": server_host,
            "peer_username": peer_username,
            "provisioned": true,
            "role": SetupRole::Client.as_str(),
        })
        .to_string();
        let result = self.edit_config_yaml(
            crate::libs::cluster_join::AUDIT_JOIN,
            Some(details),
            |config| {
                let map = config
                    .as_mapping_mut()
                    .ok_or_else(|| "Config root is not a mapping".to_string())?;
                let setup = or_insert_mapping(map, "setup")?;
                if setup
                    .get(&Value::String("provisioned".to_string()))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    return Err(
                        "setup is already complete on this Hub; a factory reset is the \
                                only way to make it a client"
                            .to_string(),
                    );
                }
                setup.insert(Value::String("provisioned".to_string()), Value::Bool(true));
                setup.insert(
                    Value::String("role".to_string()),
                    Value::String(SetupRole::Client.as_str().to_string()),
                );
                Ok(())
            },
        );
        if result.success {
            eprintln!("[ConfigApplier] ✓ Setup completed by the cluster join ({origin})");
        }
        result
    }

    /// The one `mqtt.export.destinations[]` entry BLE FB18 and the signed
    /// `export-destination-set` command own.
    ///
    /// Named rather than positional because `DestinationConfig` requires a
    /// `broker_id` that the ask's payload has no room for, and appending an
    /// unnamed entry per write would grow the list without bound. `remote` is
    /// the name the shipped config's own comment already gives the opt-in
    /// store-and-forward destination; `local` is the image's own and is never
    /// touched by this path.
    pub const EXPORT_DESTINATION_ID: &'static str = "remote";

    /// Where the server Hub's CA lands once something fetches it.
    ///
    /// Stated here rather than left to the caller so that the config this
    /// method writes is complete: `tls.enabled` with an empty `ca_cert_path`
    /// would be a destination that can never connect and does not say why.
    /// Nothing on a stock v2 Hub creates this file yet — the TLS listener it
    /// belongs to is `plan/portal/devices.hub.md` §12 row H — so a destination
    /// written today is configuration, not a working export, and the BLE
    /// result says so rather than implying otherwise.
    pub const EXPORT_CA_PATH: &'static str = "/data/tls/export-ca.crt";

    /// Write the export destination `plan/control/ble/peripheral.md` §6.4 describes.
    ///
    /// Three rules that are decisions, not implementation details:
    ///
    /// - **A blank username with a password is refused.** `mqtt_export`'s
    ///   destination only applies credentials when the username is non-empty, so
    ///   a half-filled pair silently becomes an *anonymous* export — the Hub
    ///   connecting as nobody to a broker the operator believed it had
    ///   authenticated to. The app already refuses this; so does the Hub. It is a
    ///   rule about this write only: the image ships an anonymous `local`
    ///   destination, which is legitimate and is not re-judged here.
    /// - **The port is not a parameter.** 8883 over TLS is the settled client
    ///   contract, so there is nothing for a caller to get wrong.
    /// - **Enabling the destination enables the export block in the same atomic
    ///   write**, the way beacon enrollment flips `eye.enabled`: a destination
    ///   enabled inside a disabled export is a setting that looks applied and
    ///   does nothing.
    pub fn apply_export_destination(
        &self,
        host: &str,
        username: &str,
        password: &str,
        enabled: bool,
    ) -> ApplyResult {
        let host = host.trim().to_string();
        let username = username.trim().to_string();

        let details = format!(
            r#"{{"broker_id":"{}","host":{:?},"enabled":{},"anonymous":{}}}"#,
            Self::EXPORT_DESTINATION_ID,
            host,
            enabled,
            username.is_empty()
        );

        let result = self.edit_config_yaml("SET_EXPORT_DESTINATION", Some(details), |config| {
            if host.is_empty() {
                return Err("host is empty".to_string());
            }
            if username.is_empty() && !password.is_empty() {
                return Err(
                    "a password without a username would enrol an anonymous export".to_string(),
                );
            }

            let root = config
                .as_mapping_mut()
                .ok_or_else(|| "Config root is not a mapping".to_string())?;
            let mqtt = or_insert_mapping(root, "mqtt")?;
            let export = or_insert_mapping(mqtt, "export")?;

            if enabled {
                export.insert(Value::String("enabled".to_string()), Value::Bool(true));
            }

            let destinations = export
                .entry(Value::String("destinations".to_string()))
                .or_insert_with(|| Value::Sequence(Vec::new()));
            if destinations.is_null() {
                *destinations = Value::Sequence(Vec::new());
            }
            let destinations = destinations
                .as_sequence_mut()
                .ok_or_else(|| "`mqtt.export.destinations` is not a list".to_string())?;

            let mut entry = serde_yaml::Mapping::new();
            entry.insert(
                Value::String("broker_id".to_string()),
                Value::String(Self::EXPORT_DESTINATION_ID.to_string()),
            );
            entry.insert(Value::String("enabled".to_string()), Value::Bool(enabled));
            entry.insert(
                Value::String("host".to_string()),
                Value::String(host.clone()),
            );
            entry.insert(
                Value::String("port".to_string()),
                Value::Number(EXPORT_TLS_PORT.into()),
            );
            entry.insert(
                Value::String("username".to_string()),
                Value::String(username.clone()),
            );
            entry.insert(
                Value::String("password".to_string()),
                Value::String(password.to_string()),
            );
            let mut tls = serde_yaml::Mapping::new();
            tls.insert(Value::String("enabled".to_string()), Value::Bool(true));
            tls.insert(
                Value::String("ca_cert_path".to_string()),
                Value::String(Self::EXPORT_CA_PATH.to_string()),
            );
            tls.insert(
                Value::String("insecure_skip_verify".to_string()),
                Value::Bool(false),
            );
            entry.insert(Value::String("tls".to_string()), Value::Mapping(tls));

            // Replace in place rather than remove-and-push: the order of this list is what an
            // operator reading the file sees, and a destination that jumps to the bottom on every
            // edit makes a diff of two Hubs unreadable.
            let existing = destinations.iter().position(|d| {
                d.get("broker_id").and_then(|v| v.as_str()) == Some(Self::EXPORT_DESTINATION_ID)
            });
            match existing {
                Some(i) => destinations[i] = Value::Mapping(entry),
                None => destinations.push(Value::Mapping(entry)),
            }
            Ok(())
        });

        if result.success {
            eprintln!(
                "[ConfigApplier] ✓ Export destination {} -> {}:{} (enabled={})",
                Self::EXPORT_DESTINATION_ID,
                host,
                EXPORT_TLS_PORT,
                enabled
            );
        }
        result
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
        let backup_path_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

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
            eprintln!(
                "[ConfigApplier] ✓ Display lines updated ({} lines)",
                lines.len()
            );
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
        let backup_path_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

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
        let backup_path_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

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
            if let Err(e) = storage.append_node_removed(dev_eui.clone(), applied_at) {
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
        let backup_path_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

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
    /// gateway is not a save-and-feed node).
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
        let backup_path_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

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
                    error_message: Some(format!(
                        "Gateway with gateway_eui '{}' not found",
                        gateway_eui
                    )),
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
    pub fn apply_beacon_tag_config(&self, mac: String, name: Option<String>) -> ApplyResult {
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
        let backup_path_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        let created = match self.update_beacon_tag_config(&mut config, &mac, name.as_deref()) {
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

    /// Add (or update) a Beacon tag **and** turn the subsystem on, in one write.
    ///
    /// EDIT (control-ble integration, AC 4). Enrolling the first Beacon on a Hub whose
    /// `eye.enabled` is false is two edits. Doing them as two `apply_*` calls would write the file
    /// twice, leaving a window in which the tag exists and the subsystem is off, and would put two
    /// rows in the audit trail for one operator action. Both edits go into one `Value` and one
    /// `write_atomic`, and the trail gets one `beacon-add` row — the v2 command vocabulary
    /// (plan.md §9.8), not v1's `ADD_EYE_TAG`.
    ///
    /// `enabled` is written unconditionally rather than only when it was false: the caller has
    /// already established that this is the add-and-enable case, and a redundant `true` costs
    /// nothing while a read-modify-check here would race the very write it is inside.
    pub fn apply_beacon_tag_config_and_enable(
        &self,
        mac: String,
        name: Option<String>,
    ) -> ApplyResult {
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
        let backup_path_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        // Both edits, then one serialise, then one write.
        if let Err(e) = self.update_beacon_tag_config(&mut config, &mac, name.as_deref()) {
            return ApplyResult {
                success: false,
                file_path: config_file.to_string_lossy().to_string(),
                backup_path: backup_path_str,
                error_message: Some(e),
                applied_at,
            };
        }
        if let Err(e) = Self::update_beacon_enabled_config(&mut config, true) {
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
            "[ConfigApplier] ✓ Beacon {} enrolled and eye.enabled set",
            mac
        );

        self.log_audit(
            "beacon-add",
            format!(
                r#"{{"mac":{:?},"name":{:?},"enabled_flipped":true}}"#,
                mac, name
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

    /// Write one audit row from outside the applier.
    ///
    /// EDIT (control-ble integration): `control_ble::host::ConfigStore::audit` reports actions the
    /// crate performed itself — an enrolment that changed nothing, a removal of a tag that was only
    /// discovered — which never reach an `apply_*` method and so would otherwise leave no trail.
    /// The private `log_audit` is unchanged; this is the public door onto it.
    pub fn log_audit_public(&self, operation: &str, details: String) {
        self.log_audit(operation, details);
    }

    /// Persist an EYE tag's recording on/off + interval into `eye.tags[mac]`.
    /// `interval_min == 0` disables recording so it survives a restart and the
    /// gap/fallback sync stops re-enabling it (H1). The tag must already exist.
    pub fn apply_beacon_recording(&self, mac: String, interval_min: u16) -> ApplyResult {
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
        let backup_path_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        if let Err(e) = self.update_beacon_recording_config(&mut config, &mac, interval_min) {
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
            if interval_min == 0 {
                "off".to_string()
            } else {
                format!("{interval_min}min")
            },
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

    /// Switch the EYE BLE tag subsystem on or off (`eye.enabled`).
    ///
    /// The subsystem had no command at all: `set_eye_recording`, `add_eye_tag`
    /// and friends all operate on tags, so once `eye.enabled` was false the only
    /// way back was SSH. Creates the `eye:` section if it is missing — serde
    /// fills the rest from `BeaconConfig`.
    ///
    /// Takes effect on the next start of the `fiber` service: the EYE monitor
    /// thread is spawned once at boot, so flipping the flag cannot start it.
    pub fn apply_beacon_enabled(&self, enabled: bool) -> ApplyResult {
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
        let backup_path_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        if let Err(e) = Self::update_beacon_enabled_config(&mut config, enabled) {
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
            "[ConfigApplier] \u{2713} EYE subsystem {}",
            if enabled { "enabled" } else { "disabled" }
        );
        self.log_audit("SET_EYE_ENABLED", format!(r#"{{"enabled":{}}}"#, enabled));

        ApplyResult {
            success: true,
            file_path: config_file.to_string_lossy().to_string(),
            backup_path: backup_path_str,
            error_message: None,
            applied_at,
        }
    }

    /// Set `eye.enabled`, creating the section when absent.
    fn update_beacon_enabled_config(config: &mut Value, enabled: bool) -> Result<(), String> {
        let config_map = config
            .as_mapping_mut()
            .ok_or_else(|| "Config root is not a mapping".to_string())?;
        let eye_key = Value::String("eye".to_string());
        let enabled_key = Value::String("enabled".to_string());

        match config_map
            .get_mut(&eye_key)
            .and_then(|v| v.as_mapping_mut())
        {
            Some(eye) => {
                eye.insert(enabled_key, Value::Bool(enabled));
            }
            None => {
                let mut eye = serde_yaml::Mapping::new();
                eye.insert(enabled_key, Value::Bool(enabled));
                config_map.insert(eye_key, Value::Mapping(eye));
            }
        }
        Ok(())
    }

    /// Toggle `eye.auto_provision` / `eye.auto_discover` at runtime. Each `Some`
    /// field overwrites the corresponding key; a `None` field is left untouched
    /// (mirrors `apply_beacon_enabled`, but per-field rather than whole-value).
    /// Creates the `eye:` section if missing.
    pub fn apply_beacon_config(
        &self,
        auto_provision: Option<bool>,
        auto_discover: Option<bool>,
    ) -> ApplyResult {
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
        let backup_path_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        if let Err(e) = Self::update_beacon_config_flags(&mut config, auto_provision, auto_discover)
        {
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
            "[ConfigApplier] \u{2713} EYE config: auto_provision={:?} auto_discover={:?}",
            auto_provision, auto_discover
        );
        self.log_audit(
            "SET_EYE_CONFIG",
            format!(
                r#"{{"auto_provision":{},"auto_discover":{}}}"#,
                auto_provision
                    .map(|b| b.to_string())
                    .unwrap_or_else(|| "null".to_string()),
                auto_discover
                    .map(|b| b.to_string())
                    .unwrap_or_else(|| "null".to_string()),
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

    /// Set the `Some` top-level `eye:` booleans (`auto_provision` /
    /// `auto_discover`). Creates the `eye:` mapping if it is absent (unlike the
    /// per-tag helpers, which require an existing tag) so a config omitting
    /// `eye:` still works.
    ///
    /// `auto_discover` is written as a real YAML key rather than being left
    /// implicit: `BeaconConfig::auto_discover` is `Option<bool>` precisely so an
    /// unset key round-trips as unset, but an operator flipping the toggle is an
    /// explicit decision and must be recorded as `Some(_)` on disk.
    fn update_beacon_config_flags(
        config: &mut Value,
        auto_provision: Option<bool>,
        auto_discover: Option<bool>,
    ) -> Result<(), String> {
        let config_map = config
            .as_mapping_mut()
            .ok_or_else(|| "Config root is not a mapping".to_string())?;
        let eye_key = Value::String("eye".to_string());

        if !config_map
            .get(&eye_key)
            .map(|v| v.is_mapping())
            .unwrap_or(false)
        {
            config_map.insert(eye_key.clone(), Value::Mapping(serde_yaml::Mapping::new()));
        }
        let eye = config_map
            .get_mut(&eye_key)
            .and_then(|v| v.as_mapping_mut())
            .ok_or_else(|| "eye config is not a mapping".to_string())?;

        if let Some(v) = auto_provision {
            eye.insert(Value::String("auto_provision".to_string()), Value::Bool(v));
        }
        if let Some(v) = auto_discover {
            eye.insert(Value::String("auto_discover".to_string()), Value::Bool(v));
        }
        Ok(())
    }

    /// Remove an EYE BLE tag from the main config (`eye.tags[]`) by MAC.
    pub fn remove_beacon_tag_config(&self, mac: String) -> ApplyResult {
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
        let backup_path_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

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
        hysteresis: Option<f32>,
        enabled: bool,
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
        // The portal's hysteresis (decision #41) as the line's own; absent keeps what it has.
        if let Some(h) = hysteresis {
            line_map.insert(
                Value::String("hysteresis_celsius".to_string()),
                Value::Number(serde_yaml::Number::from(h as f64)),
            );
        }
        // The band's switch (#167); the bounds above are kept either way.
        line_map.insert(
            Value::String("limits_enabled".to_string()),
            Value::Bool(enabled),
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
    fn update_line_location(
        &self,
        config: &mut Value,
        line: u8,
        location: &str,
    ) -> Result<(), String> {
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

    /// Update sensor intervals in the main YAML config structure. Writes the
    /// `_s` keys and drops the pre-2026-10-12 `_ms` spellings, so the file
    /// never carries two values for one setting.
    fn update_sensor_intervals(
        &self,
        config: &mut Value,
        sample_interval_s: u64,
        aggregation_interval_s: u64,
    ) -> Result<(), String> {
        let sensors = config
            .get_mut("sensors")
            .and_then(|v| v.as_mapping_mut())
            .ok_or_else(|| "Missing 'sensors' section in config".to_string())?;

        for legacy in [
            "sample_interval_ms",
            "aggregation_interval_ms",
            "report_interval_ms",
        ] {
            sensors.remove(Value::String(legacy.to_string()));
        }
        sensors.insert(
            Value::String("sample_interval_s".to_string()),
            Value::Number(serde_yaml::Number::from(sample_interval_s)),
        );
        sensors.insert(
            Value::String("aggregation_interval_s".to_string()),
            Value::Number(serde_yaml::Number::from(aggregation_interval_s)),
        );

        Ok(())
    }

    /// Update system info interval in the MQTT section of main config.
    ///
    /// Writes `mqtt.publish.intervals.system_info_sec`, which is the key the
    /// deserializer actually reads (`config::PublishIntervals`). This used to
    /// write a flat `mqtt.system_info_interval_seconds` — a key nothing in the
    /// codebase ever read — so `set_system_info_interval` reported success,
    /// rewrote the config file, and changed nothing.
    fn update_system_info_interval(
        &self,
        config: &mut Value,
        interval_seconds: u64,
    ) -> Result<(), String> {
        let mqtt = config
            .get_mut("mqtt")
            .and_then(|v| v.as_mapping_mut())
            .ok_or_else(|| "Missing 'mqtt' section in config".to_string())?;

        // Drop the stale flat key first, so a config written by an older build
        // stops carrying a value that looks authoritative but is inert.
        mqtt.remove(Value::String("system_info_interval_seconds".to_string()));

        let publish_key = Value::String("publish".to_string());
        if !mqtt
            .get(&publish_key)
            .map(|v| v.is_mapping())
            .unwrap_or(false)
        {
            mqtt.insert(
                publish_key.clone(),
                Value::Mapping(serde_yaml::Mapping::new()),
            );
        }
        let publish = mqtt
            .get_mut(&publish_key)
            .and_then(|v| v.as_mapping_mut())
            .ok_or_else(|| "Could not access 'mqtt.publish' section".to_string())?;

        let intervals_key = Value::String("intervals".to_string());
        if !publish
            .get(&intervals_key)
            .map(|v| v.is_mapping())
            .unwrap_or(false)
        {
            publish.insert(
                intervals_key.clone(),
                Value::Mapping(serde_yaml::Mapping::new()),
            );
        }
        let intervals = publish
            .get_mut(&intervals_key)
            .and_then(|v| v.as_mapping_mut())
            .ok_or_else(|| "Could not access 'mqtt.publish.intervals' section".to_string())?;

        intervals.insert(
            Value::String("system_info_sec".to_string()),
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
    fn apply_system_field_u8_change(
        &self,
        field_name: &str,
        value: u8,
        display_name: &str,
    ) -> ApplyResult {
        self.apply_system_field_num_change(field_name, u64::from(value), display_name, "%")
    }

    /// Update a `u32` field in the `system` section (e.g. `screen_timeout_secs`,
    /// which exceeds a `u8`; logged without a `%` suffix).
    fn apply_system_field_u32_change(
        &self,
        field_name: &str,
        value: u32,
        display_name: &str,
    ) -> ApplyResult {
        self.apply_system_field_num_change(field_name, u64::from(value), display_name, "")
    }

    /// Shared implementation for numeric `system` fields: atomic YAML rewrite with
    /// timestamped backup, rollback on failure, and an audit log entry. `unit` is
    /// appended to the success log line (e.g. `"%"` for percentages, `""` otherwise).
    fn apply_system_field_num_change(
        &self,
        field_name: &str,
        value: u64,
        display_name: &str,
        unit: &str,
    ) -> ApplyResult {
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
        let backup_path_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        // Update field in system section
        if let Some(system) = config.get_mut("system").and_then(|v| v.as_mapping_mut()) {
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

        eprintln!(
            "[ConfigApplier] ✓ {} updated: {}{}",
            display_name, value, unit
        );

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
            lorawan.insert(
                Value::String("sensors".to_string()),
                Value::Sequence(Vec::new()),
            );
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
            sensor_map.insert(
                Value::String("name".to_string()),
                Value::String(n.to_string()),
            );
        }
        if let Some(sn) = serial_number {
            sensor_map.insert(
                Value::String("serial_number".to_string()),
                Value::String(sn.to_string()),
            );
        }
        if let Some(loc) = location {
            sensor_map.insert(
                Value::String("location".to_string()),
                Value::String(loc.to_string()),
            );
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
            lorawan.insert(
                Value::String("gateways".to_string()),
                Value::Sequence(Vec::new()),
            );
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
            gateway_map.insert(
                Value::String("name".to_string()),
                Value::String(n.to_string()),
            );
        }

        Ok(())
    }

    /// Upsert a per-field threshold inside lorawan.sensors[*].field_thresholds.
    /// Upsert an EYE tag entry in `eye.tags[]` (get-or-create the `eye` section
    /// and `tags` array, then match on `mac` case-insensitively). `mac` is
    /// expected already uppercased by the caller.
    /// Set `recording` (+ `logging_interval_min` when on) on an existing
    /// `eye.tags[mac]` entry. Errors if the tag is not present.
    fn update_beacon_recording_config(
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

    fn update_beacon_tag_config(
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
            eye.insert(
                Value::String("tags".to_string()),
                Value::Sequence(Vec::new()),
            );
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
            tag_map.insert(
                Value::String("name".to_string()),
                Value::String(n.to_string()),
            );
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
        // False switches the alarm OFF for this field, recorded separately from
        // the bounds in `disarmed_fields` — an omitted bound inherits the YAML
        // default, so clearing the numbers cannot express "off".
        enabled: bool,
    ) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        if dev_eui.is_empty() {
            return ApplyResult {
                success: false,
                file_path: String::new(),
                backup_path: None,
                error_message: Some("dev_eui cannot be empty".into()),
                applied_at,
            };
        }
        let config_file = self.config_dir.join("fiber.config.yaml");
        let content = match fs::read_to_string(&config_file) {
            Ok(c) => c,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().into(),
                    backup_path: None,
                    error_message: Some(format!("Failed to read config: {}", e)),
                    applied_at,
                }
            }
        };
        let mut cfg: Value = match serde_yaml::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().into(),
                    backup_path: None,
                    error_message: Some(format!("Failed to parse YAML: {}", e)),
                    applied_at,
                }
            }
        };
        let backup_path = self.create_backup(&config_file, &content);
        let backup_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        if let Err(e) = self.upsert_field_threshold(
            &mut cfg,
            &dev_eui,
            &field,
            critical_low,
            warning_low,
            warning_high,
            critical_high,
            enabled,
        ) {
            return ApplyResult {
                success: false,
                file_path: config_file.to_string_lossy().into(),
                backup_path: backup_str,
                error_message: Some(e),
                applied_at,
            };
        }

        let new_content = match serde_yaml::to_string(&cfg) {
            Ok(c) => c,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().into(),
                    backup_path: backup_str,
                    error_message: Some(e.to_string()),
                    applied_at,
                }
            }
        };
        // Atomic write + rollback on failure, matching every other apply_*
        // method. fs::write directly would truncate the YAML mid-write on
        // power loss, leaving the device with an unparseable config — and
        // there's no rollback path back to the backup we just took.
        if let Err(e) = self.write_atomic(&config_file, &new_content) {
            if let Some(b) = backup_path.as_ref() {
                let _ = self.rollback(&config_file, b);
            }
            return ApplyResult {
                success: false,
                file_path: config_file.to_string_lossy().into(),
                backup_path: backup_str,
                error_message: Some(e),
                applied_at,
            };
        }
        self.log_audit(
            "SET_LORAWAN_FIELD_THRESHOLD",
            format!(
                r#"{{"dev_eui":{:?},"field":{:?},"critical_low":{:?},"warning_low":{:?},"warning_high":{:?},"critical_high":{:?},"enabled":{}}}"#,
                dev_eui, field, critical_low, warning_low, warning_high, critical_high, enabled,
            ),
        );
        ApplyResult {
            success: true,
            file_path: config_file.to_string_lossy().into(),
            backup_path: backup_str,
            error_message: None,
            applied_at,
        }
    }

    /// Remove a per-field threshold from lorawan.sensors[*].field_thresholds.
    pub fn delete_lorawan_field_threshold(&self, dev_eui: String, field: String) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let config_file = self.config_dir.join("fiber.config.yaml");
        let content = match fs::read_to_string(&config_file) {
            Ok(c) => c,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().into(),
                    backup_path: None,
                    error_message: Some(format!("Failed to read config: {}", e)),
                    applied_at,
                }
            }
        };
        let mut cfg: Value = match serde_yaml::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().into(),
                    backup_path: None,
                    error_message: Some(format!("Failed to parse YAML: {}", e)),
                    applied_at,
                }
            }
        };
        let backup_path = self.create_backup(&config_file, &content);
        let backup_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());

        // Surface whether anything was actually removed so callers can tell
        // a real delete from a misspelled-dev_eui/no-op. The YAML rewrite
        // still happens either way because re-serialising is harmless.
        let removed = self
            .remove_field_threshold(&mut cfg, &dev_eui, &field)
            .unwrap_or(false);
        let new_content = match serde_yaml::to_string(&cfg) {
            Ok(c) => c,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().into(),
                    backup_path: backup_str,
                    error_message: Some(e.to_string()),
                    applied_at,
                }
            }
        };
        if let Err(e) = self.write_atomic(&config_file, &new_content) {
            if let Some(b) = backup_path.as_ref() {
                let _ = self.rollback(&config_file, b);
            }
            return ApplyResult {
                success: false,
                file_path: config_file.to_string_lossy().into(),
                backup_path: backup_str,
                error_message: Some(e),
                applied_at,
            };
        }
        if !removed {
            eprintln!(
                "[ConfigApplier] delete_lorawan_field_threshold: no threshold matched (dev_eui={}, field={}) — wrote yaml anyway",
                dev_eui, field,
            );
        }
        self.log_audit(
            "DELETE_LORAWAN_FIELD_THRESHOLD",
            format!(
                r#"{{"dev_eui":{:?},"field":{:?},"removed":{}}}"#,
                dev_eui, field, removed
            ),
        );
        ApplyResult {
            success: true,
            file_path: config_file.to_string_lossy().into(),
            backup_path: backup_str,
            error_message: None,
            applied_at,
        }
    }

    fn upsert_field_threshold(
        &self,
        config: &mut Value,
        dev_eui: &str,
        field: &str,
        critical_low: Option<f64>,
        warning_low: Option<f64>,
        warning_high: Option<f64>,
        critical_high: Option<f64>,
        enabled: bool,
    ) -> Result<(), String> {
        let lorawan_key = Value::String("lorawan".to_string());
        let sensors_key = Value::String("sensors".to_string());
        let ft_key = Value::String("field_thresholds".to_string());

        let lorawan = config
            .as_mapping_mut()
            .ok_or_else(|| "Config root is not a mapping".to_string())?
            .entry(lorawan_key.clone())
            .or_insert_with(|| {
                let mut m = Mapping::new();
                m.insert(Value::String("enabled".to_string()), Value::Bool(true));
                m.insert(sensors_key.clone(), Value::Sequence(Vec::new()));
                Value::Mapping(m)
            });
        let lorawan_map = lorawan
            .as_mapping_mut()
            .ok_or_else(|| "lorawan is not a mapping".to_string())?;
        let sensors = lorawan_map
            .entry(sensors_key.clone())
            .or_insert_with(|| Value::Sequence(Vec::new()))
            .as_sequence_mut()
            .ok_or_else(|| "sensors is not a sequence".to_string())?;

        // Find or create sensor entry
        let idx = sensors
            .iter()
            .position(|s| s.get("dev_eui").and_then(|v| v.as_str()) == Some(dev_eui));
        let sensor_map = if let Some(i) = idx {
            sensors[i]
                .as_mapping_mut()
                .ok_or_else(|| "sensor entry is not a mapping".to_string())?
        } else {
            let mut m = Mapping::new();
            m.insert(
                Value::String("dev_eui".to_string()),
                Value::String(dev_eui.to_string()),
            );
            m.insert(Value::String("enabled".to_string()), Value::Bool(true));
            sensors.push(Value::Mapping(m));
            sensors.last_mut().unwrap().as_mapping_mut().unwrap()
        };

        // The on/off flag lives beside the bounds, not inside them: a field can be
        // switched off while having no bounds of its own (the default-armed case
        // this exists for), and the key stays absent entirely while nothing is off,
        // so an untouched config is byte-identical to before.
        {
            let df_key = Value::String("disarmed_fields".to_string());
            let mut list: Vec<Value> = sensor_map
                .get(&df_key)
                .and_then(|v| v.as_sequence())
                .map(|s| {
                    s.iter()
                        .filter(|v| v.as_str() != Some(field))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            if !enabled {
                list.push(Value::String(field.to_string()));
            }
            if list.is_empty() {
                sensor_map.remove(&df_key);
            } else {
                sensor_map.insert(df_key, Value::Sequence(list));
            }
        }

        let thresholds = sensor_map
            .entry(ft_key.clone())
            .or_insert_with(|| Value::Sequence(Vec::new()))
            .as_sequence_mut()
            .ok_or_else(|| "field_thresholds is not a sequence".to_string())?;

        let make_entry = || -> Value {
            let mut m = Mapping::new();
            m.insert(
                Value::String("field".to_string()),
                Value::String(field.to_string()),
            );
            for (k, v) in [
                ("critical_low", critical_low),
                ("warning_low", warning_low),
                ("warning_high", warning_high),
                ("critical_high", critical_high),
            ] {
                if let Some(v) = v {
                    m.insert(
                        Value::String(k.to_string()),
                        Value::Number(serde_yaml::Number::from(v)),
                    );
                }
            }
            Value::Mapping(m)
        };

        if let Some(existing) = thresholds
            .iter_mut()
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
    fn remove_field_threshold(
        &self,
        config: &mut Value,
        dev_eui: &str,
        field: &str,
    ) -> Result<bool, String> {
        let lorawan = config
            .as_mapping_mut()
            .and_then(|m| m.get_mut(&Value::String("lorawan".into())))
            .and_then(|v| v.as_mapping_mut());
        let Some(lorawan_map) = lorawan else {
            return Ok(false);
        };
        let Some(sensors) = lorawan_map
            .get_mut(&Value::String("sensors".into()))
            .and_then(|v| v.as_sequence_mut())
        else {
            return Ok(false);
        };
        let mut removed = false;
        for s in sensors.iter_mut() {
            if s.get("dev_eui").and_then(|v| v.as_str()) != Some(dev_eui) {
                continue;
            }
            let sm = match s.as_mapping_mut() {
                Some(m) => m,
                None => continue,
            };
            if let Some(thresholds) = sm
                .get_mut(&Value::String("field_thresholds".into()))
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
    pub fn apply_beacon_field_threshold(
        &self,
        mac: String,
        field: String,
        critical_low: Option<f64>,
        warning_low: Option<f64>,
        warning_high: Option<f64>,
        critical_high: Option<f64>,
        hysteresis: Option<f64>,
    ) -> ApplyResult {
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
                error_message: Some("mac cannot be empty".into()),
                applied_at,
            };
        }
        let config_file = self.config_dir.join("fiber.config.yaml");
        let content = match fs::read_to_string(&config_file) {
            Ok(c) => c,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().into(),
                    backup_path: None,
                    error_message: Some(format!("Failed to read config: {}", e)),
                    applied_at,
                }
            }
        };
        let mut cfg: Value = match serde_yaml::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().into(),
                    backup_path: None,
                    error_message: Some(format!("Failed to parse YAML: {}", e)),
                    applied_at,
                }
            }
        };
        let backup_path = self.create_backup(&config_file, &content);
        let backup_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        if let Err(e) = self.upsert_beacon_field_threshold(
            &mut cfg,
            &mac,
            &field,
            critical_low,
            warning_low,
            warning_high,
            critical_high,
            hysteresis,
        ) {
            return ApplyResult {
                success: false,
                file_path: config_file.to_string_lossy().into(),
                backup_path: backup_str,
                error_message: Some(e),
                applied_at,
            };
        }
        let new_content = match serde_yaml::to_string(&cfg) {
            Ok(c) => c,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().into(),
                    backup_path: backup_str,
                    error_message: Some(e.to_string()),
                    applied_at,
                }
            }
        };
        if let Err(e) = self.write_atomic(&config_file, &new_content) {
            if let Some(b) = backup_path.as_ref() {
                let _ = self.rollback(&config_file, b);
            }
            return ApplyResult {
                success: false,
                file_path: config_file.to_string_lossy().into(),
                backup_path: backup_str,
                error_message: Some(e),
                applied_at,
            };
        }
        self.log_audit("SET_EYE_FIELD_THRESHOLD",
            format!(r#"{{"mac":{:?},"field":{:?},"critical_low":{:?},"warning_low":{:?},"warning_high":{:?},"critical_high":{:?},"hysteresis":{:?}}}"#,
                mac, field, critical_low, warning_low, warning_high, critical_high, hysteresis));
        ApplyResult {
            success: true,
            file_path: config_file.to_string_lossy().into(),
            backup_path: backup_str,
            error_message: None,
            applied_at,
        }
    }

    /// Remove a per-field EYE alarm threshold. Returns success even on no-op.
    pub fn delete_beacon_field_threshold(&self, mac: String, field: String) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let mac = mac.to_uppercase();
        let config_file = self.config_dir.join("fiber.config.yaml");
        let content = match fs::read_to_string(&config_file) {
            Ok(c) => c,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().into(),
                    backup_path: None,
                    error_message: Some(format!("Failed to read config: {}", e)),
                    applied_at,
                }
            }
        };
        let mut cfg: Value = match serde_yaml::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().into(),
                    backup_path: None,
                    error_message: Some(format!("Failed to parse YAML: {}", e)),
                    applied_at,
                }
            }
        };
        let backup_path = self.create_backup(&config_file, &content);
        let backup_str = backup_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        let removed = self
            .remove_beacon_field_threshold(&mut cfg, &mac, &field)
            .unwrap_or(false);
        let new_content = match serde_yaml::to_string(&cfg) {
            Ok(c) => c,
            Err(e) => {
                return ApplyResult {
                    success: false,
                    file_path: config_file.to_string_lossy().into(),
                    backup_path: backup_str,
                    error_message: Some(e.to_string()),
                    applied_at,
                }
            }
        };
        if let Err(e) = self.write_atomic(&config_file, &new_content) {
            if let Some(b) = backup_path.as_ref() {
                let _ = self.rollback(&config_file, b);
            }
            return ApplyResult {
                success: false,
                file_path: config_file.to_string_lossy().into(),
                backup_path: backup_str,
                error_message: Some(e),
                applied_at,
            };
        }
        self.log_audit(
            "DELETE_EYE_FIELD_THRESHOLD",
            format!(
                r#"{{"mac":{:?},"field":{:?},"removed":{}}}"#,
                mac, field, removed
            ),
        );
        ApplyResult {
            success: true,
            file_path: config_file.to_string_lossy().into(),
            backup_path: backup_str,
            error_message: None,
            applied_at,
        }
    }

    fn upsert_beacon_field_threshold(
        &self,
        config: &mut Value,
        mac: &str,
        field: &str,
        critical_low: Option<f64>,
        warning_low: Option<f64>,
        warning_high: Option<f64>,
        critical_high: Option<f64>,
        hysteresis: Option<f64>,
    ) -> Result<(), String> {
        let eye_key = Value::String("eye".to_string());
        let tags_key = Value::String("tags".to_string());
        let ft_key = Value::String("field_thresholds".to_string());
        let eye = config
            .as_mapping_mut()
            .ok_or_else(|| "Config root is not a mapping".to_string())?
            .entry(eye_key.clone())
            .or_insert_with(|| {
                let mut m = Mapping::new();
                m.insert(Value::String("enabled".to_string()), Value::Bool(true));
                m.insert(tags_key.clone(), Value::Sequence(Vec::new()));
                Value::Mapping(m)
            });
        let eye_map = eye
            .as_mapping_mut()
            .ok_or_else(|| "eye is not a mapping".to_string())?;
        let tags = eye_map
            .entry(tags_key.clone())
            .or_insert_with(|| Value::Sequence(Vec::new()))
            .as_sequence_mut()
            .ok_or_else(|| "tags is not a sequence".to_string())?;
        let idx = tags.iter().position(|t| {
            t.get("mac")
                .and_then(|v| v.as_str())
                .map(|m| m.to_uppercase() == mac)
                .unwrap_or(false)
        });
        let tag_map = if let Some(i) = idx {
            tags[i]
                .as_mapping_mut()
                .ok_or_else(|| "tag entry is not a mapping".to_string())?
        } else {
            let mut m = Mapping::new();
            m.insert(
                Value::String("mac".to_string()),
                Value::String(mac.to_string()),
            );
            m.insert(Value::String("enabled".to_string()), Value::Bool(true));
            tags.push(Value::Mapping(m));
            tags.last_mut().unwrap().as_mapping_mut().unwrap()
        };
        let thresholds = tag_map
            .entry(ft_key.clone())
            .or_insert_with(|| Value::Sequence(Vec::new()))
            .as_sequence_mut()
            .ok_or_else(|| "field_thresholds is not a sequence".to_string())?;
        let make_entry = || -> Value {
            let mut m = Mapping::new();
            m.insert(
                Value::String("field".to_string()),
                Value::String(field.to_string()),
            );
            for (k, v) in [
                ("critical_low", critical_low),
                ("warning_low", warning_low),
                ("warning_high", warning_high),
                ("critical_high", critical_high),
                ("hysteresis", hysteresis),
            ] {
                if let Some(v) = v {
                    m.insert(
                        Value::String(k.to_string()),
                        Value::Number(serde_yaml::Number::from(v)),
                    );
                }
            }
            Value::Mapping(m)
        };
        if let Some(existing) = thresholds
            .iter_mut()
            .find(|t| t.get("field").and_then(|v| v.as_str()) == Some(field))
        {
            *existing = make_entry();
        } else {
            thresholds.push(make_entry());
        }
        Ok(())
    }

    fn remove_beacon_field_threshold(
        &self,
        config: &mut Value,
        mac: &str,
        field: &str,
    ) -> Result<bool, String> {
        let eye = config
            .as_mapping_mut()
            .and_then(|m| m.get_mut(&Value::String("eye".into())))
            .and_then(|v| v.as_mapping_mut());
        let Some(eye_map) = eye else {
            return Ok(false);
        };
        let Some(tags) = eye_map
            .get_mut(&Value::String("tags".into()))
            .and_then(|v| v.as_sequence_mut())
        else {
            return Ok(false);
        };
        let mut removed = false;
        for t in tags.iter_mut() {
            let is_match = t
                .get("mac")
                .and_then(|v| v.as_str())
                .map(|m| m.to_uppercase() == mac)
                .unwrap_or(false);
            if !is_match {
                continue;
            }
            let tm = match t.as_mapping_mut() {
                Some(m) => m,
                None => continue,
            };
            if let Some(thresholds) = tm
                .get_mut(&Value::String("field_thresholds".into()))
                .and_then(|v| v.as_sequence_mut())
            {
                let before = thresholds.len();
                thresholds.retain(|x| x.get("field").and_then(|v| v.as_str()) != Some(field));
                if thresholds.len() != before {
                    removed = true;
                }
            }
        }
        Ok(removed)
    }

    /// Create a timestamped backup of the config file
    /// The band keys of the synced kinds this Hub holds now
    /// (plan/portal/config_sync.md §7): `hub/threshold/<line>` for a wired
    /// line with its own bounds, `beacon/<MAC>/threshold/<field>` for each
    /// Beacon field band. A config sync prunes the ones a full document
    /// leaves out. Unreadable files contribute nothing.
    pub fn synced_band_keys(&self) -> Vec<String> {
        let read = |name: &str| -> Option<Value> {
            serde_yaml::from_str(&fs::read_to_string(self.config_dir.join(name)).ok()?).ok()
        };
        let mut keys = Vec::new();
        if let Some(cfg) = read("fiber.sensors.config.yaml") {
            for entry in cfg
                .get("lines")
                .and_then(Value::as_sequence)
                .into_iter()
                .flatten()
            {
                let Some(line) = entry.get("line").and_then(Value::as_u64) else {
                    continue;
                };
                if LINE_BOUND_KEYS.iter().any(|k| entry.get(*k).is_some()) {
                    keys.push(format!("hub/threshold/{line}"));
                }
            }
        }
        if let Some(cfg) = read("fiber.config.yaml") {
            let tags = cfg
                .get("eye")
                .and_then(|e| e.get("tags"))
                .and_then(Value::as_sequence);
            for tag in tags.into_iter().flatten() {
                let Some(mac) = tag.get("mac").and_then(Value::as_str) else {
                    continue;
                };
                let fts = tag.get("field_thresholds").and_then(Value::as_sequence);
                for ft in fts.into_iter().flatten() {
                    if let Some(field) = ft.get("field").and_then(Value::as_str) {
                        keys.push(format!("beacon/{}/threshold/{field}", mac.to_uppercase()));
                    }
                }
            }
        }
        keys
    }

    /// Remove a wired line's own bounds, so it falls back to the common ones
    /// — the line's band was disabled in the Portal (config sync prune).
    pub fn clear_line_thresholds(&self, line: u8) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let config_file = self.config_dir.join("fiber.sensors.config.yaml");
        let fail = |msg: String, backup: Option<String>| ApplyResult {
            success: false,
            file_path: config_file.to_string_lossy().to_string(),
            backup_path: backup,
            error_message: Some(msg),
            applied_at,
        };
        let content = match fs::read_to_string(&config_file) {
            Ok(c) => c,
            Err(e) => return fail(format!("Failed to read config file: {e}"), None),
        };
        let mut config: Value = match serde_yaml::from_str(&content) {
            Ok(c) => c,
            Err(e) => return fail(format!("Failed to parse YAML: {e}"), None),
        };
        let entry = config
            .get_mut("lines")
            .and_then(Value::as_sequence_mut)
            .and_then(|lines| {
                lines
                    .iter_mut()
                    .find(|e| e.get("line").and_then(Value::as_u64) == Some(line as u64))
            })
            .and_then(Value::as_mapping_mut);
        let Some(map) = entry else {
            return fail(format!("Line {line} not found in config"), None);
        };
        for k in LINE_BOUND_KEYS {
            map.remove(Value::String(k.to_string()));
        }
        let backup = self
            .create_backup(&config_file, &content)
            .map(|p| p.to_string_lossy().to_string());
        let new_content = match serde_yaml::to_string(&config) {
            Ok(c) => c,
            Err(e) => return fail(format!("Failed to serialize YAML: {e}"), backup),
        };
        if let Err(e) = self.write_atomic(&config_file, &new_content) {
            return fail(e, backup);
        }
        self.log_audit("CLEAR_SENSOR_THRESHOLD", format!(r#"{{"line":{line}}}"#));
        ApplyResult {
            success: true,
            file_path: config_file.to_string_lossy().to_string(),
            backup_path: backup,
            error_message: None,
            applied_at,
        }
    }

    /// Write only `sensors.sample_interval_s` (config_sync.md §13 Q19): the
    /// storage window stays as the Hub has it. The range is the protocol's
    /// [`fiber_protocol::config_sync::SAMPLE_INTERVAL_S`], which never
    /// exceeds the smallest window, so no cross-check is needed.
    pub fn apply_sample_interval(&self, sample_interval_s: u64) -> ApplyResult {
        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let config_file = self.config_dir.join("fiber.config.yaml");
        let fail = |msg: String, backup: Option<String>| ApplyResult {
            success: false,
            file_path: config_file.to_string_lossy().to_string(),
            backup_path: backup,
            error_message: Some(msg),
            applied_at,
        };
        let range = fiber_protocol::config_sync::SAMPLE_INTERVAL_S;
        if !range.contains(&sample_interval_s) {
            return fail(
                format!(
                    "sample_interval_s ({sample_interval_s}) must be within {}..={}",
                    range.start(),
                    range.end()
                ),
                None,
            );
        }
        let content = match fs::read_to_string(&config_file) {
            Ok(c) => c,
            Err(e) => return fail(format!("Failed to read config file: {e}"), None),
        };
        let mut config: Value = match serde_yaml::from_str(&content) {
            Ok(c) => c,
            Err(e) => return fail(format!("Failed to parse YAML: {e}"), None),
        };
        let Some(sensors) = config.get_mut("sensors").and_then(Value::as_mapping_mut) else {
            return fail("Missing 'sensors' section in config".to_string(), None);
        };
        sensors.remove(Value::String("sample_interval_ms".to_string()));
        sensors.insert(
            Value::String("sample_interval_s".to_string()),
            Value::Number(serde_yaml::Number::from(sample_interval_s)),
        );
        let backup = self
            .create_backup(&config_file, &content)
            .map(|p| p.to_string_lossy().to_string());
        let new_content = match serde_yaml::to_string(&config) {
            Ok(c) => c,
            Err(e) => return fail(format!("Failed to serialize YAML: {e}"), backup),
        };
        if let Err(e) = self.write_atomic(&config_file, &new_content) {
            return fail(e, backup);
        }
        self.log_audit(
            "SET_SAMPLE_INTERVAL",
            format!(r#"{{"sample_interval_s":{sample_interval_s}}}"#),
        );
        ApplyResult {
            success: true,
            file_path: config_file.to_string_lossy().to_string(),
            backup_path: backup,
            error_message: None,
            applied_at,
        }
    }

    fn create_backup(&self, config_file: &Path, content: &str) -> Option<PathBuf> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let filename = config_file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config.yaml");

        let backup_file = self
            .backup_dir
            .join(format!("{}.{}.bak", filename, timestamp));

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

        fs::copy(backup_file, config_file).map_err(|e| format!("Failed to rollback: {}", e))?;

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

        let applier =
            ConfigApplier::new_with_storage(tmp_config_dir.path(), Some(storage.clone())).unwrap();
        let result = applier.remove_lorawan_sensor_config("70b3d5".to_string());
        assert!(
            result.success,
            "removal should succeed: {:?}",
            result.error_message
        );

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
    fn apply_beacon_tag_config_creates_section_and_entry() {
        let tmp_config_dir = tempfile::tempdir().unwrap();
        // No `eye:` section yet — the helper must create it.
        std::fs::write(
            tmp_config_dir.path().join("fiber.config.yaml"),
            "system:\n  device_label: \"X\"\n",
        )
        .unwrap();

        let applier = ConfigApplier::new(tmp_config_dir.path()).unwrap();
        let result = applier
            .apply_beacon_tag_config("aa:bb:cc:dd:ee:ff".to_string(), Some("Freezer".to_string()));
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
    fn apply_beacon_tag_config_updates_existing_name() {
        let tmp_config_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp_config_dir.path().join("fiber.config.yaml"),
            "eye:\n  enabled: true\n  tags:\n    - mac: 'AA:BB:CC:DD:EE:FF'\n      enabled: true\n      name: Old\n",
        )
        .unwrap();

        let applier = ConfigApplier::new(tmp_config_dir.path()).unwrap();
        // Same MAC in lowercase must match the existing (uppercase) entry.
        let result = applier
            .apply_beacon_tag_config("aa:bb:cc:dd:ee:ff".to_string(), Some("New".to_string()));
        assert!(result.success, "{:?}", result.error_message);

        let contents =
            std::fs::read_to_string(tmp_config_dir.path().join("fiber.config.yaml")).unwrap();
        let parsed: Value = serde_yaml::from_str(&contents).unwrap();
        let tags = parsed["eye"]["tags"].as_sequence().unwrap();
        assert_eq!(tags.len(), 1, "must upsert, not duplicate");
        assert_eq!(tags[0]["name"].as_str().unwrap(), "New");
    }

    #[test]
    fn remove_beacon_tag_config_removes_entry() {
        let tmp_config_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp_config_dir.path().join("fiber.config.yaml"),
            "eye:\n  tags:\n    - mac: 'AA:BB:CC:DD:EE:FF'\n      enabled: true\n",
        )
        .unwrap();

        let applier = ConfigApplier::new(tmp_config_dir.path()).unwrap();
        let result = applier.remove_beacon_tag_config("aa:bb:cc:dd:ee:ff".to_string());
        assert!(result.success, "{:?}", result.error_message);

        let contents =
            std::fs::read_to_string(tmp_config_dir.path().join("fiber.config.yaml")).unwrap();
        let parsed: Value = serde_yaml::from_str(&contents).unwrap();
        assert_eq!(parsed["eye"]["tags"].as_sequence().unwrap().len(), 0);
    }

    #[test]
    fn remove_beacon_tag_config_missing_is_error() {
        let tmp_config_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp_config_dir.path().join("fiber.config.yaml"),
            "eye:\n  tags: []\n",
        )
        .unwrap();

        let applier = ConfigApplier::new(tmp_config_dir.path()).unwrap();
        let result = applier.remove_beacon_tag_config("AA:BB:CC:DD:EE:FF".to_string());
        assert!(!result.success);
        assert!(result.error_message.unwrap().contains("not found"));
    }

    #[test]
    fn apply_beacon_enabled_flips_an_existing_section() {
        let tmp_config_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp_config_dir.path().join("fiber.config.yaml"),
            "eye:\n  enabled: false\n  publish_interval_s: 30\n  tags:\n    - mac: 'AA:BB:CC:DD:EE:FF'\n",
        )
        .unwrap();
        let applier = ConfigApplier::new(tmp_config_dir.path()).unwrap();

        assert!(applier.apply_beacon_enabled(true).success);
        let parsed: Value = serde_yaml::from_str(
            &std::fs::read_to_string(tmp_config_dir.path().join("fiber.config.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(parsed["eye"]["enabled"].as_bool(), Some(true));
        // The rest of the section survives.
        assert_eq!(parsed["eye"]["publish_interval_s"].as_u64(), Some(30));
        assert_eq!(
            parsed["eye"]["tags"].as_sequence().map(|s| s.len()),
            Some(1)
        );

        // ...and back off again, which is the point of having the command.
        assert!(applier.apply_beacon_enabled(false).success);
        let parsed: Value = serde_yaml::from_str(
            &std::fs::read_to_string(tmp_config_dir.path().join("fiber.config.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(parsed["eye"]["enabled"].as_bool(), Some(false));
    }

    #[test]
    fn apply_beacon_enabled_creates_a_missing_section() {
        let tmp_config_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp_config_dir.path().join("fiber.config.yaml"),
            "mqtt:\n  enabled: true\n",
        )
        .unwrap();
        let applier = ConfigApplier::new(tmp_config_dir.path()).unwrap();

        assert!(applier.apply_beacon_enabled(true).success);
        let parsed: Value = serde_yaml::from_str(
            &std::fs::read_to_string(tmp_config_dir.path().join("fiber.config.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(parsed["eye"]["enabled"].as_bool(), Some(true));
        assert_eq!(parsed["mqtt"]["enabled"].as_bool(), Some(true));
    }

    #[test]
    fn apply_beacon_config_persists_flags_and_creates_section() {
        // Existing eye: section — only the Some field is written, the other and
        // the rest of the section survive.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("fiber.config.yaml"),
            "eye:\n  enabled: true\n  auto_provision: true\n  auto_discover: true\n  tags: []\n",
        )
        .unwrap();
        let applier = ConfigApplier::new(tmp.path()).unwrap();
        assert!(applier.apply_beacon_config(Some(false), None).success);
        let parsed: Value = serde_yaml::from_str(
            &std::fs::read_to_string(tmp.path().join("fiber.config.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(parsed["eye"]["auto_provision"].as_bool(), Some(false));
        assert_eq!(parsed["eye"]["auto_discover"].as_bool(), Some(true)); // untouched
        assert_eq!(parsed["eye"]["enabled"].as_bool(), Some(true));

        // Config WITHOUT an eye: section — it is created, and auto_discover
        // lands as a real key (not an omitted Option).
        let tmp2 = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp2.path().join("fiber.config.yaml"),
            "mqtt:\n  enabled: true\n",
        )
        .unwrap();
        let applier2 = ConfigApplier::new(tmp2.path()).unwrap();
        assert!(
            applier2
                .apply_beacon_config(Some(true), Some(false))
                .success
        );
        let raw = std::fs::read_to_string(tmp2.path().join("fiber.config.yaml")).unwrap();
        assert!(raw.contains("auto_discover"));
        let parsed2: Value = serde_yaml::from_str(&raw).unwrap();
        assert_eq!(parsed2["eye"]["auto_provision"].as_bool(), Some(true));
        assert_eq!(parsed2["eye"]["auto_discover"].as_bool(), Some(false));
        assert_eq!(parsed2["mqtt"]["enabled"].as_bool(), Some(true));
    }

    #[test]
    fn apply_beacon_recording_persists_off_and_interval() {
        let tmp_config_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp_config_dir.path().join("fiber.config.yaml"),
            "eye:\n  tags:\n    - mac: 'AA:BB:CC:DD:EE:FF'\n      enabled: true\n",
        )
        .unwrap();
        let applier = ConfigApplier::new(tmp_config_dir.path()).unwrap();

        // interval 5 -> recording on + interval persisted
        assert!(
            applier
                .apply_beacon_recording("aa:bb:cc:dd:ee:ff".to_string(), 5)
                .success
        );
        let parsed: Value = serde_yaml::from_str(
            &std::fs::read_to_string(tmp_config_dir.path().join("fiber.config.yaml")).unwrap(),
        )
        .unwrap();
        let tag = &parsed["eye"]["tags"][0];
        assert_eq!(tag["recording"].as_bool(), Some(true));
        assert_eq!(tag["logging_interval_min"].as_u64(), Some(5));

        // interval 0 -> recording off persisted (H1: must survive restart)
        assert!(
            applier
                .apply_beacon_recording("AA:BB:CC:DD:EE:FF".to_string(), 0)
                .success
        );
        let parsed: Value = serde_yaml::from_str(
            &std::fs::read_to_string(tmp_config_dir.path().join("fiber.config.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(parsed["eye"]["tags"][0]["recording"].as_bool(), Some(false));

        // unknown MAC -> error
        let r = applier.apply_beacon_recording("11:22:33:44:55:66".to_string(), 1);
        assert!(!r.success);
        assert!(r.error_message.unwrap().contains("not found"));
    }

    #[test]
    fn apply_and_delete_beacon_field_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("fiber.config.yaml"),
            "eye:\n  tags:\n    - mac: 'AA:BB:CC:DD:EE:FF'\n      enabled: true\n",
        )
        .unwrap();
        let applier = ConfigApplier::new(tmp.path()).unwrap();

        // upsert (lowercase MAC must match the uppercase entry)
        assert!(
            applier
                .apply_beacon_field_threshold(
                    "aa:bb:cc:dd:ee:ff".into(),
                    "temperature".into(),
                    Some(-20.0),
                    Some(0.0),
                    Some(8.0),
                    Some(12.0),
                    Some(1.0),
                )
                .success
        );
        let parsed: Value = serde_yaml::from_str(
            &std::fs::read_to_string(tmp.path().join("fiber.config.yaml")).unwrap(),
        )
        .unwrap();
        let ft = &parsed["eye"]["tags"][0]["field_thresholds"][0];
        assert_eq!(ft["field"].as_str(), Some("temperature"));
        assert_eq!(ft["critical_high"].as_f64(), Some(12.0));

        // delete
        assert!(
            applier
                .delete_beacon_field_threshold("AA:BB:CC:DD:EE:FF".into(), "temperature".into())
                .success
        );
        let parsed: Value = serde_yaml::from_str(
            &std::fs::read_to_string(tmp.path().join("fiber.config.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            parsed["eye"]["tags"][0]["field_thresholds"]
                .as_sequence()
                .unwrap()
                .len(),
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

        let contents =
            std::fs::read_to_string(tmp_config_dir.path().join("fiber.config.yaml")).unwrap();
        assert!(contents.contains("Ward 3 Freezer"), "got: {contents}");
        assert!(
            !contents.contains("OLD-LABEL"),
            "old value should be gone: {contents}"
        );
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
        let contents =
            std::fs::read_to_string(tmp_config_dir.path().join("fiber.config.yaml")).unwrap();
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
        assert!(result
            .error_message
            .unwrap()
            .to_lowercase()
            .contains("empty"),);
    }

    #[test]
    fn apply_device_label_change_rejects_mqtt_chars() {
        let tmp_config_dir = tempfile::tempdir().unwrap();
        write_minimal_main_config(tmp_config_dir.path());
        let applier = ConfigApplier::new(tmp_config_dir.path()).unwrap();

        let result = applier.apply_device_label_change("bad/label".to_string());
        assert!(!result.success);
        assert!(result
            .error_message
            .unwrap()
            .to_lowercase()
            .contains("mqtt"));

        // And the on-disk file is untouched.
        let contents =
            std::fs::read_to_string(tmp_config_dir.path().join("fiber.config.yaml")).unwrap();
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
        let applier =
            ConfigApplier::new_with_storage(tmp_config_dir.path(), Some(storage.clone())).unwrap();

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
        assert!(
            n >= 1,
            "expected an SET_DEVICE_LABEL audit row to be written"
        );

        let details: Option<String> = conn
            .query_row(
                "SELECT details FROM audit_log WHERE operation = 'SET_DEVICE_LABEL' ORDER BY timestamp DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let details = details.expect("details should be Some");
        assert!(
            details.contains("New Label"),
            "details should carry new label: {details}"
        );
    }
}

/// A wired line's own bound keys in `fiber.sensors.config.yaml`.
const LINE_BOUND_KEYS: [&str; 6] = [
    "critical_low_celsius",
    "low_alarm_celsius",
    "warning_low_celsius",
    "warning_high_celsius",
    "high_alarm_celsius",
    "critical_high_celsius",
];

#[cfg(test)]
mod config_sync_prune_tests {
    use super::*;

    #[test]
    fn held_band_keys_are_listed_and_a_line_band_clears() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("fiber.sensors.config.yaml"),
            "lines:\n- line: 0\n  enabled: true\n  name: a\n  warning_low_celsius: 2.0\n  \
             critical_high_celsius: 30.0\n- line: 1\n  enabled: true\n  name: b\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("fiber.config.yaml"),
            "eye:\n  enabled: true\n  tags:\n  - mac: aa:bb:cc:dd:ee:ff\n    \
             field_thresholds:\n    - field: temperature\n      warning_high: 8.0\n",
        )
        .unwrap();
        let applier = ConfigApplier::new(dir.path()).unwrap();
        assert_eq!(
            applier.synced_band_keys(),
            vec![
                "hub/threshold/0".to_string(),
                "beacon/AA:BB:CC:DD:EE:FF/threshold/temperature".to_string(),
            ]
        );
        let r = applier.clear_line_thresholds(0);
        assert!(r.success, "{:?}", r.error_message);
        let r =
            applier.delete_beacon_field_threshold("AA:BB:CC:DD:EE:FF".into(), "temperature".into());
        assert!(r.success, "{:?}", r.error_message);
        assert!(applier.synced_band_keys().is_empty());
        let written =
            std::fs::read_to_string(dir.path().join("fiber.sensors.config.yaml")).unwrap();
        assert!(written.contains("name: a"), "{written}");
        assert!(!applier.clear_line_thresholds(7).success);
    }
}

#[cfg(test)]
mod sensor_interval_apply_tests {
    use super::*;

    /// Q19: only the read interval is written; the window is the Hub's.
    #[test]
    fn the_sample_interval_alone_is_written_and_range_checked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("fiber.config.yaml"),
            "sensors:\n  num_lines: 8\n  sample_interval_ms: 2000\n  \
             aggregation_interval_s: 15\n",
        )
        .unwrap();
        let applier = ConfigApplier::new(dir.path()).unwrap();
        let r = applier.apply_sample_interval(5);
        assert!(r.success, "{:?}", r.error_message);
        let written = std::fs::read_to_string(dir.path().join("fiber.config.yaml")).unwrap();
        assert!(written.contains("sample_interval_s: 5"), "{written}");
        assert!(written.contains("aggregation_interval_s: 15"), "{written}");
        assert!(!written.contains("sample_interval_ms"), "{written}");
        assert!(!applier.apply_sample_interval(0).success);
        assert!(!applier.apply_sample_interval(61).success);
    }

    /// The command speaks milliseconds; the file gets seconds, and the old
    /// spellings go, so one setting never has two values in the file.
    #[test]
    fn writes_seconds_and_drops_the_millisecond_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("fiber.config.yaml"),
            "sensors:\n  num_lines: 8\n  sample_interval_ms: 2000\n  \
             aggregation_interval_ms: 15000\n  report_interval_ms: 60000\n  \
             failure_threshold: 3\n",
        )
        .unwrap();
        let applier = ConfigApplier::new(dir.path()).unwrap();

        let result = applier.apply_interval_change(5000, 600_000, 0);
        assert!(result.success, "{:?}", result.error_message);

        let written = std::fs::read_to_string(dir.path().join("fiber.config.yaml")).unwrap();
        assert!(!written.contains("_ms"), "{written}");
        let doc: Value = serde_yaml::from_str(&written).unwrap();
        let sensors: crate::libs::config::SensorConfig =
            serde_yaml::from_value(doc["sensors"].clone()).unwrap();
        assert_eq!(
            (sensors.sample_interval_s, sensors.aggregation_interval_s),
            (5, 600)
        );
    }

    #[test]
    fn rejects_a_window_outside_the_bounds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("fiber.config.yaml"),
            "sensors:\n  num_lines: 8\n  failure_threshold: 3\n",
        )
        .unwrap();
        let applier = ConfigApplier::new(dir.path()).unwrap();
        assert!(!applier.apply_interval_change(2000, 15_000, 60_000).success);
    }
}

#[cfg(test)]
mod system_info_interval_tests {
    use super::*;

    /// The written key must be the one the deserializer reads.
    ///
    /// This wrote a flat `mqtt.system_info_interval_seconds` while
    /// `config::PublishIntervals` reads `mqtt.publish.intervals.system_info_sec`,
    /// so `set_system_info_interval` rewrote the config, reported success, and
    /// changed nothing. The round-trip assertion below is the part that matters:
    /// it fails if the two ever drift apart again.
    fn applier() -> ConfigApplier {
        let dir = tempfile::tempdir().unwrap();
        let applier = ConfigApplier::new(dir.path()).unwrap();
        std::mem::forget(dir); // keep the temp dir alive for the test
        applier
    }

    #[test]
    fn writes_the_key_the_deserializer_reads() {
        let applier = applier();
        let mut config: Value = serde_yaml::from_str(
            r#"
mqtt:
  enabled: true
  publish:
    intervals:
      system_info_sec: 60
"#,
        )
        .unwrap();

        applier
            .update_system_info_interval(&mut config, 15)
            .unwrap();

        // Round-trip through the real config type that owns the field, so the
        // assertion is against serde's actual contract rather than a string we
        // also wrote. This is what fails if the two paths drift apart again.
        let intervals: crate::libs::config::PublishIntervals = serde_yaml::from_value(
            config
                .get("mqtt")
                .and_then(|m| m.get("publish"))
                .and_then(|p| p.get("intervals"))
                .expect("mqtt.publish.intervals must exist")
                .clone(),
        )
        .expect("intervals must deserialize into PublishIntervals");
        assert_eq!(intervals.system_info_sec, 15);
    }

    #[test]
    fn creates_the_nested_sections_when_absent() {
        let applier = applier();
        let mut config: Value = serde_yaml::from_str("mqtt:\n  enabled: true\n").unwrap();

        applier
            .update_system_info_interval(&mut config, 45)
            .unwrap();

        let value = config
            .get("mqtt")
            .and_then(|m| m.get("publish"))
            .and_then(|p| p.get("intervals"))
            .and_then(|i| i.get("system_info_sec"))
            .and_then(|v| v.as_u64());
        assert_eq!(value, Some(45));
    }

    #[test]
    fn drops_the_stale_flat_key() {
        let applier = applier();
        let mut config: Value =
            serde_yaml::from_str("mqtt:\n  enabled: true\n  system_info_interval_seconds: 999\n")
                .unwrap();

        applier
            .update_system_info_interval(&mut config, 30)
            .unwrap();

        let mqtt = config.get("mqtt").unwrap();
        assert!(
            mqtt.get("system_info_interval_seconds").is_none(),
            "the inert key a previous build wrote must not survive"
        );
        assert_eq!(
            mqtt.get("publish")
                .and_then(|p| p.get("intervals"))
                .and_then(|i| i.get("system_info_sec"))
                .and_then(|v| v.as_u64()),
            Some(30)
        );
    }

    #[test]
    fn errors_without_an_mqtt_section() {
        let applier = applier();
        let mut config: Value = serde_yaml::from_str("system:\n  led_brightness: 50\n").unwrap();
        assert!(applier
            .update_system_info_interval(&mut config, 30)
            .is_err());
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
            mac: None,
            field: "temperature".to_string(),
            label: None,
            format: DisplayLineFormat::default(),
        }
    }

    fn node_line(field: &str) -> DisplayLine {
        DisplayLine {
            source: DisplayLineSource::Node,
            line: None,
            dev_eui: Some(EUI.to_string()),
            mac: None,
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

        let result = applier.apply_display_custom_lines(vec![node_line("voltage"), ds_line(0)]);
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
        let result = applier.apply_display_custom_lines(vec![node_line("humidity")]);
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
        assert_eq!(
            parsed["mqtt"]["broker"]["host"].as_str(),
            Some("example.invalid")
        );
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
        assert!(
            err.contains("display line 1"),
            "should name the index: {err}"
        );

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

        let mut line = node_line("ext_temperature_1");
        line.format.decimals = Some(2);
        line.format.status_char = false;
        assert!(
            applier
                .apply_display_custom_lines(vec![line, ds_line(5)])
                .success
        );

        // `Config::from_file` needs every non-defaulted section, so merge the
        // written display section onto a full default config and re-read it.
        let written = read_yaml(&path);
        let mut base =
            match serde_yaml::to_value(crate::libs::config::Config::default_config()).unwrap() {
                Value::Mapping(m) => m,
                other => panic!("expected mapping, got {other:?}"),
            };
        base.insert(Value::String("display".into()), written["display"].clone());
        let merged = dir.path().join("merged.yaml");
        std::fs::write(
            &merged,
            serde_yaml::to_string(&Value::Mapping(base)).unwrap(),
        )
        .unwrap();

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

/// The `setup:` write (`plan/control/ble/peripheral.md` §6.3, GitLab #58) and the
/// validate-before-backup ordering `edit_config_yaml` owes every caller that shares it.
#[cfg(test)]
mod setup_and_edit_config_yaml_tests {
    use super::*;

    fn hub(body: &str) -> (tempfile::TempDir, ConfigApplier) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("fiber.config.yaml"), body).expect("seed");
        let applier = ConfigApplier::new(dir.path())
            .expect("applier")
            .with_cluster_dir(dir.path().join("cluster"));
        (dir, applier)
    }

    /// The role marker a cluster join leaves (`plan/control/cluster.md` §3).
    fn joined(dir: &tempfile::TempDir, marker: &str) {
        std::fs::create_dir_all(dir.path().join("cluster")).expect("cluster dir");
        std::fs::write(dir.path().join("cluster/role"), marker).expect("marker");
    }

    /// Every `.bak` sitting in the applier's backup directory.
    fn backups(dir: &tempfile::TempDir) -> Vec<PathBuf> {
        let backups = dir.path().join(".backups");
        let Ok(entries) = std::fs::read_dir(&backups) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("bak"))
            .collect()
    }

    fn role_of(dir: &tempfile::TempDir) -> Option<String> {
        let text = std::fs::read_to_string(dir.path().join("fiber.config.yaml")).expect("read");
        let doc: Value = serde_yaml::from_str(&text).expect("parse");
        doc.get("setup")?
            .get("role")?
            .as_str()
            .map(|s| s.to_string())
    }

    #[test]
    fn re_writing_the_same_role_is_idempotent_and_still_succeeds() {
        let (dir, applier) = hub("system:\n  device_label: bench\n");
        assert!(applier.apply_setup_complete(SetupRole::Server).success);
        let second = applier.apply_setup_complete(SetupRole::Server);
        assert!(second.success, "{:?}", second.error_message);
        assert_eq!(role_of(&dir).as_deref(), Some("server"));
    }

    #[test]
    fn a_provisioned_hub_refuses_a_different_role_and_names_the_factory_reset() {
        // A Server, provisioned: the client role is refused by the lock on the setup state,
        // and the message is the one §6.3 wrote, factory reset included.
        let (dir, applier) = hub("setup:\n  provisioned: true\n  role: server\n");
        joined(&dir, "client");
        let refused = applier.apply_setup_complete(SetupRole::Client);
        assert!(!refused.success);
        let message = refused.error_message.expect("a reason");
        assert!(message.contains("server"), "{message}");
        assert!(message.contains("factory reset"), "{message}");
        assert_eq!(role_of(&dir).as_deref(), Some("server"));

        let (dir, applier) = hub("system:\n  device_label: bench\n");
        joined(&dir, "client");
        assert!(applier.apply_setup_complete(SetupRole::Client).success);

        let refused = applier.apply_setup_complete(SetupRole::Server);
        assert!(!refused.success);
        let message = refused.error_message.expect("a reason");
        assert!(message.contains("client"), "{message}");
        assert!(message.contains("server"), "{message}");
        assert!(
            message.contains("factory reset"),
            "peripheral.md §6.3's way back must be in the message the app shows: {message}"
        );
        assert_eq!(
            role_of(&dir).as_deref(),
            Some("client"),
            "the refused write leaves the file alone"
        );
    }

    #[test]
    fn a_role_this_build_does_not_know_is_still_a_role_the_lock_protects() {
        // A firmware downgrade must not be the one way to unlock re-roling.
        let (_dir, applier) = hub("setup:\n  provisioned: true\n  role: arbiter\n");
        let refused = applier.apply_setup_complete(SetupRole::Server);
        assert!(!refused.success);
        assert!(refused.error_message.expect("a reason").contains("arbiter"));
    }

    #[test]
    fn a_hub_that_is_not_provisioned_yet_may_pick_either_role() {
        for role in [SetupRole::Server, SetupRole::Client] {
            let (dir, applier) = hub("setup:\n  provisioned: false\n  role: server\n");
            if role == SetupRole::Client {
                joined(&dir, "client");
            }
            assert!(
                applier.apply_setup_complete(role).success,
                "provisioned: false means no choice has been made yet, whatever `role:` says"
            );
            assert_eq!(role_of(&dir).as_deref(), Some(role.as_str()));
        }
    }

    #[test]
    fn a_hub_provisioned_without_a_role_may_still_choose_one() {
        let (dir, applier) = hub("setup:\n  provisioned: true\n");
        joined(&dir, "client");
        assert!(applier.apply_setup_complete(SetupRole::Client).success);
        assert_eq!(role_of(&dir).as_deref(), Some("client"));
    }

    #[test]
    fn a_refused_write_leaves_no_backup_behind() {
        // FB18 is not rate-limited, so a refusal that costs a `.bak` is a way to fill /data one
        // file per second. `edit_config_yaml` validates before it backs up; this is that.
        let (dir, applier) = hub("system:\n  device_label: bench\n");
        assert!(backups(&dir).is_empty(), "the fixture starts clean");

        let refused = applier.apply_export_destination("hub-1.local", "", "s3cret", true);
        assert!(!refused.success);
        assert_eq!(refused.backup_path, None, "no backup path is even reported");
        assert!(
            backups(&dir).is_empty(),
            "a refused write must not leave a backup: {:?}",
            backups(&dir)
        );

        // And the refused role change, the other caller of the shared helper.
        joined(&dir, "client");
        assert!(applier.apply_setup_complete(SetupRole::Client).success);
        let before = backups(&dir).len();
        assert!(!applier.apply_setup_complete(SetupRole::Server).success);
        assert_eq!(backups(&dir).len(), before, "still no new backup");
    }

    /// cluster.md §3: `client` is refused unless the marker says `client` — on a fresh Hub, on a
    /// Hub whose marker says something else, and with nothing written or backed up.
    #[test]
    fn the_client_role_needs_the_join_marker() {
        for marker in [None, Some(""), Some("server"), Some("clients")] {
            let (dir, applier) = hub("system:\n  device_label: bench\n");
            if let Some(m) = marker {
                joined(&dir, m);
            }
            let refused = applier.apply_setup_complete(SetupRole::Client);
            assert!(!refused.success, "marker {marker:?}");
            let message = refused.error_message.expect("a reason");
            assert!(message.contains("has not joined a Server"), "{message}");
            assert_eq!(role_of(&dir), None, "nothing written for marker {marker:?}");
            assert!(backups(&dir).is_empty(), "no backup for marker {marker:?}");
        }
    }

    /// The other half: a Hub that runs as a Client cannot be set up as a Server, provisioned or
    /// not — the crash window between the join's marker and its setup write included.
    #[test]
    fn a_joined_hub_refuses_the_server_role() {
        for body in [
            "system:\n  device_label: bench\n",
            "setup:\n  provisioned: true\n  role: client\n",
        ] {
            let (dir, applier) = hub(body);
            joined(&dir, "client\n");
            let refused = applier.apply_setup_complete(SetupRole::Server);
            assert!(!refused.success, "{body}");
            let message = refused.error_message.expect("a reason");
            assert!(message.contains("joined a Server"), "{message}");
            assert!(message.contains("factory reset"), "{message}");
        }
    }

    /// On a joined Hub, `client` again is the idempotent same-role write §6.3 allows.
    #[test]
    fn a_joined_hub_takes_the_client_role_again() {
        let (dir, applier) = hub("setup:\n  provisioned: true\n  role: client\n");
        joined(&dir, "client");
        let again = applier.apply_setup_complete(SetupRole::Client);
        assert!(again.success, "{:?}", again.error_message);
        assert_eq!(role_of(&dir).as_deref(), Some("client"));
    }

    /// A marker that cannot be read is not "no marker": the role cannot be checked, so neither is
    /// accepted.
    #[test]
    fn an_unreadable_marker_refuses_both_roles() {
        let (dir, applier) = hub("system:\n  device_label: bench\n");
        // A directory where the marker file should be: reading it fails with something other
        // than NotFound.
        std::fs::create_dir_all(dir.path().join("cluster/role")).expect("dir");
        for role in [SetupRole::Server, SetupRole::Client] {
            let refused = applier.apply_setup_complete(role);
            assert!(!refused.success);
            assert!(refused
                .error_message
                .expect("a reason")
                .contains("cannot read the cluster role marker"));
        }
    }

    #[test]
    fn an_accepted_write_still_backs_the_previous_content_up() {
        // The reorder must not have turned the backup off for the writes that do happen.
        let (dir, applier) = hub("system:\n  device_label: bench\n");
        let result = applier.apply_setup_complete(SetupRole::Server);
        assert!(result.success);
        let backup = result.backup_path.expect("an accepted write is backed up");
        let contents = std::fs::read_to_string(&backup).expect("the backup exists");
        assert!(
            !contents.contains("provisioned"),
            "the backup holds the pre-change content: {contents}"
        );
        assert_eq!(backups(&dir).len(), 1);
    }
}
