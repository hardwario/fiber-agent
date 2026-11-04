//! The FB01 credential — `plan/control/ble/peripheral.md` §3, AC 5.
//!
//! Exercised through the **public** surface, which is the point: the token must be unreachable from
//! outside the module except as a QR payload, and this file is where that is provable.

use std::sync::Arc;

use control_ble::host::{FakeClock, SystemClock};
use control_ble::mac::MacAddr;
use control_ble::peripheral::session::{
    QrPayload, Session, SessionSlot, TOKEN_ALPHABET, TOKEN_LEN, TOKEN_MASK,
};

fn hub() -> MacAddr {
    MacAddr::parse("B8:27:EB:11:22:33").expect("mac")
}

/// The only public route to a real token: the payload the LCD renders.
fn token_of(slot: &SessionSlot) -> String {
    slot.qr_payload(&hub(), "test-hub").expect("live").tok
}

#[test]
fn token_has_the_documented_shape() {
    let clock = FakeClock::at(1_700_000_000);
    for _ in 0..50 {
        let s = Session::mint(&clock, 300);
        let tok = s.qr_payload(&hub(), "test-hub").tok;
        assert_eq!(tok.chars().count(), TOKEN_LEN, "six characters");
        for b in tok.bytes() {
            assert!(
                TOKEN_ALPHABET.contains(&b),
                "{b:?} is outside the 36-symbol alphabet"
            );
        }
    }
    assert_eq!(TOKEN_ALPHABET.len(), 36);
    assert_eq!(TOKEN_LEN, 6);
}

#[test]
fn tokens_are_unique_in_practice() {
    let clock = FakeClock::at(1_700_000_000);
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..100 {
        let tok = Session::mint(&clock, 300)
            .qr_payload(&hub(), "test-hub")
            .tok;
        assert!(
            seen.insert(tok),
            "a repeat inside 100 mints means the CSPRNG is not being used"
        );
    }
}

#[test]
fn a_correct_token_passes_and_tolerates_a_trailing_newline() {
    let clock = FakeClock::at(1_700_000_000);
    let slot = SessionSlot::new();
    slot.start(&clock, 300, &hub(), "test-hub");
    let tok = token_of(&slot);
    assert!(slot.verify(&clock, &tok));
    assert!(
        slot.verify(&clock, &format!("{tok}\n")),
        "a BLE client may append one"
    );
    assert!(slot.verify(&clock, &format!("\r\n  {tok}  \r\n")));
}

#[test]
fn a_wrong_token_fails() {
    let clock = FakeClock::at(1_700_000_000);
    let slot = SessionSlot::new();
    slot.start(&clock, 300, &hub(), "test-hub");
    let tok = token_of(&slot);
    assert!(!slot.verify(&clock, ""));
    assert!(!slot.verify(&clock, "000000"));
    assert!(!slot.verify(&clock, &tok[..TOKEN_LEN - 1]), "a prefix");
    assert!(!slot.verify(&clock, &format!("{tok}X")), "a superstring");
    // With no session at all, nothing verifies.
    slot.stop();
    assert!(!slot.verify(&clock, &tok));
}

#[test]
fn an_idle_expired_session_rejects_the_correct_token() {
    let clock = FakeClock::at(0);
    let slot = SessionSlot::new();
    slot.start(&clock, 300, &hub(), "test-hub");
    let tok = token_of(&slot);
    assert!(slot.verify(&clock, &tok));
    clock.set(300);
    assert!(
        !slot.verify(&clock, &tok),
        "the timeout is checked first, so knowing the secret cannot skip it"
    );
    assert!(!slot.is_live(&clock));
}

#[test]
fn every_gatt_op_bumps_the_idle_timer() {
    // Touch at 299, verify at 598: with a 300 s timeout the session is alive because the touch moved
    // the deadline. This is the property that lets a long provisioning flow finish.
    let clock = FakeClock::at(0);
    let slot = SessionSlot::new();
    slot.start(&clock, 300, &hub(), "test-hub");
    let tok = token_of(&slot);
    clock.set(299);
    slot.touch_if_live(&clock);
    clock.set(598);
    assert!(slot.verify(&clock, &tok), "299 + 300 = 599 > 598");
    clock.set(599);
    assert!(!slot.verify(&clock, &tok));
}

#[test]
fn qr_payload_is_the_v2_json_the_app_scans() {
    let clock = FakeClock::at(1_700_000_000);
    let s = Session::mint(&clock, 300);
    let payload = s.qr_payload(&hub(), "proximos-hub-1");
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
    let clock = FakeClock::at(1_700_000_000);
    let slot = SessionSlot::new();
    slot.start(&clock, 300, &hub(), "test-hub");
    let tok = token_of(&slot);
    let dbg = format!("{slot:?}");
    assert!(dbg.contains("<redacted>"), "{dbg}");
    assert!(
        !dbg.contains(&tok),
        "a {{:?}} of the slot must never be how the token reaches a log: {dbg}"
    );
}

#[test]
fn the_cli_preview_masks_the_token() {
    let clock = FakeClock::at(1_700_000_000);
    let slot = SessionSlot::new();
    // `start` returns the preview — the only thing a CLI, an MQTT response or a portal may see.
    let preview = slot.start(&clock, 300, &hub(), "test-hub");
    assert_eq!(preview.tok, TOKEN_MASK);
    let json = serde_json::to_string(&preview).expect("serialize");
    let real = token_of(&slot);
    assert!(
        !json.contains(&real),
        "the preview must not carry the token: {json}"
    );
    // Everything else is real, so an operator can still confirm which Hub they are looking at.
    assert_eq!(preview.m, "B8:27:EB:11:22:33");
    assert_eq!(preview.n, "test-hub");
    assert_eq!(preview.exp, 1_700_000_300);
}

#[test]
fn there_is_no_public_accessor_that_returns_the_token_as_a_bare_string() {
    // The token leaves the module only inside a QR payload. This is a compile-time property, so the
    // test is the absence of an alternative: the slot exposes status, verification and payloads, and
    // `Session` has no `token()`.
    let clock = FakeClock::at(1_700_000_000);
    let slot = SessionSlot::new();
    slot.start(&clock, 300, &hub(), "test-hub");
    let status = slot.status(&clock);
    assert!(status.active);
    assert_eq!(status.expires_at, Some(1_700_000_300));
    let as_json = serde_json::to_string(&status).expect("serialize");
    for forbidden in ["tok", "token", "pin"] {
        assert!(
            !as_json.contains(forbidden),
            "{forbidden} must not appear in a status document: {as_json}"
        );
    }
}

#[test]
fn the_session_is_shareable_across_tasks_the_way_the_gatt_server_needs() {
    // `SessionSlot` is what the GATT handlers and the housekeeping tick share, so it has to be
    // usable behind an `Arc` from more than one place at once.
    let clock: Arc<dyn SystemClock> = Arc::new(FakeClock::at(0));
    let slot = Arc::new(SessionSlot::new());
    slot.start(clock.as_ref(), 300, &hub(), "test-hub");
    let tok = token_of(&slot);

    let a = slot.clone();
    let b = slot.clone();
    let c = clock.clone();
    let t = std::thread::spawn(move || a.verify(c.as_ref(), &tok));
    assert!(t.join().expect("joins"), "verified from another thread");
    assert!(b.is_live(clock.as_ref()));
}
