//! FB0B — one-shot clock bootstrap.
//! `plan/control/ble/peripheral.md` §2.
//!
//! An RTC-less Hub that boots to 1970 timestamps every reading wrongly, so a phone standing in front
//! of it is the fastest way to fix the clock. The bounds below are what stop a typo from setting it
//! somewhere that would make the whole archive nonsense.

use bluer::gatt::local::ReqError;

use super::GattCtx;
use crate::host::{EPOCH_2020, EPOCH_2100};
use crate::peripheral::buffers::PeerAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeSetRequest {
    pub epoch: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TimePayload {
    pub epoch: i64,
    pub synchronized: bool,
}

pub fn write(ctx: &GattCtx, peer: PeerAddr, value: &[u8]) -> Result<(), ReqError> {
    ctx.gate(peer)?;
    let req: TimeSetRequest = super::parse_write(value, super::MAX_WRITE_BYTES)?;
    if !(EPOCH_2020..=EPOCH_2100).contains(&req.epoch) {
        return Err(ReqError::InvalidValueLength);
    }
    ctx.host
        .clock
        .set_time(req.epoch)
        .map_err(|_| ReqError::Failed)?;

    // Re-base the session's idle timer onto the new clock.
    //
    // Without this, FB0B kills the very session that used it: the idle timer is measured against
    // wall-clock time, so an RTC-less Hub that booted to 1970 and is being bootstrapped to the
    // present jumps *decades* forward in one write — vastly more than the 300 s idle timeout — and
    // the phone is de-authenticated mid-flow by its own successful operation. A clock step is not
    // idleness, so it must not be counted as any.
    //
    // Not in the design docs; raised for peripheral.md §3 rather than left as a surprise.
    ctx.session.rebase_after_clock_step(ctx.host.clock.as_ref());
    Ok(())
}

pub fn read(ctx: &GattCtx, peer: PeerAddr, mtu: u16, offset: u16) -> Result<Vec<u8>, ReqError> {
    ctx.gate(peer)?;
    let body = serde_json::to_vec(&TimePayload {
        epoch: ctx.host.clock.now_unix(),
        synchronized: ctx.host.clock.synchronized(),
    })
    .map_err(|_| ReqError::Failed)?;
    super::serve_json(&body, mtu, offset)
}
