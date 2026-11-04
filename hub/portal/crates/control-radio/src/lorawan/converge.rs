//! `create` / `wipe` / `clear` — `plan/control/radio/lorawan.md` §5 and `plan.md` §5.
//!
//! **Get-then-create per resource; existing resources are left in place, not updated.**
//! `status` reports drift instead, and `create` twice is a no-op — a CI assertion, and a unit
//! assertion here.
//!
//! The rationale is qualified and worth keeping in view: users have no ChirpStack access, so
//! the only out-of-band writes are rare support-tunnel interventions. Never-update therefore
//! protects a support edit from silent reversal, nothing more — which is also why drift is an
//! anomaly indicator in the portal rather than something this code repairs, and why issue #36
//! (no update path after the first `create`) stays open.
//!
//! Two things `create` must never do, both of which are structural here rather than
//! remembered:
//!
//! - **Create a tenant.** It resolves the first one, which is the default a fresh ChirpStack
//!   ships. `wipe` therefore must not delete it — asserted.
//! - **Attach a payload codec.** [`DeviceProfileSpec`] has no codec field at all, so `NONE` is
//!   not a value passed but the only thing expressible.

use crate::config::RadioConfig;
use crate::error::{Component, Error, Kind, Result, Transport};
use crate::lorawan::bootstrap::Bootstrap;
use crate::lorawan::client::ChirpStack;
use crate::lorawan::eui::Eui;
use crate::lorawan::gateway::ApiStatus;
use crate::lorawan::model::{AbpProfileParams, DeviceProfileSpec, GatewaySpec};
use crate::lorawan::state::{Activation, ConvergedState};
use crate::lorawan::token::{Presence, Tokens};

/// Whether a gateway is registered yet. Not a bool, because "pending" is a real third state:
/// converge completed, no radio was present, and that is a degraded success rather than a
/// failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(untagged)]
pub enum GatewayState {
    Registered(bool),
    Pending(&'static str),
}

impl GatewayState {
    pub const PENDING: GatewayState = GatewayState::Pending("pending");

    pub fn is_registered(self) -> bool {
        matches!(self, GatewayState::Registered(true))
    }
}

/// The `converged` block of `status --json`'s `lorawan` object.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Converged {
    pub tenant: bool,
    pub application: bool,
    /// A key per named `device_profiles.{otaa,abp}.<name>` entry — `create` converges however
    /// many profiles a fleet defines, not always exactly two, so this replaces the old fixed
    /// `profile_otaa`/`profile_abp` booleans.
    pub profiles: ProfilesConverged,
    pub gateway: GatewayState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct ProfilesConverged {
    pub otaa: std::collections::BTreeMap<String, bool>,
    pub abp: std::collections::BTreeMap<String, bool>,
}

/// The whole `"lorawan": {...}` block — the one shape `status`, `create`, `wipe`, `clear` and
/// `--dry-run` all return. A dry run is not a third response shape.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LorawanReport {
    pub tokens: std::collections::BTreeMap<&'static str, Presence>,
    /// Whether the ChirpStack API answered, and if not, how it failed. Always present, so a
    /// consumer never has to infer "the API was fine" from the absence of a key. A rejected
    /// token and an unreachable service are separate values because they are separate
    /// operator actions — conflating them is the v1 defect issue #8 names.
    pub api_status: ApiStatus,
    /// Why the API poll failed, when it did. Absent on the success path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_error: Option<String>,
    pub converged: Converged,
    /// Mirrors the per-arm boot marker: `false` means converge completed but the marker was
    /// withheld — no gateway yet.
    pub provisioned: bool,
    /// Resources whose live state differs from the declared config. Reported, never repaired.
    pub drift: Vec<Drift>,
}

