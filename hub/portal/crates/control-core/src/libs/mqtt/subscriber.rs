// MQTT command subscription and handling

use crate::libs::authorization::level::{Envelope, Level};
use crate::libs::authorization::manager::ENVELOPE_VERSION;
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
            eprintln!(
                "[MQTT Subscriber] Received command on {}: {}",
                topic, json_str
            );
        }

        // Extract command type from JSON
        let command_type = json
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing 'command' field".to_string())?;

        // One dialect (plan/portal/mqtt/design.md §1, §14 P4). `command` is
        // unsigned, so it may **route** and never authorize (Q2 §3) — which
        // leaves it exactly two values. Every verb, including the eight that
        // arrived flat and unauthenticated in v1, is now a `command_type`
        // inside a signed envelope.
        //
        // What was here until P4: `set_threshold`, `get_status`, `set_screen`,
        // `flush_storage`, `get_info`, `get_sensor_config`, `silence_buzzer`,
        // `restart`, `set_interval`, `get_interval`, `history_request`, the
        // four `get_sticker_*` reads and `sticker_force_send`. The first ten
        // were inert — the dispatch loop dropped the `MqttCommand`s they built
        // — but "inert" is a property of today's call graph, not of the parser,
        // and ISO 14971 counts them as a hazard source either way (§11). The
        // rest were live and unauthenticated.
        match command_type {
            "config_request" => {
                #[cfg(feature = "dev-platform")]
                {
                    self.parse_config_request_dev(&json)
                }
                #[cfg(not(feature = "dev-platform"))]
                {
                    self.parse_config_request(&json)
                }
            }
            "config_confirm" => self.parse_config_confirm(&json),
            // `command` is the dialect selector and there are exactly two
            // dialects, so anything else *is* an unsupported envelope — which
            // is both the accurate answer and the useful one. A stale sender
            // reaching here is almost always using a real verb in the wrong
            // shape (`{"command": "get_sticker_info", …}`), and telling it
            // "unknown command type" would be true of the dialect while being
            // misleading about the verb, which the device does still know.
            //
            // Judging the *verb* stays where it belongs: `process_config_request`
            // answers `Invalid command: Unknown command type: …` for a
            // `command_type` the table has no row for (design.md §3.3 step 3).
            // The subscriber judges the envelope; the manager judges the verb.
            _ => Err("unsupported_envelope: v=2 required".to_string()),
        }
    }

    /// Split a [`parse_command`](Self::parse_command) error into the
    /// `errors`-topic triple `(command, error, message)` design.md §3.2
    /// spells out:
    /// `{"command":"config_request","error":"unsupported_envelope","message":"v=2 required"}`.
    ///
    /// Until this existed the monitor published every parse failure as
    /// `("unknown", "parse_error", <the whole string>)`, so the one v1-shaped
    /// answer a v2 device is allowed to give arrived with the wrong `command`,
    /// the wrong slug, and the slug buried in the prose. The tests all
    /// asserted on the `Result`, never on the JSON, which is exactly why it
    /// survived P4 — and the mock, which implements §3.2 literally, made the
    /// e2e suite green against a contract only one half honoured.
    ///
    /// `command` is `config_request` and not the dialect that actually
    /// arrived: an unsupported envelope is by definition one whose dialect
    /// selector we do not trust, and §3.2 names the value.
    ///
    /// The second shape it knows is `invalid_command@<dialect>: <detail>`,
    /// which [`Self::parse_envelope`] produces for a v2 frame whose `level`
    /// is missing or not in the enum. That is design.md §3.2's
    /// `InvalidCommand`, and it goes out exactly as the authorization
    /// manager's own `InvalidCommand` does — `authorization_failed`,
    /// `Invalid command: …`, under the dialect that actually arrived — so a
    /// portal cannot tell which half of the device refused it, which is the
    /// point.
    pub fn classify_parse_error(err: &str) -> (&'static str, &'static str, String) {
        if let Some(message) = err.strip_prefix("unsupported_envelope: ") {
            return (
                "config_request",
                "unsupported_envelope",
                message.to_string(),
            );
        }
        if let Some((dialect, detail)) = err
            .strip_prefix("invalid_command@")
            .and_then(|rest| rest.split_once(": "))
        {
            let dialect = if dialect == "config_confirm" {
                "config_confirm"
            } else {
                "config_request"
            };
            return (
                dialect,
                "authorization_failed",
                format!("Invalid command: {detail}"),
            );
        }
        ("unknown", "parse_error", err.to_string())
    }

    /// Validate a NODE `dev_eui` field: exactly 16 hex chars, normalized to
    /// lowercase to match the on-device / fPort-85 downlink representation.
    fn parse_node_dev_eui(json: &Value) -> Result<String, String> {
        let dev_eui = json
            .get("dev_eui")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing 'dev_eui' field".to_string())?;
        if dev_eui.len() != 16 || !dev_eui.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!(
                "Invalid dev_eui {:?} (expected 16 hex chars)",
                dev_eui
            ));
        }
        Ok(dev_eui.to_lowercase())
    }

    /// Parse an optional u32 field (absent or null → None).
    fn parse_opt_u32(json: &Value, field: &str) -> Result<Option<u32>, String> {
        match json.get(field) {
            None => Ok(None),
            Some(v) if v.is_null() => Ok(None),
            Some(v) => {
                let n = v.as_u64().ok_or_else(|| {
                    format!("Invalid '{}' field (expected unsigned integer)", field)
                })?;
                if n > u32::MAX as u64 {
                    return Err(format!("'{}' out of u32 range: {}", field, n));
                }
                Ok(Some(n as u32))
            }
        }
    }

    /// Parse temperature value from JSON
    fn parse_temp(obj: &Value, field: &str) -> Result<f32, String> {
        let temp =
            obj.get(field)
                .and_then(|v| v.as_f64())
                .ok_or_else(|| format!("Missing or invalid '{}' field", field))? as f32;

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

    /// Parse config_request command in dev-platform mode (no signature required)
    /// Accepts a simplified JSON format:
    /// { "command": "config_request", "command_type": "set_threshold", "params": {...} }
    #[cfg(feature = "dev-platform")]
    fn parse_config_request_dev(&self, json: &Value) -> Result<MqttCommand, String> {
        let request_id = json
            .get("request_id")
            .and_then(|v| v.as_str())
            .unwrap_or("dev-request")
            .to_string();

        let command_type = json
            .get("command_type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing 'command_type' field".to_string())?
            .to_string();

        let params = json
            .get("params")
            .ok_or_else(|| "Missing 'params' field".to_string())?
            .clone();

        // dev-platform bypasses verification and executes directly, so the
        // envelope is carried for shape only: an absent or malformed one
        // reads as "claims no level" rather than failing a build nobody signs
        // against. This is the one place `Envelope::V1` is still constructed,
        // and it is behind the feature — the wire path answers
        // `unsupported_envelope` to the same payload (P4).
        //
        // It is still only ever reached for `command_type` inside a
        // `config_request`: the dev platform loosens *authentication*, never
        // the dialect, so no flat-dialect parser survives through here either.
        let envelope = self
            .parse_envelope(json, "config_request")
            .unwrap_or(Envelope::V1);

        let reason = json
            .get("reason")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let signer_id = json
            .get("signer_id")
            .and_then(|v| v.as_str())
            .unwrap_or("dev-student")
            .to_string();

        let timestamp = json
            .get("timestamp")
            .and_then(|v| v.as_i64())
            .unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64
            });

        // Use dummy values for fields not needed in dev mode
        let dummy_cert = UserCertificate {
            signer_id: signer_id.clone(),
            full_name: "Dev Student".to_string(),
            role: "developer".to_string(),
            public_key_ed25519: "0".repeat(64),
            permissions: vec!["*".to_string()],
            issued_at: "2020-01-01T00:00:00Z".to_string(),
            expires_at: "2099-12-31T23:59:59Z".to_string(),
            issuer: "dev-platform".to_string(),
            certificate_signature: "dev-platform-no-signature".to_string(),
        };

        Ok(MqttCommand::ConfigRequest {
            envelope,
            request_id,
            command_type,
            params,
            reason,
            signer_id,
            signature: "dev-platform-no-signature".to_string(),
            timestamp,
            nonce: format!("dev-{}", uuid::Uuid::new_v4()),
            certificate: dummy_cert,
        })
    }

    /// Read the envelope version and level off a signed command
    /// (plan/portal/mqtt/design.md §3.2).
    ///
    /// v2 requires `v == 2` and a `level` from the enum; both ride inside
    /// the signature, so a wrong one is caught here before any crypto and
    /// again by the signature itself.
    ///
    /// **P4 made this mandatory.** The transitional `accept_v1_envelope`
    /// flag that let a v1-shaped request through between P1 and P4 is gone
    /// with its one caller; a command carrying neither field now answers
    /// `unsupported_envelope`, which is the only v1-shaped thing a v2
    /// device says. There is no v1 fleet to migrate — both halves ship as
    /// one image — so no dual-accept survives the branch.
    ///
    /// A half-v2 envelope (a `v` with no `level`, or the reverse) is
    /// refused outright rather than read as either: it would sign a
    /// different pre-image than either side expects.
    ///
    /// *2026-09-15, second review:* the two half-v2 cases and a bad `level`
    /// spelling used to fall out as bare parse errors — `("unknown",
    /// "parse_error", …)` on the wire — while the mock answered
    /// `unsupported_envelope` and `Invalid command: unknown level: …`, and
    /// design.md §3.2 says the latter in as many words. Same divergence class
    /// as `655f405`, three cases it did not reach. The rule now: **`v` is
    /// judged first and alone** — anything that is not `v: 2` is an
    /// unsupported envelope whatever else is in the frame — and once the
    /// frame *is* v2, a missing or unknown `level` is `InvalidCommand`, the
    /// claim inside a well-formed envelope being what is wrong. `dialect`
    /// rides in the error string so [`Self::classify_parse_error`] can put
    /// the right `command` on the `errors` document without re-parsing.
    fn parse_envelope(&self, json: &Value, dialect: &str) -> Result<Envelope, String> {
        let Some(version) = json.get("v") else {
            return Err("unsupported_envelope: v=2 required".to_string());
        };
        match version.as_u64() {
            Some(v) if v == ENVELOPE_VERSION as u64 => {}
            Some(v) => return Err(format!("unsupported_envelope: v=2 required, got {v}")),
            None => return Err(format!("unsupported_envelope: v=2 required, got {version}")),
        }

        let level = match json.get("level") {
            None => {
                return Err(format!(
                    "invalid_command@{dialect}: unknown level: <missing>"
                ))
            }
            Some(Value::String(s)) => Level::parse(s)
                .ok_or_else(|| format!("invalid_command@{dialect}: unknown level: {s}"))?,
            Some(other) => {
                return Err(format!("invalid_command@{dialect}: unknown level: {other}"))
            }
        };
        Ok(Envelope::V2(level))
    }

    /// Parse config_request command (signed with Ed25519)
    #[cfg_attr(feature = "dev-platform", allow(dead_code))]
    pub(crate) fn parse_config_request(&self, json: &Value) -> Result<MqttCommand, String> {
        let envelope = self.parse_envelope(json, "config_request")?;

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

        // v2 makes `reason` a required string; `""` is the absent value,
        // never `null` or missing. v1's device canonicalized a missing
        // reason as `null` while the portal always emitted `""` — the
        // asymmetry only never bit because the portal never omitted it,
        // and stating the rule is what stops it coming back.
        let reason =
            match (envelope, json.get("reason")) {
                (Envelope::V2(_), Some(Value::String(s))) => Some(s.clone()),
                (Envelope::V2(_), _) => return Err(
                    "Missing or invalid 'reason' field (v2 requires a string, \"\" when absent)"
                        .to_string(),
                ),
                (Envelope::V1, reason) => reason.and_then(|v| v.as_str()).map(|s| s.to_string()),
            };

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
            envelope,
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
        let envelope = self.parse_envelope(json, "config_confirm")?;

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
            envelope,
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
}
