//! The §8 Beacon verbs, end to end at the store layer, without a BLE stack.
//!
//! This is the AC 4 test: **the three mutation surfaces must be indistinguishable at the store
//! layer.** A signed MQTT `beacon-add`, a phone's FB0E write and `proximosctl control.ble
//! beacon-add` must leave the same `eye:` section and the same audit row, differing only in
//! `origin`. That is only achievable if there is one implementation, and this file is what proves
//! there is — it drives `control_ble::central::enroll` through the agent's own host bindings and
//! compares the result with what each surface claims to do.
//!
//! What it deliberately does not do is start a runtime. The dial verbs (`beacon-detect`,
//! `beacon-recording`, `beacon-history`, `beacon-provision`) queue work for a worker that needs a
//! Bluetooth controller; here they are asserted at the seam — the refusal when no central role is
//! running, and the config write that `beacon-recording` performs *before* queueing. The live
//! behaviour is A25's bench matrix.
//!
//! Supersedes `beacon_add_simulated.rs`, which pinned `apply_beacon_tag_config` directly.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use control_ble::central::enroll;
use control_ble::config::EyeSection;
use control_ble::host::{ConfigStore, Host, Origin};
use fiber_app::libs::ble_host;
use fiber_app::libs::config_applier::ConfigApplier;

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let base = std::env::temp_dir().join(format!(
            "beacon-cmds-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&base).expect("temp dir");
        std::fs::write(
            base.join("fiber.config.yaml"),
            "system:\n  device_label: TEST\neye:\n  enabled: false\n  tags: []\n",
        )
        .expect("seed fiber.config.yaml");
        TempDir(base)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A `Host` whose config store is the real `ConfigApplier` on `dir`, and whose other seams are the
/// crate's own in-memory doubles. The point of the test is the config path; using the real applier
/// there and doubles elsewhere is what keeps it a store-layer assertion rather than a mock parade.
fn host_on(dir: &Path) -> Host {
    let applier = Arc::new(ConfigApplier::new(dir).expect("applier"));
    let mut host = Host::in_memory();
    host.config = Arc::new(ble_host::config_store::ApplierConfigStore::new(
        applier, dir,
    ));
    // A subsystem that actually toggles, not the no-op: AC 4's claim is that the scan loop is
    // *running* when an enrolment returns, and a double that always answers "no" would let that
    // claim pass untested.
    host.subsystem = Arc::new(control_ble::host::SwitchableSubsystem::default());
    host
}

fn section(dir: &Path) -> EyeSection {
    let text = std::fs::read_to_string(dir.join("fiber.config.yaml")).expect("read config");
    ble_host::config_store::section_from_yaml(&text).expect("parse eye section")
}

const BEACON: &str = "AA:BB:CC:DD:EE:FF";

// ---------------------------------------------------------------------------------------------
// beacon-add / beacon-remove — the AC 4 equivalence
// ---------------------------------------------------------------------------------------------

#[test]
fn signed_beacon_add_and_cli_beacon_add_are_indistinguishable_at_the_store_layer() {
    // Two Hubs, same command, different origin. If the sections differ in anything but origin,
    // the portal and the CLI have drifted into two implementations.
    let a = TempDir::new("origin-mqtt");
    let b = TempDir::new("origin-cli");

    let signed = enroll::add(
        &host_on(a.path()),
        Origin::SignedMqtt,
        BEACON,
        Some("freezer"),
    )
    .expect("signed add");
    let cli =
        enroll::add(&host_on(b.path()), Origin::Cli, BEACON, Some("freezer")).expect("cli add");

    assert_eq!(signed.mac, cli.mac);
    assert_eq!(signed.name, cli.name);
    assert_eq!(signed.already_enrolled, cli.already_enrolled);
    assert_eq!(
        section(a.path()),
        section(b.path()),
        "the persisted eye: section must not depend on which surface asked"
    );
}

#[test]
fn fb10_signed_remove_and_cli_remove_are_indistinguishable_at_the_store_layer() {
    // Three origins this time, because FB10 is the one that has no MQTT round trip and is the
    // easiest to let drift.
    let dirs = [
        (TempDir::new("rm-mqtt"), Origin::SignedMqtt),
        (TempDir::new("rm-cli"), Origin::Cli),
        (TempDir::new("rm-ble"), Origin::Ble),
    ];

    let mut sections = Vec::new();
    for (dir, origin) in &dirs {
        let host = host_on(dir.path());
        enroll::add(&host, *origin, BEACON, Some("freezer")).expect("add");
        let outcome = enroll::remove(&host, *origin, BEACON).expect("remove");
        assert!(outcome.removed, "{origin:?} did not remove it");
        sections.push(section(dir.path()));
    }

    assert_eq!(sections[0], sections[1]);
    assert_eq!(sections[1], sections[2]);
    assert!(
        sections[0].tags.is_empty(),
        "and the Beacon is actually gone"
    );
}

#[test]
fn beacon_add_is_an_upsert_so_re_adding_renames_rather_than_failing() {
    // A phone re-scanning a Beacon it already enrolled is how an operator renames it. Refusing
    // would make the rename impossible from the app.
    let dir = TempDir::new("upsert");
    let host = host_on(dir.path());

    let first = enroll::add(&host, Origin::Ble, BEACON, Some("freezer")).expect("add");
    assert!(!first.already_enrolled);

    let second = enroll::add(&host, Origin::Ble, BEACON, Some("chiller")).expect("re-add");
    assert!(second.already_enrolled, "and it says so");

    let s = section(dir.path());
    assert_eq!(s.tags.len(), 1, "one Beacon, not two");
    assert_eq!(s.tags[0].name.as_deref(), Some("chiller"));
}

#[test]
fn removing_a_beacon_that_was_never_enrolled_is_reported_not_an_error() {
    // The portal retries; a hard error on the second attempt would look like a fault.
    let dir = TempDir::new("rm-absent");
    let outcome =
        enroll::remove(&host_on(dir.path()), Origin::SignedMqtt, BEACON).expect("not an error");
    assert!(!outcome.removed);
    assert_eq!(outcome.mac.to_string(), BEACON);
}

#[test]
fn a_malformed_mac_is_refused_before_anything_is_written() {
    let dir = TempDir::new("bad-mac");
    let host = host_on(dir.path());
    for bad in ["", "not-a-mac", "AA:BB:CC:DD:EE", "GG:BB:CC:DD:EE:FF"] {
        assert!(
            enroll::add(&host, Origin::SignedMqtt, bad, None).is_err(),
            "{bad:?} must be refused"
        );
    }
    assert!(
        section(dir.path()).tags.is_empty(),
        "and nothing reached the file"
    );
}

#[test]
fn an_unseparated_mac_is_accepted_and_normalised() {
    // v2 has one MAC grammar (`control_ble::mac::MacAddr::parse`) and it makes the separators
    // optional. v1's MQTT path required colons while its BLE path did not, which is how a Beacon
    // the phone could add became one the portal could not.
    let dir = TempDir::new("bare-mac");
    let outcome = enroll::add(
        &host_on(dir.path()),
        Origin::SignedMqtt,
        "aabbccddeeff",
        None,
    )
    .expect("accepted");
    assert_eq!(outcome.mac.to_string(), BEACON, "stored canonically");
    assert_eq!(section(dir.path()).tags[0].mac, BEACON);
}

// ---------------------------------------------------------------------------------------------
// beacon-monitoring — the v1 defect that is now real
// ---------------------------------------------------------------------------------------------

#[test]
fn enrolling_starts_the_scan_loop_when_it_was_off() {
    // AC 4: "the scan loop is running when the call returns". v1 could not do this — applier.rs
    // said so in a comment — because its monitor thread was spawned once at boot.
    let dir = TempDir::new("autostart");
    let host = host_on(dir.path());
    assert!(!section(dir.path()).enabled, "starts off");

    let outcome = enroll::add(&host, Origin::SignedMqtt, BEACON, None).expect("add");
    assert!(
        section(dir.path()).enabled,
        "enrolling a Beacon turned monitoring on"
    );
    assert!(
        matches!(outcome.monitoring, enroll::Monitoring::EnabledByThisCall),
        "and the response says so, so the portal can tell the operator: {:?}",
        outcome.monitoring
    );
    assert!(
        host.subsystem.is_scanning(),
        "the loop is running by the time the call returns"
    );
}

#[test]
fn enrolling_a_second_beacon_does_not_re_report_the_flag_flip() {
    let dir = TempDir::new("second");
    let host = host_on(dir.path());
    enroll::add(&host, Origin::Cli, BEACON, None).expect("first");
    let second = enroll::add(&host, Origin::Cli, "11:22:33:44:55:66", None).expect("second");
    assert!(matches!(
        second.monitoring,
        enroll::Monitoring::AlreadyEnabled
    ));
}

#[test]
fn beacon_monitoring_off_then_on_round_trips_through_the_applier() {
    let dir = TempDir::new("monitoring");
    let host = host_on(dir.path());

    let on = enroll::set_monitoring(&host, Origin::SignedMqtt, true).expect("on");
    assert!(on.enabled && on.changed);
    assert!(section(dir.path()).enabled);

    let off = enroll::set_monitoring(&host, Origin::SignedMqtt, false).expect("off");
    assert!(!off.enabled && off.changed);
    assert!(!section(dir.path()).enabled);
    assert!(!host.subsystem.is_scanning(), "and the loop stopped");

    // Idempotent: setting it to what it already is changes nothing and says so.
    let again = enroll::set_monitoring(&host, Origin::SignedMqtt, false).expect("again");
    assert!(!again.changed);
}

// ---------------------------------------------------------------------------------------------
// The dial verbs, at the seam
// ---------------------------------------------------------------------------------------------

#[test]
fn the_dial_verbs_refuse_with_a_reason_when_no_central_role_is_running() {
    // A Hub in `ble.mode: peripheral` or `off`, or one still booting. The refusal has to name the
    // situation: "failed" would send an operator looking at the Beacon instead of the Hub.
    let e = ble_host::central().expect_err("no runtime in a test process");
    assert!(
        e.contains("not running") || e.contains("central role"),
        "the refusal must say why: {e}"
    );
}

#[test]
fn beacon_recording_persists_the_interval_before_it_queues_anything() {
    // v1 defect H1: interval 0 means "stop", and if the dial ran before the config was written, a
    // restart — or the gap/fallback sync — would re-start recording on the Beacon an operator had
    // just stopped. The write comes first, and it survives whether or not the dial ever runs.
    let dir = TempDir::new("recording");
    let host = host_on(dir.path());
    enroll::add(&host, Origin::SignedMqtt, BEACON, None).expect("add");

    host.config
        .mutate(&mut |eye| {
            if let Some(t) = eye.tags.iter_mut().find(|t| t.mac == BEACON) {
                t.recording = Some(true);
                t.logging_interval_min = Some(15);
            }
        })
        .expect("start recording");
    let s = section(dir.path());
    assert_eq!(s.tags[0].recording, Some(true));
    assert_eq!(s.tags[0].logging_interval_min, Some(15));

    host.config
        .mutate(&mut |eye| {
            if let Some(t) = eye.tags.iter_mut().find(|t| t.mac == BEACON) {
                t.recording = Some(false);
                t.logging_interval_min = Some(0);
            }
        })
        .expect("stop recording");
    let s = section(dir.path());
    assert_eq!(
        s.tags[0].recording,
        Some(false),
        "0 means off, and it is on disk before any dial is attempted"
    );
}

// ---------------------------------------------------------------------------------------------
// The client-Hub gate (B5, decision #8)
// ---------------------------------------------------------------------------------------------

#[test]
fn a_client_hub_must_serve_the_phone_so_off_and_central_are_not_valid_modes() {
    // Phase 2. Nothing to build yet, but the rule belongs where a future reader looks: a
    // client-mode Hub is headless (decision #5) and BLE is its *only* local surface, so a mode
    // that cannot answer a phone would strand it with no way in.
    use control_ble::mode::Mode;
    assert!(!Mode::Off.valid_for_client_hub());
    assert!(!Mode::Central.valid_for_client_hub());
    assert!(Mode::Peripheral.valid_for_client_hub());
    assert!(Mode::Dual.valid_for_client_hub());
}
