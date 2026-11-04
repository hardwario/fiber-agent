//! An in-memory [`ChirpStack`] for tests — `plan/control/radio/lorawan.md` §10 ("converge
//! idempotency against a mocked client trait").
//!
//! It is deliberately not a friendly mock. It reproduces the behaviours §7 measured against a
//! live 4.19.1, because those are exactly what a rewrite gets wrong:
//!
//! - **A duplicate `Create` does not return `AlreadyExists`.** It returns `Internal` carrying
//!   a raw storage error (`duplicate key value violates unique constraint "device_pkey"`).
//!   v1 branched on the message text; neither of its two patterns matches, and the device
//!   runs the SQLite build whose message differs again. Code that depends on recognising it
//!   fails here.
//! - **`Delete` of an absent resource returns a real `NotFound`**, which is safe to branch on
//!   and is treated as success.
//! - **`InternalService` accepts only a session JWT.** The three API-key RPCs refuse anything
//!   that did not come out of `login`, exactly as 4.19.0 refuses an admin API key there —
//!   which is what makes `bootstrap.rs`'s Login-first sequence the only one that passes.
//!
//! Also records a call log, so tests can assert *how* converge reached its result — that it
//! asked before creating, and that a second run made no writes at all.

use std::sync::Mutex;

use async_trait::async_trait;
use chirpstack_api::tonic::{Code, Status};

use crate::error::Result;
use crate::lorawan::client::{map_status, ChirpStack};
use crate::lorawan::eui::Eui;
use crate::lorawan::keys::{PlacedAbpSession, PlacedOtaaKeys};
use crate::lorawan::model::*;

/// ChirpStack v4's well-known default-tenant id — the one `create` resolves rather than
/// creates.
pub const DEFAULT_TENANT_ID: &str = "52f14cd4-c6f1-4fbd-8f87-4025e1d49242";

/// The password a fresh [`FakeChirpStack`] accepts for the `admin` user. A real Hub replaces
/// the factory `admin`/`admin` before the service ever listens (`lorawan.md` §4); the fake
/// keeps a value rather than accepting everything, so the rejected-login path is reachable.
pub const FAKE_ADMIN_PASSWORD: &str = "seeded-admin-password";

/// The session JWT `login` hands out. Named, so a test can assert the API-key RPCs were given
/// *this* credential and not a token file.
pub const FAKE_SESSION_JWT: &str = "session.jwt.admin";

#[derive(Debug, Default)]
struct Inner {
    tenants: Vec<Tenant>,
    applications: Vec<(String, Application)>, // (tenant_id, app)
    profiles: Vec<(String, DeviceProfile)>,   // (tenant_id, profile)
    gateways: Vec<Gateway>,
    devices: Vec<(String, Device)>, // (application_id, device)
    device_keys: Vec<(String, PlacedOtaaKeys)>,
    /// The JoinEUI each device was created with. `Device` is the read model and does not
    /// carry it, but `node-add`'s config fallback has to be assertable (F22).
    device_join_euis: Vec<(String, Option<String>)>,
    sessions: Vec<(String, PlacedAbpSession)>,
    /// Every enqueued downlink, in order — `node-send` has to be assertable on the fPort and
    /// the bytes, not just on "a call happened".
    downlinks: Vec<Downlink>,
    /// What is still queued per device: every enqueue lands here too, and a flush empties it.
    queue: Vec<(Downlink, String)>,
    /// The queue items ChirpStack has on the air (`is_pending`), by id.
    pending: Vec<String>,
    api_keys: Vec<ApiKeyInfo>,
    /// The password `login` accepts, and the session it hands out in exchange.
    admin_password: String,
    issued_session: Option<String>,
    calls: Vec<String>,
    next_id: u32,
    /// Set to fail every call with this status, for the failure-path tests.
    fail_with: Option<(Code, String)>,
    /// Mirror ChirpStack 4.19.0's authorization model for `GatewayService.Get`: the
    /// EXISTS-join in `validate_key` means a *non-existent* gateway id is reported as
    /// `Unauthenticated`, never `NotFound`, for every key below admin. Measured live
    /// 2026-08-25 (readonly and tenant both `Unauthenticated`, admin `NotFound`).
    gateway_get_exists_join: bool,
}

