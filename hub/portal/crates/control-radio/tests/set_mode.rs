//! `set-mode` deletes nothing — `plan/control/radio/node_registry.md` §11.
//!
//! The bench round trip of §11's HIL (p2p → dual → lorawan → p2p), with systemd faked: at each
//! step the plan is executed and what `radio-setup` then runs under the new mode — `create` for
//! each arm the mode enables — is run for real against the store. After every step each file
//! that existed before is still there with the same bytes: the registry, the per-Node caches,
//! `sessions.db`, the P2P `net_id`, the ChirpStack tokens and `config.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::Parser;
use control_radio::cache::{Cache, CacheSlot};
use control_radio::cli::ControlRadioCli;
use control_radio::config::RadioConfig;
use control_radio::dispatch::run_with_store;
use control_radio::lorawan::client::ChirpStack;
use control_radio::lorawan::fake::{FakeChirpStack, FakePublisher};
use control_radio::lorawan::{Scope, Tokens};
use control_radio::setmode::{self, Applied, Systemd};
use control_radio::store::Store;
use control_radio::{Hardware, Mode, Transport};

const RADIO_SECTION: &str = include_str!("fixtures/radio-section.yaml");
const LORAWAN_NODE: &str = "5876070000000413";
const P2P_NODE: &str = "5876070000005722";

