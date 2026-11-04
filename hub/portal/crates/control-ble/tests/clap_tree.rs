//! The clap-tree assertion — `plan/control/ble/plan.md` §Testing, `system/design.md` §10, ledger #22.
//!
//! The radio half's version of this walk exists because v1 put key material in argv. The BLE half
//! has a stronger guarantee to protect: **the FB01 credential is minted, never accepted.** There is
//! no `--pin` and no `--token` anywhere in this tree, and no sanctioned exception either — so
//! `ALLOWED` is empty, and any secret-shaped flag that takes a value is a bug.

use clap::{ColorChoice, CommandFactory, Parser};
use control_ble::cli::{Command, ControlBleCli};
use control_ble::Kind;

/// Substrings that mark a flag as carrying secret material.
const SECRET_MARKERS: &[&str] = &[
    "key",
    "secret",
    "password",
    "passwd",
    "token",
    "credential",
    "pin",
];

/// **Deliberately empty.** The radio half needs `--keys-file`/`--keys-stdin`; this tree has no
/// sanctioned secret-shaped flag at all, because the session token never travels inbound. An entry
/// appearing here is a design change, not a fix.
const ALLOWED: &[&str] = &[];

fn takes_a_value(arg: &clap::Arg) -> bool {
    matches!(
        arg.get_action(),
        clap::ArgAction::Set | clap::ArgAction::Append
    )
}

fn walk(cmd: &clap::Command, path: &mut Vec<String>, offenders: &mut Vec<String>) {
    path.push(cmd.get_name().to_string());
    for arg in cmd.get_arguments() {
        let id = arg.get_id().as_str().replace('_', "-");
        let long = arg.get_long().unwrap_or("").to_string();
        let looks_secret = SECRET_MARKERS
            .iter()
            .any(|m| id.contains(m) || long.contains(m));
        let allowed = ALLOWED.contains(&id.as_str()) || ALLOWED.contains(&long.as_str());
        if looks_secret && takes_a_value(arg) && !allowed {
            offenders.push(format!("  {} --{}", path.join(" "), long));
        }
    }
    for sub in cmd.get_subcommands() {
        walk(sub, path, offenders);
    }
    path.pop();
}

