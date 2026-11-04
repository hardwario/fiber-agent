//! The ephemeral provisioning session — `plan/control/ble/peripheral.md` §3, AC 5.
//!
//! **There is no stored PIN in v2.** The FB01 credential is a per-session token, and that is
//! strictly stronger than a per-device persistent PIN: there is no secret at rest to exfiltrate, and
//! physical presence is required for **every** session rather than once.
//!
//! It closes the v1 `@main` defect — `ble.pin: "123456"`, in config, identical on every device — the
//! same way v1 `@dev` closed it.
//!
//! Five properties are the contract, and each has a test:
//!
//! - 6 characters from a 36-symbol alphabet, from a **CSPRNG**, sampled without modulo bias;
//! - every GATT operation bumps `last_activity`; the session dies after the idle timeout;
//! - compared in **constant time**, and `attempt.trim()` is compared so a trailing newline from a
//!   BLE client is tolerated;
//! - the QR payload is exactly the v2 JSON the app already scans;
//! - **never persisted, never logged, never in a result envelope, never on argv** — which is why
//!   `Debug` redacts it and the CLI only ever sees a masked preview.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::RwLock;

use rand::rngs::OsRng;
use rand::RngCore;
use subtle::ConstantTimeEq;

use crate::host::SystemClock;
use crate::mac::MacAddr;

pub const TOKEN_LEN: usize = 6;
/// 36 symbols. Deliberately **not** ambiguity-stripped: the portal pairing code has its own
/// stripped alphabet, and peripheral.md §3's warning is that conflating the three six-character
/// codes is a naming decision to make once, not silently here.
pub const TOKEN_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// What the CLI is allowed to print in place of the token.
pub const TOKEN_MASK: &str = "******";

/// The idle timeout a host uses when it has no configured value — the same 300 s the shipped
/// `ble.peripheral.session_idle_timeout_s` carries.
///
/// It exists for the front-panel path: an operator holding ENTER is starting a session before
/// anything has read the config section, and a hard-coded number in the button handler would be a
/// second place for this value to live.
pub const DEFAULT_IDLE_TIMEOUT_S: u64 = 300;

/// Mint a token uniformly over the alphabet.
///
/// Rejection sampling, not `byte % 36`: the modulo would make the first four symbols very slightly
/// more likely, and biasing a credential is not a corner worth cutting for two lines of code.
fn mint_token() -> String {
    let n = TOKEN_ALPHABET.len() as u8; // 36
    let limit = u8::MAX - (u8::MAX % n); // 252: bytes >= this are rejected
    let mut out = String::with_capacity(TOKEN_LEN);
    let mut buf = [0u8; 32];
    while out.len() < TOKEN_LEN {
        OsRng.fill_bytes(&mut buf);
        for b in buf {
            if out.len() == TOKEN_LEN {
                break;
            }
            if b < limit {
                out.push(char::from(TOKEN_ALPHABET[(b % n) as usize]));
            }
        }
    }
    out
}

/// The QR payload the Hub renders on its LCD. Field order is the contract the app already parses.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QrPayload {
    pub t: String,
    pub v: u32,
    pub m: String,
    pub tok: String,
    pub n: String,
    /// `created_at + idle_timeout` — a scan-by **hint**, not a hard deadline.
    pub exp: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionStatus {
    pub active: bool,
    pub expires_at: Option<i64>,
}

pub struct Session {
    token: String,
    created_at: i64,
    last_activity: AtomicI64,
    idle_timeout_s: u64,
}

/// Redacted on purpose. A `{:?}` of a session must never be the thing that puts the token in a log.
impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("token", &"<redacted>")
            .field("created_at", &self.created_at)
            .field("last_activity", &self.last_activity.load(Ordering::SeqCst))
            .field("idle_timeout_s", &self.idle_timeout_s)
            .finish()
    }
}

impl Session {
    pub fn mint(clock: &dyn SystemClock, idle_timeout_s: u64) -> Session {
        let now = clock.now_unix();
        Session {
            token: mint_token(),
            created_at: now,
            last_activity: AtomicI64::new(now),
            idle_timeout_s,
        }
    }

    pub const fn created_at(&self) -> i64 {
        self.created_at
    }

    pub const fn idle_timeout_s(&self) -> u64 {
        self.idle_timeout_s
    }

    /// The scan-by hint. Not enforced: the session lives on activity, not on this value.
    pub fn expires_at_hint(&self) -> i64 {
        self.created_at + self.idle_timeout_s as i64
    }

