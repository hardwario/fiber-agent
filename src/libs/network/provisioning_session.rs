//! Ephemeral BLE provisioning session.
//!
//! Lifecycle (idle-reset model):
//! - Holding ENTER on the device for the countdown duration mints a fresh
//!   session: random 6-char A-Z0-9 token + a precomputed v:2 QR payload.
//! - `last_activity` is bumped to *now* on creation and again on every BLE
//!   GATT read/write the phone performs (FB01–FB0x). Each handler in
//!   `ble::gatt::service` calls `touch()` before doing its work.
//! - `is_expired()` returns true once `now - last_activity >= IDLE_TIMEOUT`.
//!   So an actively-used session can stretch arbitrarily long; an
//!   abandoned one dies after IDLE_TIMEOUT and the button thread tears it
//!   down (clears the slot, stops advertising, returns to overview).
//! - The QR carries `exp = created_at + IDLE_TIMEOUT` as a *scan-by hint*
//!   for the mobile app: if no one scans by then the session is already
//!   gone (idle the whole time). After a single successful interaction the
//!   real deadline rolls forward; the QR value is no longer a hard limit.
//!
//! `last_activity` is an `AtomicU64` so handlers only need a read-lock on
//! the outer `RwLock<Option<ProvisioningSession>>` to bump it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use rand::Rng;

use super::QrCodeGenerator;

/// How long a session may sit idle (no BLE activity) before the device
/// tears it down. Active phones can stretch sessions past this by
/// continuing to read/write characteristics.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Backwards-compat alias for the QR `exp` field generation — kept so older
/// callers reading `DEFAULT_SESSION_DURATION` still link. Same value as
/// [`IDLE_TIMEOUT`]; do not assume sessions die after exactly this long.
pub const DEFAULT_SESSION_DURATION: Duration = IDLE_TIMEOUT;

const TOKEN_LEN: usize = 6;
const TOKEN_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

pub struct ProvisioningSession {
    token: String,
    /// Unix seconds when the session was first minted. Only used to render
    /// the QR `exp` hint; not consulted for expiry decisions.
    created_at_unix: u64,
    /// Unix seconds of the last BLE GATT op observed for this session.
    /// Bumped via [`touch()`] from each FB0x handler. Atomic so callers
    /// can bump while holding only a read-lock on the outer slot.
    last_activity_unix: AtomicU64,
    qr_generator: Arc<QrCodeGenerator>,
}

