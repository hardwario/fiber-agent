//! Challenge state tracking for authorization flow

use serde_json::Value;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// State of a pending challenge
#[derive(Debug, Clone, PartialEq)]
pub enum ChallengeState {
    /// Awaiting confirmation from authorized signer
    AwaitingConfirmation,

    /// Confirmation received, applying changes
    Applying,

    /// Successfully applied
    Applied,

    /// Rejected by signer
    Rejected,

    /// Expired (timeout exceeded)
    Expired,

    /// Failed to apply (error occurred)
    Failed,
}

/// Pending challenge awaiting confirmation
#[derive(Debug, Clone)]
pub struct PendingChallenge {
    /// Unique challenge ID (UUID)
    pub challenge_id: String,

    /// Original request ID
    pub request_id: String,

    /// Signer ID who initiated the request
    pub signer_id: String,

    /// Signer's full name (for audit trail)
    pub signer_name: String,

    /// Command type (set_threshold, restart_application, etc.)
    pub command_type: String,

    /// Command parameters as JSON
    pub params: Value,

    /// Optional reason for the change
    pub reason: Option<String>,

    /// Original signature from request
    pub signature: String,

    /// Original nonce from request
    pub nonce: String,

    /// Timestamp when request was received
    pub timestamp: i64,

    /// Timestamp when challenge expires
    pub expires_at: i64,

    /// Current state
    pub state: ChallengeState,

    /// Timestamp when state last changed
    pub state_changed_at: i64,
}

impl PendingChallenge {
    /// Create a new pending challenge
    pub fn new(
        challenge_id: String,
        request_id: String,
        signer_id: String,
        signer_name: String,
        command_type: String,
        params: Value,
        reason: Option<String>,
        signature: String,
        nonce: String,
        timestamp: i64,
        expires_at: i64,
    ) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        Self {
            challenge_id,
            request_id,
            signer_id,
            signer_name,
            command_type,
            params,
            reason,
            signature,
            nonce,
            timestamp,
            expires_at,
            state: ChallengeState::AwaitingConfirmation,
            state_changed_at: now,
        }
    }

    /// Check if challenge has expired
    pub fn is_expired(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        now > self.expires_at
    }

    /// Update challenge state
    pub fn set_state(&mut self, new_state: ChallengeState) {
        self.state = new_state;
        self.state_changed_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
    }
}

/// Challenge registry for tracking active challenges
pub struct ChallengeRegistry {
    /// Active challenges indexed by challenge_id
    challenges: HashMap<String, PendingChallenge>,

    /// Maximum number of concurrent challenges
    max_concurrent_challenges: usize,
}

impl ChallengeRegistry {
    /// Create a new challenge registry
    pub fn new(max_concurrent_challenges: usize) -> Self {
        Self {
            challenges: HashMap::new(),
            max_concurrent_challenges,
        }
    }

    /// Add a new challenge
    pub fn add_challenge(&mut self, challenge: PendingChallenge) -> Result<(), String> {
        // Check limit
        if self.challenges.len() >= self.max_concurrent_challenges {
            return Err(format!(
                "Maximum concurrent challenges reached: {}",
                self.max_concurrent_challenges
            ));
        }

        // Check for duplicate
        if self.challenges.contains_key(&challenge.challenge_id) {
            return Err(format!(
                "Challenge already exists: {}",
                challenge.challenge_id
            ));
        }

        self.challenges.insert(challenge.challenge_id.clone(), challenge);
        Ok(())
    }

    /// Get a challenge by ID
    pub fn get_challenge(&self, challenge_id: &str) -> Option<&PendingChallenge> {
        self.challenges.get(challenge_id)
    }

    /// Get a mutable challenge by ID
    pub fn get_challenge_mut(&mut self, challenge_id: &str) -> Option<&mut PendingChallenge> {
        self.challenges.get_mut(challenge_id)
    }

    /// Remove a challenge
    pub fn remove_challenge(&mut self, challenge_id: &str) -> Option<PendingChallenge> {
        self.challenges.remove(challenge_id)
    }

    /// Cleanup expired challenges
    pub fn cleanup_expired(&mut self) -> Vec<PendingChallenge> {
        let expired: Vec<String> = self
            .challenges
            .iter()
            .filter(|(_, c)| c.is_expired() && c.state == ChallengeState::AwaitingConfirmation)
            .map(|(id, _)| id.clone())
            .collect();

        let mut expired_challenges = Vec::new();
        for id in expired {
            if let Some(mut challenge) = self.challenges.remove(&id) {
                challenge.set_state(ChallengeState::Expired);
                expired_challenges.push(challenge);
            }
        }

        expired_challenges
    }

    /// Get count of active challenges
    pub fn active_count(&self) -> usize {
        self.challenges.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_challenge_expiry() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Not expired
        let challenge = PendingChallenge::new(
            "test-123".to_string(),
            "req-456".to_string(),
            "dr@example.com".to_string(),
            "Dr. Test".to_string(),
            "set_threshold".to_string(),
            serde_json::json!({}),
            None,
            "signature".to_string(),
            "nonce".to_string(),
            now,
            now + 300, // Expires in 5 minutes
        );

        assert!(!challenge.is_expired());

        // Expired
        let expired_challenge = PendingChallenge::new(
            "test-789".to_string(),
            "req-012".to_string(),
            "dr@example.com".to_string(),
            "Dr. Test".to_string(),
            "set_threshold".to_string(),
            serde_json::json!({}),
            None,
            "signature".to_string(),
            "nonce".to_string(),
            now - 600,
            now - 300, // Expired 5 minutes ago
        );

        assert!(expired_challenge.is_expired());
    }

    #[test]
    fn test_challenge_registry() {
        let mut registry = ChallengeRegistry::new(10);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let challenge = PendingChallenge::new(
            "test-123".to_string(),
            "req-456".to_string(),
            "dr@example.com".to_string(),
            "Dr. Test".to_string(),
            "set_threshold".to_string(),
            serde_json::json!({}),
            None,
            "signature".to_string(),
            "nonce".to_string(),
            now,
            now + 300,
        );

        // Add challenge
        assert!(registry.add_challenge(challenge.clone()).is_ok());
        assert_eq!(registry.active_count(), 1);

        // Get challenge
        assert!(registry.get_challenge("test-123").is_some());

        // Remove challenge
        assert!(registry.remove_challenge("test-123").is_some());
        assert_eq!(registry.active_count(), 0);
    }
}