    /// When the session would die if nothing else happened.
    pub fn deadline(&self) -> i64 {
        self.last_activity.load(Ordering::SeqCst) + self.idle_timeout_s as i64
    }

    /// Every GATT operation calls this.
    pub fn touch(&self, clock: &dyn SystemClock) {
        self.last_activity.store(clock.now_unix(), Ordering::SeqCst);
    }

    pub fn is_expired(&self, clock: &dyn SystemClock) -> bool {
        let idle = clock
            .now_unix()
            .saturating_sub(self.last_activity.load(Ordering::SeqCst));
        idle >= self.idle_timeout_s as i64
    }

    /// Constant-time comparison of a trimmed attempt.
    ///
    /// An expired session rejects even the correct token: the timeout is the security property, so
    /// it is checked first and cannot be skipped by knowing the secret.
    pub fn verify(&self, clock: &dyn SystemClock, attempt: &str) -> bool {
        if self.is_expired(clock) {
            return false;
        }
        let attempt = attempt.trim();
        // The *length* is not secret, and checking it first keeps the compare fixed-width.
        if attempt.len() != self.token.len() {
            return false;
        }
        attempt.as_bytes().ct_eq(self.token.as_bytes()).into()
    }

    /// The real payload, token included. **Only the LCD renderer may call this.**
    pub fn qr_payload(&self, mac: &MacAddr, hostname: &str) -> QrPayload {
        QrPayload {
            t: "fiber".to_string(),
            v: 2,
            m: mac.to_string(),
            tok: self.token.clone(),
            n: hostname.to_string(),
            exp: self.expires_at_hint(),
        }
    }

    /// The same payload with the token masked — what a CLI or a portal response may carry.
    pub fn qr_payload_preview(&self, mac: &MacAddr, hostname: &str) -> QrPayload {
        QrPayload {
            tok: TOKEN_MASK.to_string(),
            ..self.qr_payload(mac, hostname)
        }
    }
}

/// The one live session, if any.
#[derive(Debug, Default)]
pub struct SessionSlot {
    inner: RwLock<Option<Session>>,
    /// Bumped on every mint, and never reused. It is how state kept *outside* this module — FB1A's
    /// attempt counter, today — can tell "the same session, seen again" from "a new operator at
    /// the front panel" without holding the token or a pointer to the session.
    ///
    /// A connection is not a session and must not be read as one: the token survives a disconnect,
    /// so a peer that drops and re-writes the same token is still inside the session it was in.
    /// Only the front-panel button mints, so only the front-panel button moves this.
    generation: AtomicU64,
}

fn read_lock<T>(l: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|p| p.into_inner())
}
fn write_lock<T>(l: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|p| p.into_inner())
}

impl SessionSlot {
    pub fn new() -> SessionSlot {
        SessionSlot::default()
    }

    /// Mint a session, replacing any live one, and return the **masked** preview.
    ///
    /// The real token leaves this module only through [`Session::qr_payload`], which the LCD
    /// renderer calls. Nothing that can reach a shell, a log or an MQTT response sees it.
    pub fn start(
        &self,
        clock: &dyn SystemClock,
        idle_timeout_s: u64,
        mac: &MacAddr,
        hostname: &str,
    ) -> QrPayload {
        let s = Session::mint(clock, idle_timeout_s);
        let preview = s.qr_payload_preview(mac, hostname);
        *write_lock(&self.inner) = Some(s);
        self.generation.fetch_add(1, Ordering::SeqCst);
        preview
    }

    /// Which session this is, counting from 1. `0` means none has ever been minted.
    ///
    /// Per-session state elsewhere compares this value rather than caching a session: a mismatch is
    /// a new session and nothing else, and a session that has merely ended cannot be confused with
    /// one that has been replaced — no FB1A write can reach the counter without a live session
    /// anyway, since [`crate::peripheral::gatt::GattCtx::gate`] runs first.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// The real payload for the LCD. `None` when no session is live.
    pub fn qr_payload(&self, mac: &MacAddr, hostname: &str) -> Option<QrPayload> {
        read_lock(&self.inner)
            .as_ref()
            .map(|s| s.qr_payload(mac, hostname))
    }

    /// End the session. Every authenticated peer dies with it — that is the caller's job to act on.
    pub fn stop(&self) -> bool {
        write_lock(&self.inner).take().is_some()
    }

