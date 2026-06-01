//! Ephemeral BLE provisioning session.
//!
//! When the user holds ENTER long enough to trigger provisioning mode, a fresh
//! `ProvisioningSession` is minted: a random 6-char A-Z0-9 token, a 5-minute
//! expiry, and a precomputed `QrCodeGenerator` that bakes both into a v:2 QR
//! payload. The session is shared (read-only) with the display thread (to
//! render the QR) and the BLE GATT layer (to authorize pairing).
//!
//! Lifecycle: the session is `None` outside provisioning mode. It becomes
//! `Some` on countdown completion and is cleared on user exit, on successful
//! provisioning, or once `expires_at` passes.

use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use rand::Rng;

use super::QrCodeGenerator;

/// Default lifetime for a provisioning session.
pub const DEFAULT_SESSION_DURATION: Duration = Duration::from_secs(5 * 60);

const TOKEN_LEN: usize = 6;
const TOKEN_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

pub struct ProvisioningSession {
    token: String,
    expires_at_unix: u64,
    qr_generator: Arc<QrCodeGenerator>,
}

impl ProvisioningSession {
    /// Create a new session: random token + `duration`-long lifetime,
    /// pre-rendered v:2 QR for `mac_address` / `hostname`.
    pub fn new(
        mac_address: &str,
        hostname: &str,
        duration: Duration,
    ) -> Result<Self> {
        let token = generate_token();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let expires_at_unix = now.saturating_add(duration.as_secs());

        let qr = QrCodeGenerator::new(
            mac_address.to_string(),
            token.clone(),
            expires_at_unix,
            hostname.to_string(),
        )?;

        Ok(Self {
            token,
            expires_at_unix,
            qr_generator: Arc::new(qr),
        })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn expires_at_unix(&self) -> u64 {
        self.expires_at_unix
    }

    pub fn qr_generator(&self) -> Arc<QrCodeGenerator> {
        self.qr_generator.clone()
    }

    /// True once the wall-clock has passed `expires_at`.
    pub fn is_expired(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now >= self.expires_at_unix
    }

    /// Constant-time-ish comparison against this session's token. Returns
    /// false if the session has expired or the token does not match.
    /// Trims whitespace from the attempt to tolerate trailing newlines from
    /// BLE clients.
    pub fn verify(&self, attempt: &str) -> bool {
        if self.is_expired() {
            return false;
        }
        let a = attempt.trim().as_bytes();
        let b = self.token.as_bytes();
        if a.len() != b.len() {
            return false;
        }
        let mut diff: u8 = 0;
        for i in 0..b.len() {
            diff |= a[i] ^ b[i];
        }
        diff == 0
    }
}

fn generate_token() -> String {
    let mut rng = rand::thread_rng();
    (0..TOKEN_LEN)
        .map(|_| {
            let idx = rng.gen_range(0..TOKEN_ALPHABET.len());
            TOKEN_ALPHABET[idx] as char
        })
        .collect()
}

/// Shared handle: `None` outside provisioning mode, `Some` while active.
pub type SharedProvisioningSession = Arc<RwLock<Option<ProvisioningSession>>>;

pub fn new_shared_provisioning_session() -> SharedProvisioningSession {
    Arc::new(RwLock::new(None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_has_expected_shape() {
        let t = generate_token();
        assert_eq!(t.len(), TOKEN_LEN);
        for c in t.chars() {
            assert!(
                TOKEN_ALPHABET.contains(&(c as u8)),
                "unexpected token char: {c}"
            );
        }
    }

    #[test]
    fn tokens_are_unique_in_practice() {
        // not a strict guarantee but a smoke test for the RNG path
        let a = generate_token();
        let b = generate_token();
        let c = generate_token();
        assert!(!(a == b && b == c), "three tokens collided: {a}");
    }

    #[test]
    fn verify_accepts_own_token() {
        let s = ProvisioningSession::new("AA:BB:CC:DD:EE:FF", "FIBER-T", Duration::from_secs(60))
            .expect("session");
        assert!(s.verify(s.token()));
        // whitespace tolerated
        assert!(s.verify(&format!("{}\r\n", s.token())));
    }

    #[test]
    fn verify_rejects_wrong_and_short() {
        let s = ProvisioningSession::new("AA:BB:CC:DD:EE:FF", "FIBER-T", Duration::from_secs(60))
            .expect("session");
        assert!(!s.verify(""));
        assert!(!s.verify("00000"));
        assert!(!s.verify("0000000"));
        // flip one char of the real token
        let mut bad: Vec<u8> = s.token().as_bytes().to_vec();
        bad[0] = if bad[0] == b'A' { b'B' } else { b'A' };
        assert!(!s.verify(&String::from_utf8(bad).unwrap()));
    }

    #[test]
    fn expired_session_rejects_even_correct_token() {
        // duration=0 → expires_at == now → already expired
        let s = ProvisioningSession::new("AA:BB:CC:DD:EE:FF", "FIBER-T", Duration::from_secs(0))
            .expect("session");
        assert!(s.is_expired());
        assert!(!s.verify(s.token()));
    }
}