/// One `DeviceService.Enqueue` as the fake saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downlink {
    pub dev_eui: String,
    pub f_port: u8,
    pub data: Vec<u8>,
    pub confirmed: bool,
}

#[derive(Debug, Default)]
pub struct FakeChirpStack {
    inner: Mutex<Inner>,
}

impl FakeChirpStack {
    /// A virgin ChirpStack: the default tenant exists, nothing else does.
    pub fn new() -> Self {
        let f = FakeChirpStack::default();
        {
            let mut i = f.lock();
            i.tenants.push(Tenant {
                id: DEFAULT_TENANT_ID.to_string(),
                name: "ChirpStack".to_string(),
            });
            i.admin_password = FAKE_ADMIN_PASSWORD.to_string();
        }
        f
    }

    /// Change the password `login` accepts — how the `Unauthenticated` bootstrap path is
    /// reached without a server.
    pub fn with_admin_password(self, password: &str) -> Self {
        self.lock().admin_password = password.to_string();
        self
    }

    /// The API keys that exist server-side, in mint order.
    pub fn api_keys(&self) -> Vec<ApiKeyInfo> {
        self.lock().api_keys.clone()
    }

    /// Seed a key row that no token file matches — a half-failed mint, for the prune path.
    pub fn seed_api_key(&self, name: &str) -> String {
        let id = self.fresh_id();
        self.lock().api_keys.push(ApiKeyInfo {
            id: id.clone(),
            name: name.to_string(),
        });
        id
    }

    /// Reproduce ChirpStack 4.19.0's EXISTS-join on `GatewayService.Get`, so a lookup of a
    /// not-yet-registered gateway fails `Unauthenticated` instead of returning `None`.
    pub fn with_gateway_get_exists_join(self) -> Self {
        self.lock().gateway_get_exists_join = true;
        self
    }

    /// Make every subsequent call fail — for asserting the taxonomy mapping end to end.
    pub fn fail_every_call_with(&self, code: Code, message: &str) {
        self.lock().fail_with = Some((code, message.to_string()));
    }

    pub fn recover(&self) {
        self.lock().fail_with = None;
    }

    /// Everything called since the last [`Self::clear_calls`], in order.
    pub fn calls(&self) -> Vec<String> {
        self.lock().calls.clone()
    }

    pub fn clear_calls(&self) {
        self.lock().calls.clear();
    }

