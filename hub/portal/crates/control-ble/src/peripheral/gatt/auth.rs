//! FB01 — the session-token write that gates everything except FB07.
//! `plan/control/ble/peripheral.md` §3.

use bluer::gatt::local::ReqError;

use super::{GattCtx, WriteResult};
use crate::peripheral::buffers::PeerAddr;
use crate::peripheral::events::PeripheralEvent;

/// Write the token. Success authenticates **this** peer; a failure clears it.
///
/// Deliberately not gated (it is the gate), and deliberately not rate-limited here: the session
/// itself is the rate limit — it lives for one idle timeout and dies with the operator's presence.
pub fn write(ctx: &GattCtx, peer: PeerAddr, value: &[u8]) -> Result<(), ReqError> {
    if value.len() > super::MAX_WRITE_BYTES {
        return Err(ReqError::InvalidValueLength);
    }
    // A non-UTF-8 body is simply a wrong token, not a protocol error worth a distinct code.
    let attempt = String::from_utf8_lossy(value);
    if ctx.session.verify(ctx.host.clock.as_ref(), &attempt) {
        ctx.set_authenticated(peer);
        ctx.events.emit(PeripheralEvent::AuthSucceeded {
            addr: crate::mac::MacAddr::from(peer).to_string(),
        });
        Ok(())
    } else {
        ctx.events.emit(PeripheralEvent::AuthFailed {
            addr: crate::mac::MacAddr::from(peer).to_string(),
        });
        // Only this peer's authentication is cleared: a wrong guess from a second phone must not
        // knock the first one out mid-flow.
        if ctx.is_authenticated_for(peer) {
            ctx.clear_authentication();
        }
        Err(ReqError::NotAuthorized)
    }
}

/// Read whether **this** peer is authenticated.
pub fn read(ctx: &GattCtx, peer: PeerAddr, mtu: u16, offset: u16) -> Result<Vec<u8>, ReqError> {
    let r = if ctx.is_authenticated_for(peer) && ctx.session.is_live(ctx.host.clock.as_ref()) {
        WriteResult::ok("Authenticated")
    } else {
        WriteResult::fail("Not authenticated")
    };
    super::serve_json(&r.to_bytes(), mtu, offset)
}
