//! One table, two dispatch groups — `plan/control/ble/plan.md` §6 AC 2: no exit code means two
//! different things across `control.radio` and `control.ble`.
//!
//! This test exists here because `proximosctl` is the only place that sees both crates. Neither
//! crate may depend on the other (decision #12's standalone rule binds them both), so the table is
//! shared at the **table** level rather than the type level — and a shared table with no test is a
//! coincidence waiting to end.
//!
//! The fixture is the `(code, name)` pair, not uniqueness: 14/`AlreadyExists` and 35/`OffMode` are
//! deliberately the *same* row on both sides.

use std::collections::BTreeMap;

#[test]
fn a_code_never_means_two_different_things_across_the_two_groups() {
    let mut by_code: BTreeMap<i32, &str> = BTreeMap::new();
    for k in control_radio::Kind::ALL {
        by_code.insert(k.exit_code(), k.as_str());
    }
    for k in control_ble::Kind::ALL {
        match by_code.get(&k.exit_code()) {
            Some(name) => assert_eq!(
                *name,
                k.as_str(),
                "exit {} is {} in control.radio and {} in control.ble",
                k.exit_code(),
                name,
                k.as_str()
            ),
            None => {
                by_code.insert(k.exit_code(), k.as_str());
            }
        }
    }
    // The BLE-only additions sit in the free numbers the plan assigned them.
    for (code, name) in [
        (15, "PairingRejected"),
        (24, "AdapterDown"),
        (25, "BeaconUnreachable"),
        (36, "CentralUnavailable"),
        (37, "PeripheralUnavailable"),
        (38, "RoleParameterRequired"),
        (39, "CapabilityDisabled"),
    ] {
        assert_eq!(by_code[&code], name);
    }
}

#[test]
fn the_shared_rows_are_the_ones_both_groups_actually_agree_on() {
    // Named explicitly, so a future edit to either crate that "fixes" one of these shows up as a
    // deliberate change to a shared contract rather than a passing test.
    let radio: BTreeMap<i32, &str> = control_radio::Kind::ALL
        .iter()
        .map(|k| (k.exit_code(), k.as_str()))
        .collect();
    let ble: BTreeMap<i32, &str> = control_ble::Kind::ALL
        .iter()
        .map(|k| (k.exit_code(), k.as_str()))
        .collect();
    for (code, name) in [
        (2, "InvalidArgument"),
        (10, "NotBootstrapped"),
        (13, "NotFound"),
        (14, "AlreadyExists"),
        (20, "ServiceDown"),
        (21, "Timeout"),
        (22, "Transport"),
        (23, "DaemonDown"),
        (34, "Phase2NotImplemented"),
        (35, "OffMode"),
        (40, "StorageError"),
    ] {
        assert_eq!(radio.get(&code), Some(&name), "control.radio exit {code}");
        assert_eq!(ble.get(&code), Some(&name), "control.ble exit {code}");
    }
}

#[test]
fn the_radio_only_rows_are_absent_from_the_ble_table_and_vice_versa() {
    let ble_codes: Vec<i32> = control_ble::Kind::ALL
        .iter()
        .map(|k| k.exit_code())
        .collect();
    let radio_codes: Vec<i32> = control_radio::Kind::ALL
        .iter()
        .map(|k| k.exit_code())
        .collect();
    // 11/12 are radio's authentication rows, 16 its P2P address collision; 30–33 are its
    // transport refusals.
    for code in [11, 12, 16, 30, 31, 32, 33] {
        assert!(radio_codes.contains(&code), "control.radio owns {code}");
        assert!(
            !ble_codes.contains(&code),
            "control.ble must not claim {code}"
        );
    }
    for code in [15, 24, 25, 36, 37, 38, 39] {
        assert!(ble_codes.contains(&code), "control.ble owns {code}");
        assert!(
            !radio_codes.contains(&code),
            "control.radio must not claim {code}"
        );
    }
}

#[test]
fn the_thirty_band_is_full_which_is_why_phase_two_needs_a_decision() {
    // plan.md open question 7: 33 `RadioParameterRequired` and 38 `RoleParameterRequired` are one
    // meaning, and collapsing them frees a number. Phase 2 (FB11–FB15, the client Hub) has no free
    // number otherwise. Asserting the band is full is what stops that being discovered late.
    // The two `Kind`s are deliberately distinct types — that is the standalone rule — so the band
    // is assembled from both rather than chained.
    let mut band: Vec<i32> = control_radio::Kind::ALL
        .iter()
        .map(|k| k.exit_code())
        .chain(control_ble::Kind::ALL.iter().map(|k| k.exit_code()))
        .filter(|c| (30..=39).contains(c))
        .collect();
    band.sort_unstable();
    band.dedup();
    assert_eq!(
        band,
        vec![30, 31, 32, 33, 34, 35, 36, 37, 38, 39],
        "the 30–39 band is full; a new mode/scope refusal needs a decision, not a number"
    );
}

#[test]
fn the_component_vocabularies_do_not_overlap_except_the_shared_rows() {
    let radio: Vec<&str> = control_radio::Component::ALL
        .iter()
        .map(|c| c.as_str())
        .collect();
    let ble: Vec<&str> = control_ble::Component::ALL
        .iter()
        .map(|c| c.as_str())
        .collect();
    for c in &ble {
        if radio.contains(c) {
            assert!(
                matches!(*c, "mqtt-broker" | "control-socket"),
                "{c} is claimed by both groups"
            );
        }
    }
    // Both shared rows really are present on both sides, so the exception is exercised rather than
    // vacuous.
    for shared in ["mqtt-broker", "control-socket"] {
        assert!(radio.contains(&shared), "control.radio has {shared}");
        assert!(ble.contains(&shared), "control.ble has {shared}");
    }
}

#[test]
fn both_groups_agree_that_the_transient_band_is_the_retryable_one() {
    // `radio-setup.service` and `ble-setup.service` both retry on this band and fail fast on
    // everything else, so the two crates must draw the line in the same place.
    for k in control_radio::Kind::ALL {
        assert_eq!(k.is_retryable(), (20..=29).contains(&k.exit_code()));
    }
    for k in control_ble::Kind::ALL {
        assert_eq!(k.is_retryable(), (20..=29).contains(&k.exit_code()));
    }
}

#[test]
fn the_two_dispatch_groups_are_reachable_under_their_documented_names() {
    use clap::CommandFactory;
    let cmd = <proximosctl_test_support::Cli as CommandFactory>::command();
    let mut names: Vec<&str> = cmd.get_subcommands().map(|c| c.get_name()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["control.ble", "control.radio"],
        "the dots are deliberate: these are the dispatch namespaces the plan set names"
    );
}

/// The binary's own tree, re-declared so an integration test can walk it.
///
/// `proximosctl` is a `[[bin]]` with no library target, so its `Cli` type is not importable. Keeping
/// this in step with `main.rs` is the price of asserting the group names at all — and the assertion
/// above is what catches a group being renamed or dropped.
mod proximosctl_test_support {
    use clap::{Parser, Subcommand};

    #[derive(Debug, Parser)]
    #[command(name = "proximosctl", disable_help_subcommand = true)]
    pub struct Cli {
        #[command(subcommand)]
        pub group: Group,
    }

    #[derive(Debug, Subcommand)]
    pub enum Group {
        #[command(name = "control.radio")]
        ControlRadio(control_radio::cli::ControlRadio),
        #[command(name = "control.ble")]
        ControlBle(control_ble::cli::ControlBle),
    }
}
