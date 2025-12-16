//! Authorization manager for signed configuration commands

use super::state::{ChallengeRegistry, ChallengeState, PendingChallenge};
use crate::libs::crypto::{SignatureVerifier, UserCertificate, VerificationResult};
use crate::libs::mqtt::messages::{MqttCommand, MqttMessage};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Result type for authorization operations
pub type AuthResult<T> = Result<T, AuthError>;

/// Authorization error types
#[derive(Debug)]
pub enum AuthError {
    SignatureVerificationFailed(String),
    ChallengeNotFound(String),
    ChallengeExpired(String),
    InvalidState(String),
    DatabaseError(String),
    InvalidCommand(String),
    RegistryFull(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::SignatureVerificationFailed(msg) => write!(f, "Signature verification failed: {}", msg),
            AuthError::ChallengeNotFound(id) => write!(f, "Challenge not found: {}", id),
            AuthError::ChallengeExpired(id) => write!(f, "Challenge expired: {}", id),
            AuthError::InvalidState(msg) => write!(f, "Invalid state: {}", msg),
            AuthError::DatabaseError(msg) => write!(f, "Database error: {}", msg),
            AuthError::InvalidCommand(msg) => write!(f, "Invalid command: {}", msg),
            AuthError::RegistryFull(msg) => write!(f, "Registry full: {}", msg),
        }
    }
}

impl std::error::Error for AuthError {}

/// Authorization manager for signed configuration commands
pub struct AuthorizationManager {
    /// Signature verifier
    verifier: Arc<SignatureVerifier>,

    /// Challenge registry
    challenges: Arc<Mutex<ChallengeRegistry>>,

    /// Database path for audit trail
    db_path: String,

    /// Challenge timeout in seconds (default: 5 minutes)
    challenge_timeout_sec: i64,
}

impl AuthorizationManager {
    /// Create a new authorization manager
    pub fn new(
        verifier: Arc<SignatureVerifier>,
        db_path: &Path,
        challenge_timeout_sec: i64,
        max_concurrent_challenges: usize,
    ) -> Self {
        let db_path_str = db_path.to_string_lossy().to_string();

        // Initialize audit database with config_changes table
        if let Err(e) = Self::init_audit_db(&db_path_str) {
            eprintln!("[AuthManager] Warning: Failed to initialize audit database: {}", e);
        }

        Self {
            verifier,
            challenges: Arc::new(Mutex::new(ChallengeRegistry::new(max_concurrent_challenges))),
            db_path: db_path_str,
            challenge_timeout_sec,
        }
    }