#[test]
fn no_secret_bearing_flag_accepts_a_value_on_argv() {
    let mut offenders = Vec::new();
    walk(&ControlBleCli::command(), &mut Vec::new(), &mut offenders);
    assert!(
        offenders.is_empty(),
        "no credential may reach argv (ledger #22); these flags take a value:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn there_is_no_pin_or_token_flag_anywhere() {
    // AC 5: the session token is minted, never accepted. Not "not on argv" — absent entirely, so a
    // future flag cannot quietly reopen the v1 `@main` hole through a different spelling.
    let mut found = Vec::new();
    fn scan(cmd: &clap::Command, path: &mut Vec<String>, found: &mut Vec<String>) {
        path.push(cmd.get_name().to_string());
        for arg in cmd.get_arguments() {
            let id = arg.get_id().as_str().replace('_', "-");
            let long = arg.get_long().unwrap_or("");
            for bad in ["pin", "token"] {
                if id.contains(bad) || long.contains(bad) {
                    found.push(format!("  {} --{long} (id {id})", path.join(" ")));
                }
            }
        }
        for sub in cmd.get_subcommands() {
            scan(sub, path, found);
        }
        path.pop();
    }
    scan(&ControlBleCli::command(), &mut Vec::new(), &mut found);
    assert!(
        found.is_empty(),
        "there is no --pin and no --token in this tree:\n{}",
        found.join("\n")
    );
}

/// Clap's own consistency check over the *built* tree — the walks above iterate an unbuilt
/// `Command`, so a malformed `conflicts_with` is invisible to them while every debug-build
/// invocation panics.
#[test]
fn the_whole_tree_passes_claps_own_debug_assertions() {
    ControlBleCli::command().debug_assert();
}

/// The published command set of §5, exactly — all 22. A command appearing or vanishing here is a
/// contract change, so it shows up as a diff in this list.
#[test]
fn the_published_command_set_is_the_documented_one() {
    let cmd = ControlBleCli::command();
    let mut names: Vec<&str> = cmd.get_subcommands().map(|c| c.get_name()).collect();
    names.sort_unstable();

    let mut expected = vec![
        "create",
        "status",
        "beacon-scan-start",
        "beacon-scan-stop",
        "beacon-scan-list",
        "beacon-list",
        "beacon-add",
        "beacon-remove",
        "beacon-info",
        "beacon-get",
        "beacon-provision",
        "beacon-send",
        "beacon-detect",
        "beacon-recording",
        "beacon-history",
        "beacon-monitoring",
        "known-beacons",
        "phone-session-start",
        "phone-session-stop",
        "phone-session-status",
        "phone-advertise-start",
        "phone-advertise-stop",
    ];
    expected.sort_unstable();
    assert_eq!(names, expected);
    assert_eq!(expected.len(), 22, "§5 publishes 22 commands");
}

/// Every peripheral command carries a `phone-` prefix and every central command a `beacon-`
/// prefix (§5); `create`/`status` are the two that genuinely exist on both roles.
#[test]
fn the_role_prefixes_are_consistent() {
    let cmd = ControlBleCli::command();
    for sub in cmd.get_subcommands() {
        let n = sub.get_name();
        if n == "create" || n == "status" || n == "known-beacons" {
            continue;
        }
        assert!(
            n.starts_with("beacon-") || n.starts_with("phone-"),
            "{n} carries neither role prefix"
        );
    }
}

#[test]
fn every_command_has_a_wire_name_matching_its_clap_name() {
    // The `command_type` on the signed-MQTT wire is this same string (§9.1's one rule, shared with
    // radio), so a drift between the clap name and `Command::name()` would split the vocabulary.
    for (argv, expected) in minimal_invocations() {
        let cli = ControlBleCli::try_parse_from(argv.clone())
            .unwrap_or_else(|e| panic!("{argv:?} must parse: {e}"));
        assert_eq!(cli.inner.command.name(), expected, "{argv:?}");
    }
}

/// One minimal, valid invocation per command, with the wire name it must report.
fn minimal_invocations() -> Vec<(Vec<&'static str>, &'static str)> {
    const MAC: &str = "AA:BB:CC:DD:EE:FF";
    vec![
        (vec!["control.ble", "create"], "create"),
        (vec!["control.ble", "status"], "status"),
        (
            vec!["control.ble", "beacon-scan-start"],
            "beacon-scan-start",
        ),
        (vec!["control.ble", "beacon-scan-stop"], "beacon-scan-stop"),
        (vec!["control.ble", "beacon-scan-list"], "beacon-scan-list"),
        (vec!["control.ble", "beacon-list"], "beacon-list"),
        (
            vec!["control.ble", "beacon-add", "--mac", MAC],
            "beacon-add",
        ),
        (
            vec!["control.ble", "beacon-remove", "--mac", MAC],
            "beacon-remove",
        ),
        (
            vec!["control.ble", "beacon-info", "--mac", MAC],
            "beacon-info",
        ),
        (
            vec!["control.ble", "beacon-get", "--mac", MAC, "reading"],
            "beacon-get",
        ),
        (
            vec!["control.ble", "beacon-provision", "--mac", MAC],
            "beacon-provision",
        ),
        (
            vec![
                "control.ble",
                "beacon-send",
                "config",
                "--mac",
                MAC,
                "--tx-power-dbm",
                "4",
            ],
            "beacon-send",
        ),
        (
            vec!["control.ble", "beacon-detect", "--mac", MAC],
            "beacon-detect",
        ),
        (
            vec![
                "control.ble",
                "beacon-recording",
                "--mac",
                MAC,
                "--interval-min",
                "5",
            ],
            "beacon-recording",
        ),
        (
            vec!["control.ble", "beacon-history", "--mac", MAC],
            "beacon-history",
        ),
        (
            vec!["control.ble", "beacon-monitoring", "--enabled", "true"],
            "beacon-monitoring",
        ),
        (
            vec!["control.ble", "known-beacons", "--macs", MAC],
            "known-beacons",
        ),
        (
            vec!["control.ble", "phone-session-start"],
            "phone-session-start",
        ),
        (
            vec!["control.ble", "phone-session-stop"],
            "phone-session-stop",
        ),
        (
            vec!["control.ble", "phone-session-status"],
            "phone-session-status",
        ),
        (
            vec!["control.ble", "phone-advertise-start"],
            "phone-advertise-start",
        ),
        (
            vec!["control.ble", "phone-advertise-stop"],
            "phone-advertise-stop",
        ),
    ]
}

#[test]
fn every_documented_command_has_a_minimal_invocation_covered() {
    // Keeps the table above honest: a new command must be added here too, or this fails.
    let mut covered: Vec<&str> = minimal_invocations().iter().map(|(_, n)| *n).collect();
    covered.sort_unstable();
    covered.dedup();
    let cmd = ControlBleCli::command();
    let mut names: Vec<&str> = cmd.get_subcommands().map(|c| c.get_name()).collect();
    names.sort_unstable();
    assert_eq!(covered, names);
}

#[test]
fn beacon_send_config_requires_at_least_one_value_flag() {
    let cli = ControlBleCli::try_parse_from(vec![
        "control.ble",
        "beacon-send",
        "config",
        "--mac",
        "AA:BB:CC:DD:EE:FF",
    ])
    .expect("parses, then fails validation");
    let err = cli
        .inner
        .command
        .validate()
        .expect_err("a partial override with nothing to write is a usage error");
    assert_eq!(err.kind(), Kind::InvalidArgument);
    assert_eq!(err.exit_code(), 2);

    // Any one of the three is enough.
    for flag in [
        vec!["--advertising-interval-s", "10"],
        vec!["--tx-power-dbm", "4"],
        vec!["--sensors", "temperature"],
    ] {
        let mut argv = vec![
            "control.ble",
            "beacon-send",
            "config",
            "--mac",
            "AA:BB:CC:DD:EE:FF",
        ];
        argv.extend(flag.iter().copied());
        let cli = ControlBleCli::try_parse_from(argv.clone())
            .unwrap_or_else(|e| panic!("{argv:?} must parse: {e}"));
        cli.inner
            .command
            .validate()
            .unwrap_or_else(|e| panic!("{argv:?} must validate: {e}"));
    }
}

#[test]
fn recording_interval_outside_the_set_is_refused() {
    // beacon-btsmp1.md §3: the beacon accepts {1, 5, 15} minutes, and 0 stops. Anything else is
    // InvalidArgument and is **never silently snapped** — an operator who asked for a five-minute
    // archive and got fifteen has a compliance problem and no way to know.
    for good in ["0", "1", "5", "15"] {
        let cli = ControlBleCli::try_parse_from(vec![
            "control.ble",
            "beacon-recording",
            "--mac",
            "AA:BB:CC:DD:EE:FF",
            "--interval-min",
            good,
        ])
        .expect("parses");
        cli.inner
            .command
            .validate()
            .unwrap_or_else(|e| panic!("{good} must be accepted: {e}"));
    }
    for bad in ["2", "7", "10", "30", "60"] {
        let cli = ControlBleCli::try_parse_from(vec![
            "control.ble",
            "beacon-recording",
            "--mac",
            "AA:BB:CC:DD:EE:FF",
            "--interval-min",
            bad,
        ])
        .expect("parses");
        let err = cli
            .inner
            .command
            .validate()
            .expect_err("an interval outside the set must be refused");
        assert_eq!(err.kind(), Kind::InvalidArgument, "{bad}");
        assert!(
            err.message().contains('1') && err.message().contains("15"),
            "the message must name the supported set: {}",
            err.message()
        );
    }
}

#[test]
fn sensors_must_be_a_subset_of_the_documented_four() {
    for bad in ["temperature,pressure", "co2", "temperature,"] {
        let cli = ControlBleCli::try_parse_from(vec![
            "control.ble",
            "beacon-send",
            "config",
            "--mac",
            "AA:BB:CC:DD:EE:FF",
            "--sensors",
            bad,
        ])
        .expect("parses");
        assert_eq!(
            cli.inner
                .command
                .validate()
                .expect_err(&format!("{bad} must be refused"))
                .kind(),
            Kind::InvalidArgument,
            "{bad}"
        );
    }
    let cli = ControlBleCli::try_parse_from(vec![
        "control.ble",
        "beacon-send",
        "config",
        "--mac",
        "AA:BB:CC:DD:EE:FF",
        "--sensors",
        "temperature,humidity,magnet,movement",
    ])
    .expect("parses");
    cli.inner.command.validate().expect("all four are valid");
}

#[test]
fn a_malformed_mac_is_refused_at_validation_with_exit_two() {
    let cli =
        ControlBleCli::try_parse_from(vec!["control.ble", "beacon-add", "--mac", "AA:BB:CC:DD:EE"])
            .expect("clap accepts any string; the shape is ours to check");
    let err = cli.inner.command.validate().expect_err("five octets");
    assert_eq!(err.kind(), Kind::InvalidArgument);
}

#[test]
fn an_empty_known_beacons_list_is_legal_and_means_listen_for_none() {
    let cli = ControlBleCli::try_parse_from(vec!["control.ble", "known-beacons", "--macs", ""])
        .expect("parses");
    cli.inner
        .command
        .validate()
        .expect("an empty allowlist is legal");
}

#[test]
fn the_mtu_floor_is_the_att_minimum() {
    for bad in ["0", "22"] {
        let cli = ControlBleCli::try_parse_from(vec!["control.ble", "beacon-list", "--mtu", bad])
            .expect("parses");
        assert_eq!(
            cli.inner
                .command
                .validate()
                .expect_err("below the ATT minimum")
                .kind(),
            Kind::InvalidArgument,
            "{bad}"
        );
    }
    let cli = ControlBleCli::try_parse_from(vec!["control.ble", "beacon-list", "--mtu", "23"])
        .expect("parses");
    cli.inner.command.validate().expect("23 is the floor");
}

#[test]
fn the_role_flag_exists_only_on_the_two_commands_that_span_both_roles() {
    let cmd = ControlBleCli::command();
    for sub in cmd.get_subcommands() {
        let has_role = sub.get_arguments().any(|a| a.get_long() == Some("role"));
        let expected = matches!(sub.get_name(), "create" | "status");
        assert_eq!(
            has_role,
            expected,
            "{}: --role must exist on create/status and nowhere else — the role is in every \
             other command's name",
            sub.get_name()
        );
    }
}

#[test]
fn the_role_arity_matches_the_command_name() {
    use control_ble::mode::RoleArity;
    use control_ble::Role;
    for (argv, expected) in minimal_invocations() {
        let cli = ControlBleCli::try_parse_from(argv.clone()).expect("parses");
        let arity = cli.inner.command.role_arity();
        let want = match expected {
            "create" | "status" => RoleArity::Both,
            n if n.starts_with("phone-") => RoleArity::Fixed(Role::Peripheral),
            _ => RoleArity::Fixed(Role::Central),
        };
        assert_eq!(arity, want, "{argv:?}");
    }
}

#[test]
fn the_mutating_set_is_the_documented_one() {
    // §5 "Command result envelope": every mutating command carries one. The read-only ones are the
    // four that only report.
    let read_only = [
        "status",
        "beacon-list",
        "beacon-info",
        "beacon-get",
        "beacon-scan-list",
        "known-beacons",
        "phone-session-status",
    ];
    for (argv, name) in minimal_invocations() {
        let cli = ControlBleCli::try_parse_from(argv.clone()).expect("parses");
        assert_eq!(
            cli.inner.command.is_mutating(),
            !read_only.contains(&name),
            "{name}"
        );
    }
}

#[test]
fn json_is_a_global_flag_so_it_works_before_or_after_the_subcommand() {
    for argv in [
        vec!["control.ble", "--json", "status"],
        vec!["control.ble", "status", "--json"],
    ] {
        let cli =
            ControlBleCli::try_parse_from(argv.clone()).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        assert!(cli.inner.json, "{argv:?}");
    }
}

#[test]
fn beacon_get_accepts_only_the_three_documented_types() {
    for good in ["reading", "axes", "history"] {
        ControlBleCli::try_parse_from(vec![
            "control.ble",
            "beacon-get",
            "--mac",
            "AA:BB:CC:DD:EE:FF",
            good,
        ])
        .unwrap_or_else(|e| panic!("{good}: {e}"));
    }
    let err = ControlBleCli::command()
        .color(ColorChoice::Never)
        .try_get_matches_from(vec![
            "control.ble",
            "beacon-get",
            "--mac",
            "AA:BB:CC:DD:EE:FF",
            "archive",
        ])
        .expect_err("an unknown type is a usage error");
    assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
}

#[test]
fn beacon_send_requires_its_config_subcommand() {
    let err = ControlBleCli::command()
        .color(ColorChoice::Never)
        .try_get_matches_from(vec!["control.ble", "beacon-send"])
        .expect_err("beacon-send alone is incomplete");
    // Clap answers a missing required subcommand with its help rather than a bare error kind; the
    // contract is that the invocation is refused and that the refusal names the only valid form.
    assert!(
        matches!(
            err.kind(),
            clap::error::ErrorKind::MissingSubcommand
                | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        ),
        "unexpected kind {:?}",
        err.kind()
    );
    assert!(
        err.to_string().contains("config"),
        "the refusal must name `config`, the only form: {err}"
    );
}

#[test]
fn every_minimal_invocation_validates() {
    // Nothing in the happy-path table may be refused by `validate()`; if one is, either the table
    // or the validation is wrong.
    for (argv, name) in minimal_invocations() {
        let cli = ControlBleCli::try_parse_from(argv.clone()).expect("parses");
        cli.inner
            .command
            .validate()
            .unwrap_or_else(|e| panic!("{name} ({argv:?}) must validate: {e}"));
    }
}

#[test]
fn the_command_enum_is_exhaustively_named() {
    // Every variant reports a kebab-case name with no underscores — the wire spelling rule.
    for (argv, _) in minimal_invocations() {
        let cli = ControlBleCli::try_parse_from(argv).expect("parses");
        let n = cli.inner.command.name();
        assert!(!n.contains('_'), "{n} must be kebab-case");
        assert_eq!(n, n.to_lowercase(), "{n} must be lower case");
        assert!(
            !n.contains("eye"),
            "{n}: the v1 `*_eye_*` vocabulary exists only in plan.md §9.8's historical map"
        );
    }
    // And the type is reachable as a re-export, so `proximosctl` can name it.
    fn _assert_type(_: Option<Command>) {}
}

#[test]
fn beacon_history_takes_an_optional_window_and_job() {
    // beacon_history.md §2.1: `from`, `to` and `job` are all optional — a bare `--mac` resumes
    // after the newest stored record, as before.
    let ok: [&[&str]; 3] = [
        &[],
        &[
            "--from",
            "1700000000",
            "--to",
            "1700086400",
            "--job",
            "bh-1",
        ],
        &["--to", "1700086400"],
    ];
    for extra in ok {
        let mut argv = vec![
            "control.ble",
            "beacon-history",
            "--mac",
            "AA:BB:CC:DD:EE:FF",
        ];
        argv.extend_from_slice(extra);
        let cli = ControlBleCli::try_parse_from(argv.clone())
            .unwrap_or_else(|e| panic!("{argv:?} must parse: {e}"));
        cli.inner
            .command
            .validate()
            .unwrap_or_else(|e| panic!("{argv:?} must validate: {e}"));
    }
    let bad: [&[&str]; 3] = [
        &["--from", "200", "--to", "100"],
        &["--from", "-1"],
        &["--job", "has space"],
    ];
    for extra in bad {
        let mut argv = vec![
            "control.ble",
            "beacon-history",
            "--mac",
            "AA:BB:CC:DD:EE:FF",
        ];
        argv.extend_from_slice(extra);
        let cli = ControlBleCli::try_parse_from(argv.clone())
            .unwrap_or_else(|e| panic!("{argv:?} must parse: {e}"));
        let err = cli
            .inner
            .command
            .validate()
            .expect_err("a reversed or out-of-range window, or a malformed job, is refused");
        assert_eq!(err.kind(), Kind::InvalidArgument, "{argv:?}");
        assert_eq!(err.exit_code(), 2, "{argv:?}");
    }
}
