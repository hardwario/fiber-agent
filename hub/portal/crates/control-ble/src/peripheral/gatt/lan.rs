//! FB09 / FB0C — LAN config and status.
//! `plan/control/ble/peripheral.md` §6.4.

use std::sync::atomic::Ordering;

use bluer::gatt::local::ReqError;

use super::GattCtx;
use crate::host::{LanConfigRequest, LanMode};
use crate::peripheral::buffers::PeerAddr;
use crate::peripheral::events::PeripheralEvent;

/// At most three resolvers: more is a config the operator did not mean to write.
pub const MAX_DNS: usize = 3;

/// FB09 write. A second write while one is in flight is `InProgress` — reconfiguring an interface
/// twice concurrently is how a Hub loses its own network.
pub fn config_write(ctx: &GattCtx, peer: PeerAddr, value: &[u8]) -> Result<(), ReqError> {
    ctx.gate(peer)?;
    let req: LanConfigRequest = super::parse_write(value, super::MAX_WRITE_BYTES)?;

    if req.mode == LanMode::Static {
        let Some(ipv4) = req.ipv4.as_ref() else {
            // Static with no address would apply an empty configuration.
            return Err(ReqError::InvalidValueLength);
        };
        if ipv4.dns.len() > MAX_DNS {
            return Err(ReqError::InvalidValueLength);
        }
    }

    if ctx
        .lan_in_flight
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err(ReqError::InProgress);
    }
    let outcome = ctx.host.network.lan_apply(&req);
    ctx.lan_in_flight.store(false, Ordering::SeqCst);
    let mode = match req.mode {
        LanMode::Dhcp => "dhcp",
        LanMode::Static => "static",
    };
    match &outcome {
        Ok(()) => ctx.events.emit(PeripheralEvent::LanConfigured {
            mode: mode.to_string(),
            ip: ctx.host.network.lan_status().ip_address,
        }),
        Err(category) => ctx.events.emit(PeripheralEvent::LanFailed {
            mode: mode.to_string(),
            error: format!("{category:?}"),
        }),
    }
    outcome.map_err(|_| ReqError::Failed)
}

/// FB0C read.
pub fn status_read(
    ctx: &GattCtx,
    peer: PeerAddr,
    mtu: u16,
    offset: u16,
) -> Result<Vec<u8>, ReqError> {
    ctx.gate(peer)?;
    let body = serde_json::to_vec(&ctx.host.network.lan_status()).map_err(|_| ReqError::Failed)?;
    super::serve_json(&body, mtu, offset)
}
