//! FB10 — Beacon remove.
//! `plan/control/ble/peripheral.md` §4.
//!
//! > The acceptance criterion is stated at the **store layer**, not the API layer: FB10's effects and
//! > the signed `beacon-remove` command's effects must be identical, asserted by test. Two surfaces
//! > that agree today because someone wrote them carefully is not the same as two surfaces that
//! > cannot disagree.
//!
//! Hence: no logic here beyond parsing. The removal is `enroll::remove`, the same call the CLI and
//! the signed command make.

use bluer::gatt::local::ReqError;

use super::{GattCtx, WriteResult};
use crate::central::enroll;
use crate::host::Origin;
use crate::peripheral::buffers::PeerAddr;

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeaconRemoveRequest {
    pub mac: String,
}

pub fn write(ctx: &GattCtx, peer: PeerAddr, value: &[u8]) -> Result<(), ReqError> {
    ctx.gate(peer)?;
    let req: BeaconRemoveRequest = super::parse_write(value, super::MAX_WRITE_BYTES)?;
    let result = match enroll::remove(&ctx.host, Origin::Ble, &req.mac) {
        Ok(out) if out.removed => {
            // Drop it from the tracked set too, so it reappears as `discovered: true` on its next
            // frame rather than lingering as an enrolled beacon (UI-13).
            if let Ok(mut central) = ctx.central.write() {
                central.remove_enrollment(&out.mac);
            }
            WriteResult::ok(format!("{} removed", out.mac))
        }
        // An unknown MAC is `success: false` with a message, **not** an error — the FB10 contract.
        Ok(out) => WriteResult::fail(format!("{} is not enrolled", out.mac)),
        Err(e) => WriteResult::fail(e.message().to_string()),
    };
    ctx.remove_results.store(peer, result);
    Ok(())
}

pub fn read(ctx: &GattCtx, peer: PeerAddr, mtu: u16, offset: u16) -> Result<Vec<u8>, ReqError> {
    ctx.gate(peer)?;
    let r = ctx
        .remove_results
        .read(&peer)
        .unwrap_or_else(|| WriteResult::fail("no removal has been requested on this connection"));
    super::serve_json(&r.to_bytes(), mtu, offset)
}