    /// The calls that changed server state. A converged `create` re-run must make none.
    pub fn writes(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| {
                c.starts_with("create_")
                    || c.starts_with("delete_")
                    || c.starts_with("update_")
                    || c.starts_with("activate_")
            })
            .collect()
    }

    pub fn gateway_count(&self) -> usize {
        self.lock().gateways.len()
    }

    pub fn device_count(&self) -> usize {
        self.lock().devices.len()
    }

    pub fn application_count(&self) -> usize {
        self.lock().applications.len()
    }

    pub fn profile_count(&self) -> usize {
        self.lock().profiles.len()
    }

    pub fn tenant_count(&self) -> usize {
        self.lock().tenants.len()
    }

    /// The JoinEUI `create_device` was called with, for the `--join-eui`/config-default test.
    pub fn stored_join_eui(&self, dev_eui: &Eui) -> Option<String> {
        self.lock()
            .device_join_euis
            .iter()
            .find(|(e, _)| e == dev_eui.as_str())
            .and_then(|(_, j)| j.clone())
    }

    pub fn stored_keys(&self, dev_eui: &Eui) -> Option<PlacedOtaaKeys> {
        self.lock()
            .device_keys
            .iter()
            .find(|(e, _)| e == dev_eui.as_str())
            .map(|(_, k)| k.clone())
    }

    /// Put the device's first queued item on the air, as ChirpStack does once it has handed
    /// it to a gateway: `get_queue` reports it `is_pending` until a flush drops it.
    pub fn mark_head_pending(&self, dev_eui: &Eui) {
        let mut i = self.lock();
        if let Some(id) = i
            .queue
            .iter()
            .find(|(d, _)| d.dev_eui == dev_eui.as_str())
            .map(|(_, id)| id.clone())
        {
            i.pending.push(id);
        }
    }

    /// The device's queued item carrying `data` went out: ChirpStack drops an unconfirmed
    /// downlink from the queue once it has transmitted it. The first match only.
    pub fn transmitted(&self, dev_eui: &Eui, data: &[u8]) {
        let mut i = self.lock();
        if let Some(at) = i
            .queue
            .iter()
            .position(|(d, _)| d.dev_eui == dev_eui.as_str() && d.data == data)
        {
            let (_, id) = i.queue.remove(at);
            i.pending.retain(|p| *p != id);
        }
    }

    /// Every downlink enqueued so far, in order.
    pub fn downlinks(&self) -> Vec<Downlink> {
        self.lock().downlinks.clone()
    }

    /// Seed a device that `create`/`node-add` did not put there — an out-of-band enrollment,
    /// which is what `node-list` must still show (and show without a serial).
    pub fn seed_device(&self, application_id: &str, device: Device) {
        self.lock()
            .devices
            .push((application_id.to_string(), device));
    }

    /// Record an uplink having been seen, so the online/last-seen half of the read commands is
    /// reachable without a radio.
    pub fn mark_device_seen(&self, dev_eui: &Eui, at: &str) {
        let mut i = self.lock();
        if let Some((_, d)) = i.devices.iter_mut().find(|(_, d)| d.dev_eui == *dev_eui) {
            d.last_seen_at = Some(at.to_string());
        }
    }

    /// The same for a gateway — `gateway-list`'s online window has to be exercisable.
    pub fn mark_gateway_seen(&self, gateway_id: &Eui, at: &str) {
        let mut i = self.lock();
        if let Some(g) = i.gateways.iter_mut().find(|g| g.gateway_id == *gateway_id) {
            g.last_seen_at = Some(at.to_string());
        }
    }

    pub fn stored_session(&self, dev_eui: &Eui) -> Option<PlacedAbpSession> {
        self.lock()
            .sessions
            .iter()
            .find(|(e, _)| e == dev_eui.as_str())
            .map(|(_, s)| s.clone())
    }

    /// Simulate an out-of-band support edit, so drift detection has something to find.
    pub fn rename_application(&self, id: &str, new_name: &str) {
        let mut i = self.lock();
        if let Some((_, app)) = i.applications.iter_mut().find(|(_, a)| a.id == id) {
            app.name = new_name.to_string();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(g) => g,
            // A poisoned mutex means a test panicked mid-call; keep going so the real failure
            // is the one that gets reported, not this.
            Err(p) => p.into_inner(),
        }
    }

    fn begin(&self, call: &str) -> Result<()> {
        let mut i = self.lock();
        i.calls.push(call.to_string());
        if let Some((code, msg)) = i.fail_with.clone() {
            return Err(map_status(&Status::new(code, msg), call));
        }
        Ok(())
    }

    /// `InternalService` authenticates against session claims only: an API-key bearer token is
    /// rejected there however privileged it is (measured against 4.19.0). The fake enforces it
    /// so a regression to `authed(Scope::Admin)` fails here rather than on a Hub.
    fn require_session(&self, session: &str, doing: &str) -> Result<()> {
        if self.lock().issued_session.as_deref() == Some(session) {
            return Ok(());
        }
        Err(map_status(
            &Status::new(
                Code::Unauthenticated,
                "authentication failed: InternalService accepts a session JWT only",
            ),
            doing,
        ))
    }

    fn fresh_id(&self) -> String {
        let mut i = self.lock();
        i.next_id += 1;
        format!("00000000-0000-0000-0000-{:012}", i.next_id)
    }
}

/// The error a real 4.19.1 returns for a duplicate `Create`: `Internal`, carrying the raw
/// storage error — *not* `AlreadyExists`.
fn duplicate_key(constraint: &str, call: &str) -> crate::error::Error {
    map_status(
        &Status::new(
            Code::Internal,
            format!("duplicate key value violates unique constraint \"{constraint}\""),
        ),
        call,
    )
}

