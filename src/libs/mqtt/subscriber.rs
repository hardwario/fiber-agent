// MQTT command subscription and handling

use crate::libs::crypto::UserCertificate;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

use super::messages::MqttCommand;

/// Command parser and validator
pub struct MqttSubscriber {
    max_commands_per_second: u32,
    audit_enabled: bool,
    command_count: u32,
    last_reset_time: u64,
}

impl MqttSubscriber {
    /// Create a new MQTT subscriber
    pub fn new(max_commands_per_second: u32, audit_enabled: bool) -> Self {
        Self {
            max_commands_per_second,
            audit_enabled,
            command_count: 0,
            last_reset_time: Self::current_timestamp(),
        }
    }

    /// Get current Unix timestamp
    fn current_timestamp() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    /// Check and update rate limiting
    fn check_rate_limit(&mut self) -> Result<(), String> {
        let now = Self::current_timestamp();

        // Reset counter if a second has passed
        if now > self.last_reset_time {
            self.command_count = 0;
            self.last_reset_time = now;
        }

        // Check rate limit
        if self.command_count >= self.max_commands_per_second {
            return Err(format!(
                "Rate limit exceeded: {} commands/second",
                self.max_commands_per_second
            ));
        }

        self.command_count += 1;
        Ok(())
    }

    /// Parse MQTT command from topic and payload
    pub fn parse_command(&mut self, topic: &str, payload: &[u8]) -> Result<MqttCommand, String> {
        // Check rate limiting
        self.check_rate_limit()?;

        // Parse JSON payload
        let json_str = std::str::from_utf8(payload).map_err(|e| format!("Invalid UTF-8: {}", e))?;
        let json: Value =
            serde_json::from_str(json_str).map_err(|e| format!("Invalid JSON: {}", e))?;

        // Log command if audit enabled
        if self.audit_enabled {
            eprintln!("[MQTT Subscriber] Received command on {}: {}", topic, json_str);
        }

        // Extract command type from JSON
        let command_type = json
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing 'command' field".to_string())?;

        // Parse based on command type
        match command_type {
            "set_threshold" => self.parse_set_threshold(&json),
            "get_status" => self.parse_get_status(&json),
            "set_screen" => self.parse_set_screen(&json),
            "flush_storage" => Ok(MqttCommand::FlushStorage),
            "get_info" => Ok(MqttCommand::GetDeviceInfo),
            "get_sensor_config" => Ok(MqttCommand::GetSensorConfig),
            "silence_buzzer" => Ok(MqttCommand::SilenceBuzzer),
            "restart" => self.parse_restart(&json),
            "set_interval" => self.parse_set_interval(&json),
            "get_interval" => Ok(MqttCommand::GetInterval),
            "config_request" => self.parse_config_request(&json),
            "config_confirm" => self.parse_config_confirm(&json),
            _ => Err(format!("Unknown command type: {}", command_type)),
        }
    }

    /// Parse set_threshold command
    fn parse_set_threshold(&self, json: &Value) -> Result<MqttCommand, String> {
        let line = json
            .get("line")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "Missing or invalid 'line' field".to_string())?
            as u8;

        // Validate line number
        if line > 7 {
            return Err(format!("Invalid line number: {} (must be 0-7)", line));
        }

        let thresholds = json
            .get("thresholds")
            .ok_or_else(|| "Missing 'thresholds' field".to_string())?;

        let critical_low = Self::parse_temp(thresholds, "critical_low")?;
        let alarm_low = Self::parse_temp_optional(thresholds, "alarm_low", 0.0);
        let warning_low = Self::parse_temp(thresholds, "warning_low")?;
        let warning_high = Self::parse_temp(thresholds, "warning_high")?;
        let alarm_high = Self::parse_temp_optional(thresholds, "alarm_high", 100.0);
        let critical_high = Self::parse_temp(thresholds, "critical_high")?;

        // Validate threshold ordering (4-level: critical_low < warning_low < warning_high < critical_high)
        self.validate_threshold_ordering(
            critical_low,
            warning_low,
            warning_high,
            critical_high,
        )?;

