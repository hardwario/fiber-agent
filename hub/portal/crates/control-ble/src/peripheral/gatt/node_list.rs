//! FB11 — Node list, paged.
//! `plan/control/ble/peripheral.md` §2, §4, §6.1.
//!
//! The Node half of the inventory ask, and deliberately the *same* module as FB0F with the item type
//! swapped: §4 declared the paging contract reusable, and reusing it is what that declaration is
//! worth. Write `{"page": N}`, read a page. The page is built **at write time** against this
//! central's own MTU and buffered per peer, so a read is idempotent and two phones cannot consume
//! each other's cursor.

use bluer::gatt::local::ReqError;

use super::GattCtx;
use crate::peripheral::buffers::PeerAddr;
use crate::peripheral::paging::{
    build_page, parse_request, ListRead, NodeItem, NodeList, PageError, ERR_NO_REQUEST,
};

pub fn write(ctx: &GattCtx, peer: PeerAddr, mtu: u16, value: &[u8]) -> Result<(), ReqError> {
    ctx.gate(peer)?;
    if value.len() > super::MAX_WRITE_BYTES {
        return Err(ReqError::InvalidValueLength);
    }
    let read = match parse_request(value) {
        Ok(req) => {
            // A host that cannot answer is an ATT-level error, **not** an empty page. A phone shown
            // `total: 0` would tell the installer this Hub holds no Nodes, which is a different
            // claim from "this Hub could not look" — and the second one is what a failure here
            // means.
            let entries = ctx.host.nodes.list().map_err(|_| ReqError::Failed)?;
            let list = NodeList::from_items(
                entries
                    .into_iter()
                    .map(|e| NodeItem {
                        deveui: e.deveui,
                        serial: e.serial,
                        name: e.name,
                    })
                    .collect(),
            );
            build_page(&list, req.page, mtu, ctx.cfg.max_page_bytes)
        }
        // A malformed request is a buffered payload, not an ATT error: the phone gets to read back
        // *which* key it got wrong.
        Err(e) => ListRead::Error(e),
    };
    ctx.node_list_buffers.store(peer, read.to_bytes());
    Ok(())
}

pub fn read(ctx: &GattCtx, peer: PeerAddr, mtu: u16, offset: u16) -> Result<Vec<u8>, ReqError> {
    ctx.gate(peer)?;
    let body = match ctx.node_list_buffers.read(&peer) {
        Some(b) => b,
        // Rule 2: a read with no prior write is a defined error, never a default first page.
        None => PageError::new(ERR_NO_REQUEST, "write {\"page\": N} before reading").to_bytes(),
    };

    // §5.1: if even the refusal cannot fit, answer at the ATT level rather than emitting the
    // truncated fragment the guard exists to prevent.
    super::serve_json(&body, mtu, offset)
}
