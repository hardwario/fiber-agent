//! Pairing state machine
//!
//! Manages the state transitions for the device pairing protocol.

use std::time::Instant;

/// Pairing code expiration time in seconds (5 minutes)
pub const CODE_EXPIRY_SECS: u64 = 300;

/// Pairing protocol state
#[derive(Debug, Clone)]
pub enum PairingState {
    /// Pairing mode is not active
    Inactive,

    /// Waiting for MQTT pairing request with active code
    WaitingForRequest {
        /// The pairing code displayed on LCD
        code: String,
        /// When the code expires
        expires_at: Instant,
    },

    /// Processing a pairing request
    Processing {
        /// The code that was used (for encryption)
        code: String,
        /// Request ID being processed
        request_id: String,
    },
}

impl Default for PairingState {
    fn default() -> Self {
        Self::Inactive
    }
}

/// Pairing state machine
#[derive(Debug, Default)]
pub struct PairingStateMachine {
    /// Current state
    state: PairingState,
}

impl PairingStateMachine {
    /// Create a new state machine in inactive state
    pub fn new() -> Self {
        Self::default()
    }

    /// Get current state
    pub fn state(&self) -> &PairingState {
        &self.state
    }

    /// Check if pairing mode is active (waiting or processing)
    pub fn is_active(&self) -> bool {
        !matches!(self.state, PairingState::Inactive)
    }

    /// Check if waiting for a request
    pub fn is_waiting(&self) -> bool {
        matches!(self.state, PairingState::WaitingForRequest { .. })
    }

    /// Check if the current code has expired
    pub fn is_expired(&self) -> bool {
        match &self.state {
            PairingState::WaitingForRequest { expires_at, .. } => Instant::now() > *expires_at,
            _ => false,
        }
    }

    /// Get remaining time until expiry in seconds
    pub fn remaining_secs(&self) -> Option<u64> {
        match &self.state {
            PairingState::WaitingForRequest { expires_at, .. } => {
                let now = Instant::now();
                if now > *expires_at {
                    Some(0)
                } else {
                    Some((*expires_at - now).as_secs())
                }
            }
            _ => None,
        }
    }

    /// Get the current pairing code if waiting
    pub fn current_code(&self) -> Option<&str> {
        match &self.state {
            PairingState::WaitingForRequest { code, .. } => Some(code),
            PairingState::Processing { code, .. } => Some(code),
            PairingState::Inactive => None,
        }
    }

    /// Enter pairing mode with a new code
    pub fn start_pairing(&mut self, code: String) {
        let expires_at = Instant::now() + std::time::Duration::from_secs(CODE_EXPIRY_SECS);
        self.state = PairingState::WaitingForRequest { code, expires_at };
        eprintln!("[PairingState] Entered pairing mode, code expires in {} seconds", CODE_EXPIRY_SECS);
    }

    /// Begin processing a request
    ///
    /// Returns the pairing code if successful, or None if not in waiting state.
    pub fn begin_processing(&mut self, request_id: String) -> Option<String> {
        match &self.state {
            PairingState::WaitingForRequest { code, expires_at } => {
                if Instant::now() > *expires_at {
                    eprintln!("[PairingState] Cannot process: code expired");
                    return None;
                }
                let code = code.clone();
                self.state = PairingState::Processing {
                    code: code.clone(),
                    request_id,
                };
                eprintln!("[PairingState] Processing pairing request");
                Some(code)
            }
            _ => {
                eprintln!("[PairingState] Cannot process: not in waiting state");
                None
            }
        }
    }

    /// Complete pairing (success or failure) and return to inactive state
    pub fn complete(&mut self) {
        self.state = PairingState::Inactive;
        eprintln!("[PairingState] Pairing completed, returned to inactive");
    }

    /// Cancel pairing mode
    pub fn cancel(&mut self) {
        if self.is_active() {
            eprintln!("[PairingState] Pairing cancelled");
            self.state = PairingState::Inactive;
        }
    }

    /// Check expiration and auto-cancel if expired
    ///
    /// Returns true if state changed (was expired and now inactive).
    pub fn check_expiration(&mut self) -> bool {
        if self.is_expired() {
            eprintln!("[PairingState] Pairing code expired, returning to inactive");
            self.state = PairingState::Inactive;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_initial_state() {
        let sm = PairingStateMachine::new();
        assert!(matches!(sm.state(), PairingState::Inactive));
        assert!(!sm.is_active());
        assert!(!sm.is_waiting());
    }

    #[test]
    fn test_start_pairing() {
        let mut sm = PairingStateMachine::new();
        sm.start_pairing("ABC123".to_string());

        assert!(sm.is_active());
        assert!(sm.is_waiting());
        assert_eq!(sm.current_code(), Some("ABC123"));
        assert!(!sm.is_expired());
    }

    #[test]
    fn test_remaining_time() {
        let mut sm = PairingStateMachine::new();
        sm.start_pairing("XYZ789".to_string());

        let remaining = sm.remaining_secs().unwrap();
        // Should be close to CODE_EXPIRY_SECS (within a second)
        assert!(remaining >= CODE_EXPIRY_SECS - 1);
        assert!(remaining <= CODE_EXPIRY_SECS);
    }

    #[test]
    fn test_begin_processing() {
        let mut sm = PairingStateMachine::new();
        sm.start_pairing("TEST12".to_string());

        let code = sm.begin_processing("req-001".to_string());
        assert_eq!(code, Some("TEST12".to_string()));

        // Should now be in processing state
        assert!(sm.is_active());
        assert!(!sm.is_waiting());
        assert_eq!(sm.current_code(), Some("TEST12"));
    }

    #[test]
    fn test_begin_processing_when_inactive() {
        let mut sm = PairingStateMachine::new();

        let code = sm.begin_processing("req-001".to_string());
        assert_eq!(code, None);
    }

    #[test]
    fn test_complete() {
        let mut sm = PairingStateMachine::new();
        sm.start_pairing("ABC123".to_string());
        sm.begin_processing("req-001".to_string());
        sm.complete();

        assert!(!sm.is_active());
        assert!(matches!(sm.state(), PairingState::Inactive));
    }

    #[test]
    fn test_cancel() {
        let mut sm = PairingStateMachine::new();
        sm.start_pairing("ABC123".to_string());
        sm.cancel();

        assert!(!sm.is_active());
    }

    #[test]
    fn test_expiration_check() {
        let mut sm = PairingStateMachine::new();

        // Manually set an already-expired state
        sm.state = PairingState::WaitingForRequest {
            code: "EXPIRED".to_string(),
            expires_at: Instant::now() - Duration::from_secs(1),
        };

        assert!(sm.is_expired());

        let changed = sm.check_expiration();
        assert!(changed);
        assert!(!sm.is_active());
    }

    #[test]
    fn test_no_remaining_when_inactive() {
        let sm = PairingStateMachine::new();
        assert_eq!(sm.remaining_secs(), None);
    }
}