    pub fn is_live(&self, clock: &dyn SystemClock) -> bool {
        read_lock(&self.inner)
            .as_ref()
            .is_some_and(|s| !s.is_expired(clock))
    }

    /// Re-base the idle timer onto a clock that has just been **stepped**, even if the session now
    /// looks expired against it.
    ///
    /// This is not a way to revive a timed-out session — [`Self::touch_if_live`] deliberately
    /// refuses that. It exists because the idle timer is measured against wall-clock time, so a
    /// clock *step* injects fictitious idleness: an RTC-less Hub bootstrapped from 1970 to the
    /// present jumps decades in one FB0B write, and the phone would be de-authenticated by its own
    /// successful operation. A step is not idleness.
    ///
    /// Only the FB0B handler calls this, and only after a write that already passed the gate.
    pub fn rebase_after_clock_step(&self, clock: &dyn SystemClock) {
        if let Some(s) = read_lock(&self.inner).as_ref() {
            s.touch(clock);
        }
    }

    /// Bump the idle timer if a session is live. Called by every gated characteristic.
    pub fn touch_if_live(&self, clock: &dyn SystemClock) {
        if let Some(s) = read_lock(&self.inner).as_ref() {
            if !s.is_expired(clock) {
                s.touch(clock);
            }
        }
    }

    pub fn verify(&self, clock: &dyn SystemClock, attempt: &str) -> bool {
        read_lock(&self.inner)
            .as_ref()
            .is_some_and(|s| s.verify(clock, attempt))
    }

    pub fn status(&self, clock: &dyn SystemClock) -> SessionStatus {
        match read_lock(&self.inner).as_ref() {
            Some(s) if !s.is_expired(clock) => SessionStatus {
                active: true,
                expires_at: Some(s.deadline()),
            },
            _ => SessionStatus {
                active: false,
                expires_at: None,
            },
        }
    }