impl LorawanReport {
    /// What is still knowable when ChirpStack cannot be asked: the token files and the boot
    /// marker are on disk. Nothing is claimed as converged, because nothing was checked.
    pub fn degraded(
        tokens: std::collections::BTreeMap<&'static str, Presence>,
        provisioned: bool,
        api_status: ApiStatus,
        why: String,
    ) -> Self {
        LorawanReport {
            tokens,
            api_status,
            api_error: Some(why),
            converged: Converged {
                tenant: false,
                application: false,
                profiles: Default::default(),
                gateway: GatewayState::PENDING,
            },
            provisioned,
            drift: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Drift {
    pub resource: String,
    pub declared: String,
    pub actual: String,
}

pub struct Converger<'a> {
    pub client: &'a dyn ChirpStack,
    pub config: &'a RadioConfig,
    pub tokens: &'a Tokens,
    pub store: &'a crate::store::Store,
}

impl Converger<'_> {
    /// Converge the declared object set. Idempotent; safe to re-run at any time.
    ///
    /// `gateway_eui` is the discovered concentrator EUI, or `None` when no radio answered —
    /// which is a degraded success, not a failure: converge completes, the gateway reads
    /// `pending`, the command exits 0 so the health gate passes, and the `provisioned` marker
    /// is **withheld** so the next boot re-converges.
    pub async fn create(&self, gateway_eui: Option<&Eui>, dry_run: bool) -> Result<LorawanReport> {
        let mut state = self.load_state()?;

        let tenant_id = self.resolve_tenant().await?;
        state.tenant_id = Some(tenant_id.clone());

        // §4 steps 3-4: the tenant-scoped tokens cannot exist before the tenant id does, so
        // they are minted here — after resolution, before the first call that needs one.
        // `create` is the only command that mints; everywhere else an absent token stays
        // `NotBootstrapped` (exit 10), because a `node-add` that quietly acquired credentials
        // would hide a Hub that never converged.
        //
        // Not on a dry run: `status` reaches this same function with `dry_run: true`, and a
        // read-only report must not mint a credential as a side effect.
        if !dry_run {
            Bootstrap::new(self.client, self.tokens)
                .mint_missing(&tenant_id)
                .await?;
        }

        let (application_id, app_drift) = self.converge_application(&tenant_id, dry_run).await?;
        state.application_id = application_id.clone();

        let profiles = self
            .converge_profiles(&tenant_id, &mut state, dry_run)
            .await?;

        let gateway = self
            .converge_gateway(&tenant_id, gateway_eui, &mut state, dry_run)
            .await?;
        if gateway.is_registered() {
            if let Some(eui) = gateway_eui {
                state.gateway_id = Some(eui.clone());
            }
        }

        if !dry_run {
            self.save_state(&state)?;
            // The marker records "a gateway was actually registered", not "converge ran".
            // Withholding it on degraded-green is what makes the next boot try again.
            if gateway.is_registered() {
                self.store.mark_provisioned(Transport::Lorawan)?;
            }
        }

        Ok(LorawanReport {
            tokens: self.tokens.presence_report(),
            api_status: ApiStatus::Ok,
            api_error: None,
            converged: Converged {
                tenant: true,
                application: application_id.is_some(),
                profiles,
                gateway,
            },
            provisioned: self.store.is_provisioned(Transport::Lorawan),
            drift: app_drift,
        })
    }

    /// Delete everything this component provisioned — devices, gateway, profiles, application.
    ///
    /// The tenant survives: it was resolved, not created. `create` is required afterwards.
    pub async fn wipe(&self) -> Result<LorawanReport> {
        let state = self.load_state()?;
        let tenant_id = self.resolve_tenant().await?;

        self.delete_devices(&state).await?;

        if let Some(eui) = &state.gateway_id {
            self.tolerate_missing(self.client.delete_gateway(eui).await)?;
        }
        for profile_id in state
            .device_profile_ids
            .otaa
            .values()
            .chain(state.device_profile_ids.abp.values())
        {
            self.tolerate_missing(self.client.delete_device_profile(profile_id).await)?;
        }
        if let Some(app) = &state.application_id {
            self.tolerate_missing(self.client.delete_application(app).await)?;
        }

        self.store.clear_provisioned(Transport::Lorawan)?;
        self.drop_enrollment_records()?;
        // Tenant resolution is forgotten with the rest; the tenant object itself is untouched.
        let cleared = ConvergedState::default();
        self.save_state(&cleared)?;

        Ok(LorawanReport {
            tokens: self.tokens.presence_report(),
            api_status: ApiStatus::Ok,
            api_error: None,
            converged: Converged {
                tenant: !tenant_id.is_empty(),
                application: false,
                profiles: ProfilesConverged::default(),
                gateway: GatewayState::Registered(false),
            },
            provisioned: false,
            drift: Vec::new(),
        })
    }

    /// Delete only the enrolled devices. Application, profiles and gateway registration are
    /// untouched, and so is `config.json` — a `create` after this is a no-op.
    pub async fn clear(&self) -> Result<LorawanReport> {
        let state = self.load_state()?;
        self.delete_devices(&state).await?;
        self.drop_enrollment_records()?;

        Ok(LorawanReport {
            tokens: self.tokens.presence_report(),
            api_status: ApiStatus::Ok,
            api_error: None,
            converged: Converged {
                tenant: state.tenant_id.is_some(),
                application: state.application_id.is_some(),
                profiles: ProfilesConverged {
                    otaa: state
                        .device_profile_ids
                        .otaa
                        .keys()
                        .map(|k| (k.clone(), true))
                        .collect(),
                    abp: state
                        .device_profile_ids
                        .abp
                        .keys()
                        .map(|k| (k.clone(), true))
                        .collect(),
                },
                gateway: GatewayState::Registered(state.gateway_id.is_some()),
            },
            provisioned: self.store.is_provisioned(Transport::Lorawan),
            drift: Vec::new(),
        })
    }

    async fn delete_devices(&self, state: &ConvergedState) -> Result<()> {
        let Some(app) = &state.application_id else {
            return Ok(());
        };
        for device in self.client.list_devices(app).await? {
            self.tolerate_missing(self.client.delete_device(&device.dev_eui).await)?;
        }
        Ok(())
    }

    /// Resolve the tenant — take the first, never create one.
    ///
    /// Public because `gateway-add` needs the tenant without running a whole converge.
    pub async fn resolve_tenant_id(&self) -> Result<String> {
        self.resolve_tenant().await
    }

    async fn resolve_tenant(&self) -> Result<String> {
        let tenants = self.client.list_tenants().await?;
        tenants.into_iter().next().map(|t| t.id).ok_or_else(|| {
            Error::new(
                Kind::NotFound,
                Transport::Lorawan,
                Component::ChirpstackApi,
                "ChirpStack reports no tenant at all; a fresh v4 install ships a default one, \
                 so its database is not in a state this can converge against",
            )
        })
    }

    async fn converge_application(
        &self,
        tenant_id: &str,
        dry_run: bool,
    ) -> Result<(Option<String>, Vec<Drift>)> {
        let declared = &self.config.application.name;
        let existing = self.client.list_applications(tenant_id).await?;

        if let Some(found) = existing.iter().find(|a| a.name == *declared) {
            return Ok((Some(found.id.clone()), Vec::new()));
        }

        // Nothing by that name. If exactly one application exists under a different name, it
        // is a support edit, not an absence: report it as drift and adopt it rather than
        // creating a second one alongside.
        if let [only] = existing.as_slice() {
            return Ok((
                Some(only.id.clone()),
                vec![Drift {
                    resource: "application".to_string(),
                    declared: declared.clone(),
                    actual: only.name.clone(),
                }],
            ));
        }

        if dry_run {
            return Ok((None, Vec::new()));
        }
        let id = self
            .client
            .create_application(tenant_id, declared, &self.config.application.description)
            .await?;
        Ok((Some(id), Vec::new()))
    }

    async fn converge_profiles(
        &self,
        tenant_id: &str,
        state: &mut ConvergedState,
        dry_run: bool,
    ) -> Result<ProfilesConverged> {
        let existing = self.client.list_device_profiles(tenant_id).await?;
        let mut out = ProfilesConverged::default();

        for (name, p) in &self.config.device_profiles.otaa {
            let spec = DeviceProfileSpec {
                tenant_id: tenant_id.to_string(),
                name: p.name.clone(),
                description: p.description.clone(),
                region: self.config.region.as_str().to_string(),
                mac_version: format!("{:?}", p.mac_version),
                reg_params_revision: format!("{:?}", p.reg_params_revision),
                adr_algorithm_id: p.adr_algorithm_id.clone(),
                flush_queue_on_activate: p.flush_queue_on_activate,
                uplink_interval: p.uplink_interval,
                device_status_req_interval: p.device_status_req_interval,
                supports_otaa: true,
                abp: None,
            };
            let done = self
                .converge_one_profile(&existing, &spec, Activation::Otaa, name, state, dry_run)
                .await?;
            out.otaa.insert(name.clone(), done);
        }

        // Every ABP profile is created from phase 1 on, even though ABP *enrollment* is
        // refused until phase 2: it means ChirpStack never needs a later schema change to
        // support ABP, and `node-add --profile` can then select purely by name.
        for (name, p) in &self.config.device_profiles.abp {
            let spec = DeviceProfileSpec {
                tenant_id: tenant_id.to_string(),
                name: p.name.clone(),
                description: p.description.clone(),
                region: self.config.region.as_str().to_string(),
                mac_version: format!("{:?}", p.mac_version),
                reg_params_revision: format!("{:?}", p.reg_params_revision),
                adr_algorithm_id: p.adr_algorithm_id.clone(),
                flush_queue_on_activate: p.flush_queue_on_activate,
                uplink_interval: p.uplink_interval,
                device_status_req_interval: p.device_status_req_interval,
                supports_otaa: false,
                abp: Some(AbpProfileParams {
                    rx1_delay: p.abp_rx1_delay,
                    rx1_dr_offset: p.abp_rx1_dr_offset,
                    rx2_dr: p.abp_rx2_dr,
                    rx2_freq: p.abp_rx2_freq,
                }),
            };
            let done = self
                .converge_one_profile(&existing, &spec, Activation::Abp, name, state, dry_run)
                .await?;
            out.abp.insert(name.clone(), done);
        }

        Ok(out)
    }

    async fn converge_one_profile(
        &self,
        existing: &[crate::lorawan::model::DeviceProfile],
        spec: &DeviceProfileSpec,
        activation: Activation,
        config_name: &str,
        state: &mut ConvergedState,
        dry_run: bool,
    ) -> Result<bool> {
        if let Some(found) = existing.iter().find(|p| p.name == spec.name) {
            state.set_profile_id(activation, config_name, found.id.clone());
            return Ok(true);
        }
        if dry_run {
            return Ok(false);
        }
        let id = self.client.create_device_profile(spec).await?;
        state.set_profile_id(activation, config_name, id);
        Ok(true)
    }

    async fn converge_gateway(
        &self,
        tenant_id: &str,
        gateway_eui: Option<&Eui>,
        state: &mut ConvergedState,
        dry_run: bool,
    ) -> Result<GatewayState> {
        // No radio: a degraded success. Not an error, and not a reason to fail the boot unit —
        // the qemuarm64 smoke image has no concentrator and must still go green.
        let Some(eui) = gateway_eui.or(state.gateway_id.as_ref()) else {
            return Ok(GatewayState::PENDING);
        };

        // Ask-first through the tenant-scoped list, never `get_gateway`: ChirpStack 4.19.0's
        // EXISTS-join makes `GatewayService.Get` answer `Unauthenticated` for an EUI that is
        // not registered yet under every key below admin — which is exactly this call on a
        // first boot. It surfaced as `create` exiting 11 and never registering the gateway,
        // leaving every uplink dropped (F20, reproduced live 2026-08-25).
        if self.client.find_gateway(tenant_id, eui).await?.is_some() {
            return Ok(GatewayState::Registered(true));
        }
        if dry_run {
            return Ok(GatewayState::PENDING);
        }
        self.client
            .create_gateway(&GatewaySpec {
                gateway_id: eui.clone(),
                tenant_id: tenant_id.to_string(),
                name: self.config.gateway.name.clone(),
                description: self.config.gateway.description.clone(),
            })
            .await?;
        Ok(GatewayState::Registered(true))
    }

    /// Register the local concentrator the health tick saw after `create` left it `pending`
    /// (#156).
    ///
    /// `create` discovers the gateway once, at `radio-setup`; when the forwarder publishes its
    /// `state/conn` only after that read (the factory-reset race), converge ends degraded-green
    /// and nothing retried — ChirpStack then dropped every frame from our own radio. The daemon
    /// sees the retained topic on its next tick and calls this with the EUIs ChirpStack does
    /// not know.
    ///
    /// Does exactly what `create`'s gateway step does — same spec, `gateway_id` persisted to
    /// `config.json`, `provisioned` marker set — and nothing else (no tenant resolution, no
    /// token minting). It acts only when that is unambiguous:
    /// - `create` has run (a tenant is recorded); before that, `create` itself will register it;
    /// - exactly one candidate, as `discover` requires — two local forwarders is a
    ///   misconfiguration, never something to pick from;
    /// - the recorded `gateway_id` is absent or is this EUI, so another gateway already
    ///   adopted is never replaced.
    ///
    /// Returns the EUI it registered, or `None` when it had nothing to do.
    pub async fn adopt_gateway(&self, candidates: &[Eui]) -> Result<Option<Eui>> {
        let [eui] = candidates else {
            return Ok(None);
        };
        let mut state = self.load_state()?;
        let Some(tenant_id) = state.tenant_id.clone().filter(|t| !t.is_empty()) else {
            return Ok(None);
        };
        if state.gateway_id.as_ref().is_some_and(|g| g != eui) {
            return Ok(None);
        }
        let gateway = self
            .converge_gateway(&tenant_id, Some(eui), &mut state, false)
            .await?;
        if !gateway.is_registered() {
            return Ok(None);
        }
        state.gateway_id = Some(eui.clone());
        self.save_state(&state)?;
        self.store.mark_provisioned(Transport::Lorawan)?;
        Ok(Some(eui.clone()))
    }

    /// Forget a gateway registration locally — the mirror of the `create` path that records
    /// one. Only clears state that names *this* EUI, so removing some other Hub's external
    /// gateway does not deprovision ours.
    ///
    /// The boot marker goes with it: lorawan.md §6 writes it only when a gateway is actually
    /// registered, because without one ChirpStack drops every uplink. Leaving the marker
    /// behind would tell the next boot it had nothing to do.
    pub fn forget_gateway(&self, eui: &Eui) -> Result<()> {
        let mut state = self.load_state()?;
        if state.gateway_id.as_ref() == Some(eui) {
            state.gateway_id = None;
            self.save_state(&state)?;
            self.store.clear_provisioned(Transport::Lorawan)?;
        }
        Ok(())
    }

    /// Drop `nodes.json` alongside the devices it describes.
    ///
    /// The records map serial to dev_eui and carry the OTAA root key in plaintext. Once the
    /// devices are deleted they describe nothing, and keeping a root key for a device that no
    /// longer exists is the worst of both — plan.md §5 has `clear`/`wipe` dropping the node
    /// cache, and lorawan.md §4 puts `nodes.json` in the same component state.
    ///
    /// The registry's LoRaWAN Nodes go with it (node_registry.md): `nodes.json` is only their
    /// mirror, and a Node left there would reappear in the next load.
    fn drop_enrollment_records(&self) -> Result<()> {
        crate::registry::drop_carrier(self.store, Transport::Lorawan)?;
        self.store.remove(crate::store::Slot::LorawanNodes)
    }

    /// `pub(crate)`: `dispatch::node_add` reads the already-converged state to enroll against
    /// it — enrollment must never itself re-converge, only `create` does that.
    pub(crate) fn load_state(&self) -> Result<ConvergedState> {
        match self.store.read(crate::store::Slot::LorawanConfig)? {
            Some(raw) => ConvergedState::from_json(&raw),
            None => Ok(ConvergedState::default()),
        }
    }

    fn save_state(&self, state: &ConvergedState) -> Result<()> {
        self.store
            .write(crate::store::Slot::LorawanConfig, &state.to_json()?)
    }

    /// `NotFound` on a delete is success, as in v1 — and unlike a duplicate `Create`, this one
    /// really is a `Status::NOT_FOUND` and safe to branch on.
    fn tolerate_missing(&self, r: Result<()>) -> Result<()> {
        match r {
            Err(e) if e.kind() == Kind::NotFound => Ok(()),
            other => other,
        }
    }
}