fn not_found(what: &str, call: &str) -> crate::error::Error {
    map_status(
        &Status::new(Code::NotFound, format!("object not found: {what}")),
        call,
    )
}

#[async_trait]
impl ChirpStack for FakeChirpStack {
    async fn list_tenants(&self) -> Result<Vec<Tenant>> {
        self.begin("list_tenants")?;
        Ok(self.lock().tenants.clone())
    }

    async fn list_applications(&self, tenant_id: &str) -> Result<Vec<Application>> {
        self.begin("list_applications")?;
        Ok(self
            .lock()
            .applications
            .iter()
            .filter(|(t, _)| t == tenant_id)
            .map(|(_, a)| a.clone())
            .collect())
    }

    async fn create_application(
        &self,
        tenant_id: &str,
        name: &str,
        description: &str,
    ) -> Result<String> {
        self.begin("create_application")?;
        let _ = description;
        if self
            .lock()
            .applications
            .iter()
            .any(|(t, a)| t == tenant_id && a.name == name)
        {
            return Err(duplicate_key("application_name_key", "create_application"));
        }
        let id = self.fresh_id();
        self.lock().applications.push((
            tenant_id.to_string(),
            Application {
                id: id.clone(),
                name: name.to_string(),
            },
        ));
        Ok(id)
    }

    async fn delete_application(&self, id: &str) -> Result<()> {
        self.begin("delete_application")?;
        let mut i = self.lock();
        let before = i.applications.len();
        i.applications.retain(|(_, a)| a.id != id);
        if i.applications.len() == before {
            return Err(not_found(id, "delete_application"));
        }
        i.devices.retain(|(app, _)| app != id);
        Ok(())
    }

    async fn list_device_profiles(&self, tenant_id: &str) -> Result<Vec<DeviceProfile>> {
        self.begin("list_device_profiles")?;
        Ok(self
            .lock()
            .profiles
            .iter()
            .filter(|(t, _)| t == tenant_id)
            .map(|(_, p)| p.clone())
            .collect())
    }

    async fn create_device_profile(&self, spec: &DeviceProfileSpec) -> Result<String> {
        self.begin("create_device_profile")?;
        if self
            .lock()
            .profiles
            .iter()
            .any(|(t, p)| *t == spec.tenant_id && p.name == spec.name)
        {
            return Err(duplicate_key(
                "device_profile_name_key",
                "create_device_profile",
            ));
        }
        let id = self.fresh_id();
        self.lock().profiles.push((
            spec.tenant_id.clone(),
            DeviceProfile {
                id: id.clone(),
                name: spec.name.clone(),
            },
        ));
        Ok(id)
    }

    async fn delete_device_profile(&self, id: &str) -> Result<()> {
        self.begin("delete_device_profile")?;
        let mut i = self.lock();
        let before = i.profiles.len();
        i.profiles.retain(|(_, p)| p.id != id);
        if i.profiles.len() == before {
            return Err(not_found(id, "delete_device_profile"));
        }
        Ok(())
    }

    async fn get_gateway(&self, gateway_id: &Eui) -> Result<Option<Gateway>> {
        self.begin("get_gateway")?;
        let i = self.lock();
        let found = i
            .gateways
            .iter()
            .find(|g| g.gateway_id == *gateway_id)
            .cloned();
        if found.is_none() && i.gateway_get_exists_join {
            return Err(map_status(
                &Status::new(Code::Unauthenticated, "authentication failed"),
                "getting the gateway",
            ));
        }
        Ok(found)
    }

    async fn find_gateway(&self, tenant_id: &str, gateway_id: &Eui) -> Result<Option<Gateway>> {
        self.begin("find_gateway")?;
        Ok(self
            .lock()
            .gateways
            .iter()
            .find(|g| g.tenant_id == tenant_id && g.gateway_id == *gateway_id)
            .cloned())
    }

