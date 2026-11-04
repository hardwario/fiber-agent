//! FB07 — device info. **Deliberately not session-gated.**
//! `plan/control/ble/peripheral.md` §3.
//!
//! An accepted dependency rather than an oversight: the phone uses it to *recognize* a Hub —
//! hostname and MAC — before it has any credential, which is what lets the app tell "a Hub I have
//! configured" from "a Hub I have not" at the top of its flow.
//!
//! What it must **not** carry is anything about the Hub's *state*; that is §6.3's ask and it is
//! gated. So this payload is deliberately four fixed identity fields and nothing else.

use bluer::gatt::local::ReqError;

use super::GattCtx;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeviceInfoPayload {
    pub hostname: String,
    pub version: String,
    pub uptime: String,
    pub mac: String,
}

pub fn payload(ctx: &GattCtx) -> DeviceInfoPayload {
    DeviceInfoPayload {
        hostname: ctx.host.device.hostname(),
        version: ctx.host.device.version(),
        uptime: ctx.host.device.uptime(),
        mac: ctx.mac.to_string(),
    }
}

pub fn read(ctx: &GattCtx, mtu: u16, offset: u16) -> Result<Vec<u8>, ReqError> {
    let body = serde_json::to_vec(&payload(ctx)).map_err(|_| ReqError::Failed)?;
    super::serve_json(&body, mtu, offset)
}
