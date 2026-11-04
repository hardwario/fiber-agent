//! FB18 — the MQTT export destination write.
//! `plan/control/ble/peripheral.md` §6.4, GitLab #59.
//!
//! One of the two asks that were blocked outright without BLE: configuring an export needs the
//! phone on the Hub's LAN, and it usually is not. The app's client-role wizard already collects the
//! address and credentials and then has nowhere to send them — its own step says so.
//!
//! Routes through the same seam as the signed `export-destination-set` command, so there is no
//! second way to write `mqtt.export` (`plan/control/ble/plan.md` §9.6).

use bluer::gatt::local::ReqError;

use super::{GattCtx, WriteResult};
use crate::host::{ExportDestinationRequest, Origin};
use crate::peripheral::buffers::PeerAddr;

pub fn write(ctx: &GattCtx, peer: PeerAddr, value: &[u8]) -> Result<(), ReqError> {
    ctx.gate(peer)?;
    let req: ExportDestinationRequest = super::parse_write(value, super::MAX_WRITE_BYTES)?;
    let outcome = ctx.host.export.set(Origin::Ble, &req);
    ctx.export_results.store(
        peer,
        if outcome.success {
            WriteResult::ok(outcome.message)
        } else {
            WriteResult::fail(outcome.message)
        },
    );
    // The write succeeded even when the configuration was refused: the reason is the read's
    // business, and an ATT error here would discard it.
    Ok(())
}

pub fn read(ctx: &GattCtx, peer: PeerAddr, mtu: u16, offset: u16) -> Result<Vec<u8>, ReqError> {
    ctx.gate(peer)?;
    let r = ctx.export_results.read(&peer).unwrap_or_else(|| {
        WriteResult::fail("no export destination has been written on this connection")
    });
    super::serve_json(&r.to_bytes(), mtu, offset)
}
