//! `create`/`wipe`/`clear` against the in-memory fake — `lorawan.md` §5, §10.
//!
//! The properties asserted here are the ones a real ChirpStack would only reveal after a
//! painful field failure: that a second `create` writes nothing, that a missing radio is a
//! degraded success rather than an error, and that `wipe` cannot delete the tenant.

use control_radio::config::RadioConfig;
use control_radio::lorawan::client::ChirpStack;
use control_radio::lorawan::converge::{Converger, GatewayState};
use control_radio::lorawan::fake::{FakeChirpStack, DEFAULT_TENANT_ID};
use control_radio::lorawan::token::Presence;
use control_radio::lorawan::{Activation, ConvergedState, Eui, Scope, Tokens};
use control_radio::store::{Slot, Store};
use control_radio::{Kind, Transport};

const PLAN_EXAMPLE: &str = include_str!("fixtures/radio-section.yaml");

struct Fixture {
    fake: FakeChirpStack,
    config: RadioConfig,
    tokens: Tokens,
    store: Store,
    _dir: TempDir,
}

/// A self-removing temp directory; the crate deliberately has no `tempfile` dependency to
/// keep its dependency surface reviewable.
struct TempDir(std::path::PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture(tag: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!(
        "control-radio-converge-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::new(&dir);
    let tokens = Tokens::new(store.clone());
    for scope in Scope::ALL {
        tokens.store(scope, "jwt").expect("seeds a token");
    }
    Fixture {
        fake: FakeChirpStack::new(),
        config: RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("the plan's example"),
        tokens,
        store,
        _dir: TempDir(dir),
    }
}

impl Fixture {
    fn converger(&self) -> Converger<'_> {
        Converger {
            client: &self.fake,
            config: &self.config,
            tokens: &self.tokens,
            store: &self.store,
        }
    }
    fn state(&self) -> ConvergedState {
        let raw = self
            .store
            .read(Slot::LorawanConfig)
            .expect("readable")
            .expect("config.json written");
        ConvergedState::from_json(&raw).expect("parses")
    }
}

fn eui() -> Eui {
    Eui::parse("0011223344556677").expect("valid")
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

/// The headline invariant: `create` twice is a no-op, with stable ids.
#[test]
fn create_is_idempotent_and_the_second_run_writes_nothing() {
    let f = fixture("idempotent");
    rt().block_on(async {
        let first = f
            .converger()
            .create(Some(&eui()), false)
            .await
            .expect("first create");
        assert!(first.converged.tenant);
        assert!(first.converged.application);
        assert_eq!(first.converged.gateway, GatewayState::Registered(true));
        assert!(
            first.provisioned,
            "a registered gateway must set the marker"
        );

        let ids_after_first = f.state();

        f.fake.clear_calls();
        let second = f
            .converger()
            .create(Some(&eui()), false)
            .await
            .expect("second create");

        assert_eq!(
            f.fake.writes(),
            Vec::<String>::new(),
            "a converged create must make no writes at all"
        );
        assert_eq!(second.converged, first.converged, "same reported state");
        assert_eq!(f.state(), ids_after_first, "ids must be stable across runs");

        // And nothing was duplicated server-side.
        assert_eq!(f.fake.application_count(), 1);
        assert_eq!(f.fake.profile_count(), 2, "one OTAA + one ABP profile");
        assert_eq!(f.fake.gateway_count(), 1);
    });
}

/// Both profiles are created in phase 1 — ABP's ChirpStack side stays prepared even though
/// ABP enrollment is refused until phase 2.
#[test]
fn both_activation_profiles_are_created_and_recorded() {
    let f = fixture("profiles");
    rt().block_on(async {
        let r = f
            .converger()
            .create(Some(&eui()), false)
            .await
            .expect("create");
        assert_eq!(r.converged.profiles.otaa.get("default"), Some(&true));
        assert_eq!(r.converged.profiles.abp.get("default"), Some(&true));

        let s = f.state();
        assert!(s.profile_id(Activation::Otaa, "default").is_some());
        assert!(s.profile_id(Activation::Abp, "default").is_some());
        // The legacy top-level keys carry the `default` profile's id, so a bench file still
        // round-trips through v1's shape.
        assert_eq!(
            s.device_profile_id_otaa.as_deref(),
            s.profile_id(Activation::Otaa, "default")
        );
    });
}

/// No radio is a degraded success: converge completes, the gateway reads `pending`, and the
/// marker is withheld so the next boot re-converges.
#[test]
fn no_radio_is_degraded_green_and_withholds_the_marker() {
    let f = fixture("degraded");
    rt().block_on(async {
        let r = f
            .converger()
            .create(None, false)
            .await
            .expect("must not fail without a radio");

        assert_eq!(r.converged.gateway, GatewayState::PENDING);
        assert!(r.converged.application, "everything else still converged");
        assert!(
            !r.provisioned,
            "the marker records a registered gateway, not a completed run"
        );
        assert!(!f.store.is_provisioned(Transport::Lorawan));
        assert_eq!(f.fake.gateway_count(), 0);

        // The pending state serializes as the documented string, not as `false`.
        let json = serde_json::to_value(&r).expect("serializes");
        assert_eq!(json["converged"]["gateway"], "pending");

        // A later run that does find a radio completes the job and sets the marker.
        let r2 = f
            .converger()
            .create(Some(&eui()), false)
            .await
            .expect("second create");
        assert_eq!(r2.converged.gateway, GatewayState::Registered(true));
        assert!(f.store.is_provisioned(Transport::Lorawan));
    });
}

/// §3's hard invariant: the tenant was resolved, not created, so `wipe` must not delete it.
#[test]
fn wipe_removes_everything_it_created_and_never_the_tenant() {
    let f = fixture("wipe");
    rt().block_on(async {
        f.converger()
            .create(Some(&eui()), false)
            .await
            .expect("create");
        assert_eq!(f.fake.tenant_count(), 1);

        let r = f.converger().wipe().await.expect("wipe");
        assert_eq!(f.fake.tenant_count(), 1, "the tenant must survive wipe");
        assert_eq!(f.fake.application_count(), 0);
        assert_eq!(f.fake.profile_count(), 0);
        assert_eq!(f.fake.gateway_count(), 0);
        assert!(!r.converged.application);
        assert!(r.converged.tenant, "the tenant is still there to report");
        assert!(!f.store.is_provisioned(Transport::Lorawan));

        // No delete_tenant call was ever made — the trait does not even offer one.
        assert!(!f
            .fake
            .calls()
            .iter()
            .any(|c| c.contains("tenant") && c.starts_with("delete")));

        // And `create` after `wipe` rebuilds from scratch.
        let again = f
            .converger()
            .create(Some(&eui()), false)
            .await
            .expect("re-create");
        assert!(again.converged.application);
        assert_eq!(f.fake.application_count(), 1);
    });
}

/// `clear` deletes devices only; the provisioning objects and `config.json` survive, so a
/// following `create` is a no-op.
#[test]
fn clear_removes_devices_only() {
    let f = fixture("clear");
    rt().block_on(async {
        f.converger()
            .create(Some(&eui()), false)
            .await
            .expect("create");
        let app = f.state().application_id.expect("app id");

        for n in 0..3u8 {
            let dev = Eui::parse(&format!("00112233445566{n:02x}")).expect("valid");
            f.fake
                .create_device(&control_radio::lorawan::model::DeviceSpec {
                    dev_eui: dev,
                    application_id: app.clone(),
                    device_profile_id: "p".into(),
                    name: format!("n{n}"),
                    description: String::new(),
                    join_eui: None,
                })
                .await
                .expect("seeds a device");
        }
        assert_eq!(f.fake.device_count(), 3);

        let r = f.converger().clear().await.expect("clear");
        assert_eq!(f.fake.device_count(), 0, "devices gone");
        assert_eq!(f.fake.application_count(), 1, "application survives");
        assert_eq!(f.fake.profile_count(), 2, "profiles survive");
        assert_eq!(f.fake.gateway_count(), 1, "gateway registration survives");
        assert!(r.converged.application);

        f.fake.clear_calls();
        f.converger()
            .create(Some(&eui()), false)
            .await
            .expect("create after clear");
        assert_eq!(f.fake.writes(), Vec::<String>::new(), "still converged");
    });
}

/// `--dry-run` reports the same shape and writes nothing — on disk or server-side.
#[test]
fn dry_run_reports_without_writing() {
    let f = fixture("dryrun");
    rt().block_on(async {
        let r = f
            .converger()
            .create(Some(&eui()), true)
            .await
            .expect("dry run");

        assert_eq!(
            f.fake.writes(),
            Vec::<String>::new(),
            "no server-side writes"
        );
        assert!(
            !f.store.exists(Slot::LorawanConfig),
            "no config.json written"
        );
        assert!(!f.store.is_provisioned(Transport::Lorawan));
        // Same envelope shape as a real create — not a third response shape.
        let json = serde_json::to_value(&r).expect("serializes");
        assert!(json.get("converged").is_some());
        assert!(json.get("tokens").is_some());
        assert!(json.get("provisioned").is_some());

        // Reports what is missing, since nothing exists yet.
        assert!(!r.converged.application);
        assert_eq!(r.converged.profiles.otaa.get("default"), Some(&false));
    });
}

/// An out-of-band support edit is reported as drift, never silently reverted.
#[test]
fn a_renamed_application_is_reported_as_drift_not_repaired() {
    let f = fixture("drift");
    rt().block_on(async {
        f.converger()
            .create(Some(&eui()), false)
            .await
            .expect("create");
        let app = f.state().application_id.expect("app id");

        f.fake.rename_application(&app, "Renamed By Support");

        f.fake.clear_calls();
        let r = f
            .converger()
            .create(Some(&eui()), false)
            .await
            .expect("re-create");

        assert_eq!(
            f.fake.writes(),
            Vec::<String>::new(),
            "drift is never repaired"
        );
        assert_eq!(
            f.fake.application_count(),
            1,
            "and no second application is created"
        );
        assert_eq!(r.drift.len(), 1);
        assert_eq!(r.drift[0].resource, "application");
        assert_eq!(r.drift[0].actual, "Renamed By Support");
    });
}

/// The measured 4.19.1 behaviour: a duplicate `Create` returns `Internal` with a raw storage
/// error, not `AlreadyExists`. Converge must never reach it — it asks first.
#[test]
fn converge_never_relies_on_the_duplicate_create_error() {
    let f = fixture("askfirst");
    rt().block_on(async {
        f.converger()
            .create(Some(&eui()), false)
            .await
            .expect("create");
        f.fake.clear_calls();
        f.converger()
            .create(Some(&eui()), false)
            .await
            .expect("second create must not error");

        let calls = f.fake.calls();
        // Every resource was read before any decision to write.
        assert!(calls.contains(&"list_applications".to_string()));
        assert!(calls.contains(&"list_device_profiles".to_string()));
        // The gateway ask-first is the tenant-scoped `find_gateway`, never `get_gateway`:
        // 4.19.0's EXISTS-join answers `Unauthenticated` for a not-yet-registered EUI under
        // every key below admin, so a `Get` here fails a first boot outright (F20).
        assert!(calls.contains(&"find_gateway".to_string()));
        assert!(
            !calls.contains(&"get_gateway".to_string()),
            "the existence check must not use Get: {calls:?}"
        );
    });

    // And if something did call Create twice, the error is Transport, not AlreadyExists —
    // which is why branching on it was never an option.
    let f2 = fixture("dupe");
    rt().block_on(async {
        f2.fake
            .create_gateway(&control_radio::lorawan::model::GatewaySpec {
                gateway_id: eui(),
                tenant_id: DEFAULT_TENANT_ID.into(),
                name: "g".into(),
                description: String::new(),
            })
            .await
            .expect("first");
        let err = f2
            .fake
            .create_gateway(&control_radio::lorawan::model::GatewaySpec {
                gateway_id: eui(),
                tenant_id: DEFAULT_TENANT_ID.into(),
                name: "g".into(),
                description: String::new(),
            })
            .await
            .expect_err("duplicate");
        assert_eq!(
            err.kind(),
            Kind::Transport,
            "not AlreadyExists — this is the measured behaviour"
        );
        assert!(err.message().contains("duplicate key value"));
    });
}

/// A rejected token surfaces as `Unauthenticated`, never as an empty gateway list — the v1
/// defect issue #8 names.
#[test]
fn a_rejected_token_is_unauthenticated_never_an_empty_gateway_list() {
    let f = fixture("authfail");
    rt().block_on(async {
        f.fake.fail_every_call_with(
            chirpstack_api::tonic::Code::Unauthenticated,
            "api key not found",
        );
        let err = f
            .converger()
            .create(Some(&eui()), false)
            .await
            .expect_err("must fail");
        assert_eq!(err.kind(), Kind::Unauthenticated);
        assert_eq!(err.exit_code(), 11);
        assert!(
            !err.kind().is_retryable(),
            "an auth failure must not be retried at boot"
        );
    });
}

/// A stopped ChirpStack is `ServiceDown`, exit 20, and retryable — the band the boot oneshot
/// waits on while the DB migrates on first boot.
#[test]
fn a_stopped_service_is_service_down_and_retryable() {
    let f = fixture("down");
    rt().block_on(async {
        f.fake.fail_every_call_with(
            chirpstack_api::tonic::Code::Unavailable,
            "connection refused",
        );
        let err = f
            .converger()
            .create(Some(&eui()), false)
            .await
            .expect_err("must fail");
        assert_eq!(err.kind(), Kind::ServiceDown);
        assert_eq!(err.exit_code(), 20);
        assert!(err.kind().is_retryable());
    });
}

/// F9, seen on the bench: after `clear --force` deleted the device from ChirpStack,
/// `nodes.json` still held its serial, dev_eui **and plaintext `app_key`**. The records
/// describe devices that no longer exist, and the root key is the one secret in the file.
/// plan.md §5 has `clear`/`wipe` dropping the node cache; lorawan.md §4 puts `nodes.json` in
/// the same component state.
#[test]
fn wipe_and_clear_drop_the_enrollment_records() {
    for op in ["wipe", "clear"] {
        let f = fixture(&format!("drop-nodes-{op}"));
        rt().block_on(async {
            f.converger()
                .create(Some(&eui()), false)
                .await
                .expect("create");
            f.store
                .write(
                    control_radio::store::Slot::LorawanNodes,
                    r#"{"SN1":{"serial_number":"SN1","dev_eui":"58760700c0668fc7","profile":"default","activation":"otaa","app_key":"00112233445566778899aabbccddeeff"}}"#,
                )
                .expect("seed a record");
            assert!(f.store.exists(control_radio::store::Slot::LorawanNodes));

            match op {
                "wipe" => {
                    f.converger().wipe().await.expect("wipe");
                }
                _ => {
                    f.converger().clear().await.expect("clear");
                }
            }

            let left = f
                .store
                .read(control_radio::store::Slot::LorawanNodes)
                .expect("read")
                .unwrap_or_default();
            assert!(
                !left.contains("00112233445566778899aabbccddeeff"),
                "{op} left a plaintext app_key behind: {left}"
            );
            assert!(
                !left.contains("SN1"),
                "{op} left a stale enrollment record behind: {left}"
            );
        });
    }
}

/// WP-A's call site: the tenant/read-only tokens are minted **inside `create`**, after tenant
/// resolution and before `ApplicationService.Create` (`lorawan.md` §4 steps 3-5).
///
/// Asserted through the refusal rather than the mint, because the mint needs a password file
/// this test deliberately does not have: `create` gets as far as `list_tenants`, then stops.
/// Nothing is created, which is the ordering claim.
#[test]
fn create_mints_the_tenant_tokens_before_it_creates_the_application() {
    if control_radio::lorawan::bootstrap::admin_password_path().exists() {
        // A Hub, or a shell with the compose override exported. The mint would succeed and
        // there would be no refusal to observe.
        return;
    }
    let f = fixture("mint-call-site");
    f.tokens.remove(Scope::Tenant).expect("removes");
    f.tokens.remove(Scope::ReadOnly).expect("removes");

    rt().block_on(async {
        let err = f
            .converger()
            .create(Some(&eui()), false)
            .await
            .expect_err("no admin password, no tokens, no converge");
        assert_eq!(err.kind(), Kind::NotBootstrapped);
        assert_eq!(err.exit_code(), 10);
        assert_eq!(
            f.fake.calls(),
            vec!["list_tenants"],
            "the mint sits between tenant resolution and the application create"
        );
        assert_eq!(f.fake.application_count(), 0);
    });
}

/// `status` reaches the same function with `dry_run: true`. A read-only report must not mint
/// a credential as a side effect — nor fail because it could not.
#[test]
fn a_dry_run_never_mints_anything() {
    let f = fixture("mint-dry-run");
    f.tokens.remove(Scope::Tenant).expect("removes");
    f.tokens.remove(Scope::ReadOnly).expect("removes");

    rt().block_on(async {
        f.converger()
            .create(Some(&eui()), true)
            .await
            .expect("a dry run reports, it does not provision");
        assert!(
            !f.fake.calls().iter().any(|c| c == "login"),
            "a dry run must not log in: {:?}",
            f.fake.calls()
        );
        assert_eq!(f.tokens.presence(Scope::Tenant), Presence::Absent);
        assert_eq!(f.tokens.presence(Scope::ReadOnly), Presence::Absent);
    });
}

/// #156: `radio-setup` raced the forwarder after a factory reset — discovery found nothing,
/// the gateway stayed `pending`, and nothing registered it later, so ChirpStack dropped every
/// frame. The health tick sees the retained `state/conn`, and adopting it registers the
/// gateway exactly as `create` would: id persisted, marker set.
#[test]
fn the_health_tick_registers_a_gateway_create_left_pending() {
    use control_radio::lorawan::fake::FakeRetainedTopics;
    use control_radio::tasks::health::{HealthPoller, Inputs};

    let f = fixture("adopt");
    rt().block_on(async {
        let r = f.converger().create(None, false).await.expect("degraded");
        assert_eq!(r.converged.gateway, GatewayState::PENDING);

        let topics = FakeRetainedTopics::with(&["eu868/gateway/0011223344556677/state/conn"]);
        let tenant = f.state().tenant_id.expect("create recorded the tenant");
        let mut poller = HealthPoller::new();
        let inputs = || Inputs {
            client: Some(&f.fake as &dyn ChirpStack),
            tenant_id: Some(&tenant),
            topics: Some(&topics),
            prefix: "eu868",
            gateway_name: "ProXimos Gateway",
        };
        poller.tick(inputs()).await;
        assert_eq!(poller.unregistered(), vec![eui()]);

        let adopted = f
            .converger()
            .adopt_gateway(&poller.unregistered())
            .await
            .expect("registers");
        assert_eq!(adopted, Some(eui()));
        assert_eq!(f.fake.gateway_count(), 1);
        assert_eq!(f.state().gateway_id, Some(eui()));
        assert!(f.store.is_provisioned(Transport::Lorawan));

        // The next tick finds it in ChirpStack, so there is nothing left to adopt.
        poller.tick(inputs()).await;
        assert!(poller.unregistered().is_empty());
    });
}

/// Adoption never picks between two forwarders, never replaces a different recorded gateway,
/// and does nothing before `create` has resolved a tenant.
#[test]
fn adopting_a_gateway_only_acts_when_unambiguous() {
    let other = Eui::parse("24e124fffefd3bda").expect("valid");
    let f = fixture("adopt-guards");
    rt().block_on(async {
        // Before `create`: no tenant recorded.
        assert_eq!(
            f.converger().adopt_gateway(&[eui()]).await.expect("ok"),
            None
        );
        f.converger()
            .create(Some(&eui()), false)
            .await
            .expect("create");
        // Two candidates: ambiguous.
        assert_eq!(
            f.converger()
                .adopt_gateway(&[eui(), other.clone()])
                .await
                .expect("ok"),
            None
        );
        // A different gateway is already ours.
        assert_eq!(
            f.converger().adopt_gateway(&[other]).await.expect("ok"),
            None
        );
        assert_eq!(f.fake.gateway_count(), 1);
    });
}
