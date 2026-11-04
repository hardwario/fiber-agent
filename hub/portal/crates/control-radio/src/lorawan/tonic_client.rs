//! The live ChirpStack client — the one implementation of [`ChirpStack`] that opens a
//! connection.
//!
//! Every call reads its token from disk **per call**, which is what preserves v1's
//! rotate-without-restart property: replacing a token file takes effect on the next RPC, with
//! no service restart and no reconnect. The scope a call is trusted with is chosen here, at the
//! call site, and is the whole point of the three-token split.

use std::future::Future;
use std::time::Duration;

use async_trait::async_trait;
use chirpstack_api::api;
use chirpstack_api::tonic::transport::Channel;
use chirpstack_api::tonic::{Code, Request, Status};

use crate::error::{Component, Error, Kind, Result, Transport};
use crate::lorawan::client::{map_status, ChirpStack, API_ENDPOINT};
use crate::lorawan::eui::Eui;
use crate::lorawan::keys::{PlacedAbpSession, PlacedOtaaKeys};
use crate::lorawan::model::*;
use crate::lorawan::token::{Scope, Tokens};

pub struct TonicChirpStack {
    channel: Channel,
    tokens: Tokens,
}

/// How long to wait for the TCP/HTTP2 handshake before calling the API down.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Between two connect attempts of [`TonicChirpStack::connect_waiting`].
const CONNECT_RETRY: Duration = Duration::from_secs(2);
/// How long any single RPC may take. ChirpStack answers local calls in milliseconds; a call
/// still outstanding after this is not slow, it is not coming.
const REQUEST_DEADLINE: Duration = Duration::from_secs(20);

/// Bound one RPC. Elapsed is reported as `DeadlineExceeded` so it travels through the same
/// `map_status` taxonomy as a server-side deadline and lands on `Kind::Timeout` (exit 21)
/// rather than the catch-all `Transport` (22) — a hung API is a retry-with-backoff condition,
/// and the exit code has to say so.
///
/// Without this the only way to produce `Kind::Timeout` was for the server to volunteer it: a
/// paused ChirpStack (TCP up, no answers) blocked `create` forever, which on a Hub means the
/// boot oneshot never returns.
async fn deadline<T, F>(fut: F) -> std::result::Result<T, Status>
where
    F: Future<Output = std::result::Result<T, Status>>,
{
    deadline_of(REQUEST_DEADLINE, fut).await
}

/// [`deadline`] with an explicit budget, so the behaviour is testable without waiting out
/// `REQUEST_DEADLINE`.
async fn deadline_of<T, F>(budget: Duration, fut: F) -> std::result::Result<T, Status>
where
    F: Future<Output = std::result::Result<T, Status>>,
{
    match tokio::time::timeout(budget, fut).await {
        Ok(r) => r,
        Err(_) => Err(Status::new(
            Code::DeadlineExceeded,
            format!(
                "no answer from the ChirpStack API within {} ms",
                budget.as_millis()
            ),
        )),
    }
}

impl TonicChirpStack {
    /// Connect to the local ChirpStack API.
    pub async fn connect(tokens: Tokens) -> Result<Self> {
        let channel = Channel::from_static(API_ENDPOINT)
            .connect_timeout(CONNECT_TIMEOUT)
            .connect()
            .await
            .map_err(|e| {
                Error::new(
                    Kind::ServiceDown,
                    Transport::Lorawan,
                    Component::ChirpstackService,
                    format!("cannot reach the ChirpStack API at {API_ENDPOINT}: {e}"),
                )
            })?;
        Ok(TonicChirpStack { channel, tokens })
    }

    /// [`Self::connect`], retried for up to `wait` while the API refuses: ChirpStack is
    /// `Type=simple`, so right after a switch to LoRaWAN `radio-setup` runs before it listens.
    pub async fn connect_waiting(tokens: Tokens, wait: Duration) -> Result<Self> {
        let started = tokio::time::Instant::now();
        loop {
            match Self::connect(tokens.clone()).await {
                Ok(c) => return Ok(c),
                Err(e) if started.elapsed() + CONNECT_RETRY > wait => return Err(e),
                Err(_) => tokio::time::sleep(CONNECT_RETRY).await,
            }
        }
    }

