//! Deep standby across a boot — the scenarios that motivated the feature,
//! exercised through the public API against a real temp directory.
//!
//! `systemctl poweroff` used to halt the SoC while the battery held the rails
//! up, and nothing on this board can wake a halted CM4. Reconnecting PoE did
//! nothing. Standby keeps the agent alive watching VIN instead, and the marker
//! written here is what carries the operator's intent across a boot the agent
//! did not choose — a panic under `Restart=on-failure`, or the battery giving out
//! entirely.
//!
//! The unit tests in `libs::power::standby` cover the pieces. These cover the
//! decisions as a person would describe them, so a regression reads as a broken
//! promise rather than a broken assertion.

use std::fs;

use fiber_app::libs::power::standby::{
    boot_decision, marker_dir, ArmEvidence, ResumeWatch, StandbyMarker,
};
use fiber_app::libs::power::BootDecision;
use tempfile::TempDir;

/// What `fiber.config.yaml` ships as `power.ac_power.dc_connect_mv`.
///
/// Deliberately not 12000: see `a_correctly_powered_device_is_never_stranded_by_the_threshold`.
const CONNECT_MV: u16 = fiber_app::libs::power::status::DEFAULT_DC_CONNECT_MV;

fn marker_for(reason: &str, signer: &str) -> StandbyMarker {
    StandbyMarker::new(reason.to_string(), signer.to_string())
}

#[test]
fn a_poe_cable_reconnected_while_the_device_slept_brings_it_back() {
    // The whole point of the feature. The device was switched off, ran down on
    // battery, and someone plugged the network cable back in.
    let dir = TempDir::new().unwrap();
    marker_for("ward closed for the weekend", "dr.jane@hospital.eu")
        .write(dir.path())
        .unwrap();

    let present = StandbyMarker::read(dir.path()).is_some();
    assert_eq!(
        boot_decision(present, Some(13_200), CONNECT_MV),
        BootDecision::Awake,
        "PoE is back, so the device must come up monitoring"
    );
}

#[test]
fn a_crash_restart_on_battery_does_not_silently_resume_monitoring() {
    // fiber.service is Restart=on-failure with RestartSec=10. Without the marker
    // a panic in standby would put the device back to work inside ten seconds,
    // on a unit the operator believes is off and is not watching.
    let dir = TempDir::new().unwrap();
    marker_for("decommissioned", "dr.jane@hospital.eu")
        .write(dir.path())
        .unwrap();

    let present = StandbyMarker::read(dir.path()).is_some();
    assert_eq!(
        boot_decision(present, Some(0), CONNECT_MV),
        BootDecision::ReenterStandby
    );
}

#[test]
fn a_battery_that_gave_out_cold_boots_normally_when_poe_returns() {
    // Second scenario: standby drained the pack, every rail dropped, and the CM4
    // cold-booted when PoE came back. The marker is stale — DC power has to win,
    // or a mains-powered device would put itself straight back to sleep.
    let dir = TempDir::new().unwrap();
    marker_for("overnight", "dr.jane@hospital.eu")
        .write(dir.path())
        .unwrap();

    assert_eq!(
        boot_decision(true, Some(12_500), CONNECT_MV),
        BootDecision::Awake
    );
}

#[test]
fn an_unreadable_marker_leaves_the_device_monitoring() {
    // A device that cannot read its own marker must still boot and monitor
    // patients. Failing towards monitoring is the only defensible bias.
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("standby.json"), b"\x00\x01 not json").unwrap();

    assert!(
        StandbyMarker::read(dir.path()).is_none(),
        "corrupt must read as absent, not panic"
    );
    assert_eq!(
        boot_decision(false, Some(0), CONNECT_MV),
        BootDecision::Awake,
        "even with no power, an unmarked device boots monitoring"
    );
}

#[test]
fn a_boot_with_no_vin_reading_stays_off_rather_than_guessing() {
    // The ADC read at boot can fail. Coming up in standby when we cannot prove
    // power is present keeps the device visibly off instead of quietly resuming
    // clinical measurement; the next successful read wakes it seconds later.
    let dir = TempDir::new().unwrap();
    marker_for("r", "dr.jane@hospital.eu").write(dir.path()).unwrap();

    assert_eq!(
        boot_decision(true, None, CONNECT_MV),
        BootDecision::ReenterStandby
    );
}

