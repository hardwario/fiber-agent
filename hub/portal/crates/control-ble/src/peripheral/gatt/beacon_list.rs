//! FB0F — Beacon list, paged.
//! `plan/control/ble/peripheral.md` §4, §5.1.
//!
//! Write `{"page": N}`, read a page. The page is built **at write time** against this central's own
//! MTU and buffered per peer, so a read is idempotent and two phones cannot consume each other's
//! cursor.

use bluer::gatt::local::ReqError;

use super::GattCtx;
use crate::peripheral::buffers::PeerAddr;
use crate::peripheral::paging::{
    build_page, parse_request, BeaconList, ListRead, PageError, ERR_MTU_TOO_SMALL, ERR_NO_REQUEST,
};

pub fn write(ctx: &GattCtx, peer: PeerAddr, mtu: u16, value: &[u8]) -> Result<(), ReqError> {
    ctx.gate(peer)?;
    if value.len() > super::MAX_WRITE_BYTES {
        return Err(ReqError::InvalidValueLength);
    }
    let read = match parse_request(value) {
        Ok(req) => {
            let section = ctx.host.config.load().map_err(|_| ReqError::Failed)?;
            let list = BeaconList::from_tags(&section.tags);
            build_page(&list, req.page, mtu, ctx.cfg.max_page_bytes)
        }
        Err(e) => ListRead::Error(e),
    };
    ctx.list_buffers.store(peer, read.to_bytes());
    Ok(())
}

pub fn read(ctx: &GattCtx, peer: PeerAddr, mtu: u16, offset: u16) -> Result<Vec<u8>, ReqError> {
    ctx.gate(peer)?;
    let buffered = ctx.list_buffers.read(&peer);
    let body = match buffered {
        Some(b) => b,
        // Rule 2: a read with no prior write is a defined error, never a default first page.
        None => PageError::new(ERR_NO_REQUEST, "write {\"page\": N} before reading").to_bytes(),
    };

    // §5.1: if even the refusal cannot fit, answer at the ATT level rather than emitting the
    // truncated fragment the guard exists to prevent. `serve_json` decides that uniformly, and this
    // is also what turns a buffered `mtu_too_small` into a legible read failure.
    super::serve_json(&body, mtu, offset)
}

/// Whether a buffered body is itself an MTU refusal — used by the tests to assert §5.1's intent
/// without re-parsing JSON at the call site.
pub fn is_mtu_refusal(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .is_some_and(|e| e == ERR_MTU_TOO_SMALL)
}
