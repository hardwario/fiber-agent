//! Dispatch through the shipped `run` path with an injected store — the seam the CLI-level
//! behaviour tests need (status while ChirpStack is down, node-remove, gateway-remove).

use clap::Parser;
use control_radio::cache::{Cache, CacheSlot};
use control_radio::cli::{Command, ControlRadioCli};
use control_radio::config::RadioConfig;
use control_radio::dispatch::{run_with_store, Outcome};
use control_radio::error::Kind;
use control_radio::lorawan::client::ChirpStack;
use control_radio::lorawan::fake::{FakeChirpStack, FakePublisher};
use control_radio::lorawan::gateway::ApiStatus;
use control_radio::lorawan::token::Presence;
use control_radio::lorawan::{Scope, Tokens};
use control_radio::northbound::{Area, RetainedPublisher, Topics};
use control_radio::store::Store;

const PLAN_EXAMPLE: &str = include_str!("fixtures/radio-section.yaml");

struct TempDir(std::path::PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

/// A store with all three tokens seeded, in its own temp dir.
fn seeded_store(tag: &str) -> (Store, TempDir) {
    let dir = std::env::temp_dir().join(format!(
        "control-radio-dispatch-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::new(&dir);
    let tokens = Tokens::new(store.clone());
    for s in Scope::ALL {
        tokens.store(s, "jwt").expect("seed");
    }
    (store, TempDir(dir))
}

fn cli(args: &[&str]) -> ControlRadioCli {
    let mut full = vec!["control.radio"];
    full.extend_from_slice(args);
    ControlRadioCli::try_parse_from(full).expect("parses")
}

/// The retained `info` topic for one node on this host — what `node-add` publishes and
/// `node-remove` tombstones. Built from the shipped helpers rather than a literal, so a change
/// to the topic family shows up in `northbound.rs`'s own tests, not as a puzzle here.
fn info_topic(dev_eui: &str) -> String {
    Topics::new(control_radio::northbound::hostname()).node(dev_eui, Area::Info)
}

/// Every retained topic `node-remove` must clear for one node — `info`, `device_info` and
/// `config` (node_reported_state.md §2) — each as an empty retained payload. From the shipped
/// helper, for the same reason as [`info_topic`].
fn tombstones(dev_eui: &str) -> Vec<(String, Vec<u8>)> {
    let stones: Vec<(String, Vec<u8>)> = Topics::new(control_radio::northbound::hostname())
        .tombstones(dev_eui)
        .into_iter()
        .map(|t| (t, Vec::new()))
        .collect();
    assert!(
        stones.iter().any(|(t, _)| *t == info_topic(dev_eui)),
        "the inventory topic is always among them"
    );
    stones
}

#[test]
fn status_runs_through_the_injected_store() {
    let (store, _dir) = seeded_store("status");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    let cli = cli(&["status"]);

    let outcome = rt()
        .block_on(run_with_store(
            &cli.inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect("status succeeds");

    match outcome {
        Outcome::Status(report) => {
            let lorawan = report
                .lorawan
                .as_ref()
                .expect("lorawan block present under mode lorawan");
            assert_eq!(
                lorawan.tokens.get("admin"),
                Some(&Presence::Present),
                "tokens are read from the injected store"
            );
        }
        _ => panic!("status returns Outcome::Status"),
    }
}

/// F22: `node-add` without `--join-eui` must fall back to `radio.join_eui`. It passed
/// `args.join_eui` straight through, so the device landed in ChirpStack with an all-zero
/// JoinEUI even though the fleet config named one — plan.md §5 says the flag *overrides*
/// the config value, which means the config value has to apply when the flag is absent.
#[test]
fn node_add_falls_back_to_the_configured_join_eui() {
    let (store, _dir) = seeded_store("joineui");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    assert_eq!(
        config.join_eui.as_deref(),
        Some("5876070000000000"),
        "the fixture is the plan example, which sets a default JoinEUI"
    );
    let fake = FakeChirpStack::new();
    let pubr = FakePublisher::new();
    let keys = std::env::temp_dir().join(format!("cr-joineui-{}.json", std::process::id()));
    std::fs::write(&keys, br#"{"app_key":"000102030405060708090a0b0c0d0e0f"}"#).expect("keys");

    rt().block_on(async {
        run_with_store(
            &cli(&["create", "--radio", "lorawan"]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        )
        .await
        .expect("create");

        run_with_store(
            &cli(&[
                "node-add",
                "--radio",
                "lorawan",
                "--profile",
                "default",
                "--serial",
                "SN-JOINEUI",
                "--dev-eui",
                "58760700c0668fc7",
                "--otaa",
                "--keys-file",
                keys.to_str().expect("utf8"),
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            Some(&pubr as &dyn RetainedPublisher),
            store,
        )
        .await
        .expect("node-add");
    });
    let _ = std::fs::remove_file(&keys);

    let eui = control_radio::lorawan::eui::Eui::parse("58760700c0668fc7").expect("eui");
    assert_eq!(
        fake.stored_join_eui(&eui).as_deref(),
        Some("5876070000000000"),
        "the configured JoinEUI applies when --join-eui is omitted"
    );
}

/// F10: `status` is the command an operator runs *because* something is broken, so a
/// ChirpStack that is down or rejecting the token must produce a report, not an empty
/// `lorawan` block. It used to `.ok()` the converge result away, dropping token presence and
/// the provisioned marker — everything that is still knowable from disk — along with it.
#[test]
fn status_reports_unreachable_when_there_is_no_client() {
    let (store, _dir) = seeded_store("degraded-none");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");

    let outcome = rt()
        .block_on(run_with_store(
            &cli(&["status"]).inner,
            &config,
            None,
            None,
            None,
            store,
        ))
        .expect("status succeeds with no ChirpStack at all");

    match outcome {
        Outcome::Status(report) => {
            let l = report.lorawan.as_ref().expect("the lorawan block survives");
            assert_eq!(l.api_status, ApiStatus::Unreachable);
            assert_eq!(
                l.tokens.get("tenant"),
                Some(&Presence::Present),
                "token presence is a disk fact and stays readable"
            );
            assert!(!l.converged.tenant, "nothing is claimed as converged");
            assert!(l.api_error.is_some(), "the reason is reported");
        }
        _ => panic!("status returns Outcome::Status"),
    }
}

/// A rejected token and an unreachable service are different operator actions, so they are
/// different values — the v1 defect issue #8 names is exactly this conflation.
#[test]
fn status_reports_auth_failed_when_the_token_is_rejected() {
    let (store, _dir) = seeded_store("degraded-auth");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    fake.fail_every_call_with(
        chirpstack_api::tonic::Code::Unauthenticated,
        "authentication failed",
    );

    let outcome = rt()
        .block_on(run_with_store(
            &cli(&["status"]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect("status succeeds even when every RPC is rejected");

    match outcome {
        Outcome::Status(report) => {
            let l = report.lorawan.as_ref().expect("the lorawan block survives");
            assert_eq!(l.api_status, ApiStatus::AuthFailed);
            assert_eq!(l.tokens.get("admin"), Some(&Presence::Present));
        }
        _ => panic!("status returns Outcome::Status"),
    }
}

/// The success path still says so explicitly, so `api_status` is always present in the JSON
/// rather than being a field that only appears when something is wrong.
#[test]
fn status_reports_ok_when_chirpstack_answers() {
    let (store, _dir) = seeded_store("degraded-ok");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();

    let outcome = rt()
        .block_on(run_with_store(
            &cli(&["status"]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect("status succeeds");

    match outcome {
        Outcome::Status(report) => {
            let l = report.lorawan.as_ref().expect("lorawan block");
            assert_eq!(l.api_status, ApiStatus::Ok);
            assert!(l.api_error.is_none(), "no error on the success path");
        }
        _ => panic!("status returns Outcome::Status"),
    }
}

/// Enrol one device through the shipped path, so the commands under test are exercised
/// against state `node-add` actually produced rather than a hand-built fixture.
///
/// Returns the publisher it enrolled through, so a caller can assert what `node-add` put on
/// the retained topic.
fn enrol_one(
    fake: &FakeChirpStack,
    config: &RadioConfig,
    store: &Store,
    serial: &str,
    eui: &str,
) -> FakePublisher {
    let pubr = FakePublisher::new();
    // Unique per call, not just per serial: `cargo test` runs these in parallel and two tests
    // enrolling the same serial would otherwise share one temp file — the first to finish
    // deletes it while the second is still reading it, and the failure lands in whichever
    // test lost the race rather than in the helper that caused it.
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let keys = std::env::temp_dir().join(format!(
        "cr-rm-{}-{unique}-{serial}.json",
        std::process::id()
    ));
    std::fs::write(&keys, br#"{"app_key":"000102030405060708090a0b0c0d0e0f"}"#).expect("keys");
    rt().block_on(async {
        run_with_store(
            &cli(&["create", "--radio", "lorawan"]).inner,
            config,
            Some(fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        )
        .await
        .expect("create");
        run_with_store(
            &cli(&[
                "node-add",
                "--radio",
                "lorawan",
                "--profile",
                "default",
                "--serial",
                serial,
                "--dev-eui",
                eui,
                "--otaa",
                "--keys-file",
                keys.to_str().expect("utf8"),
            ])
            .inner,
            config,
            Some(fake as &dyn ChirpStack),
            None,
            Some(&pubr as &dyn RetainedPublisher),
            store.clone(),
        )
        .await
        .expect("node-add");
    });
    let _ = std::fs::remove_file(&keys);
    pubr
}

#[test]
fn node_remove_deletes_the_device_and_the_record() {
    let (store, _dir) = seeded_store("rm-serial");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    enrol_one(&fake, &config, &store, "SN-RM", "58760700c0668fc7");
    assert_eq!(fake.device_count(), 1);
    let pubr = FakePublisher::new();

    let outcome = rt()
        .block_on(run_with_store(
            &cli(&["node-remove", "--radio", "lorawan", "--serial", "SN-RM"]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            Some(&pubr as &dyn RetainedPublisher),
            store.clone(),
        ))
        .expect("node-remove succeeds");

    match outcome {
        Outcome::Json(v) => {
            assert_eq!(v["command"], "node-remove");
            assert_eq!(v["state"], "deleted");
            assert_eq!(v["serial_number"], "SN-RM");
            assert_eq!(v["dev_eui"], "58760700c0668fc7");
        }
        _ => panic!("node-remove returns Outcome::Json"),
    }
    assert_eq!(fake.device_count(), 0, "the ChirpStack device is gone");
    let raw = store
        .read(control_radio::store::Slot::LorawanNodes)
        .expect("read")
        .unwrap_or_default();
    assert!(
        !raw.contains("SN-RM"),
        "the enrollment record is gone: {raw}"
    );

    // §9.4: removal **must** clear the retained topic, or the node stays in every
    // subscriber's inventory for as long as the broker lives.
    assert_eq!(
        pubr.published(),
        tombstones("58760700c0668fc7"),
        "the tombstones are empty retained payloads on every retained topic"
    );
}

/// §9.4: the retained `info` document is what a late subscriber learns the inventory from, so
/// `node-add` publishes it in the same command that enrols — not on some later daemon tick.
#[test]
fn node_add_publishes_the_retained_info_document() {
    let (store, _dir) = seeded_store("add-info");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    let pubr = enrol_one(&fake, &config, &store, "SN-INFO", "58760700c0668fc7");

    let payload = pubr
        .last_payload(&info_topic("58760700c0668fc7"))
        .expect("node-add publishes the retained info document");
    let doc: serde_json::Value = serde_json::from_slice(&payload).expect("it is JSON");

    assert_eq!(doc["serial_number"], "SN-INFO");
    assert_eq!(doc["dev_eui"], "58760700c0668fc7");
    assert_eq!(doc["transport"], "lorawan");
    assert_eq!(doc["profile"], "default");
    assert_eq!(doc["activation"], "otaa");

    // The one rule the retained document does not share with the CLI readback: a broker topic
    // is a different exposure surface from a control-socket read, so the key never goes on it.
    assert!(
        doc.get("app_key").is_none(),
        "the retained document must never carry key material: {doc}"
    );
    assert!(
        !String::from_utf8_lossy(&payload).contains("000102030405060708090a0b0c0d0e0f"),
        "the AppKey must not appear anywhere in the retained payload"
    );
}

/// #157: the app sends the decimal serial, the registry keys the canonical 8-hex one. The
/// lookup by the typed serial missed and `null` went out as the retained document, so the
/// Node stayed invisible in the portal until its join.
#[test]
fn node_add_with_a_decimal_serial_publishes_a_real_info_document() {
    let (store, _dir) = seeded_store("add-info-decimal");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    let pubr = enrol_one(&fake, &config, &store, "2162165374", "5876070798ec538a");

    let payload = pubr
        .last_payload(&info_topic("5876070798ec538a"))
        .expect("node-add publishes the retained info document");
    let doc: serde_json::Value = serde_json::from_slice(&payload).expect("it is JSON");
    assert!(!doc.is_null(), "never a null info document");
    assert_eq!(doc["dev_eui"], "5876070798ec538a");
    assert_eq!(doc["transport"], "lorawan");
}

/// A broker failure **after** ChirpStack has already deleted the device is the one ordering
/// that matters: the message has to say the device is gone, and the re-run — by the same
/// `--serial` the operator typed — has to actually clear the topic. That only works if the
/// enrollment record outlives the failed publish, since `--serial` resolves through it.
#[test]
fn a_failed_tombstone_leaves_node_remove_re_runnable_by_serial() {
    let (store, _dir) = seeded_store("rm-broker-down");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    enrol_one(&fake, &config, &store, "SN-BROKER", "58760700c0668fc7");

    let pubr = FakePublisher::failing(control_radio::error::Error::new(
        control_radio::error::Kind::ServiceDown,
        control_radio::error::Transport::Lorawan,
        control_radio::error::Component::MqttBroker,
        "connection refused",
    ));
    let err = rt()
        .block_on(run_with_store(
            &cli(&["node-remove", "--radio", "lorawan", "--serial", "SN-BROKER"]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            Some(&pubr as &dyn RetainedPublisher),
            store.clone(),
        ))
        .expect_err("an unpublished tombstone is a failure, not a silent partial success");

    assert_eq!(
        err.component(),
        control_radio::error::Component::MqttBroker,
        "the failing component is named, so a script branches on the broker"
    );
    assert_eq!(err.kind(), control_radio::error::Kind::ServiceDown);
    assert!(
        err.message().contains("IS deleted"),
        "the operator must not read this as 'the device was not deleted': {}",
        err.message()
    );
    assert!(
        err.message().contains("re-run"),
        "the message states the remedy: {}",
        err.message()
    );
    assert_eq!(fake.device_count(), 0, "ChirpStack really did delete it");

    // The re-run: the record survived precisely so `--serial` still resolves.
    pubr.recover();
    rt().block_on(run_with_store(
        &cli(&["node-remove", "--radio", "lorawan", "--serial", "SN-BROKER"]).inner,
        &config,
        Some(&fake as &dyn ChirpStack),
        None,
        Some(&pubr as &dyn RetainedPublisher),
        store.clone(),
    ))
    .expect("removing an already-deleted device is success, and it clears the topic");

    assert_eq!(
        pubr.published(),
        tombstones("58760700c0668fc7"),
        "the re-run is what finally publishes the tombstone"
    );
    let raw = store
        .read(control_radio::store::Slot::LorawanNodes)
        .expect("read")
        .unwrap_or_default();
    assert!(
        !raw.contains("SN-BROKER"),
        "and only now is the record dropped: {raw}"
    );
}

/// `--dev-eui` addresses ChirpStack directly, so a device neither side knows is not an
/// error — removal is idempotent, matching `Enroller::remove` tolerating `NotFound`. It
/// still tombstones: an absent device and a cleared topic are the same desired state.
#[test]
fn node_remove_by_unknown_dev_eui_is_idempotent() {
    let (store, _dir) = seeded_store("rm-deveui");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    let pubr = FakePublisher::new();

    let outcome = rt()
        .block_on(run_with_store(
            &cli(&[
                "node-remove",
                "--radio",
                "lorawan",
                "--dev-eui",
                "0011223344556677",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            Some(&pubr as &dyn RetainedPublisher),
            store,
        ))
        .expect("removing something that is not there is not a failure");
    match outcome {
        Outcome::Json(v) => {
            assert_eq!(v["state"], "deleted");
            assert_eq!(v["dev_eui"], "0011223344556677");
            assert!(
                v.get("serial_number").is_none(),
                "nothing addressed it by serial"
            );
        }
        _ => panic!("Outcome::Json"),
    }
    assert_eq!(
        pubr.published(),
        tombstones("0011223344556677"),
        "the tombstone goes out even when there was nothing to delete"
    );
}

/// A `--serial` miss is a local bookkeeping miss, deliberately distinct from a ChirpStack one.
#[test]
fn node_remove_by_unknown_serial_is_not_found_on_the_enrollment_record() {
    let (store, _dir) = seeded_store("rm-miss");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();

    let err = rt()
        .block_on(run_with_store(
            &cli(&["node-remove", "--radio", "lorawan", "--serial", "NOPE"]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect_err("an unknown serial cannot be resolved");
    assert_eq!(err.kind(), control_radio::error::Kind::NotFound);
    assert_eq!(
        err.component(),
        control_radio::error::Component::EnrollmentRecord
    );
}

/// Under `mode: lorawan` the mode gate answers first — `--radio p2p` is refused as
/// `P2PUnavailable` before any command arm runs. That ordering is deliberate (`mode.rs`,
/// "the mode gate runs before the capability check"), so it is what this asserts.
#[test]
fn node_remove_over_p2p_under_lorawan_mode_hits_the_mode_gate() {
    let (store, _dir) = seeded_store("rm-p2p-gate");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    let err = rt()
        .block_on(run_with_store(
            &cli(&["node-remove", "--radio", "p2p", "--serial", "SN"]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect_err("the p2p arm is not available under mode: lorawan");
    assert_eq!(err.kind(), control_radio::error::Kind::P2pUnavailable);
}

/// With the P2P arm enabled, `node-remove --radio p2p` reaches the registry. Removing a serial
/// that was never registered is `NotFound` — the registry is the source of truth, and there is
/// nothing to detach.
#[test]
fn node_remove_over_p2p_is_not_found_on_an_empty_registry() {
    let (store, _dir) = seeded_store("rm-p2p");
    let dual = PLAN_EXAMPLE.replacen("mode: lorawan", "mode: dual", 1);
    let config = RadioConfig::from_device_config_yaml(&dual).expect("dual-mode plan example");
    let fake = FakeChirpStack::new();
    let err = rt()
        .block_on(run_with_store(
            &cli(&["node-remove", "--radio", "p2p", "--serial", "0x80e007b2"]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect_err("nothing registered with that serial");
    assert_eq!(err.kind(), control_radio::error::Kind::NotFound);
}

/// §5: P2P `node-add` is registration and the bundle is **required** — the pairing window that
/// used to make it optional is withdrawn (`p2p.md` §8). A partial bundle is refused locally as
/// `InvalidArgument` naming the caller's missing argument; a complete bundle writes the registry.
#[test]
fn a_partial_p2p_registration_bundle_is_refused_and_a_complete_one_registers() {
    let dual = PLAN_EXAMPLE.replacen("mode: lorawan", "mode: dual", 1);
    let config = RadioConfig::from_device_config_yaml(&dual).expect("dual-mode plan example");

    // Each of the three bundle members, missing in turn.
    let partials: [&[&str]; 3] = [
        // no --serial
        &[
            "node-add",
            "--radio",
            "p2p",
            "--profile",
            "default",
            "--dev-eui",
            "70b3d57ed0000abe",
            "--keys-stdin",
        ],
        // no --dev-eui
        &[
            "node-add",
            "--radio",
            "p2p",
            "--profile",
            "default",
            "--serial",
            "SN125",
            "--keys-stdin",
        ],
        // no key material
        &[
            "node-add",
            "--radio",
            "p2p",
            "--profile",
            "default",
            "--serial",
            "SN125",
            "--dev-eui",
            "70b3d57ed0000abe",
        ],
    ];
    for (i, argv) in partials.iter().enumerate() {
        let (store, _dir) = seeded_store(&format!("p2p-add-partial-{i}"));
        let fake = FakeChirpStack::new();
        let err = match rt().block_on(run_with_store(
            &cli(argv).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        )) {
            Err(e) => e,
            Ok(_) => panic!("{argv:?} must be refused: the bundle is incomplete"),
        };
        assert_eq!(
            err.kind(),
            control_radio::error::Kind::InvalidArgument,
            "{argv:?} must be a usage error, not a phase-2 refusal"
        );
        assert_eq!(
            err.detail().component,
            control_radio::error::Component::ControlSocket
        );
    }

    // A malformed EUI is `InvalidArgument` too, but must not report `transport: lorawan` just
    // because the EUI parser lives on the LoRaWAN side (§6: never name a transport the caller
    // did not mention).
    let (store, _dir) = seeded_store("p2p-add-bad-eui");
    let fake = FakeChirpStack::new();
    let err = rt()
        .block_on(run_with_store(
            &cli(&[
                "node-add",
                "--radio",
                "p2p",
                "--profile",
                "default",
                "--serial",
                "SN125",
                "--dev-eui",
                "not-hex",
                "--keys-stdin",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect_err("a malformed dev_eui is refused");
    assert_eq!(err.kind(), control_radio::error::Kind::InvalidArgument);
    assert_eq!(
        err.detail().transport,
        Some(control_radio::error::Transport::P2p)
    );

    // The complete bundle now succeeds: it writes the node registry (§7). A numeric serial is
    // required (canonicalised to the on-air 8-hex form the join handler looks up by), and the
    // key comes from a file (never argv).
    let (store, _dir) = seeded_store("p2p-add-complete");
    let store_after = store.clone();
    let keys = std::env::temp_dir().join(format!("p2p-add-keys-{}.json", std::process::id()));
    std::fs::write(&keys, "{\"app_key\":\"00112233445566778899aabbccddeeff\"}").expect("keys file");
    let outcome = rt()
        .block_on(run_with_store(
            &cli(&[
                "node-add",
                "--radio",
                "p2p",
                "--profile",
                "default",
                "--serial",
                "2162165682",
                "--dev-eui",
                "70b3d57ed0000abe",
                "--keys-file",
                keys.to_str().unwrap(),
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect("a complete bundle registers the node");
    let _ = std::fs::remove_file(&keys);
    match outcome {
        Outcome::Json(v) => {
            assert_eq!(v["command"], "node-add");
            assert_eq!(v["radio"], "p2p");
            // 2162165682 == 0x80e007b2, canonicalised to the on-air spelling.
            assert_eq!(v["serial_number"], "80e007b2");
            assert_eq!(v["dev_eui"], "70b3d57ed0000abe");
            assert_eq!(v["profile"], "default");
            assert!(
                v.get("pairing_window").is_none(),
                "no window: registration is the gate"
            );
        }
        other => panic!("expected Outcome::Json, got {other:?}"),
    }
    // The registry was actually written, keyed by the canonical serial, with the app_key stored.
    let db = control_radio::p2p::db::SessionsDb::load(&store_after).expect("db loads");
    let node = db.node_by_serial("80e007b2").expect("registered");
    assert_eq!(node.app_key, "00112233445566778899aabbccddeeff");
    assert_eq!(node.dev_eui, "70b3d57ed0000abe");
}

/// `p2p_tower_gateway.md` §3.1 (amendment 3): the P2P gateway keys its peers by the low 32
/// bits of the DevEUI, so a second Node on the same address is refused — at `node-add --radio
/// p2p` and at `node-move --to p2p` — with `AddressCollision` (exit 16) naming both DevEUIs,
/// and nothing is written.
#[test]
fn a_second_p2p_node_on_the_same_address_is_refused_at_add_and_at_move() {
    use control_radio::error::{Kind, Transport};
    use control_radio::registry::Registry;
    let dual = PLAN_EXAMPLE.replacen("mode: lorawan", "mode: dual", 1);
    let config = RadioConfig::from_device_config_yaml(&dual).expect("dual-mode plan example");
    let (store, _dir) = seeded_store("p2p-addr-collision");
    let fake = FakeChirpStack::new();
    // On LoRaWAN first: the address only matters once both are on P2P.
    enrol_one(&fake, &config, &store, "2162165690", "5876070fd0000abe");
    let keys = std::env::temp_dir().join(format!("p2p-collide-{}.json", std::process::id()));
    std::fs::write(&keys, "{\"app_key\":\"00112233445566778899aabbccddeeff\"}").expect("keys");
    let add = |serial: &str, eui: &str| {
        rt().block_on(run_with_store(
            &cli(&[
                "node-add",
                "--radio",
                "p2p",
                "--profile",
                "default",
                "--serial",
                serial,
                "--dev-eui",
                eui,
                "--keys-file",
                keys.to_str().expect("utf8"),
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            Some(&FakePublisher::new() as &dyn RetainedPublisher),
            store.clone(),
        ))
    };
    add("2162165682", "70b3d57ed0000abe").expect("the first one registers");

    let err = add("2162165683", "58760700d0000abe").expect_err("same low 32 bits");
    assert_eq!(err.kind(), Kind::AddressCollision);
    assert_eq!(err.kind().exit_code(), 16);
    assert!(
        err.message().contains("70b3d57ed0000abe"),
        "{}",
        err.message()
    );
    assert!(
        err.message().contains("58760700d0000abe"),
        "{}",
        err.message()
    );
    assert!(err.message().contains("d0000abe"), "the address, 8-hex");
    assert!(
        !err.message().to_lowercase().contains("tower"),
        "no internal name"
    );
    let reg = Registry::load(&store).expect("load").expect("registry");
    assert!(reg.get("58760700d0000abe").is_none(), "nothing written");

    let pubr = FakePublisher::new();
    let err = rt()
        .block_on(run_with_store(
            &cli(&["node-move", "--dev-eui", "5876070fd0000abe", "--to", "p2p"]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            Some(&pubr as &dyn RetainedPublisher),
            store.clone(),
        ))
        .expect_err("same low 32 bits");
    assert_eq!(err.kind(), Kind::AddressCollision);
    let reg = Registry::load(&store).expect("load").expect("registry");
    assert_eq!(
        reg.get("5876070fd0000abe").map(|n| n.radio),
        Some(Transport::Lorawan),
        "the move did not happen"
    );
    let _ = std::fs::remove_file(&keys);
}

/// §9.4: a P2P `node-add` publishes the retained `info` inventory document, so the portal sees
/// the node at registration rather than only after it first joins on air. This is the write
/// side of the same family the portal already subscribes to (`radio_node_topics`); without it a
/// registered P2P node was invisible in the UI. The document carries the shared identity core
/// with `pairing_status: "registered"`, and never the `app_key`.
#[test]
fn p2p_node_add_publishes_the_retained_info_document() {
    let (store, _dir) = seeded_store("p2p-add-info");
    let dual = PLAN_EXAMPLE.replacen("mode: lorawan", "mode: dual", 1);
    let config = RadioConfig::from_device_config_yaml(&dual).expect("dual-mode plan example");
    let fake = FakeChirpStack::new();
    let pubr = FakePublisher::new();
    let keys = std::env::temp_dir().join(format!("p2p-add-info-keys-{}.json", std::process::id()));
    std::fs::write(&keys, "{\"app_key\":\"00112233445566778899aabbccddeeff\"}").expect("keys file");

    rt().block_on(run_with_store(
        &cli(&[
            "node-add",
            "--radio",
            "p2p",
            "--profile",
            "default",
            "--serial",
            "2162165682",
            "--dev-eui",
            "70b3d57ed0000abe",
            "--keys-file",
            keys.to_str().unwrap(),
        ])
        .inner,
        &config,
        Some(&fake as &dyn ChirpStack),
        None,
        Some(&pubr as &dyn RetainedPublisher),
        store,
    ))
    .expect("node-add registers and publishes");
    let _ = std::fs::remove_file(&keys);

    // Exactly one retained document, on the node's `info` topic (D3: no transport level).
    let published = pubr.published();
    let info = published
        .iter()
        .find(|(t, _)| t.ends_with("/nodes/70b3d57ed0000abe/info"))
        .unwrap_or_else(|| panic!("no info document published; got {:?}", published));

    let v: serde_json::Value = serde_json::from_slice(&info.1).expect("info payload is JSON");
    assert_eq!(
        v["transport"], "p2p",
        "transport rides inside the document (D3)"
    );
    // 2162165682 == 0x80e007b2, canonicalised to the on-air spelling.
    assert_eq!(v["serial_number"], "80e007b2");
    assert_eq!(v["dev_eui"], "70b3d57ed0000abe");
    assert_eq!(v["profile"], "default");
    assert_eq!(
        v["pairing_status"], "registered",
        "registered, not joined: no session exists yet"
    );
    assert!(
        v.get("app_key").is_none(),
        "key material never leaves the registry (§9.7)"
    );
}

/// PN-6 (night test 2026-09-26): `--name` is the operator's name on both carriers. The P2P arm
/// ignored it, so a Node the portal registered on a P2P Hub was listed under its DevEUI.
#[test]
fn p2p_node_add_keeps_the_name_and_publishes_it() {
    let (store, _dir) = seeded_store("p2p-add-name");
    let dual = PLAN_EXAMPLE.replacen("mode: lorawan", "mode: dual", 1);
    let config = RadioConfig::from_device_config_yaml(&dual).expect("dual-mode plan example");
    let fake = FakeChirpStack::new();
    let pubr = FakePublisher::new();
    let keys = std::env::temp_dir().join(format!("p2p-add-name-keys-{}.json", std::process::id()));
    std::fs::write(&keys, "{\"app_key\":\"00112233445566778899aabbccddeeff\"}").expect("keys file");

    rt().block_on(run_with_store(
        &cli(&[
            "node-add",
            "--radio",
            "p2p",
            "--profile",
            "default",
            "--serial",
            "2162165682",
            "--dev-eui",
            "70b3d57ed0000abe",
            "--name",
            "  Cold room 2 ",
            "--keys-file",
            keys.to_str().unwrap(),
        ])
        .inner,
        &config,
        Some(&fake as &dyn ChirpStack),
        None,
        Some(&pubr as &dyn RetainedPublisher),
        store.clone(),
    ))
    .expect("node-add registers and publishes");
    let _ = std::fs::remove_file(&keys);

    let db = control_radio::p2p::db::SessionsDb::load(&store).expect("registry");
    assert_eq!(
        db.node("70b3d57ed0000abe").and_then(|n| n.name.clone()),
        Some("Cold room 2".to_string()),
        "stored, trimmed"
    );
    let published = pubr.published();
    let info = published
        .iter()
        .find(|(t, _)| t.ends_with("/nodes/70b3d57ed0000abe/info"))
        .expect("info published");
    let v: serde_json::Value = serde_json::from_slice(&info.1).expect("JSON");
    assert_eq!(v["name"], "Cold room 2");

    // And the read commands report it, as the LoRaWAN arm's do.
    let config = p2p_dual_config();
    for args in [
        vec!["node-list", "--radio", "p2p"],
        vec![
            "node-info",
            "--radio",
            "p2p",
            "--dev-eui",
            "70b3d57ed0000abe",
        ],
    ] {
        let out = rt()
            .block_on(run_with_store(
                &cli(&args).inner,
                &config,
                Some(&fake as &dyn ChirpStack),
                None,
                None,
                store.clone(),
            ))
            .expect("read command");
        let Outcome::Json(v) = out else {
            panic!("expected JSON from {args:?}")
        };
        let row = if args[0] == "node-list" {
            v["nodes"][0].clone()
        } else {
            v
        };
        assert_eq!(row["name"], "Cold room 2", "{args:?}");
    }
}

/// One `node-add --radio p2p`, so the duplicate-registration tests below differ only in the
/// identity they re-use.
fn p2p_node_add(
    store: Store,
    serial: &str,
    dev_eui: &str,
    keys: &std::path::Path,
) -> control_radio::Result<()> {
    let dual = PLAN_EXAMPLE.replacen("mode: lorawan", "mode: dual", 1);
    let config = RadioConfig::from_device_config_yaml(&dual).expect("dual-mode plan example");
    let fake = FakeChirpStack::new();
    rt().block_on(run_with_store(
        &cli(&[
            "node-add",
            "--radio",
            "p2p",
            "--profile",
            "default",
            "--serial",
            serial,
            "--dev-eui",
            dev_eui,
            "--keys-file",
            keys.to_str().unwrap_or_default(),
        ])
        .inner,
        &config,
        Some(&fake as &dyn ChirpStack),
        None,
        None,
        store,
    ))
    .map(|_| ())
}

/// A keys file for the tests above, removed when the guard drops.
struct KeysFile(std::path::PathBuf);
impl KeysFile {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("p2p-{tag}-keys-{}.json", std::process::id()));
        std::fs::write(&p, "{\"app_key\":\"00112233445566778899aabbccddeeff\"}")
            .expect("keys file");
        KeysFile(p)
    }
}
impl Drop for KeysFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// §6: P2P registration is add-only. Re-adding a DevEUI already in the registry is
/// `AlreadyExists` + `component: node-registry` (exit 14), never a silent key-rotating
/// overwrite — an `app_key` rotation is a `node-remove` followed by a fresh `node-add`
/// (`p2p.md` §10, decided 2026-08-31). Since #73 the DevEUI is the registry key, so this is
/// the primary duplicate.
#[test]
fn re_registering_an_existing_p2p_dev_eui_is_already_exists() {
    let keys = KeysFile::new("dup-eui");
    let (store, _dir) = seeded_store("p2p-add-dup-eui");
    p2p_node_add(store.clone(), "2162165682", "70b3d57ed0000abe", &keys.0)
        .expect("the first registration succeeds");

    // Same DevEUI, *different* serial: still refused. The key is the DevEUI.
    let err = p2p_node_add(store, "2162165683", "70b3d57ed0000abe", &keys.0)
        .expect_err("re-registering an existing dev_eui is refused");
    assert_eq!(err.kind(), control_radio::error::Kind::AlreadyExists);
    assert_eq!(
        err.detail().component,
        control_radio::error::Component::NodeRegistry
    );
    assert_eq!(
        err.detail().transport,
        Some(control_radio::error::Transport::P2p)
    );
    assert!(
        err.message().contains("dev_eui 70b3d57ed0000abe"),
        "names the duplicate identity: {}",
        err.message()
    );
}

/// Decision #23 (amended 2026-09-27): a Node is registered on one carrier only. On a `dual`
/// Hub, `node-add` on either carrier refuses a DevEUI the other one holds, `AlreadyExists` on
/// the node registry, and names the carrier to remove it from. Both directions, one scenario.
#[test]
fn node_add_refuses_a_dev_eui_registered_on_the_other_carrier() {
    use control_radio::error::{Component, Kind, Transport};
    const EUI: &str = "70b3d57ed0000c23";
    let dual = PLAN_EXAMPLE.replacen("mode: lorawan", "mode: dual", 1);
    let config = RadioConfig::from_device_config_yaml(&dual).expect("dual-mode plan example");
    let keys = KeysFile::new("other-carrier");

    for first in [Transport::P2p, Transport::Lorawan] {
        let (store, _dir) = seeded_store(&format!("other-carrier-{first}"));
        let fake = FakeChirpStack::new();
        let add_lorawan = || {
            rt().block_on(async {
                run_with_store(
                    &cli(&["create", "--radio", "lorawan"]).inner,
                    &config,
                    Some(&fake as &dyn ChirpStack),
                    None,
                    None,
                    store.clone(),
                )
                .await
                .expect("create");
                let pubr = FakePublisher::new();
                run_with_store(
                    &cli(&[
                        "node-add",
                        "--radio",
                        "lorawan",
                        "--profile",
                        "default",
                        "--serial",
                        "3107",
                        "--dev-eui",
                        EUI,
                        "--otaa",
                        "--keys-file",
                        keys.0.to_str().expect("utf8"),
                    ])
                    .inner,
                    &config,
                    Some(&fake as &dyn ChirpStack),
                    None,
                    Some(&pubr as &dyn RetainedPublisher),
                    store.clone(),
                )
                .await
                .map(|_| ())
            })
        };
        let add_p2p = || p2p_node_add(store.clone(), "2162165690", EUI, &keys.0);

        let (then, second) = match first {
            Transport::P2p => {
                add_p2p().expect("the P2P registration succeeds");
                (Transport::Lorawan, add_lorawan())
            }
            Transport::Lorawan => {
                add_lorawan().expect("the LoRaWAN registration succeeds");
                (Transport::P2p, add_p2p())
            }
        };
        let err = second.expect_err("a second carrier is refused");
        assert_eq!(err.kind(), Kind::AlreadyExists, "{first} then {then}");
        assert_eq!(err.detail().component, Component::NodeRegistry);
        assert_eq!(err.detail().transport, Some(then));
        assert!(
            err.message()
                .contains(&format!("already registered on {first}")),
            "names the carrier it is on: {}",
            err.message()
        );
    }
}

/// The registry key is the DevEUI, so it has to be the *canonical* DevEUI — the same 16
/// lowercase hex digits `Eui::from_bytes` produces from the bytes on the air. `Eui::parse`
/// deliberately accepts separators and any case (it is what is printed on a device label), and
/// `node-add` validated the spelling but stored the operator's raw string as the key.
///
/// That made this the one way to break the identity change without any error at all: the
/// record looks right, `node-list` and every `--serial` command keep working, and only the
/// radio link is dead — the node's JoinRequest carries `70b3d57e…`, the registry is keyed by
/// `70:B3:D5:7E:…`, and the join is dropped forever with one diag line.
#[test]
fn node_add_canonicalizes_the_dev_eui_it_keys_the_registry_by() {
    let keys = KeysFile::new("canon");
    let (store, _dir) = seeded_store("p2p-add-canonical");

    p2p_node_add(
        store.clone(),
        "2162165682",
        "70:B3:D5:7E:D0:00:0A:BE",
        &keys.0,
    )
    .expect("a separator-bearing DevEUI is a legal spelling");

    let db = control_radio::p2p::db::SessionsDb::load(&store).expect("load");
    assert_eq!(
        db.nodes.keys().collect::<Vec<_>>(),
        vec![P2P_DEV_EUI],
        "the map key is the canonical DevEUI, not what was typed"
    );
    assert_eq!(
        db.node(P2P_DEV_EUI).expect("registered").dev_eui,
        P2P_DEV_EUI
    );

    // And the identity the node will actually put on the air resolves to it.
    assert!(
        db.node(
            control_radio::lorawan::Eui::parse("70:B3:D5:7E:D0:00:0A:BE")
                .expect("valid")
                .as_str()
        )
        .is_some(),
        "the on-air spelling finds the record"
    );

    // Re-registering the same device under a different spelling is still a duplicate.
    let err = p2p_node_add(store, "2162165683", "70b3d57ed0000abe", &keys.0)
        .expect_err("the same DevEUI, spelled differently, is still already registered");
    assert_eq!(err.kind(), control_radio::error::Kind::AlreadyExists);
}

/// The serial is only an attribute now, but `--serial` still resolves through the registry, so
/// one serial under two DevEUIs would make that alias ambiguous — a later `node-send --serial`
/// would reach whichever record the scan found first. Refused at registration instead.
#[test]
fn re_registering_a_serial_under_a_second_dev_eui_is_already_exists() {
    let keys = KeysFile::new("dup-serial");
    let (store, _dir) = seeded_store("p2p-add-dup-serial");
    p2p_node_add(store.clone(), "2162165682", "70b3d57ed0000abe", &keys.0)
        .expect("the first registration succeeds");

    let err = p2p_node_add(store, "2162165682", "70b3d57ed0000abf", &keys.0)
        .expect_err("a serial may not sit under two dev_euis");
    assert_eq!(err.kind(), control_radio::error::Kind::AlreadyExists);
    assert_eq!(
        err.detail().component,
        control_radio::error::Component::NodeRegistry
    );
    assert!(
        err.message()
            .contains("already registered under dev_eui 70b3d57ed0000abe"),
        "names the record that owns the serial: {}",
        err.message()
    );
}

/// `gateway-remove` mirrors `gateway-add`: delete the registration and forget it locally, so
/// the next `create` re-converges from `pending` instead of believing a gateway it no longer
/// has. Without the local half, `config.json` keeps a `gateway_id` for a gateway ChirpStack
/// has never heard of.
#[test]
fn gateway_remove_deletes_the_registration_and_forgets_it() {
    let (store, _dir) = seeded_store("gw-rm");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();

    rt().block_on(async {
        run_with_store(
            &cli(&[
                "create",
                "--radio",
                "lorawan",
                "--gateway-eui",
                "24e124fffefd3bda",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        )
        .await
        .expect("create registers the gateway");
    });
    assert_eq!(fake.gateway_count(), 1);
    assert!(store.is_provisioned(control_radio::error::Transport::Lorawan));

    let outcome = rt()
        .block_on(run_with_store(
            &cli(&[
                "gateway-remove",
                "--radio",
                "lorawan",
                "--gateway-eui",
                "24e124fffefd3bda",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        ))
        .expect("gateway-remove succeeds");

    match outcome {
        Outcome::Json(v) => {
            assert_eq!(v["command"], "gateway-remove");
            assert_eq!(v["gateway_eui"], "24e124fffefd3bda");
            assert_eq!(v["state"], "deleted");
        }
        _ => panic!("Outcome::Json"),
    }
    assert_eq!(fake.gateway_count(), 0, "deregistered in ChirpStack");
    let raw = store
        .read(control_radio::store::Slot::LorawanConfig)
        .expect("read")
        .unwrap_or_default();
    assert!(
        !raw.contains("24e124fffefd3bda"),
        "the converged state forgot the gateway: {raw}"
    );
    assert!(
        !store.is_provisioned(control_radio::error::Transport::Lorawan),
        "the boot marker is withdrawn so the next boot re-converges"
    );
}

/// Removing a gateway nobody registered is a no-op, matching `wipe`'s tolerate-missing rule.
#[test]
fn gateway_remove_of_an_unknown_eui_is_idempotent() {
    let (store, _dir) = seeded_store("gw-rm-unknown");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    let outcome = rt()
        .block_on(run_with_store(
            &cli(&[
                "gateway-remove",
                "--radio",
                "lorawan",
                "--gateway-eui",
                "0011223344556677",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect("removing an unregistered gateway is not a failure");
    match outcome {
        Outcome::Json(v) => assert_eq!(v["state"], "deleted"),
        _ => panic!("Outcome::Json"),
    }
}

#[test]
fn gateway_remove_over_p2p_is_still_phase_two() {
    let (store, _dir) = seeded_store("gw-rm-p2p");
    let dual = PLAN_EXAMPLE.replacen("mode: lorawan", "mode: dual", 1);
    let config = RadioConfig::from_device_config_yaml(&dual).expect("dual");
    let fake = FakeChirpStack::new();
    let err = rt()
        .block_on(run_with_store(
            &cli(&[
                "gateway-remove",
                "--radio",
                "p2p",
                "--gateway-eui",
                "0011223344556677",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect_err("a P2P gateway is a Hub; nothing is registered");
    assert_eq!(err.kind(), control_radio::error::Kind::Phase2NotImplemented);
}

#[test]
fn gateway_remove_rejects_a_malformed_eui_before_any_rpc() {
    let (store, _dir) = seeded_store("gw-rm-bad");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    let err = rt()
        .block_on(run_with_store(
            &cli(&[
                "gateway-remove",
                "--radio",
                "lorawan",
                "--gateway-eui",
                "nothex",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect_err("a typo is a local usage error");
    assert_eq!(err.kind(), control_radio::error::Kind::InvalidArgument);
    assert!(
        fake.calls().is_empty(),
        "no RPC was made: {:?}",
        fake.calls()
    );
}

// ---------------------------------------------------------------------------------------
// The read commands and `node-send` — the arms that used to be one `Phase2NotImplemented`.
// ---------------------------------------------------------------------------------------

/// The `application_id` `create` recorded, read the way `node-list` reads it.
fn application_id(store: &Store) -> String {
    let raw = store
        .read(control_radio::store::Slot::LorawanConfig)
        .expect("read")
        .expect("create wrote config.json");
    control_radio::lorawan::ConvergedState::from_json(&raw)
        .expect("parses")
        .application_id
        .expect("create recorded the application id")
}

/// §5: `node-info` is this component's own enrollment record, key material included — that is
/// decided in the plan, not defaulted ("the operational need to verify a key actually matches
/// what was provisioned … outweighs the exposure concern").
#[test]
fn node_info_returns_the_enrollment_record_with_the_plaintext_key() {
    let (store, _dir) = seeded_store("info-ok");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    enrol_one(&fake, &config, &store, "SN-INFO", "58760700c0668fc7");

    for argv in [
        vec!["node-info", "--radio", "lorawan", "--serial", "SN-INFO"],
        vec![
            "node-info",
            "--radio",
            "lorawan",
            "--dev-eui",
            "58760700c0668fc7",
        ],
    ] {
        fake.clear_calls();
        let outcome = rt()
            .block_on(run_with_store(
                &cli(&argv).inner,
                &config,
                Some(&fake as &dyn ChirpStack),
                None,
                None,
                store.clone(),
            ))
            .unwrap_or_else(|e| panic!("{argv:?}: {e}"));

        match outcome {
            Outcome::Json(v) => {
                assert_eq!(v["serial_number"], "SN-INFO", "{argv:?}");
                assert_eq!(v["dev_eui"], "58760700c0668fc7");
                assert_eq!(v["transport"], "lorawan");
                assert_eq!(v["profile"], "default");
                assert_eq!(v["activation"], "otaa");
                assert_eq!(v["join_eui"], "5876070000000000");
                assert_eq!(v["app_key"], "000102030405060708090a0b0c0d0e0f");
                // Present-but-null: nothing local can say whether the device has joined, and a
                // guess would be worse than an admitted gap (WP-D's cache fills it).
                assert!(v["pairing_status"].is_null());
            }
            _ => panic!("node-info returns Outcome::Json"),
        }
        // Bookkeeping only: it must answer with ChirpStack down, so it must not call it.
        assert!(
            fake.calls().is_empty(),
            "node-info made an RPC: {:?}",
            fake.calls()
        );
    }
}

/// A node this component never enrolled has no record to report — `NotFound` (exit 13) on the
/// `enrollment-record` component, the same distinction §6 draws for `node-remove --serial`.
#[test]
fn node_info_for_an_unknown_node_is_not_found() {
    let (store, _dir) = seeded_store("info-miss");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();

    for argv in [
        vec!["node-info", "--radio", "lorawan", "--serial", "NOPE"],
        // Even the identifier that addresses ChirpStack directly on `node-remove`: there is
        // simply no local record to render, and inventing one would be a lie.
        vec![
            "node-info",
            "--radio",
            "lorawan",
            "--dev-eui",
            "0011223344556677",
        ],
    ] {
        let err = match rt().block_on(run_with_store(
            &cli(&argv).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        )) {
            Err(e) => e,
            Ok(o) => panic!("{argv:?} must be refused, got {o:?}"),
        };
        assert_eq!(err.kind(), control_radio::error::Kind::NotFound, "{argv:?}");
        assert_eq!(err.exit_code(), 13, "{argv:?}");
        assert_eq!(
            err.component(),
            control_radio::error::Component::EnrollmentRecord,
            "{argv:?}: a local bookkeeping miss, not a ChirpStack one"
        );
    }
}

/// §5: `node-list` is ChirpStack's device list joined with this Hub's enrollment records —
/// one shape, with the local bookkeeping filled in where it exists and honestly absent where
/// it does not.
#[test]
fn node_list_joins_chirpstack_devices_with_the_enrollment_records() {
    let (store, _dir) = seeded_store("list-join");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    enrol_one(&fake, &config, &store, "SN-LIST", "58760700c0668fc7");

    // A device somebody added straight in ChirpStack's own UI — no enrollment record here.
    fake.seed_device(
        &application_id(&store),
        control_radio::lorawan::model::Device {
            dev_eui: control_radio::lorawan::eui::Eui::parse("0011223344556677").expect("eui"),
            name: "added-in-the-ui".to_string(),
            device_profile_id: String::new(),
            last_seen_at: None,
        },
    );
    let seen = control_radio::lorawan::eui::Eui::parse("58760700c0668fc7").expect("eui");
    fake.mark_device_seen(&seen, "2026-08-20T12:00:00Z");

    let outcome = rt()
        .block_on(run_with_store(
            &cli(&["node-list", "--radio", "lorawan"]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        ))
        .expect("node-list succeeds");

    let v = match outcome {
        Outcome::Json(v) => v,
        _ => panic!("node-list returns Outcome::Json"),
    };
    let nodes = v["nodes"].as_array().expect("an array of nodes").clone();
    assert_eq!(nodes.len(), 2, "both devices are listed: {v}");

    let enrolled = nodes
        .iter()
        .find(|n| n["dev_eui"] == "58760700c0668fc7")
        .expect("the enrolled device");
    assert_eq!(enrolled["serial_number"], "SN-LIST");
    assert_eq!(enrolled["profile"], "default");
    assert_eq!(enrolled["activation"], "otaa");
    assert_eq!(enrolled["transport"], "lorawan");
    assert_eq!(
        enrolled["pairing_status"], "joined",
        "it has been heard from"
    );
    assert_eq!(enrolled["last_seen"], "2026-08-20T12:00:00Z");
    // Per-gateway RSSI/SNR needs the uplink metadata only the WP-D cache holds: present as
    // null, so a consumer can tell "not known yet" from "no gateway heard it".
    assert!(enrolled["gateways"].is_null());
    // §5: node-list is node-info's core *trimmed* — no key material.
    assert!(
        enrolled.get("app_key").is_none(),
        "node-list must carry no key material: {enrolled}"
    );

    let foreign = nodes
        .iter()
        .find(|n| n["dev_eui"] == "0011223344556677")
        .expect("the out-of-band device is listed too");
    assert!(
        foreign.get("serial_number").is_none(),
        "no record, so no serial — its absence is the fact: {foreign}"
    );
    assert_eq!(foreign["pairing_status"], "never");
    assert!(foreign["last_seen"].is_null());
}

/// Before `create` there is no application to list devices from, and that is a config fact —
/// `NotBootstrapped` (exit 10), not an empty list that reads as "no nodes".
#[test]
fn node_list_before_create_is_not_bootstrapped() {
    let (store, _dir) = seeded_store("list-nocreate");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    let err = rt()
        .block_on(run_with_store(
            &cli(&["node-list", "--radio", "lorawan"]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect_err("no application_id yet");
    assert_eq!(err.kind(), control_radio::error::Kind::NotBootstrapped);
    assert_eq!(err.exit_code(), 10);
}

/// §5/§6: `gateway-list` returns the `radio/gateways` payload on demand — the same three keys,
/// with `api_status` top-level rather than folded into every row.
#[test]
fn gateway_list_returns_the_radio_gateways_shape() {
    let (store, _dir) = seeded_store("gw-list");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    rt().block_on(run_with_store(
        &cli(&[
            "create",
            "--radio",
            "lorawan",
            "--gateway-eui",
            "24e124fffefd3bda",
        ])
        .inner,
        &config,
        Some(&fake as &dyn ChirpStack),
        None,
        None,
        store.clone(),
    ))
    .expect("create registers the gateway");

    let outcome = rt()
        .block_on(run_with_store(
            &cli(&["gateway-list", "--radio", "lorawan"]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        ))
        .expect("gateway-list succeeds");
    let v = match outcome {
        Outcome::Json(v) => v,
        _ => panic!("gateway-list returns Outcome::Json"),
    };

    assert_eq!(v["api_status"], "ok");
    assert!(
        v["timestamp"].as_str().is_some_and(|t| t.ends_with('Z')),
        "an RFC3339 UTC timestamp: {v}"
    );
    let rows = v["gateways"].as_array().expect("gateways[]");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["gateway_eui"], "24e124fffefd3bda");
    assert_eq!(rows[0]["radio"], "lorawan");
    assert_eq!(
        rows[0]["status"], "offline",
        "never seen is offline (§6), not unknown"
    );

    // And a gateway heard from just now reads online through the same 300 s window.
    let eui = control_radio::lorawan::eui::Eui::parse("24e124fffefd3bda").expect("eui");
    fake.mark_gateway_seen(
        &eui,
        &control_radio::lorawan::gateway::format_rfc3339(
            control_radio::lorawan::gateway::now_unix(),
        ),
    );
    let outcome = rt()
        .block_on(run_with_store(
            &cli(&["gateway-list", "--radio", "lorawan"]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect("gateway-list succeeds");
    match outcome {
        Outcome::Json(v) => assert_eq!(v["gateways"][0]["status"], "online"),
        _ => panic!("Outcome::Json"),
    }
}

/// §5: `gateway-info` is one `gateway-list` row **plus the registration record** — the two
/// ChirpStack-only fields. An EUI nobody registered is `NotFound`, not `Unauthenticated`:
/// that is why the lookup goes through the tenant-scoped list (F20).
#[test]
fn gateway_info_adds_the_registration_record_and_misses_are_not_found() {
    let (store, _dir) = seeded_store("gw-info");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new().with_gateway_get_exists_join();
    rt().block_on(run_with_store(
        &cli(&[
            "create",
            "--radio",
            "lorawan",
            "--gateway-eui",
            "24e124fffefd3bda",
        ])
        .inner,
        &config,
        Some(&fake as &dyn ChirpStack),
        None,
        None,
        store.clone(),
    ))
    .expect("create");

    let outcome = rt()
        .block_on(run_with_store(
            &cli(&[
                "gateway-info",
                "--radio",
                "lorawan",
                "--gateway-eui",
                "24e124fffefd3bda",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        ))
        .expect("gateway-info succeeds");
    match outcome {
        Outcome::Json(v) => {
            assert_eq!(v["gateway_eui"], "24e124fffefd3bda");
            assert_eq!(v["radio"], "lorawan");
            assert_eq!(v["status"], "offline");
            assert!(
                v["tenant_id"].as_str().is_some(),
                "registration record: {v}"
            );
            assert!(
                v["created_at"].as_str().is_some(),
                "registration record: {v}"
            );
        }
        _ => panic!("gateway-info returns Outcome::Json"),
    }

    let err = rt()
        .block_on(run_with_store(
            &cli(&[
                "gateway-info",
                "--radio",
                "lorawan",
                "--gateway-eui",
                "0011223344556677",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect_err("an unregistered gateway has no record");
    assert_eq!(
        err.kind(),
        control_radio::error::Kind::NotFound,
        "F20: the EXISTS-join on GatewayService.Get must not turn this into Unauthenticated"
    );
    assert_eq!(err.exit_code(), 13);
}

/// §5: `node-send --hex` enqueues a raw downlink and confirms *acceptance*. The port is the
/// caller's — §5 pins no default, so `--fport` carries it all the way to ChirpStack.
#[test]
fn node_send_hex_enqueues_the_payload_on_the_given_fport() {
    let (store, _dir) = seeded_store("send-hex");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    enrol_one(&fake, &config, &store, "SN-SEND", "58760700c0668fc7");

    let outcome = rt()
        .block_on(run_with_store(
            &cli(&[
                "node-send",
                "--radio",
                "lorawan",
                "--serial",
                "SN-SEND",
                "--hex",
                "08072200",
                "--fport",
                "85",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect("node-send succeeds");

    match outcome {
        Outcome::Json(v) => {
            assert_eq!(v["command"], "node-send");
            assert_eq!(v["radio"], "lorawan");
            assert_eq!(v["serial_number"], "SN-SEND");
            assert_eq!(v["dev_eui"], "58760700c0668fc7");
            assert_eq!(v["fport"], 85);
            assert_eq!(v["queued"], true, "acceptance, never completion (§9.7)");
        }
        _ => panic!("node-send returns Outcome::Json"),
    }

    let queued = fake.downlinks();
    assert_eq!(queued.len(), 1, "exactly one downlink: {queued:?}");
    assert_eq!(queued[0].dev_eui, "58760700c0668fc7");
    assert_eq!(queued[0].f_port, 85);
    assert_eq!(queued[0].data, vec![0x08, 0x07, 0x22, 0x00]);
    assert!(
        !queued[0].confirmed,
        "§5 exposes no --confirmed flag, so a raw downlink is unconfirmed"
    );
}

/// A malformed payload is refused locally — nothing is queued, because the wrong bytes on the
/// air are worse than a refusal.
#[test]
fn node_send_rejects_a_malformed_hex_payload_before_any_rpc() {
    let (store, _dir) = seeded_store("send-bad-hex");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    enrol_one(&fake, &config, &store, "SN-SEND", "58760700c0668fc7");
    fake.clear_calls();

    for bad in ["0a0", "zz", "0a0b0c0"] {
        let err = match rt().block_on(run_with_store(
            &cli(&[
                "node-send",
                "--radio",
                "lorawan",
                "--serial",
                "SN-SEND",
                "--hex",
                bad,
                "--fport",
                "85",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        )) {
            Err(e) => e,
            Ok(o) => panic!("{bad:?} must be refused, got {o:?}"),
        };
        assert_eq!(
            err.kind(),
            control_radio::error::Kind::InvalidArgument,
            "{bad:?}"
        );
        assert_eq!(err.exit_code(), 2, "{bad:?}");
    }
    assert!(
        fake.downlinks().is_empty(),
        "nothing was queued: {:?}",
        fake.downlinks()
    );
}

/// The structured `<type>` forms encode an fPort-85 `Command` and enqueue it on port 85 — no
/// `--fport` is offered for them, because they are that port by protocol.
#[test]
fn node_send_get_info_enqueues_the_bench_vector_on_fport_85() {
    let (store, _dir) = seeded_store("send-get-info");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    enrol_one(&fake, &config, &store, "SN-SEND", "58760700c0668fc7");

    let outcome = rt()
        .block_on(run_with_store(
            &cli(&[
                "node-send",
                "--radio",
                "lorawan",
                "--serial",
                "SN-SEND",
                "get-info",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect("node-send get-info succeeds");

    match outcome {
        Outcome::Json(v) => {
            assert_eq!(v["command"], "node-send");
            assert_eq!(v["type"], "get-info");
            assert_eq!(v["fport"], 85);
            assert_eq!(v["queued"], true);
            assert_eq!(v["seq"], 1, "the first command this Hub sends this device");
            assert!(v["downlink_id"].is_string());
        }
        other => panic!("node-send returns Outcome::Json, got {other:?}"),
    }

    let queued = fake.downlinks();
    assert_eq!(queued.len(), 1, "exactly one downlink: {queued:?}");
    assert_eq!(queued[0].f_port, 85);
    // The bench vector, with seq 1 rather than 7: `Command { seq, get_info {} }`.
    assert_eq!(queued[0].data, vec![0x08, 0x01, 0x22, 0x00]);
    assert!(!queued[0].confirmed, "unconfirmed — §9.7 correlates by seq");
}

/// sticker-firmware #429: `get-settings` is `Command { seq, get_settings {} }`, field 31, and the
/// Hub remembers it was sent — its answer merges, where a `get-config` answer would replace
/// (node_reported_state.md §2.2).
#[test]
fn node_send_get_settings_enqueues_field_31_and_records_what_was_sent() {
    let (store, _dir) = seeded_store("send-get-settings");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    enrol_one(&fake, &config, &store, "SN-SET", "58760700c0668fc7");

    let outcome = rt()
        .block_on(run_with_store(
            &cli(&[
                "node-send",
                "--radio",
                "lorawan",
                "--serial",
                "SN-SET",
                "get-settings",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        ))
        .expect("node-send get-settings succeeds");
    match outcome {
        Outcome::Json(v) => {
            assert_eq!(v["type"], "get-settings");
            assert_eq!(v["seq"], 1);
        }
        other => panic!("node-send returns Outcome::Json, got {other:?}"),
    }

    let queued = fake.downlinks();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].f_port, 85);
    // The firmware's own vector is `0807fa0100` at seq 7; here seq 1.
    assert_eq!(queued[0].data, vec![0x08, 0x01, 0xfa, 0x01, 0x00]);

    let doc = control_radio::cache::Cache::new(&store, control_radio::error::Transport::Lorawan)
        .read("58760700c0668fc7")
        .expect("reads")
        .expect("the send wrote the node's cache document");
    assert_eq!(
        doc.sent_tag(1),
        Some(control_radio::decode::command::CommandTag::GetSettings as u32)
    );
}

/// #129: a structured LoRaWAN send records ChirpStack's queue-item id next to its seq, so the
/// `event/log` ChirpStack raises when it discards that item at transmit time fails the seq as
/// `undelivered` on the command topic, with the sizes in the reason.
#[test]
fn a_downlink_chirpstack_discards_fails_the_seq_it_carried() {
    let (store, _dir) = seeded_store("send-discarded");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    let dev_eui = "58760700c0668fc7";
    enrol_one(&fake, &config, &store, "SN-DISC", dev_eui);

    let mut sent = Vec::new();
    for _ in 0..2 {
        let outcome = rt()
            .block_on(run_with_store(
                &cli(&[
                    "node-send",
                    "--radio",
                    "lorawan",
                    "--serial",
                    "SN-DISC",
                    "get-settings",
                ])
                .inner,
                &config,
                Some(&fake as &dyn ChirpStack),
                None,
                None,
                store.clone(),
            ))
            .expect("node-send succeeds");
        let Outcome::Json(v) = outcome else {
            panic!("node-send returns Outcome::Json");
        };
        let id = v["downlink_id"]
            .as_str()
            .expect("a downlink_id")
            .to_string();
        sent.push((v["seq"].as_u64().expect("seq") as u32, id));
    }

    let cache = Cache::new(&store, control_radio::error::Transport::Lorawan);
    let doc = cache.read(dev_eui).expect("reads").expect("present");
    for (seq, id) in &sent {
        assert_eq!(doc.seq_for_downlink(id), Some(*seq), "{id}");
    }

    // ChirpStack discards the first frame, as its 4.x JSON marshaler reports it.
    let (seq, item) = &sent[0];
    let payload = serde_json::json!({
        "time": "2026-10-13T12:08:18.412345678+00:00",
        "deviceInfo": {"devEui": dev_eui, "deviceName": "SN-DISC"},
        "level": "ERROR",
        "code": "DOWNLINK_PAYLOAD_SIZE",
        "description": "Device queue-item discarded because it exceeds the max. payload size",
        "context": {"max_payload_size": "11", "item_size": "52", "queue_item_id": item},
    });
    let ev = control_radio::lorawan::event::parse_log(&serde_json::to_vec(&payload).unwrap())
        .expect("parses");
    let nodes = control_radio::lorawan::enroll::NodesFile::load(&store).expect("nodes");
    let topics = Topics::new("hub-test");
    let report = control_radio::tasks::uplink::process_log(
        &ev,
        &control_radio::tasks::uplink::Context {
            config: &config,
            nodes: &nodes,
            topics: &topics,
        },
        |id| cache.read(id).expect("reads"),
    );
    assert!(
        report.line.contains(&format!("seq {seq}")),
        "{}",
        report.line
    );
    let p = report
        .outcome
        .published(&topics, dev_eui, Area::Command)
        .expect("a command document");
    let doc: serde_json::Value = serde_json::from_slice(&p.payload).expect("json");
    assert_eq!(doc["seq"], *seq);
    assert_eq!(doc["status"], "undelivered");
    assert_eq!(doc["serial"], "SN-DISC");
    assert_eq!(
        doc["reason"],
        "discarded by the network server: 52 B exceeds 11 B at the current data rate"
    );
}

/// `get-config --page N --seq S` (#96) goes out under the operator's `seq`, not a fresh one, and
/// is recorded as a GetConfig all the same: without the record its answer — the missing pages
/// of a full dump — would merge instead of replacing (node_reported_state.md §2.2).
#[test]
fn a_get_config_under_an_explicit_seq_is_recorded_as_sent() {
    let (store, _dir) = seeded_store("send-get-config-seq");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    enrol_one(&fake, &config, &store, "SN-SEQ", "58760700c0668fc8");

    let outcome = rt()
        .block_on(run_with_store(
            &cli(&[
                "node-send",
                "--radio",
                "lorawan",
                "--serial",
                "SN-SEQ",
                "get-config",
                "--page",
                "3",
                "--seq",
                "10",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        ))
        .expect("node-send get-config --seq succeeds");
    match outcome {
        Outcome::Json(v) => assert_eq!(v["seq"], 10),
        other => panic!("node-send returns Outcome::Json, got {other:?}"),
    }

    let queued = fake.downlinks();
    assert_eq!(queued.len(), 1);
    // The bench re-request of 2026-09-23 17:47Z, byte for byte.
    assert_eq!(queued[0].data, vec![0x08, 0x0a, 0x2a, 0x02, 0x08, 0x03]);

    let doc = control_radio::cache::Cache::new(&store, control_radio::error::Transport::Lorawan)
        .read("58760700c0668fc8")
        .expect("reads")
        .expect("the send wrote the node's cache document");
    assert_eq!(
        doc.sent_tag(10),
        Some(control_radio::decode::command::CommandTag::GetConfig as u32)
    );
}

/// `get-param` is `Command { seq, get_param { … } }`, tag 3 — one frame for the named keys, and
/// the Hub remembers it was sent, so its answer merges rather than replaces.
#[test]
fn node_send_get_param_enqueues_tag_3_with_packed_fields() {
    let (store, _dir) = seeded_store("send-get-param");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    enrol_one(&fake, &config, &store, "SN-GP", "58760700c0668fc7");

    rt().block_on(run_with_store(
        &cli(&[
            "node-send",
            "--radio",
            "lorawan",
            "--serial",
            "SN-GP",
            "get-param",
            "alarms.alarm_limit",
            "alarms.alarm_0",
        ])
        .inner,
        &config,
        Some(&fake as &dyn ChirpStack),
        None,
        None,
        store.clone(),
    ))
    .expect("node-send get-param succeeds");

    let queued = fake.downlinks();
    assert_eq!(queued.len(), 1, "one frame for every named key");
    // The bench frame `08741a0422020103` at seq 116, here at seq 1.
    assert_eq!(
        queued[0].data,
        vec![0x08, 0x01, 0x1a, 0x04, 0x22, 0x02, 0x01, 0x03]
    );
    let doc = control_radio::cache::Cache::new(&store, control_radio::error::Transport::Lorawan)
        .read("58760700c0668fc7")
        .expect("reads")
        .expect("present");
    assert_eq!(
        doc.sent_tag(1),
        Some(control_radio::decode::command::CommandTag::GetParam as u32)
    );
}

/// A `set-config` records `{seq, tag}` like every structured send, and nothing more: the ack
/// fold is withdrawn (node_reported_state.md §2.2, 2026-09-28, Q-N3), so no `written` body is
/// kept for the Node's ack to fold into the config.
#[test]
fn node_send_set_config_records_the_seq_and_not_what_it_writes() {
    let (store, _dir) = seeded_store("send-set-written");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    enrol_one(&fake, &config, &store, "SN-W", "58760700c0668fc7");

    rt().block_on(run_with_store(
        &cli(&[
            "node-send",
            "--radio",
            "lorawan",
            "--serial",
            "SN-W",
            "set-config",
            "application.interval_report=60",
        ])
        .inner,
        &config,
        Some(&fake as &dyn ChirpStack),
        None,
        None,
        store.clone(),
    ))
    .expect("node-send set-config succeeds");

    let doc = control_radio::cache::Cache::new(&store, control_radio::error::Transport::Lorawan)
        .read("58760700c0668fc7")
        .expect("reads")
        .expect("present");
    assert_eq!(
        doc.sent_tag(1),
        Some(control_radio::decode::command::CommandTag::SetParam as u32)
    );
    let entry = &doc.slots[control_radio::cache::SENT_COMMANDS_KEY][0];
    assert!(entry.get("written").is_none(), "{entry}");
}

/// §9.7: the sequence is per device and monotonic. A CLI invocation is a process that exits,
/// so this only holds if the high-water mark survives it.
#[test]
fn structured_sends_carry_a_monotonic_per_device_sequence() {
    let (store, _dir) = seeded_store("send-seq");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    enrol_one(&fake, &config, &store, "SN-A", "58760700c0668fc7");
    enrol_one(&fake, &config, &store, "SN-B", "58760700c0668fc8");

    let send = |serial: &str, what: &str| {
        rt().block_on(run_with_store(
            &cli(&["node-send", "--radio", "lorawan", "--serial", serial, what]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        ))
        .expect("node-send succeeds")
    };

    for expect in [1u64, 2, 3] {
        match send("SN-A", "get-config") {
            Outcome::Json(v) => assert_eq!(v["seq"], expect),
            other => panic!("{other:?}"),
        }
    }
    // A different device has its own counter — the sequence is per device, not per Hub.
    match send("SN-B", "get-info") {
        Outcome::Json(v) => assert_eq!(v["seq"], 1),
        other => panic!("{other:?}"),
    }

    let queued = fake.downlinks();
    assert_eq!(queued.len(), 4);
    // seq 1..3 of get_config on A, then seq 1 of get_info on B.
    assert_eq!(queued[0].data, vec![0x08, 0x01, 0x2a, 0x00]);
    assert_eq!(queued[2].data, vec![0x08, 0x03, 0x2a, 0x00]);
    assert_eq!(queued[3].dev_eui, "58760700c0668fc8");
    assert_eq!(queued[3].data, vec![0x08, 0x01, 0x22, 0x00]);
}

/// §9.7: a multi-key `set-config` is a sequential per-key exchange, so it is one frame per key
/// — which is also the only shape that cannot overflow the 51 B DR0 downlink budget.
#[test]
fn set_config_packs_its_keys_into_one_frame_and_names_them() {
    let (store, _dir) = seeded_store("send-set-config");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    enrol_one(&fake, &config, &store, "SN-SET", "58760700c0668fc7");

    let outcome = rt()
        .block_on(run_with_store(
            &cli(&[
                "node-send",
                "--radio",
                "lorawan",
                "--serial",
                "SN-SET",
                "set-config",
                "application.interval_report=900",
                "sensors.cap_barometer=true",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect("node-send set-config succeeds");

    match outcome {
        Outcome::Json(v) => {
            assert_eq!(v["type"], "set-config");
            assert_eq!(
                v["keys"],
                serde_json::json!(["application.interval_report", "sensors.cap_barometer"])
            );
            let sent = v["downlinks"].as_array().expect("one row per frame");
            assert_eq!(sent.len(), 1, "two small keys fit one DR0 frame");
            assert_eq!(
                sent[0]["keys"],
                serde_json::json!(["application.interval_report", "sensors.cap_barometer"])
            );
            assert!(sent[0].get("key").is_none(), "`key` is for a one-key frame");
            assert_eq!(sent[0]["seq"], 1);
        }
        other => panic!("{other:?}"),
    }

    let queued = fake.downlinks();
    assert_eq!(queued.len(), 1, "packed into one frame");
    for d in &queued {
        assert_eq!(d.f_port, 85);
        assert!(
            d.data.len() <= 51,
            "inside the DR0 budget: {} B",
            d.data.len()
        );
    }
    // The frame carries `save = true`, so the staged set is committed once.
    assert!(
        queued[0].data.ends_with(&[0x18, 0x01]),
        "the last frame saves: {:02x?}",
        queued[0].data
    );
}

/// The alarm table (nodes_phase1.cs.md WP2) packs by the Node's own data rate, read from its
/// last uplink in the cache: ten filled slots are one frame at DR5 and several at DR0, with
/// `alarms_replace` on the first frame only.
#[test]
fn set_config_packs_by_the_nodes_data_rate() {
    let slots: Vec<String> = (0..10)
        .map(|i| format!("alarms.alarm_{i}=0300000100000000a0410000c84100000000"))
        .collect();
    let send = |dr: Option<u64>| {
        let (store, _dir) = seeded_store("send-set-config-dr");
        let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
        let fake = FakeChirpStack::new();
        enrol_one(&fake, &config, &store, "SN-DR", "58760700c0668fc7");
        if let Some(dr) = dr {
            Cache::new(&store, control_radio::error::Transport::Lorawan)
                .merge_write(
                    "58760700c0668fc7",
                    CacheSlot::Message,
                    "2026-09-24T10:00:00Z",
                    serde_json::json!({ "dr": dr }),
                )
                .expect("cache");
        }
        fake.clear_calls();
        let mut argv = vec![
            "node-send",
            "--radio",
            "lorawan",
            "--serial",
            "SN-DR",
            "set-config",
            "--replace-alarms",
        ];
        argv.extend(slots.iter().map(String::as_str));
        rt().block_on(run_with_store(
            &cli(&argv).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect("node-send set-config succeeds");
        fake.downlinks()
    };

    let dr5 = send(Some(5));
    assert_eq!(dr5.len(), 1, "one frame at DR5");
    assert!(dr5[0].data.len() <= 222);

    let dr0 = send(None);
    assert!(dr0.len() > 1, "no known data rate plans for DR0");
    let replace = [0x30u8, 0x01];
    for (i, d) in dr0.iter().enumerate() {
        assert!(d.data.len() <= 51, "frame {i} is {} B", d.data.len());
        let has_replace = d.data.windows(2).any(|w| w == replace);
        assert_eq!(
            has_replace,
            i == 0,
            "alarms_replace rides the first frame only"
        );
    }
}

/// The NFC/shell-only operations have no CLI spelling at all, and a bad key is refused before
/// anything reaches the air — the wrong bytes cost an RX window the operator cannot get back.
#[test]
fn unspellable_commands_and_bad_keys_never_reach_the_radio() {
    let (store, _dir) = seeded_store("send-refused");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    enrol_one(&fake, &config, &store, "SN-NO", "58760700c0668fc7");
    fake.clear_calls();

    let refused: Vec<Vec<&str>> = vec![
        vec!["cmd", "device-reset"],
        vec!["cmd", "factory-reset"],
        vec!["cmd", "set-secret-key"],
        vec!["cmd", "req-history-page"],
        vec!["cmd", "reset-counters"], // no --counter: an empty selection clears everything
        vec![
            "set-config",
            "lorawan.appkey=000102030405060708090a0b0c0d0e0f",
        ],
        vec!["set-config", "application.no_such_key=1"],
        vec!["set-config", "application.interval_report=soon"],
    ];
    for tail in refused {
        let mut args = vec!["node-send", "--radio", "lorawan", "--serial", "SN-NO"];
        args.extend(tail.iter().copied());
        let err = match rt().block_on(run_with_store(
            &cli(&args).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        )) {
            Err(e) => e,
            Ok(o) => panic!("{tail:?} must be refused, got {o:?}"),
        };
        assert_eq!(
            err.kind(),
            control_radio::error::Kind::InvalidArgument,
            "{tail:?}: {}",
            err.message()
        );
        assert_eq!(err.exit_code(), 2, "{tail:?}");
    }
    assert!(
        fake.downlinks().is_empty(),
        "nothing was queued: {:?}",
        fake.downlinks()
    );
}

// `node-get`'s own behaviour lives in `tests/node_get.rs`: it turns on the heartbeat file,
// whose path is a process-global environment override, so those tests serialise among
// themselves in a binary of their own rather than racing every test here.

/// The `node-get` cache is *"wiped by `clear`/`wipe`"* (plan.md, Runtime form). A device that
/// has been deleted must not still answer `node-get` from data it reported while enrolled.
#[test]
fn clear_and_wipe_empty_the_node_get_cache() {
    for command in ["clear", "wipe"] {
        let (store, _dir) = seeded_store(&format!("cache-{command}"));
        let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
        let fake = FakeChirpStack::new();
        enrol_one(&fake, &config, &store, "SN-C", "58760700c0668fc7");

        let cache = Cache::new(&store, control_radio::error::Transport::Lorawan);
        cache
            .merge_write(
                "58760700c0668fc7",
                CacheSlot::Message,
                "2026-08-27T10:00:00Z",
                serde_json::json!({"voltage": 3.0}),
            )
            .expect("seeds the cache");
        assert_eq!(cache.ids().expect("ids"), vec!["58760700c0668fc7"]);

        rt().block_on(run_with_store(
            &cli(&[command, "--radio", "lorawan", "--force"]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        ))
        .unwrap_or_else(|e| panic!("{command}: {e}"));

        assert!(
            cache.ids().expect("ids").is_empty(),
            "{command} left cached radio data behind"
        );
    }
}

/// `node-remove` drops the cache with the record, for the same reason it tombstones the
/// retained topic: a removed node must not linger anywhere an operator can read it.
#[test]
fn node_remove_drops_the_cached_radio_data() {
    let (store, _dir) = seeded_store("cache-rm");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    enrol_one(&fake, &config, &store, "SN-RMC", "58760700c0668fc7");

    let cache = Cache::new(&store, control_radio::error::Transport::Lorawan);
    cache
        .merge_write(
            "58760700c0668fc7",
            CacheSlot::Message,
            "2026-08-27T10:00:00Z",
            serde_json::json!({"voltage": 3.0}),
        )
        .expect("seeds the cache");

    let pubr = FakePublisher::new();
    rt().block_on(run_with_store(
        &cli(&["node-remove", "--radio", "lorawan", "--serial", "SN-RMC"]).inner,
        &config,
        Some(&fake as &dyn ChirpStack),
        None,
        Some(&pubr as &dyn RetainedPublisher),
        store.clone(),
    ))
    .expect("node-remove");

    assert!(cache.ids().expect("ids").is_empty());
}

/// The DevEUI every seeded P2P node in this file carries — the registry and session key (#73).
const P2P_DEV_EUI: &str = "70b3d57ed0000abe";

/// A small seeded P2P state: one registered node, optionally with a live session, so the
/// `node-send` dispatch arm can be exercised without running the radio runtime.
/// `plan.md` §5 "`--json` shapes — golden baseline" is the contract both arms answer to, and the
/// LoRaWAN arm conforms. The P2P arm renamed half of it (`serial`, `state`), omitted `transport`,
/// `net_id`, `last_seen` and `gateways`, wrapped the payload in a `{command, radio}` envelope the
/// baseline does not have, and emitted `last_counter` — an anti-replay counter the baseline
/// explicitly keeps out of `node-list`. A consumer written against §5 reads a P2P node as a
/// different kind of object from a LoRaWAN one.
///
/// `pairing_status` and the key material stay out until the §5 vocabulary question is settled.
#[test]
fn p2p_node_list_and_node_info_carry_the_section_5_common_core() {
    let (store, _dir) = seeded_store("p2p-section-5-shapes");
    let config = p2p_dual_config();
    seed_p2p_node(&store, "80e007b2", true);
    let fake = FakeChirpStack::new();

    let list = rt()
        .block_on(run_with_store(
            &cli(&["node-list", "--radio", "p2p"]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        ))
        .expect("node-list");
    let list = match list {
        Outcome::Json(v) => v,
        other => panic!("expected Outcome::Json, got {other:?}"),
    };
    let top: Vec<&str> = list
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        top,
        vec!["nodes"],
        "the baseline has no {{command, radio}} envelope"
    );

    let row = &list["nodes"][0];
    let mut keys: Vec<&str> = row
        .as_object()
        .expect("a row")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "dev_addr",
            "dev_eui",
            "gateways",
            "last_seen",
            "net_id",
            "profile",
            "serial_number",
            "session_state",
            "transport",
        ]
    );
    assert_eq!(row["transport"], "p2p");
    assert_eq!(row["serial_number"], "80e007b2");
    assert_eq!(row["session_state"], "joined");
    assert!(
        !row.as_object().expect("a row").contains_key("last_counter"),
        "§5 keeps anti-replay counters out of node-list"
    );

    let info = rt()
        .block_on(run_with_store(
            &cli(&["node-info", "--radio", "p2p", "--dev-eui", P2P_DEV_EUI]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect("node-info");
    let info = match info {
        Outcome::Json(v) => v,
        other => panic!("expected Outcome::Json, got {other:?}"),
    };
    let mut keys: Vec<&str> = info
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "dev_addr",
            "dev_eui",
            "dev_nonce",
            "downlink_pending",
            "last_join_at",
            "net_id",
            "profile",
            "serial_number",
            "session_state",
            "transport",
            "uplink_counter",
        ]
    );
    assert_eq!(info["transport"], "p2p");
    assert!(
        !info.as_object().expect("an object").contains_key("app_key"),
        "node-info prints no key material on either arm"
    );
}

fn seed_p2p_node(store: &Store, serial: &str, with_session: bool) {
    use control_radio::p2p::db::SessionsDb;
    use control_radio::p2p::session::{DownlinkPhase, RegisteredNode, Session};
    let mut db = SessionsDb::load(store).expect("load");
    db.upsert_node(RegisteredNode {
        serial: serial.into(),
        dev_eui: P2P_DEV_EUI.into(),
        profile: "default".into(),
        app_key: "00".repeat(16),
        name: None,
    });
    if with_session {
        db.sessions.insert(
            P2P_DEV_EUI.into(),
            Session {
                dev_eui: P2P_DEV_EUI.into(),
                addr: control_radio::p2p::tower::addr_of(P2P_DEV_EUI).expect("a DevEUI"),
                session_key: "00".repeat(16),
                dev_nonce: 0,
                last_seen: 0,
                joined_at: None,
                proto_version: 2,
                capabilities: None,
                hello: None,
                downlink_queue: Default::default(),
                downlink_phase: DownlinkPhase::Idle,
                history: None,
                detach: None,
                home: None,
            },
        );
    }
    db.save(store).expect("save");
}

fn p2p_dual_config() -> RadioConfig {
    let dual = PLAN_EXAMPLE.replacen("mode: lorawan", "mode: dual", 1);
    RadioConfig::from_device_config_yaml(&dual).expect("dual-mode plan example")
}

/// S2 (FW B4): `node-send --radio p2p --hex` queues a raw downlink onto a joined node's per-node
/// queue and reports the new depth. The runtime drains it on the node's next uplink.
#[test]
fn node_send_over_p2p_queues_a_hex_downlink() {
    let (store, _dir) = seeded_store("p2p-send-ok");
    let config = p2p_dual_config();
    seed_p2p_node(&store, "80e007b2", true);
    let fake = FakeChirpStack::new();
    let outcome = rt()
        .block_on(run_with_store(
            &cli(&[
                "node-send",
                "--radio",
                "p2p",
                "--serial",
                "0x80e007b2",
                "--fport",
                "85",
                "--hex",
                "1122",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        ))
        .expect("a live session accepts a queued downlink");
    match outcome {
        Outcome::Json(v) => {
            assert_eq!(v["command"], "node-send");
            assert_eq!(v["radio"], "p2p");
            assert_eq!(v["serial_number"], "80e007b2");
            assert_eq!(v["state"], "queued");
            assert_eq!(v["queue_depth"], 1);
            assert_eq!(v["fport"], 85);
            // §5's baseline: the wire identity and the boolean the LoRaWAN arm answers with.
            assert_eq!(v["dev_eui"], P2P_DEV_EUI);
            assert_eq!(v["queued"], true);
        }
        other => panic!("expected JSON, got {other:?}"),
    }
    // The payload landed on the session's queue, decoded from hex.
    let db = control_radio::p2p::db::SessionsDb::load(&store).expect("reload");
    let q = &db
        .sessions
        .get(P2P_DEV_EUI)
        .expect("session")
        .downlink_queue;
    assert_eq!(
        q.front().map(|v| v.body.as_slice()),
        Some([0x11, 0x22].as_slice())
    );
}

/// A P2P frame carries no port field — the `0x56` body *is* an fPort-85 command (`p2p.md` §6.2).
/// clap requires `--fport` with `--hex`; 85 is the only value this arm can honour, and any other
/// is refused rather than silently ignored.
#[test]
fn node_send_over_p2p_accepts_only_fport_85() {
    let (store, _dir) = seeded_store("p2p-send-fport");
    let config = p2p_dual_config();
    seed_p2p_node(&store, "80e007b2", true);
    let fake = FakeChirpStack::new();
    let err = rt()
        .block_on(run_with_store(
            &cli(&[
                "node-send",
                "--radio",
                "p2p",
                "--serial",
                "0x80e007b2",
                "--fport",
                "2",
                "--hex",
                "1122",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        ))
        .expect_err("fPort 2 is not a command port");
    assert_eq!(err.kind(), control_radio::error::Kind::InvalidArgument);
    // Refused before the queue was touched.
    let db = control_radio::p2p::db::SessionsDb::load(&store).expect("reload");
    assert!(db
        .sessions
        .get(P2P_DEV_EUI)
        .expect("session")
        .downlink_queue
        .is_empty());
}

/// `node-send` to a node with no live session is `NotFound`: a P2P downlink is sealed under the
/// session key, so there is nothing to queue it against until the node has joined.
#[test]
fn node_send_over_p2p_needs_a_live_session() {
    let (store, _dir) = seeded_store("p2p-send-nosession");
    let config = p2p_dual_config();
    seed_p2p_node(&store, "80e007b2", false); // registered, never joined
    let fake = FakeChirpStack::new();
    let err = rt()
        .block_on(run_with_store(
            &cli(&[
                "node-send",
                "--radio",
                "p2p",
                "--serial",
                "0x80e007b2",
                "--fport",
                "85",
                "--hex",
                "aa",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect_err("no session to seal a downlink under");
    assert_eq!(err.kind(), control_radio::error::Kind::NotFound);
}

/// `node-send` to an unregistered serial is `NotFound` on the registry — the same source of truth
/// `node-remove` and `node-info` use.
#[test]
fn node_send_over_p2p_to_an_unregistered_serial_is_not_found() {
    let (store, _dir) = seeded_store("p2p-send-unreg");
    let config = p2p_dual_config();
    let fake = FakeChirpStack::new();
    let err = rt()
        .block_on(run_with_store(
            &cli(&[
                "node-send",
                "--radio",
                "p2p",
                "--serial",
                "0x80e007b2",
                "--fport",
                "85",
                "--hex",
                "aa",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect_err("nothing registered with that serial");
    assert_eq!(err.kind(), control_radio::error::Kind::NotFound);
}

/// `--dev-eui` is the canonical selector since #73: it is the registry key and the session
/// key, so it addresses both directly. The answer still reports `serial_number`, which now
/// comes out of the registry record rather than off the wire.
#[test]
fn node_send_by_dev_eui_needs_no_registry_scan() {
    let (store, _dir) = seeded_store("p2p-send-by-eui");
    let config = p2p_dual_config();
    seed_p2p_node(&store, "80e007b2", true);
    let fake = FakeChirpStack::new();
    let outcome = rt()
        .block_on(run_with_store(
            &cli(&[
                "node-send",
                "--radio",
                "p2p",
                "--dev-eui",
                P2P_DEV_EUI,
                "--fport",
                "85",
                "--hex",
                "aa",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        ))
        .expect("the dev_eui addresses the session");
    match outcome {
        Outcome::Json(v) => {
            assert_eq!(v["state"], "queued");
            assert_eq!(
                v["serial_number"], "80e007b2",
                "the serial comes from the registry record, not the selector"
            );
        }
        other => panic!("expected Outcome::Json, got {other:?}"),
    }
    let db = control_radio::p2p::db::SessionsDb::load(&store).expect("reload");
    assert_eq!(
        db.sessions[P2P_DEV_EUI].downlink_queue.len(),
        1,
        "queued against the dev_eui-keyed session"
    );
}

/// The DevEUI half of the registration gate: an identity nobody registered is a
/// `node-registry` miss before the session is even consulted.
#[test]
fn node_send_over_p2p_to_an_unregistered_node_is_not_found() {
    let (store, _dir) = seeded_store("p2p-send-unreg-eui");
    let config = p2p_dual_config();
    seed_p2p_node(&store, "80e007b2", true);
    let fake = FakeChirpStack::new();
    let err = rt()
        .block_on(run_with_store(
            &cli(&[
                "node-send",
                "--radio",
                "p2p",
                "--dev-eui",
                "70b3d57ed0000abf",
                "--fport",
                "85",
                "--hex",
                "aa",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect_err("an unregistered dev_eui has no record");
    assert_eq!(err.kind(), control_radio::error::Kind::NotFound);
    assert_eq!(
        err.detail().component,
        control_radio::error::Component::NodeRegistry,
        "refused on the registry, not on the missing session"
    );
}

/// D3: a node removed while it has *no* live session will never transmit again, so the
/// runtime's detach path — which clears the retained `info` for a live session — can never
/// run for it. The CLI arm has to tombstone here or the node stays in every subscriber's
/// inventory forever, because retained means forever.
#[test]
fn p2p_node_remove_without_a_session_tombstones_the_info() {
    let (store, _dir) = seeded_store("p2p-remove-tombstone");
    let config = p2p_dual_config();
    seed_p2p_node(&store, "80e007b2", false);
    let fake = FakeChirpStack::new();
    let pubr = FakePublisher::new();

    let outcome = rt()
        .block_on(run_with_store(
            &cli(&["node-remove", "--radio", "p2p", "--dev-eui", P2P_DEV_EUI]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            Some(&pubr as &dyn RetainedPublisher),
            store,
        ))
        .expect("node-remove");
    match outcome {
        Outcome::Json(v) => assert_eq!(v["state"], "deleted", "no session to detach"),
        other => panic!("expected Outcome::Json, got {other:?}"),
    }

    // Every retained area goes: `info`, and since !99 the Node's own `device_info` and
    // `config` (node_reported_state.md §2), which would otherwise outlive the removal.
    let retained = |area: &str| info_topic(P2P_DEV_EUI).replace("/info", &format!("/{area}"));
    assert_eq!(
        pubr.published(),
        vec![
            (info_topic(P2P_DEV_EUI), Vec::new()),
            (retained("device_info"), Vec::new()),
            (retained("config"), Vec::new()),
            (retained("device_status"), Vec::new()),
        ],
        "an empty retained payload on each retained topic is the tombstone"
    );
}

/// The other half of that guarantee: if the tombstone is what makes the removal real, then a
/// removal that cannot publish one must not happen at all.
///
/// The arm used to mutate first and publish `if let Some(broker)`, so with no broker it
/// deregistered the node and answered `state: "deleted"` while every subscriber kept the node in
/// its inventory forever. Re-running the command could not repair it either — the registry entry
/// the operator's identifier resolves through was already gone, so the second run answers
/// `NotFound`. `plan.md` §5 ¶467-481 asks for `ServiceDown` naming the device instead, which is
/// what the LoRaWAN arm has always done.
#[test]
fn p2p_node_remove_without_a_session_and_no_broker_is_service_down() {
    let (store, _dir) = seeded_store("p2p-remove-no-broker");
    let config = p2p_dual_config();
    seed_p2p_node(&store, "80e007b2", false);
    let fake = FakeChirpStack::new();

    let err = rt()
        .block_on(run_with_store(
            &cli(&["node-remove", "--radio", "p2p", "--dev-eui", P2P_DEV_EUI]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        ))
        .expect_err("no broker to tombstone on");

    assert_eq!(err.kind(), control_radio::error::Kind::ServiceDown);
    assert_eq!(
        err.component(),
        control_radio::error::Component::MqttBroker,
        "the broker is what is down, not the registry"
    );
    assert!(
        control_radio::p2p::db::SessionsDb::load(&store)
            .expect("load")
            .node(P2P_DEV_EUI)
            .is_some(),
        "nothing was mutated before the refusal, so the operator can re-run by the same identifier"
    );
}

/// A node that is reflashed comes back with its `dev_nonce` counter at zero, but a session
/// left behind would hold the old high-water — so every JoinReq it sends would read as a replay
/// and it would never rejoin.
///
/// So removing a node removes its session, and re-adding one starts from a clean mark even if a
/// stale session survived some other way. Otherwise the only escape is `clear --force`, which
/// takes every other node with it.
#[test]
fn a_re_registered_node_starts_from_a_clean_replay_mark() {
    let keys = KeysFile::new("replay");
    let (store, _dir) = seeded_store("p2p-replay-reset");
    let config = p2p_dual_config();
    let fake = FakeChirpStack::new();
    let pubr = FakePublisher::new();

    // A joined node whose dev_nonce high-water has climbed.
    seed_p2p_node(&store, "80e007b2", true);
    {
        use control_radio::p2p::db::SessionsDb;
        let mut db = SessionsDb::load(&store).expect("load");
        db.sessions.get_mut(P2P_DEV_EUI).expect("session").dev_nonce = 9_000;
        db.save(&store).expect("save");
    }

    // The operator removes it.
    rt().block_on(run_with_store(
        &cli(&["node-remove", "--radio", "p2p", "--dev-eui", P2P_DEV_EUI]).inner,
        &config,
        Some(&fake as &dyn ChirpStack),
        None,
        Some(&pubr as &dyn RetainedPublisher),
        store.clone(),
    ))
    .expect("node-remove");
    {
        use control_radio::p2p::db::SessionsDb;
        let db = SessionsDb::load(&store).expect("reload");
        assert!(
            !db.sessions.contains_key(P2P_DEV_EUI),
            "the session and its replay mark go with the node, so a reflash can rejoin"
        );
    }

    // And re-adding starts clean, however the session got there.
    seed_p2p_node(&store, "80e007b2", true);
    {
        use control_radio::p2p::db::SessionsDb;
        let mut db = SessionsDb::load(&store).expect("load");
        db.nodes.remove(P2P_DEV_EUI);
        db.save(&store).expect("save");
    }
    p2p_node_add(store.clone(), "2162165682", P2P_DEV_EUI, &keys.0).expect("re-add");
    {
        use control_radio::p2p::db::SessionsDb;
        let db = SessionsDb::load(&store).expect("reload");
        assert!(
            !db.sessions.contains_key(P2P_DEV_EUI),
            "a fresh registration inherits no replay state from the previous one"
        );
    }
}

/// `--serial` survives as an alias, and `node-remove` is where it matters most: the serial is
/// what is printed on the device, so an operator holding only that must still be able to
/// remove the node. It resolves through the registry to the dev_eui both maps are keyed by.
#[test]
fn node_remove_by_serial_resolves_through_the_registry() {
    let (store, _dir) = seeded_store("p2p-remove-by-serial");
    let config = p2p_dual_config();
    seed_p2p_node(&store, "80e007b2", true);
    let fake = FakeChirpStack::new();
    let pubr = FakePublisher::new();
    let outcome = rt()
        .block_on(run_with_store(
            &cli(&["node-remove", "--radio", "p2p", "--serial", "0x80e007b2"]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            Some(&pubr as &dyn RetainedPublisher),
            store.clone(),
        ))
        .expect("the serial resolves through the registry");
    match outcome {
        Outcome::Json(v) => {
            assert_eq!(v["dev_eui"], P2P_DEV_EUI);
            assert_eq!(v["serial_number"], "80e007b2");
            assert_eq!(
                v["state"], "deleted",
                "the session ends with the registration; the central takes the address off \
                 the gateway"
            );
        }
        other => panic!("expected Outcome::Json, got {other:?}"),
    }
    let db = control_radio::p2p::db::SessionsDb::load(&store).expect("reload");
    assert!(
        db.node(P2P_DEV_EUI).is_none(),
        "the dev_eui-keyed record is the one that went"
    );
    assert!(
        !db.sessions.contains_key(P2P_DEV_EUI),
        "and its session with it"
    );
    assert_eq!(
        pubr.published().len(),
        4,
        "the retained areas are tombstoned"
    );
}

/// The structured forms encode an fPort-85 `Command` with §9.7's `seq` from the node's own
/// persistent allocator and queue the bytes — the same path the LoRaWAN arm takes, minus
/// ChirpStack. `seq` is never 0, so it can never match a queue head that carried none.
#[test]
fn structured_node_send_over_p2p_queues_a_seq_carrying_command() {
    let (store, _dir) = seeded_store("p2p-send-structured");
    let config = p2p_dual_config();
    seed_p2p_node(&store, "80e007b2", true);
    let fake = FakeChirpStack::new();
    let outcome = rt()
        .block_on(run_with_store(
            &cli(&[
                "node-send",
                "--radio",
                "p2p",
                "--serial",
                "0x80e007b2",
                "get-info",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        ))
        .expect("the structured form encodes and queues");
    match outcome {
        Outcome::Json(v) => {
            assert_eq!(v["command"], "node-send");
            assert_eq!(v["radio"], "p2p");
            assert_eq!(v["fport"], 85);
            assert_eq!(v["queue_depth"], 1);
            assert_ne!(v["seq"], 0, "the allocator never issues 0");
        }
        other => panic!("expected JSON, got {other:?}"),
    }
    let db = control_radio::p2p::db::SessionsDb::load(&store).expect("reload");
    let q = &db
        .sessions
        .get(P2P_DEV_EUI)
        .expect("session")
        .downlink_queue;
    assert_eq!(q.len(), 1, "one frame queued");
    assert!(
        !q.front().expect("head").body.is_empty(),
        "encoded body present"
    );
}

/// F3: a P2P downlink only moves when the node transmits, so the queue is bounded. The eighth
/// entry is accepted, the ninth is refused — and a refusal adds nothing, so the operator can see
/// exactly what is waiting.
#[test]
fn node_send_over_p2p_refuses_a_ninth_queued_downlink() {
    let (store, _dir) = seeded_store("p2p-send-cap");
    let config = p2p_dual_config();
    seed_p2p_node(&store, "80e007b2", true);
    let fake = FakeChirpStack::new();
    let send = |store: Store| {
        rt().block_on(run_with_store(
            &cli(&[
                "node-send",
                "--radio",
                "p2p",
                "--serial",
                "0x80e007b2",
                "--fport",
                "85",
                "--hex",
                "aa",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
    };

    for i in 1..=control_radio::p2p::db::MAX_QUEUED_DOWNLINKS {
        let outcome = send(store.clone()).unwrap_or_else(|e| panic!("downlink {i}: {e}"));
        match outcome {
            Outcome::Json(v) => assert_eq!(v["queue_depth"], i),
            other => panic!("expected JSON, got {other:?}"),
        }
    }

    let err = send(store.clone()).expect_err("the queue is full");
    assert_eq!(err.kind(), control_radio::error::Kind::InvalidArgument);
    assert_eq!(
        err.detail().component,
        control_radio::error::Component::SessionDb
    );
    let db = control_radio::p2p::db::SessionsDb::load(&store).expect("reload");
    assert_eq!(
        db.sessions[P2P_DEV_EUI].downlink_queue.len(),
        control_radio::p2p::db::MAX_QUEUED_DOWNLINKS,
        "a refused node-send queues nothing"
    );
}

/// Decision #4: the P2P `node-get` cache is the shared `p2p/cache/{dev_eui}.json` document, not
/// the retired `SessionsDb.cache` map — so the two commands that delete a node have to delete
/// it too. Until they did, a removed node left its last reading behind, and re-registering
/// the same identity inherited the previous device's data. The cache id is the dev_eui on both
/// carriers since #73, which is what the `--serial` alias below has to resolve to.
#[test]
fn p2p_node_remove_and_clear_drop_the_shared_cache() {
    let config = p2p_dual_config();
    let fake = FakeChirpStack::new();
    let seed = |store: &Store| {
        seed_p2p_node(store, "80e007b2", true);
        Cache::new(store, control_radio::error::Transport::P2p)
            .merge_write(
                P2P_DEV_EUI,
                CacheSlot::Message,
                "2026-09-14T10:00:00Z",
                serde_json::json!({"fields": {"voltage": 3.36}}),
            )
            .expect("seeds the P2P cache");
    };
    let p2p_cache = |store: &Store| {
        Cache::new(store, control_radio::error::Transport::P2p)
            .ids()
            .expect("ids")
    };

    for args in [
        vec!["node-remove", "--radio", "p2p", "--serial", "0x80e007b2"],
        vec!["clear", "--radio", "p2p", "--force"],
    ] {
        let (store, _dir) = seeded_store(&format!("p2p-drop-{}", args[0]));
        seed(&store);
        assert_eq!(p2p_cache(&store), vec![P2P_DEV_EUI], "{args:?}: seeded");

        rt().block_on(run_with_store(
            &cli(&args).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            Some(&FakePublisher::new() as &dyn RetainedPublisher),
            store.clone(),
        ))
        .unwrap_or_else(|e| panic!("{args:?}: {e}"));

        assert!(
            p2p_cache(&store).is_empty(),
            "{args:?} left {:?} behind",
            p2p_cache(&store)
        );
    }
}

/// `clear` keeps the gateway TX counter floor with the `net_id` it belongs to; `wipe`, which
/// replaces the `net_id`, takes both (p2p_tower_gateway.md §3.4).
#[test]
fn clear_keeps_the_p2p_counter_floor_and_wipe_drops_it() {
    use control_radio::p2p::db::SessionsDb;

    let config = p2p_dual_config();
    let fake = FakeChirpStack::new();
    for (command, floor) in [("clear", Some(8192)), ("wipe", None)] {
        let (store, _dir) = seeded_store(&format!("p2p-floor-{command}"));
        seed_p2p_node(&store, "80e007b2", true);
        {
            let _lock = store
                .lock(control_radio::store::Slot::P2pSessions)
                .expect("lock");
            let mut db = SessionsDb::load(&store).expect("load");
            db.ctr_block_end = 8192;
            db.save(&store).expect("save");
        }
        rt().block_on(run_with_store(
            &cli(&[command, "--radio", "p2p", "--force"]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            Some(&FakePublisher::new() as &dyn RetainedPublisher),
            store.clone(),
        ))
        .unwrap_or_else(|e| panic!("{command}: {e}"));
        let db = SessionsDb::load(&store).expect("load");
        assert!(db.nodes.is_empty() && db.sessions.is_empty(), "{command}");
        assert_eq!(
            store
                .exists(control_radio::store::Slot::P2pSessions)
                .then_some(db.ctr_block_end),
            floor,
            "{command}"
        );
    }
}

/// C7: `radio-set --radio p2p --spreading-factor <7..12>` records an **effective** spreading
/// factor in `sessions.db`, leaving the Hub config file untouched (decision #4). Each set bumps
/// a generation, which is what the running central watches; `--reset` drops the override and the
/// config key stands again.
#[test]
fn radio_set_over_p2p_records_the_effective_spreading_factor_under_the_lock() {
    use control_radio::p2p::db::SessionsDb;

    let (store, dir) = seeded_store("radio-set");
    let config = p2p_dual_config(); // the plan example's p2p_radio.spreading_factor is 10
    let fake = FakeChirpStack::new();

    let set = |sf: &str| {
        rt().block_on(run_with_store(
            &cli(&["radio-set", "--radio", "p2p", "--spreading-factor", sf]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        ))
    };

    match set("9").expect("sf 9 is in range") {
        Outcome::Json(v) => {
            assert_eq!(v["success"], true);
            assert_eq!(v["command"], "radio-set");
            assert_eq!(v["radio"], "p2p");
            assert_eq!(v["spreading_factor"], 9);
            assert_eq!(v["config_spreading_factor"], 10);
            assert_eq!(v["effective_spreading_factor"], 9);
            assert_eq!(v["generation"], 1, "the first set is generation 1");
        }
        other => panic!("expected JSON, got {other:?}"),
    }
    let db = SessionsDb::load(&store).expect("reload");
    let radio = db.radio.clone().expect("an override was recorded");
    assert_eq!(radio.spreading_factor, 9);
    assert_eq!(radio.generation, 1);
    assert!(
        !radio.set_at.is_empty(),
        "the record says when the operator moved it"
    );
    // Written under `Slot::P2pSessions`, so the running central's own load-mutate-save cannot
    // lose it: the sidecar flock file only exists because the arm took that lock.
    assert!(
        dir.0.join("p2p/sessions.lock").exists(),
        "the write went through the session-DB lock"
    );

    // A second set replaces the value and advances the generation the central compares against.
    match set("8").expect("sf 8 is in range") {
        Outcome::Json(v) => {
            assert_eq!(v["effective_spreading_factor"], 8);
            assert_eq!(v["config_spreading_factor"], 10);
            assert_eq!(v["generation"], 2);
        }
        other => panic!("expected JSON, got {other:?}"),
    }
    let db = SessionsDb::load(&store).expect("reload");
    assert_eq!(
        db.radio
            .as_ref()
            .map(|r| (r.spreading_factor, r.generation)),
        Some((8, 2))
    );

    // `--reset` clears the override outright; the config key is the effective value again.
    match rt()
        .block_on(run_with_store(
            &cli(&["radio-set", "--radio", "p2p", "--reset"]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        ))
        .expect("reset")
    {
        Outcome::Json(v) => {
            assert_eq!(v["spreading_factor"], serde_json::Value::Null);
            assert_eq!(v["config_spreading_factor"], 10);
            assert_eq!(v["effective_spreading_factor"], 10);
        }
        other => panic!("expected JSON, got {other:?}"),
    }
    let db = SessionsDb::load(&store).expect("reload");
    assert_eq!(db.radio, None, "--reset clears the override");

    // The Hub config file is never rewritten — the override lives in the session DB alone.
    assert_eq!(
        config
            .p2p_radio
            .as_ref()
            .map(|p| p.spreading_factor)
            .expect("p2p_radio"),
        10
    );
}

/// The modem accepts 7..=12 and nothing else. clap guards the CLI path; `Command::validate`
/// guards the library entry point, which the portal calls without going through clap at all.
#[test]
fn radio_set_refuses_a_spreading_factor_outside_7_to_12() {
    use clap::Parser;
    use control_radio::cli::{
        Command, ControlRadio, ControlRadioCli, RadioArg, RadioSelector, RadioSetArgs,
    };

    for bad in ["6", "13"] {
        let parsed = ControlRadioCli::try_parse_from(vec![
            "control.radio",
            "radio-set",
            "--radio",
            "p2p",
            "--spreading-factor",
            bad,
        ]);
        assert!(
            parsed.is_err(),
            "clap must refuse a spreading factor of {bad}"
        );
    }

    // The same refusal one layer in, where clap's value parser never ran.
    let (store, _dir) = seeded_store("radio-set-range");
    let config = p2p_dual_config();
    let fake = FakeChirpStack::new();
    let hand_built = ControlRadio {
        json: true,
        command: Command::RadioSet(RadioSetArgs {
            radio: RadioSelector {
                radio: Some(RadioArg::P2p),
            },
            spreading_factor: Some(13),
            reset: false,
        }),
    };
    let err = rt()
        .block_on(run_with_store(
            &hand_built,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        ))
        .expect_err("13 is outside the range the modem accepts");
    assert_eq!(err.kind(), control_radio::error::Kind::InvalidArgument);
    assert_eq!(
        control_radio::p2p::db::SessionsDb::load(&store)
            .expect("reload")
            .radio,
        None,
        "refused before anything was written"
    );
}

/// There is no LoRaWAN arm: the data rate is ADR's to choose there, not an operator setting.
#[test]
fn radio_set_over_lorawan_is_invalid_argument() {
    let (store, _dir) = seeded_store("radio-set-lorawan");
    let config = p2p_dual_config();
    let fake = FakeChirpStack::new();
    let err = rt()
        .block_on(run_with_store(
            &cli(&["radio-set", "--radio", "lorawan", "--spreading-factor", "9"]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect_err("radio-set has no LoRaWAN arm");
    assert_eq!(err.kind(), control_radio::error::Kind::InvalidArgument);
}

/// `status` answers what is actually in force, not what the config file says — otherwise an
/// operator who moved the SF has no way to confirm it took.
#[test]
fn status_reports_the_effective_p2p_spreading_factor_override() {
    let (store, _dir) = seeded_store("radio-set-status");
    let config = p2p_dual_config();
    let fake = FakeChirpStack::new();

    let status = || {
        rt().block_on(run_with_store(
            &cli(&["status", "--radio", "p2p"]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        ))
        .expect("status")
    };
    let p2p_block = |o: Outcome| match o {
        Outcome::Status(report) => report.p2p.clone().expect("the p2p block"),
        other => panic!("expected a status report, got {other:?}"),
    };

    // Before any radio-set: the config key is the effective value and nothing is flagged.
    let before = p2p_block(status());
    assert_eq!(
        before.spreading_factor,
        Some(control_radio::status::SpreadingFactorReport {
            config: 10,
            effective: 10,
            overridden: false,
        })
    );

    rt().block_on(run_with_store(
        &cli(&["radio-set", "--radio", "p2p", "--spreading-factor", "9"]).inner,
        &config,
        Some(&fake as &dyn ChirpStack),
        None,
        None,
        store.clone(),
    ))
    .expect("radio-set");

    let after = p2p_block(status());
    assert_eq!(
        after.spreading_factor,
        Some(control_radio::status::SpreadingFactorReport {
            config: 10,
            effective: 9,
            overridden: true,
        })
    );
}

/// node_registry.md §4 (R4): `node-move` changes a Node's carrier and loses nothing. LoRaWAN →
/// P2P deletes the ChirpStack device, keeps the Node (and its key) in the registry, and
/// republishes its `info` under P2P; the way back recreates the device with the same key. A
/// move with no broker is refused before anything changes. Either way the LoRaWAN session the
/// last join left in the cache is cleared (§5, amended 2026-09-28, Q-N2), and nothing the Node
/// reported is.
#[test]
fn node_move_changes_the_carrier_and_keeps_the_node() {
    use control_radio::cache::{Cache, CacheSlot, LorawanSession};
    use control_radio::error::{Kind, Transport};
    use control_radio::lorawan::enroll::NodesFile;
    use control_radio::lorawan::eui::Eui;
    use control_radio::registry::Registry;
    const EUI: &str = "5876070000005722";
    let dual = PLAN_EXAMPLE.replacen("mode: lorawan", "mode: dual", 1);
    let config = RadioConfig::from_device_config_yaml(&dual).expect("dual-mode plan example");
    let (store, _dir) = seeded_store("node-move");
    let fake = FakeChirpStack::new();
    enrol_one(&fake, &config, &store, "2162165722", EUI);

    let move_to = |to: &str, publisher: Option<&FakePublisher>| {
        rt().block_on(run_with_store(
            &cli(&["node-move", "--dev-eui", EUI, "--to", to]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            publisher.map(|p| p as &dyn RetainedPublisher),
            store.clone(),
        ))
    };
    let radio = || {
        Registry::load(&store)
            .expect("load")
            .and_then(|r| r.get(EUI).map(|n| n.radio))
    };
    let transport_published = |p: &FakePublisher| {
        let payload = p.last_payload(&info_topic(EUI)).expect("info republished");
        let v: serde_json::Value = serde_json::from_slice(&payload).expect("json");
        assert!(v.get("dev_addr").is_none(), "{v}");
        assert!(v.get("joined_at").is_none(), "{v}");
        v["transport"].as_str().unwrap_or_default().to_string()
    };
    let cache = Cache::new(&store, Transport::Lorawan);
    let joined = || {
        cache
            .set_lorawan_session(
                EUI,
                &LorawanSession {
                    dev_addr: "01ab23cd".into(),
                    joined_at: "2026-09-27T21:04:11.482161Z".into(),
                },
            )
            .expect("a join");
    };
    let session = || {
        cache
            .read(EUI)
            .expect("reads")
            .and_then(|d| d.lorawan_session())
    };
    cache
        .merge_write(
            EUI,
            CacheSlot::DeviceInfo,
            "2026-09-27T21:05:00Z",
            serde_json::json!({ "fw_version": "2.0.0" }),
        )
        .expect("a report");
    joined();

    let err = move_to("p2p", None).expect_err("no broker");
    assert_eq!(err.kind(), Kind::ServiceDown);
    assert_eq!(radio(), Some(Transport::Lorawan), "nothing changed");
    assert!(session().is_some(), "a refused move keeps the session");
    assert!(rt()
        .block_on(fake.get_device(&Eui::parse(EUI).expect("eui")))
        .expect("get")
        .is_some());

    let pubr = FakePublisher::new();
    move_to("p2p", Some(&pubr)).expect("moves to p2p");
    assert_eq!(radio(), Some(Transport::P2p));
    assert_eq!(session(), None, "the LoRaWAN session stays behind");
    let doc = cache.read(EUI).expect("reads").expect("present");
    assert!(
        doc.slot(CacheSlot::DeviceInfo).is_some(),
        "the reports move"
    );
    assert!(rt()
        .block_on(fake.get_device(&Eui::parse(EUI).expect("eui")))
        .expect("get")
        .is_none());
    let db = control_radio::p2p::db::SessionsDb::load(&store).expect("load");
    assert_eq!(
        db.node(EUI).map(|n| n.app_key.as_str()),
        Some("000102030405060708090a0b0c0d0e0f"),
        "the P2P view has the Node with its one key"
    );
    assert!(NodesFile::load(&store)
        .expect("load")
        .by_dev_eui(&Eui::parse(EUI).expect("eui"))
        .is_none());
    assert_eq!(transport_published(&pubr), "p2p");

    // One left behind by a build before this one is cleared on the way back too.
    joined();
    let pubr = FakePublisher::new();
    move_to("lorawan", Some(&pubr)).expect("moves back");
    assert_eq!(radio(), Some(Transport::Lorawan));
    assert_eq!(session(), None, "the next join sets up a new session");
    let eui = Eui::parse(EUI).expect("eui");
    assert!(rt().block_on(fake.get_device(&eui)).expect("get").is_some());
    assert!(fake.stored_keys(&eui).is_some(), "the same key went back");
    assert_eq!(transport_published(&pubr), "lorawan");

    let again = move_to("lorawan", Some(&FakePublisher::new())).expect("no-op");
    let Outcome::Json(v) = again else {
        panic!("json outcome")
    };
    assert_eq!(v["state"], "unchanged");
}

/// node_registry.md §4 (amended 2026-09-28, Q-N4; bench A4-2): `node-move` off P2P parks the
/// Node's session instead of dropping it. The session is kept with its queue and delivery
/// state emptied, `node-info` on the Node's new carrier says `p2p_session: parked`, and
/// `node-remove` drops the parked session with the Node, both on the carrier's own arm and on
/// a Hub that has that carrier switched off.
#[test]
fn a_move_off_p2p_parks_the_session_and_node_remove_drops_it() {
    use control_radio::p2p::db::SessionsDb;
    use control_radio::p2p::session::{DownlinkPhase, QueuedDownlink, Session};
    const EUIS: [&str; 2] = ["5876070000005801", "5876070000005802"];
    let dual = p2p_dual_config();
    let p2p_only = RadioConfig::from_device_config_yaml(&PLAN_EXAMPLE.replacen(
        "mode: lorawan",
        "mode: p2p",
        1,
    ))
    .expect("p2p plan example");
    let (store, _dir) = seeded_store("move-parks");
    let fake = FakeChirpStack::new();
    let run = |args: &[&str], config: &RadioConfig, client: bool| {
        rt().block_on(run_with_store(
            &cli(args).inner,
            config,
            client.then_some(&fake as &dyn ChirpStack),
            None,
            Some(&FakePublisher::new() as &dyn RetainedPublisher),
            store.clone(),
        ))
        .unwrap_or_else(|e| panic!("{args:?}: {e}"))
    };
    let json = |o: Outcome| match o {
        Outcome::Json(v) => v,
        other => panic!("expected json, got {other:?}"),
    };

    for (i, eui) in EUIS.iter().enumerate() {
        enrol_one(&fake, &dual, &store, &format!("216216580{i}"), eui);
        let info = json(run(
            &["node-info", "--radio", "lorawan", "--dev-eui", eui],
            &dual,
            true,
        ));
        assert!(info.get("p2p_session").is_none(), "no P2P session yet");
        run(&["node-move", "--dev-eui", eui, "--to", "p2p"], &dual, true);

        // The Node joined over P2P and has a command in flight.
        let _lock = store
            .lock(control_radio::store::Slot::P2pSessions)
            .expect("lock");
        let mut db = SessionsDb::load(&store).expect("load");
        let queue: std::collections::VecDeque<_> = [QueuedDownlink::with_seq(3, vec![0x01])].into();
        db.sessions.insert(
            eui.to_string(),
            Session {
                dev_eui: eui.to_string(),
                addr: 0x5801 + i as u32,
                session_key: "00".repeat(16),
                dev_nonce: 4,
                last_seen: 41,
                joined_at: None,
                proto_version: 1,
                capabilities: None,
                hello: None,
                downlink_queue: queue,
                downlink_phase: DownlinkPhase::Sent,
                history: None,
                detach: None,
                home: None,
            },
        );
        db.save(&store).expect("save");
    }

    for (i, eui) in EUIS.iter().enumerate() {
        let moved = json(run(
            &["node-move", "--dev-eui", eui, "--to", "lorawan"],
            &dual,
            true,
        ));
        assert!(
            moved["notes"].to_string().contains("parked"),
            "the move says so: {moved}"
        );
        let db = SessionsDb::load(&store).expect("load");
        let s = db.sessions.get(*eui).expect("the session is kept");
        assert!(db.is_parked(eui));
        assert!(s.downlink_queue.is_empty(), "its P2P queue is dropped");
        assert_eq!(s.downlink_phase, DownlinkPhase::Idle);
        assert_eq!(
            (s.addr, s.last_seen, s.dev_nonce),
            (0x5801 + i as u32, 41, 4),
            "the counters and the address are the session's"
        );
        let info = json(run(
            &["node-info", "--radio", "lorawan", "--dev-eui", eui],
            &dual,
            true,
        ));
        assert_eq!(info["p2p_session"], "parked", "{info}");
    }

    // On the Node's own carrier.
    run(
        &["node-remove", "--radio", "lorawan", "--dev-eui", EUIS[0]],
        &dual,
        true,
    );
    // And with LoRaWAN switched off on the Hub.
    run(
        &["node-remove", "--radio", "lorawan", "--dev-eui", EUIS[1]],
        &p2p_only,
        false,
    );
    let db = SessionsDb::load(&store).expect("load");
    assert!(
        db.sessions.is_empty(),
        "node-remove drops a parked session: {:?}",
        db.sessions.keys()
    );
}

/// `node-move` reaches ChirpStack at either end of a move, so the CLI connects to it wherever
/// LoRaWAN is on. On `dual` it used to connect to nothing (no `--radio` to resolve), and a move
/// off LoRaWAN was refused as "ChirpStack is not reachable".
#[test]
fn node_move_connects_to_chirpstack_wherever_lorawan_is_on() {
    let parsed = cli(&["node-move", "--dev-eui", "5876070000005722", "--to", "p2p"]);
    for (mode, needs) in [("lorawan", true), ("dual", true), ("p2p", false)] {
        let yaml = PLAN_EXAMPLE.replacen("mode: lorawan", &format!("mode: {mode}"), 1);
        let config = RadioConfig::from_device_config_yaml(&yaml).expect(mode);
        assert_eq!(
            control_radio::dispatch::needs_chirpstack(&parsed.inner, &config),
            needs,
            "{mode}"
        );
    }
}

/// node_registry.md §4 (amended 2026-09-27, Hynek: „ano, povolit jeho smazání“): a LoRaWAN
/// Node on a Hub that has LoRaWAN switched off can be removed. The tombstones go out, the
/// record goes, and the DevEUI waits on `removed` until LoRaWAN is on again. The mirror then
/// deletes its ChirpStack device instead of adopting it back, and clears the entry.
#[test]
fn a_node_on_a_switched_off_carrier_can_be_removed() {
    use control_radio::lorawan::eui::Eui;
    use control_radio::registry::Registry;
    const EUI: &str = "5876070000009001";
    let lorawan = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let p2p = RadioConfig::from_device_config_yaml(&PLAN_EXAMPLE.replacen(
        "mode: lorawan",
        "mode: p2p",
        1,
    ))
    .expect("p2p plan example");
    let (store, _dir) = seeded_store("remove-off-carrier");
    let fake = FakeChirpStack::new();
    enrol_one(&fake, &lorawan, &store, "2162169001", EUI);

    // The Hub is switched to p2p: no ChirpStack client, and --radio lorawan would be gated.
    let pubr = FakePublisher::new();
    let out = rt()
        .block_on(run_with_store(
            &cli(&["node-remove", "--radio", "lorawan", "--dev-eui", EUI]).inner,
            &p2p,
            None,
            None,
            Some(&pubr as &dyn RetainedPublisher),
            store.clone(),
        ))
        .expect("removed on a switched-off carrier");
    let Outcome::Json(v) = out else {
        panic!("json outcome")
    };
    assert_eq!(v["state"], "deleted");
    assert_eq!(pubr.published(), tombstones(EUI));
    let reg = Registry::load(&store).expect("load").expect("exists");
    assert!(reg.get(EUI).is_none(), "the record is gone");
    assert!(
        reg.removed.contains_key(EUI),
        "waiting for the ChirpStack delete"
    );
    let eui = Eui::parse(EUI).expect("eui");
    assert!(
        rt().block_on(fake.get_device(&eui)).expect("get").is_some(),
        "ChirpStack was off"
    );

    // LoRaWAN back on: the next create's mirror deletes the device, not adopts it.
    let json = rt()
        .block_on(run_with_store(
            &cli(&["create", "--radio", "lorawan"]).inner,
            &lorawan,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        ))
        .expect("create");
    let Outcome::Json(v) = json else {
        panic!("json outcome")
    };
    assert_eq!(v["mirror"]["deleted"][0], EUI, "{v}");
    assert!(rt().block_on(fake.get_device(&eui)).expect("get").is_none());
    let reg = Registry::load(&store).expect("load").expect("exists");
    assert!(reg.get(EUI).is_none(), "not adopted back");
    assert!(reg.removed.is_empty(), "the entry is cleared");
}

/// The boot unit's `create` carries no `--radio` (plan.md §5, "Boot invocation"). Under `dual`
/// it runs both arms: the LoRaWAN converge with its ChirpStack mirror, then the P2P init. It
/// used to stop at the dual-ambiguity rule with `RadioParameterRequired`, so `radio-setup`
/// failed on every dual Hub and ChirpStack never followed the Node registry.
#[test]
fn a_dual_boot_create_runs_both_arms() {
    let (store, _dir) = seeded_store("dual-create");
    let doc = PLAN_EXAMPLE.replace("  mode: lorawan", "  mode: dual");
    let config = RadioConfig::from_device_config_yaml(&doc).expect("dual example");
    let fake = FakeChirpStack::new();
    let args = cli(&["create", "--gateway-eui", "24e124fffefd3bda"]);
    assert!(
        control_radio::dispatch::needs_chirpstack(&args.inner, &config),
        "the dual boot create reaches ChirpStack"
    );

    let outcome = rt()
        .block_on(run_with_store(
            &args.inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        ))
        .expect("a dual boot create succeeds");

    let Outcome::Json(v) = outcome else {
        panic!("create returns Outcome::Json");
    };
    assert!(v["lorawan"].is_object(), "the LoRaWAN arm reports: {v}");
    assert!(
        v["lorawan"]["mirror"].is_object(),
        "the LoRaWAN arm runs the registry mirror: {v}"
    );
    assert_eq!(v["p2p"]["initialized"], true, "the P2P arm runs: {v}");
}

// ---------------------------------------------------------------------------------------------
// Q-N1 (plan.md §5 `node-send`, amended 2026-09-28): identical idempotent asks waiting in a
// Node's queue coalesce into the newest.
// ---------------------------------------------------------------------------------------------

/// One `node-send` with a broker at hand: its JSON result.
fn send_with_broker(
    fake: &FakeChirpStack,
    pubr: &FakePublisher,
    config: &RadioConfig,
    store: &Store,
    args: &[&str],
) -> serde_json::Value {
    let mut full = vec!["node-send"];
    full.extend_from_slice(args);
    match rt()
        .block_on(run_with_store(
            &cli(&full).inner,
            config,
            Some(fake as &dyn ChirpStack),
            None,
            Some(pubr as &dyn RetainedPublisher),
            store.clone(),
        ))
        .unwrap_or_else(|e| panic!("{full:?}: {e}"))
    {
        Outcome::Json(v) => v,
        other => panic!("{full:?}: {other:?}"),
    }
}

/// The seqs of the device's ChirpStack queue, in order, each with whether it is on the air.
fn lorawan_queue(fake: &FakeChirpStack, dev_eui: &str) -> Vec<(u32, bool)> {
    let eui = control_radio::lorawan::eui::Eui::parse(dev_eui).expect("eui");
    rt().block_on(fake.get_queue(&eui))
        .expect("queue")
        .iter()
        .map(|q| {
            (
                control_radio::lorawan::queue::downlink_seq(&q.data).expect("a command frame"),
                q.is_pending,
            )
        })
        .collect()
}

/// The P2P session queue's seqs, in order.
fn p2p_queue(store: &Store) -> Vec<Option<u32>> {
    control_radio::p2p::db::SessionsDb::load(store)
        .expect("reload")
        .sessions[P2P_DEV_EUI]
        .downlink_queue
        .iter()
        .map(|q| q.seq)
        .collect()
}

/// The `superseded` `command` documents a broker got, as `(seq, superseded_by)`, after checking
/// each is on the node's `command` topic and has the shape the plan names.
fn superseded(pubr: &FakePublisher, dev_eui: &str) -> Vec<(u64, u64)> {
    let topic = Topics::new(control_radio::northbound::hostname()).node(dev_eui, Area::Command);
    assert!(pubr.published().is_empty(), "nothing retained");
    pubr.events()
        .iter()
        .map(|(t, payload)| {
            assert_eq!(*t, topic);
            let doc: serde_json::Value = serde_json::from_slice(payload).expect("json");
            assert_eq!(doc["status"], "superseded");
            assert_eq!(doc["kind"], "superseded");
            assert_eq!(doc["dev_eui"], dev_eui);
            assert!(doc["at"].is_string(), "{doc}");
            (
                doc["seq"].as_u64().expect("seq"),
                doc["superseded_by"].as_u64().expect("superseded_by"),
            )
        })
        .collect()
}

/// The `superseded_by` a node's `sent_commands` entry for `seq` carries.
fn superseded_by(store: &Store, dev_eui: &str, seq: u64) -> Option<u64> {
    let doc = Cache::new(store, control_radio::error::Transport::Lorawan)
        .read(dev_eui)
        .expect("reads")
        .expect("present");
    doc.slots[control_radio::cache::SENT_COMMANDS_KEY]
        .as_array()
        .expect("array")
        .iter()
        .rfind(|e| e["seq"].as_u64() == Some(seq))
        .and_then(|e| e["superseded_by"].as_u64())
}

/// Bench F5: two `cmd clock-sync` went out as two downlinks and brought two Infos. On LoRaWAN
/// the second withdraws the first from ChirpStack's queue and goes out under its own, newer
/// seq; the result names the one withdrawn, its `sent_commands` entry names the successor, and
/// a `superseded` document goes out for it.
#[test]
fn two_clock_syncs_on_lorawan_coalesce_into_the_newest() {
    let (store, _dir) = seeded_store("coalesce-lrw");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    let dev = "58760700c0668fc7";
    enrol_one(&fake, &config, &store, "SN-CO", dev);
    let pubr = FakePublisher::new();
    let clock = [
        "--radio",
        "lorawan",
        "--serial",
        "SN-CO",
        "cmd",
        "clock-sync",
    ];

    let first = send_with_broker(&fake, &pubr, &config, &store, &clock);
    assert_eq!(first["seq"], 1);
    assert!(first.get("coalesced").is_none(), "nothing waited: {first}");
    let second = send_with_broker(&fake, &pubr, &config, &store, &clock);
    assert_eq!(second["seq"], 2);
    assert_eq!(second["coalesced"], serde_json::json!([1]));

    assert_eq!(
        lorawan_queue(&fake, dev),
        [(2, false)],
        "one frame, the newer seq"
    );
    assert_eq!(superseded_by(&store, dev, 1), Some(2));
    assert_eq!(superseded_by(&store, dev, 2), None);
    assert_eq!(superseded(&pubr, dev), [(1, 2)]);

    // A different ask waiting is not the same one: it stays, and the new one queues behind.
    let info = send_with_broker(
        &fake,
        &pubr,
        &config,
        &store,
        &["--radio", "lorawan", "--serial", "SN-CO", "get-info"],
    );
    assert!(info.get("coalesced").is_none(), "{info}");
    assert_eq!(lorawan_queue(&fake, dev), [(2, false), (3, false)]);
}

/// A frame ChirpStack already has on the air (`is_pending`) is left alone, and the new one
/// queues behind it.
#[test]
fn an_on_air_lorawan_frame_is_not_withdrawn() {
    let (store, _dir) = seeded_store("coalesce-lrw-pending");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    let dev = "58760700c0668fc7";
    enrol_one(&fake, &config, &store, "SN-CP", dev);
    let pubr = FakePublisher::new();
    let clock = [
        "--radio",
        "lorawan",
        "--serial",
        "SN-CP",
        "cmd",
        "clock-sync",
    ];

    send_with_broker(&fake, &pubr, &config, &store, &clock);
    fake.mark_head_pending(&control_radio::lorawan::eui::Eui::parse(dev).expect("eui"));
    let second = send_with_broker(&fake, &pubr, &config, &store, &clock);
    assert!(second.get("coalesced").is_none(), "{second}");
    assert_eq!(lorawan_queue(&fake, dev), [(1, true), (2, false)]);
    assert!(pubr.events().is_empty());
    assert_eq!(superseded_by(&store, dev, 1), None);
}

/// Nothing that writes or acts coalesces, and neither does a `get-config` continuation: a
/// `--seq` re-request waiting is not withdrawn by a fresh `get-config`, nor does a new one
/// withdraw a waiting `get-config`.
#[test]
fn writes_actions_and_continuations_never_coalesce() {
    let (store, _dir) = seeded_store("coalesce-lrw-never");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    let dev = "58760700c0668fc7";
    enrol_one(&fake, &config, &store, "SN-CN", dev);
    let pubr = FakePublisher::new();
    let send = |tail: &[&str]| {
        let mut args = vec!["--radio", "lorawan", "--serial", "SN-CN"];
        args.extend_from_slice(tail);
        send_with_broker(&fake, &pubr, &config, &store, &args)
    };

    for tail in [
        &["set-config", "application.interval_report=60"][..],
        &["set-config", "application.interval_report=60"],
        &["cmd", "reboot"],
        &["cmd", "reboot"],
        // A continuation of exchange 50, then a fresh full read: both stay.
        &["get-config", "--seq", "50"],
        &["get-config"],
        // A fresh read waiting, then a continuation page: both stay.
        &["get-config", "--page", "2", "--seq", "50"],
    ] {
        let out = send(tail);
        assert!(out.get("coalesced").is_none(), "{tail:?}: {out}");
    }
    let seqs: Vec<u32> = lorawan_queue(&fake, dev).iter().map(|(s, _)| *s).collect();
    assert_eq!(seqs, [1, 2, 3, 4, 50, 5, 50]);
    assert!(pubr.events().is_empty());

    // Two fresh `get-config`s do coalesce: the waiting one (seq 5) goes.
    let out = send(&["get-config"]);
    assert_eq!(out["coalesced"], serde_json::json!([5]));
    let seqs: Vec<u32> = lorawan_queue(&fake, dev).iter().map(|(s, _)| *s).collect();
    assert_eq!(seqs, [1, 2, 3, 4, 50, 50, 6]);
}

/// config_sync.md §13 Q20b: a `set-config --supersede` (the Portal's whole pending message)
/// withdraws an earlier `--supersede` write still waiting; a plain `set-config` is never
/// withdrawn and never withdraws; a frame on the air stays and the new one queues behind it.
#[test]
fn a_superseding_set_config_replaces_the_waiting_one_on_lorawan() {
    let (store, _dir) = seeded_store("supersede-lrw");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    let dev = "58760700c0668fc7";
    enrol_one(&fake, &config, &store, "SN-CN", dev);
    let pubr = FakePublisher::new();
    let send = |tail: &[&str]| {
        let mut args = vec!["--radio", "lorawan", "--serial", "SN-CN"];
        args.extend_from_slice(tail);
        send_with_broker(&fake, &pubr, &config, &store, &args)
    };

    // A plain write waits (seq 1); the first whole message does not take it (seq 2).
    send(&["set-config", "application.interval_report=60"]);
    let first = send(&[
        "set-config",
        "--supersede",
        "application.interval_report=120",
    ]);
    assert!(first.get("coalesced").is_none(), "{first}");
    // The second whole message replaces the first one, not the plain write.
    let second = send(&[
        "set-config",
        "--supersede",
        "application.interval_report=300",
    ]);
    assert_eq!(second["coalesced"], serde_json::json!([2]), "{second}");
    let seqs: Vec<u32> = lorawan_queue(&fake, dev).iter().map(|(s, _)| *s).collect();
    assert_eq!(seqs, [1, 3]);
    assert_eq!(superseded_by(&store, dev, 2), Some(3));

    // On the air: the head stays, and the next version queues behind it.
    let fake2 = FakeChirpStack::new();
    let (store2, _dir2) = seeded_store("supersede-lrw-air");
    enrol_one(&fake2, &config, &store2, "SN-CN", dev);
    let send2 = |tail: &[&str]| {
        let mut args = vec!["--radio", "lorawan", "--serial", "SN-CN"];
        args.extend_from_slice(tail);
        send_with_broker(&fake2, &pubr, &config, &store2, &args)
    };
    send2(&[
        "set-config",
        "--supersede",
        "application.interval_report=120",
    ]);
    fake2.mark_head_pending(&control_radio::lorawan::eui::Eui::parse(dev).expect("eui"));
    let behind = send2(&[
        "set-config",
        "--supersede",
        "application.interval_report=300",
    ]);
    assert!(behind.get("coalesced").is_none(), "{behind}");
    assert_eq!(lorawan_queue(&fake2, dev), [(1, true), (2, false)]);
}

/// The same on P2P.
#[test]
fn a_superseding_set_config_replaces_the_waiting_one_on_p2p() {
    let (store, _dir) = seeded_store("supersede-p2p");
    let config = p2p_dual_config();
    seed_p2p_node(&store, "80e007b2", true);
    let fake = FakeChirpStack::new();
    let pubr = FakePublisher::new();
    let send = |tail: &[&str]| {
        let mut args = vec!["--radio", "p2p", "--serial", "0x80e007b2"];
        args.extend_from_slice(tail);
        send_with_broker(&fake, &pubr, &config, &store, &args)
    };
    send(&["set-config", "application.interval_report=60"]);
    send(&[
        "set-config",
        "--supersede",
        "application.interval_report=120",
    ]);
    let third = send(&[
        "set-config",
        "--supersede",
        "application.interval_report=300",
    ]);
    assert_eq!(third["coalesced"], serde_json::json!([2]), "{third}");
    assert_eq!(p2p_queue(&store), [Some(1), Some(3)]);
    assert_eq!(superseded_by(&store, P2P_DEV_EUI, 2), Some(3));
}

/// The same on P2P: the waiting entry leaves the session queue, the new one goes in under its
/// newer seq, and the result, the cache and the broker say the same as on LoRaWAN.
#[test]
fn two_clock_syncs_on_p2p_coalesce_into_the_newest() {
    let (store, _dir) = seeded_store("coalesce-p2p");
    let config = p2p_dual_config();
    seed_p2p_node(&store, "80e007b2", true);
    let fake = FakeChirpStack::new();
    let pubr = FakePublisher::new();
    let clock = [
        "--radio",
        "p2p",
        "--serial",
        "0x80e007b2",
        "cmd",
        "clock-sync",
    ];

    let first = send_with_broker(&fake, &pubr, &config, &store, &clock);
    assert_eq!(first["seq"], 1);
    let second = send_with_broker(&fake, &pubr, &config, &store, &clock);
    assert_eq!(second["seq"], 2);
    assert_eq!(second["coalesced"], serde_json::json!([1]));
    assert_eq!(second["queue_depth"], 1);

    assert_eq!(p2p_queue(&store), [Some(2)]);
    assert_eq!(superseded_by(&store, P2P_DEV_EUI, 1), Some(2));
    assert_eq!(superseded(&pubr, P2P_DEV_EUI), [(1, 2)]);
    assert!(fake.downlinks().is_empty(), "nothing reached ChirpStack");
}

/// A P2P head already delivered, waiting for its `0x55`, is on the air: it stays, and the new
/// frame queues behind it. An announced head has not gone out yet and is withdrawn.
#[test]
fn a_delivered_p2p_head_is_not_withdrawn() {
    use control_radio::p2p::session::DownlinkPhase;
    let (store, _dir) = seeded_store("coalesce-p2p-sent");
    let config = p2p_dual_config();
    seed_p2p_node(&store, "80e007b2", true);
    let fake = FakeChirpStack::new();
    let pubr = FakePublisher::new();
    let info = ["--radio", "p2p", "--serial", "0x80e007b2", "get-info"];
    let phase = |p: DownlinkPhase| {
        let mut db = control_radio::p2p::db::SessionsDb::load(&store).expect("load");
        db.sessions
            .get_mut(P2P_DEV_EUI)
            .expect("session")
            .downlink_phase = p;
        db.save(&store).expect("save");
    };

    send_with_broker(&fake, &pubr, &config, &store, &info);
    phase(DownlinkPhase::Sent);
    let second = send_with_broker(&fake, &pubr, &config, &store, &info);
    assert!(second.get("coalesced").is_none(), "{second}");
    assert_eq!(p2p_queue(&store), [Some(1), Some(2)]);
    assert!(pubr.events().is_empty());

    // Seq 2 waits behind the delivered head, and a third ask replaces it, not the head.
    let third = send_with_broker(&fake, &pubr, &config, &store, &info);
    assert_eq!(third["coalesced"], serde_json::json!([2]));
    assert_eq!(p2p_queue(&store), [Some(1), Some(3)]);

    // An announced head is not on the air yet.
    let mut db = control_radio::p2p::db::SessionsDb::load(&store).expect("load");
    db.sessions
        .get_mut(P2P_DEV_EUI)
        .expect("session")
        .downlink_queue
        .pop_front();
    db.save(&store).expect("save");
    phase(DownlinkPhase::Announced);
    let fourth = send_with_broker(&fake, &pubr, &config, &store, &info);
    assert_eq!(fourth["coalesced"], serde_json::json!([3]));
    assert_eq!(p2p_queue(&store), [Some(4)]);
}

/// What coalescing withdraws makes room: a full queue whose last entry is a waiting
/// `force-send` still takes a new one.
#[test]
fn a_coalescing_send_fits_a_full_p2p_queue() {
    let (store, _dir) = seeded_store("coalesce-p2p-full");
    let config = p2p_dual_config();
    seed_p2p_node(&store, "80e007b2", true);
    let fake = FakeChirpStack::new();
    let pubr = FakePublisher::new();
    let force = [
        "--radio",
        "p2p",
        "--serial",
        "0x80e007b2",
        "cmd",
        "force-send",
    ];
    let raw = [
        "--radio",
        "p2p",
        "--serial",
        "0x80e007b2",
        "--fport",
        "85",
        "--hex",
        "aa",
    ];
    for _ in 1..control_radio::p2p::db::MAX_QUEUED_DOWNLINKS {
        send_with_broker(&fake, &pubr, &config, &store, &raw);
    }
    send_with_broker(&fake, &pubr, &config, &store, &force);
    let full = control_radio::p2p::db::MAX_QUEUED_DOWNLINKS;
    assert_eq!(p2p_queue(&store).len(), full);

    let out = send_with_broker(&fake, &pubr, &config, &store, &force);
    assert_eq!(out["coalesced"], serde_json::json!([1]));
    assert_eq!(out["queue_depth"], full);
    assert_eq!(p2p_queue(&store).last(), Some(&Some(2)));
}

// ---------------------------------------------------------------------------------------------
// `create --role client` — the cluster join's CLI twin (plan/control/cluster.md §8). The join
// itself runs in the agent; these are the refusals this crate makes before it reads stdin or
// dials the control socket. The socket half is `cluster_join`'s own tests, and the whole binary
// is proximosctl's `cluster_join_cli`.
// ---------------------------------------------------------------------------------------------

const JOIN: [&str; 6] = [
    "--role",
    "client",
    "--server-host",
    "proximos-1a2b3c4d.local",
    "--ca-sha256",
    "4Fg1yj5x8x2R0T3y4bWcL9e6t5Qa8n2rJ7kPq0sVwXz",
];

fn create_client(radio: Option<&str>) -> ControlRadioCli {
    let mut args = vec!["create"];
    if let Some(r) = radio {
        args.extend(["--radio", r]);
    }
    args.extend(JOIN);
    cli(&args)
}

/// The flags travel together, and no password flag exists to put it on argv.
#[test]
fn create_role_client_needs_the_server_and_the_fingerprint_and_takes_no_password_flag() {
    let parse = |args: &[&str]| {
        let mut full = vec!["control.radio", "create", "--radio", "lorawan"];
        full.extend_from_slice(args);
        ControlRadioCli::try_parse_from(full)
    };
    assert!(parse(&JOIN).is_ok());
    assert!(parse(&["--role", "client", "--server-host", "h.local"]).is_err());
    assert!(parse(&["--role", "client", "--ca-sha256", "x"]).is_err());
    assert!(parse(&["--server-host", "h.local", "--ca-sha256", "x"]).is_err());
    assert!(parse(&["--role", "server", "--server-host", "h", "--ca-sha256", "x"]).is_err());
    let mut with_password = JOIN.to_vec();
    with_password.extend(["--password", "Zm9vYmFyYmF6cXV4cXV1eA"]);
    assert!(parse(&with_password).is_err(), "the password is stdin's");

    let Command::Create(args) = create_client(Some("lorawan")).inner.command else {
        panic!("create");
    };
    assert!(args.role_client());
    assert_eq!(args.server_host.as_deref(), Some("proximos-1a2b3c4d.local"));
}

/// The join makes no gRPC call: the CLI must not wait on (or fail for) a ChirpStack.
#[test]
fn create_role_client_never_needs_chirpstack() {
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("example");
    assert!(!control_radio::dispatch::needs_chirpstack(
        &create_client(Some("lorawan")).inner,
        &config
    ));
    assert!(control_radio::dispatch::needs_chirpstack(
        &cli(&["create", "--radio", "lorawan"]).inner,
        &config
    ));
}

#[test]
fn create_role_client_refusals_before_the_join() {
    let (store, _dir) = seeded_store("role-client");
    let lorawan = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("lorawan");
    let dual = RadioConfig::from_device_config_yaml(&PLAN_EXAMPLE.replacen(
        "mode: lorawan",
        "mode: dual",
        1,
    ))
    .expect("dual");
    let p2p = RadioConfig::from_device_config_yaml(&PLAN_EXAMPLE.replacen(
        "mode: lorawan",
        "mode: p2p",
        1,
    ))
    .expect("p2p");
    let refuse = |cli: &ControlRadioCli, config: &RadioConfig| {
        rt().block_on(run_with_store(
            &cli.inner,
            config,
            None,
            None,
            None,
            store.clone(),
        ))
        .expect_err("refused")
    };

    let off = RadioConfig::from_device_config_yaml(&PLAN_EXAMPLE.replacen(
        "mode: lorawan",
        "mode: off",
        1,
    ))
    .expect("off");
    // No mode gate (decision #47): a Client runs the Server's mode, so every radio mode and
    // every `--radio`, or none, reaches the join's own refusals — here the dry run.
    for config in [&off, &lorawan, &p2p, &dual] {
        for radio in [None, Some("lorawan"), Some("p2p")] {
            let mut dry = vec!["create"];
            if let Some(r) = radio {
                dry.extend(["--radio", r]);
            }
            dry.push("--dry-run");
            dry.extend(JOIN);
            let e = refuse(&cli(&dry), config);
            assert_eq!(
                e.kind(),
                Kind::InvalidArgument,
                "{radio:?} on {:?}: {e}",
                config.mode
            );
            assert!(e.message().contains("dry run"), "{e}");
        }
    }
    // No dry run: the checks talk to the Server and a success reboots the Hub.
    let mut dry = vec!["create", "--radio", "lorawan", "--dry-run"];
    dry.extend(JOIN);
    let e = refuse(&cli(&dry), &lorawan);
    assert_eq!(e.kind(), Kind::InvalidArgument, "{e}");
    assert!(e.message().contains("dry run"), "{e}");
}

/// U4: `northbridge-update` takes `--file` and `--manifest` together — clap refuses one alone,
/// and so does `validate` for a hand-built command that never went through clap.
#[test]
fn northbridge_update_needs_file_and_manifest_together() {
    use clap::Parser;
    use control_radio::cli::{
        Command, ControlRadio, ControlRadioCli, NorthbridgeUpdateArgs, RadioArg, RadioSelector,
    };

    for args in [vec!["--file", "nb.bin"], vec!["--manifest", "nb.json"]] {
        let mut full = vec!["control.radio", "northbridge-update", "--radio", "p2p"];
        full.extend(args.iter().copied());
        assert!(ControlRadioCli::try_parse_from(&full).is_err(), "{full:?}");
    }
    let parsed = cli(&[
        "northbridge-update",
        "--radio",
        "p2p",
        "--file",
        "nb.bin",
        "--manifest",
        "nb.json",
        "--force",
        "--no-wait",
    ]);
    match &parsed.inner.command {
        Command::NorthbridgeUpdate(a) => {
            assert!(a.force && a.no_wait);
            assert_eq!(a.timeout, 180, "the default wait");
        }
        other => panic!("parsed as {other:?}"),
    }

    let (store, _dir) = seeded_store("nb-update-validate");
    let hand_built = ControlRadio {
        json: true,
        command: Command::NorthbridgeUpdate(NorthbridgeUpdateArgs {
            radio: RadioSelector {
                radio: Some(RadioArg::P2p),
            },
            file: Some("nb.bin".into()),
            manifest: None,
            force: false,
            no_wait: false,
            timeout: 180,
        }),
    };
    let err = rt()
        .block_on(run_with_store(
            &hand_built,
            &p2p_dual_config(),
            None,
            None,
            None,
            store,
        ))
        .expect_err("a file without its manifest");
    assert_eq!(err.kind(), control_radio::error::Kind::InvalidArgument);
}

/// U4: one-armed on P2P — `--radio lorawan` is a usage error. Since #158 the command does not
/// depend on `radio.mode`: a LoRaWAN-only Hub is not refused as `P2pUnavailable` but goes on
/// to the update itself (here refused for the empty test image).
#[test]
fn northbridge_update_is_p2p_only_but_mode_independent() {
    let (store, _dir) = seeded_store("nb-update-gate");
    let run = |args: &[&str], config: &RadioConfig| {
        rt().block_on(run_with_store(
            &cli(args).inner,
            config,
            None,
            None,
            None,
            store.clone(),
        ))
    };
    let err = run(
        &["northbridge-update", "--radio", "lorawan"],
        &p2p_dual_config(),
    )
    .expect_err("no LoRaWAN arm");
    assert_eq!(err.kind(), control_radio::error::Kind::InvalidArgument);

    let mut lorawan_only = p2p_dual_config();
    lorawan_only.mode = control_radio::Mode::Lorawan;
    let err = run(&["northbridge-update", "--radio", "p2p"], &lorawan_only)
        .expect_err("the test image carries no Northbridge firmware");
    assert_ne!(err.kind(), control_radio::error::Kind::P2pUnavailable);
    assert!(
        err.to_string().contains("carries no Northbridge firmware"),
        "{err}"
    );
}

/// U4: a hotfix that does not match its manifest is refused before any request is written.
#[test]
fn northbridge_update_refuses_a_hotfix_its_manifest_does_not_describe() {
    let (store, dir) = seeded_store("nb-update-hotfix");
    let bin = dir.0.join("nb.bin");
    let json = dir.0.join("nb.json");
    std::fs::write(&bin, [0u8; 64]).unwrap();
    std::fs::write(&json, "{\"format\": 1}").unwrap();
    let err = rt()
        .block_on(run_with_store(
            &cli(&[
                "northbridge-update",
                "--radio",
                "p2p",
                "--file",
                bin.to_str().unwrap(),
                "--manifest",
                json.to_str().unwrap(),
            ])
            .inner,
            &p2p_dual_config(),
            None,
            None,
            None,
            store.clone(),
        ))
        .expect_err("not a manifest of this image");
    assert_eq!(err.kind(), control_radio::error::Kind::InvalidArgument);
    assert!(!store.exists(control_radio::store::Slot::NbUpdateRequest));
}

// ---- P2P control commands (p2p_control_commands.md, sticker-firmware #487) ----

/// Give the seeded session the Capabilities a #487 Node reports (`bits`), or none.
fn set_p2p_caps(store: &Store, bits: &[u8]) {
    use control_radio::p2p::tower::control::{Capabilities, PROFILE_LORA};
    let mut db = control_radio::p2p::db::SessionsDb::load(store).expect("load");
    db.sessions
        .get_mut(P2P_DEV_EUI)
        .expect("session")
        .capabilities = Some(Capabilities {
        proto_version: 1,
        mtu: 96,
        profiles: PROFILE_LORA,
        cmd_bitmap: bits.iter().fold(0u64, |m, b| m | (1u64 << b)),
        power_class: 1,
    });
    db.save(store).expect("save");
}

fn p2p_run(store: &Store, args: &[&str]) -> control_radio::Result<Outcome> {
    let config = p2p_dual_config();
    let fake = FakeChirpStack::new();
    let pubr = FakePublisher::new();
    rt().block_on(run_with_store(
        &cli(args).inner,
        &config,
        Some(&fake as &dyn ChirpStack),
        None,
        Some(&pubr as &dyn RetainedPublisher),
        store.clone(),
    ))
}

fn p2p_control_queue(store: &Store) -> Vec<(Vec<u8>, bool)> {
    let db = control_radio::p2p::db::SessionsDb::load(store).expect("reload");
    db.sessions
        .get(P2P_DEV_EUI)
        .expect("session")
        .downlink_queue
        .iter()
        .map(|d| (d.body.clone(), d.control))
        .collect()
}

/// H-2 / H-5 / H-1: the three control commands queue their `0x91` TLV, once the Node has
/// named them in its Capabilities.
#[test]
fn node_send_control_commands_queue_their_0x91_tlv() {
    use control_radio::p2p::tower::control::cmd;
    let (store, _dir) = seeded_store("p2p-send-control");
    seed_p2p_node(&store, "80e007b2", true);
    set_p2p_caps(&store, &[cmd::DEV_STATUS, cmd::BACKOFF, cmd::REJOIN_REQ]);
    let base = ["node-send", "--radio", "p2p", "--dev-eui", P2P_DEV_EUI];
    let cases: [(&[&str], &str, Vec<u8>); 4] = [
        (&["dev-status"], "dev-status", vec![0x30, 0x00]),
        (
            &["backoff", "--seconds", "120"],
            "backoff",
            vec![0x15, 0x02, 120, 0],
        ),
        (&["rejoin"], "rejoin", vec![0x04, 0x01, 0x01]),
        (
            &["rejoin", "--kind", "rediscover"],
            "rejoin",
            vec![0x04, 0x01, 0x00],
        ),
    ];
    for (i, (extra, name, _)) in cases.iter().enumerate() {
        let mut args = base.to_vec();
        args.extend_from_slice(extra);
        match p2p_run(&store, &args).expect("queued") {
            Outcome::Json(v) => {
                assert_eq!(v["type"], *name);
                assert_eq!(v["envelope"], "0x91");
                assert_eq!(v["queue_depth"], i + 1);
            }
            other => panic!("expected JSON, got {other:?}"),
        }
    }
    let queued = p2p_control_queue(&store);
    let expected: Vec<(Vec<u8>, bool)> = cases
        .iter()
        .map(|(_, _, tlv)| (tlv.clone(), true))
        .collect();
    assert_eq!(queued, expected);
}

/// H-7: without the bit, or with no Capabilities at all, the command is refused and
/// nothing is queued.
#[test]
fn node_send_control_commands_follow_the_capabilities_gate() {
    use control_radio::p2p::tower::control::cmd;
    let (store, _dir) = seeded_store("p2p-send-control-gate");
    seed_p2p_node(&store, "80e007b2", true);
    let args = [
        "node-send",
        "--radio",
        "p2p",
        "--dev-eui",
        P2P_DEV_EUI,
        "dev-status",
    ];
    let err = p2p_run(&store, &args).expect_err("no Capabilities yet");
    assert_eq!(err.kind(), Kind::InvalidArgument);
    set_p2p_caps(&store, &[cmd::BACKOFF]);
    let err = p2p_run(&store, &args).expect_err("no DevStatus bit");
    assert_eq!(err.kind(), Kind::InvalidArgument);
    assert!(err.to_string().contains("dev-status"), "{err}");
    assert!(
        p2p_control_queue(&store).is_empty(),
        "a refusal queues nothing"
    );
}

/// `backoff` is capped at the 3600 s the Node accepts, before anything is opened.
#[test]
fn backoff_above_an_hour_is_a_usage_error() {
    let parsed = ControlRadioCli::try_parse_from([
        "control.radio",
        "node-send",
        "--radio",
        "p2p",
        "--dev-eui",
        P2P_DEV_EUI,
        "backoff",
        "--seconds",
        "3601",
    ]);
    assert!(parsed.is_err());
}

/// LoRaWAN has no `0x91`: ChirpStack owns the MAC layer.
#[test]
fn node_send_control_commands_are_refused_on_lorawan() {
    let (store, _dir) = seeded_store("send-control-lrw");
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let fake = FakeChirpStack::new();
    enrol_one(&fake, &config, &store, "SN-CTL", "58760700c0668fc7");
    let err = rt()
        .block_on(run_with_store(
            &cli(&[
                "node-send",
                "--radio",
                "lorawan",
                "--serial",
                "SN-CTL",
                "dev-status",
            ])
            .inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            None,
            store,
        ))
        .expect_err("not a LoRaWAN command");
    assert_eq!(err.kind(), Kind::InvalidArgument);
    assert!(fake.downlinks().is_empty());
}

/// H-1 (§3): removing a joined Node with the Detach bit keeps its session, detaching, with
/// only the Detach queued; the registration and the retained areas go as before.
#[test]
fn p2p_node_remove_of_a_detach_capable_node_is_detaching() {
    use control_radio::p2p::tower::control::cmd;
    let (store, _dir) = seeded_store("p2p-remove-detach");
    seed_p2p_node(&store, "80e007b2", true);
    set_p2p_caps(&store, &[cmd::DETACH]);
    let config = p2p_dual_config();
    let fake = FakeChirpStack::new();
    let pubr = FakePublisher::new();
    let outcome = rt()
        .block_on(run_with_store(
            &cli(&["node-remove", "--radio", "p2p", "--dev-eui", P2P_DEV_EUI]).inner,
            &config,
            Some(&fake as &dyn ChirpStack),
            None,
            Some(&pubr as &dyn RetainedPublisher),
            store.clone(),
        ))
        .expect("node-remove");
    match outcome {
        Outcome::Json(v) => assert_eq!(v["state"], "detaching"),
        other => panic!("expected JSON, got {other:?}"),
    }
    assert_eq!(
        pubr.published().len(),
        4,
        "the retained areas are tombstoned"
    );
    let db = control_radio::p2p::db::SessionsDb::load(&store).expect("reload");
    assert!(db.node(P2P_DEV_EUI).is_none(), "the registration went");
    let s = db.sessions.get(P2P_DEV_EUI).expect("the session stays");
    assert!(s.detach.is_some());
    let queued: Vec<_> = s.downlink_queue.iter().map(|d| d.body.clone()).collect();
    assert_eq!(queued, vec![vec![0x03, 0x01, 0x01]]);
}
