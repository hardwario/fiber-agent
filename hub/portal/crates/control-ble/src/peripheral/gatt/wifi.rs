//! FB02 / FB03 / FB04 / FB08 — Wi-Fi scan, connect, status, disconnect.
//! `plan/control/ble/peripheral.md` §6.4, `plan/control/ble/plan.md` §9.7.
//!
//! The `nmcli` exec surface stays in the **host** (CP-1): this component never spawns a process, it
//! calls `NetworkOps`. That is the whole reason those operations are a trait.

use bluer::gatt::local::ReqError;

use super::GattCtx;
use crate::peripheral::buffers::PeerAddr;
use crate::peripheral::events::PeripheralEvent;

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WifiConnectRequest {
    pub ssid: String,
    pub password: String,
}

/// FB02 read — the scan result.
pub fn scan_read(
    ctx: &GattCtx,
    peer: PeerAddr,
    mtu: u16,
    offset: u16,
) -> Result<Vec<u8>, ReqError> {
    ctx.gate(peer)?;
    let networks = ctx.host.network.wifi_scan().map_err(|_| ReqError::Failed)?;
    let body = serde_json::to_vec(&networks).map_err(|_| ReqError::Failed)?;
    super::serve_json(&body, mtu, offset)
}

/// FB03 write — connect. A failure is an ATT error, so the phone's own stack surfaces it.
pub fn connect_write(ctx: &GattCtx, peer: PeerAddr, value: &[u8]) -> Result<(), ReqError> {
    ctx.gate(peer)?;
    let req: WifiConnectRequest = super::parse_write(value, super::MAX_WRITE_BYTES)?;
    let status = ctx.host.network.wifi_connect(&req.ssid, &req.password);
    if status.connected {
        ctx.events.emit(PeripheralEvent::WifiConnected {
            ssid: req.ssid.clone(),
            ip: status.ip_address.clone(),
        });
        Ok(())
    } else {
        // `status.error` is the host's own operator-facing message; the password never travels.
        ctx.events.emit(PeripheralEvent::WifiFailed {
            ssid: req.ssid.clone(),
            error: status.error.clone(),
        });
        Err(ReqError::Failed)
    }
}

/// FB04 read — status.
pub fn status_read(
    ctx: &GattCtx,
    peer: PeerAddr,
    mtu: u16,
    offset: u16,
) -> Result<Vec<u8>, ReqError> {
    ctx.gate(peer)?;
    let body = serde_json::to_vec(&ctx.host.network.wifi_status()).map_err(|_| ReqError::Failed)?;
    super::serve_json(&body, mtu, offset)
}

/// FB08 write — disconnect. The value is ignored; the write itself is the request.
pub fn disconnect_write(ctx: &GattCtx, peer: PeerAddr, _value: &[u8]) -> Result<(), ReqError> {
    ctx.gate(peer)?;
    let status = ctx.host.network.wifi_disconnect();
    if status.connected {
        // Still connected after a disconnect request is a failure, not a success with a caveat.
        Err(ReqError::Failed)
    } else {
        Ok(())
    }
}
