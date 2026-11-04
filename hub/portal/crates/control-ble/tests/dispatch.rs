//! Dispatch — `plan/control/ble/plan.md` §1, §5, §6.
//!
//! The gate order is the thing under test here, more than any individual command: the mode gate runs
//! before argument validation, and the capability flag is decided before the adapter or the daemon.
//! Getting either backwards produces an answer that sends an operator to the wrong place.

use std::sync::Arc;

use clap::Parser;

use control_ble::cli::ControlBleCli;
use control_ble::config::{BleConfig, En12830Config, EyeSection};
use control_ble::dispatch::{run_with_store, Outcome};
use control_ble::host::{
    ConfigStore, Host, MemoryConfigStore, NullPublisher, NullReadingSink, Origin,
};
use control_ble::peripheral::paging::{build_page, BeaconList};
use control_ble::store::Store;
use control_ble::{Kind, Role};

const PLAN_FIXTURE: &str = include_str!("fixtures/ble-section.yaml");

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new(tag: &str) -> TempDir {
        let base = std::env::temp_dir().join(format!(
            "control-ble-dispatch-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        TempDir(base)
    }
    fn store(&self) -> Store {
        Store::new(&self.0)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn cfg_dual() -> BleConfig {
    BleConfig::from_device_config_yaml(PLAN_FIXTURE).expect("the plan fixture parses")
}

fn cfg(mode: &str) -> BleConfig {
    BleConfig::from_device_config_yaml(&format!("ble:\n  mode: {mode}\n")).expect("parses")
}

fn host_with(section: EyeSection) -> (Host, Arc<MemoryConfigStore>) {
    let config = Arc::new(MemoryConfigStore::with_section(section));
    let host = Host {
        config: config.clone(),
        publisher: Arc::new(NullPublisher),
        readings: Arc::new(NullReadingSink),
        ..Host::in_memory()
    };
    (host, config)
}

fn parse(argv: &[&str]) -> ControlBleCli {
    let mut full = vec!["control.ble"];
    full.extend_from_slice(argv);
    ControlBleCli::try_parse_from(full).expect("parses")
}

/// Run one command with no hosting process — the one-shot CLI case.
async fn run_oneshot(
    argv: &[&str],
    config: &BleConfig,
    host: &Host,
    store: Store,
) -> control_ble::Result<Outcome> {
    let cli = parse(argv);
    run_with_store(&cli.inner, config, host, None, store).await
}

/// Every command, in the minimal form the clap tree accepts.
fn all_commands() -> Vec<Vec<&'static str>> {
    const MAC: &str = "AA:BB:CC:DD:EE:FF";
    vec![
        vec!["create"],
        vec!["status"],
        vec!["beacon-scan-start"],
        vec!["beacon-scan-stop"],
        vec!["beacon-scan-list"],
        vec!["beacon-list"],
        vec!["beacon-add", "--mac", MAC],
        vec!["beacon-remove", "--mac", MAC],
        vec!["beacon-info", "--mac", MAC],
        vec!["beacon-get", "--mac", MAC, "reading"],
        vec!["beacon-provision", "--mac", MAC],
        vec!["beacon-send", "config", "--mac", MAC, "--tx-power-dbm", "4"],
        vec!["beacon-detect", "--mac", MAC],
        vec!["beacon-recording", "--mac", MAC, "--interval-min", "5"],
        vec!["beacon-history", "--mac", MAC],
        vec!["beacon-monitoring", "--enabled", "true"],
        vec!["known-beacons", "--macs", MAC],
        vec!["phone-session-start"],
        vec!["phone-session-stop"],
        vec!["phone-session-status"],
        vec!["phone-advertise-start"],
        vec!["phone-advertise-stop"],
    ]
}

#[tokio::test]
async fn off_mode_refuses_every_command_as_off_mode() {
    let t = TempDir::new("off");
    let (host, _) = host_with(EyeSection::default());
    let off = cfg("off");
    let commands = all_commands();
    assert_eq!(commands.len(), 22, "§5 publishes 22 commands");

    for argv in &commands {
        match run_oneshot(argv, &off, &host, t.store()).await {
            Err(e) => assert_eq!(e.kind(), Kind::OffMode, "{argv:?}"),
            Ok(out) => panic!(
                "{argv:?} must not succeed on an `off` Hub: {}",
                out.to_value()
            ),
        }
    }
}

#[tokio::test]
async fn off_mode_is_exit_35_with_the_ble_mode_component() {
    let t = TempDir::new("off35");
    let (host, config_store) = host_with(EyeSection::default());
    let off = cfg("off");
    for argv in all_commands() {
        let err = run_oneshot(&argv, &off, &host, t.store())
            .await
            .expect_err("refused");
        assert_eq!(err.kind(), Kind::OffMode, "{argv:?}");
        assert_eq!(err.exit_code(), 35, "{argv:?}");
        assert_eq!(err.component(), control_ble::Component::BleMode, "{argv:?}");
    }
    // And nothing was written on the way to any of those refusals.
    assert_eq!(config_store.write_count(), 0);
}

#[tokio::test]
async fn the_mode_gate_runs_before_argument_validation() {
    // A malformed `--mac` on an `off` Hub must answer OffMode, not InvalidArgument: the honest
    // answer is that no BLE role exists here at all.
    let t = TempDir::new("order");
    let (host, _) = host_with(EyeSection::default());
    let err = run_oneshot(
        &["beacon-add", "--mac", "AA:BB:CC:DD:EE"],
        &cfg("off"),
        &host,
        t.store(),
    )
    .await
    .expect_err("refused");
    assert_eq!(err.kind(), Kind::OffMode);

    // On a Hub where the role exists, the same argument is a usage error.
    let err = run_oneshot(
        &["beacon-add", "--mac", "AA:BB:CC:DD:EE"],
        &cfg("central"),
        &host,
        t.store(),
    )
    .await
    .expect_err("refused");
    assert_eq!(err.kind(), Kind::InvalidArgument);
}

#[tokio::test]
async fn under_dual_the_spanning_commands_cover_both_roles_without_a_flag() {
    // Two independent sources require this, and they are why `RoleParameterRequired` has no
    // phase-1 caller: §5's golden `status --json` carries BOTH role blocks under `"ble_mode":
    // "dual"`, and `ble-setup.service` runs `create` with no `--role` on a Hub whose shipped mode
    // is `dual`. Demanding the flag would fail the boot unit on every shipped device.
    let t = TempDir::new("dual");
    let (host, _) = host_with(EyeSection::default());
    let dual = cfg_dual();

    let v = run_oneshot(&["status"], &dual, &host, t.store())
        .await
        .expect("status spans both roles under dual")
        .to_value();
    assert_eq!(v["ble_mode"], "dual");
    assert!(v["central"].is_object(), "the §5 golden shape carries both");
    assert!(v["peripheral"].is_object(), "{v}");

    let v = run_oneshot(&["create", "--dry-run"], &dual, &host, t.store())
        .await
        .expect("create spans both roles under dual — the boot unit passes no --role")
        .to_value();
    assert_eq!(
        v["roles"].as_array().expect("array").len(),
        2,
        "both roles are initialised, each with its own outcome: {v}"
    );

    // An explicit role narrows both of them to one.
    let v = run_oneshot(&["status", "--role", "central"], &dual, &host, t.store())
        .await
        .expect("narrowed")
        .to_value();
    assert!(v["central"].is_object());
    assert!(
        v.get("peripheral").is_none(),
        "--role narrows the report: {v}"
    );
    let v = run_oneshot(
        &["create", "--role", "peripheral", "--dry-run"],
        &dual,
        &host,
        t.store(),
    )
    .await
    .expect("narrowed")
    .to_value();
    assert_eq!(v["roles"], serde_json::json!(["peripheral"]));

    // Every other command carries its role in its name and needs no flag either.
    let out = run_oneshot(&["beacon-list"], &dual, &host, t.store())
        .await
        .expect("beacon-list needs no --role under dual");
    assert_eq!(out.to_value()["total"], 0);
}

#[tokio::test]
async fn a_role_the_mode_excludes_is_refused_with_its_own_code() {
    let t = TempDir::new("roles");
    let (host, _) = host_with(EyeSection::default());

    let err = run_oneshot(&["beacon-list"], &cfg("peripheral"), &host, t.store())
        .await
        .expect_err("no central role");
    assert_eq!(err.kind(), Kind::CentralUnavailable);
    assert_eq!(err.exit_code(), 36);
    assert_eq!(err.role(), Some(Role::Central));

    let err = run_oneshot(&["phone-session-status"], &cfg("central"), &host, t.store())
        .await
        .expect_err("no peripheral role");
    assert_eq!(err.kind(), Kind::PeripheralUnavailable);
    assert_eq!(err.exit_code(), 37);
}

#[tokio::test]
async fn status_works_with_no_adapter_and_no_live_process() {
    // `status` never fails for a reason it exists to report.
    let t = TempDir::new("status");
    let (host, _) = host_with(EyeSection::default());
    let out = run_oneshot(&["status"], &cfg("central"), &host, t.store())
        .await
        .expect("status must answer");
    let v = out.to_value();
    assert_eq!(v["ble_mode"], "central");
    assert!(v["central"].is_object(), "the block is emitted regardless");
    assert_eq!(v["central"]["scanning"], false);
    assert_eq!(v["central"]["provisioned"], false);
    assert!(
        v.get("peripheral").is_none(),
        "the mode excludes it, so it is absent"
    );
    // Whether an adapter exists on this build host is not the point; that it answered is.
    assert!(v["adapter"]["present"].is_boolean());
}

#[tokio::test]
async fn live_only_commands_report_daemon_down_without_a_host_process() {
    let t = TempDir::new("daemon");
    let (host, _) = host_with(EyeSection::default());
    let dual = cfg_dual();
    // The §6 row 23 list, exactly.
    for argv in [
        vec!["beacon-get", "--mac", "AA:BB:CC:DD:EE:FF", "reading"],
        vec!["beacon-scan-list"],
        vec!["beacon-scan-start"],
        vec!["beacon-scan-stop"],
        vec!["phone-session-status"],
        vec!["phone-advertise-start"],
        vec!["phone-advertise-stop"],
        vec!["phone-session-start"],
        vec!["phone-session-stop"],
    ] {
        match run_oneshot(&argv, &dual, &host, t.store()).await {
            Err(e) => assert_eq!(e.kind(), Kind::DaemonDown, "{argv:?}"),
            Ok(out) => panic!("{argv:?} must not answer from nothing: {}", out.to_value()),
        }
    }
}

#[tokio::test]
async fn the_daemon_down_code_and_component_are_the_documented_ones() {
    let t = TempDir::new("daemon2");
    let (host, _) = host_with(EyeSection::default());
    let err = run_oneshot(&["beacon-scan-list"], &cfg_dual(), &host, t.store())
        .await
        .expect_err("no daemon");
    assert_eq!(err.kind(), Kind::DaemonDown);
    assert_eq!(err.exit_code(), 23);
    assert_eq!(err.component(), control_ble::Component::ControlSocket);
}

#[tokio::test]
async fn capability_disabled_is_decided_before_the_adapter_and_before_the_daemon() {
    // AC 3, and the ordering that makes it legible: with the flag off the answer is 39, NOT 23.
    // Answering `DaemonDown` would send an operator looking for a missing process when the real
    // reason is a config flag they can see.
    let t = TempDir::new("capability");
    let (host, _) = host_with(EyeSection {
        en12830: En12830Config {
            enabled: false,
            protocol_failure_alarm_after: 3,
        },
        ..EyeSection::default()
    });
    let dual = cfg_dual();
    for argv in [
        vec!["beacon-detect", "--mac", "AA:BB:CC:DD:EE:FF"],
        vec!["beacon-history", "--mac", "AA:BB:CC:DD:EE:FF"],
        vec![
            "beacon-recording",
            "--mac",
            "AA:BB:CC:DD:EE:FF",
            "--interval-min",
            "5",
        ],
    ] {
        let err = run_oneshot(&argv, &dual, &host, t.store())
            .await
            .expect_err("capability off");
        assert_eq!(err.kind(), Kind::CapabilityDisabled, "{argv:?}");
        assert_eq!(err.exit_code(), 39, "{argv:?}");
        assert!(
            err.message().contains("eye.en12830.enabled: false"),
            "the message names the flag: {}",
            err.message()
        );
    }
}

#[tokio::test]
async fn with_the_capability_on_the_same_commands_reach_the_daemon_check() {
    let t = TempDir::new("capability-on");
    let (host, _) = host_with(EyeSection::default());
    let err = run_oneshot(
        &["beacon-detect", "--mac", "AA:BB:CC:DD:EE:FF"],
        &cfg_dual(),
        &host,
        t.store(),
    )
    .await
    .expect_err("no daemon");
    assert_eq!(
        err.kind(),
        Kind::DaemonDown,
        "flag on → the next gate is the daemon"
    );
}

#[tokio::test]
async fn a_refused_recording_interval_is_never_snapped() {
    let t = TempDir::new("interval");
    let (host, _) = host_with(EyeSection::default());
    let err = run_oneshot(
        &[
            "beacon-recording",
            "--mac",
            "AA:BB:CC:DD:EE:FF",
            "--interval-min",
            "7",
        ],
        &cfg_dual(),
        &host,
        t.store(),
    )
    .await
    .expect_err("7 is not supported");
    assert_eq!(err.kind(), Kind::InvalidArgument);
}

#[tokio::test]
async fn beacon_add_then_beacon_list_page_matches_fb0f_byte_for_byte() {
    let t = TempDir::new("paging");
    let (host, config_store) = host_with(EyeSection::default());
    let dual = cfg_dual();
    for i in 0..14 {
        let mac = format!("AA:BB:CC:DD:EE:{i:02X}");
        run_oneshot(
            &["beacon-add", "--mac", &mac, "--name", "Freezer aisle 3"],
            &dual,
            &host,
            t.store(),
        )
        .await
        .expect("enrols");
    }
    let list = BeaconList::from_tags(&config_store.load().expect("load").tags);
    for page in 0..5u16 {
        let page_s = page.to_string();
        let out = run_oneshot(
            &["beacon-list", "--page", &page_s, "--mtu", "185"],
            &dual,
            &host,
            t.store(),
        )
        .await
        .expect("page");
        let expected = build_page(&list, page, 185, dual.peripheral.max_page_bytes);
        assert_eq!(
            out.to_value(),
            serde_json::to_value(&expected).expect("serialize"),
            "page {page}: `beacon-list --page` must be exactly what FB0F answers"
        );
    }
}

#[tokio::test]
async fn beacon_remove_unknown_is_not_found_at_the_cli() {
    // Unlike FB10, the CLI has an exit code to say this with.
    let t = TempDir::new("remove");
    let (host, _) = host_with(EyeSection::default());
    let err = run_oneshot(
        &["beacon-remove", "--mac", "AA:BB:CC:DD:EE:FF"],
        &cfg_dual(),
        &host,
        t.store(),
    )
    .await
    .expect_err("not enrolled");
    assert_eq!(err.kind(), Kind::NotFound);
    assert_eq!(err.exit_code(), 13);
    assert_eq!(err.component(), control_ble::Component::BeaconRegistry);
}

#[tokio::test]
async fn beacon_add_reports_the_monitoring_outcome_and_unavailable_without_a_loop() {
    let t = TempDir::new("add");
    let (host, config_store) = host_with(EyeSection {
        enabled: false,
        ..EyeSection::default()
    });
    let out = run_oneshot(
        &[
            "beacon-add",
            "--mac",
            "aa:bb:cc:dd:ee:ff",
            "--name",
            "Freezer 1",
        ],
        &cfg_dual(),
        &host,
        t.store(),
    )
    .await
    .expect("enrols");
    let v = out.to_value();
    assert_eq!(v["command"], "beacon-add");
    assert_eq!(v["role"], "central");
    assert_eq!(v["mac"], "AA:BB:CC:DD:EE:FF", "normalized on every surface");
    assert_eq!(v["name"], "Freezer 1");
    assert_eq!(
        v["monitoring"], "unavailable",
        "the flag flipped but no loop could start, and the result says so rather than nothing"
    );
    assert_eq!(v["already_enrolled"], false);
    assert!(config_store.load().expect("load").enabled);
}

#[tokio::test]
async fn beacon_monitoring_reports_whether_it_changed_anything() {
    let t = TempDir::new("monitoring");
    let (host, _) = host_with(EyeSection {
        enabled: false,
        ..EyeSection::default()
    });
    let dual = cfg_dual();
    let v = run_oneshot(
        &["beacon-monitoring", "--enabled", "true"],
        &dual,
        &host,
        t.store(),
    )
    .await
    .expect("enables")
    .to_value();
    assert_eq!(v["enabled"], true);
    assert_eq!(v["changed"], true);
    // Idempotent.
    let v = run_oneshot(
        &["beacon-monitoring", "--enabled", "true"],
        &dual,
        &host,
        t.store(),
    )
    .await
    .expect("again")
    .to_value();
    assert_eq!(v["changed"], false);
}

#[tokio::test]
async fn known_beacons_validates_and_reports_but_never_installs() {
    let t = TempDir::new("known");
    let (host, config_store) = host_with(EyeSection::default());
    let dual = cfg_dual();
    let v = run_oneshot(
        &[
            "known-beacons",
            "--macs",
            "aa:bb:cc:dd:ee:ff,00:11:22:33:44:55",
        ],
        &dual,
        &host,
        t.store(),
    )
    .await
    .expect("validates")
    .to_value();
    assert_eq!(v["count"], 2);
    assert_eq!(v["accepted"][0], "AA:BB:CC:DD:EE:FF");
    assert_eq!(
        v["installed"], false,
        "the allowlist is daemon state; the CLI reports what would be accepted"
    );
    assert_eq!(
        config_store.write_count(),
        0,
        "and nothing reached the config file"
    );

    // An empty list is legal.
    let v = run_oneshot(&["known-beacons", "--macs", ""], &dual, &host, t.store())
        .await
        .expect("empty is legal")
        .to_value();
    assert_eq!(v["count"], 0);

    // A malformed entry names itself.
    let err = run_oneshot(
        &["known-beacons", "--macs", "AA:BB:CC:DD:EE:FF,nope"],
        &dual,
        &host,
        t.store(),
    )
    .await
    .expect_err("malformed");
    assert_eq!(err.kind(), Kind::InvalidArgument);
    assert!(err.message().contains("nope"), "{}", err.message());
}

#[tokio::test]
async fn create_dry_run_writes_nothing() {
    let t = TempDir::new("dryrun");
    let (host, _) = host_with(EyeSection::default());
    let v = run_oneshot(
        &["create", "--role", "central", "--dry-run"],
        &cfg_dual(),
        &host,
        t.store(),
    )
    .await
    .expect("dry run")
    .to_value();
    assert_eq!(v["dry_run"], true);
    assert!(!t.0.exists(), "a dry run creates no directories at all");
    assert!(!t.store().is_provisioned(Role::Central));
}

#[tokio::test]
async fn create_is_degraded_green_and_withholds_the_marker() {
    // The property that makes a controller-less Hub boot green: exit 0, a warning, no marker — so
    // the next boot re-probes rather than assuming.
    let t = TempDir::new("create");
    let (host, _) = host_with(EyeSection::default());
    let v = run_oneshot(&["create"], &cfg("central"), &host, t.store())
        .await
        .expect("create must not fail for a missing controller")
        .to_value();
    assert_eq!(v["command"], "create");
    assert_eq!(v["roles"][0], "central");
    assert_eq!(
        v["provisioned"]["central"], false,
        "provisioned-complete and degraded-green are different states"
    );
    assert!(
        !v["warnings"].as_array().expect("array").is_empty(),
        "degraded-green is loud, not silent: {v}"
    );
    assert!(
        !t.store().is_provisioned(Role::Central),
        "the marker is withheld so the next boot re-probes"
    );
    // But the /data tree exists, so the first give-up has somewhere to go.
    assert!(t.0.join("central/archive").is_dir());
}

#[tokio::test]
async fn beacon_info_reports_both_halves_and_not_found_when_neither_exists() {
    let t = TempDir::new("info");
    let (host, _) = host_with(EyeSection::default());
    let dual = cfg_dual();
    let err = run_oneshot(
        &["beacon-info", "--mac", "AA:BB:CC:DD:EE:FF"],
        &dual,
        &host,
        t.store(),
    )
    .await
    .expect_err("unknown");
    assert_eq!(err.kind(), Kind::NotFound);

    control_ble::central::enroll::add(&host, Origin::Cli, "AA:BB:CC:DD:EE:FF", Some("Freezer 1"))
        .expect("enrols");
    let v = run_oneshot(
        &["beacon-info", "--mac", "AA:BB:CC:DD:EE:FF"],
        &dual,
        &host,
        t.store(),
    )
    .await
    .expect("known")
    .to_value();
    assert_eq!(v["mac"], "AA:BB:CC:DD:EE:FF");
    assert_eq!(v["enrollment"]["name"], "Freezer 1");
    assert!(
        v["provisioning"].is_null(),
        "never attempted is absent, not false"
    );
}

#[tokio::test]
async fn beacon_list_without_a_page_lists_everything_with_its_rev() {
    let t = TempDir::new("listall");
    let (host, _) = host_with(EyeSection::default());
    let dual = cfg_dual();
    for i in 0..3 {
        control_ble::central::enroll::add(
            &host,
            Origin::Cli,
            &format!("AA:BB:CC:DD:EE:{i:02X}"),
            None,
        )
        .expect("enrols");
    }
    let v = run_oneshot(&["beacon-list"], &dual, &host, t.store())
        .await
        .expect("lists")
        .to_value();
    assert_eq!(v["total"], 3);
    assert_eq!(v["rev"].as_str().expect("rev").len(), 8);
    assert_eq!(v["items"].as_array().expect("array").len(), 3);
}

#[tokio::test]
async fn a_hub_that_is_not_the_beacons_home_refuses_every_gatt_command() {
    // Decision #42: in a cluster only the home Hub talks to a Beacon over GATT. The Hub's view
    // names it home for another Beacon only.
    let t = TempDir::new("not-home");
    let publisher = Arc::new(control_ble::host::RecordingPublisher::default());
    let other = control_ble::mac::MacAddr::parse("11:22:33:44:55:66").expect("mac");
    let config = Arc::new(MemoryConfigStore::with_section(EyeSection {
        home: Some(std::collections::BTreeSet::from([other])),
        ..EyeSection::default()
    }));
    let host = Host {
        config,
        publisher: publisher.clone(),
        readings: Arc::new(NullReadingSink),
        ..Host::in_memory()
    };
    let dual = cfg_dual();
    const MAC: &str = "AA:BB:CC:DD:EE:FF";
    for argv in [
        vec!["beacon-provision", "--mac", MAC],
        vec!["beacon-send", "config", "--mac", MAC, "--tx-power-dbm", "4"],
        vec!["beacon-detect", "--mac", MAC],
        vec!["beacon-recording", "--mac", MAC, "--interval-min", "5"],
        vec!["beacon-history", "--mac", MAC, "--job", "job-7"],
    ] {
        let err = run_oneshot(&argv, &dual, &host, t.store())
            .await
            .expect_err("not home");
        assert_eq!(err.kind(), Kind::CapabilityDisabled, "{argv:?}");
        assert!(err.message().contains("not homed on this Hub"), "{argv:?}");
    }
    // The download asked of the wrong Hub closes at once, so the portal's job does not hang.
    let history = publisher.history();
    assert_eq!(history.len(), 1);
    let v = serde_json::to_value(&history[0]).expect("serializes");
    assert_eq!(v["job"], "job-7");
    assert_eq!(v["state"], "failed");
    assert_eq!(v["reason"], "not_home");
}
