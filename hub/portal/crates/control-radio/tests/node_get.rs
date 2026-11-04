//! `node-get` through the shipped dispatch path — `plan/control/radio/plan.md` §5, §6.
//!
//! A binary of its own because the liveness gate is keyed on a file whose path comes from
//! `PROXIMOS_RADIO_ALIVE`, a **process-global** override. Every test here takes the same lock
//! before touching it, so they serialise among themselves instead of racing the rest of the
//! suite — which they would, invisibly and intermittently, if they lived in `dispatch.rs`.

use std::sync::{Mutex, MutexGuard, OnceLock};

use clap::Parser;
use control_radio::cache::{Cache, CacheSlot};
use control_radio::cli::ControlRadioCli;
use control_radio::config::RadioConfig;
use control_radio::dispatch::{run_with_store, Outcome};
use control_radio::error::{Kind, Transport};
use control_radio::lorawan::enroll::{EnrollmentRecord, NodesFile};
use control_radio::lorawan::{Eui, Scope, Tokens};
use control_radio::store::Store;

const PLAN_EXAMPLE: &str = include_str!("fixtures/radio-section.yaml");
const DEV: &str = "58760700c0668fc7";

struct TempDir(std::path::PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

fn cli(args: &[&str]) -> ControlRadioCli {
    let mut full = vec!["control.radio"];
    full.extend_from_slice(args);
    ControlRadioCli::try_parse_from(full).expect("parses")
}

/// A store with tokens seeded and one node enrolled, in its own temp dir.
///
/// The enrollment record is written directly rather than through `node-add`: `node-get` reads
/// only local state, so standing up a fake ChirpStack and a broker to produce that record
/// would test those instead of the command under test.
fn fixture(tag: &str) -> (Store, TempDir) {
    let dir = std::env::temp_dir().join(format!(
        "control-radio-node-get-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::new(&dir);
    let tokens = Tokens::new(store.clone());
    for s in Scope::ALL {
        tokens.store(s, "jwt").expect("seed");
    }
    let mut nodes = NodesFile::default();
    nodes.by_serial.insert(
        "SN-1".into(),
        EnrollmentRecord {
            serial_number: "SN-1".into(),
            dev_eui: Eui::parse(DEV).expect("eui"),
            profile: "default".into(),
            activation: "otaa".into(),
            join_eui: None,
            name: None,
            app_key: Some("000102030405060708090a0b0c0d0e0f".into()),
        },
    );
    nodes.save(&store).expect("nodes.json");
    (store, TempDir(dir))
}

/// Point the liveness gate at a file inside the fixture, and make it fresh or not.
fn set_alive(store: &Store, fresh: bool) {
    let path = store.root().join("radio-alive");
    std::env::set_var(
        control_radio::tasks::heartbeat::ALIVE_PATH_ENV,
        path.as_os_str(),
    );
    let _ = std::fs::remove_file(&path);
    if fresh {
        control_radio::tasks::heartbeat::touch_at(&path).expect("touches");
    }
}

fn run(store: &Store, args: &[&str]) -> control_radio::Result<Outcome> {
    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    rt().block_on(run_with_store(
        &cli(args).inner,
        &config,
        None,
        None,
        None,
        store.clone(),
    ))
}

fn seed(store: &Store, slot: CacheSlot, at: &str, value: serde_json::Value) {
    Cache::new(store, Transport::Lorawan)
        .merge_write(DEV, slot, at, value)
        .expect("seeds the cache");
}

/// The happy path: a running daemon, a cached reading, and the value plus its age.
#[test]
fn a_fresh_daemon_answers_from_the_cache() {
    let _g = env_lock();
    let (store, _dir) = fixture("fresh");
    set_alive(&store, true);
    seed(
        &store,
        CacheSlot::Message,
        "2026-08-27T09:59:19.187803Z",
        serde_json::json!({"dev_eui": DEV, "fields": {"voltage": 3.36}}),
    );

    let outcome = run(
        &store,
        &[
            "node-get", "--radio", "lorawan", "--serial", "SN-1", "message",
        ],
    )
    .expect("node-get succeeds");

    match outcome {
        Outcome::Json(v) => {
            assert_eq!(v["command"], "node-get");
            assert_eq!(v["transport"], "lorawan");
            assert_eq!(v["id"], DEV, "the serial resolved to the wire identity");
            assert_eq!(v["type"], "message");
            assert_eq!(
                v["received_at"], "2026-08-27T09:59:19.187803Z",
                "radio-reported data may be hours old; the age is part of the answer"
            );
            assert_eq!(v["value"]["fields"]["voltage"], 3.36);
        }
        other => panic!("node-get returns Outcome::Json, got {other:?}"),
    }
}

/// §5's asymmetric corner survives: a `--dev-eui` with no enrollment record still addresses
/// the cache directly.
#[test]
fn a_dev_eui_addresses_the_cache_without_an_enrollment_record() {
    let _g = env_lock();
    let (store, _dir) = fixture("byeui");
    set_alive(&store, true);
    seed(
        &store,
        CacheSlot::Alarm,
        "2026-08-27T10:00:00Z",
        serde_json::json!({"events": []}),
    );
    // Drop the record entirely — the device was enrolled somewhere else.
    NodesFile::default().save(&store).expect("empty nodes.json");

    let outcome = run(
        &store,
        &["node-get", "--radio", "lorawan", "--dev-eui", DEV, "alarm"],
    )
    .expect("addressed directly");
    match outcome {
        Outcome::Json(v) => assert_eq!(v["type"], "alarm"),
        other => panic!("{other:?}"),
    }
}

/// §5: *"Process down → `DaemonDown` for the cache-backed reads, never a silent stale
/// answer."* Exit 23, and it wins over every other diagnosis — a stale cache read on a Hub
/// whose real problem is a stopped daemon would send an operator hunting the wrong fault.
#[test]
fn a_stale_heartbeat_is_daemon_down_even_with_a_full_cache() {
    let _g = env_lock();
    let (store, _dir) = fixture("stale");
    set_alive(&store, false);
    seed(
        &store,
        CacheSlot::Message,
        "2026-08-27T09:00:00Z",
        serde_json::json!({"fields": {"voltage": 3.0}}),
    );

    let err = run(
        &store,
        &[
            "node-get", "--radio", "lorawan", "--serial", "SN-1", "message",
        ],
    )
    .expect_err("the daemon is not running");
    assert_eq!(err.kind(), Kind::DaemonDown);
    assert_eq!(err.exit_code(), 23);
    assert!(
        err.message().contains("proximos-control.service"),
        "the refusal names the remedy: {}",
        err.message()
    );
}

/// The liveness gate runs *before* the node is resolved, so a Hub with a stopped daemon never
/// reports `NotFound` for a device whose real problem is that nothing is listening.
#[test]
fn a_stopped_daemon_outranks_an_unknown_node() {
    let _g = env_lock();
    let (store, _dir) = fixture("order");
    set_alive(&store, false);

    let err = run(
        &store,
        &[
            "node-get",
            "--radio",
            "lorawan",
            "--dev-eui",
            "0011223344556677",
            "message",
        ],
    )
    .expect_err("refused");
    assert_eq!(err.kind(), Kind::DaemonDown);
}

/// A node nothing has been heard from is `NotFound`, and the message says why rather than
/// implying the device does not exist.
#[test]
fn a_node_with_no_cache_file_is_not_found() {
    let _g = env_lock();
    let (store, _dir) = fixture("nocache");
    set_alive(&store, true);

    let err = run(
        &store,
        &[
            "node-get", "--radio", "lorawan", "--serial", "SN-1", "message",
        ],
    )
    .expect_err("nothing received yet");
    assert_eq!(err.kind(), Kind::NotFound);
    assert_eq!(err.exit_code(), 13);
    assert!(err.message().contains("no uplink"), "{}", err.message());
}

/// A node that reports telemetry but has never answered a `get-config` is `NotFound` for that
/// slot — and the refusal points at the command that would trigger one, because `node-get`
/// never does.
#[test]
fn a_slot_that_was_never_filled_names_the_command_that_would_fill_it() {
    let _g = env_lock();
    let (store, _dir) = fixture("noslot");
    set_alive(&store, true);
    seed(
        &store,
        CacheSlot::Message,
        "2026-08-27T10:00:00Z",
        serde_json::json!({"fields": {}}),
    );

    let err = run(
        &store,
        &[
            "node-get", "--radio", "lorawan", "--serial", "SN-1", "config",
        ],
    )
    .expect_err("never asked for its config");
    assert_eq!(err.kind(), Kind::NotFound);
    assert!(
        err.message().contains("node-send get-config"),
        "{}",
        err.message()
    );
    assert!(
        err.message().contains("message"),
        "it says what *is* available: {}",
        err.message()
    );
}

/// An unknown `--serial` is a bookkeeping miss, distinct from a device that exists but has
/// said nothing — the same distinction every other command draws.
#[test]
fn an_unknown_serial_is_a_bookkeeping_miss() {
    let _g = env_lock();
    let (store, _dir) = fixture("badserial");
    set_alive(&store, true);

    let err = run(
        &store,
        &[
            "node-get", "--radio", "lorawan", "--serial", "SN-NOPE", "message",
        ],
    )
    .expect_err("no such record");
    assert_eq!(err.kind(), Kind::NotFound);
    assert_eq!(
        err.component(),
        control_radio::error::Component::EnrollmentRecord
    );
    assert!(err.message().contains("SN-NOPE"), "{}", err.message());
}

/// Every `<type>` reads its own slot; none of them reaches another's.
#[test]
fn each_type_reads_its_own_slot() {
    let _g = env_lock();
    let (store, _dir) = fixture("slots");
    set_alive(&store, true);
    for (slot, arg) in [
        (CacheSlot::Message, "message"),
        (CacheSlot::DeviceInfo, "device-info"),
        (CacheSlot::Alarm, "alarm"),
        (CacheSlot::Config, "config"),
        (CacheSlot::FullConfig, "full-config"),
    ] {
        seed(
            &store,
            slot,
            "2026-08-27T10:00:00Z",
            serde_json::json!({"which": slot.as_str()}),
        );
        let outcome = run(
            &store,
            &["node-get", "--radio", "lorawan", "--serial", "SN-1", arg],
        )
        .unwrap_or_else(|e| panic!("{arg}: {e}"));
        match outcome {
            Outcome::Json(v) => {
                assert_eq!(v["type"], slot.as_str());
                assert_eq!(v["value"]["which"], slot.as_str());
            }
            other => panic!("{arg}: {other:?}"),
        }
    }
}

/// The P2P arm reads its own registry and cache (`p2p.md` §7), so it answers about the node
/// the operator named rather than about the arm. Two honest refusals, neither of them
/// `Phase2NotImplemented`: a serial that cannot be a P2P device is the caller's mistake, and a
/// well-formed serial nobody registered is a bookkeeping miss.
#[test]
fn the_p2p_arm_answers_about_the_node_not_about_the_arm() {
    let _g = env_lock();

    // The plan example with both arms enabled, so the mode gate lets `--radio p2p` through to
    // the command rather than refusing it as P2PUnavailable.
    let dual = PLAN_EXAMPLE.replacen("mode: lorawan", "mode: dual", 1);
    let config = RadioConfig::from_device_config_yaml(&dual).expect("dual is a valid config");

    // A LoRaWAN-shaped name is not a P2P serial: the registry key is the on-air serial.
    let (store, _dir) = fixture("p2p-bad-serial");
    set_alive(&store, true);
    let err = rt()
        .block_on(run_with_store(
            &cli(&["node-get", "--radio", "p2p", "--serial", "SN-1", "message"]).inner,
            &config,
            None,
            None,
            None,
            store,
        ))
        .expect_err("SN-1 cannot be canonicalised to an on-air serial");
    assert_eq!(err.kind(), Kind::InvalidArgument);
    assert_eq!(err.exit_code(), 2);

    // Well-formed, but nothing was ever registered under it.
    let (store, _dir) = fixture("p2p-empty-registry");
    set_alive(&store, true);
    let err = rt()
        .block_on(run_with_store(
            &cli(&[
                "node-get",
                "--radio",
                "p2p",
                "--serial",
                "0x80e007b2",
                "message",
            ])
            .inner,
            &config,
            None,
            None,
            None,
            store,
        ))
        .expect_err("an empty registry has no such node");
    assert_eq!(err.kind(), Kind::NotFound);
    assert_eq!(err.exit_code(), 13);
}

/// `node-get` **never triggers a downlink** (§5). Proven structurally: the command runs to
/// completion with no ChirpStack client injected at all, so there was nothing to enqueue on.
#[test]
fn node_get_never_reaches_chirpstack() {
    let _g = env_lock();
    let (store, _dir) = fixture("nodownlink");
    set_alive(&store, true);
    seed(
        &store,
        CacheSlot::DeviceInfo,
        "2026-08-27T10:00:00Z",
        serde_json::json!({"fw_version": "1.4.0"}),
    );

    let config = RadioConfig::from_device_config_yaml(PLAN_EXAMPLE).expect("plan example");
    let parsed = cli(&[
        "node-get",
        "--radio",
        "lorawan",
        "--serial",
        "SN-1",
        "device-info",
    ]);
    assert!(
        !control_radio::dispatch::needs_chirpstack(&parsed.inner, &config),
        "node-get is a local read; connecting would make it fail when ChirpStack is down"
    );
    run(
        &store,
        &[
            "node-get",
            "--radio",
            "lorawan",
            "--serial",
            "SN-1",
            "device-info",
        ],
    )
    .expect("answers with no client injected");
}

const P2P_SERIAL: &str = "80e007b2";
const P2P_DEV_EUI: &str = "70b3d57ed0000abe";

/// The plan example with both arms enabled, so `--radio p2p` reaches the command rather than
/// being refused by the mode gate.
fn dual_config() -> RadioConfig {
    let dual = PLAN_EXAMPLE.replacen("mode: lorawan", "mode: dual", 1);
    RadioConfig::from_device_config_yaml(&dual).expect("dual is a valid config")
}

/// One registered P2P node — the registry `node-get --radio p2p` resolves against.
fn register_p2p(store: &Store) {
    use control_radio::p2p::db::SessionsDb;
    use control_radio::p2p::session::RegisteredNode;
    let mut db = SessionsDb::load(store).expect("load");
    db.upsert_node(RegisteredNode {
        serial: P2P_SERIAL.into(),
        dev_eui: P2P_DEV_EUI.into(),
        profile: "default".into(),
        app_key: "00".repeat(16),
        name: None,
    });
    db.save(store).expect("save");
}

fn run_dual(store: &Store, args: &[&str]) -> control_radio::Result<Outcome> {
    rt().block_on(run_with_store(
        &cli(args).inner,
        &dual_config(),
        None,
        None,
        None,
        store.clone(),
    ))
}

fn json(outcome: Outcome) -> serde_json::Value {
    match outcome {
        Outcome::Json(v) => v,
        other => panic!("node-get returns Outcome::Json, got {other:?}"),
    }
}

/// PI1: the P2P central runs inside `proximos-control`, so a stale heartbeat means nothing is
/// filling the P2P cache either — `DaemonDown`, exactly as on the LoRaWAN arm, never a stale
/// answer out of a cache nobody is writing.
#[test]
fn node_get_over_p2p_is_gated_on_the_heartbeat() {
    let _g = env_lock();
    let (store, _dir) = fixture("p2p-stale");
    set_alive(&store, false);
    register_p2p(&store);
    Cache::new(&store, Transport::P2p)
        .merge_write(
            P2P_DEV_EUI,
            CacheSlot::Message,
            "2026-09-26T09:00:00Z",
            serde_json::json!({"fields": {"voltage": 3.0}}),
        )
        .expect("seeds the cache");

    let err = run_dual(
        &store,
        &[
            "node-get",
            "--radio",
            "p2p",
            "--dev-eui",
            P2P_DEV_EUI,
            "message",
        ],
    )
    .expect_err("the daemon is not running");
    assert_eq!(err.kind(), Kind::DaemonDown);
}

/// One `node-get` shape, whichever carrier answered. A consumer reading a config dump back
/// must not have to branch on the arm: the same six keys, the same `value`, and `transport`
/// the only thing that says where it came from. Before this, the P2P arm answered with
/// `{radio, serial, last_reading, last_alarm}` out of the retired `SessionsDb.cache`.
#[test]
fn both_carriers_answer_node_get_config_in_one_shape() {
    let _g = env_lock();
    let (store, _dir) = fixture("one-shape");
    set_alive(&store, true);
    register_p2p(&store);

    let dump = serde_json::json!({
        "page_count": 2,
        "complete": true,
        "config": {"application": {"interval_report": 120}},
    });
    for (transport, id) in [(Transport::Lorawan, DEV), (Transport::P2p, P2P_DEV_EUI)] {
        Cache::new(&store, transport)
            .merge_write(id, CacheSlot::Config, "2026-09-14T10:00:00Z", dump.clone())
            .expect("seeds the cache");
    }

    let lorawan = json(
        run_dual(
            &store,
            &[
                "node-get", "--radio", "lorawan", "--serial", "SN-1", "config",
            ],
        )
        .expect("the LoRaWAN arm answers"),
    );
    let p2p = json(
        run_dual(
            &store,
            &[
                "node-get",
                "--radio",
                "p2p",
                "--serial",
                "0x80e007b2",
                "config",
            ],
        )
        .expect("the P2P arm answers"),
    );

    let keys = |v: &serde_json::Value| {
        v.as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        keys(&lorawan),
        // `serde_json::Map` is a BTreeMap here, so this is the key *set* in its sorted
        // spelling — `{command, transport, id, type, received_at, value}`.
        vec!["command", "id", "received_at", "transport", "type", "value"],
        "the documented key set"
    );
    assert_eq!(keys(&p2p), keys(&lorawan), "one shape, not two");

    assert_eq!(lorawan["transport"], "lorawan");
    assert_eq!(p2p["transport"], "p2p");
    assert_eq!(lorawan["id"], DEV, "LoRaWAN is keyed by wire identity");
    assert_eq!(
        p2p["id"], P2P_DEV_EUI,
        "both carriers are keyed by dev_eui (#73)"
    );
    for k in ["command", "type", "received_at", "value"] {
        assert_eq!(lorawan[k], p2p[k], "{k} must not differ by carrier");
    }
    assert_eq!(p2p["value"], dump);
}

/// Since #73 both carriers' caches are keyed by `dev_eui`, so the DevEUI an operator (or the
/// mobile app) holds addresses the document directly — no registry indirection at all. The
/// registry is still consulted, but only to prove the node is registered.
///
/// The heartbeat is fresh: since PI1 the P2P central runs inside `proximos-control`, so the
/// P2P arm is gated on its liveness file like the LoRaWAN one
/// (`node_get_over_p2p_is_gated_on_the_heartbeat`).
#[test]
fn a_p2p_dev_eui_addresses_the_dev_eui_keyed_cache_directly() {
    let _g = env_lock();
    let (store, _dir) = fixture("p2p-by-eui");
    set_alive(&store, true);
    register_p2p(&store);
    Cache::new(&store, Transport::P2p)
        .merge_write(
            P2P_DEV_EUI,
            CacheSlot::Message,
            "2026-09-14T10:00:00Z",
            serde_json::json!({"fields": {"voltage": 3.36}}),
        )
        .expect("seeds the P2P cache");

    let v = json(
        run_dual(
            &store,
            &[
                "node-get",
                "--radio",
                "p2p",
                "--dev-eui",
                P2P_DEV_EUI,
                "message",
            ],
        )
        .expect("the DevEUI addresses the cache directly"),
    );
    assert_eq!(v["id"], P2P_DEV_EUI);
    assert_eq!(v["transport"], "p2p");
    assert_eq!(v["value"]["fields"]["voltage"], 3.36);
}

/// The other direction, and the one that moved: `--serial` is now an *alias*. The serial is
/// printed on the device, so it stays a way in, but it is no longer a key — it resolves
/// through the registry to the DevEUI the cache document is named after. A serial nobody
/// registered is a `node-registry` miss, not an empty answer from the cache.
#[test]
fn a_p2p_serial_resolves_through_the_registry_to_the_dev_eui_keyed_cache() {
    let _g = env_lock();
    let (store, _dir) = fixture("p2p-serial-alias");
    set_alive(&store, true);
    register_p2p(&store);
    Cache::new(&store, Transport::P2p)
        .merge_write(
            P2P_DEV_EUI,
            CacheSlot::Message,
            "2026-09-14T10:00:00Z",
            serde_json::json!({"fields": {"voltage": 3.36}}),
        )
        .expect("seeds the P2P cache");

    let v = json(
        run_dual(
            &store,
            &[
                "node-get",
                "--radio",
                "p2p",
                "--serial",
                "0x80e007b2",
                "message",
            ],
        )
        .expect("the serial resolves through the registry"),
    );
    assert_eq!(
        v["id"], P2P_DEV_EUI,
        "the answer names the dev_eui the document is keyed by, not the alias typed"
    );
    assert_eq!(v["value"]["fields"]["voltage"], 3.36);

    let err = run_dual(
        &store,
        &[
            "node-get",
            "--radio",
            "p2p",
            "--serial",
            "0xdeadbeef",
            "message",
        ],
    )
    .expect_err("an unregistered serial has nothing to resolve to");
    assert_eq!(err.kind(), control_radio::error::Kind::NotFound);
    assert_eq!(
        err.detail().component,
        control_radio::error::Component::NodeRegistry,
        "a serial nobody registered is a registry miss, not an empty cache"
    );
    assert!(err.message().contains("deadbeef"), "{}", err.message());
}

/// The P2P counterpart of the LoRaWAN refusal: a node that reports telemetry but has never
/// answered a `get-config` is `NotFound` for that slot, and the message points at the command
/// that would fill it — `node-get` never triggers a downlink on either carrier.
#[test]
fn a_p2p_slot_never_filled_names_the_command_that_would_fill_it() {
    let _g = env_lock();
    let (store, _dir) = fixture("p2p-noslot");
    set_alive(&store, true);
    register_p2p(&store);
    Cache::new(&store, Transport::P2p)
        .merge_write(
            P2P_DEV_EUI,
            CacheSlot::Message,
            "2026-09-14T10:00:00Z",
            serde_json::json!({"fields": {}}),
        )
        .expect("seeds the P2P cache");

    let err = run_dual(
        &store,
        &[
            "node-get",
            "--radio",
            "p2p",
            "--serial",
            "0x80e007b2",
            "config",
        ],
    )
    .expect_err("never asked for its config");
    assert_eq!(err.kind(), Kind::NotFound);
    assert!(
        err.message().contains("node-send get-config"),
        "{}",
        err.message()
    );
    assert!(
        err.message().contains("message"),
        "it says what *is* available: {}",
        err.message()
    );
}
