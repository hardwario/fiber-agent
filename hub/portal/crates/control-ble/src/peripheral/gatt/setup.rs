//! FB16 — provisioned/role read. FB17 — setup-complete write.
//! `plan/control/ble/peripheral.md` §6.3, `plan/decisions.md` #4. GitLab #58.
//!
//! The gap these close is not identity: FB07 already gives hostname and MAC without a credential,
//! which is enough to *recognize* a Hub. What was missing is its **state** — so a second phone, or
//! a reinstall, could not tell a Hub an operator had already set up from one out of the box, and
//! walked the whole wizard again against a configured device.
//!
//! Both are gated. FB07 stays un-gated precisely because it carries no state; this does, so it does
//! not join it.

use bluer::gatt::local::ReqError;

use super::{GattCtx, WriteResult};
use crate::host::{HubRole, Origin};
use crate::peripheral::buffers::PeerAddr;

/// FB17's payload.
///
/// The role rides along with the completion rather than being inferred, for the reason decision #4
/// gives: firmware cannot tell "someone wrote a label" from "an operator completed setup", and the
/// app is the party that owns the wizard-finish moment. `deny_unknown_fields` so a phone sending a
/// shape this Hub does not implement is told, rather than silently marking setup complete with a
/// role nobody read.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupCompleteRequest {
    pub role: HubRole,
}

/// FB16 — `{provisioned, role}`.
///
/// A Hub that **cannot** answer returns an ATT-level error, never `provisioned: false`. The two are
/// different claims — one is about the Hub, the other about whether anyone could look — and a phone
/// that cannot tell them apart is back to walking the wizard against a configured Hub, which is the
/// whole defect. This mirrors FB11's "an empty page is a claim about the site, a failure is a claim
/// about the Hub".
pub fn read(ctx: &GattCtx, peer: PeerAddr, mtu: u16, offset: u16) -> Result<Vec<u8>, ReqError> {
    ctx.gate(peer)?;
    let status = ctx.host.setup.read().map_err(|_| ReqError::Failed)?;
    let body = serde_json::to_vec(&status).map_err(|_| ReqError::Failed)?;
    super::serve_json(&body, mtu, offset)
}

/// FB17 — mark setup complete.
pub fn write(ctx: &GattCtx, peer: PeerAddr, value: &[u8]) -> Result<(), ReqError> {
    ctx.gate(peer)?;
    let req: SetupCompleteRequest = super::parse_write(value, super::MAX_WRITE_BYTES)?;
    let outcome = ctx.host.setup.mark_complete(Origin::Ble, req.role);
    ctx.setup_results.store(
        peer,
        if outcome.success {
            WriteResult::ok(outcome.message)
        } else {
            WriteResult::fail(outcome.message)
        },
    );
    // The *write* succeeded even when the mutation did not, as on FB0E/FB10: an ATT error here
    // would throw away the reason the phone needs to show.
    Ok(())
}

/// FB17's result.
pub fn read_result(
    ctx: &GattCtx,
    peer: PeerAddr,
    mtu: u16,
    offset: u16,
) -> Result<Vec<u8>, ReqError> {
    ctx.gate(peer)?;
    let r = ctx.setup_results.read(&peer).unwrap_or_else(|| {
        WriteResult::fail("setup completion has not been requested on this connection")
    });
    super::serve_json(&r.to_bytes(), mtu, offset)
}