    /// Wrap a request with the bearer token for one scope, read now.
    fn authed<T>(&self, scope: Scope, body: T) -> Result<Request<T>> {
        let token = self.tokens.get(scope)?;
        self.bearer(&token, body)
    }

    /// [`Self::authed`] for a credential the caller already holds rather than one on disk —
    /// the `InternalService` session JWT, which has no token file and no scope.
    fn session_authed<T>(&self, session: &str, body: T) -> Result<Request<T>> {
        self.bearer(session, body)
    }

    fn bearer<T>(&self, token: &str, body: T) -> Result<Request<T>> {
        let mut req = Request::new(body);
        let value = format!("Bearer {token}").parse().map_err(|_| {
            Error::new(
                Kind::InvalidArgument,
                Transport::Lorawan,
                Component::TokenFile,
                "the API token contains characters that cannot go in an HTTP header".to_string(),
            )
        })?;
        req.metadata_mut().insert("authorization", value);
        Ok(req)
    }
}

/// Translate a `NOT_FOUND` into `Ok(None)` for the get-then-create paths, and everything else
/// into the taxonomy.
fn optional<T>(r: std::result::Result<T, Status>, doing: &str) -> Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(s) if s.code() == chirpstack_api::tonic::Code::NotFound => Ok(None),
        Err(s) => Err(map_status(&s, doing)),
    }
}

/// Keys travel as lowercase hex strings, not bytes.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex16(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[async_trait]
impl ChirpStack for TonicChirpStack {
    async fn list_tenants(&self) -> Result<Vec<Tenant>> {
        let mut c = api::tenant_service_client::TenantServiceClient::new(self.channel.clone());
        let req = self.authed(
            Scope::Admin,
            api::ListTenantsRequest {
                limit: 100,
                offset: 0,
                search: String::new(),
                user_id: String::new(),
            },
        )?;
        let r = deadline(c.list(req))
            .await
            .map_err(|s| map_status(&s, "listing tenants"))?;
        Ok(r.into_inner()
            .result
            .into_iter()
            .map(|t| Tenant {
                id: t.id,
                name: t.name,
            })
            .collect())
    }

    async fn list_applications(&self, tenant_id: &str) -> Result<Vec<Application>> {
        let mut c =
            api::application_service_client::ApplicationServiceClient::new(self.channel.clone());
        let req = self.authed(
            Scope::Tenant,
            api::ListApplicationsRequest {
                limit: 100,
                offset: 0,
                search: String::new(),
                tenant_id: tenant_id.to_string(),
            },
        )?;
        let r = deadline(c.list(req))
            .await
            .map_err(|s| map_status(&s, "listing applications"))?;
        Ok(r.into_inner()
            .result
            .into_iter()
            .map(|a| Application {
                id: a.id,
                name: a.name,
            })
            .collect())
    }

    async fn create_application(
        &self,
        tenant_id: &str,
        name: &str,
        description: &str,
    ) -> Result<String> {
        let mut c =
            api::application_service_client::ApplicationServiceClient::new(self.channel.clone());
        let req = self.authed(
            Scope::Admin,
            api::CreateApplicationRequest {
                application: Some(api::Application {
                    id: String::new(),
                    name: name.to_string(),
                    description: description.to_string(),
                    tenant_id: tenant_id.to_string(),
                    tags: Default::default(),
                }),
            },
        )?;
        let r = deadline(c.create(req))
            .await
            .map_err(|s| map_status(&s, "creating the application"))?;
        Ok(r.into_inner().id)
    }

    async fn delete_application(&self, id: &str) -> Result<()> {
        let mut c =
            api::application_service_client::ApplicationServiceClient::new(self.channel.clone());
        let req = self.authed(
            Scope::Admin,
            api::DeleteApplicationRequest { id: id.to_string() },
        )?;
        deadline(c.delete(req))
            .await
            .map_err(|s| map_status(&s, "deleting the application"))?;
        Ok(())
    }

