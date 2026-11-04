//! FB0E — Beacon add.
//! `plan/control/ble/peripheral.md` §4, `plan/control/ble/plan.md` §9.3, §9.6.
//!
//! Routes through **the same enrollment seam** as the signed `beacon-add` command and as
//! `proximosctl control.ble beacon-add`. Three surfaces, one code path, no fourth mutation dialect.

use bluer::gatt::local::ReqError;

use super::{GattCtx, WriteResult};
use crate::central::enroll;
use crate::host::Origin;
use crate::peripheral::buffers::PeerAddr;

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeaconAddRequest {
    pub mac: String,
    #[serde(default)]
    pub name: Option<String>,
}

pub fn write(ctx: &GattCtx, peer: PeerAddr, value: &[u8]) -> Result<(), ReqError> {
    ctx.gate(peer)?;
    let req: BeaconAddRequest = super::parse_write(value, super::MAX_WRITE_BYTES)?;
    let result = match enroll::add(&ctx.host, Origin::Ble, &req.mac, req.name.as_deref()) {
        // §6.2's ask has two halves — turn monitoring on, *or* tell the app clearly that it is off —
        // and reporting the outcome verbatim satisfies both.
        Ok(out) => {
            // The same follow-up dial the signed and the CLI enrollment queue (GitLab #84). A full
            // queue only delays it; the enrollment itself already stands.
            if let Some(dial) = &ctx.dial {
                let _ = dial.try_send(crate::tasks::DialJob {
                    mac: out.mac,
                    kind: out.follow_up.dial_kind(),
                });
            }
            WriteResult::ok(format!("enrolled; monitoring {}", out.monitoring.as_str()))
        }
        Err(e) => WriteResult::fail(e.message().to_string()),
    };
    ctx.beacon_add_results.store(peer, result);
    // The *write* succeeded even when the enrollment did not: the outcome is the read's business,
    // and an ATT error here would lose the reason.
    Ok(())
}

pub fn read(ctx: &GattCtx, peer: PeerAddr, mtu: u16, offset: u16) -> Result<Vec<u8>, ReqError> {
    ctx.gate(peer)?;
    let r = ctx.beacon_add_results.read(&peer).unwrap_or_else(|| {
        WriteResult::fail("no enrollment has been requested on this connection")
    });
    super::serve_json(&r.to_bytes(), mtu, offset)
}
