//! FB0D — Node add. The radio crate's operation; the host forwards it.
//! `plan/control/ble/peripheral.md` §2, `plan/control/ble/plan.md` §9.7.

use bluer::gatt::local::ReqError;

use super::GattCtx;
use crate::host::NodeAddRequest;
use crate::peripheral::buffers::PeerAddr;

pub fn write(ctx: &GattCtx, peer: PeerAddr, value: &[u8]) -> Result<(), ReqError> {
    ctx.gate(peer)?;
    let req: NodeAddRequest = super::parse_write(value, super::MAX_NODE_ADD_BYTES)?;
    // The add is asynchronous in the agent, so `pending` is preserved rather than flattened: the
    // phone reads back an interim answer and then the final one.
    let response = ctx.host.nodes.add(&req);
    ctx.node_add_results.store(peer, response);
    Ok(())
}

pub fn read(ctx: &GattCtx, peer: PeerAddr, mtu: u16, offset: u16) -> Result<Vec<u8>, ReqError> {
    ctx.gate(peer)?;
    // Before any write, the documented default — not an error, because a phone may poll.
    let r = ctx.node_add_results.read(&peer).unwrap_or_default();
    let body = serde_json::to_vec(&r).map_err(|_| ReqError::Failed)?;
    super::serve_json(&body, mtu, offset)
}