    async fn list_device_profiles(&self, tenant_id: &str) -> Result<Vec<DeviceProfile>> {
        let mut c = api::device_profile_service_client::DeviceProfileServiceClient::new(
            self.channel.clone(),
        );
        let req = self.authed(
            Scope::Tenant,
            api::ListDeviceProfilesRequest {
                limit: 100,
                offset: 0,
                search: String::new(),
                tenant_id: tenant_id.to_string(),
                device_id: String::new(),
                global_only: false,
                tenant_only: true,
            },
        )?;
        let r = deadline(c.list(req))
            .await
            .map_err(|s| map_status(&s, "listing device profiles"))?;
        Ok(r.into_inner()
            .result
            .into_iter()
            .map(|p| DeviceProfile {
                id: p.id,
                name: p.name,
            })
            .collect())
    }

    async fn create_device_profile(&self, spec: &DeviceProfileSpec) -> Result<String> {
        let mut c = api::device_profile_service_client::DeviceProfileServiceClient::new(
            self.channel.clone(),
        );

        let mut profile = api::DeviceProfile {
            id: String::new(),
            tenant_id: spec.tenant_id.clone(),
            name: spec.name.clone(),
            description: spec.description.clone(),
            adr_algorithm_id: spec.adr_algorithm_id.clone(),
            // Always NONE. Not read from config, not settable, not conditional — decode is
            // application-side only (decision #8 §5), and `DeviceProfileSpec` has no codec
            // field for this to come from.
            payload_codec_runtime: api::CodecRuntime::None as i32,
            payload_codec_script: String::new(),
            flush_queue_on_activate: spec.flush_queue_on_activate,
            uplink_interval: spec.uplink_interval,
            device_status_req_interval: spec.device_status_req_interval,
            supports_otaa: spec.supports_otaa,
            ..Default::default()
        };
        profile.set_region(region_enum(&spec.region)?);
        profile.set_mac_version(mac_version_enum(&spec.mac_version)?);
        profile.set_reg_params_revision(reg_params_enum(&spec.reg_params_revision)?);

        if let Some(abp) = &spec.abp {
            profile.abp_rx1_delay = abp.rx1_delay;
            profile.abp_rx1_dr_offset = abp.rx1_dr_offset;
            profile.abp_rx2_dr = abp.rx2_dr;
            profile.abp_rx2_freq = abp.rx2_freq;
        }

        let req = self.authed(
            Scope::Tenant,
            api::CreateDeviceProfileRequest {
                device_profile: Some(profile),
            },
        )?;
        let r = deadline(c.create(req))
            .await
            .map_err(|s| map_status(&s, "creating the device profile"))?;
        Ok(r.into_inner().id)
    }

    async fn delete_device_profile(&self, id: &str) -> Result<()> {
        let mut c = api::device_profile_service_client::DeviceProfileServiceClient::new(
            self.channel.clone(),
        );
        let req = self.authed(
            Scope::Tenant,
            api::DeleteDeviceProfileRequest { id: id.to_string() },
        )?;
        deadline(c.delete(req))
            .await
            .map_err(|s| map_status(&s, "deleting the device profile"))?;
        Ok(())
    }

    async fn get_gateway(&self, gateway_id: &Eui) -> Result<Option<Gateway>> {
        let mut c = api::gateway_service_client::GatewayServiceClient::new(self.channel.clone());
        // Read-only scope: this is the health path, the most-used credential, and it can
        // change nothing.
        let req = self.authed(
            Scope::ReadOnly,
            api::GetGatewayRequest {
                gateway_id: gateway_id.as_str().to_string(),
            },
        )?;
        let r = optional(deadline(c.get(req)).await, "getting the gateway")?;
        Ok(r.and_then(|r| {
            let inner = r.into_inner();
            inner.gateway.map(|g| Gateway {
                gateway_id: gateway_id.clone(),
                name: g.name,
                tenant_id: g.tenant_id,
                last_seen_at: inner.last_seen_at.map(|t| t.to_string()),
                created_at: inner.created_at.map(|t| t.to_string()),
            })
        }))
    }