#[test]
fn who_authorised_the_gap_survives_the_boot() {
    // The audit row is in the encrypted, hash-chained log, but the marker is what
    // lets the resumed boot name the same operator and entry time on MQTT and in
    // `fiberctl power`.
    let dir = TempDir::new().unwrap();
    let written = marker_for("moved to storage", "dr.jane@hospital.eu");
    written.write(dir.path()).unwrap();

    let read_back = StandbyMarker::read(dir.path()).expect("marker must survive");
    assert_eq!(read_back.requested_by, "dr.jane@hospital.eu");
    assert_eq!(read_back.reason, "moved to storage");
    assert_eq!(read_back, written);
    assert!(read_back.entered_at_rfc3339().contains('T'));
}

#[test]
fn clearing_on_a_normal_boot_does_not_leave_the_device_stuck_off() {
    let dir = TempDir::new().unwrap();
    marker_for("r", "b").write(dir.path()).unwrap();

    // What main() does on the Awake branch when a stale marker is present.
    StandbyMarker::clear(dir.path());

    assert!(StandbyMarker::read(dir.path()).is_none());
    assert_eq!(
        boot_decision(false, Some(0), CONNECT_MV),
        BootDecision::Awake,
        "the next boot must not find the marker again"
    );
}

#[test]
fn the_marker_lands_on_the_same_partition_as_the_medical_database() {
    // PrivateTmp=true gives the unit a private tmpfs that does not survive a
    // restart, which is the very event the marker exists to survive.
    assert_eq!(
        marker_dir("/data/fiber/fiber_medical.db"),
        std::path::PathBuf::from("/data/fiber")
    );
}

#[test]
fn switching_off_a_device_that_still_has_poe_leaves_it_off() {
    // "Newly detected" has to mean an edge. A level check would resume on the
    // very next poll and the power-off would look broken.
    let mut watch = ResumeWatch::new(2, true);
    for _ in 0..30 {
        assert!(
            !watch.observe(true, false),
            "DC never went away and the cable was never disturbed"
        );
    }

    // Unplug, then plug back in: now it is a genuine arrival.
    assert!(!watch.observe(false, false));
    assert!(!watch.observe(true, false), "still debouncing");
    assert!(watch.observe(true, false));
}

#[test]
fn a_replug_faster_than_the_poll_interval_still_wakes_the_device() {
    // The reported field failure, as a narrative. The device was switched off
    // while on PoE, then the cable was pulled and pushed back in within a single
    // poll interval — so every VIN sample read "present" and the absence was
    // never observed. Before the carrier counter, the device stayed dark and only
    // an unplug longer than the interval would wake it.
    let mut watch = ResumeWatch::new(2, true);

    // Poll before anything was touched.
    assert!(!watch.observe(true, false));
    assert!(!watch.is_armed());

    // The unplug and replug both happened between this poll and the last, so VIN
    // still reads present. Only the kernel's monotonic carrier count remembers.
    assert!(!watch.observe(true, true), "armed by the link, now confirming");
    assert_eq!(watch.evidence(), Some(ArmEvidence::LinkDown));
    assert!(
        watch.observe(true, true),
        "the device must come back without the operator timing the unplug"
    );
}

#[test]
fn a_link_bounce_cannot_wake_a_device_that_has_no_power() {
    // Carrier evidence only arms; the resume still needs power actually present.
    // Otherwise a switch reboot would wake a device running on battery.
    let mut watch = ResumeWatch::new(2, true);
    for _ in 0..30 {
        assert!(!watch.observe(false, true), "no power, so no resume");
    }
}

#[test]
fn a_flapping_cable_does_not_thrash_the_device_awake() {
    // Each resume raises the sensor rails, repaints the panel, writes an audit
    // row and publishes; the entry path costs a database flush. Neither belongs
    // on a loop driven at the poll rate by a loose connector.
    let mut watch = ResumeWatch::new(2, false);
    for _ in 0..30 {
        assert!(!watch.observe(true, false));
        assert!(!watch.observe(false, false));
    }
    // A steady connection still gets through.
    assert!(!watch.observe(true, false));
    assert!(watch.observe(true, false));
}

#[test]
fn a_correctly_powered_device_is_never_stranded_by_the_threshold() {
    // The latent defect found while fixing the above. The southbridge reports VIN
    // through two integer truncations, so a nominal 12 V supply can report just
    // under 12000 mV — and the first version used 12000 as the wake and boot
    // threshold. Such a unit could never resume, and re-entered standby on every
    // boot while sitting on mains.
    for reported_mv in [11_700u16, 11_900, 11_998, 12_004, 12_098] {
        assert_eq!(
            boot_decision(true, Some(reported_mv), CONNECT_MV),
            BootDecision::Awake,
            "{reported_mv} mV is a powered device and must boot awake"
        );
    }
}
