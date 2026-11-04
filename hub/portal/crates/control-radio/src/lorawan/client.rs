//! The one ChirpStack client — `plan/control/radio/lorawan.md` §3.
//!
//! > **Nothing else in the monorepo may open a ChirpStack connection.**
//!
//! The trait is defined over the domain types in [`crate::lorawan::model`] rather than
//! `chirpstack_api` types, for two reasons: converge logic becomes testable against an
//! in-memory fake with no server, and exactly one file knows ChirpStack's field names — which
//! matters most for key placement, where the names do not match the values.
//!
//! The endpoint is `127.0.0.1:8080`, deliberately non-configurable: it carries v1's
//! deployment contract forward, and upstream's default bind is `0.0.0.0:8080`, so the
//! renderer template must pin `[api] bind` to loopback explicitly.

use async_trait::async_trait;
// tonic comes from chirpstack_api's own re-export rather than a direct dependency, so the
// Status type this maps is byte-for-byte the one the generated stubs return. A separate
// `tonic` line in Cargo.toml could resolve to a different minor and reintroduce exactly the
// version-skew class the single-client rule exists to prevent.
use chirpstack_api::tonic;

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::lorawan::eui::Eui;
use crate::lorawan::keys::{PlacedAbpSession, PlacedOtaaKeys};
use crate::lorawan::model::*;

/// The management API address. Not a config key — see the module note.
pub const API_ENDPOINT: &str = "http://127.0.0.1:8080";

/// The §3 call surface, as a trait.
///
/// `GetKeys`/`UpdateKeys` appear here although §3's table omits them: §7's ask-first upsert
/// requires both, so the table is incomplete rather than these being extra. Carried to the
/// plan-feedback set.
#[async_trait]
pub trait ChirpStack: Send + Sync {
    /// Resolved, never created — v1 takes the first entry, which is the default tenant a
    /// fresh ChirpStack v4 ships. `TenantService/Create` is deliberately absent from this
    /// trait, which is also why `wipe` cannot delete a tenant it did not create.
    async fn list_tenants(&self) -> Result<Vec<Tenant>>;

    async fn list_applications(&self, tenant_id: &str) -> Result<Vec<Application>>;
    async fn create_application(
        &self,
        tenant_id: &str,
        name: &str,
        description: &str,
    ) -> Result<String>;
    async fn delete_application(&self, id: &str) -> Result<()>;

    async fn list_device_profiles(&self, tenant_id: &str) -> Result<Vec<DeviceProfile>>;
    async fn create_device_profile(&self, spec: &DeviceProfileSpec) -> Result<String>;
    async fn delete_device_profile(&self, id: &str) -> Result<()>;

    async fn get_gateway(&self, gateway_id: &Eui) -> Result<Option<Gateway>>;
    async fn list_gateways(&self, tenant_id: &str) -> Result<Vec<Gateway>>;
    /// Ask-first existence check, scoped to the tenant's own gateway list.
    ///
    /// Deliberately not `get_gateway`: ChirpStack 4.19.0 applies an EXISTS-join in
    /// `validate_key`, so `GatewayService.Get` on a gateway that does not exist yet answers
    /// `Unauthenticated` for every key below admin — the exact call `create` makes on a first
    /// boot. `List` is scoped by tenant and answers under all three keys (measured
    /// 2026-08-25). Its `search` field does not match on EUI, so the scan is over the list.
    async fn find_gateway(&self, tenant_id: &str, gateway_id: &Eui) -> Result<Option<Gateway>>;
    async fn create_gateway(&self, spec: &GatewaySpec) -> Result<()>;
    async fn delete_gateway(&self, gateway_id: &Eui) -> Result<()>;

    async fn get_device(&self, dev_eui: &Eui) -> Result<Option<Device>>;
    async fn list_devices(&self, application_id: &str) -> Result<Vec<Device>>;
    async fn create_device(&self, spec: &DeviceSpec) -> Result<()>;
    async fn delete_device(&self, dev_eui: &Eui) -> Result<()>;

    async fn get_device_keys(&self, dev_eui: &Eui) -> Result<Option<PlacedOtaaKeys>>;
    async fn create_device_keys(&self, dev_eui: &Eui, keys: &PlacedOtaaKeys) -> Result<()>;
    async fn update_device_keys(&self, dev_eui: &Eui, keys: &PlacedOtaaKeys) -> Result<()>;
    async fn activate_device(&self, dev_eui: &Eui, session: &PlacedAbpSession) -> Result<()>;

    async fn enqueue_downlink(
        &self,
        dev_eui: &Eui,
        f_port: u8,
        data: Vec<u8>,
        confirmed: bool,
    ) -> Result<String>;

    /// The device's queued downlinks, oldest first — `DeviceService.GetQueue`.
    async fn get_queue(&self, dev_eui: &Eui) -> Result<Vec<QueuedDownlink>>;

    /// Drop every queued downlink of the device — `DeviceService.FlushQueue`.
    async fn flush_queue(&self, dev_eui: &Eui) -> Result<()>;

