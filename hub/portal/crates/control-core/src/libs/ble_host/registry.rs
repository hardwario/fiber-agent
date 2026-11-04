//! The process-wide handle on the BLE runtime, for code that cannot be handed one.
//!
//! `execute_config_command` is a free function with fifteen parameters, reached from three places.
//! Threading a sixteenth through it to carry the `Host` would touch every call site for no gain, so
//! the seams live in a `OnceLock` that `main.rs` fills once, immediately after `tasks::spawn`.
//!
//! This is the same idiom the code it replaces used — v1's `beacon_state_handle()` /
//! `beacon_config_handle()` were process-wide slots for exactly this reason — with two differences
//! that matter: there is **one** slot rather than four, and it is write-once, so nothing can swap
//! the runtime under a command that is already executing.
//!
//! **An empty slot is a refusal, not a panic.** A Hub in `ble.mode: off`, or one whose controller
//! is missing, never fills it; a signed Beacon command arriving there is answered with a reason.

use std::sync::OnceLock;

use control_ble::host::Host;
use control_ble::tasks::{CentralHandle, PeripheralHandle};

/// What the signed-command executor needs to serve the §8 verbs.
pub struct BleSeams {
    pub host: Host,
    /// Absent when the mode excludes the central role — `peripheral` or `off`.
    pub central: Option<CentralHandle>,
    /// Absent when the mode excludes the peripheral role — `central` or `off`.
    pub peripheral: Option<PeripheralHandle>,
}

static SEAMS: OnceLock<BleSeams> = OnceLock::new();

/// Install the seams. Returns false if they were already installed.
pub fn install(seams: BleSeams) -> bool {
    SEAMS.set(seams).is_ok()
}

/// The seams, or `None` before start-up has reached the BLE thread.
pub fn seams() -> Option<&'static BleSeams> {
    SEAMS.get()
}

/// The host, or the message a signed command should fail with.
pub fn host() -> Result<&'static Host, String> {
    SEAMS
        .get()
        .map(|s| &s.host)
        .ok_or_else(|| "the Bluetooth subsystem is not running on this Hub".to_string())
}

/// The central handle, or the message a signed command should fail with.
///
/// The two failures are worth distinguishing: "not running" is a Hub that has not finished booting
/// or has `ble.mode: off`, while "no central role" is a deliberate configuration, and an operator
/// chasing a Beacon that never appears needs to know which one they are looking at.
pub fn central() -> Result<&'static CentralHandle, String> {
    match SEAMS.get() {
        None => Err("the Bluetooth subsystem is not running on this Hub".to_string()),
        Some(s) => s.central.as_ref().ok_or_else(|| {
            "this Hub's ble.mode excludes the central role, so it does not talk to Beacons"
                .to_string()
        }),
    }
}

/// The peripheral handle, or the message the front panel should log.
///
/// The button monitor calls this on an ENTER hold. A Hub in `ble.mode: central` has no FB00 server,
/// so there is nothing to advertise and the QR screen would be a lie — saying so beats showing a
/// code no phone can use.
pub fn peripheral() -> Result<&'static PeripheralHandle, String> {
    match SEAMS.get() {
        None => Err("the Bluetooth subsystem is not running on this Hub".to_string()),
        Some(s) => s.peripheral.as_ref().ok_or_else(|| {
            "this Hub's ble.mode excludes the peripheral role, so it does not serve phones"
                .to_string()
        }),
    }
}

/// Mint a provisioning session and start advertising — the ENTER-hold flow.
///
/// Returns the QR payload the LCD renders. **The token is inside it and nowhere else**: this is the
/// only route by which a real token leaves the crate (`peripheral.md` §3), and it goes straight to
/// a screen an operator is standing in front of.
pub fn begin_phone_session(
    idle_timeout_s: u64,
    hub_mac: &control_ble::mac::MacAddr,
    hostname: &str,
) -> Result<control_ble::peripheral::session::QrPayload, String> {
    let seams = SEAMS
        .get()
        .ok_or_else(|| "the Bluetooth subsystem is not running on this Hub".to_string())?;
    let p = peripheral()?;
    // Start the session before advertising: a phone that connects to an advertisement with no live
    // session would be refused at FB01 with no way to know why.
    let preview = p
        .session
        .start(seams.host.clock.as_ref(), idle_timeout_s, hub_mac, hostname);
    let _ = preview;
    p.start_advertising();
    p.session
        .qr_payload(hub_mac, hostname)
        .ok_or_else(|| "the session expired before it could be shown".to_string())
}

/// End the session, then stop advertising.
///
/// **In that order.** Invalidating the token first means a pairing attempt that races the teardown
/// fails closed; stopping the advertisement first would leave a live credential on a Hub the
/// operator believes they have shut.
pub fn end_phone_session() -> Result<(), String> {
    let p = peripheral()?;
    p.session.stop();
    p.stop_advertising();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unfilled_registry_refuses_with_a_reason_rather_than_panicking() {
        // The whole point: a signed command that arrives before (or without) a BLE runtime must
        // produce an operator-readable failure, not take the MQTT thread down with it.
        //
        // Note this asserts the *uninstalled* path; `install` is write-once and process-wide, so a
        // test that filled it would change every other test in this binary.
        if seams().is_none() {
            let e = host().expect_err("refused");
            assert!(e.contains("not running"), "{e}");
            let e = central().expect_err("refused");
            assert!(e.contains("not running"), "{e}");
        }
    }
}
