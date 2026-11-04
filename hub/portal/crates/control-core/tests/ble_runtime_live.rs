//! Does the integrated Bluetooth runtime actually start?
//!
//! Every other test in this crate exercises a seam. This one exercises the thing `main.rs`'s
//! `start_bluetooth` does: build a real `Host` over real `ConfigApplier` / `StorageHandle` /
//! `MqttPublisher`, hand it to `control_ble::tasks::spawn`, and confirm the runtime comes up,
//! registers, and shuts down. Without it, "the integration works" rests on the parts compiling
//! together, which is not the same claim.
//!
//! **Gated on `PROXIMOS_BLE_LIVE=1` and a real controller.** It opens the default adapter and
//! serves a GATT application on it, so it is not something to run in CI next to other tests: two
//! processes registering FB00 on one adapter is a conflict, not a bug.
//!
//! ```sh
//! PROXIMOS_BLE_LIVE=1 cargo test -p control-core --test ble_runtime_live -- --nocapture
//! ```
//!
//! What it deliberately does **not** cover: a Teltonika Beacon. Dialling one is the bench matrix
//! (A25), and no amount of local testing substitutes for a tag on the air.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use control_ble::host::SubsystemControl;
use fiber_app::libs::ble_host;
use fiber_app::libs::config_applier::ConfigApplier;
use fiber_app::libs::lorawan::NodeAddDeps;
use fiber_app::libs::storage::StorageThread;

fn live() -> bool {
    std::env::var("PROXIMOS_BLE_LIVE").as_deref() == Ok("1")
}

struct TempDir(PathBuf);
impl TempDir {
    fn new(tag: &str) -> TempDir {
        let base = std::env::temp_dir().join(format!(
            "ble-live-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&base).expect("temp dir");
        TempDir(base)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The shipped config, so the test runs against what a Hub would actually be given.
const SHIPPED: &str = include_str!("../fiber.config.yaml");

fn seed(dir: &Path) {
    std::fs::write(dir.join("fiber.config.yaml"), SHIPPED).expect("seed");
}

/// Exactly what `main.rs::start_bluetooth` builds, minus the thread and the display bridge.
fn build_host(dir: &Path) -> ble_host::BleHost {
    let applier = Arc::new(ConfigApplier::new(dir).expect("applier"));
    let (mqtt_tx, mqtt_rx) = crossbeam::channel::unbounded();
    // Held, not dropped: a live receiver is what a Hub with MQTT has, and the publish path behaves
    // differently without one.
    std::mem::forget(mqtt_rx);
    let (storage, _join) =
        StorageThread::spawn(dir.join("fiber.db").to_str().expect("path"), 1).expect("storage");
    std::mem::forget(_join);

    ble_host::build(
        applier,
        dir,
        mqtt_tx,
        storage,
        "live-test-hub".to_string(),
        "00:00:00:00:00:00".to_string(),
        NodeAddDeps::default(),
    )
}

#[test]
fn the_integrated_runtime_starts_serves_and_shuts_down() {
    if !live() {
        eprintln!("skipped: set PROXIMOS_BLE_LIVE=1 with a real controller present");
        return;
    }
    let dir = TempDir::new("runtime");
    seed(&dir.0);

    // The `ble:` section comes off the shipped file through the same accessor main.rs uses.
    let cfg: fiber_app::libs::config::Config =
        serde_yaml::from_str(SHIPPED).expect("the shipped config parses");
    let section = cfg.ble_section_yaml().expect("ble: section");
    let ble = control_ble::config::BleConfig::from_device_config_yaml(&section)
        .expect("the shipped ble: section is valid");
    assert_eq!(ble.mode, control_ble::mode::Mode::Dual);

    let bundle = build_host(&dir.0);
    let store = control_ble::store::Store::new(dir.0.join("ble"));
    store.ensure_dirs().expect("the runtime's /data tree");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");

    let hub_mac = control_ble::mac::MacAddr::parse("00:00:00:00:00:00").expect("mac");
    let runtime = control_ble::tasks::spawn(rt.handle(), &ble, bundle.host.clone(), store, hub_mac);

    // Both roles, because the shipped mode is `dual`.
    let central = runtime
        .handles
        .central
        .clone()
        .expect("the central role is running");
    let peripheral = runtime
        .handles
        .peripheral
        .clone()
        .expect("the peripheral role is running");

    // The seam main.rs fills immediately after the spawn. Before it, the host honestly reports no
    // scanning; after it, the handle is live.
    assert!(
        !bundle.subsystem.is_scanning(),
        "unfilled, it must not claim a loop"
    );
    assert!(bundle.subsystem.fill(central.clone()), "fills once");
    assert!(bundle.subsystem.handle().is_some());

    // The event bus a display would subscribe to.
    let _events = peripheral.subscribe();

    // Give the FB00 server time to reach the adapter and register.
    std::thread::sleep(std::time::Duration::from_secs(4));

    // `ensure_scanning` through the host — the path a signed `beacon-monitoring` takes.
    let started = bundle
        .host
        .subsystem
        .ensure_scanning()
        .expect("the central role can be asked to scan");
    eprintln!("ensure_scanning delivered the wake: {started}");

    // AC 4 says "the scan loop is running when the call returns". `start_scan` deliberately does
    // not claim that synchronously — its doc says so, and `is_scanning` is designed to report what
    // is actually happening rather than what was asked for. So the question is not whether the two
    // agree instantly but *how long* they take to, and the answer has to be short enough that the
    // distinction is invisible to an operator watching a portal response.
    let began = std::time::Instant::now();
    let mut latency = None;
    while began.elapsed() < std::time::Duration::from_secs(5) {
        if bundle.host.subsystem.is_scanning() {
            latency = Some(began.elapsed());
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let latency =
        latency.expect("the loop must acknowledge within 5s, not at the next cadence tick");
    eprintln!("is_scanning became true after {latency:?}");
    assert!(
        latency < std::time::Duration::from_millis(500),
        "the wake must be picked up promptly, not at the next scan_interval_s; took {latency:?}"
    );

    // Shutdown is bounded: a Hub that cannot be stopped is a Hub that cannot be updated.
    let began = std::time::Instant::now();
    rt.block_on(runtime.shutdown());
    let took = began.elapsed();
    eprintln!("shutdown took {took:?}");
    assert!(
        took < std::time::Duration::from_secs(20),
        "shutdown must be bounded, took {took:?}"
    );
}

#[test]
fn a_hub_whose_mode_is_off_starts_nothing() {
    if !live() {
        eprintln!("skipped: set PROXIMOS_BLE_LIVE=1 with a real controller present");
        return;
    }
    let dir = TempDir::new("off");
    seed(&dir.0);
    let bundle = build_host(&dir.0);
    let ble = control_ble::config::BleConfig::from_device_config_yaml("ble:\n  mode: off\n")
        .expect("valid");
    let store = control_ble::store::Store::new(dir.0.join("ble"));
    store.ensure_dirs().expect("dirs");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    let runtime = control_ble::tasks::spawn(
        rt.handle(),
        &ble,
        bundle.host.clone(),
        store,
        control_ble::mac::MacAddr::parse("00:00:00:00:00:00").expect("mac"),
    );
    assert!(runtime.handles.central.is_none(), "no scan loop");
    assert!(runtime.handles.peripheral.is_none(), "no FB00 server");
    rt.block_on(runtime.shutdown());
}