        Ok(MqttCommand::SetSensorThreshold {
            line,
            critical_low,
            alarm_low,
            warning_low,
            warning_high,
            alarm_high,
            critical_high,
        })
    }

    /// Parse temperature value from JSON
    fn parse_temp(obj: &Value, field: &str) -> Result<f32, String> {
        let temp = obj
            .get(field)
            .and_then(|v| v.as_f64())
            .ok_or_else(|| format!("Missing or invalid '{}' field", field))?
            as f32;

        // Validate temperature range (-50°C to 100°C)
        if !(-50.0..=100.0).contains(&temp) {
            return Err(format!(
                "Temperature {} out of valid range (-50 to 100°C): {}",
                field, temp
            ));
        }

        Ok(temp)
    }

    /// Parse optional temperature value from JSON (returns default if missing)
    fn parse_temp_optional(obj: &Value, field: &str, default: f32) -> f32 {
        obj.get(field)
            .and_then(|v| v.as_f64())
            .map(|v| v as f32)
            .unwrap_or(default)
    }

    /// Validate threshold ordering (4-level system)
    fn validate_threshold_ordering(
        &self,
        critical_low: f32,
        warning_low: f32,
        warning_high: f32,
        critical_high: f32,
    ) -> Result<(), String> {
        if critical_low >= warning_low {
            return Err("critical_low must be less than warning_low".to_string());
        }
        if warning_low >= warning_high {
            return Err("warning_low must be less than warning_high".to_string());
        }
        if warning_high >= critical_high {
            return Err("warning_high must be less than critical_high".to_string());
        }

        Ok(())
    }

    /// Parse get_status command
    fn parse_get_status(&self, json: &Value) -> Result<MqttCommand, String> {
        let line = json
            .get("line")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "Missing or invalid 'line' field".to_string())?
            as u8;

        if line > 7 {
            return Err(format!("Invalid line number: {} (must be 0-7)", line));
        }

        Ok(MqttCommand::GetSensorStatus { line })
    }

    /// Parse set_screen command
    fn parse_set_screen(&self, json: &Value) -> Result<MqttCommand, String> {
        let screen = json
            .get("screen")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'screen' field".to_string())?
            .to_string();

        // Validate screen name
        let valid_screens = ["sensors", "power", "network", "qr_code"];
        if !valid_screens.contains(&screen.as_str()) {
            return Err(format!(
                "Invalid screen name: {} (must be one of: {})",
                screen,
                valid_screens.join(", ")
            ));
        }

        Ok(MqttCommand::SetDisplayScreen { screen })
    }

    /// Parse restart command
    fn parse_restart(&self, json: &Value) -> Result<MqttCommand, String> {
        let reason = json
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("Remote command")
            .to_string();

        // Limit reason length
        if reason.len() > 256 {
            return Err("Reason too long (max 256 characters)".to_string());
        }

        Ok(MqttCommand::RestartApplication { reason })
    }

    /// Parse set_interval command
    fn parse_set_interval(&self, json: &Value) -> Result<MqttCommand, String> {
        let sample_interval_ms = json
            .get("sample_interval_ms")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "Missing or invalid 'sample_interval_ms' field".to_string())?;

        let aggregation_interval_ms = json
            .get("aggregation_interval_ms")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "Missing or invalid 'aggregation_interval_ms' field".to_string())?;

        let report_interval_ms = json
            .get("report_interval_ms")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "Missing or invalid 'report_interval_ms' field".to_string())?;

        // Validate intervals
        if sample_interval_ms < 100 {
            return Err("sample_interval_ms must be >= 100ms".to_string());
        }
        if report_interval_ms > 86_400_000 {
            return Err("report_interval_ms must be <= 24 hours (86400000ms)".to_string());
        }
        if sample_interval_ms > aggregation_interval_ms {
            return Err("sample_interval_ms must be <= aggregation_interval_ms".to_string());
        }
        if aggregation_interval_ms > report_interval_ms {
            return Err("aggregation_interval_ms must be <= report_interval_ms".to_string());
        }

        Ok(MqttCommand::SetInterval {
            sample_interval_ms,
            aggregation_interval_ms,
            report_interval_ms,
        })
    }

    /// Parse set_system_info_interval command
    fn parse_set_system_info_interval(&self, json: &Value) -> Result<MqttCommand, String> {
        let interval_seconds = json
            .get("interval_seconds")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "Missing or invalid 'interval_seconds' field".to_string())?;

        // Validate interval
        if interval_seconds < 5 {
            return Err("interval_seconds must be >= 5".to_string());
        }
        if interval_seconds > 86400 {
            return Err("interval_seconds must be <= 86400 (24 hours)".to_string());
        }

        Ok(MqttCommand::SetSystemInfoInterval { interval_seconds })
    }

    /// Parse config_request command (signed with Ed25519)
    fn parse_config_request(&self, json: &Value) -> Result<MqttCommand, String> {
        let request_id = json
            .get("request_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'request_id' field".to_string())?
            .to_string();

        let command_type = json
            .get("command_type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'command_type' field".to_string())?
            .to_string();

        let params = json
            .get("params")
            .ok_or_else(|| "Missing 'params' field".to_string())?
            .clone();

        let reason = json
            .get("reason")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let signer_id = json
            .get("signer_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'signer_id' field".to_string())?
            .to_string();

        let signature = json
            .get("signature")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'signature' field".to_string())?
            .to_string();

        let timestamp = json
            .get("timestamp")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| "Missing or invalid 'timestamp' field".to_string())?;

        let nonce = json
            .get("nonce")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'nonce' field".to_string())?
            .to_string();

        // Parse user certificate (signed by CA)
        let certificate = self.parse_certificate(json)?;

        // Validate request_id format (UUID-like)
        if request_id.len() < 16 || request_id.len() > 64 {
            return Err("Invalid request_id format (must be 16-64 chars)".to_string());
        }

        // Validate nonce format (should be 32+ chars for security)
        if nonce.len() < 32 {
            return Err("Invalid nonce format (must be at least 32 chars)".to_string());
        }

        // Validate signature is base64-like (Ed25519 signatures are 64 bytes -> ~88 chars base64)
        if signature.len() < 80 || signature.len() > 100 {
            return Err("Invalid signature format (expected base64 Ed25519 signature)".to_string());
        }

        // Validate signer_id matches certificate
        if signer_id != certificate.signer_id {
            return Err(format!(
                "Signer ID mismatch: command has '{}' but certificate has '{}'",
                signer_id, certificate.signer_id
            ));
        }

        Ok(MqttCommand::ConfigRequest {
            request_id,
            command_type,
            params,
            reason,
            signer_id,
            signature,
            timestamp,
            nonce,
            certificate,
        })
    }

    /// Parse config_confirm command (signed confirmation)
    fn parse_config_confirm(&self, json: &Value) -> Result<MqttCommand, String> {
        let challenge_id = json
            .get("challenge_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'challenge_id' field".to_string())?
            .to_string();

        let confirmation = json
            .get("confirmation")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'confirmation' field".to_string())?
            .to_string();

        let signer_id = json
            .get("signer_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'signer_id' field".to_string())?
            .to_string();

        let signature = json
            .get("signature")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'signature' field".to_string())?
            .to_string();

        let timestamp = json
            .get("timestamp")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| "Missing or invalid 'timestamp' field".to_string())?;

        let nonce = json
            .get("nonce")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'nonce' field".to_string())?
            .to_string();

        // Parse user certificate (signed by CA)
        let certificate = self.parse_certificate(json)?;

        // Validate challenge_id format
        if challenge_id.len() < 16 || challenge_id.len() > 64 {
            return Err("Invalid challenge_id format (must be 16-64 chars)".to_string());
        }

        // Validate confirmation value
        if confirmation != "APPROVED" && confirmation != "REJECTED" {
            return Err(format!(
                "Invalid confirmation value: {} (must be APPROVED or REJECTED)",
                confirmation
            ));
        }

        // Validate nonce format (should be 32+ chars for security)
        if nonce.len() < 32 {
            return Err("Invalid nonce format (must be at least 32 chars)".to_string());
        }

        // Validate signature format (Ed25519 signatures are 64 bytes -> ~88 chars base64)
        if signature.len() < 80 || signature.len() > 100 {
            return Err("Invalid signature format (expected base64 Ed25519 signature)".to_string());
        }

        // Validate signer_id matches certificate
        if signer_id != certificate.signer_id {
            return Err(format!(
                "Signer ID mismatch: command has '{}' but certificate has '{}'",
                signer_id, certificate.signer_id
            ));
        }

        Ok(MqttCommand::ConfigConfirm {
            challenge_id,
            confirmation,
            signer_id,
            signature,
            timestamp,
            nonce,
            certificate,
        })
    }

    /// Parse user certificate from JSON command
    fn parse_certificate(&self, json: &Value) -> Result<UserCertificate, String> {
        let cert_json = json
            .get("certificate")
            .ok_or_else(|| "Missing 'certificate' field".to_string())?;

        let certificate: UserCertificate =
            serde_json::from_value(cert_json.clone()).map_err(|e| {
                format!(
                    "Invalid certificate format: {}. Expected fields: signer_id, full_name, role, public_key_ed25519, permissions, issued_at, expires_at, issuer, certificate_signature",
                    e
                )
            })?;

        // Basic validation of certificate fields
        if certificate.signer_id.is_empty() {
            return Err("Certificate signer_id cannot be empty".to_string());
        }

        if certificate.public_key_ed25519.len() != 64 {
            return Err(format!(
                "Certificate public_key_ed25519 must be 64 hex characters, got {}",
                certificate.public_key_ed25519.len()
            ));
        }

        if certificate.certificate_signature.is_empty() {
            return Err("Certificate signature cannot be empty".to_string());
        }

        Ok(certificate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limiting() {
        let mut subscriber = MqttSubscriber::new(3, false);

        // Should allow 3 commands per second
        assert!(subscriber.check_rate_limit().is_ok());
        assert!(subscriber.check_rate_limit().is_ok());
        assert!(subscriber.check_rate_limit().is_ok());

        // 4th command should fail
        assert!(subscriber.check_rate_limit().is_err());
    }

    #[test]
    fn test_parse_flush_storage_command() {
        let mut subscriber = MqttSubscriber::new(10, false);

        let payload = br#"{"command": "flush_storage"}"#;
        let result = subscriber.parse_command("test/commands", payload);

        assert!(result.is_ok());
        matches!(result.unwrap(), MqttCommand::FlushStorage);
    }

    #[test]
    fn test_parse_set_threshold_command() {
        let mut subscriber = MqttSubscriber::new(10, false);

        let payload = br#"{
            "command": "set_threshold",
            "line": 0,
            "thresholds": {
                "critical_low": 32.0,
                "alarm_low": 34.0,
                "warning_low": 35.0,
                "warning_high": 39.0,
                "alarm_high": 40.0,
                "critical_high": 42.0
            }
        }"#;

        let result = subscriber.parse_command("test/commands", payload);
        assert!(result.is_ok());

        match result.unwrap() {
            MqttCommand::SetSensorThreshold { line, critical_low, .. } => {
                assert_eq!(line, 0);
                assert_eq!(critical_low, 32.0);
            }
            _ => panic!("Wrong command type"),
        }
    }

    #[test]
    fn test_invalid_threshold_ordering() {
        let mut subscriber = MqttSubscriber::new(10, false);

        // critical_low (36) >= warning_low (35) should fail
        let payload = br#"{
            "command": "set_threshold",
            "line": 0,
            "thresholds": {
                "critical_low": 36.0,
                "warning_low": 35.0,
                "warning_high": 39.0,
                "critical_high": 42.0
            }
        }"#;

        let result = subscriber.parse_command("test/commands", payload);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("critical_low"));
    }

    #[test]
    fn test_set_threshold_without_alarm_fields() {
        // 4-level threshold command (no alarm_low/alarm_high)
        let mut subscriber = MqttSubscriber::new(10, false);

        let payload = br#"{
            "command": "set_threshold",
            "line": 2,
            "thresholds": {
                "critical_low": 18.0,
                "warning_low": 27.0,
                "warning_high": 38.5,
                "critical_high": 41.0
            }
        }"#;

        let result = subscriber.parse_command("test/commands", payload);
        assert!(result.is_ok());

        match result.unwrap() {
            MqttCommand::SetSensorThreshold { line, critical_low, alarm_low, warning_low, warning_high, alarm_high, critical_high } => {
                assert_eq!(line, 2);
                assert_eq!(critical_low, 18.0);
                assert_eq!(alarm_low, 0.0);  // default
                assert_eq!(warning_low, 27.0);
                assert_eq!(warning_high, 38.5);
                assert_eq!(alarm_high, 100.0);  // default
                assert_eq!(critical_high, 41.0);
            }
            _ => panic!("Wrong command type"),
        }
    }

    #[test]
    fn test_invalid_line_number() {
        let mut subscriber = MqttSubscriber::new(10, false);

        let payload = br#"{"command": "get_status", "line": 99}"#;
        let result = subscriber.parse_command("test/commands", payload);

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid line number"));
    }
}
