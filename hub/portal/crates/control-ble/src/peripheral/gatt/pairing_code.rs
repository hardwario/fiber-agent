//! FB1A — submit the portal pairing code.
//! `plan/control/ble/peripheral.md` §6.5, GitLab #61.
//!
//! Today the app must POST the code to the portal over the LAN, and the login path is an explicit
//! guess; the in-app entry has been hidden in the wizard for exactly that reason. This hands the
//! code to the portal on this machine instead, over a link the phone always has.
//!
//! **Which code.** The front-panel *pairing* code — six characters from the ambiguity-stripped
//! alphabet, no O, 0, I, 1 or L, because an operator reads it off one screen and types it into
//! another. §3 warns that three different six-character codes exist and an operator meets all of
//! them, so every message this characteristic produces names the one it wanted.
//!
//! **This Hub cannot tell you the code is right.** The pairing protocol uses it as the password
//! the *response* is encrypted under, so a wrong code fails at the portal, which is the only party
//! that can tell. What is checked here is the shape — and that check is deliberately stricter than
//! the portal's own form, which accepts any six alphanumerics.

use bluer::gatt::local::ReqError;

use super::{GattCtx, WriteResult};
use crate::host::Origin;
use crate::peripheral::buffers::PeerAddr;

/// The alphabet, mirrored from `control-core`'s `libs/pairing/code.rs`. Mirrored rather than
/// imported because this crate may not depend on that one — the dependency runs the other way.
const CHARSET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
const CODE_LENGTH: usize = 6;

/// How many code submissions one session may make, and over what window.
///
/// The **first rate limit in this crate**, and it needs a reason, because everything else here is
/// bounded by the session instead (`gatt/auth.rs`: "the session itself is the rate limit"). That
/// argument holds for a token an attacker has to read off an LCD. It does not hold here: the code
/// space is 31^6, an authenticated peer could walk it, and each attempt costs the Hub a real
/// pairing exchange with its own portal. Five per minute leaves an operator room to fat-finger and
/// retype, and leaves a walker nothing.
const MAX_ATTEMPTS: u32 = 5;
/// Seconds of *elapsed* time, measured on the monotonic source rather than the wall clock: FB0B
/// lets the same authenticated peer set the wall clock, and a limit its target can reset is not one
/// (`GattCtx::pairing_attempts`).
const WINDOW_SECS: u64 = 60;

pub(crate) const fn max_attempts() -> u32 {
    MAX_ATTEMPTS
}
pub(crate) const fn window_secs() -> u64 {
    WINDOW_SECS
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingCodeRequest {
    pub code: String,
}

/// Uppercased and trimmed, then checked against the alphabet.
///
/// Trimming for the same reason FB01 trims: a BLE client that appends a newline is not making a
/// mistake worth refusing. Uppercasing because an operator typing into a phone keyboard gets
/// lowercase for free and the alphabet has no lowercase to confuse it with.
fn normalize(code: &str) -> Option<String> {
    let code = code.trim().to_ascii_uppercase();
    if code.len() != CODE_LENGTH {
        return None;
    }
    if !code.bytes().all(|b| CHARSET.contains(&b)) {
        return None;
    }
    Some(code)
}

pub fn write(ctx: &GattCtx, peer: PeerAddr, value: &[u8]) -> Result<(), ReqError> {
    ctx.gate(peer)?;
    let req: PairingCodeRequest = super::parse_write(value, super::MAX_WRITE_BYTES)?;

    let result = if !ctx.pairing_attempt_allowed() {
        WriteResult::fail(format!(
            "too many pairing-code attempts; wait {WINDOW_SECS}s and try again"
        ))
    } else {
        match normalize(&req.code) {
            None => WriteResult::fail(
                "not a pairing code: expected six characters from the code shown on this Hub's \
                 display, which never contains O, 0, I, 1 or L"
                    .to_string(),
            ),
            Some(code) => {
                let outcome = ctx.host.pairing.submit_code(Origin::Ble, &code);
                if outcome.success {
                    WriteResult::ok(outcome.message)
                } else {
                    WriteResult::fail(outcome.message)
                }
            }
        }
    };

    ctx.pairing_results.store(peer, result);
    Ok(())
}

pub fn read(ctx: &GattCtx, peer: PeerAddr, mtu: u16, offset: u16) -> Result<Vec<u8>, ReqError> {
    ctx.gate(peer)?;
    let r = ctx.pairing_results.read(&peer).unwrap_or_else(|| {
        WriteResult::fail("no pairing code has been submitted on this connection")
    });
    super::serve_json(&r.to_bytes(), mtu, offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_alphabet_is_the_ambiguity_stripped_one() {
        // The whole reason the set is what it is: an operator reads these off an LCD.
        for bad in ['O', '0', 'I', '1', 'L'] {
            assert!(
                !CHARSET.contains(&(bad as u8)),
                "{bad} is one of the five that were removed"
            );
        }
        assert_eq!(CHARSET.len(), 31);
    }

    #[test]
    fn normalize_accepts_what_an_operator_types_and_refuses_the_rest() {
        assert_eq!(normalize("23456789").as_deref(), None, "too long");
        assert_eq!(normalize("2345").as_deref(), None, "too short");
        assert_eq!(normalize(" 23456X \n").as_deref(), Some("23456X"));
        assert_eq!(normalize("23456x").as_deref(), Some("23456X"), "lowercase");
        assert_eq!(normalize("2345O6").as_deref(), None, "O is not in the set");
        assert_eq!(normalize("23451X").as_deref(), None, "nor is 1");
    }
}
