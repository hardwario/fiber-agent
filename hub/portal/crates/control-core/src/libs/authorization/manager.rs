//! Authorization manager for signed configuration commands

use super::level::{Envelope, Level};
use super::state::{ChallengeRegistry, ChallengeState, PendingChallenge};
use crate::libs::crypto::{SignatureVerifier, UserCertificate, VerificationResult};
use crate::libs::mqtt::messages::{MqttCommand, MqttMessage};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// The signed envelope version this device speaks (design.md §3.2).
pub const ENVELOPE_VERSION: u8 = 2;

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
        match Self::open_audit_db(&db_path_str) {
            Ok(_) => eprintln!("[AuthManager] Audit database initialized: {}", db_path_str),
            Err(e) => eprintln!(
                "[AuthManager] Warning: Failed to initialize audit database: {}",
                e
            ),
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

    /// Open the audit database, creating its table and indexes if missing.
    ///
    /// Not only at startup: every write opens the file by path, and SQLite
    /// creates a missing file as an empty database. When systemd's tmpfiles
    /// cleanup removed it under a running `fiber_app`, every later write
    /// failed with "no such table: config_changes", and `log_config_request`
    /// failing rejects the command (#136). Creating the schema here recreates
    /// it instead.
    fn open_audit_db(db_path: &str) -> Result<Connection, String> {
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

        Ok(conn)
    }

    /// The audit database for one write, see [`Self::open_audit_db`].
    fn audit_db(&self) -> AuthResult<Connection> {
        Self::open_audit_db(&self.db_path).map_err(AuthError::DatabaseError)
    }

    /// Process a ConfigRequest command.
    ///
    /// Verifies the signature and then, per the verb's [`Level`], either
    /// executes it there and then (`read`/`operate` — design.md §5) or
    /// mints a challenge for the signer to confirm (`manage` and above).
    /// The return shape is the one `process_config_confirm` already had:
    /// the message to publish, and the command to run if there is one.
    ///
    /// The order below is design.md §3.3, which is the order this function
    /// already ran in, with two insertions marked ▲.
    #[allow(clippy::too_many_arguments)]
    pub fn process_config_request(
        &self,
        envelope: Envelope,
        request_id: String,
        command_type: String,
        params: Value,
        reason: Option<String>,
        signer_id: String,
        signature: String,
        timestamp: i64,
        nonce: String,
        certificate: &UserCertificate,
    ) -> AuthResult<(MqttMessage, Option<MqttCommand>)> {
        // 3. ▲ command_type → (permission, expected level), before any
        //    crypto. An unknown verb is InvalidCommand exactly as it was.
        let (required_permission, level_expected) = self.command_type_to_spec(&command_type)?;

        // 4. ▲ The signed claim must be the level the table says. The
        //    signature binds the level; the table decides what it must be,
        //    so a caller cannot relabel `sticker_device_reset` as `read` to
        //    dodge the certificate check in step 8.
        if let Some(claimed) = envelope.level() {
            if claimed != level_expected {
                return Err(AuthError::InvalidCommand(format!(
                    "level mismatch: {} is {}, request claims {}",
                    command_type, level_expected, claimed
                )));
            }
        }

        // 1. Build canonical message for signature verification
        let canonical_msg = self.build_canonical_request_message(
            envelope,
            &request_id,
            &command_type,
            &params,
            &reason,
            &signer_id,
            timestamp,
            &nonce,
        );

        // 5-10. CA chain, expiry, timestamp window, nonce, permissions,
        //       signature, record nonce.
        //
        // Step 8 is two strings in v2: the verb's own permission **and**
        // `level:<level>`. The second is what makes the level enforceable
        // without changing the certificate schema — a manager's
        // certificate carries `set_lorawan_sensor_config`, which is also
        // `sticker_device_reset`'s permission, so without `level:
        // destructive` being absent from it a compromised portal holding a
        // manager key could issue the reset (design.md §4.3).
        let mut required = vec![required_permission];
        if let Some(level) = envelope.level() {
            required.push(level.permission());
        }
        let verification = self
            .verifier
            .verify_signed_command(
                &canonical_msg,
                &signature,
                certificate,
                timestamp,
                &nonce,
                &required,
            )
            .map_err(|e| AuthError::SignatureVerificationFailed(format!("{:?}", e)))?;

        eprintln!(
            "[AuthManager] ✓ ConfigRequest verified: signer={} ({}) command={} level={} issuer={}",
            verification.signer_id,
            verification.signer_name,
            command_type,
            level_expected,
            verification.issuer
        );

        let signer_name = verification.signer_name.clone();
        let challenge_id = Uuid::new_v4().to_string();

        // 11. Branch on the level.
        if envelope.level().is_some_and(Level::is_single_shot) {
            return self.execute_single_shot(
                challenge_id,
                request_id,
                command_type,
                params,
                reason,
                signer_id,
                signer_name,
                signature,
                nonce,
                timestamp,
                &verification,
                level_expected,
            );
        }

        // Everything above `operate` keeps challenge/confirm.
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
        )
        .with_level(envelope.level());

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
        Ok((
            MqttMessage::PublishConfigChallenge {
                challenge_id,
                request_id,
                signer_id: signer_id.clone(),
                expires_at,
                preview,
            },
            None,
        ))
    }

    /// One item of a config-sync document (plan/portal/config_sync.md §6,
    /// §7; decision #43): the same checks as [`Self::process_config_request`]
    /// (verb table, level claim, CA chain, expiry, timestamp window, nonce,
    /// permission + `level:<level>`, signature), then built and audited like
    /// a single-shot verb **at any level** — no challenge/confirm round-trip.
    ///
    /// The challenge exists so the signer confirms a pending change live.
    /// A sync item is a value the editor already saved in the Portal and the
    /// Portal re-signed for this send; there is nobody to confirm it to, and
    /// a Hub that is behind must converge without a human. The authority is
    /// the editor's signature (still checked in full, including
    /// `level:manage`) plus the restriction to desired-state verbs, which the
    /// caller enforces with `fiber_protocol::config_sync::is_desired_verb`.
    ///
    /// Each item carries its own nonce, recorded by the verifier as for a
    /// live command, so two items never collide and a redelivered document
    /// is refused per item as a replay.
    ///
    /// Returns the audit key (for `log_config_execution`) and the command.
    #[allow(clippy::too_many_arguments)]
    pub fn process_sync_item(
        &self,
        envelope: Envelope,
        request_id: String,
        command_type: String,
        params: Value,
        reason: Option<String>,
        signer_id: String,
        signature: String,
        timestamp: i64,
        nonce: String,
        certificate: &UserCertificate,
    ) -> AuthResult<(String, MqttCommand)> {
        let (required_permission, level_expected) = self.command_type_to_spec(&command_type)?;
        match envelope.level() {
            Some(claimed) if claimed == level_expected => {}
            Some(claimed) => {
                return Err(AuthError::InvalidCommand(format!(
                    "level mismatch: {} is {}, request claims {}",
                    command_type, level_expected, claimed
                )))
            }
            None => {
                return Err(AuthError::InvalidCommand(
                    "sync items require a v2 envelope".to_string(),
                ))
            }
        }
        let canonical_msg = self.build_canonical_request_message(
            envelope,
            &request_id,
            &command_type,
            &params,
            &reason,
            &signer_id,
            timestamp,
            &nonce,
        );
        let required = vec![required_permission, level_expected.permission()];
        let verification = self
            .verifier
            .verify_signed_command(
                &canonical_msg,
                &signature,
                certificate,
                timestamp,
                &nonce,
                &required,
            )
            .map_err(|e| AuthError::SignatureVerificationFailed(format!("{:?}", e)))?;
        let signer_name = verification.signer_name.clone();
        let challenge_id = Uuid::new_v4().to_string();
        let (_, command) = self.execute_single_shot(
            challenge_id.clone(),
            request_id,
            command_type,
            params,
            reason,
            signer_id,
            signer_name,
            signature,
            nonce,
            timestamp,
            &verification,
            level_expected,
        )?;
        let command = command
            .ok_or_else(|| AuthError::InvalidState("single-shot built no command".to_string()))?;
        Ok((challenge_id, command))
    }

    /// `read` and `operate`: verified, executed, one audit trail, no
    /// challenge (design.md §5.1).
    ///
    /// No challenge slot is held, so the 10-concurrent cap applies only to
    /// `manage` and above — which is the constraint single-shot actually
    /// removes: a portal outage between challenge and confirm used to park
    /// a slot for the full 300 s.
    ///
    /// The `challenge_id` is minted purely as the **audit key**:
    /// `config_changes.challenge_id` is `TEXT NOT NULL` and
    /// `log_config_response` updates by it, so the schema is untouched and
    /// the trail reads the same for every verb — the `PENDING` insert, the
    /// verification verdict, and (from `monitor.rs::run_and_answer`, once the
    /// command has actually run) the execution result, as a confirmed
    /// command has.
    #[allow(clippy::too_many_arguments)]
    fn execute_single_shot(
        &self,
        challenge_id: String,
        request_id: String,
        command_type: String,
        params: Value,
        reason: Option<String>,
        signer_id: String,
        signer_name: String,
        signature: String,
        nonce: String,
        timestamp: i64,
        verification: &VerificationResult,
        level: Level,
    ) -> AuthResult<(MqttMessage, Option<MqttCommand>)> {
        self.log_config_request(
            &challenge_id,
            &request_id,
            verification,
            &command_type,
            &params,
            &reason,
            &signature,
            &nonce,
            timestamp,
        )?;

        // The command is built through the same one builder a confirmed
        // command uses, from a challenge that is never registered — so a
        // verb cannot mean one thing single-shot and another confirmed.
        let transient = PendingChallenge::new(
            challenge_id.clone(),
            request_id.clone(),
            signer_id,
            signer_name,
            command_type.clone(),
            params,
            reason,
            signature,
            nonce,
            timestamp,
            timestamp,
        )
        .with_level(Some(level));

        let command = match self.build_command_from_challenge(&transient) {
            Ok(command) => command,
            Err(e) => {
                // The audit row exists and must not be left PENDING
                // forever; the caller turns this into an `errors` publish.
                let _ = self.log_config_response(&challenge_id, "REJECTED", timestamp);
                return Err(e);
            }
        };

        let applied_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        self.log_config_response(&challenge_id, "APPROVED", timestamp)?;

        eprintln!(
            "[AuthManager] ✓ Single-shot {} ({}) executed for {}",
            command_type, level, request_id
        );

        Ok((
            MqttMessage::PublishConfigResponse {
                challenge_id,
                request_id,
                status: "SUCCESS".to_string(),
                applied_at: Some(applied_at),
                effective_at: Some(applied_at),
                message: format!("Command executed: {}", command_type),
                result: None,
            },
            Some(command),
        ))
    }

    /// Process a ConfigConfirm command
    ///
    /// This verifies the confirmation signature and either applies or rejects the change.
    #[allow(clippy::too_many_arguments)]
    pub fn process_config_confirm(
        &self,
        envelope: Envelope,
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

        // 3b. ▲ The confirm's signed level must be the level the challenge
        //     was minted at, so a confirm cannot escalate its own request
        //     (design.md §3.3). Both halves must speak the same envelope:
        //     a v2 confirm against a v1 challenge, or the reverse, is a
        //     mismatch too, because they do not even sign the same fields.
        if envelope.level() != challenge.level {
            return Err(AuthError::InvalidCommand(format!(
                "level mismatch: challenge {} was minted at {:?}, confirm claims {:?}",
                challenge_id,
                challenge.level.map(|l| l.as_str()),
                envelope.level().map(|l| l.as_str())
            )));
        }

        // 4. Build canonical confirmation message
        let canonical_msg = self.build_canonical_confirm_message(
            envelope,
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
                // No permission check for confirmation: the request it
                // confirms was already checked against both the verb's
                // permission and `level:<level>`, and §3b above pins the
                // confirm to that same challenge.
                &[],
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
                result: None,
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
                result: None,
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

    /// Build the canonical request message the signature covers.
    ///
    /// v2 is nine keys — `level` sorts between `command_type` and `nonce`,
    /// `v` sorts last (design.md §3.2). v1 is the seven it always was. The
    /// `BTreeMap` sorts structurally either way, so the two extra fields
    /// cost no ordering code; the checked-in
    /// `fiber-protocol/vectors/envelope_v2.json` pins the exact strings
    /// this produces against the portal's and the mock's (design.md §10).
    ///
    /// **`reason`.** v1 canonicalized a missing reason as `null` here while
    /// the portal always emitted `""` — an asymmetry that only never bit
    /// because the portal never omitted it. v2 makes `reason` a required
    /// string, rejected at parse if it is `null` or missing, and
    /// canonicalized verbatim.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn build_canonical_request_message(
        &self,
        envelope: Envelope,
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
        if let Envelope::V2(level) = envelope {
            msg.insert("level", Value::String(level.as_str().to_string()));
            msg.insert("v", json!(ENVELOPE_VERSION));
        }

        serde_json::to_string(&msg).unwrap()
    }

    /// Build the canonical confirmation message the signature covers —
    /// seven keys in v2, five in v1. The confirm repeats the level the
    /// challenge was minted at, so a confirm cannot escalate past its own
    /// request (design.md §3.3).
    pub(super) fn build_canonical_confirm_message(
        &self,
        envelope: Envelope,
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
        if let Envelope::V2(level) = envelope {
            msg.insert("level", Value::String(level.as_str().to_string()));
            msg.insert("v", json!(ENVELOPE_VERSION));
        }

        serde_json::to_string(&msg).unwrap()
    }

    /// The verb table: `command_type` → (certificate permission, [`Level`]).
    ///
    /// These permission strings must match the permissions in user
    /// certificates, and the level must match the portal's registry row —
    /// `vectors/catalogue.json` is the one file both sides are tested
    /// against (design.md §10). An unknown verb is rejected here, before
    /// any crypto runs (design.md §3.3 step 3), exactly as it was before
    /// the level existed.
    ///
    /// This table is a **superset** of the portal's 60 rows (52 plus
    /// `node-add`, `node-remove`, `node-move`, `setup-complete` (#58),
    /// `export-destination-set` (#59), `radio-set-mode` (R7),
    /// `cluster-client-add`/`-remove` (cluster.md §7), `northbridge-update`
    /// (northbridge_update.md §3.4) and `southbridge-update` (southbridge_update.md §3.7), less the retired `set_lorawan_cluster`):
    /// `get_status`, `get_info`, `set_screen`, `flush_storage` and
    /// `beacon-detect` have arms here but no portal publisher. The count the
    /// tests actually assert is `the_device_table_agrees_with_the_catalogue`'s
    /// — this prose is the reading aid, and has been stale once already.
    pub(super) fn command_type_to_spec(&self, command_type: &str) -> AuthResult<(String, Level)> {
        // Map command types to (permission string, level).
        let spec: (&str, Level) = match command_type {
            // ---- read: returns state, changes nothing, redelivery is harmless
            "get_info" => ("get_info", Level::Read),
            "get_status" => ("get_status", Level::Read),
            // The eight verbs that arrived unsigned in v1 and are signed
            // rows now (design.md §1, §4.2). `get_sensor_config` and
            // `get_interval` already existed as certificate permissions in
            // the portal's viewer set and now require themselves.
            "get_sensor_config" => ("get_sensor_config", Level::Read),
            "get_interval" => ("get_interval", Level::Read),
            "get_sticker_config" => ("get_status", Level::Read),
            "get_sticker_info" => ("get_status", Level::Read),
            "get_sticker_full_config" => ("get_status", Level::Read),
            "get_sticker_history" => ("get_status", Level::Read),
            "beacon-history" => ("beacon.manage", Level::Read),

            // ---- operate: a transient physical effect, nothing persisted
            // `silence_buzzer` had no permission at all "by design" (v1
            // parity) and gets its own. The alternative — mapping it onto
            // `get_status` — would let a read-only certificate silence a
            // medical alarm.
            "silence_buzzer" => ("silence_buzzer", Level::Operate),
            "sticker_force_send" => ("get_status", Level::Operate),
            "sticker_clock_sync" => ("set_lorawan_sensor_config", Level::Operate),
            "set_screen" => ("set_screen", Level::Operate),
            "set_led_brightness" => ("set_led_brightness", Level::Operate),
            "set_screen_brightness" => ("set_screen_brightness", Level::Operate),
            // reuse: screen-control permission (works with existing certs)
            "set_screen_timeout" => ("set_screen_brightness", Level::Operate),
            "beacon-detect" => ("beacon.manage", Level::Operate),

            // ---- manage: persists configuration on the Hub or a node
            "set_threshold" => ("set_threshold", Level::Manage),
            "set_sensor_name" => ("set_sensor_name", Level::Manage),
            "set_sensor_location" => ("set_sensor_location", Level::Manage),
            "set_alarm_pattern" => ("set_alarm_pattern", Level::Manage),
            "set_interval" => ("set_interval", Level::Manage),
            "set_sample_interval" => ("set_interval", Level::Manage),
            "set_system_info_interval" => ("set_system_info_interval", Level::Manage),
            "set_device_label" => ("set_device_label", Level::Manage),
            // See the registry note: a rename and a commissioning are different
            // capabilities, so they are different permissions. GitLab #58.
            "setup-complete" => ("setup.manage", Level::Manage),
            // Admin: where a site's measurement data leaves the device for. GitLab #59.
            "export-destination-set" => ("export.manage", Level::Admin),
            "set_buzzer_volume" => ("set_buzzer_volume", Level::Manage),
            // Deliberately NOT the screen-control permission. That one covers how
            // brightly and how long the panel is lit; this one decides which
            // measurements the panel lists at all, which is a different capability
            // on a Class IIa device. The cost is accepted: certificates issued
            // before this permission existed do not carry it and must be reissued.
            "set_display_lines" => ("set_display_lines", Level::Manage),
            "set_lorawan_sensor_config" => ("set_lorawan_sensor_config", Level::Manage),
            "add_lorawan_sticker" => ("set_lorawan_sensor_config", Level::Manage),
            // The v2 enrolment verb (plan/portal/control.radio.md §7): one permission for
            // every Node mutation, minted fresh by the v2 portal, so no v1 certificate is
            // being protected here — the string is the plan's, not v1's.
            "node-add" => ("node.manage", Level::Manage),
            "add_external_gateway" => ("set_lorawan_sensor_config", Level::Manage),
            "remove_external_gateway" => ("set_lorawan_sensor_config", Level::Manage),
            "set_lorawan_field_threshold" => ("set_threshold", Level::Manage),
            "delete_lorawan_field_threshold" => ("set_threshold", Level::Manage),
            "set_sticker_config" => ("set_lorawan_sensor_config", Level::Manage),
            "send_sticker_raw" => ("set_lorawan_sensor_config", Level::Manage),
            "sticker_reset_counters" => ("set_lorawan_sensor_config", Level::Manage),
            // The two survivors of the `*_eye_*` vocabulary. They are fleet *config*,
            // not Beacon operations — the crate reads the thresholds it finds on its
            // next tick and never issues these itself — so they are renamed with the
            // rest of the threshold family in plan/portal/control.md, not here.
            "set_eye_field_threshold" => ("set_lorawan_sensor_config", Level::Manage),
            "delete_eye_field_threshold" => ("set_lorawan_sensor_config", Level::Manage),
            // The Beacon verbs (plan/control/ble/plan.md §5, §8). v2 speaks these and
            // only these: the v1 `*_eye_*` strings are gone from all three registries,
            // so a portal that still sends one gets "Unsupported command type" rather
            // than a silent second vocabulary. plan.md §9.8 carries the historical map.
            //
            // All nine take `beacon.manage`, a v2 permission the portal now mints. The
            // v1 reuse of `set_lorawan_sensor_config` was an artefact of not wanting to
            // re-issue certificates; the rename forces re-issuance anyway, so the same
            // change may as well stop a Node-management certificate from also
            // authorising every Beacon operation on the Hub.
            "beacon-monitoring" => ("beacon.manage", Level::Manage),
            "beacon-send" => ("beacon.manage", Level::Manage),
            "beacon-recording" => ("beacon.manage", Level::Manage),
            "beacon-add" => ("beacon.manage", Level::Manage),
            "beacon-provision" => ("beacon.manage", Level::Manage),
            // system#6. Not a registration — it only widens what this gateway
            // listens for — but it is still a fleet-scoped write, so it takes the
            // same permission as adding a Beacon rather than a read permission.
            "known-beacons" => ("beacon.manage", Level::Manage),

            // ---- admin: identity, trust, connectivity, export/audit state
            "set_network_config" => ("set_network_config", Level::Admin),
            // The Hub's radio mode at runtime (node_registry.md §11, amended R7). Its own
            // permission: switching a carrier off takes every Node on it off the air, which a
            // Node-management certificate must not be able to do to the whole Hub.
            "radio-set-mode" => ("radio.manage", Level::Admin),
            // Flash the Northbridge with the bundled firmware (northbridge_update.md §3.4,
            // amended 2026-10-09). The radio permission and tier: it stops the P2P radio for
            // about a minute, which a Node-management certificate must not be able to do.
            "northbridge-update" => ("radio.manage", Level::Admin),
            // The Southbridge's twin (southbridge_update.md §3.7): the same permission, so the
            // certificates that may flash one Hub coprocessor may flash the other.
            "southbridge-update" => ("radio.manage", Level::Admin),
            "add_signer" => ("add_signer", Level::Admin),
            "remove_signer" => ("remove_signer", Level::Admin),
            "update_signer" => ("update_signer", Level::Admin),
            // admin op: align with node management
            "reset_export_cursor" => ("set_lorawan_sensor_config", Level::Admin),
            // A Node changes carrier (plan/portal/control.radio.md §7, amendment
            // 2026-09-27). `admin`: until the Node is switched too it cannot be reached,
            // which is the connectivity rung; nothing is lost, so not `destructive`. The
            // Node permission every other Node mutation takes.
            "node-move" => ("node.manage", Level::Admin),
            // A Server Hub mints, rotates and revokes a Client Hub's broker account and
            // registers its gateway (plan/control/cluster.md §7). `admin`: it decides who may
            // connect to this Hub's broker. Its own permission, so no certificate issued for
            // anything else can hand out a broker credential.
            "cluster-client-add" => ("cluster.manage", Level::Admin),
            "cluster-client-remove" => ("cluster.manage", Level::Admin),
            // #164: grants a registered Client its gateway; the same decision as an add.
            "cluster-client-gateway" => ("cluster.manage", Level::Admin),

            // ---- destructive: destroys data or availability
            "flush_storage" => ("flush_storage", Level::Destructive),
            // design.md §4.3 flags this row for review: `destructive` means
            // managers lose it, and the alternative is `admin`-with-exception.
            // The plan takes `destructive` (a reboot is availability loss on a
            // monitoring device) and the review may flip it (design.md §13 item 3).
            "restart_application" => ("restart_application", Level::Destructive),
            "power_off" => ("power_off_device", Level::Destructive),
            // Deliberately its own permission, not a reuse of
            // restart_application/power_off_device: a certificate that authorizes
            // a reboot or power-off must not thereby authorize wiping the
            // device's data. This exact string must match the Python side's
            // Permission.FACTORY_RESET.value, or a correctly signed factory_reset
            // is rejected here with PermissionDenied while the viewer believes
            // it succeeded.
            "factory_reset" => ("factory_reset", Level::Destructive),
            "remove_lorawan_sticker" => ("set_lorawan_sensor_config", Level::Destructive),
            "node-remove" => ("node.manage", Level::Destructive),
            // #71 control commands. They reuse the node-management permission
            // rather than introducing a new one, because anyone who can already
            // write node config can already reboot the device via
            // SetParam{save:true} — a dedicated permission would raise no real bar
            // while forcing every provisioned signer certificate to be re-issued.
            // Tightening this is a follow-up, not a prerequisite.
            "sticker_reboot" => ("set_lorawan_sensor_config", Level::Destructive),
            "sticker_device_reset" => ("set_lorawan_sensor_config", Level::Destructive),
            "beacon-remove" => ("beacon.manage", Level::Destructive),

            _ => {
                return Err(AuthError::InvalidCommand(format!(
                    "Unknown command type: {}",
                    command_type
                )))
            }
        };
        Ok((spec.0.to_string(), spec.1))
    }

    /// The permission half of [`Self::command_type_to_spec`], for the
    /// tests that predate the level and assert on permissions alone.
    #[cfg(test)]
    fn command_type_to_permission(&self, command_type: &str) -> AuthResult<String> {
        self.command_type_to_spec(command_type)
            .map(|(permission, _)| permission)
    }

    /// Build change preview from command
    fn build_change_preview(&self, command_type: &str, params: &Value) -> AuthResult<Value> {
        // `cluster-client-add`'s `reply_key` is not a change, and it is not a secret either: it is
        // the requester's public key for the sealed answer. Showing 43 characters of base64 to a
        // signer would read as a credential, so it is named and not shown.
        let mut changes = params.clone();
        if command_type == "cluster-client-add" {
            if let Some(map) = changes.as_object_mut() {
                if map.remove("reply_key").is_some() {
                    map.insert(
                        "reply".to_string(),
                        json!("password sealed to the requester's public key"),
                    );
                }
            }
        }
        Ok(json!({
            "command_type": command_type,
            "changes": changes,
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
            "set_sample_interval" => {
                let s = params.get("sample_interval_s").and_then(|v| v.as_u64()).unwrap_or(0);
                format!("Change 1-Wire read interval to {}s", s)
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
            "setup-complete" => {
                let role = params.get("role").and_then(|v| v.as_str()).unwrap_or("");
                format!("Mark setup complete, role \"{}\"", role)
            }
            "radio-set-mode" => {
                let mode = params.get("mode").and_then(|v| v.as_str()).unwrap_or("");
                format!(
                    "Change the Hub's radio to \"{}\" and restart the radio services and the \
                     Portal. Nothing is deleted; Nodes on a radio that goes off stay registered",
                    mode
                )
            }
            "northbridge-update" => {
                let force = params.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
                if force {
                    "Flash the Northbridge with the firmware this Hub's image bundles, even if it \
                     already runs it. The P2P radio stops for about a minute"
                        .to_string()
                } else {
                    "Flash the Northbridge with the firmware this Hub's image bundles. The P2P \
                     radio stops for about a minute"
                        .to_string()
                }
            }
            "southbridge-update" => {
                let force = params.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
                if force {
                    "Flash the Southbridge with the firmware this Hub's image bundles, even if it \
                     already runs it. Sensor power and the line LEDs stop for about 15 seconds"
                        .to_string()
                } else {
                    "Flash the Southbridge with the firmware this Hub's image bundles. Sensor \
                     power and the line LEDs stop for about 15 seconds"
                        .to_string()
                }
            }
            "cluster-client-add" => {
                let hostname = params.get("hostname").and_then(|v| v.as_str()).unwrap_or("");
                match params.get("gateway_eui").and_then(|v| v.as_str()).filter(|e| !e.is_empty()) {
                    Some(eui) => format!(
                        "Let Client Hub \"{}\" connect to this Hub's broker with a new password \
                         (an existing one stops working), and register its gateway {} in place \
                         of any other",
                        hostname, eui
                    ),
                    None => format!(
                        "Let Client Hub \"{}\" connect to this Hub's broker with a new password \
                         (an existing one stops working); a gateway it already has stays \
                         registered",
                        hostname
                    ),
                }
            }
            "cluster-client-gateway" => {
                let hostname = params.get("hostname").and_then(|v| v.as_str()).unwrap_or("");
                let eui = params
                    .get("gateway_eui")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                format!(
                    "Grant Client Hub \"{}\" gateway {}: it may publish as that gateway and the \
                     gateway is registered here; its password does not change",
                    hostname, eui
                )
            }
            "cluster-client-remove" => {
                let hostname = params.get("hostname").and_then(|v| v.as_str()).unwrap_or("");
                format!(
                    "Revoke Client Hub \"{}\": its broker account and its sessions end now, and its \
                     gateway is deregistered",
                    hostname
                )
            }
            "export-destination-set" => {
                let host = params.get("host").and_then(|v| v.as_str()).unwrap_or("");
                let anonymous = params
                    .get("username")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .is_empty();
                // The preview names the credential state without carrying the credential: an
                // operator confirming this needs to see that it is an anonymous export, which is
                // the mistake the write itself refuses when it is half-filled.
                format!(
                    "Export measurements to {} ({})",
                    host,
                    if anonymous { "anonymous" } else { "authenticated" }
                )
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
            "node-add" => {
                let dev_eui = params.get("dev_eui").and_then(|v| v.as_str()).unwrap_or("unknown");
                let serial = params.get("serial").and_then(|v| v.as_str()).unwrap_or("?");
                let radio = params.get("radio").and_then(|v| v.as_str()).unwrap_or("?");
                let profile = params.get("profile").and_then(|v| v.as_str()).unwrap_or("?");
                // The JoinEUI decides whether the device can join at all, so the operator
                // confirming the command sees which one — or that the Hub default applies.
                let join_eui = params
                    .get("join_eui")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .unwrap_or("the Hub default");
                format!(
                    "Enrol Node {} (serial {}) on the {} radio under profile \"{}\", JoinEUI {}",
                    dev_eui, serial, radio, profile, join_eui
                )
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
            "node-remove" => {
                let dev_eui = params.get("dev_eui").and_then(|v| v.as_str()).unwrap_or("unknown");
                let radio = params.get("radio").and_then(|v| v.as_str()).unwrap_or("?");
                format!("Deregister Node {} from the {} radio", dev_eui, radio)
            }
            // The consequence the operator must act on is in the preview: the Hub does not
            // switch the Node (node_registry.md §10 Q1).
            "node-move" => {
                let dev_eui = params.get("dev_eui").and_then(|v| v.as_str()).unwrap_or("unknown");
                let to = params.get("to").and_then(|v| v.as_str()).unwrap_or("?");
                format!(
                    "Move Node {} to the {} radio. The Hub does not switch the Node itself: \
                     switch it by NFC or shell radio-mode, or it cannot reach the Hub.",
                    dev_eui, to
                )
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
            "beacon-monitoring" => {
                let enabled = params.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
                format!(
                    "{} Beacon monitoring on this Hub",
                    if enabled { "Enable" } else { "Disable" }
                )
            }
            "beacon-send" => {
                let mut parts = Vec::new();
                if let Some(v) = params.get("auto_provision").and_then(|v| v.as_bool()) {
                    parts.push(format!("auto-provision={}", if v { "on" } else { "off" }));
                }
                if let Some(v) = params.get("auto_discover").and_then(|v| v.as_bool()) {
                    parts.push(format!("auto-discover={}", if v { "on" } else { "off" }));
                }
                format!("Set the Beacon subsystem ({})", parts.join(", "))
            }
            "beacon-recording" => {
                let mac = params.get("mac").and_then(|v| v.as_str()).unwrap_or("unknown");
                let interval = params.get("interval_min").and_then(|v| v.as_u64()).unwrap_or(0);
                format!("Set Beacon {} recording interval to {} min", mac, interval)
            }
            "beacon-history" => {
                let mac = params.get("mac").and_then(|v| v.as_str()).unwrap_or("unknown");
                let at = |k: &str| params.get(k).and_then(|v| v.as_i64());
                match (at("from"), at("to")) {
                    (None, None) => format!("Download Beacon {} temperature archive", mac),
                    (from, to) => format!(
                        "Download Beacon {} temperature archive ({} to {})",
                        mac,
                        from.map_or("last stored".to_string(), |t| t.to_string()),
                        to.map_or("now".to_string(), |t| t.to_string())
                    ),
                }
            }
            "beacon-add" => {
                let mac = params.get("mac").and_then(|v| v.as_str()).unwrap_or("unknown");
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                format!("Enrol Beacon {} (name: \"{}\")", mac, name)
            }
            "beacon-remove" => {
                let mac = params.get("mac").and_then(|v| v.as_str()).unwrap_or("unknown");
                format!("Remove Beacon {}", mac)
            }
            "beacon-detect" => {
                let mac = params.get("mac").and_then(|v| v.as_str()).unwrap_or("unknown");
                format!("Detect whether Beacon {} is an EN12830 recorder", mac)
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
            "beacon-provision" => {
                let mac = params.get("mac").and_then(|v| v.as_str()).unwrap_or("unknown");
                // Says what it writes, not just that it "provisions": this command changes the
                // Beacon's own advertising configuration over GATT, and an operator approving it
                // should know the change lands on the device, not on the Hub.
                format!(
                    "Write the {} sensor profile into Beacon {} over GATT",
                    proximos_brand::PRODUCT,
                    mac
                )
            }
            "known-beacons" => {
                let n = params
                    .get("macs")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                // The count, not the list: a fleet allowlist is long enough that
                // spelling it out would bury the rest of the confirmation prompt.
                format!("Listen for {} fleet-known Beacon(s)", n)
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
                    hysteresis: thresholds
                        .get("hysteresis")
                        .and_then(|v| v.as_f64())
                        .filter(|h| h.is_finite() && *h >= 0.0)
                        .map(|h| h as f32),
                    enabled: thresholds
                        .get("enabled")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true),
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
            "set_sample_interval" => {
                let sample_interval_s = challenge
                    .params
                    .get("sample_interval_s")
                    .and_then(|v| v.as_u64())
                    .ok_or_else(|| {
                        AuthError::InvalidCommand("Missing sample_interval_s".to_string())
                    })?;
                Ok(MqttCommand::SetSampleInterval { sample_interval_s })
            }
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
            "setup-complete" => {
                let role = challenge
                    .params
                    .get("role")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing role".to_string()))?
                    .to_string();

                // The closed set is checked here, at the only construction site, rather than at
                // the applier: a role this build does not know must be refused before it is
                // written, or the Hub ends up reporting a role nothing can read back.
                if role != "server" && role != "client" {
                    return Err(AuthError::InvalidCommand(format!(
                        "Unknown role: {role} (expected \"server\" or \"client\")"
                    )));
                }

                Ok(MqttCommand::SetupComplete { role })
            }
            "radio-set-mode" => {
                let mode = challenge
                    .params
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing mode".to_string()))?;
                // The closed set, checked at the one construction site: the executor then plans
                // against a mode that exists.
                let mode = control_radio::setmode::parse_mode(mode)
                    .map_err(|e| AuthError::InvalidCommand(e.message().to_string()))?;
                Ok(MqttCommand::SetRadioMode {
                    mode: mode.as_str().to_string(),
                })
            }
            "northbridge-update" => {
                // `force` only: the image is always the bundled one, and an absent flag is
                // `false`. A flag that is there but not a boolean is refused rather than read
                // as either, since `true` flashes past a spent retry budget.
                let force = match challenge.params.get("force") {
                    None | Some(Value::Null) => false,
                    Some(Value::Bool(b)) => *b,
                    Some(other) => {
                        return Err(AuthError::InvalidCommand(format!(
                            "force must be true or false, not {other}"
                        )))
                    }
                };
                Ok(MqttCommand::NorthbridgeUpdate { force })
            }
            "southbridge-update" => {
                // As `northbridge-update`: a `force` that is not a boolean is refused.
                let force = match challenge.params.get("force") {
                    None | Some(Value::Null) => false,
                    Some(Value::Bool(b)) => *b,
                    Some(other) => {
                        return Err(AuthError::InvalidCommand(format!(
                            "force must be true or false, not {other}"
                        )))
                    }
                };
                Ok(MqttCommand::SouthbridgeUpdate { force })
            }
            "cluster-client-add" | "cluster-client-gateway" | "cluster-client-remove" => {
                // Both checked here, at the one construction site, with the rules the apply path
                // itself uses — so a malformed name is refused before the operator confirms.
                let hostname = challenge
                    .params
                    .get("hostname")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing hostname".to_string()))?;
                let hostname = crate::libs::cluster_clients::validate_hostname(hostname)
                    .map_err(AuthError::InvalidCommand)?;
                if challenge.command_type == "cluster-client-remove" {
                    return Ok(MqttCommand::ClusterClientRemove { hostname });
                }
                if challenge.command_type == "cluster-client-gateway" {
                    let eui = challenge
                        .params
                        .get("gateway_eui")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            AuthError::InvalidCommand("Missing gateway_eui".to_string())
                        })?;
                    let gateway_eui = crate::libs::cluster_clients::validate_gateway_eui(eui)
                        .map_err(AuthError::InvalidCommand)?;
                    return Ok(MqttCommand::ClusterClientGateway {
                        hostname,
                        gateway_eui,
                    });
                }
                let gateway_eui = match challenge.params.get("gateway_eui") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(e)) if e.trim().is_empty() => None,
                    Some(Value::String(e)) => Some(
                        crate::libs::cluster_clients::validate_gateway_eui(e)
                            .map_err(AuthError::InvalidCommand)?,
                    ),
                    Some(_) => {
                        return Err(AuthError::InvalidCommand(
                            "gateway_eui must be a string".to_string(),
                        ))
                    }
                };
                // The key the new password is sealed to (cluster.md §7, amended 2026-10-02).
                // Required: the answer is published on config/response, which every loopback
                // subscriber reads, so there is no clear-text fallback. It is a parameter of the
                // signed request, so the signature covers it, and the seal is bound to the
                // signed request's id.
                let reply_key = challenge
                    .params
                    .get("reply_key")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        AuthError::InvalidCommand(
                            "Missing reply_key: cluster-client-add seals the new password to it"
                                .to_string(),
                        )
                    })?;
                let reply =
                    crate::libs::cluster_clients::ReplyTo::new(reply_key, &challenge.request_id)
                        .map_err(AuthError::InvalidCommand)?;
                Ok(MqttCommand::ClusterClientAdd {
                    hostname,
                    gateway_eui,
                    reply,
                })
            }
            "export-destination-set" => {
                let host = challenge
                    .params
                    .get("host")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing host".to_string()))?
                    .to_string();
                let username = challenge
                    .params
                    .get("username")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let password = challenge
                    .params
                    .get("password")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let enabled = challenge
                    .params
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);

                // The half-filled credential pair is refused here as well as at the applier: a
                // blank username silently means anonymous, and a challenge an operator approved
                // as authenticated must not become an anonymous export downstream.
                if username.trim().is_empty() && !password.is_empty() {
                    return Err(AuthError::InvalidCommand(
                        "a password without a username would enrol an anonymous export".to_string(),
                    ));
                }

                Ok(MqttCommand::SetExportDestination {
                    host,
                    username,
                    password,
                    enabled,
                })
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
                if !crate::libs::ble_host::is_valid_mac(&mac) {
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
                    hysteresis: challenge
                        .params
                        .get("hysteresis")
                        .and_then(|v| v.as_f64())
                        .filter(|h| h.is_finite() && *h >= 0.0),
                })
            }
            "delete_eye_field_threshold" => {
                let mac = challenge
                    .params
                    .get("mac")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing mac".to_string()))?
                    .to_uppercase();
                if !crate::libs::ble_host::is_valid_mac(&mac) {
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
            "node-add" => {
                // Shape-only checks here; `control_radio` validates the EUI, the key and
                // the profile against the rendered config when the command executes, and
                // its own error text is what the response carries.
                let required = |key: &str| -> AuthResult<String> {
                    challenge
                        .params
                        .get(key)
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(str::to_string)
                        .ok_or_else(|| AuthError::InvalidCommand(format!("Missing {key}")))
                };
                let radio = required("radio")?;
                if radio != "lorawan" && radio != "p2p" {
                    return Err(AuthError::InvalidCommand(format!(
                        "radio must be \"lorawan\" or \"p2p\", not {radio:?}"
                    )));
                }
                let app_key = required("app_key")?.to_lowercase();
                if app_key.len() != 32 || !app_key.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(AuthError::InvalidCommand(
                        "app_key must be exactly 32 hex characters".to_string(),
                    ));
                }
                let name = challenge
                    .params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string);
                // Optional (#86): blank or absent means `radio.join_eui`, which
                // `enroll_lorawan_node` fills in. OTAA has a JoinEUI and P2P does not, so a
                // P2P registration carrying one is a caller mistake, not a value to drop.
                let join_eui = challenge
                    .params
                    .get("join_eui")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_lowercase);
                if let Some(join_eui) = &join_eui {
                    if radio == "p2p" {
                        return Err(AuthError::InvalidCommand(
                            "join_eui is LoRaWAN-only: P2P has no JoinEUI".to_string(),
                        ));
                    }
                    if join_eui.len() != 16 || !join_eui.chars().all(|c| c.is_ascii_hexdigit()) {
                        return Err(AuthError::InvalidCommand(
                            "join_eui must be exactly 16 hex characters".to_string(),
                        ));
                    }
                }

                Ok(MqttCommand::NodeAdd {
                    radio,
                    profile: required("profile")?,
                    serial: required("serial")?,
                    dev_eui: required("dev_eui")?.to_lowercase(),
                    name,
                    app_key,
                    join_eui,
                })
            }
            "node-remove" => {
                let radio = challenge
                    .params
                    .get("radio")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing radio".to_string()))?
                    .to_string();
                if radio != "lorawan" && radio != "p2p" {
                    return Err(AuthError::InvalidCommand(format!(
                        "radio must be \"lorawan\" or \"p2p\", not {radio:?}"
                    )));
                }
                let dev_eui = challenge
                    .params
                    .get("dev_eui")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing dev_eui".to_string()))?
                    .to_lowercase();

                Ok(MqttCommand::NodeRemove { radio, dev_eui })
            }
            "node-move" => {
                // Shape only, like `node-add`: `control_radio::node_move` resolves the Node in
                // the registry and refuses what it cannot do, and its sentence is the answer.
                let dev_eui = challenge
                    .params
                    .get("dev_eui")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .ok_or_else(|| AuthError::InvalidCommand("Missing dev_eui".to_string()))?
                    .to_lowercase();
                if dev_eui.len() != 16 || !dev_eui.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(AuthError::InvalidCommand(
                        "dev_eui must be exactly 16 hex characters".to_string(),
                    ));
                }
                let to = challenge
                    .params
                    .get("to")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .ok_or_else(|| AuthError::InvalidCommand("Missing to".to_string()))?
                    .to_string();
                if to != "lorawan" && to != "p2p" {
                    return Err(AuthError::InvalidCommand(format!(
                        "to must be \"lorawan\" or \"p2p\", not {to:?}"
                    )));
                }

                Ok(MqttCommand::NodeMove { dev_eui, to })
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
            "beacon-monitoring" => {
                let enabled = challenge
                    .params
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing enabled".to_string()))?;
                Ok(MqttCommand::SetBeaconEnabled { enabled })
            }
            "beacon-send" => MqttCommand::parse_set_beacon_config(&challenge.params)
                .map_err(AuthError::InvalidCommand),
            "beacon-recording" => {
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
            "beacon-history" => {
                let mac = challenge
                    .params
                    .get("mac")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing mac".to_string()))?
                    .to_uppercase();
                let (from, to, job) =
                    crate::libs::mqtt::messages::beacon_history_window(&challenge.params)
                        .map_err(AuthError::InvalidCommand)?;
                Ok(MqttCommand::DownloadBeaconHistory { mac, from, to, job })
            }
            "beacon-add" => {
                let mac = challenge
                    .params
                    .get("mac")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing mac".to_string()))?
                    .to_uppercase();
                if !crate::libs::ble_host::is_valid_mac(&mac) {
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
            "known-beacons" => {
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
                    if !crate::libs::ble_host::is_valid_mac(&mac) {
                        return Err(AuthError::InvalidCommand(format!(
                            "Invalid MAC address: {mac}"
                        )));
                    }
                    macs.push(mac);
                }
                Ok(MqttCommand::SetBeaconKnownTags { macs })
            }
            "beacon-remove" => {
                let mac = challenge
                    .params
                    .get("mac")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing mac".to_string()))?
                    .to_uppercase();
                if !crate::libs::ble_host::is_valid_mac(&mac) {
                    return Err(AuthError::InvalidCommand(format!(
                        "Invalid MAC address: {mac}"
                    )));
                }
                Ok(MqttCommand::RemoveBeaconTag { mac })
            }
            "beacon-provision" => {
                let mac = challenge
                    .params
                    .get("mac")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing mac".to_string()))?
                    .to_uppercase();
                if !crate::libs::ble_host::is_valid_mac(&mac) {
                    return Err(AuthError::InvalidCommand(format!(
                        "Invalid MAC address: {mac}"
                    )));
                }
                Ok(MqttCommand::ProvisionBeacon { mac })
            }
            "beacon-detect" => {
                let mac = challenge
                    .params
                    .get("mac")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AuthError::InvalidCommand("Missing mac".to_string()))?
                    .to_uppercase();
                if !crate::libs::ble_host::is_valid_mac(&mac) {
                    return Err(AuthError::InvalidCommand(format!(
                        "Invalid MAC address: {mac}"
                    )));
                }
                Ok(MqttCommand::DetectBeaconTag { mac })
            }

            // The eight verbs that arrived unsigned in v1 and are signed
            // `read`/`operate` rows now (design.md §4.2). They reach this
            // builder through `execute_single_shot`, never through a
            // challenge, and they resolve to exactly the commands the
            // subscriber's unsigned arms produced — the same parsers, so
            // there is no second meaning for a signed one.
            "get_sensor_config" => Ok(MqttCommand::GetSensorConfig),
            "get_interval" => Ok(MqttCommand::GetInterval),
            "silence_buzzer" => Ok(MqttCommand::SilenceBuzzer),
            "get_sticker_config" => MqttCommand::parse_get_node_config(&challenge.params)
                .map_err(AuthError::InvalidCommand),
            "get_sticker_full_config" => MqttCommand::parse_get_node_full_config(&challenge.params)
                .map_err(AuthError::InvalidCommand),
            "get_sticker_info" => MqttCommand::parse_get_node_info(&challenge.params)
                .map_err(AuthError::InvalidCommand),
            "get_sticker_history" => MqttCommand::parse_get_node_history(&challenge.params)
                .map_err(AuthError::InvalidCommand),
            "sticker_force_send" => MqttCommand::parse_node_force_send(&challenge.params)
                .map_err(AuthError::InvalidCommand),

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
        let conn = self.audit_db()?;

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

    /// Log config response to database: the **verification** verdict.
    ///
    /// `APPROVED` records `SUCCESS` and leaves `applied` at `0`. Until the
    /// 2026-09-15 second review this wrote `applied = 1` here — before the
    /// command had run — and nothing corrected the row when execution then
    /// failed, so the audit trail said "applied" about changes the wire had
    /// already reported as `ERROR`. `applied` is now written by
    /// [`Self::log_config_execution`], from the execution result, by the
    /// monitor that ran the command. Verification and execution are two
    /// facts; the row keeps them in two columns.
    fn log_config_response(
        &self,
        challenge_id: &str,
        confirmation: &str,
        _timestamp: i64,
    ) -> AuthResult<()> {
        let conn = self.audit_db()?;

        let status = if confirmation == "APPROVED" {
            "SUCCESS"
        } else {
            "REJECTED"
        };

        conn.execute(
            "UPDATE config_changes SET verification_status = ?, applied = 0 WHERE challenge_id = ?",
            rusqlite::params![status, challenge_id],
        )
        .map_err(|e| AuthError::DatabaseError(format!("Failed to log response: {}", e)))?;

        Ok(())
    }

    /// Record what executing the verified command actually did.
    ///
    /// `Ok` sets `applied = 1`; `Err(e)` leaves `applied = 0` and writes `e`
    /// into `error_msg`, so a row that verified but did not take effect
    /// reads as exactly that. Called by `monitor.rs::run_and_answer` on both
    /// branches, for single-shot and confirmed commands alike — the same
    /// function that decides what `config/response` says, so the row and
    /// the wire cannot disagree about the outcome.
    pub fn log_config_execution(
        &self,
        challenge_id: &str,
        result: Result<(), &str>,
    ) -> AuthResult<()> {
        let conn = self.audit_db()?;

        match result {
            Ok(()) => conn.execute(
                "UPDATE config_changes SET applied = 1 WHERE challenge_id = ?",
                rusqlite::params![challenge_id],
            ),
            Err(error) => conn.execute(
                "UPDATE config_changes SET applied = 0, error_msg = ? WHERE challenge_id = ?",
                rusqlite::params![error, challenge_id],
            ),
        }
        .map_err(|e| AuthError::DatabaseError(format!("Failed to log execution: {}", e)))?;

        Ok(())
    }

    /// Log challenge expiry
    fn log_challenge_expired(&self, challenge: &PendingChallenge) -> AuthResult<()> {
        let conn = self.audit_db()?;

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
    /// That is exactly what happened to the v1 port's `set_lorawan_cluster` (system#7,
    /// since retired): the handler, the dispatch, the persistent state and the UI all shipped, and the
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
            "set_sample_interval",
            "set_system_info_interval",
            "add_signer",
            "remove_signer",
            "update_signer",
            "set_device_label",
            "setup-complete",
            "export-destination-set",
            "radio-set-mode",
            "northbridge-update",
            "southbridge-update",
            "cluster-client-add",
            "cluster-client-gateway",
            "cluster-client-remove",
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
            "node-add",
            "node-remove",
            "node-move",
            "remove_lorawan_sticker",
            "add_external_gateway",
            "remove_external_gateway",
            // `set_lorawan_cluster` left this list with its arm: retired, not forgotten
            // (`the_retired_set_lorawan_cluster_verb_is_unknown`).
            "beacon-monitoring",
            "beacon-send",
            "beacon-recording",
            "beacon-history",
            "beacon-add",
            "known-beacons",
            "beacon-remove",
            "beacon-detect",
            "beacon-provision",
            // The eight ex-`PLAIN` verbs (design.md §4.2). They reach the
            // builder through `execute_single_shot` rather than through a
            // challenge, but they reach it — so the same rule applies.
            "get_sensor_config",
            "get_interval",
            "silence_buzzer",
            "get_sticker_config",
            "get_sticker_full_config",
            "get_sticker_info",
            "get_sticker_history",
            "sticker_force_send",
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

    /// The v1 port's LoRaWAN leader/follower verb is retired (`plan/control/cluster.md` §3,
    /// decision #38): it never shipped, and a Client Hub now joins over BLE FB1C or `proximosctl
    /// control.radio create --role client`, never over a signed command. A request for it is
    /// refused before any challenge exists, like any verb this build does not know.
    #[test]
    fn the_retired_set_lorawan_cluster_verb_is_unknown() {
        let manager = create_test_manager();
        assert!(manager
            .command_type_to_permission("set_lorawan_cluster")
            .is_err());
    }

    #[test]
    fn every_beacon_verb_takes_the_beacon_manage_permission() {
        // The whole §8 table, one permission. v1 reused `set_lorawan_sensor_config` so that already
        // provisioned certificates would keep working; the rename to the §5 verbs forces re-issuance
        // anyway, so this is the moment to stop a Node-management certificate from also authorising
        // every Beacon operation on the Hub.
        let manager = create_test_manager();
        for cmd in [
            "beacon-add",
            "beacon-remove",
            "beacon-monitoring",
            "beacon-detect",
            "beacon-recording",
            "beacon-history",
            "beacon-provision",
            "beacon-send",
            "known-beacons",
        ] {
            assert_eq!(
                manager.command_type_to_permission(cmd).unwrap(),
                "beacon.manage",
                "unexpected permission for {cmd}"
            );
        }
    }

    #[test]
    fn the_v1_eye_command_vocabulary_is_gone() {
        // plan.md §9.8: v2 never accepts a `*_eye_*` string. Two survive on purpose — the field
        // thresholds are fleet config and are renamed with the rest of the threshold family in
        // plan/portal/control.md, not here.
        let manager = create_test_manager();
        for dead in [
            "set_eye_enabled",
            "set_eye_config",
            "set_eye_recording",
            "download_eye_history",
            "add_eye_tag",
            "remove_eye_tag",
            "detect_eye_tag",
            "set_eye_known_tags",
        ] {
            assert!(
                manager.command_type_to_permission(dead).is_err(),
                "{dead} must no longer be a known command type"
            );
            let challenge = test_challenge(dead, serde_json::json!({"mac": "AA:BB:CC:DD:EE:FF"}));
            assert!(
                manager.build_command_from_challenge(&challenge).is_err(),
                "{dead} must not parse into a command either"
            );
        }
        for survivor in ["set_eye_field_threshold", "delete_eye_field_threshold"] {
            assert!(
                manager.command_type_to_permission(survivor).is_ok(),
                "{survivor} is still v1-named until control.md renames the family"
            );
        }
    }

    #[test]
    fn every_beacon_verb_is_present_in_all_three_registries() {
        // plan.md §9.6. v1 shipped a command_type that was in the permission map and the executor
        // but not the challenge builder, so it was authorised, then failed to produce a prompt. A
        // verb has to exist in all three or it is broken in a way no single-registry test catches.
        let manager = create_test_manager();
        // (verb, the params a real portal would send)
        let verbs: [(&str, serde_json::Value); 9] = [
            (
                "beacon-add",
                serde_json::json!({"mac": "AA:BB:CC:DD:EE:FF", "name": "freezer"}),
            ),
            (
                "beacon-remove",
                serde_json::json!({"mac": "AA:BB:CC:DD:EE:FF"}),
            ),
            ("beacon-monitoring", serde_json::json!({"enabled": true})),
            (
                "beacon-detect",
                serde_json::json!({"mac": "AA:BB:CC:DD:EE:FF"}),
            ),
            (
                "beacon-recording",
                serde_json::json!({"mac": "AA:BB:CC:DD:EE:FF", "interval_min": 15}),
            ),
            (
                "beacon-history",
                serde_json::json!({"mac": "AA:BB:CC:DD:EE:FF"}),
            ),
            (
                "beacon-provision",
                serde_json::json!({"mac": "AA:BB:CC:DD:EE:FF"}),
            ),
            ("beacon-send", serde_json::json!({"auto_provision": true})),
            (
                "known-beacons",
                serde_json::json!({"macs": ["AA:BB:CC:DD:EE:FF"]}),
            ),
        ];

        for (verb, params) in &verbs {
            // 1. the permission map
            assert_eq!(
                manager.command_type_to_permission(verb).ok().as_deref(),
                Some("beacon.manage"),
                "{verb} is missing from command_type_to_permission"
            );

            // 2. the challenge builder — a verb with no arm falls through to a generic
            //    description, which is how v1's gap hid. Require it to name itself.
            let description = manager.describe_change(verb, params);
            assert!(!description.is_empty(), "{verb} has no confirmation prompt");
            assert!(
                description.to_lowercase().contains("beacon"),
                "{verb}'s prompt does not say what it acts on: {description}"
            );

            // 3. the parse arm — it must produce a command, and that command must report the same
            //    command_type it was built from. A mismatch here means the executor would see a
            //    different verb than the one that was authorised.
            let challenge = test_challenge(verb, params.clone());
            let cmd = manager
                .build_command_from_challenge(&challenge)
                .unwrap_or_else(|e| panic!("{verb} does not parse: {e}"));
            assert_eq!(
                cmd.name(),
                *verb,
                "{verb} round-trips as a different command type"
            );
        }
        assert_eq!(verbs.len(), 9, "the §8 table has nine Beacon verbs");
    }

    #[test]
    fn a_signed_beacon_history_carries_its_window_and_job() {
        // beacon_history.md §2.1: the Portal's `from`/`to`/`job` reach the dial; a reversed
        // window is refused at the signature, not discovered by the dial worker.
        let manager = create_test_manager();
        let challenge = test_challenge(
            "beacon-history",
            serde_json::json!({"mac": "aa:bb:cc:dd:ee:ff", "from": 100, "to": 200, "job": "bh-7"}),
        );
        match manager.build_command_from_challenge(&challenge).unwrap() {
            MqttCommand::DownloadBeaconHistory { mac, from, to, job } => {
                assert_eq!(mac, "AA:BB:CC:DD:EE:FF");
                assert_eq!(
                    (from, to, job.as_deref()),
                    (Some(100), Some(200), Some("bh-7"))
                );
            }
            other => panic!("expected DownloadBeaconHistory, got {other:?}"),
        }
        let description = manager.describe_change(
            "beacon-history",
            &serde_json::json!({"mac": "AA:BB:CC:DD:EE:FF", "from": 100}),
        );
        assert!(description.contains("100 to now"), "{description}");
        let reversed = test_challenge(
            "beacon-history",
            serde_json::json!({"mac": "AA:BB:CC:DD:EE:FF", "from": 200, "to": 100}),
        );
        assert!(manager.build_command_from_challenge(&reversed).is_err());
    }

    #[test]
    fn build_add_beacon_tag_uppercases_mac_and_keeps_name() {
        let manager = create_test_manager();
        let challenge = test_challenge(
            "beacon-add",
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
        let challenge = test_challenge("beacon-add", serde_json::json!({"mac": "not-a-mac"}));
        assert!(manager.build_command_from_challenge(&challenge).is_err());
    }

    #[test]
    fn beacon_send_parses_from_a_challenge() {
        let manager = create_test_manager();
        assert_eq!(
            manager.command_type_to_permission("beacon-send").unwrap(),
            "beacon.manage"
        );
        let challenge = test_challenge(
            "beacon-send",
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
        let empty = test_challenge("beacon-send", serde_json::json!({}));
        assert!(manager.build_command_from_challenge(&empty).is_err());
    }

    #[test]
    fn build_detect_beacon_tag_ok() {
        let manager = create_test_manager();
        let challenge = test_challenge(
            "beacon-detect",
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

    fn node_add_params(extra: Value) -> Value {
        let mut params = json!({
            "radio": "lorawan",
            "profile": "default",
            "serial": "11111",
            "dev_eui": "5876070000000413",
            "app_key": "0123456789abcdef0123456789abcdef",
        });
        if let (Some(p), Some(e)) = (params.as_object_mut(), extra.as_object()) {
            p.extend(e.clone());
        }
        params
    }

    fn built_join_eui(params: Value) -> Result<Option<String>, AuthError> {
        let manager = create_test_manager();
        match manager.build_command_from_challenge(&test_challenge("node-add", params))? {
            MqttCommand::NodeAdd { join_eui, .. } => Ok(join_eui),
            other => panic!("expected NodeAdd, got {other:?}"),
        }
    }

    /// #86: absent and blank both mean "the Hub default" — `None`, which
    /// `enroll_lorawan_node` fills with `radio.join_eui`.
    #[test]
    fn node_add_without_a_join_eui_leaves_it_to_the_hub_default() {
        assert_eq!(built_join_eui(node_add_params(json!({}))).unwrap(), None);
        assert_eq!(
            built_join_eui(node_add_params(json!({ "join_eui": "  " }))).unwrap(),
            None
        );
    }

    #[test]
    fn node_add_carries_the_operators_join_eui_lowercased() {
        assert_eq!(
            built_join_eui(node_add_params(json!({ "join_eui": " 00000000000000AB " })))
                .unwrap()
                .as_deref(),
            Some("00000000000000ab")
        );
    }

    #[test]
    fn node_add_refuses_a_malformed_join_eui_and_one_on_p2p() {
        for bad in ["123", "zz00000000000000", "00000000000000000"] {
            assert!(
                built_join_eui(node_add_params(json!({ "join_eui": bad }))).is_err(),
                "{bad} must be refused"
            );
        }
        let p2p = node_add_params(json!({ "radio": "p2p", "join_eui": "0000000000000000" }));
        let err = built_join_eui(p2p).unwrap_err();
        assert!(format!("{err:?}").contains("P2P"), "{err:?}");
    }

    /// The operator confirms against this preview, and the JoinEUI decides whether the
    /// device can join at all.
    #[test]
    fn describe_change_node_add_names_the_join_eui() {
        let manager = create_test_manager();
        let explicit = manager.describe_change(
            "node-add",
            &node_add_params(json!({ "join_eui": "0000000000000000" })),
        );
        assert!(explicit.contains("JoinEUI 0000000000000000"), "{explicit}");
        let default = manager.describe_change("node-add", &node_add_params(json!({})));
        assert!(default.contains("Hub default"), "{default}");
    }

    fn built_move(params: Value) -> Result<(String, String), AuthError> {
        let manager = create_test_manager();
        match manager.build_command_from_challenge(&test_challenge("node-move", params))? {
            MqttCommand::NodeMove { dev_eui, to } => Ok((dev_eui, to)),
            other => panic!("expected NodeMove, got {other:?}"),
        }
    }

    /// control.radio.md §7 (amendment 2026-09-27): `{dev_eui, to}`, the DevEUI lowercased.
    #[test]
    fn node_move_carries_the_dev_eui_and_the_target_carrier() {
        for to in ["lorawan", "p2p"] {
            assert_eq!(
                built_move(json!({ "dev_eui": " 587607000000572A ", "to": to })).unwrap(),
                ("587607000000572a".to_string(), to.to_string())
            );
        }
    }

    #[test]
    fn node_move_refuses_a_missing_or_unknown_target_and_a_malformed_dev_eui() {
        for (params, names) in [
            (json!({ "dev_eui": "5876070000005722" }), "to"),
            (json!({ "dev_eui": "5876070000005722", "to": "wifi" }), "to"),
            (json!({ "dev_eui": "5876070000005722", "to": "" }), "to"),
            (json!({ "to": "p2p" }), "dev_eui"),
            (json!({ "dev_eui": "5722", "to": "p2p" }), "dev_eui"),
            (
                json!({ "dev_eui": "zz76070000005722", "to": "p2p" }),
                "dev_eui",
            ),
        ] {
            let err = built_move(params.clone()).expect_err("refused");
            assert!(format!("{err:?}").contains(names), "{params}: {err:?}");
        }
    }

    /// Admin only, like `node-remove`, under the one Node permission; `admin`, not
    /// `destructive`, because nothing is lost.
    #[test]
    fn node_move_is_node_manage_at_the_admin_level() {
        let manager = create_test_manager();
        let (permission, level) = manager.command_type_to_spec("node-move").unwrap();
        assert_eq!(permission, "node.manage");
        assert_eq!(level, Level::Admin);
        let (remove, _) = manager.command_type_to_spec("node-remove").unwrap();
        assert_eq!(permission, remove, "the permission node-remove takes");
    }

    /// The operator confirms against this preview, so it says the Node must be switched too.
    #[test]
    fn describe_change_node_move_says_the_node_is_not_switched() {
        let manager = create_test_manager();
        let preview = manager.describe_change(
            "node-move",
            &json!({ "dev_eui": "5876070000005722", "to": "p2p" }),
        );
        assert!(preview.contains("5876070000005722"), "{preview}");
        assert!(preview.contains("p2p"), "{preview}");
        assert!(preview.contains("radio-mode"), "{preview}");
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
            level: None,
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
            "known-beacons",
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
        let challenge = test_challenge("known-beacons", serde_json::json!({ "macs": [] }));
        match manager.build_command_from_challenge(&challenge) {
            Ok(MqttCommand::SetBeaconKnownTags { macs }) => assert!(macs.is_empty()),
            other => panic!("expected SetBeaconKnownTags, got {:?}", other),
        }
    }

    #[test]
    fn set_beacon_known_tags_rejects_a_malformed_mac_and_a_missing_list() {
        let manager = create_test_manager();
        let bad_mac = test_challenge(
            "known-beacons",
            serde_json::json!({ "macs": ["7C:D9:F4:13:10:DE", "not-a-mac"] }),
        );
        assert!(manager.build_command_from_challenge(&bad_mac).is_err());

        let not_strings = test_challenge("known-beacons", serde_json::json!({ "macs": [42] }));
        assert!(manager.build_command_from_challenge(&not_strings).is_err());

        let missing = test_challenge("known-beacons", serde_json::json!({}));
        assert!(manager.build_command_from_challenge(&missing).is_err());
    }

    #[test]
    fn known_beacons_takes_the_beacon_permission_not_the_node_one() {
        // v1 mapped this to `set_lorawan_sensor_config`, so a certificate issued for Node
        // management could also widen this Hub's Beacon allowlist. It is a fleet-scoped write and
        // belongs with the rest of the Beacon verbs.
        let manager = create_test_manager();
        assert_eq!(
            manager.command_type_to_permission("known-beacons").unwrap(),
            "beacon.manage",
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

    /// #136: the audit database removed under a running `fiber_app` is
    /// recreated rather than failing every write, because a failed
    /// `log_config_request` rejects the command.
    #[test]
    fn an_audit_database_removed_under_the_manager_is_recreated() {
        let manager = create_test_manager();
        std::fs::remove_file(&manager.db_path).unwrap();

        manager.log_config_execution("challenge-1", Ok(())).unwrap();

        let conn = Connection::open(&manager.db_path).unwrap();
        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'config_changes'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1);
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
        // open_audit_db creates unencrypted (bare Connection::open, no PRAGMA
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