    /// Initialize the audit database with required tables
    fn init_audit_db(db_path: &str) -> Result<(), String> {
        let conn = Connection::open(db_path)
            .map_err(|e| format!("Failed to open audit database: {}", e))?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS config_changes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL,
                challenge_id TEXT NOT NULL,
                request_id TEXT NOT NULL,
                signer_id TEXT NOT NULL,
                signer_name TEXT NOT NULL,
                command_type TEXT NOT NULL,
                command_json TEXT NOT NULL,
                signature_base64 TEXT NOT NULL,
                nonce TEXT NOT NULL,
                verification_status TEXT NOT NULL,
                applied INTEGER NOT NULL DEFAULT 0,
                error_msg TEXT
            )",
            [],
        ).map_err(|e| format!("Failed to create config_changes table: {}", e))?;

        // Create indexes for efficient querying
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_config_changes_timestamp ON config_changes(timestamp DESC);
             CREATE INDEX IF NOT EXISTS idx_config_changes_signer ON config_changes(signer_id);
             CREATE INDEX IF NOT EXISTS idx_config_changes_nonce ON config_changes(nonce);"
        ).map_err(|e| format!("Failed to create indexes: {}", e))?;

        eprintln!("[AuthManager] Audit database initialized: {}", db_path);
        Ok(())
    }

    /// Process a ConfigRequest command
    ///
    /// This verifies the signature, creates a challenge, and returns the challenge message
    /// to be published via MQTT.
    pub fn process_config_request(
        &self,
        request_id: String,
        command_type: String,
        params: Value,
        reason: Option<String>,
        signer_id: String,
        signature: String,
        timestamp: i64,
        nonce: String,
        certificate: &UserCertificate,
    ) -> AuthResult<MqttMessage> {
        // 1. Build canonical message for signature verification
        let canonical_msg = self.build_canonical_request_message(
            &request_id,
            &command_type,
            &params,
            &reason,
            &signer_id,
            timestamp,
            &nonce,
        );

        // 2. Determine required permission from command type
        let required_permission = self.command_type_to_permission(&command_type)?;

        // 3. Verify signature with certificate chain validation
        let verification = self
            .verifier
            .verify_signed_command(
                &canonical_msg,
                &signature,
                certificate,
                timestamp,
                &nonce,
                Some(&required_permission),
            )
            .map_err(|e| AuthError::SignatureVerificationFailed(format!("{:?}", e)))?;

        eprintln!(
            "[AuthManager] ✓ ConfigRequest verified: signer={} ({}) command={} issuer={}",
            verification.signer_id, verification.signer_name, command_type, verification.issuer
        );

        let signer_name = verification.signer_name.clone();

        // 4. Create challenge
        let challenge_id = Uuid::new_v4().to_string();
        let expires_at = timestamp + self.challenge_timeout_sec;

        let challenge = PendingChallenge::new(
            challenge_id.clone(),
            request_id.clone(),
            signer_id.clone(),
            signer_name.clone(),
            command_type.clone(),
            params.clone(),
            reason.clone(),
            signature.clone(),
            nonce.clone(),
            timestamp,
            expires_at,
        );

        // 5. Add to registry
        let mut registry = self.challenges.lock().unwrap();
        registry
            .add_challenge(challenge)
            .map_err(|e| AuthError::RegistryFull(e))?;

        eprintln!(
            "[AuthManager] Challenge created: {} (expires in {}s)",
            challenge_id, self.challenge_timeout_sec
        );

        // 6. Log to audit trail (request received)
        self.log_config_request(
            &challenge_id,
            &request_id,
            &verification,
            &command_type,
            &params,
            &reason,
            &signature,
            &nonce,
            timestamp,
        )?;

        // 7. Build preview of changes
        let preview = self.build_change_preview(&command_type, &params)?;

        // 8. Return PublishConfigChallenge message
        Ok(MqttMessage::PublishConfigChallenge {
            challenge_id,
            request_id,
            signer_id: signer_id.clone(),
            expires_at,
            preview,
        })
    }

    /// Process a ConfigConfirm command
    ///
    /// This verifies the confirmation signature and either applies or rejects the change.
    pub fn process_config_confirm(
        &self,
        challenge_id: String,
        confirmation: String,
        signer_id: String,
        signature: String,
        timestamp: i64,
        nonce: String,
        certificate: &UserCertificate,
    ) -> AuthResult<(MqttMessage, Option<MqttCommand>)> {
        // 1. Get challenge from registry
        let mut registry = self.challenges.lock().unwrap();
        let challenge = registry
            .get_challenge_mut(&challenge_id)
            .ok_or_else(|| AuthError::ChallengeNotFound(challenge_id.clone()))?;

        // 2. Check if expired
        if challenge.is_expired() {
            challenge.set_state(ChallengeState::Expired);
            return Err(AuthError::ChallengeExpired(challenge_id));
        }

        // 3. Verify same signer as original request
        if challenge.signer_id != signer_id {
            return Err(AuthError::SignatureVerificationFailed(format!(
                "Signer mismatch: expected {}, got {}",
                challenge.signer_id, signer_id
            )));
        }

        // 4. Build canonical confirmation message
        let canonical_msg = self.build_canonical_confirm_message(
            &challenge_id,
            &confirmation,
            &signer_id,
            timestamp,
            &nonce,
        );

        // 5. Verify signature with certificate chain validation (no specific permission needed for confirmation)
        let _verification = self
            .verifier
            .verify_signed_command(
                &canonical_msg,
                &signature,
                certificate,
                timestamp,
                &nonce,
                None, // No permission check for confirmation
            )
            .map_err(|e| AuthError::SignatureVerificationFailed(format!("{:?}", e)))?;

        eprintln!(
            "[AuthManager] ✓ ConfigConfirm verified: challenge={} confirmation={}",
            challenge_id, confirmation
        );

        // 6. Process confirmation
        let response_msg: MqttMessage;
        let command_to_execute: Option<MqttCommand>;

        if confirmation == "APPROVED" {
            challenge.set_state(ChallengeState::Applying);

            // Build command to execute
            let cmd = self.build_command_from_challenge(challenge)?;
            command_to_execute = Some(cmd);

            // Mark as applied
            challenge.set_state(ChallengeState::Applied);

            let applied_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;

            response_msg = MqttMessage::PublishConfigResponse {
                challenge_id: challenge_id.clone(),
                request_id: challenge.request_id.clone(),
                status: "SUCCESS".to_string(),
                applied_at: Some(applied_at),
                effective_at: Some(applied_at),
                message: format!("Configuration applied: {}", challenge.command_type),
            };

            eprintln!(
                "[AuthManager] ✓ Configuration applied: {}",
                challenge.command_type
            );
        } else {
            // REJECTED
            challenge.set_state(ChallengeState::Rejected);
            command_to_execute = None;

            response_msg = MqttMessage::PublishConfigResponse {
                challenge_id: challenge_id.clone(),
                request_id: challenge.request_id.clone(),
                status: "REJECTED".to_string(),
                applied_at: None,
                effective_at: None,
                message: "Configuration rejected by authorized signer".to_string(),
            };

            eprintln!("[AuthManager] Configuration rejected: {}", challenge_id);
        }

        // 7. Log to audit trail
        self.log_config_response(&challenge_id, &confirmation, timestamp)?;

        // 8. Remove from registry
        drop(registry);
        let mut registry = self.challenges.lock().unwrap();
        registry.remove_challenge(&challenge_id);

        Ok((response_msg, command_to_execute))
    }

    /// Cleanup expired challenges
    pub fn cleanup_expired_challenges(&self) -> usize {
        let mut registry = self.challenges.lock().unwrap();
        let expired = registry.cleanup_expired();
        let count = expired.len();

        for challenge in expired {
            eprintln!(
                "[AuthManager] Challenge expired: {} ({})",
                challenge.challenge_id, challenge.command_type
            );

            // Log expiry to audit trail
            let _ = self.log_challenge_expired(&challenge);
        }

        count
    }

    /// Get active challenge count
    pub fn active_challenge_count(&self) -> usize {
        self.challenges.lock().unwrap().active_count()
    }

    /// Reload CA registry from disk
    pub fn reload_ca_registry(&self) -> Result<bool, AuthError> {
        self.verifier
            .reload_registry()
            .map_err(|e| AuthError::DatabaseError(format!("Failed to reload CA registry: {:?}", e)))
    }

    // --- Private helper methods ---

    /// Recursively convert all JSON objects to use sorted keys (BTreeMap)
    /// This ensures deterministic serialization matching Python's json.dumps(sort_keys=True)
    fn sort_json_keys(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let sorted: std::collections::BTreeMap<String, Value> = map
                    .iter()
                    .map(|(k, v)| (k.clone(), Self::sort_json_keys(v)))
                    .collect();
                Value::Object(serde_json::Map::from_iter(sorted))
            }
            Value::Array(arr) => Value::Array(arr.iter().map(Self::sort_json_keys).collect()),
            other => other.clone(),
        }
    }

    /// Build canonical request message for signature verification
    fn build_canonical_request_message(
        &self,
        request_id: &str,
        command_type: &str,
        params: &Value,
        reason: &Option<String>,
        signer_id: &str,
        timestamp: i64,
        nonce: &str,
    ) -> String {
        use std::collections::BTreeMap;

        // Use BTreeMap for deterministic alphabetical key ordering
        let mut msg: BTreeMap<&str, Value> = BTreeMap::new();
        msg.insert("command_type", Value::String(command_type.to_string()));
        msg.insert("nonce", Value::String(nonce.to_string()));
        msg.insert("params", Self::sort_json_keys(params));
        msg.insert("reason", match reason {
            Some(r) => Value::String(r.clone()),
            None => Value::Null,
        });
        msg.insert("request_id", Value::String(request_id.to_string()));
        msg.insert("signer_id", Value::String(signer_id.to_string()));
        msg.insert("timestamp", json!(timestamp));

        serde_json::to_string(&msg).unwrap()
    }

    /// Build canonical confirmation message for signature verification
    fn build_canonical_confirm_message(
        &self,
        challenge_id: &str,
        confirmation: &str,
        signer_id: &str,
        timestamp: i64,
        nonce: &str,
    ) -> String {
        use std::collections::BTreeMap;

        // Use BTreeMap for deterministic alphabetical key ordering
        let mut msg: BTreeMap<&str, Value> = BTreeMap::new();
        msg.insert("challenge_id", Value::String(challenge_id.to_string()));
        msg.insert("confirmation", Value::String(confirmation.to_string()));
        msg.insert("nonce", Value::String(nonce.to_string()));
        msg.insert("signer_id", Value::String(signer_id.to_string()));
        msg.insert("timestamp", json!(timestamp));

        serde_json::to_string(&msg).unwrap()
    }

    /// Convert command type to required permission string
    /// These permission strings must match the permissions in user certificates
    fn command_type_to_permission(&self, command_type: &str) -> AuthResult<String> {
        // Map command types to permission strings that must be in certificate
        let permission = match command_type {
            "set_threshold" => "set_threshold",
            "set_sensor_name" => "set_sensor_name",
            "set_alarm_pattern" => "set_alarm_pattern",
            "set_screen" => "set_screen",
            "flush_storage" => "flush_storage",
            "restart_application" => "restart_application",
            "set_interval" => "set_interval",
            "get_info" => "get_info",
            "get_status" => "get_status",
            "add_signer" => "add_signer",
            "remove_signer" => "remove_signer",
            "update_signer" => "update_signer",
            _ => {
                return Err(AuthError::InvalidCommand(format!(
                    "Unknown command type: {}",
                    command_type
                )))
            }
        };
        Ok(permission.to_string())
    }

    /// Build change preview from command
    fn build_change_preview(&self, command_type: &str, params: &Value) -> AuthResult<Value> {
        Ok(json!({
            "command_type": command_type,
            "changes": params,
            "description": self.describe_change(command_type, params),
        }))
    }

    /// Describe change in human-readable format
    fn describe_change(&self, command_type: &str, params: &Value) -> String {
        match command_type {
            "set_threshold" => {
                let line = params.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
                format!("Change alarm thresholds for sensor line {}", line)
            }
            "set_sensor_name" => {
                let line = params.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                format!("Change sensor line {} name to \"{}\"", line, name)
            }
            "restart_application" => "Reboot the device".to_string(),
            "set_interval" => {
                let sample = params.get("sample_interval_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                let aggregation = params.get("aggregation_interval_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                let report = params.get("report_interval_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                format!("Change sensor intervals: sample={}ms, aggregation={}ms, report={}ms", sample, aggregation, report)
            }
            "add_signer" => {
                let signer_id = params.get("signer_id").and_then(|v| v.as_str()).unwrap_or("Unknown");
                let role = params.get("role").and_then(|v| v.as_str()).unwrap_or("Unknown");
                format!("Add new signer '{}' with role '{}'", signer_id, role)
            }
            "remove_signer" => {
                let signer_id = params.get("signer_id").and_then(|v| v.as_str()).unwrap_or("Unknown");
                format!("Remove signer '{}'", signer_id)
            }
            "update_signer" => {
                let signer_id = params.get("signer_id").and_then(|v| v.as_str()).unwrap_or("Unknown");
                format!("Update signer '{}' permissions/settings", signer_id)
            }
            _ => format!("Execute command: {}", command_type),
        }
    }

    /// Build executable command from challenge
    fn build_command_from_challenge(&self, challenge: &PendingChallenge) -> AuthResult<MqttCommand> {
        match challenge.command_type.as_str() {
            "set_threshold" => {
                let line = challenge.params.get("line")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing line".to_string()))? as u8;

                let thresholds = challenge.params.get("thresholds")
                    .ok_or_else(|| AuthError::InvalidCommand("Missing thresholds".to_string()))?;

                Ok(MqttCommand::SetSensorThreshold {
                    line,
                    critical_low: thresholds["critical_low"].as_f64().unwrap_or(0.0) as f32,
                    alarm_low: thresholds["alarm_low"].as_f64().unwrap_or(0.0) as f32,
                    warning_low: thresholds["warning_low"].as_f64().unwrap_or(0.0) as f32,
                    warning_high: thresholds["warning_high"].as_f64().unwrap_or(0.0) as f32,
                    alarm_high: thresholds["alarm_high"].as_f64().unwrap_or(0.0) as f32,
                    critical_high: thresholds["critical_high"].as_f64().unwrap_or(0.0) as f32,
                })
            }
            "set_sensor_name" => {
                let line = challenge.params.get("line")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing line".to_string()))? as u8;

                let name = challenge.params.get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing name".to_string()))?
                    .to_string();

                Ok(MqttCommand::SetSensorName { line, name })
            }
            "restart_application" => {
                let reason = challenge.reason.clone().unwrap_or_else(|| "Remote configuration".to_string());
                Ok(MqttCommand::RestartApplication { reason })
            }
            "set_interval" => {
                let sample_interval_ms = challenge.params.get("sample_interval_ms")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing sample_interval_ms".to_string()))?;

                let aggregation_interval_ms = challenge.params.get("aggregation_interval_ms")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing aggregation_interval_ms".to_string()))?;

                let report_interval_ms = challenge.params.get("report_interval_ms")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing report_interval_ms".to_string()))?;

                Ok(MqttCommand::SetInterval {
                    sample_interval_ms,
                    aggregation_interval_ms,
                    report_interval_ms,
                })
            }
            "add_signer" => {
                Ok(MqttCommand::AddSigner {
                    signer_data: challenge.params.clone(),
                })
            }
            "remove_signer" => {
                let signer_id = challenge.params.get("signer_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing signer_id".to_string()))?
                    .to_string();

                Ok(MqttCommand::RemoveSigner { signer_id })
            }
            "update_signer" => {
                let signer_id = challenge.params.get("signer_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing signer_id".to_string()))?
                    .to_string();

                let changes = challenge.params.get("changes")
                    .ok_or_else(|| AuthError::InvalidCommand("Missing changes".to_string()))?
                    .clone();

                Ok(MqttCommand::UpdateSigner {
                    signer_id,
                    changes,
                })
            }
            _ => Err(AuthError::InvalidCommand(format!(
                "Unsupported command type: {}",
                challenge.command_type
            ))),
        }
    }

    /// Log config request to database
    fn log_config_request(
        &self,
        challenge_id: &str,
        request_id: &str,
        verification: &VerificationResult,
        command_type: &str,
        params: &Value,
        reason: &Option<String>,
        signature: &str,
        nonce: &str,
        timestamp: i64,
    ) -> AuthResult<()> {
        let conn = Connection::open(&self.db_path)
            .map_err(|e| AuthError::DatabaseError(format!("Failed to open database: {}", e)))?;

        let command_json = json!({
            "command_type": command_type,
            "params": params,
            "reason": reason,
        });

        conn.execute(
            "INSERT INTO config_changes (
                timestamp, challenge_id, request_id, signer_id, signer_name,
                command_type, command_json, signature_base64, nonce, verification_status, applied
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                timestamp,
                challenge_id,
                request_id,
                &verification.signer_id,
                &verification.signer_name,
                command_type,
                command_json.to_string(),
                signature,
                nonce,
                "PENDING",
                0,
            ],
        )
        .map_err(|e| AuthError::DatabaseError(format!("Failed to log request: {}", e)))?;

        Ok(())
    }

    /// Log config response to database
    fn log_config_response(&self, challenge_id: &str, confirmation: &str, _timestamp: i64) -> AuthResult<()> {
        let conn = Connection::open(&self.db_path)
            .map_err(|e| AuthError::DatabaseError(format!("Failed to open database: {}", e)))?;

        let status = if confirmation == "APPROVED" {
            "SUCCESS"
        } else {
            "REJECTED"
        };

        let applied = if confirmation == "APPROVED" { 1 } else { 0 };

        conn.execute(
            "UPDATE config_changes SET verification_status = ?, applied = ? WHERE challenge_id = ?",
            rusqlite::params![status, applied, challenge_id],
        )
        .map_err(|e| AuthError::DatabaseError(format!("Failed to log response: {}", e)))?;

        Ok(())
    }

    /// Log challenge expiry
    fn log_challenge_expired(&self, challenge: &PendingChallenge) -> AuthResult<()> {
        let conn = Connection::open(&self.db_path)
            .map_err(|e| AuthError::DatabaseError(format!("Failed to open database: {}", e)))?;

        conn.execute(
            "UPDATE config_changes SET verification_status = ?, error_msg = ? WHERE challenge_id = ?",
            rusqlite::params!["EXPIRED", "Challenge timed out", &challenge.challenge_id],
        )
        .map_err(|e| AuthError::DatabaseError(format!("Failed to log expiry: {}", e)))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_type_to_permission() {
        let manager = create_test_manager();

        assert_eq!(
            manager.command_type_to_permission("set_threshold").unwrap(),
            "set_threshold"
        );

        assert_eq!(
            manager.command_type_to_permission("restart_application").unwrap(),
            "restart_application"
        );

        assert!(manager.command_type_to_permission("unknown").is_err());
    }

    fn create_test_manager() -> AuthorizationManager {
        // This is a placeholder - in real tests you'd need to set up the full verifier
        // For now, just create a manager with minimal setup
        use crate::libs::crypto::{CARegistry, NonceTracker, SignatureVerifier};
        use std::path::Path;

        let ca_registry = Arc::new(Mutex::new(
            CARegistry::load_from_file(Path::new("/tmp/test_ca_registry.yaml")).unwrap(),
        ));
        let nonce_tracker = Arc::new(Mutex::new(
            NonceTracker::new(Path::new("/tmp/test_nonces.db"), 600, 100).unwrap(),
        ));
        let verifier = Arc::new(SignatureVerifier::new(ca_registry, nonce_tracker, 300));

        AuthorizationManager::new(verifier, Path::new("/tmp/test_audit.db"), 300, 10)
    }
}