    async fn list_gateways(&self, tenant_id: &str) -> Result<Vec<Gateway>> {
        let mut c = api::gateway_service_client::GatewayServiceClient::new(self.channel.clone());
        let req = self.authed(
            Scope::ReadOnly,
            api::ListGatewaysRequest {
                limit: 100,
                offset: 0,
                search: String::new(),
                tenant_id: tenant_id.to_string(),
                multicast_group_id: String::new(),
                order_by: 0,
                order_by_desc: false,
            },
        )?;
        let r = deadline(c.list(req))
            .await
            .map_err(|s| map_status(&s, "listing gateways"))?;
        let mut out = Vec::new();
        for g in r.into_inner().result {
            // A malformed EUI from the server is skipped rather than failing the whole poll:
            // one bad row must not make every gateway invisible.
            if let Ok(eui) = Eui::parse(&g.gateway_id) {
                out.push(Gateway {
                    gateway_id: eui,
                    name: g.name,
                    tenant_id: g.tenant_id,
                    last_seen_at: g.last_seen_at.map(|t| t.to_string()),
                    created_at: g.created_at.map(|t| t.to_string()),
                });
            }
        }
        Ok(out)
    }

    async fn find_gateway(&self, tenant_id: &str, gateway_id: &Eui) -> Result<Option<Gateway>> {
        // Paged, because `ListGatewaysRequest::search` does not match on the EUI (verified
        // against 4.19.0: a search for a registered gateway's own EUI returns an empty page)
        // and `list_gateways` above stops at the first 100 rows.
        const PAGE: u32 = 100;
        let mut offset = 0u32;
        loop {
            let mut c =
                api::gateway_service_client::GatewayServiceClient::new(self.channel.clone());
            let req = self.authed(
                Scope::ReadOnly,
                api::ListGatewaysRequest {
                    limit: PAGE,
                    offset,
                    search: String::new(),
                    tenant_id: tenant_id.to_string(),
                    multicast_group_id: String::new(),
                    order_by: 0,
                    order_by_desc: false,
                },
            )?;
            let page = deadline(c.list(req))
                .await
                .map_err(|s| map_status(&s, "looking up the gateway"))?
                .into_inner();
            for g in &page.result {
                if let Ok(eui) = Eui::parse(&g.gateway_id) {
                    if eui == *gateway_id {
                        return Ok(Some(Gateway {
                            gateway_id: eui,
                            name: g.name.clone(),
                            tenant_id: g.tenant_id.clone(),
                            last_seen_at: g.last_seen_at.map(|t| t.to_string()),
                            created_at: g.created_at.map(|t| t.to_string()),
                        }));
                    }
                }
            }
            offset += PAGE;
            if page.result.is_empty() || offset >= page.total_count {
                return Ok(None);
            }
        }
    }

    async fn create_gateway(&self, spec: &GatewaySpec) -> Result<()> {
        let mut c = api::gateway_service_client::GatewayServiceClient::new(self.channel.clone());
        let req = self.authed(
            Scope::Tenant,
            api::CreateGatewayRequest {
                gateway: Some(api::Gateway {
                    gateway_id: spec.gateway_id.as_str().to_string(),
                    name: spec.name.clone(),
                    description: spec.description.clone(),
                    tenant_id: spec.tenant_id.clone(),
                    // Both of these are proto3 scalars, so `Default` leaves them 0 — and a
                    // zero `downlink_priority` is refused outright by 4.19 with
                    // `downlink_priority must be > 0`, which made every local-gateway
                    // registration fail as InvalidArgument. 1 is the neutral bias (the field
                    // is only a positive/negative weight in ChirpStack's downlink gateway
                    // selection). `stats_interval` is what ChirpStack measures gateway
                    // liveness against; 30 s matches what chirpstack-mqtt-forwarder and
                    // gateway-bridge actually publish, so leaving it 0 would keep a healthy
                    // gateway reading as never-seen.
                    downlink_priority: 1,
                    stats_interval: 30,
                    ..Default::default()
                }),
            },
        )?;
        deadline(c.create(req))
            .await
            .map_err(|s| map_status(&s, "creating the gateway"))?;
        Ok(())
    }