    async fn list_gateways(&self, tenant_id: &str) -> Result<Vec<Gateway>> {
        self.begin("list_gateways")?;
        Ok(self
            .lock()
            .gateways
            .iter()
            .filter(|g| g.tenant_id == tenant_id)
            .cloned()
            .collect())
    }

    async fn create_gateway(&self, spec: &GatewaySpec) -> Result<()> {
        self.begin("create_gateway")?;
        if self
            .lock()
            .gateways
            .iter()
            .any(|g| g.gateway_id == spec.gateway_id)
        {
            return Err(duplicate_key("gateway_pkey", "create_gateway"));
        }
        self.lock().gateways.push(Gateway {
            gateway_id: spec.gateway_id.clone(),
            name: spec.name.clone(),
            tenant_id: spec.tenant_id.clone(),
            last_seen_at: None,
            created_at: Some("2026-08-20T00:00:00Z".to_string()),
        });
        Ok(())
    }

    async fn delete_gateway(&self, gateway_id: &Eui) -> Result<()> {
        self.begin("delete_gateway")?;
        let mut i = self.lock();
        let before = i.gateways.len();
        i.gateways.retain(|g| g.gateway_id != *gateway_id);
        if i.gateways.len() == before {
            return Err(not_found(gateway_id.as_str(), "delete_gateway"));
        }
        Ok(())
    }

    async fn get_device(&self, dev_eui: &Eui) -> Result<Option<Device>> {
        self.begin("get_device")?;
        Ok(self
            .lock()
            .devices
            .iter()
            .find(|(_, d)| d.dev_eui == *dev_eui)
            .map(|(_, d)| d.clone()))
    }

    async fn list_devices(&self, application_id: &str) -> Result<Vec<Device>> {
        self.begin("list_devices")?;
        Ok(self
            .lock()
            .devices
            .iter()
            .filter(|(a, _)| a == application_id)
            .map(|(_, d)| d.clone())
            .collect())
    }

    async fn create_device(&self, spec: &DeviceSpec) -> Result<()> {
        self.begin("create_device")?;
        if self
            .lock()
            .devices
            .iter()
            .any(|(_, d)| d.dev_eui == spec.dev_eui)
        {
            return Err(duplicate_key("device_pkey", "create_device"));
        }
        let mut i = self.lock();
        i.devices.push((
            spec.application_id.clone(),
            Device {
                dev_eui: spec.dev_eui.clone(),
                name: spec.name.clone(),
                device_profile_id: spec.device_profile_id.clone(),
                last_seen_at: None,
            },
        ));
        i.device_join_euis
            .push((spec.dev_eui.as_str().to_string(), spec.join_eui.clone()));
        Ok(())
    }

    async fn delete_device(&self, dev_eui: &Eui) -> Result<()> {
        self.begin("delete_device")?;
        let mut i = self.lock();
        let before = i.devices.len();
        i.devices.retain(|(_, d)| d.dev_eui != *dev_eui);
        if i.devices.len() == before {
            return Err(not_found(dev_eui.as_str(), "delete_device"));
        }
        i.device_keys.retain(|(e, _)| e != dev_eui.as_str());
        i.sessions.retain(|(e, _)| e != dev_eui.as_str());
        Ok(())
    }

    async fn get_device_keys(&self, dev_eui: &Eui) -> Result<Option<PlacedOtaaKeys>> {
        self.begin("get_device_keys")?;
        Ok(self
            .lock()
            .device_keys
            .iter()
            .find(|(e, _)| e == dev_eui.as_str())
            .map(|(_, k)| k.clone()))
    }

    async fn create_device_keys(&self, dev_eui: &Eui, keys: &PlacedOtaaKeys) -> Result<()> {
        self.begin("create_device_keys")?;
        if self
            .lock()
            .device_keys
            .iter()
            .any(|(e, _)| e == dev_eui.as_str())
        {
            return Err(duplicate_key("device_keys_pkey", "create_device_keys"));
        }
        self.lock()
            .device_keys
            .push((dev_eui.as_str().to_string(), keys.clone()));
        Ok(())
    }