impl ProvisioningSession {
    /// Mint a new session. `last_activity` starts at *now* so a freshly
    /// minted session does not appear stale before the phone even
    /// connects.
    pub fn new(mac_address: &str, hostname: &str) -> Result<Self> {
        let token = generate_token();
        let now = now_unix();
        // `exp` in the QR is a scan-by hint, not a binding deadline.
        let qr_exp = now.saturating_add(IDLE_TIMEOUT.as_secs());

        let qr = QrCodeGenerator::new(
            mac_address.to_string(),
            token.clone(),
            qr_exp,
            hostname.to_string(),
        )?;

        Ok(Self {
            token,
            created_at_unix: now,
            last_activity_unix: AtomicU64::new(now),
            qr_generator: Arc::new(qr),
        })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn created_at_unix(&self) -> u64 {
        self.created_at_unix
    }

    /// Last-activity timestamp (Unix seconds). Useful for diagnostics.
    pub fn last_activity_unix(&self) -> u64 {
        self.last_activity_unix.load(Ordering::Relaxed)
    }

    pub fn qr_generator(&self) -> Arc<QrCodeGenerator> {
        self.qr_generator.clone()
    }

    /// Record a BLE GATT op against this session. Cheap (atomic store).
    /// Idempotent. Safe to call from a handler that only holds the outer
    /// `RwLock` for read.
    pub fn touch(&self) {
        self.last_activity_unix.store(now_unix(), Ordering::Relaxed);
    }

    /// True iff the session has been idle for at least [`IDLE_TIMEOUT`].
    pub fn is_expired(&self) -> bool {
        let last = self.last_activity_unix.load(Ordering::Relaxed);
        now_unix().saturating_sub(last) >= IDLE_TIMEOUT.as_secs()
    }

    /// Test-only: force `last_activity` to a specific Unix timestamp so
    /// tests can simulate idle-aging without sleeping.
    #[cfg(test)]
    pub fn set_last_activity_unix_for_test(&self, t: u64) {
        self.last_activity_unix.store(t, Ordering::Relaxed);
    }

    /// Constant-time-ish comparison against this session's token.
    /// Returns false when the session is idle-expired or the token does
    /// not match. Trims whitespace from `attempt` to tolerate trailing
    /// newlines from BLE clients.
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

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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

/// Bump activity on the live session if one exists. Cheap when idle
/// (read-lock + atomic store). Returns true if a session was found and
/// touched. Intended to be called at the top of every BLE GATT handler.
pub fn touch_shared(session: &SharedProvisioningSession) -> bool {
    match session.read() {
        Ok(guard) => match guard.as_ref() {
            Some(s) => {
                s.touch();
                true
            }
            None => false,
        },
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

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
        let a = generate_token();
        let b = generate_token();
        let c = generate_token();
        assert!(!(a == b && b == c), "three tokens collided: {a}");
    }

    #[test]
    fn verify_accepts_own_token() {
        let s = ProvisioningSession::new("AA:BB:CC:DD:EE:FF", "FIBER-T").expect("session");
        assert!(s.verify(s.token()));
        assert!(s.verify(&format!("{}\r\n", s.token())));
    }

    #[test]
    fn verify_rejects_wrong_and_short() {
        let s = ProvisioningSession::new("AA:BB:CC:DD:EE:FF", "FIBER-T").expect("session");
        assert!(!s.verify(""));
        assert!(!s.verify("00000"));
        assert!(!s.verify("0000000"));
        let mut bad: Vec<u8> = s.token().as_bytes().to_vec();
        bad[0] = if bad[0] == b'A' { b'B' } else { b'A' };
        assert!(!s.verify(&String::from_utf8(bad).unwrap()));
    }

    #[test]
    fn fresh_session_is_not_expired() {
        let s = ProvisioningSession::new("AA:BB", "FIBER-T").expect("session");
        assert!(!s.is_expired());
        assert!(s.verify(s.token()));
    }

    #[test]
    fn manually_aged_session_is_expired_until_touched() {
        let s = ProvisioningSession::new("AA:BB", "FIBER-T").expect("session");
        let too_old = now_unix().saturating_sub(IDLE_TIMEOUT.as_secs() + 1);
        // Backdate last_activity past the idle window.
        s.last_activity_unix.store(too_old, Ordering::Relaxed);
        assert!(s.is_expired());
        assert!(!s.verify(s.token()));

        // A touch resets the clock — session is alive again.
        s.touch();
        assert!(!s.is_expired());
        assert!(s.verify(s.token()));
    }

    #[test]
    fn touch_shared_bumps_only_when_session_present() {
        let slot = new_shared_provisioning_session();
        assert!(!touch_shared(&slot), "no session yet");

        let session = ProvisioningSession::new("AA:BB", "FIBER-T").expect("session");
        let before = session.last_activity_unix();
        *slot.write().unwrap() = Some(session);

        // Make sure at least one second of wall-clock can pass so the
        // touch is observable. Cheap test: backdate, then touch.
        slot.write()
            .unwrap()
            .as_mut()
            .unwrap()
            .last_activity_unix
            .store(before.saturating_sub(10), Ordering::Relaxed);

        assert!(touch_shared(&slot));
        let after = slot.read().unwrap().as_ref().unwrap().last_activity_unix();
        assert!(after >= before, "touch_shared should bump last_activity");
    }

    #[test]
    #[ignore = "wall-clock dependent; run manually"]
    fn idle_session_eventually_expires() {
        // Sanity check that the timeout constant is honored. Excluded
        // from CI because it would sleep IDLE_TIMEOUT.
        let s = ProvisioningSession::new("AA:BB", "FIBER-T").expect("session");
        thread::sleep(IDLE_TIMEOUT + Duration::from_secs(1));
        assert!(s.is_expired());
    }
}