    async fn delete_gateway(&self, gateway_id: &Eui) -> Result<()> {
        let mut c = api::gateway_service_client::GatewayServiceClient::new(self.channel.clone());
        let req = self.authed(
            Scope::Tenant,
            api::DeleteGatewayRequest {
                gateway_id: gateway_id.as_str().to_string(),
            },
        )?;
        deadline(c.delete(req))
            .await
            .map_err(|s| map_status(&s, "deleting the gateway"))?;
        Ok(())
    }

    async fn get_device(&self, dev_eui: &Eui) -> Result<Option<Device>> {
        let mut c = api::device_service_client::DeviceServiceClient::new(self.channel.clone());
        // Scope::Admin, not Tenant — measured against 4.19.0. ChirpStack's tenant-key
        // authorization for DeviceService::Get is an EXISTS join against the device row
        // itself (`validator.rs`'s `ValidateDeviceAccess::validate_key`, `Flag::Read`): a
        // tenant key is authorized only if a device with this dev_eui already exists under
        // that tenant. For a dev_eui that does not exist yet — exactly the ask-first check
        // §7's enrollment upsert makes before every `create_device` — that join matches zero
        // rows and ChirpStack returns `Unauthenticated`, not `NotFound`, indistinguishable
        // from a real permission failure. The admin key's `is_admin` branch short-circuits
        // that join, so it is the only scope that can legitimately ask "does this device
        // exist" before it exists. `Create` is unaffected because it is scoped by
        // `application_id`, which is real by the time enrollment runs.
        let req = self.authed(
            Scope::Admin,
            api::GetDeviceRequest {
                dev_eui: dev_eui.as_str().to_string(),
            },
        )?;
        let r = optional(deadline(c.get(req)).await, "getting the device")?;
        Ok(r.and_then(|r| {
            let inner = r.into_inner();
            inner.device.map(|d| Device {
                dev_eui: dev_eui.clone(),
                name: d.name,
                device_profile_id: d.device_profile_id,
                last_seen_at: inner.last_seen_at.map(|t| t.to_string()),
            })
        }))
    }

    async fn list_devices(&self, application_id: &str) -> Result<Vec<Device>> {
        let mut c = api::device_service_client::DeviceServiceClient::new(self.channel.clone());
        let req = self.authed(
            Scope::Tenant,
            api::ListDevicesRequest {
                limit: 1000,
                offset: 0,
                search: String::new(),
                application_id: application_id.to_string(),
                ..Default::default()
            },
        )?;
        let r = deadline(c.list(req))
            .await
            .map_err(|s| map_status(&s, "listing devices"))?;
        let mut out = Vec::new();
        for d in r.into_inner().result {
            if let Ok(eui) = Eui::parse(&d.dev_eui) {
                out.push(Device {
                    dev_eui: eui,
                    name: d.name,
                    device_profile_id: d.device_profile_id,
                    last_seen_at: d.last_seen_at.map(|t| t.to_string()),
                });
            }
        }
        Ok(out)
    }

    async fn create_device(&self, spec: &DeviceSpec) -> Result<()> {
        let mut c = api::device_service_client::DeviceServiceClient::new(self.channel.clone());
        let req = self.authed(
            Scope::Tenant,
            api::CreateDeviceRequest {
                device: Some(api::Device {
                    dev_eui: spec.dev_eui.as_str().to_string(),
                    name: spec.name.clone(),
                    description: spec.description.clone(),
                    application_id: spec.application_id.clone(),
                    device_profile_id: spec.device_profile_id.clone(),
                    join_eui: spec.join_eui.clone().unwrap_or_default(),
                    ..Default::default()
                }),
            },
        )?;
        deadline(c.create(req))
            .await
            .map_err(|s| map_status(&s, "creating the device"))?;
        Ok(())
    }

    async fn delete_device(&self, dev_eui: &Eui) -> Result<()> {
        let mut c = api::device_service_client::DeviceServiceClient::new(self.channel.clone());
        // Scope::Admin — same `ValidateDeviceAccess` EXISTS-join as `get_device` above
        // (`Flag::Delete` this time), same failure mode: a tenant key asked to delete a
        // dev_eui that is already gone (or never existed) gets `Unauthenticated`, not
        // `NotFound`. `Enroller::remove` depends on `NotFound` meaning "already gone, and
        // that's a success" (`enroll.rs`), so this call needs the scope that can actually
        // reach that branch.
        let req = self.authed(
            Scope::Admin,
            api::DeleteDeviceRequest {
                dev_eui: dev_eui.as_str().to_string(),
            },
        )?;
        deadline(c.delete(req))
            .await
            .map_err(|s| map_status(&s, "deleting the device"))?;
        Ok(())
    }

