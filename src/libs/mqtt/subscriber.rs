// MQTT command subscription and handling

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
            "restart" => self.parse_restart(&json),
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
        let alarm_low = Self::parse_temp(thresholds, "alarm_low")?;
        let warning_low = Self::parse_temp(thresholds, "warning_low")?;
        let warning_high = Self::parse_temp(thresholds, "warning_high")?;
        let alarm_high = Self::parse_temp(thresholds, "alarm_high")?;
        let critical_high = Self::parse_temp(thresholds, "critical_high")?;

        // Validate threshold ordering
        self.validate_threshold_ordering(
            critical_low,
            alarm_low,
            warning_low,
            warning_high,
            alarm_high,
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

    /// Validate threshold ordering
    fn validate_threshold_ordering(
        &self,
        critical_low: f32,
        alarm_low: f32,
        warning_low: f32,
        warning_high: f32,
        alarm_high: f32,
        critical_high: f32,
    ) -> Result<(), String> {
        if critical_low >= alarm_low {
            return Err("critical_low must be less than alarm_low".to_string());
        }
        if alarm_low >= warning_low {
            return Err("alarm_low must be less than warning_low".to_string());
        }
        if warning_low >= warning_high {
            return Err("warning_low must be less than warning_high".to_string());
        }
        if warning_high >= alarm_high {
            return Err("warning_high must be less than alarm_high".to_string());
        }
        if alarm_high >= critical_high {
            return Err("alarm_high must be less than critical_high".to_string());
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

        let payload = br#"{
            "command": "set_threshold",
            "line": 0,
            "thresholds": {
                "critical_low": 35.0,
                "alarm_low": 34.0,
                "warning_low": 35.0,
                "warning_high": 39.0,
                "alarm_high": 40.0,
                "critical_high": 42.0
            }
        }"#;

        let result = subscriber.parse_command("test/commands", payload);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("critical_low"));
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