    /// Exchange the seeded admin password for a session JWT — `InternalService.Login`.
    ///
    /// §4 called this "Option A fallback only, never in steady state". It is neither: the
    /// three API-key RPCs below accept **only** a session JWT, so this is the sole way to mint
    /// the tenant and read-only keys, and `create` runs it once on a Hub that has not got them
    /// yet. The `email` field is literally `admin` for the seeded default user, not an address.
    /// Carried to the plan-feedback set (`lorawan.md` §3/§4).
    async fn login(&self, email: &str, password: &str) -> Result<String>;

    /// On `InternalService`, not an `ApiKeyService` — there is no such service.
    ///
    /// `session` is a JWT from [`ChirpStack::login`], **not** an API-key bearer token:
    /// ChirpStack v4 authenticates `InternalService` against its own session claims only, so an
    /// admin API key — however privileged — is rejected here. Measured against 4.19.0; passing
    /// the credential explicitly is what keeps that fact at the call site instead of hidden in
    /// a scope lookup that cannot work.
    async fn create_api_key(&self, session: &str, spec: &ApiKeySpec) -> Result<MintedApiKey>;
    async fn list_api_keys(
        &self,
        session: &str,
        tenant_id: Option<&str>,
    ) -> Result<Vec<ApiKeyInfo>>;
    async fn delete_api_key(&self, session: &str, id: &str) -> Result<()>;
}

/// Map a gRPC status onto the error taxonomy — §6's "source" column for the LoRaWAN arm.
///
/// The distinction that carries operational weight is `Unauthenticated` vs `NotBootstrapped`:
/// a rejected token (revoked row, rotated `api.secret`, wiped DB) is not the same as an
/// absent one, and they send an operator to different runbooks. Absence is decided before any
/// RPC, in the token store; this function only ever sees the rejected case.
pub fn map_status(status: &tonic::Status, doing: &str) -> Error {
    use tonic::Code;
    let kind = match status.code() {
        Code::Unauthenticated => Kind::Unauthenticated,
        Code::PermissionDenied => Kind::PermissionDenied,
        Code::NotFound => Kind::NotFound,
        Code::AlreadyExists => Kind::AlreadyExists,
        Code::InvalidArgument | Code::OutOfRange => Kind::InvalidArgument,
        Code::DeadlineExceeded => Kind::Timeout,
        Code::Unavailable => Kind::ServiceDown,
        _ => Kind::Transport,
    };
    Error::new(
        kind,
        Transport::Lorawan,
        Component::ChirpstackApi,
        format!("{doing}: {} ({})", status.message(), status.code()),
    )
}

/// Map a transport-level failure (no server listening, TLS, DNS) onto the taxonomy.
///
/// `ServiceDown` rather than `Transport`, because "connection refused" has one cause an
/// operator can act on — chirpstack is not listening — and the boot retry policy keys on the
/// 20-29 band to wait for it.
pub fn map_transport_error(e: &(dyn std::error::Error + 'static), doing: &str) -> Error {
    Error::new(
        Kind::ServiceDown,
        Transport::Lorawan,
        Component::ChirpstackService,
        format!("{doing}: cannot reach the ChirpStack API at {API_ENDPOINT}: {e}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grpc_status_codes_map_onto_the_taxonomy() {
        use tonic::{Code, Status};
        let cases = [
            (Code::Unauthenticated, Kind::Unauthenticated),
            (Code::PermissionDenied, Kind::PermissionDenied),
            (Code::NotFound, Kind::NotFound),
            (Code::AlreadyExists, Kind::AlreadyExists),
            (Code::InvalidArgument, Kind::InvalidArgument),
            (Code::DeadlineExceeded, Kind::Timeout),
            (Code::Unavailable, Kind::ServiceDown),
            // Everything else is residual, including the Internal that a duplicate Create
            // actually returns.
            (Code::Internal, Kind::Transport),
            (Code::Unknown, Kind::Transport),
            (Code::ResourceExhausted, Kind::Transport),
        ];
        for (code, expected) in cases {
            let err = map_status(&Status::new(code, "boom"), "listing tenants");
            assert_eq!(err.kind(), expected, "{code:?}");
            assert_eq!(err.component(), Component::ChirpstackApi);
            assert_eq!(err.transport(), Some(Transport::Lorawan));
            // The message says what was being attempted, not just what failed.
            assert!(err.message().contains("listing tenants"));
        }
    }

    /// The retry band: a down service and a slow one are retryable, an auth failure is not.
    #[test]
    fn only_the_reachability_failures_are_retryable() {
        use tonic::{Code, Status};
        assert!(map_status(&Status::new(Code::Unavailable, ""), "x")
            .kind()
            .is_retryable());
        assert!(map_status(&Status::new(Code::DeadlineExceeded, ""), "x")
            .kind()
            .is_retryable());
        assert!(!map_status(&Status::new(Code::Unauthenticated, ""), "x")
            .kind()
            .is_retryable());
        assert!(!map_status(&Status::new(Code::PermissionDenied, ""), "x")
            .kind()
            .is_retryable());
    }

    #[test]
    fn the_endpoint_is_loopback_and_not_configurable() {
        assert_eq!(API_ENDPOINT, "http://127.0.0.1:8080");
    }
}