    async fn get_device_keys(&self, dev_eui: &Eui) -> Result<Option<PlacedOtaaKeys>> {
        let mut c = api::device_service_client::DeviceServiceClient::new(self.channel.clone());
        let req = self.authed(
            Scope::Tenant,
            api::GetDeviceKeysRequest {
                dev_eui: dev_eui.as_str().to_string(),
            },
        )?;
        let r = optional(deadline(c.get_keys(req)).await, "getting the device keys")?;
        Ok(r.and_then(|r| {
            r.into_inner().device_keys.and_then(|k| {
                Some(PlacedOtaaKeys {
                    nwk_key: unhex16(&k.nwk_key)?,
                    // ChirpStack leaves app_key empty for a 1.0.x device; the root key lives
                    // in nwk_key, and this mirrors it back so a comparison sees one value.
                    app_key: unhex16(&k.app_key).unwrap_or(unhex16(&k.nwk_key)?),
                })
            })
        }))
    }

    async fn create_device_keys(&self, dev_eui: &Eui, keys: &PlacedOtaaKeys) -> Result<()> {
        let mut c = api::device_service_client::DeviceServiceClient::new(self.channel.clone());
        let req = self.authed(
            Scope::Tenant,
            api::CreateDeviceKeysRequest {
                device_keys: Some(api::DeviceKeys {
                    dev_eui: dev_eui.as_str().to_string(),
                    nwk_key: hex(&keys.nwk_key),
                    app_key: hex(&keys.app_key),
                    gen_app_key: String::new(),
                }),
            },
        )?;
        deadline(c.create_keys(req))
            .await
            .map_err(|s| map_status(&s, "creating the device keys"))?;
        Ok(())
    }

    async fn update_device_keys(&self, dev_eui: &Eui, keys: &PlacedOtaaKeys) -> Result<()> {
        let mut c = api::device_service_client::DeviceServiceClient::new(self.channel.clone());
        let req = self.authed(
            Scope::Tenant,
            api::UpdateDeviceKeysRequest {
                device_keys: Some(api::DeviceKeys {
                    dev_eui: dev_eui.as_str().to_string(),
                    nwk_key: hex(&keys.nwk_key),
                    app_key: hex(&keys.app_key),
                    gen_app_key: String::new(),
                }),
            },
        )?;
        deadline(c.update_keys(req))
            .await
            .map_err(|s| map_status(&s, "updating the device keys"))?;
        Ok(())
    }

    async fn activate_device(&self, dev_eui: &Eui, session: &PlacedAbpSession) -> Result<()> {
        let mut c = api::device_service_client::DeviceServiceClient::new(self.channel.clone());
        let req = self.authed(
            Scope::Tenant,
            api::ActivateDeviceRequest {
                device_activation: Some(api::DeviceActivation {
                    dev_eui: dev_eui.as_str().to_string(),
                    dev_addr: hex(&session.dev_addr),
                    app_s_key: hex(&session.app_s_key),
                    nwk_s_enc_key: hex(&session.nwk_s_enc_key),
                    s_nwk_s_int_key: hex(&session.s_nwk_s_int_key),
                    f_nwk_s_int_key: hex(&session.f_nwk_s_int_key),
                    ..Default::default()
                }),
            },
        )?;
        deadline(c.activate(req))
            .await
            .map_err(|s| map_status(&s, "activating the device"))?;
        Ok(())
    }

