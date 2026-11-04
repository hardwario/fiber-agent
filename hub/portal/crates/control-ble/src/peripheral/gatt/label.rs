//! FB0A — device label, read and write.
//! `plan/control/ble/peripheral.md` §2.

use bluer::gatt::local::ReqError;

use super::{GattCtx, WriteResult};
use crate::peripheral::buffers::PeerAddr;

/// The label is displayed in a list on a phone, so it is bounded like a Beacon name.
pub const MAX_LABEL_CHARS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LabelPayload {
    pub label: String,
}

pub fn read(ctx: &GattCtx, peer: PeerAddr, mtu: u16, offset: u16) -> Result<Vec<u8>, ReqError> {
    ctx.gate(peer)?;
    let body = serde_json::to_vec(&LabelPayload {
        label: ctx.host.device.label(),
    })
    .map_err(|_| ReqError::Failed)?;
    super::serve_json(&body, mtu, offset)
}

pub fn write(ctx: &GattCtx, peer: PeerAddr, value: &[u8]) -> Result<(), ReqError> {
    ctx.gate(peer)?;
    let req: LabelPayload = super::parse_write(value, super::MAX_WRITE_BYTES)?;
    let label = req.label.trim();
    if label.chars().count() > MAX_LABEL_CHARS || label.chars().any(char::is_control) {
        return Err(ReqError::InvalidValueLength);
    }
    match ctx.host.device.set_label(label) {
        Ok(()) => {
            ctx.label_results.store(peer, WriteResult::ok("label set"));
            Ok(())
        }
        Err(why) => {
            ctx.label_results.store(peer, WriteResult::fail(why));
            Err(ReqError::Failed)
        }
    }
}
