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
            AuthError::SignatureVerificationFailed(msg) => {
                write!(f, "Signature verification failed: {}", msg)
            }
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
            eprintln!(
                "[AuthManager] Warning: Failed to initialize audit database: {}",
                e
            );
        }

        Self {
            verifier,
            challenges: Arc::new(Mutex::new(ChallengeRegistry::new(
                max_concurrent_challenges,
            ))),
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
        )
        .map_err(|e| format!("Failed to create config_changes table: {}", e))?;

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
        let mut registry = self.challenges.lock().unwrap_or_else(|e| e.into_inner());
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
        let mut registry = self.challenges.lock().unwrap_or_else(|e| e.into_inner());
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
        let mut registry = self.challenges.lock().unwrap_or_else(|e| e.into_inner());
        registry.remove_challenge(&challenge_id);

        Ok((response_msg, command_to_execute))
    }

    /// Cleanup expired challenges
    pub fn cleanup_expired_challenges(&self) -> usize {
        let mut registry = self.challenges.lock().unwrap_or_else(|e| e.into_inner());
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
        self.challenges
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active_count()
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
        msg.insert(
            "reason",
            match reason {
                Some(r) => Value::String(r.clone()),
                None => Value::Null,
            },
        );
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
            "set_sensor_location" => "set_sensor_location",
            "set_alarm_pattern" => "set_alarm_pattern",
            "set_screen" => "set_screen",
            "flush_storage" => "flush_storage",
            "restart_application" => "restart_application",
            "power_off" => "power_off_device",
            // Deliberately its own permission, not a reuse of
            // restart_application/power_off_device: a certificate that authorizes
            // a reboot or power-off must not thereby authorize wiping the
            // device's data. This exact string must match the Python side's
            // Permission.FACTORY_RESET.value, or a correctly signed factory_reset
            // is rejected here with PermissionDenied while the viewer believes
            // it succeeded.
            "factory_reset" => "factory_reset",
            "set_interval" => "set_interval",
            "set_system_info_interval" => "set_system_info_interval",
            "get_info" => "get_info",
            "get_status" => "get_status",
            "add_signer" => "add_signer",
            "remove_signer" => "remove_signer",
            "update_signer" => "update_signer",
            "set_device_label" => "set_device_label",
            "set_led_brightness" => "set_led_brightness",
            "set_screen_brightness" => "set_screen_brightness",
            "set_screen_timeout" => "set_screen_brightness", // reuse: screen-control permission (works with existing certs)
            // Deliberately NOT the screen-control permission. That one covers how
            // brightly and how long the panel is lit; this one decides which
            // measurements the panel lists at all, which is a different capability
            // on a Class IIa device. The cost is accepted: certificates issued
            // before this permission existed do not carry it and must be reissued.
            "set_display_lines" => "set_display_lines",
            "set_buzzer_volume" => "set_buzzer_volume",
            "set_network_config" => "set_network_config",
            "set_lorawan_sensor_config" => "set_lorawan_sensor_config",
            "add_lorawan_sticker" => "set_lorawan_sensor_config", // reuse same permission
            "remove_lorawan_sticker" => "set_lorawan_sensor_config", // reuse same permission
            "add_external_gateway" => "set_lorawan_sensor_config", // reuse same permission
            "remove_external_gateway" => "set_lorawan_sensor_config", // reuse same permission
            // system#7. Reuses the node-management permission — a signer
            // certificate embeds a fixed permission list at issuance, so a new
            // one would invalidate every certificate already provisioned across
            // the fleet, and it raises no real bar: whoever can already register
            // an external gateway can already decide which radios feed this
            // device's ChirpStack. The viewer's CommandSigner maps it the same
            // way.
            "set_lorawan_cluster" => "set_lorawan_sensor_config",
            // Switching the subsystem on/off is a gateway-scoped config change, so
            // it takes the same permission as the tag operations it gates.
            "set_eye_enabled" => "set_lorawan_sensor_config",
            // Same permission as set_eye_enabled: both are gateway-scoped EYE
            // subsystem switches, and a signer certificate embeds a fixed
            // permission list at issuance — a new permission would invalidate
            // every certificate already provisioned. Matches the viewer's
            // CommandSigner.COMMAND_PERMISSIONS["set_eye_config"].
            "set_eye_config" => "set_lorawan_sensor_config",
            "set_eye_recording" => "set_lorawan_sensor_config", // reuse: sensor config change
            "download_eye_history" => "set_lorawan_sensor_config", // reuse: sensor data op
            "add_eye_tag" => "set_lorawan_sensor_config",       // reuse: sensor config change
            "remove_eye_tag" => "set_lorawan_sensor_config",    // reuse: sensor config change
            "detect_eye_tag" => "set_lorawan_sensor_config",    // reuse: sensor data op
            // system#6. Not a registration — it only widens what this gateway
            // listens for — but it is still a fleet-scoped write, so it takes the
            // same permission as adding a tag rather than a read permission.
            "set_eye_known_tags" => "set_lorawan_sensor_config",
            "reset_export_cursor" => "set_lorawan_sensor_config", // admin op: align with node management

            "set_lorawan_field_threshold" => "set_threshold",
            "delete_lorawan_field_threshold" => "set_threshold",
            "set_sticker_config" => "set_lorawan_sensor_config", // reuse node-management permission
            "send_sticker_raw" => "set_lorawan_sensor_config",   // reuse node-management permission
            "set_eye_field_threshold" => "set_lorawan_sensor_config",
            "delete_eye_field_threshold" => "set_lorawan_sensor_config",
            // #71 control commands. They reuse the node-management permission
            // rather than introducing a new one, because anyone who can already
            // write node config can already reboot the device via
            // SetParam{save:true} — a dedicated permission would raise no real bar
            // while forcing every provisioned signer certificate to be re-issued.
            // Tightening this is a follow-up, not a prerequisite.
            "sticker_reboot" => "set_lorawan_sensor_config",
            "sticker_device_reset" => "set_lorawan_sensor_config",
            "sticker_reset_counters" => "set_lorawan_sensor_config",
            "sticker_clock_sync" => "set_lorawan_sensor_config",
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
            "set_sensor_location" => {
                let line = params.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
                let location = params.get("location").and_then(|v| v.as_str()).unwrap_or("");
                format!("Change sensor line {} location to \"{}\"", line, location)
            }
            "restart_application" => "Reboot the device".to_string(),
            "power_off" => "Power the device down. It will stop monitoring, go dark and stay silent, and it will start monitoring again on its own once PoE power is reconnected".to_string(),
            "factory_reset" => {
                // This is the preview text a signer confirms against before the
                // wipe is irreversible, so it must name the real post-action
                // rather than assume one. No default here mirrors
                // parse_factory_reset's own refusal to default post_action.
                let action_text = match params.get("post_action").and_then(|v| v.as_str()) {
                    Some("power_off") => "power off",
                    Some("reboot") => "reboot",
                    Some(other) => other,
                    None => "reboot or power off",
                };
                format!(
                    "FACTORY RESET: erase all device data — measurement database, \
                     configuration, viewer pairing, TLS and broker credentials, \
                     LoRaWAN/EYE registrations, BLE PIN — then {}. QBEE fleet \
                     enrolment is preserved. The device will be unpaired and cannot \
                     be recovered from the Viewer.",
                    action_text
                )
            }
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
            "set_device_label" => {
                let label = params.get("label").and_then(|v| v.as_str()).unwrap_or("");
                format!("Change device label to \"{}\"", label)
            }
            "set_led_brightness" => {
                let brightness = params.get("brightness").and_then(|v| v.as_u64()).unwrap_or(50);
                format!("Set LED brightness to {}%", brightness)
            }
            "set_screen_brightness" => {
                let brightness = params.get("brightness").and_then(|v| v.as_u64()).unwrap_or(50);
                format!("Set screen brightness to {}%", brightness)
            }
            "set_screen_timeout" => {
                let timeout_secs = params.get("timeout_secs").and_then(|v| v.as_u64()).unwrap_or(0);
                if timeout_secs == 0 {
                    "Disable screen timeout (display always on)".to_string()
                } else {
                    format!("Set screen timeout to {}s", timeout_secs)
                }
            }
            "set_buzzer_volume" => {
                let volume = params.get("volume").and_then(|v| v.as_u64()).unwrap_or(100);
                format!("Set buzzer volume to {}%", volume)
            }
            "set_display_lines" => {
                let count = params
                    .get("lines")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                if count == 0 {
                    "Restore the built-in display layout".to_string()
                } else {
                    format!("Set {} custom display lines", count)
                }
            }
            "set_network_config" => {
                let iface = params.get("interface").and_then(|v| v.as_str()).unwrap_or("unknown");
                let cfg_type = params.get("type").and_then(|v| v.as_str()).unwrap_or("unknown");
                format!("Configure {} network to {}", iface, cfg_type)
            }
            "set_lorawan_sensor_config" => {
                let dev_eui = params.get("dev_eui").and_then(|v| v.as_str()).unwrap_or("unknown");
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                format!("Configure LoRaWAN sensor {} (name: \"{}\")", dev_eui, name)
            }
            "add_lorawan_sticker" => {
                let dev_eui = params.get("dev_eui").and_then(|v| v.as_str()).unwrap_or("unknown");
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let mode = params.get("mode").and_then(|v| v.as_str()).unwrap_or("?").to_uppercase();
                format!("Add HARDWARIO STICKER {} via {} (name: \"{}\")", dev_eui, mode, name)
            }
            "add_external_gateway" => {
                let gateway_eui = params.get("gateway_eui").and_then(|v| v.as_str()).unwrap_or("unknown");
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                format!("Add external LoRaWAN gateway {} (name: \"{}\")", gateway_eui, name)
            }
            "remove_external_gateway" => {
                let gateway_eui = params.get("gateway_eui").and_then(|v| v.as_str()).unwrap_or("unknown");
                format!("Remove external LoRaWAN gateway {}", gateway_eui)
            }
            "remove_lorawan_sticker" => {
                let dev_eui = params.get("dev_eui").and_then(|v| v.as_str()).unwrap_or("unknown");
                format!("Remove HARDWARIO STICKER {}", dev_eui)
            }
            "set_sticker_config" => {
                let dev_eui = params.get("dev_eui").and_then(|v| v.as_str()).unwrap_or("unknown");
                let keys: Vec<&str> = params
                    .get("config")
                    .and_then(|v| v.as_object())
                    .map(|o| o.keys().map(|s| s.as_str()).collect())
                    .unwrap_or_default();
                format!("Configure STICKER {} parameters: {:?}", dev_eui, keys)
            }
            "send_sticker_raw" => {
                let dev_eui = params.get("dev_eui").and_then(|v| v.as_str()).unwrap_or("unknown");
                let hex = params.get("hex").and_then(|v| v.as_str()).unwrap_or("");
                format!("Send raw downlink to STICKER {} ({} hex chars)", dev_eui, hex.len())
            }
            // These strings are what an operator actually reads before confirming a
            // signed command, so they state the real consequence rather than the
            // command name.
            "sticker_reboot" => {
                let dev_eui = params.get("dev_eui").and_then(|v| v.as_str()).unwrap_or("unknown");
                format!(
                    "Reboot STICKER {} — cold restart about 8 s after the acknowledgement. \
                     Any staged but unsaved configuration is discarded; saved settings, \
                     alarm rules and counters are unaffected.",
                    dev_eui
                )
            }
            "sticker_device_reset" => {
                let dev_eui = params.get("dev_eui").and_then(|v| v.as_str()).unwrap_or("unknown");
                format!(
                    "Reset STICKER {} to defaults. Identity and the full LoRaWAN \
                     configuration are KEPT, so the device stays joined and needs no \
                     re-provisioning — but ALL sensor capabilities, alarm rules, intervals \
                     and history settings are lost. This is not the same as an NFC factory \
                     reset, which cannot be performed over the radio.",
                    dev_eui
                )
            }
            "sticker_reset_counters" => {
                let dev_eui = params.get("dev_eui").and_then(|v| v.as_str()).unwrap_or("unknown");
                let mut chans: Vec<&str> = Vec::new();
                if params.get("all").and_then(|v| v.as_bool()) == Some(true) {
                    chans = vec!["hall_left", "hall_right", "input_a", "input_b"];
                } else if let Some(arr) = params.get("counters").and_then(|v| v.as_array()) {
                    chans = arr.iter().filter_map(|v| v.as_str()).collect();
                }
                format!(
                    "Reset STICKER {} pulse counters: {:?}. Irreversible — the totals are \
                     zeroed and persisted. Does NOT reset motion_count or \
                     accel_motion_count, which are RAM-only on the device.",
                    dev_eui, chans
                )
            }
            "sticker_clock_sync" => {
                let dev_eui = params.get("dev_eui").and_then(|v| v.as_str()).unwrap_or("unknown");
                match params.get("unix_time").and_then(|v| v.as_u64()) {
                    Some(t) => format!(
                        "Set STICKER {} clock to Unix time {}. Absolute history timestamps \
                         depend on this.",
                        dev_eui, t
                    ),
                    None => format!(
                        "Ask STICKER {} to re-sync its clock from the network. No immediate \
                         reply — the device reports the result in a later device-info uplink.",
                        dev_eui
                    ),
                }
            }
            "set_eye_enabled" => {
                let enabled = params.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
                format!(
                    "{} the EYE BLE tag subsystem",
                    if enabled { "Enable" } else { "Disable" }
                )
            }
            "set_eye_config" => {
                let mut parts = Vec::new();
                if let Some(v) = params.get("auto_provision").and_then(|v| v.as_bool()) {
                    parts.push(format!("auto-provision={}", if v { "on" } else { "off" }));
                }
                if let Some(v) = params.get("auto_discover").and_then(|v| v.as_bool()) {
                    parts.push(format!("auto-discover={}", if v { "on" } else { "off" }));
                }
                format!("Set EYE subsystem ({})", parts.join(", "))
            }
            "set_eye_recording" => {
                let mac = params.get("mac").and_then(|v| v.as_str()).unwrap_or("unknown");
                let interval = params.get("interval_min").and_then(|v| v.as_u64()).unwrap_or(0);
                format!("Set EYE {} recording interval to {} min", mac, interval)
            }
            "download_eye_history" => {
                let mac = params.get("mac").and_then(|v| v.as_str()).unwrap_or("unknown");
                format!("Download EYE {} temperature history", mac)
            }
            "add_eye_tag" => {
                let mac = params.get("mac").and_then(|v| v.as_str()).unwrap_or("unknown");
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                format!("Add EYE tag {} (name: \"{}\")", mac, name)
            }
            "remove_eye_tag" => {
                let mac = params.get("mac").and_then(|v| v.as_str()).unwrap_or("unknown");
                format!("Remove EYE tag {}", mac)
            }
            "detect_eye_tag" => {
                let mac = params.get("mac").and_then(|v| v.as_str()).unwrap_or("unknown");
                format!("Detect EYE tag type for {}", mac)
            }
            "set_eye_field_threshold" => {
                let mac = params.get("mac").and_then(|v| v.as_str()).unwrap_or("unknown");
                let field = params.get("field").and_then(|v| v.as_str()).unwrap_or("unknown");
                format!("Set EYE {} {} alarm thresholds", mac, field)
            }
            "delete_eye_field_threshold" => {
                let mac = params.get("mac").and_then(|v| v.as_str()).unwrap_or("unknown");
                let field = params.get("field").and_then(|v| v.as_str()).unwrap_or("unknown");
                format!("Clear EYE {} {} alarm thresholds", mac, field)
            }
            "set_eye_known_tags" => {
                let n = params
                    .get("macs")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                // The count, not the list: a fleet allowlist is long enough that
                // spelling it out would bury the rest of the confirmation prompt.
                format!("Listen for {} fleet-known EYE tag(s)", n)
            }
            _ => format!("Execute command: {}", command_type),
        }
    }

    /// Build executable command from challenge
    fn build_command_from_challenge(
        &self,
        challenge: &PendingChallenge,
    ) -> AuthResult<MqttCommand> {
        match challenge.command_type.as_str() {
            "set_threshold" => {
                let line = challenge
                    .params
                    .get("line")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing line".to_string()))?
                    as u8;

                let thresholds = challenge
                    .params
                    .get("thresholds")
                    .ok_or_else(|| AuthError::InvalidCommand("Missing thresholds".to_string()))?;

                Ok(MqttCommand::SetSensorThreshold {
                    line,
                    critical_low: thresholds["critical_low"].as_f64().unwrap_or(0.0) as f32,
                    alarm_low: thresholds
                        .get("alarm_low")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0) as f32,
                    warning_low: thresholds["warning_low"].as_f64().unwrap_or(0.0) as f32,
                    warning_high: thresholds["warning_high"].as_f64().unwrap_or(0.0) as f32,
                    alarm_high: thresholds
                        .get("alarm_high")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(100.0) as f32,
                    critical_high: thresholds["critical_high"].as_f64().unwrap_or(0.0) as f32,
                })
            }
            "set_sensor_name" => {
                let line = challenge
                    .params
                    .get("line")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing line".to_string()))?
                    as u8;

                let name = challenge
                    .params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing name".to_string()))?
                    .to_string();

                Ok(MqttCommand::SetSensorName { line, name })
            }
            "set_sensor_location" => {
                let line = challenge
                    .params
                    .get("line")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing line".to_string()))?
                    as u8;

                let location = challenge
                    .params
                    .get("location")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing location".to_string()))?
                    .to_string();

                Ok(MqttCommand::SetSensorLocation { line, location })
            }
            "restart_application" => {
                let reason = challenge
                    .reason
                    .clone()
                    .unwrap_or_else(|| "Remote configuration".to_string());
                Ok(MqttCommand::RestartApplication {
                    reason,
                    requested_by: challenge.signer_id.clone(),
                })
            }
            "power_off" => {
                let reason = challenge
                    .reason
                    .clone()
                    .unwrap_or_else(|| "Remote power-off".to_string());
                Ok(MqttCommand::PowerOffDevice {
                    reason,
                    requested_by: challenge.signer_id.clone(),
                })
            }
            // No reason default here, unlike restart_application/power_off above:
            // parse_factory_reset requires an explicit, non-blank reason and
            // rejects a missing one, because that reason (carried in the command,
            // not looked up from the audit log later) is the only surviving
            // record of who authorized the wipe and why once it runs.
            "factory_reset" => MqttCommand::parse_factory_reset(
                &challenge.params,
                challenge.reason.as_deref(),
                &challenge.signer_id,
                &challenge.request_id,
            )
            .map_err(AuthError::InvalidCommand),
            "set_interval" => {
                let sample_interval_ms = challenge
                    .params
                    .get("sample_interval_ms")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| {
                        AuthError::InvalidCommand("Missing sample_interval_ms".to_string())
                    })?;

                let aggregation_interval_ms = challenge
                    .params
                    .get("aggregation_interval_ms")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| {
                        AuthError::InvalidCommand("Missing aggregation_interval_ms".to_string())
                    })?;

                let report_interval_ms = challenge
                    .params
                    .get("report_interval_ms")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| {
                        AuthError::InvalidCommand("Missing report_interval_ms".to_string())
                    })?;

                Ok(MqttCommand::SetInterval {
                    sample_interval_ms,
                    aggregation_interval_ms,
                    report_interval_ms,
                })
            }
            "set_system_info_interval" => {
                let interval_seconds = challenge
                    .params
                    .get("interval_seconds")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| {
                        AuthError::InvalidCommand("Missing interval_seconds".to_string())
                    })?;

                Ok(MqttCommand::SetSystemInfoInterval { interval_seconds })
            }
            "add_signer" => Ok(MqttCommand::AddSigner {
                signer_data: challenge.params.clone(),
            }),
            "remove_signer" => {
                let signer_id = challenge
                    .params
                    .get("signer_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing signer_id".to_string()))?
                    .to_string();

                Ok(MqttCommand::RemoveSigner { signer_id })
            }
            "update_signer" => {
                let signer_id = challenge
                    .params
                    .get("signer_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing signer_id".to_string()))?
                    .to_string();

                let changes = challenge
                    .params
                    .get("changes")
                    .ok_or_else(|| AuthError::InvalidCommand("Missing changes".to_string()))?
                    .clone();

                Ok(MqttCommand::UpdateSigner { signer_id, changes })
            }
            "set_device_label" => {
                let label = challenge
                    .params
                    .get("label")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing label".to_string()))?
                    .to_string();

                Ok(MqttCommand::SetDeviceLabel { label })
            }
            "set_led_brightness" => {
                let brightness = challenge
                    .params
                    .get("brightness")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing brightness".to_string()))?
                    as u8;

                // Validate range
                if brightness > 100 {
                    return Err(AuthError::InvalidCommand(
                        "Brightness must be 0-100".to_string(),
                    ));
                }

                Ok(MqttCommand::SetLedBrightness { brightness })
            }
            "set_screen_brightness" => {
                let brightness = challenge
                    .params
                    .get("brightness")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing brightness".to_string()))?
                    as u8;

                // Validate range
                if brightness > 100 {
                    return Err(AuthError::InvalidCommand(
                        "Brightness must be 0-100".to_string(),
                    ));
                }

                Ok(MqttCommand::SetScreenBrightness { brightness })
            }
            "set_screen_timeout" => {
                let timeout_secs = challenge
                    .params
                    .get("timeout_secs")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing timeout_secs".to_string()))?;

                // 0 disables the timeout (display always on). There is no
                // practical upper bound beyond what the u32 field can hold, so
                // only guard the u64 -> u32 cast.
                if timeout_secs > u64::from(u32::MAX) {
                    return Err(AuthError::InvalidCommand(
                        "Screen timeout out of range".to_string(),
                    ));
                }

                Ok(MqttCommand::SetScreenTimeout {
                    timeout_secs: timeout_secs as u32,
                })
            }
            "set_display_lines" => {
                let raw = challenge
                    .params
                    .get("lines")
                    .ok_or_else(|| AuthError::InvalidCommand("Missing lines".to_string()))?;

                // Same `Deserialize` derive as the YAML path — one schema, two
                // encodings, no second parser to keep in sync. Deliberately NOT
                // the lenient on-disk deserializer: dropping a malformed line
                // silently is right when the alternative is failing to boot, but
                // on a command a malformed line must be a loud rejection.
                let lines: Vec<crate::libs::config::DisplayLine> =
                    serde_json::from_value(raw.clone()).map_err(|e| {
                        AuthError::InvalidCommand(format!("Invalid display lines: {}", e))
                    })?;

                // Validate here rather than only in the applier so the operator
                // learns it's wrong before the confirm round-trip.
                crate::libs::config_applier::validation::validate_display_custom_lines(&lines)
                    .map_err(AuthError::InvalidCommand)?;

                Ok(MqttCommand::SetDisplayLines { lines })
            }
            "set_buzzer_volume" => {
                let volume = challenge
                    .params
                    .get("volume")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing volume".to_string()))?
                    as u8;

                // Validate range
                if volume > 100 {
                    return Err(AuthError::InvalidCommand(
                        "Volume must be 0-100".to_string(),
                    ));
                }

                Ok(MqttCommand::SetBuzzerVolume { volume })
            }
            "set_network_config" => {
                let interface = challenge
                    .params
                    .get("interface")
                    .and_then(|v| v.as_str())
                    .unwrap_or("ethernet")
                    .to_string();

                let config_type = challenge
                    .params
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("dhcp")
                    .to_string();

                let ip_address = challenge
                    .params
                    .get("ip_address")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                let subnet_mask = challenge
                    .params
                    .get("subnet_mask")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                let gateway = challenge
                    .params
                    .get("gateway")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                let dns_primary = challenge
                    .params
                    .get("dns_primary")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                let dns_secondary = challenge
                    .params
                    .get("dns_secondary")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                Ok(MqttCommand::SetNetworkConfig {
                    interface,
                    config_type,
                    ip_address,
                    subnet_mask,
                    gateway,
                    dns_primary,
                    dns_secondary,
                })
            }
            "set_lorawan_sensor_config" => {
                let dev_eui = challenge
                    .params
                    .get("dev_eui")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing dev_eui".to_string()))?
                    .to_lowercase();

                let name = challenge
                    .params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                let serial_number = challenge
                    .params
                    .get("serial_number")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                let location = challenge
                    .params
                    .get("location")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                Ok(MqttCommand::SetLoRaWANSensorConfig {
                    dev_eui,
                    name,
                    serial_number,
                    location,
                })
            }
            "set_lorawan_field_threshold" => {
                let dev_eui = challenge
                    .params
                    .get("dev_eui")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing dev_eui".to_string()))?
                    .to_lowercase();
                let field = challenge
                    .params
                    .get("field")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing field".to_string()))?
                    .to_string();
                let critical_low = challenge
                    .params
                    .get("critical_low")
                    .and_then(|v| v.as_f64());
                let warning_low = challenge.params.get("warning_low").and_then(|v| v.as_f64());
                let warning_high = challenge
                    .params
                    .get("warning_high")
                    .and_then(|v| v.as_f64());
                let critical_high = challenge
                    .params
                    .get("critical_high")
                    .and_then(|v| v.as_f64());
                // Absent means armed: an older viewer does not send the flag and
                // must keep the behaviour it has always had.
                let enabled = challenge
                    .params
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                Ok(MqttCommand::SetLoRaWANFieldThreshold {
                    dev_eui,
                    field,
                    critical_low,
                    warning_low,
                    warning_high,
                    critical_high,
                    enabled,
                })
            }
            "delete_lorawan_field_threshold" => {
                let dev_eui = challenge
                    .params
                    .get("dev_eui")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing dev_eui".to_string()))?
                    .to_lowercase();
                let field = challenge
                    .params
                    .get("field")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing field".to_string()))?
                    .to_string();
                Ok(MqttCommand::DeleteLoRaWANFieldThreshold { dev_eui, field })
            }
            "set_sticker_config" => MqttCommand::parse_set_node_config(&challenge.params)
                .map_err(AuthError::InvalidCommand),
            "send_sticker_raw" => MqttCommand::parse_send_node_raw(&challenge.params)
                .map_err(AuthError::InvalidCommand),
            "set_eye_field_threshold" => {
                let mac = challenge
                    .params
                    .get("mac")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing mac".to_string()))?
                    .to_uppercase();
                if !crate::libs::beacon::state::is_valid_mac(&mac) {
                    return Err(AuthError::InvalidCommand(format!(
                        "Invalid MAC address: {mac}"
                    )));
                }
                let field = challenge
                    .params
                    .get("field")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing field".to_string()))?
                    .to_string();
                let critical_low = challenge
                    .params
                    .get("critical_low")
                    .and_then(|v| v.as_f64());
                let warning_low = challenge.params.get("warning_low").and_then(|v| v.as_f64());
                let warning_high = challenge
                    .params
                    .get("warning_high")
                    .and_then(|v| v.as_f64());
                let critical_high = challenge
                    .params
                    .get("critical_high")
                    .and_then(|v| v.as_f64());
                Ok(MqttCommand::SetBeaconFieldThreshold {
                    mac,
                    field,
                    critical_low,
                    warning_low,
                    warning_high,
                    critical_high,
                })
            }
            "delete_eye_field_threshold" => {
                let mac = challenge
                    .params
                    .get("mac")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing mac".to_string()))?
                    .to_uppercase();
                if !crate::libs::beacon::state::is_valid_mac(&mac) {
                    return Err(AuthError::InvalidCommand(format!(
                        "Invalid MAC address: {mac}"
                    )));
                }
                let field = challenge
                    .params
                    .get("field")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing field".to_string()))?
                    .to_string();
                Ok(MqttCommand::DeleteBeaconFieldThreshold { mac, field })
            }
            "sticker_reboot" => {
                MqttCommand::parse_node_reboot(&challenge.params).map_err(AuthError::InvalidCommand)
            }
            "sticker_device_reset" => MqttCommand::parse_node_device_reset(&challenge.params)
                .map_err(AuthError::InvalidCommand),
            "sticker_reset_counters" => MqttCommand::parse_node_reset_counters(&challenge.params)
                .map_err(AuthError::InvalidCommand),
            "sticker_clock_sync" => MqttCommand::parse_node_clock_sync(&challenge.params)
                .map_err(AuthError::InvalidCommand),
            "add_lorawan_sticker" => {
                let dev_eui = challenge
                    .params
                    .get("dev_eui")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing dev_eui".to_string()))?
                    .to_lowercase();

                let name = challenge
                    .params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing name".to_string()))?
                    .to_string();

                let serial_number = challenge
                    .params
                    .get("serial_number")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing serial_number".to_string()))?
                    .to_string();

                let mode = challenge
                    .params
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing mode".to_string()))?;

                let activation = match mode {
                    "otaa" => {
                        let app_key = challenge
                            .params
                            .get("app_key")
                            .and_then(|v| v.as_str())
                            .ok_or_else(|| {
                                AuthError::InvalidCommand("Missing app_key for OTAA".to_string())
                            })?
                            .to_string();
                        if app_key.len() != 32 || !app_key.chars().all(|c| c.is_ascii_hexdigit()) {
                            return Err(AuthError::InvalidCommand(
                                "app_key must be exactly 32 hex characters".to_string(),
                            ));
                        }
                        // join_eui: required from new viewers; absent payloads
                        // (legacy viewer) fall back to all-zeros for compatibility.
                        let join_eui = challenge
                            .params
                            .get("join_eui")
                            .and_then(|v| v.as_str())
                            .unwrap_or("0000000000000000")
                            .to_string();
                        if join_eui.len() != 16 || !join_eui.chars().all(|c| c.is_ascii_hexdigit())
                        {
                            return Err(AuthError::InvalidCommand(
                                "join_eui must be exactly 16 hex characters".to_string(),
                            ));
                        }
                        // Vendor profile from the QR label, when the viewer read
                        // one. Absent for manual entry — the operator never sees
                        // a profile number, so there is nothing to type.
                        let profile_id = match challenge.params.get("profile_id") {
                            None | Some(serde_json::Value::Null) => None,
                            Some(v) => {
                                let n = v.as_u64().ok_or_else(|| {
                                    AuthError::InvalidCommand(
                                        "profile_id must be a number".to_string(),
                                    )
                                })?;
                                if !(1..=99).contains(&n) {
                                    return Err(AuthError::InvalidCommand(
                                        "profile_id must be between 1 and 99".to_string(),
                                    ));
                                }
                                Some(n as u32)
                            }
                        };
                        crate::libs::mqtt::messages::ActivationMode::Otaa {
                            app_key,
                            join_eui,
                            profile_id,
                        }
                    }
                    "abp" => {
                        let devaddr = challenge
                            .params
                            .get("devaddr")
                            .and_then(|v| v.as_str())
                            .ok_or_else(|| {
                                AuthError::InvalidCommand("Missing devaddr for ABP".to_string())
                            })?
                            .to_string();
                        let nwkskey = challenge
                            .params
                            .get("nwkskey")
                            .and_then(|v| v.as_str())
                            .ok_or_else(|| {
                                AuthError::InvalidCommand("Missing nwkskey for ABP".to_string())
                            })?
                            .to_string();
                        let appskey = challenge
                            .params
                            .get("appskey")
                            .and_then(|v| v.as_str())
                            .ok_or_else(|| {
                                AuthError::InvalidCommand("Missing appskey for ABP".to_string())
                            })?
                            .to_string();
                        if devaddr.len() != 8 || !devaddr.chars().all(|c| c.is_ascii_hexdigit()) {
                            return Err(AuthError::InvalidCommand("devaddr must be 8 hex".into()));
                        }
                        if nwkskey.len() != 32 || !nwkskey.chars().all(|c| c.is_ascii_hexdigit()) {
                            return Err(AuthError::InvalidCommand("nwkskey must be 32 hex".into()));
                        }
                        if appskey.len() != 32 || !appskey.chars().all(|c| c.is_ascii_hexdigit()) {
                            return Err(AuthError::InvalidCommand("appskey must be 32 hex".into()));
                        }
                        crate::libs::mqtt::messages::ActivationMode::Abp {
                            devaddr,
                            nwkskey,
                            appskey,
                        }
                    }
                    other => {
                        return Err(AuthError::InvalidCommand(format!(
                            "Unknown activation mode: {}",
                            other
                        )));
                    }
                };

                Ok(MqttCommand::AddLoRaWANNode {
                    dev_eui,
                    name,
                    serial_number,
                    activation,
                })
            }
            "remove_lorawan_sticker" => {
                let dev_eui = challenge
                    .params
                    .get("dev_eui")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing dev_eui".to_string()))?
                    .to_lowercase();

                Ok(MqttCommand::RemoveLoRaWANNode { dev_eui })
            }
            "add_external_gateway" => {
                let gateway_eui = challenge
                    .params
                    .get("gateway_eui")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing gateway_eui".to_string()))?;
                // normalize_eui validates 16 hex + lowercases; reject a bad EUI early.
                let gateway_eui = crate::libs::lorawan::provisioning::normalize_eui(gateway_eui)
                    .map_err(AuthError::InvalidCommand)?;

                let name = challenge
                    .params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing name".to_string()))?
                    .to_string();

                Ok(MqttCommand::AddExternalGateway { gateway_eui, name })
            }
            "remove_external_gateway" => {
                let gateway_eui = challenge
                    .params
                    .get("gateway_eui")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing gateway_eui".to_string()))?;
                let gateway_eui = crate::libs::lorawan::provisioning::normalize_eui(gateway_eui)
                    .map_err(AuthError::InvalidCommand)?;

                Ok(MqttCommand::RemoveExternalGateway { gateway_eui })
            }
            "set_lorawan_cluster" => {
                // system#7 Goal 2. Validation belongs here because this is the
                // only place an MqttCommand is constructed from a challenge, so
                // the dispatch can treat the fields as already checked — the
                // same split AddExternalGateway uses for its EUI.
                let role = challenge
                    .params
                    .get("role")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing role".to_string()))?;

                let str_param = |k: &str| challenge.params.get(k).and_then(|v| v.as_str());
                let port = match challenge.params.get("leader_port") {
                    None => None,
                    Some(v) => Some(v.as_u64().filter(|p| *p <= u16::MAX as u64).ok_or_else(
                        || {
                            AuthError::InvalidCommand(
                                "leader_port must be a port number".to_string(),
                            )
                        },
                    )? as u16),
                };

                let arm = crate::libs::lorawan::cluster::validate_arm(
                    role,
                    str_param("leader_host"),
                    port,
                    str_param("leader_ca"),
                    str_param("leader_ca_fingerprint"),
                    str_param("peer_username"),
                    str_param("peer_password"),
                    str_param("peer_gateway_eui"),
                )
                .map_err(AuthError::InvalidCommand)?;

                Ok(MqttCommand::SetLorawanCluster {
                    role: arm.role.as_str().to_string(),
                    leader_host: arm.leader_host,
                    leader_port: arm.leader_port,
                    leader_ca: arm.leader_ca_pem,
                    leader_ca_fingerprint: arm.leader_ca_fingerprint,
                    peer_username: arm.peer_username,
                    peer_password: arm.peer_password,
                    peer_gateway_eui: arm.peer_gateway_eui,
                })
            }
            "set_eye_enabled" => {
                let enabled = challenge
                    .params
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing enabled".to_string()))?;
                Ok(MqttCommand::SetBeaconEnabled { enabled })
            }
            "set_eye_config" => MqttCommand::parse_set_beacon_config(&challenge.params)
                .map_err(AuthError::InvalidCommand),
            "set_eye_recording" => {
                let mac = challenge
                    .params
                    .get("mac")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing mac".to_string()))?
                    .to_uppercase();
                let interval_min = challenge
                    .params
                    .get("interval_min")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing interval_min".to_string()))?;
                if !matches!(interval_min, 0 | 1 | 5 | 15) {
                    return Err(AuthError::InvalidCommand(
                        "interval_min must be 0 (off), 1, 5 or 15".to_string(),
                    ));
                }
                Ok(MqttCommand::SetBeaconRecording {
                    mac,
                    interval_min: interval_min as u16,
                })
            }
            "download_eye_history" => {
                let mac = challenge
                    .params
                    .get("mac")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing mac".to_string()))?
                    .to_uppercase();
                Ok(MqttCommand::DownloadBeaconHistory { mac })
            }
            "add_eye_tag" => {
                let mac = challenge
                    .params
                    .get("mac")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing mac".to_string()))?
                    .to_uppercase();
                if !crate::libs::beacon::state::is_valid_mac(&mac) {
                    return Err(AuthError::InvalidCommand(format!(
                        "Invalid MAC address: {mac}"
                    )));
                }
                let name = challenge
                    .params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                Ok(MqttCommand::AddBeaconTag { mac, name })
            }
            "set_eye_known_tags" => {
                let arr = challenge
                    .params
                    .get("macs")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| {
                        AuthError::InvalidCommand("Missing or non-array 'macs'".to_string())
                    })?;
                // Validated and normalised here rather than left to the monitor, so
                // a malformed push is rejected at the door with a reason the caller
                // can act on. An empty list is legal and means "stop listening for
                // borrowed tags" — refusing it would leave no way to clear the
                // allowlist.
                let mut macs = Vec::with_capacity(arr.len());
                for v in arr {
                    let mac = v
                        .as_str()
                        .ok_or_else(|| {
                            AuthError::InvalidCommand("'macs' entries must be strings".to_string())
                        })?
                        .to_uppercase();
                    if !crate::libs::beacon::state::is_valid_mac(&mac) {
                        return Err(AuthError::InvalidCommand(format!(
                            "Invalid MAC address: {mac}"
                        )));
                    }
                    macs.push(mac);
                }
                Ok(MqttCommand::SetBeaconKnownTags { macs })
            }
            "remove_eye_tag" => {
                let mac = challenge
                    .params
                    .get("mac")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing mac".to_string()))?
                    .to_uppercase();
                if !crate::libs::beacon::state::is_valid_mac(&mac) {
                    return Err(AuthError::InvalidCommand(format!(
                        "Invalid MAC address: {mac}"
                    )));
                }
                Ok(MqttCommand::RemoveBeaconTag { mac })
            }
            "detect_eye_tag" => {
                let mac = challenge
                    .params
                    .get("mac")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing mac".to_string()))?
                    .to_uppercase();
                if !crate::libs::beacon::state::is_valid_mac(&mac) {
                    return Err(AuthError::InvalidCommand(format!(
                        "Invalid MAC address: {mac}"
                    )));
                }
                Ok(MqttCommand::DetectBeaconTag { mac })
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
    fn log_config_response(
        &self,
        challenge_id: &str,
        confirmation: &str,
        _timestamp: i64,
    ) -> AuthResult<()> {
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

        // The Viewer must issue certificates carrying this literal. It used to
        // issue "restart_device" — a name that exists nowhere in this table — so
        // every remote reboot was rejected here with PermissionDenied while the
        // Viewer's API reported success. Do not "fix" a future authorization
        // failure by loosening this to accept the old name; reissue the
        // certificate instead.
        assert_eq!(
            manager
                .command_type_to_permission("restart_application")
                .unwrap(),
            "restart_application"
        );
        // A reboot self-recovers, a power-off does not: holding one must never
        // imply the other. The Viewer asserts the same inequality.
        assert_ne!(
            manager
                .command_type_to_permission("restart_application")
                .unwrap(),
            manager.command_type_to_permission("power_off").unwrap()
        );

        // Screen timeout reuses the screen-brightness permission so it works
        // with certificates issued before the timeout command existed.
        assert_eq!(
            manager
                .command_type_to_permission("set_screen_timeout")
                .unwrap(),
            "set_screen_brightness"
        );

        assert!(manager.command_type_to_permission("unknown").is_err());
    }

    /// Every command `build_command_from_challenge` knows how to build must also
    /// have a permission, because `process_config_request` asks for the permission
    /// **first** — step 2, before the signature is even verified. A command missing
    /// from that table is rejected with "Unknown command type" and no challenge is
    /// ever created, which makes its handler, and its whole feature, unreachable.
    ///
    /// That is exactly what happened to `set_lorawan_cluster` (system#7): the
    /// handler, the dispatch, the persistent state and the UI all shipped, and the
    /// device refused the request before any of it ran. Nothing caught it because
    /// every existing test entered through `build_command_from_challenge`, which
    /// sits downstream of the check.
    ///
    /// Keep this list in step with the match arms of
    /// `build_command_from_challenge`. Adding an arm there without adding it here
    /// is the bug this test exists to catch — so extend the list, never delete
    /// from it to make the test pass.
    #[test]
    fn every_buildable_command_has_a_permission() {
        let manager = create_test_manager();

        // The command_type values `build_command_from_challenge` matches on.
        // "otaa"/"abp" are deliberately absent: they are the nested activation
        // arms inside `add_lorawan_sticker`, not command types of their own.
        const BUILDABLE: &[&str] = &[
            "set_threshold",
            "set_sensor_name",
            "set_sensor_location",
            "restart_application",
            "power_off",
            "factory_reset",
            "set_interval",
            "set_system_info_interval",
            "add_signer",
            "remove_signer",
            "update_signer",
            "set_device_label",
            "set_led_brightness",
            "set_screen_brightness",
            "set_screen_timeout",
            "set_display_lines",
            "set_buzzer_volume",
            "set_network_config",
            "set_lorawan_sensor_config",
            "set_lorawan_field_threshold",
            "delete_lorawan_field_threshold",
            "set_sticker_config",
            "send_sticker_raw",
            "set_eye_field_threshold",
            "delete_eye_field_threshold",
            "sticker_reboot",
            "sticker_device_reset",
            "sticker_reset_counters",
            "sticker_clock_sync",
            "add_lorawan_sticker",
            "remove_lorawan_sticker",
            "add_external_gateway",
            "remove_external_gateway",
            "set_lorawan_cluster",
            "set_eye_enabled",
            "set_eye_config",
            "set_eye_recording",
            "download_eye_history",
            "add_eye_tag",
            "set_eye_known_tags",
            "remove_eye_tag",
            "detect_eye_tag",
        ];

        let missing: Vec<&str> = BUILDABLE
            .iter()
            .copied()
            .filter(|cmd| manager.command_type_to_permission(cmd).is_err())
            .collect();

        assert!(
            missing.is_empty(),
            "these commands can be built from a challenge but have no permission, \
             so process_config_request rejects them before a challenge exists: {:?}",
            missing
        );
    }

    #[test]
    fn cluster_command_reuses_the_node_management_permission() {
        // Not its own permission: a signer certificate embeds a fixed permission
        // list at issuance, so a new one would invalidate every certificate
        // already provisioned across the fleet. The viewer's CommandSigner maps
        // it to Permission.SET_LORAWAN_SENSOR_CONFIG for the same reason; the two
        // sides must agree or the device denies a correctly signed command.
        let manager = create_test_manager();
        assert_eq!(
            manager
                .command_type_to_permission("set_lorawan_cluster")
                .unwrap(),
            "set_lorawan_sensor_config"
        );
        assert_eq!(
            manager
                .command_type_to_permission("set_lorawan_cluster")
                .unwrap(),
            manager
                .command_type_to_permission("add_external_gateway")
                .unwrap()
        );
    }

    #[test]
    fn beacon_tag_commands_map_to_lorawan_sensor_config_permission() {
        let manager = create_test_manager();
        for cmd in ["add_eye_tag", "remove_eye_tag", "detect_eye_tag"] {
            assert_eq!(
                manager.command_type_to_permission(cmd).unwrap(),
                "set_lorawan_sensor_config",
                "unexpected permission for {cmd}"
            );
        }
    }

    #[test]
    fn build_add_beacon_tag_uppercases_mac_and_keeps_name() {
        let manager = create_test_manager();
        let challenge = test_challenge(
            "add_eye_tag",
            serde_json::json!({"mac": "aa:bb:cc:dd:ee:ff", "name": "Freezer"}),
        );
        match manager.build_command_from_challenge(&challenge).unwrap() {
            MqttCommand::AddBeaconTag { mac, name } => {
                assert_eq!(mac, "AA:BB:CC:DD:EE:FF");
                assert_eq!(name.as_deref(), Some("Freezer"));
            }
            other => panic!("expected AddBeaconTag, got {other:?}"),
        }
    }

    #[test]
    fn build_add_beacon_tag_rejects_malformed_mac() {
        let manager = create_test_manager();
        let challenge = test_challenge("add_eye_tag", serde_json::json!({"mac": "not-a-mac"}));
        assert!(manager.build_command_from_challenge(&challenge).is_err());
    }

    #[test]
    fn set_eye_config_parses_from_a_challenge() {
        let manager = create_test_manager();
        assert_eq!(
            manager
                .command_type_to_permission("set_eye_config")
                .unwrap(),
            "set_lorawan_sensor_config"
        );
        let challenge = test_challenge(
            "set_eye_config",
            serde_json::json!({ "auto_provision": false }),
        );
        match manager.build_command_from_challenge(&challenge).unwrap() {
            MqttCommand::SetBeaconConfig {
                auto_provision,
                auto_discover,
            } => {
                assert_eq!(auto_provision, Some(false));
                assert_eq!(auto_discover, None);
            }
            other => panic!("expected SetBeaconConfig, got {other:?}"),
        }
        // Neither flag -> rejected before a challenge is consumed.
        let empty = test_challenge("set_eye_config", serde_json::json!({}));
        assert!(manager.build_command_from_challenge(&empty).is_err());
    }

    #[test]
    fn build_detect_beacon_tag_ok() {
        let manager = create_test_manager();
        let challenge = test_challenge(
            "detect_eye_tag",
            serde_json::json!({"mac": "AA:BB:CC:DD:EE:FF"}),
        );
        match manager.build_command_from_challenge(&challenge).unwrap() {
            MqttCommand::DetectBeaconTag { mac } => assert_eq!(mac, "AA:BB:CC:DD:EE:FF"),
            other => panic!("expected DetectBeaconTag, got {other:?}"),
        }
    }

    #[test]
    fn build_set_beacon_field_threshold_uppercases_mac_and_maps_permission() {
        let manager = create_test_manager();
        let challenge = test_challenge(
            "set_eye_field_threshold",
            serde_json::json!({
                "mac": "aa:bb:cc:dd:ee:ff", "field": "temperature",
                "warning_high": 8.0, "critical_high": 12.0
            }),
        );
        match manager.build_command_from_challenge(&challenge).unwrap() {
            MqttCommand::SetBeaconFieldThreshold {
                mac,
                field,
                warning_high,
                critical_high,
                ..
            } => {
                assert_eq!(mac, "AA:BB:CC:DD:EE:FF");
                assert_eq!(field, "temperature");
                assert_eq!(warning_high, Some(8.0));
                assert_eq!(critical_high, Some(12.0));
            }
            other => panic!("expected SetBeaconFieldThreshold, got {other:?}"),
        }
        assert_eq!(
            manager
                .command_type_to_permission("set_eye_field_threshold")
                .unwrap(),
            "set_lorawan_sensor_config",
        );
    }

    fn test_challenge(
        command_type: &str,
        params: Value,
    ) -> crate::libs::authorization::state::PendingChallenge {
        use crate::libs::authorization::state::{ChallengeState, PendingChallenge};
        PendingChallenge {
            challenge_id: "c".to_string(),
            request_id: "r".to_string(),
            signer_id: "s".to_string(),
            signer_name: "S".to_string(),
            command_type: command_type.to_string(),
            params,
            reason: None,
            signature: String::new(),
            nonce: String::new(),
            timestamp: 0,
            expires_at: 0,
            state: ChallengeState::AwaitingConfirmation,
            state_changed_at: 0,
        }
    }

    #[test]
    fn set_display_lines_has_its_own_permission() {
        let manager = create_test_manager();
        let permission = manager
            .command_type_to_permission("set_display_lines")
            .unwrap();
        assert_eq!(permission, "set_display_lines");

        // Hard cutover, asserted explicitly: choosing which sensors the local
        // panel lists is not the same capability as dimming it. A certificate
        // issued before this permission existed is *supposed* to be rejected, so
        // the tempting field fix — reinstating the reuse to make an
        // authorization failure go away — has to fail here first.
        assert_ne!(permission, "set_screen_brightness");
    }

    #[test]
    fn power_off_has_its_own_permission() {
        let manager = create_test_manager();
        let permission = manager.command_type_to_permission("power_off").unwrap();
        assert_eq!(permission, "power_off_device");

        // Same hard cutover as above, for a sharper reason: a reboot comes back
        // on its own, a power-off leaves the unit dark until someone walks to it.
        // Reusing the reboot permission would silently hand every already-issued
        // certificate the power to take a Class IIa monitor offline indefinitely,
        // so that shortcut has to fail here first.
        assert_ne!(permission, "restart_application");
    }

    #[test]
    fn build_command_from_challenge_power_off_carries_signer_and_reason() {
        let manager = create_test_manager();

        let mut challenge = test_challenge("power_off", json!({}));
        challenge.reason = Some("Fridge decommissioned".to_string());
        match manager.build_command_from_challenge(&challenge).unwrap() {
            MqttCommand::PowerOffDevice {
                reason,
                requested_by,
            } => {
                assert_eq!(reason, "Fridge decommissioned");
                // Not cosmetic: /tmp/fiber_audit.db does not survive the
                // power-off, so this is the only record of who authorized it.
                assert_eq!(requested_by, "s");
            }
            other => panic!("expected PowerOffDevice, got {other:?}"),
        }

        // No reason given still yields a command — the device must not refuse a
        // validly signed power-off over missing prose — but it must not end up
        // with an empty reason in the audit row either.
        let bare = test_challenge("power_off", json!({}));
        match manager.build_command_from_challenge(&bare).unwrap() {
            MqttCommand::PowerOffDevice { reason, .. } => assert!(!reason.is_empty()),
            other => panic!("expected PowerOffDevice, got {other:?}"),
        }
    }

    #[test]
    fn build_command_from_challenge_reboot_carries_signer_and_reason() {
        let manager = create_test_manager();

        let mut challenge = test_challenge("restart_application", json!({}));
        challenge.reason = Some("Applying new config".to_string());
        match manager.build_command_from_challenge(&challenge).unwrap() {
            MqttCommand::RestartApplication {
                reason,
                requested_by,
            } => {
                assert_eq!(reason, "Applying new config");
                // /tmp/fiber_audit.db is on tmpfs and the unit runs with
                // PrivateTmp=true, so a reboot wipes the authorization record
                // just as a power-off does. This is what the durable row keeps.
                assert_eq!(requested_by, "s");
            }
            other => panic!("expected RestartApplication, got {other:?}"),
        }

        let bare = test_challenge("restart_application", json!({}));
        match manager.build_command_from_challenge(&bare).unwrap() {
            MqttCommand::RestartApplication { reason, .. } => assert!(!reason.is_empty()),
            other => panic!("expected RestartApplication, got {other:?}"),
        }
    }

    #[test]
    fn factory_reset_has_its_own_permission() {
        let manager = create_test_manager();
        assert_eq!(
            manager.command_type_to_permission("factory_reset").unwrap(),
            "factory_reset"
        );

        // A certificate authorized for reboot or power-off must not thereby be
        // authorized to wipe the device's data — the three must be issued
        // independently. Reusing either would silently hand every
        // already-provisioned reboot/power-off certificate the power to erase
        // patient data, so that shortcut has to fail here first.
        assert_ne!(
            manager.command_type_to_permission("factory_reset").unwrap(),
            manager
                .command_type_to_permission("restart_application")
                .unwrap()
        );
        assert_ne!(
            manager.command_type_to_permission("factory_reset").unwrap(),
            manager.command_type_to_permission("power_off").unwrap()
        );
    }

    #[test]
    fn build_command_from_challenge_factory_reset_carries_signer_and_reason() {
        use crate::libs::mqtt::messages::PostResetAction;

        let manager = create_test_manager();

        let mut challenge = test_challenge("factory_reset", json!({ "post_action": "power_off" }));
        challenge.reason = Some("Decommissioned after loaner return".to_string());
        match manager.build_command_from_challenge(&challenge).unwrap() {
            MqttCommand::FactoryReset {
                post_action,
                reason,
                requested_by,
                request_id,
            } => {
                assert_eq!(post_action, PostResetAction::PowerOff);
                assert_eq!(reason, "Decommissioned after loaner return");
                assert_eq!(requested_by, "s");
                // The original signed request's id, not one minted here — see
                // `MqttCommand::FactoryReset`'s doc comment for why that
                // distinction matters for post-re-pairing correlation.
                assert_eq!(request_id, "r");
            }
            other => panic!("expected FactoryReset, got {other:?}"),
        }
    }

    #[test]
    fn build_command_from_challenge_factory_reset_rejects_a_missing_reason() {
        // Unlike restart_application/power_off, a missing reason must NOT be
        // defaulted here — parse_factory_reset requires an explicit,
        // non-blank reason because it is carried in the command itself and is
        // the only surviving record of who authorized the wipe and why, once
        // the wipe destroys the audit log that would otherwise hold it. This
        // must be enforced independent of whatever the viewer already does.
        let manager = create_test_manager();
        let bare = test_challenge("factory_reset", json!({ "post_action": "reboot" }));
        let err = manager.build_command_from_challenge(&bare).unwrap_err();
        match err {
            AuthError::InvalidCommand(msg) => assert!(msg.contains("reason"), "got {msg:?}"),
            other => panic!("expected InvalidCommand, got {other:?}"),
        }
    }

    #[test]
    fn build_command_from_challenge_factory_reset_rejects_a_missing_post_action() {
        let manager = create_test_manager();
        let mut challenge = test_challenge("factory_reset", json!({}));
        challenge.reason = Some("cleanup".to_string());
        let err = manager
            .build_command_from_challenge(&challenge)
            .unwrap_err();
        match err {
            AuthError::InvalidCommand(msg) => assert!(msg.contains("post_action"), "got {msg:?}"),
            other => panic!("expected InvalidCommand, got {other:?}"),
        }
    }

    #[test]
    fn describe_change_power_off_spells_out_the_consequence() {
        let manager = create_test_manager();
        let description = manager.describe_change("power_off", &json!({}));

        // This string is the preview the signer confirms against, so it has to
        // say both that monitoring stops and how the device comes back. A
        // generic "Execute command: power_off" fallback would let someone
        // approve a gap in patient monitoring blind.
        //
        // It also has to stay in step with what power_off physically does. It
        // used to promise the device "must be powered on by hand — it cannot be
        // woken remotely", which was true while the command halted the SoC; now
        // that it enters a PoE-wakeable standby, saying so is the whole point of
        // the preview.
        assert!(
            !description.starts_with("Execute command"),
            "got: {description}"
        );
        let lower = description.to_lowercase();
        assert!(lower.contains("power"), "got: {description}");
        assert!(lower.contains("monitoring"), "got: {description}");
        assert!(
            lower.contains("poe"),
            "the preview must name what brings the device back, got: {description}"
        );
    }

    #[test]
    fn build_command_from_challenge_set_display_lines_parses_array() {
        let manager = create_test_manager();
        let challenge = test_challenge(
            "set_display_lines",
            serde_json::json!({ "lines": [
                { "source": "sticker", "dev_eui": "70b3d57ed0051f2a", "field": "ext_temperature_1",
                  "label": "Stkr1 ext" },
                { "source": "ds18b20", "line": 0, "field": "temperature",
                  "format": { "decimals": 2, "status_char": false } },
            ]}),
        );
        match manager.build_command_from_challenge(&challenge) {
            Ok(MqttCommand::SetDisplayLines { lines }) => {
                assert_eq!(
                    lines.len(),
                    2,
                    "array order is semantic and must be preserved"
                );
                assert_eq!(lines[0].field, "ext_temperature_1");
                assert_eq!(lines[0].dev_eui.as_deref(), Some("70b3d57ed0051f2a"));
                assert_eq!(lines[1].line, Some(0));
                assert_eq!(lines[1].format.decimals, Some(2));
                assert!(!lines[1].format.status_char);
            }
            other => panic!("expected SetDisplayLines, got {:?}", other),
        }
    }

    #[test]
    fn build_command_from_challenge_set_display_lines_accepts_empty_list() {
        // Empty list is the documented way to restore the built-in layout.
        let manager = create_test_manager();
        let challenge = test_challenge("set_display_lines", serde_json::json!({ "lines": [] }));
        match manager.build_command_from_challenge(&challenge) {
            Ok(MqttCommand::SetDisplayLines { lines }) => assert!(lines.is_empty()),
            other => panic!("expected SetDisplayLines, got {:?}", other),
        }
    }

    #[test]
    fn build_command_from_challenge_set_display_lines_rejects_invalid_field() {
        // Unlike the on-disk path (which drops bad entries so a device can still
        // boot), a command carrying a bad line must be rejected outright.
        let manager = create_test_manager();
        let challenge = test_challenge(
            "set_display_lines",
            serde_json::json!({ "lines": [
                { "source": "sticker", "dev_eui": "70b3d57ed0051f2a", "field": "battery_percent" },
            ]}),
        );
        let err = manager
            .build_command_from_challenge(&challenge)
            .unwrap_err();
        assert!(
            format!("{:?}", err).contains("unknown node field"),
            "got: {:?}",
            err,
        );
    }

    #[test]
    fn build_command_from_challenge_set_display_lines_rejects_missing_lines() {
        let manager = create_test_manager();
        let challenge = test_challenge("set_display_lines", serde_json::json!({}));
        assert!(manager.build_command_from_challenge(&challenge).is_err());
    }

    #[test]
    fn build_command_from_challenge_screen_timeout_ok() {
        let manager = create_test_manager();
        let challenge = test_challenge(
            "set_screen_timeout",
            serde_json::json!({ "timeout_secs": 3600 }),
        );
        match manager.build_command_from_challenge(&challenge) {
            Ok(MqttCommand::SetScreenTimeout { timeout_secs }) => assert_eq!(timeout_secs, 3600),
            other => panic!("expected SetScreenTimeout, got {:?}", other),
        }
    }

    #[test]
    fn set_beacon_known_tags_is_reachable_in_a_production_build() {
        // The whole point of routing this through the signed path: the only other
        // way to construct the command lives in `build_dev_command`, which is
        // `#[cfg(feature = "dev-platform")]`. Without an arm here, system#6 could
        // never be enabled on a real gateway — and a dev-platform build must not be
        // deployed to one, since it disables command verification.
        let manager = create_test_manager();
        let challenge = test_challenge(
            "set_eye_known_tags",
            serde_json::json!({ "macs": ["7c:d9:f4:13:10:de", "7C:D9:F4:13:10:DF"] }),
        );
        match manager.build_command_from_challenge(&challenge) {
            Ok(MqttCommand::SetBeaconKnownTags { macs }) => {
                // Normalised to uppercase, because the scan compares against
                // uppercased MACs.
                assert_eq!(macs, vec!["7C:D9:F4:13:10:DE", "7C:D9:F4:13:10:DF"]);
            }
            other => panic!("expected SetBeaconKnownTags, got {:?}", other),
        }
    }

    #[test]
    fn set_beacon_known_tags_accepts_an_empty_list() {
        // Empty is the only way to stop listening for borrowed tags, so rejecting
        // it would make the allowlist one-way.
        let manager = create_test_manager();
        let challenge = test_challenge("set_eye_known_tags", serde_json::json!({ "macs": [] }));
        match manager.build_command_from_challenge(&challenge) {
            Ok(MqttCommand::SetBeaconKnownTags { macs }) => assert!(macs.is_empty()),
            other => panic!("expected SetBeaconKnownTags, got {:?}", other),
        }
    }

    #[test]
    fn set_beacon_known_tags_rejects_a_malformed_mac_and_a_missing_list() {
        let manager = create_test_manager();
        let bad_mac = test_challenge(
            "set_eye_known_tags",
            serde_json::json!({ "macs": ["7C:D9:F4:13:10:DE", "not-a-mac"] }),
        );
        assert!(manager.build_command_from_challenge(&bad_mac).is_err());

        let not_strings = test_challenge("set_eye_known_tags", serde_json::json!({ "macs": [42] }));
        assert!(manager.build_command_from_challenge(&not_strings).is_err());

        let missing = test_challenge("set_eye_known_tags", serde_json::json!({}));
        assert!(manager.build_command_from_challenge(&missing).is_err());
    }

    #[test]
    fn set_beacon_known_tags_takes_the_node_management_permission() {
        let manager = create_test_manager();
        assert_eq!(
            manager
                .command_type_to_permission("set_eye_known_tags")
                .unwrap(),
            "set_lorawan_sensor_config",
        );
    }

    #[test]
    fn build_command_from_challenge_screen_timeout_zero_allowed() {
        let manager = create_test_manager();
        let challenge = test_challenge(
            "set_screen_timeout",
            serde_json::json!({ "timeout_secs": 0 }),
        );
        // 0 is the documented "always on" sentinel and must be accepted.
        assert!(matches!(
            manager.build_command_from_challenge(&challenge),
            Ok(MqttCommand::SetScreenTimeout { timeout_secs: 0 })
        ));
    }

    #[test]
    fn build_command_from_challenge_screen_timeout_large_value_allowed() {
        let manager = create_test_manager();
        // No practical upper bound: any value that fits u32 is accepted
        // (e.g. beyond the old 24h/86400 guardrail).
        let challenge = test_challenge(
            "set_screen_timeout",
            serde_json::json!({ "timeout_secs": 86_401 }),
        );
        assert!(matches!(
            manager.build_command_from_challenge(&challenge),
            Ok(MqttCommand::SetScreenTimeout {
                timeout_secs: 86_401
            })
        ));
        let max = test_challenge(
            "set_screen_timeout",
            serde_json::json!({ "timeout_secs": u32::MAX as u64 }),
        );
        assert!(matches!(
            manager.build_command_from_challenge(&max),
            Ok(MqttCommand::SetScreenTimeout { timeout_secs }) if timeout_secs == u32::MAX
        ));
    }

    #[test]
    fn build_command_from_challenge_screen_timeout_out_of_range_rejected() {
        let manager = create_test_manager();
        // Only values that overflow u32 are rejected.
        let challenge = test_challenge(
            "set_screen_timeout",
            serde_json::json!({ "timeout_secs": (u32::MAX as u64) + 1 }),
        );
        assert!(manager.build_command_from_challenge(&challenge).is_err());
    }

    #[test]
    fn build_command_from_challenge_screen_timeout_missing_rejected() {
        let manager = create_test_manager();
        let challenge = test_challenge("set_screen_timeout", serde_json::json!({}));
        assert!(manager.build_command_from_challenge(&challenge).is_err());
    }

    fn create_test_manager() -> AuthorizationManager {
        // This is a placeholder - in real tests you'd need to set up the full verifier
        // For now, just create a manager with minimal setup
        use crate::libs::crypto::{CARegistry, NonceTracker, SignatureVerifier};
        use std::path::Path;
        use std::sync::atomic::AtomicUsize;

        static AUDIT_DB_SEQ: AtomicUsize = AtomicUsize::new(0);

        let ca_registry = Arc::new(Mutex::new(
            CARegistry::load_from_file(Path::new("/tmp/test_ca_registry.yaml")).unwrap(),
        ));
        let nonce_tracker = Arc::new(Mutex::new(
            NonceTracker::new(Path::new("/tmp/test_nonces.db"), 600, 100).unwrap(),
        ));
        let verifier = Arc::new(SignatureVerifier::new(ca_registry, nonce_tracker, 300));

        // Private path per call. This used to be /tmp/test_audit.db, which
        // init_audit_db creates unencrypted (bare Connection::open, no PRAGMA
        // key) — the storage::audit tests then could not reopen that file once
        // a SQLCipher key existed. Leaked on purpose: the manager owns the path
        // for its whole life, so there is no TempDir to hold here.
        let audit_db = std::env::temp_dir().join(format!(
            "fiber_test_auth_audit_{}_{}.db",
            std::process::id(),
            AUDIT_DB_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));

        AuthorizationManager::new(verifier, &audit_db, 300, 10)
    }
}