    /// Drop an idle session. Called by the housekeeping tick; `true` means one was just reaped, and
    /// the caller de-authenticates every peer.
    pub fn expire_if_idle(&self, clock: &dyn SystemClock) -> bool {
        let expired = read_lock(&self.inner)
            .as_ref()
            .is_some_and(|s| s.is_expired(clock));
        if expired {
            write_lock(&self.inner).take();
        }
        expired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeClock;

    fn mac() -> MacAddr {
        MacAddr::parse("B8:27:EB:11:22:33").expect("mac")
    }

    #[test]
    fn token_has_the_documented_shape() {
        let clock = FakeClock::at(1_000);
        for _ in 0..50 {
            let s = Session::mint(&clock, 300);
            let payload = s.qr_payload(&mac(), "hub");
            assert_eq!(payload.tok.chars().count(), TOKEN_LEN);
            for c in payload.tok.bytes() {
                assert!(
                    TOKEN_ALPHABET.contains(&c),
                    "{c:?} is not in the 36-symbol alphabet"
                );
            }
        }
        assert_eq!(TOKEN_ALPHABET.len(), 36);
    }

    #[test]
    fn tokens_are_unique_in_practice() {
        let clock = FakeClock::at(1_000);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..100 {
            let s = Session::mint(&clock, 300);
            assert!(
                seen.insert(s.qr_payload(&mac(), "hub").tok),
                "a repeated token in 100 mints means the CSPRNG is not being used"
            );
        }
    }

    #[test]
    fn the_alphabet_is_covered_so_sampling_is_not_stuck_on_a_subset() {
        // A modulo-biased or truncated sampler would visibly miss symbols over this many draws.
        let clock = FakeClock::at(1_000);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..500 {
            for c in Session::mint(&clock, 300)
                .qr_payload(&mac(), "hub")
                .tok
                .bytes()
            {
                seen.insert(c);
            }
        }
        assert_eq!(
            seen.len(),
            TOKEN_ALPHABET.len(),
            "every symbol must be reachable"
        );
    }

    #[test]
    fn a_correct_token_passes_and_tolerates_a_trailing_newline() {
        let clock = FakeClock::at(1_000);
        let s = Session::mint(&clock, 300);
        let tok = s.qr_payload(&mac(), "hub").tok;
        assert!(s.verify(&clock, &tok));
        assert!(
            s.verify(&clock, &format!("{tok}\n")),
            "a BLE client may append one"
        );
        assert!(s.verify(&clock, &format!("  {tok}  ")));
        assert!(s.verify(&clock, &format!("{tok}\r\n")));
    }

    #[test]
    fn a_wrong_token_fails() {
        let clock = FakeClock::at(1_000);
        let s = Session::mint(&clock, 300);
        let tok = s.qr_payload(&mac(), "hub").tok;
        assert!(
            !s.verify(&clock, "000000"),
            "unless it happens to be the token"
        );
        assert!(!s.verify(&clock, ""));
        assert!(
            !s.verify(&clock, &tok[..TOKEN_LEN - 1]),
            "a prefix is not enough"
        );
        assert!(
            !s.verify(&clock, &format!("{tok}X")),
            "nor is a superstring"
        );
        // The alphabet is upper case, so a lowercased token must not pass — unless this draw
        // happened to be all digits, in which case lowercasing changed nothing.
        if tok.chars().any(|c| c.is_ascii_alphabetic()) {
            assert!(!s.verify(&clock, &tok.to_lowercase()));
        }
    }

    #[test]
    fn an_idle_expired_session_rejects_the_correct_token() {
        let clock = FakeClock::at(1_000);
        let s = Session::mint(&clock, 300);
        let tok = s.qr_payload(&mac(), "hub").tok;
        assert!(s.verify(&clock, &tok));
        clock.set(1_000 + 300);
        assert!(
            s.is_expired(&clock),
            "the timeout is inclusive: 300 s of silence with a 300 s timeout is expired"
        );
        assert!(
            !s.verify(&clock, &tok),
            "the timeout is checked first, so knowing the secret cannot skip it"
        );
    }

    #[test]
    fn every_gatt_op_bumps_the_idle_timer() {
        // Touch at t=299 then verify at t=598: with a 300 s timeout, the session is still alive
        // because the touch moved the deadline.
        let clock = FakeClock::at(0);
        let s = Session::mint(&clock, 300);
        let tok = s.qr_payload(&mac(), "hub").tok;
        clock.set(299);
        s.touch(&clock);
        clock.set(598);
        assert!(s.verify(&clock, &tok), "299 + 300 = 599 > 598");
        clock.set(599);
        assert!(!s.verify(&clock, &tok), "and one second later it is gone");
    }

    #[test]
    fn qr_payload_is_the_v2_json_the_app_scans() {
        let clock = FakeClock::at(1_700_000_000);
        let s = Session::mint(&clock, 300);
        let payload = s.qr_payload(&mac(), "proximos-hub-1");
        let json = serde_json::to_string(&payload).expect("serialize");

        // The exact key order the app's parser was written against.
        assert!(
            json.starts_with(r#"{"t":"fiber","v":2,"m":"B8:27:EB:11:22:33","tok":""#),
            "{json}"
        );
        assert!(json.contains(r#""n":"proximos-hub-1""#), "{json}");
        assert!(json.ends_with(r#","exp":1700000300}"#), "{json}");

        let back: QrPayload = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(back.t, "fiber");
        assert_eq!(back.v, 2);
        assert_eq!(back.exp, 1_700_000_000 + 300, "created + idle_timeout");
        assert_eq!(back.exp, s.expires_at_hint());
    }

    #[test]
    fn debug_and_display_never_leak_the_token() {
        let clock = FakeClock::at(1_000);
        let s = Session::mint(&clock, 300);
        let tok = s.qr_payload(&mac(), "hub").tok;
        let dbg = format!("{s:?}");
        assert!(dbg.contains("<redacted>"), "{dbg}");
        assert!(
            !dbg.contains(&tok),
            "a {{:?}} of a session must never be how the token reaches a log: {dbg}"
        );

        // And the slot's Debug is built from the session's, so it is redacted too.
        let slot = SessionSlot::new();
        slot.start(&clock, 300, &mac(), "hub");
        let dbg = format!("{slot:?}");
        assert!(dbg.contains("<redacted>"), "{dbg}");
    }

    #[test]
    fn the_cli_preview_masks_the_token() {
        let clock = FakeClock::at(1_000);
        let slot = SessionSlot::new();
        let preview = slot.start(&clock, 300, &mac(), "hub");
        assert_eq!(preview.tok, TOKEN_MASK);
        // Everything else is real, so the operator can still confirm which Hub they are looking at.
        assert_eq!(preview.m, "B8:27:EB:11:22:33");
        assert_eq!(preview.n, "hub");
        assert_eq!(preview.exp, 1_300);

        // The real payload is still available to the LCD, and it differs.
        let real = slot.qr_payload(&mac(), "hub").expect("live");
        assert_ne!(real.tok, TOKEN_MASK);
        assert!(slot.verify(&clock, &real.tok));
    }

    #[test]
    fn the_slot_tracks_one_session_and_stopping_it_de_authenticates_everything() {
        let clock = FakeClock::at(1_000);
        let slot = SessionSlot::new();
        assert!(!slot.is_live(&clock));
        assert_eq!(
            slot.status(&clock),
            SessionStatus {
                active: false,
                expires_at: None
            }
        );
        assert!(!slot.stop(), "stopping nothing is not an error");

        slot.start(&clock, 300, &mac(), "hub");
        let tok = slot.qr_payload(&mac(), "hub").expect("live").tok;
        assert!(slot.is_live(&clock));
        assert_eq!(
            slot.status(&clock),
            SessionStatus {
                active: true,
                expires_at: Some(1_300)
            }
        );
        assert!(slot.verify(&clock, &tok));

        assert!(slot.stop());
        assert!(!slot.is_live(&clock));
        assert!(
            !slot.verify(&clock, &tok),
            "the token dies with the session, and so does every peer authenticated by it"
        );
    }

    #[test]
    fn starting_a_second_session_replaces_the_first_and_invalidates_its_token() {
        let clock = FakeClock::at(1_000);
        let slot = SessionSlot::new();
        slot.start(&clock, 300, &mac(), "hub");
        let first = slot.qr_payload(&mac(), "hub").expect("live").tok;
        slot.start(&clock, 300, &mac(), "hub");
        let second = slot.qr_payload(&mac(), "hub").expect("live").tok;
        assert_ne!(first, second);
        assert!(!slot.verify(&clock, &first), "the old token is dead");
        assert!(slot.verify(&clock, &second));
    }

    #[test]
    fn the_generation_moves_only_when_a_session_is_minted() {
        // What FB1A's attempt counter keys on. Only the front-panel button mints, so only it may
        // change this value: a verify, a touch or the session simply ending must not, or per-session
        // state elsewhere would reset itself on something that is not a new operator.
        let clock = FakeClock::at(1_000);
        let slot = SessionSlot::new();
        assert_eq!(slot.generation(), 0, "nothing has ever been minted");

        slot.start(&clock, 300, &mac(), "hub");
        let first = slot.generation();
        assert_eq!(first, 1);

        let tok = slot.qr_payload(&mac(), "hub").expect("live").tok;
        assert!(slot.verify(&clock, &tok));
        slot.touch_if_live(&clock);
        assert_eq!(
            slot.generation(),
            first,
            "the same session, however many times it is used"
        );

        assert!(slot.stop());
        assert_eq!(
            slot.generation(),
            first,
            "ending a session does not mint one, and nothing can be spent against a dead one"
        );

        slot.start(&clock, 300, &mac(), "hub");
        assert_eq!(
            slot.generation(),
            first + 1,
            "a new operator, a new session"
        );
    }

    #[test]
    fn the_housekeeping_tick_reaps_an_idle_session_exactly_once() {
        let clock = FakeClock::at(0);
        let slot = SessionSlot::new();
        slot.start(&clock, 300, &mac(), "hub");
        assert!(!slot.expire_if_idle(&clock), "not idle yet");
        clock.set(300);
        assert!(slot.expire_if_idle(&clock), "reaped");
        assert!(
            !slot.expire_if_idle(&clock),
            "and the edge fires once, so the de-auth happens once"
        );
        assert!(!slot.is_live(&clock));
    }

    #[test]
    fn touch_if_live_does_not_resurrect_an_expired_session() {
        let clock = FakeClock::at(0);
        let slot = SessionSlot::new();
        slot.start(&clock, 300, &mac(), "hub");
        let tok = slot.qr_payload(&mac(), "hub").expect("live").tok;
        clock.set(400);
        slot.touch_if_live(&clock);
        assert!(
            !slot.verify(&clock, &tok),
            "an expired session must not be revived by a late read"
        );
        assert!(!slot.is_live(&clock));
    }

    #[test]
    fn the_idle_timeout_is_configurable_per_session() {
        let clock = FakeClock::at(0);
        let slot = SessionSlot::new();
        // `phone-session-start --idle-timeout-s 60`
        let preview = slot.start(&clock, 60, &mac(), "hub");
        assert_eq!(preview.exp, 60);
        clock.set(59);
        assert!(slot.is_live(&clock));
        clock.set(60);
        assert!(!slot.is_live(&clock));
    }
}