    async fn enqueue_downlink(
        &self,
        dev_eui: &Eui,
        f_port: u8,
        data: Vec<u8>,
        confirmed: bool,
    ) -> Result<String> {
        let mut c = api::device_service_client::DeviceServiceClient::new(self.channel.clone());
        let req = self.authed(
            Scope::Tenant,
            api::EnqueueDeviceQueueItemRequest {
                queue_item: Some(api::DeviceQueueItem {
                    dev_eui: dev_eui.as_str().to_string(),
                    f_port: f_port as u32,
                    data,
                    // Unconfirmed: application-level confirmation is the engine's own
                    // seq-correlated Response, not the MAC layer's.
                    confirmed,
                    ..Default::default()
                }),
                // Never flush the queue on enqueue: a pending command from another exchange
                // is not this one's to discard.
                flush_queue: false,
            },
        )?;
        let r = deadline(c.enqueue(req))
            .await
            .map_err(|s| map_status(&s, "enqueuing the downlink"))?;
        Ok(r.into_inner().id)
    }

    async fn get_queue(&self, dev_eui: &Eui) -> Result<Vec<QueuedDownlink>> {
        let mut c = api::device_service_client::DeviceServiceClient::new(self.channel.clone());
        let req = self.authed(
            Scope::Tenant,
            api::GetDeviceQueueItemsRequest {
                dev_eui: dev_eui.as_str().to_string(),
                count_only: false,
            },
        )?;
        let r = deadline(c.get_queue(req))
            .await
            .map_err(|s| map_status(&s, "reading the device queue"))?;
        Ok(r.into_inner()
            .result
            .into_iter()
            .map(|i| QueuedDownlink {
                id: i.id,
                f_port: u8::try_from(i.f_port).unwrap_or(0),
                data: i.data,
                confirmed: i.confirmed,
                is_pending: i.is_pending,
            })
            .collect())
    }

    async fn flush_queue(&self, dev_eui: &Eui) -> Result<()> {
        let mut c = api::device_service_client::DeviceServiceClient::new(self.channel.clone());
        let req = self.authed(
            Scope::Tenant,
            api::FlushDeviceQueueRequest {
                dev_eui: dev_eui.as_str().to_string(),
            },
        )?;
        deadline(c.flush_queue(req))
            .await
            .map_err(|s| map_status(&s, "flushing the device queue"))?;
        Ok(())
    }

    async fn login(&self, email: &str, password: &str) -> Result<String> {
        let mut c = api::internal_service_client::InternalServiceClient::new(self.channel.clone());
        // No `authed` wrapper: this call *is* the one that produces a credential, so it carries
        // none. A wrong password comes back `Unauthenticated`, which the taxonomy already
        // separates from an absent one (exit 11 vs 10).
        let r = deadline(c.login(Request::new(api::LoginRequest {
            email: email.to_string(),
            password: password.to_string(),
        })))
        .await
        .map_err(|s| map_status(&s, "logging in to mint the tenant API keys"))?;
        Ok(r.into_inner().jwt)
    }

    async fn create_api_key(&self, session: &str, spec: &ApiKeySpec) -> Result<MintedApiKey> {
        let mut c = api::internal_service_client::InternalServiceClient::new(self.channel.clone());
        let req = self.session_authed(
            session,
            api::CreateApiKeyRequest {
                api_key: Some(api::ApiKey {
                    id: String::new(),
                    name: spec.name.clone(),
                    is_admin: spec.is_admin,
                    tenant_id: spec.tenant_id.clone().unwrap_or_default(),
                    is_read_only: spec.is_read_only,
                }),
            },
        )?;
        let r = deadline(c.create_api_key(req))
            .await
            .map_err(|s| map_status(&s, "minting an API key"))?;
        let inner = r.into_inner();
        Ok(MintedApiKey {
            id: inner.id,
            token: inner.token,
        })
    }

    async fn list_api_keys(
        &self,
        session: &str,
        tenant_id: Option<&str>,
    ) -> Result<Vec<ApiKeyInfo>> {
        let mut c = api::internal_service_client::InternalServiceClient::new(self.channel.clone());
        let req = self.session_authed(
            session,
            api::ListApiKeysRequest {
                limit: 100,
                offset: 0,
                is_admin: tenant_id.is_none(),
                tenant_id: tenant_id.unwrap_or_default().to_string(),
            },
        )?;
        let r = deadline(c.list_api_keys(req))
            .await
            .map_err(|s| map_status(&s, "listing API keys"))?;
        Ok(r.into_inner()
            .result
            .into_iter()
            .map(|k| ApiKeyInfo {
                id: k.id,
                name: k.name,
            })
            .collect())
    }