    async fn update_device_keys(&self, dev_eui: &Eui, keys: &PlacedOtaaKeys) -> Result<()> {
        self.begin("update_device_keys")?;
        let mut i = self.lock();
        match i
            .device_keys
            .iter_mut()
            .find(|(e, _)| e == dev_eui.as_str())
        {
            Some((_, slot)) => {
                *slot = keys.clone();
                Ok(())
            }
            None => Err(not_found(dev_eui.as_str(), "update_device_keys")),
        }
    }

    async fn activate_device(&self, dev_eui: &Eui, session: &PlacedAbpSession) -> Result<()> {
        self.begin("activate_device")?;
        let mut i = self.lock();
        i.sessions.retain(|(e, _)| e != dev_eui.as_str());
        i.sessions
            .push((dev_eui.as_str().to_string(), session.clone()));
        Ok(())
    }

    async fn enqueue_downlink(
        &self,
        dev_eui: &Eui,
        f_port: u8,
        data: Vec<u8>,
        confirmed: bool,
    ) -> Result<String> {
        self.begin("enqueue_downlink")?;
        let downlink = Downlink {
            dev_eui: dev_eui.as_str().to_string(),
            f_port,
            data,
            confirmed,
        };
        let id = self.fresh_id();
        let mut i = self.lock();
        i.downlinks.push(downlink.clone());
        i.queue.push((downlink, id.clone()));
        Ok(id)
    }

    async fn get_queue(&self, dev_eui: &Eui) -> Result<Vec<QueuedDownlink>> {
        self.begin("get_queue")?;
        let i = self.lock();
        Ok(i.queue
            .iter()
            .filter(|(d, _)| d.dev_eui == dev_eui.as_str())
            .map(|(d, id)| QueuedDownlink {
                id: id.clone(),
                f_port: d.f_port,
                data: d.data.clone(),
                confirmed: d.confirmed,
                is_pending: i.pending.contains(id),
            })
            .collect())
    }

    async fn flush_queue(&self, dev_eui: &Eui) -> Result<()> {
        self.begin("flush_queue")?;
        let mut i = self.lock();
        let gone: Vec<String> = i
            .queue
            .iter()
            .filter(|(d, _)| d.dev_eui == dev_eui.as_str())
            .map(|(_, id)| id.clone())
            .collect();
        i.queue.retain(|(d, _)| d.dev_eui != dev_eui.as_str());
        i.pending.retain(|id| !gone.contains(id));
        Ok(())
    }

    async fn login(&self, email: &str, password: &str) -> Result<String> {
        self.begin("login")?;
        let mut i = self.lock();
        if email != "admin" || password != i.admin_password {
            return Err(map_status(
                &Status::new(Code::Unauthenticated, "invalid username or password"),
                "logging in",
            ));
        }
        i.issued_session = Some(FAKE_SESSION_JWT.to_string());
        Ok(FAKE_SESSION_JWT.to_string())
    }

    async fn create_api_key(&self, session: &str, spec: &ApiKeySpec) -> Result<MintedApiKey> {
        self.begin("create_api_key")?;
        self.require_session(session, "minting an API key")?;
        let id = self.fresh_id();
        self.lock().api_keys.push(ApiKeyInfo {
            id: id.clone(),
            name: spec.name.clone(),
        });
        Ok(MintedApiKey {
            token: format!("jwt.for.{}", spec.name),
            id,
        })
    }

    async fn list_api_keys(
        &self,
        session: &str,
        _tenant_id: Option<&str>,
    ) -> Result<Vec<ApiKeyInfo>> {
        self.begin("list_api_keys")?;
        self.require_session(session, "listing API keys")?;
        Ok(self.lock().api_keys.clone())
    }

    async fn delete_api_key(&self, session: &str, id: &str) -> Result<()> {
        self.begin("delete_api_key")?;
        self.require_session(session, "deleting the API key")?;
        let mut i = self.lock();
        let before = i.api_keys.len();
        i.api_keys.retain(|k| k.id != id);
        if i.api_keys.len() == before {
            return Err(not_found(id, "delete_api_key"));
        }
        Ok(())
    }
}

