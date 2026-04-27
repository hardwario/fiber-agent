//! Authentication characteristic (FB01) — write PIN, read auth status.
//!
//! Uses constant-time comparison for the PIN check to avoid leaking the
//! PIN length or content via timing side-channels. This is especially
//! relevant now that the project is open-source.

use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthResponse {
    pub success: bool,
    pub message: String,
}

/// Constant-time PIN comparison. Trims whitespace from `attempt`.
pub fn verify_pin(attempt: &str, expected: &str) -> bool {
    let a = attempt.trim().as_bytes();
    let e = expected.as_bytes();
    a.ct_eq(e).into()
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

    #[test]
    fn correct_pin_passes() {
        assert!(verify_pin("123456", "123456"));
    }

    #[test]
    fn wrong_pin_fails() {
        assert!(!verify_pin("000000", "123456"));
    }

    #[test]
    fn shorter_pin_fails() {
        assert!(!verify_pin("12345", "123456"));
    }

    #[test]
    fn longer_pin_fails() {
        assert!(!verify_pin("1234567", "123456"));
    }

    #[test]
    fn whitespace_is_trimmed() {
        assert!(verify_pin(" 123456 \n", "123456"));
        assert!(verify_pin("123456\r\n", "123456"));
    }

    #[test]
    fn empty_attempt_against_real_pin_fails() {
        assert!(!verify_pin("", "123456"));
    }

    #[test]
    fn auth_response_message_matches_state() {
        assert_eq!(auth_response(true).message, "Authenticated");
        assert_eq!(auth_response(false).message, "Not authenticated");
        assert!(auth_response(true).success);
        assert!(!auth_response(false).success);
    }
}
