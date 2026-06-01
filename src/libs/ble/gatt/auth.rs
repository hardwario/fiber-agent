//! Authentication characteristic (FB01) — write provisioning token, read auth status.
//!
//! Uses constant-time comparison against the active ephemeral provisioning
//! session (no more static PIN). Auth fails when the session is absent or
//! expired, so an attacker holding an old QR cannot reuse it once the user
//! leaves provisioning mode.

use serde::{Deserialize, Serialize};

use crate::libs::network::SharedProvisioningSession;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthResponse {
    pub success: bool,
    pub message: String,
}

/// Verify an attempted token against the currently active provisioning
/// session. Returns false if no session is active or it has expired.
pub fn verify_token(attempt: &str, session: &SharedProvisioningSession) -> bool {
    let Ok(guard) = session.read() else {
        return false;
    };
    match guard.as_ref() {
        Some(s) => s.verify(attempt),
        None => false,
    }
}

pub fn auth_response(authenticated: bool) -> AuthResponse {
    AuthResponse {
        success: authenticated,
        message: if authenticated {
            "Authenticated".to_string()
        } else {
            "Not authenticated".to_string()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::network::{new_shared_provisioning_session, ProvisioningSession};
    use std::time::Duration;

    fn shared_with(session: ProvisioningSession) -> SharedProvisioningSession {
        let s = new_shared_provisioning_session();
        *s.write().unwrap() = Some(session);
        s
    }

    #[test]
    fn no_active_session_rejects_everything() {
        let s = new_shared_provisioning_session();
        assert!(!verify_token("ABC123", &s));
        assert!(!verify_token("", &s));
    }

    #[test]
    fn correct_token_passes_against_live_session() {
        let session = ProvisioningSession::new("AA:BB", "FIBER-T", Duration::from_secs(60))
            .expect("session");
        let token = session.token().to_string();
        let shared = shared_with(session);
        assert!(verify_token(&token, &shared));
        assert!(verify_token(&format!("{}\r\n", token), &shared));
    }

    #[test]
    fn wrong_token_fails() {
        let session = ProvisioningSession::new("AA:BB", "FIBER-T", Duration::from_secs(60))
            .expect("session");
        let shared = shared_with(session);
        assert!(!verify_token("000000", &shared));
        assert!(!verify_token("", &shared));
        assert!(!verify_token("ZZZZZZZ", &shared));
    }

    #[test]
    fn expired_session_rejects_correct_token() {
        let session = ProvisioningSession::new("AA:BB", "FIBER-T", Duration::from_secs(0))
            .expect("session");
        let token = session.token().to_string();
        let shared = shared_with(session);
        assert!(!verify_token(&token, &shared));
    }

    #[test]
    fn auth_response_message_matches_state() {
        assert_eq!(auth_response(true).message, "Authenticated");
        assert_eq!(auth_response(false).message, "Not authenticated");
        assert!(auth_response(true).success);
        assert!(!auth_response(false).success);
    }
}