/// An in-memory [`RetainedTopics`] for discovery tests — no broker needed.
#[derive(Debug, Default)]
pub struct FakeRetainedTopics {
    topics: Mutex<Vec<String>>,
    fail: Mutex<Option<crate::error::Error>>,
}

impl FakeRetainedTopics {
    pub fn with(topics: &[&str]) -> Self {
        FakeRetainedTopics {
            topics: Mutex::new(topics.iter().map(|s| s.to_string()).collect()),
            fail: Mutex::new(None),
        }
    }

    /// Nothing retained — a Hub with no concentrator, which is a degraded success.
    pub fn empty() -> Self {
        FakeRetainedTopics::default()
    }

    pub fn failing(e: crate::error::Error) -> Self {
        FakeRetainedTopics {
            topics: Mutex::new(Vec::new()),
            fail: Mutex::new(Some(e)),
        }
    }
}

/// An in-memory [`crate::northbound::RetainedPublisher`] — the write half of the broker seam.
///
/// Records what was published so a test can assert the retained `info` document and the
/// tombstone, and can be told to fail so the "ChirpStack delete succeeded, the broker did not"
/// path is reachable without a broker to break.
#[derive(Debug, Default)]
pub struct FakePublisher {
    published: Mutex<Vec<(String, Vec<u8>)>>,
    /// What went out not retained ([`crate::northbound::RetainedPublisher::publish_events`]),
    /// kept apart so a retained-only assertion never sees an event.
    events: Mutex<Vec<(String, Vec<u8>)>>,
    fail: Mutex<Option<crate::error::Error>>,
}

impl FakePublisher {
    pub fn new() -> Self {
        FakePublisher::default()
    }

    /// A broker that refuses everything — mosquitto down, or an ACL that does not grant this
    /// account the northbound tree.
    pub fn failing(e: crate::error::Error) -> Self {
        FakePublisher {
            published: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            fail: Mutex::new(Some(e)),
        }
    }

    /// Let a previously failing publisher succeed — the re-run half of the recovery path.
    pub fn recover(&self) {
        *self.fail.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// Everything published so far, in order, as `(topic, payload)`.
    pub fn published(&self) -> Vec<(String, Vec<u8>)> {
        self.published
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Everything published not retained so far, in order, as `(topic, payload)`.
    pub fn events(&self) -> Vec<(String, Vec<u8>)> {
        self.events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The last payload published on `topic`, if any.
    pub fn last_payload(&self, topic: &str) -> Option<Vec<u8>> {
        self.published()
            .into_iter()
            .rfind(|(t, _)| t == topic)
            .map(|(_, p)| p)
    }
}

#[async_trait]
impl crate::northbound::RetainedPublisher for FakePublisher {
    async fn publish_retained(&self, messages: &[(String, Vec<u8>)]) -> Result<()> {
        if let Some(e) = self.fail.lock().unwrap_or_else(|p| p.into_inner()).clone() {
            return Err(e);
        }
        self.published
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .extend_from_slice(messages);
        Ok(())
    }

    async fn publish_events(&self, messages: &[(String, Vec<u8>)]) -> Result<()> {
        if let Some(e) = self.fail.lock().unwrap_or_else(|p| p.into_inner()).clone() {
            return Err(e);
        }
        self.events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .extend_from_slice(messages);
        Ok(())
    }
}

#[async_trait]
impl crate::lorawan::gateway::RetainedTopics for FakeRetainedTopics {
    async fn retained_topic_names(
        &self,
        filter: &str,
        _timeout: std::time::Duration,
    ) -> Result<Vec<String>> {
        if let Some(e) = self.fail.lock().unwrap_or_else(|p| p.into_inner()).clone() {
            return Err(e);
        }
        // Apply the MQTT single-level wildcard so the fake exercises the same filter the real
        // client subscribes with, rather than returning everything regardless.
        let want: Vec<&str> = filter.split('/').collect();
        Ok(self
            .topics
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|t| {
                let got: Vec<&str> = t.split('/').collect();
                got.len() == want.len() && got.iter().zip(&want).all(|(g, w)| *w == "+" || g == w)
            })
            .cloned()
            .collect())
    }
}