struct TempDir(PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(tag: &str) -> TempDir {
    let dir = std::env::temp_dir().join(format!(
        "control-radio-setmode-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    TempDir(dir)
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

/// The rendered radio config for `mode`, the way `proximos-render` writes it.
fn config(mode: Mode) -> RadioConfig {
    let yaml = RADIO_SECTION.replacen("mode: lorawan", &format!("mode: {}", mode.as_str()), 1);
    RadioConfig::from_device_config_yaml(&yaml).expect("radio config")
}

/// Every file under `root`, with its bytes. Lock sidecars are left out: a lock is created on
/// first use and is not state.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if !path.to_string_lossy().ends_with(".lock") {
                out.insert(path.clone(), std::fs::read(&path).expect("read"));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, &mut out);
    out
}

/// Systemd that queues everything and remembers what it was asked.
#[derive(Default)]
struct Queued(std::sync::Mutex<Vec<String>>);

impl Systemd for Queued {
    fn systemctl(&self, args: &[&str]) -> Result<(), String> {
        self.0.lock().expect("lock").push(args.join(" "));
        Ok(())
    }
}

/// What `radio-setup` runs when the restart reaches it: `create` for each arm `mode` enables.
async fn radio_setup(mode: Mode, store: &Store, fake: &FakeChirpStack) {
    let config = config(mode);
    for (transport, args) in [
        (
            Transport::Lorawan,
            &[
                "create",
                "--radio",
                "lorawan",
                "--gateway-eui",
                "0016c001ff1e0000",
            ][..],
        ),
        (Transport::P2p, &["create", "--radio", "p2p"][..]),
    ] {
        if !mode.allows(transport) {
            continue;
        }
        run_with_store(
            &cli(args).inner,
            &config,
            Some(fake as &dyn ChirpStack),
            None,
            None,
            store.clone(),
        )
        .await
        .unwrap_or_else(|e| panic!("create --radio {transport} under {mode:?}: {e}"));
    }
}

async fn node_add(
    config: &RadioConfig,
    store: &Store,
    fake: &FakeChirpStack,
    keys: &Path,
    args: &[&str],
) {
    let mut full = vec!["node-add"];
    full.extend_from_slice(args);
    full.extend(["--keys-file", keys.to_str().expect("utf8")]);
    run_with_store(
        &cli(&full).inner,
        config,
        Some(fake as &dyn ChirpStack),
        None,
        Some(&FakePublisher::new()),
        store.clone(),
    )
    .await
    .unwrap_or_else(|e| panic!("node-add {args:?}: {e}"));
}

#[test]
fn a_mode_round_trip_deletes_nothing_under_the_radio_store() {
    let dir = temp_dir("roundtrip");
    let root = dir.0.join("radio");
    let store = Store::new(&root);
    for scope in Scope::ALL {
        Tokens::new(store.clone())
            .store(scope, "jwt")
            .expect("token");
    }
    let keys = dir.0.join("keys.json");
    std::fs::write(&keys, br#"{"app_key":"000102030405060708090a0b0c0d0e0f"}"#).expect("keys");
    let fake = FakeChirpStack::new();

    // A dual Hub with one Node on each carrier, both heard, and both arms converged.
    rt().block_on(async {
        radio_setup(Mode::Dual, &store, &fake).await;
        let dual = config(Mode::Dual);
        node_add(
            &dual,
            &store,
            &fake,
            &keys,
            &[
                "--radio",
                "lorawan",
                "--profile",
                "default",
                "--serial",
                "2162165722",
                "--dev-eui",
                LORAWAN_NODE,
                "--otaa",
            ],
        )
        .await;
        node_add(
            &dual,
            &store,
            &fake,
            &keys,
            &[
                "--radio",
                "p2p",
                "--profile",
                "default",
                "--serial",
                "2162190902",
                "--dev-eui",
                P2P_NODE,
            ],
        )
        .await;
    });
    for (transport, dev_eui) in [
        (Transport::Lorawan, LORAWAN_NODE),
        (Transport::P2p, P2P_NODE),
    ] {
        Cache::new(&store, transport)
            .merge_write(
                dev_eui,
                CacheSlot::DeviceInfo,
                "2026-09-27T20:00:00Z",
                serde_json::json!({ "fw": "v1.4.0" }),
            )
            .expect("cache");
    }
    let before = snapshot(&root);
    let registry = control_radio::registry::Registry::load(&store)
        .expect("registry")
        .expect("written");
    assert_eq!(registry.nodes.len(), 2, "{registry:?}");
    // What must survive, named, so the test cannot pass on an empty store.
    for held in [
        "node_registry.json".to_string(),
        "p2p/sessions.db".to_string(),
        "p2p/net_id".to_string(),
        // One cache directory for both carriers (node_registry.md §3, R5).
        format!("cache/{P2P_NODE}.json"),
        format!("cache/{LORAWAN_NODE}.json"),
        "lorawan/nodes.json".to_string(),
        "lorawan/chirpstack-api-token-tenant".to_string(),
        "lorawan/config.json".to_string(),
    ] {
        assert!(
            before.contains_key(&root.join(&held)),
            "{held} is not in the seeded store: {:?}",
            before.keys().collect::<Vec<_>>()
        );
    }

    // Down to p2p, then §11's bench path p2p → dual → lorawan → p2p, then off and back.
    let mut from = Mode::Dual;
    for to in [
        Mode::P2p,
        Mode::Dual,
        Mode::Lorawan,
        Mode::P2p,
        Mode::Off,
        Mode::P2p,
    ] {
        let plan = setmode::plan(from, to, Hardware::NorthbridgeAndConcentrator).expect("plan");
        let systemd = Queued::default();
        assert_eq!(
            setmode::execute(&plan, &systemd),
            Applied::Restarting,
            "{from:?}→{to:?}"
        );
        rt().block_on(radio_setup(to, &store, &fake));

        let after = snapshot(&root);
        for (path, bytes) in &before {
            let now = after
                .get(path)
                .unwrap_or_else(|| panic!("{from:?}→{to:?} deleted {}", path.display()));
            // `config.json` is the LoRaWAN converge record; `create` re-records it, so only its
            // existence is held. Everything else is state no mode change may touch.
            if !path.ends_with("config.json") {
                assert_eq!(now, bytes, "{from:?}→{to:?} changed {}", path.display());
            }
        }
        let still = control_radio::registry::Registry::load(&store)
            .expect("registry")
            .expect("still there");
        assert_eq!(
            still, registry,
            "{from:?}→{to:?}: the registry is untouched"
        );
        from = to;
    }
}
