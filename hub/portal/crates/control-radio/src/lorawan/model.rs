//! The domain types the ChirpStack client speaks — deliberately not `chirpstack_api` types.
//!
//! The trait in [`crate::lorawan::client`] is defined over these so that converge logic is
//! testable against an in-memory fake without a server, and so the one place that knows
//! ChirpStack's field names is the tonic implementation. That matters most for key placement
//! (§9.2), where the field names do not match the values they carry and the mapping is
//! exactly the thing a rewrite gets silently wrong.

use crate::lorawan::eui::Eui;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tenant {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Application {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceProfile {
    pub id: String,
    pub name: String,
}

/// What `create` asks ChirpStack to build for one named device profile.
///
/// There is deliberately no payload-codec field: `create` always sends `NONE`, regardless of
/// anything in the config (decision #8 §5, plan.md §7). Leaving the field out of this struct
/// is what makes that unexpressible rather than merely unwritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceProfileSpec {
    pub tenant_id: String,
    pub name: String,
    pub description: String,
    pub region: String,
    pub mac_version: String,
    pub reg_params_revision: String,
    pub adr_algorithm_id: String,
    pub flush_queue_on_activate: bool,
    pub uplink_interval: u32,
    pub device_status_req_interval: u32,
    pub supports_otaa: bool,
    /// ABP only; ignored when `supports_otaa`.
    pub abp: Option<AbpProfileParams>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbpProfileParams {
    pub rx1_delay: u32,
    pub rx1_dr_offset: u32,
    pub rx2_dr: u32,
    pub rx2_freq: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewaySpec {
    pub gateway_id: Eui,
    pub tenant_id: String,
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gateway {
    pub gateway_id: Eui,
    pub name: String,
    pub tenant_id: String,
    /// RFC3339. `None` means never seen, which reads as offline.
    pub last_seen_at: Option<String>,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSpec {
    pub dev_eui: Eui,
    pub application_id: String,
    pub device_profile_id: String,
    pub name: String,
    pub description: String,
    pub join_eui: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub dev_eui: Eui,
    pub name: String,
    pub device_profile_id: String,
    pub last_seen_at: Option<String>,
}

/// OTAA root key material, as the operator supplies it.
///
/// One key in, two fields out — see [`crate::lorawan::client`]'s key-placement note.
#[derive(Clone, PartialEq, Eq)]
pub struct OtaaKeys {
    pub app_key: [u8; 16],
}

/// ABP session key material.
#[derive(Clone, PartialEq, Eq)]
pub struct AbpKeys {
    pub dev_addr: [u8; 4],
    pub nwk_s_key: [u8; 16],
    pub app_s_key: [u8; 16],
}

// Key material never reaches a log line, a panic message or a Debug dump. Redacting in the
// Debug impl is cheaper to keep true than remembering not to print the struct.
impl std::fmt::Debug for OtaaKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OtaaKeys { app_key: <redacted> }")
    }
}

impl std::fmt::Debug for AbpKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "AbpKeys { dev_addr: <redacted>, nwk_s_key: <redacted>, app_s_key: <redacted> }",
        )
    }
}

/// An API key as ChirpStack's `InternalService` models it — three axes, no finer split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeySpec {
    pub name: String,
    pub is_admin: bool,
    pub tenant_id: Option<String>,
    pub is_read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyInfo {
    pub id: String,
    pub name: String,
}

/// A minted key: the id identifies the row, the token is the JWT.
#[derive(Clone, PartialEq, Eq)]
pub struct MintedApiKey {
    pub id: String,
    pub token: String,
}

impl std::fmt::Debug for MintedApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MintedApiKey")
            .field("id", &self.id)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// One item of a device's downlink queue — `DeviceService.GetQueue`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedDownlink {
    pub id: String,
    pub f_port: u8,
    pub data: Vec<u8>,
    pub confirmed: bool,
    /// Sent and awaiting the MAC-layer ack (confirmed downlinks only).
    pub is_pending: bool,
}