    async fn delete_api_key(&self, session: &str, id: &str) -> Result<()> {
        let mut c = api::internal_service_client::InternalServiceClient::new(self.channel.clone());
        let req = self.session_authed(session, api::DeleteApiKeyRequest { id: id.to_string() })?;
        deadline(c.delete_api_key(req))
            .await
            .map_err(|s| map_status(&s, "deleting the API key"))?;
        Ok(())
    }
}

fn enum_err(what: &str, value: &str) -> Error {
    Error::new(
        Kind::InvalidArgument,
        Transport::Lorawan,
        Component::ConfigJson,
        format!("{what} {value:?} is not one ChirpStack accepts"),
    )
}

fn region_enum(s: &str) -> Result<chirpstack_api::common::Region> {
    match s.to_ascii_lowercase().as_str() {
        "eu868" => Ok(chirpstack_api::common::Region::Eu868),
        other => Err(enum_err("region", other)),
    }
}

fn mac_version_enum(s: &str) -> Result<chirpstack_api::common::MacVersion> {
    match s {
        "LORAWAN_1_0_4" => Ok(chirpstack_api::common::MacVersion::Lorawan104),
        other => Err(enum_err("mac_version", other)),
    }
}

fn reg_params_enum(s: &str) -> Result<chirpstack_api::common::RegParamsRevision> {
    match s {
        "RP002_1_0_4" => Ok(chirpstack_api::common::RegParamsRevision::Rp002104),
        other => Err(enum_err("reg_params_revision", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_travel_as_lowercase_hex() {
        assert_eq!(hex(&[0x00, 0xab, 0xFF]), "00abff");
        assert_eq!(unhex16(&"ab".repeat(16)), Some([0xab; 16]));
        assert_eq!(unhex16("short"), None);
        assert_eq!(unhex16(&"zz".repeat(16)), None);
    }

    #[test]
    fn only_the_supported_enum_values_are_accepted() {
        assert!(region_enum("eu868").is_ok());
        assert!(region_enum("EU868").is_ok(), "case-insensitive");
        assert!(
            region_enum("us915").is_err(),
            "not shipped, and not silently coerced"
        );
        assert!(mac_version_enum("LORAWAN_1_0_4").is_ok());
        assert!(mac_version_enum("LORAWAN_1_1_0").is_err());
        assert!(reg_params_enum("RP002_1_0_4").is_ok());
        assert!(reg_params_enum("RP001_1_0_3").is_err());
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;

    /// F11: before this, `Kind::Timeout` could only be produced by the server volunteering a
    /// `DeadlineExceeded`. A ChirpStack that holds the connection open and never answers —
    /// `docker compose pause` reproduces it exactly — blocked `create` indefinitely, and on a
    /// Hub that is the boot oneshot never returning.
    #[tokio::test]
    async fn a_call_that_never_answers_becomes_deadline_exceeded() {
        let never = std::future::pending::<std::result::Result<(), Status>>();
        let status = deadline_of(Duration::from_millis(20), never)
            .await
            .expect_err("must not wait forever");
        assert_eq!(status.code(), Code::DeadlineExceeded);
        // And the taxonomy turns that into exit 21, not the retryable-but-vague 22.
        let err = map_status(&status, "creating the application");
        assert_eq!(err.kind(), Kind::Timeout);
        assert_eq!(err.component(), Component::ChirpstackApi);
    }

    /// The deadline must be invisible to a call that answers.
    #[tokio::test]
    async fn a_prompt_call_passes_straight_through() {
        let ok = async { Ok::<u8, Status>(7) };
        assert_eq!(deadline(ok).await.expect("passes through"), 7);
    }

    /// The shipped budget is the one the boot unit's `TimeoutStartSec` is sized against.
    #[test]
    fn the_shipped_budgets_are_the_ones_the_boot_unit_assumes() {
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(5));
        assert_eq!(REQUEST_DEADLINE, Duration::from_secs(20));
    }
}
