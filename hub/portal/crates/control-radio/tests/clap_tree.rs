//! The clap-tree assertion — `plan/control/radio/plan.md` §Testing, `system/design.md` §10,
//! ledger #22.
//!
//! > a test walks the command tree and fails if any secret-bearing flag accepts a value on
//! > argv — the guarantee behind `--keys-file`/`--keys-stdin`.
//!
//! v1 put credentials in argv, which puts them in `ps`, in the journal, and in any shell
//! history that recorded the invocation. The guarantee is that key material enters through a
//! file path or stdin and nothing else — and that a future flag cannot quietly reopen the
//! hole, which is why this walks the tree rather than checking the two flags we know about.

use clap::{ColorChoice, CommandFactory, Parser};
use control_radio::cli::ControlRadioCli;
use control_radio::Kind;

/// Substrings that mark a flag as carrying secret material.
const SECRET_MARKERS: &[&str] = &[
    "key",
    "secret",
    "password",
    "passwd",
    "token",
    "credential",
    "appskey",
    "nwkskey",
    "devaddr",
    "pin",
];

/// Flags that legitimately contain a secret marker while carrying no secret.
///
/// Each entry is justified, because this list is the only way to weaken the check:
/// - `keys-file` takes a **path**; the secret stays in the file.
/// - `keys-stdin` is a boolean flag; the secret arrives on stdin.
/// - `join-eui`/`dev-eui`/`gateway-eui` are public identifiers, not secrets. They are matched
///   only because "key" is a substring of nothing here — they are listed defensively in case
///   a marker is broadened later.
const ALLOWED: &[&str] = &[
    "keys-file",
    "keys-stdin",
    "join-eui",
    "dev-eui",
    "gateway-eui",
];

#[test]
fn no_secret_bearing_flag_accepts_a_value_on_argv() {
    let mut offenders = Vec::new();
    walk(&ControlRadioCli::command(), &mut Vec::new(), &mut offenders);
    assert!(
        offenders.is_empty(),
        "key material must never reach argv (ledger #22); these flags take a value:\n{}",
        offenders.join("\n")
    );
}

/// Whether an argument consumes a value from argv.
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

        // The action is authoritative and available before clap builds the command;
        // `get_num_args()` is None until then, which would make this check pass vacuously.
        let takes_a_value = takes_a_value(arg);

        if looks_secret && takes_a_value && !allowed {
            offenders.push(format!("  {} --{}", path.join(" "), long));
        }
    }

    for sub in cmd.get_subcommands() {
        walk(sub, path, offenders);
    }
    path.pop();
}

/// The two sanctioned entry points exist and have the shape the rule assumes: one takes a
/// path, the other takes nothing. If either changed, the allowlist above would be hiding a
/// real hole.
#[test]
fn the_sanctioned_secret_inputs_have_the_right_shape() {
    let cmd = ControlRadioCli::command();
    let node_add = cmd
        .get_subcommands()
        .find(|c| c.get_name() == "node-add")
        .expect("node-add exists");

    let keys_file = node_add
        .get_arguments()
        .find(|a| a.get_long() == Some("keys-file"))
        .expect("--keys-file exists");
    assert!(takes_a_value(keys_file), "--keys-file must take a path");

    let keys_stdin = node_add
        .get_arguments()
        .find(|a| a.get_long() == Some("keys-stdin"))
        .expect("--keys-stdin exists");
    assert!(
        !takes_a_value(keys_stdin),
        "--keys-stdin must be a flag, or the secret lands in argv"
    );
}

/// The two are mutually exclusive: accepting both would leave which one wins undefined.
#[test]
fn the_two_secret_inputs_are_mutually_exclusive() {
    let err = ControlRadioCli::command()
        .color(ColorChoice::Never)
        .try_get_matches_from(vec![
            "control.radio",
            "node-add",
            "--profile",
            "default",
            "--serial",
            "s1",
            "--dev-eui",
            "0011223344556677",
            "--otaa",
            "--keys-file",
            "/tmp/k",
            "--keys-stdin",
        ])
        .expect_err("--keys-file and --keys-stdin must conflict");
    assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
}

/// §5: `--serial` and `--dev-eui` are mutually exclusive on the addressing commands, and one
/// of them is required — an unaddressed `node-remove` must not mean "all".
#[test]
fn addressing_requires_exactly_one_identifier() {
    for cmd in ["node-remove", "node-info"] {
        let both = ControlRadioCli::command()
            .color(ColorChoice::Never)
            .try_get_matches_from(vec![
                "control.radio",
                cmd,
                "--serial",
                "s1",
                "--dev-eui",
                "aa",
            ])
            .expect_err("both identifiers must conflict");
        assert_eq!(
            both.kind(),
            clap::error::ErrorKind::ArgumentConflict,
            "{cmd}"
        );

        let neither = ControlRadioCli::command()
            .color(ColorChoice::Never)
            .try_get_matches_from(vec!["control.radio", cmd])
            .expect_err("an identifier is required");
        assert_eq!(
            neither.kind(),
            clap::error::ErrorKind::MissingRequiredArgument,
            "{cmd}"
        );
    }
}

/// §5: `--otaa` and `--abp` are mutually exclusive. A device is one or the other.
#[test]
fn activation_modes_are_mutually_exclusive() {
    let err = ControlRadioCli::command()
        .color(ColorChoice::Never)
        .try_get_matches_from(vec![
            "control.radio",
            "node-add",
            "--profile",
            "default",
            "--otaa",
            "--abp",
        ])
        .expect_err("--otaa and --abp must conflict");
    assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
}

/// §5: `--profile` is the one flag required on every `node-add`, both transports.
#[test]
fn node_add_always_requires_a_profile() {
    let err = ControlRadioCli::command()
        .color(ColorChoice::Never)
        .try_get_matches_from(vec!["control.radio", "node-add", "--radio", "p2p"])
        .expect_err("--profile is required on both transports");
    assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
}

/// The published command set of §5, exactly. A command appearing or vanishing here is a
/// contract change, so it should show up as a diff in this list.
#[test]
fn the_published_command_set_is_the_documented_one() {
    let cmd = ControlRadioCli::command();
    let mut names: Vec<&str> = cmd.get_subcommands().map(|c| c.get_name()).collect();
    names.sort_unstable();

    let mut expected = vec![
        "create",
        "wipe",
        "clear",
        "gateway-add",
        "gateway-remove",
        "gateway-list",
        "gateway-info",
        "gateway-scan",
        "node-add",
        "node-remove",
        "node-move",
        "node-list",
        "node-get",
        "node-info",
        "node-send",
        "node-scan-start",
        "node-scan-stop",
        "radio-set",
        "northbridge-update",
        "status",
    ];
    expected.sort_unstable();
    assert_eq!(names, expected);
}

/// Clap's own consistency check over the built tree.
///
/// The walks above iterate `get_subcommands()` on an *unbuilt* `Command`, which never runs
/// `_build_self` — so a malformed `conflicts_with` naming something that does not exist is
/// invisible to them and to CI, while every debug-build invocation of that subcommand
/// panics. `debug_assert()` is the one call that builds the whole tree and validates it.
#[test]
fn the_whole_tree_passes_clap_s_own_debug_assertions() {
    ControlRadioCli::command().debug_assert();
}

/// §5: `node-send` takes **exactly one** of `--hex <payload>` or a `<type>` subcommand.
///
/// The constraint used to be a clap `conflicts_with = "what"` naming a subcommand, which clap
/// cannot resolve: debug builds aborted, release builds accepted both. It is now
/// `Command::validate`, so the assertion is on the refusal, not on which layer produces it.
#[test]
fn node_send_rejects_hex_together_with_a_type() {
    let cli = ControlRadioCli::try_parse_from(vec![
        "control.radio",
        "node-send",
        "--serial",
        "s1",
        "--hex",
        "0a0b0c",
        "--fport",
        "85",
        "get-config",
    ])
    .expect("--hex plus a <type> must parse, then fail validation");
    let err = cli
        .inner
        .command
        .validate()
        .expect_err("--hex and a <type> must conflict");
    assert_eq!(err.kind(), Kind::InvalidArgument);
    assert_eq!(err.exit_code(), 2);
}

/// The other half: neither form is also a usage error, not a silent no-op downlink.
#[test]
fn node_send_requires_one_of_the_two_forms() {
    let cli = ControlRadioCli::try_parse_from(vec!["control.radio", "node-send", "--serial", "s1"])
        .expect("bare node-send parses");
    let err = cli
        .inner
        .command
        .validate()
        .expect_err("node-send with neither --hex nor a <type> must be refused");
    assert_eq!(err.kind(), Kind::InvalidArgument);
}

/// …and either one alone is accepted, so the check is not a blanket refusal.
#[test]
fn node_send_accepts_either_form_alone() {
    for argv in [
        vec![
            "control.radio",
            "node-send",
            "--serial",
            "s1",
            "--hex",
            "0a",
            "--fport",
            "85",
        ],
        vec!["control.radio", "node-send", "--serial", "s1", "get-config"],
    ] {
        let cli = ControlRadioCli::try_parse_from(argv.clone())
            .unwrap_or_else(|e| panic!("{argv:?} must parse: {e}"));
        cli.inner
            .command
            .validate()
            .unwrap_or_else(|e| panic!("{argv:?} must validate: {e}"));
    }
}

/// §5 pins no fPort for the raw form, so `--hex` **requires** `--fport` rather than defaulting
/// to one. Every candidate default is wrong for some caller — 85 is the command engine's port
/// (§9.3), where a hand-written payload would be read as a `Command` protobuf, and 2/3 are
/// uplink-only — so the port is the caller's to state. Carried to the plan-feedback set.
#[test]
fn node_send_hex_requires_an_explicit_fport() {
    let err = ControlRadioCli::command()
        .color(ColorChoice::Never)
        .try_get_matches_from(vec![
            "control.radio",
            "node-send",
            "--serial",
            "s1",
            "--hex",
            "0a0b",
        ])
        .expect_err("--hex without --fport must be refused, not sent on a guessed port");
    assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);

    // The structured forms are fPort 85 by protocol (§9.3), so the flag is deliberately *not*
    // required for them: a port flag there would only offer a way to send a command somewhere
    // it cannot be understood.
    ControlRadioCli::try_parse_from(vec![
        "control.radio",
        "node-send",
        "--serial",
        "s1",
        "get-config",
    ])
    .expect("a structured form needs no --fport");
}

/// A config dump is paged and the node answers exactly the page asked for, so the page has to be
/// sayable. Absent it is 0, which is what every existing caller means.
#[test]
fn node_send_get_config_takes_a_page() {
    ControlRadioCli::try_parse_from(vec![
        "control.radio",
        "node-send",
        "--serial",
        "s1",
        "get-config",
        "--page",
        "3",
    ])
    .expect("--page must parse");

    ControlRadioCli::try_parse_from(vec![
        "control.radio",
        "node-send",
        "--serial",
        "s1",
        "get-config",
    ])
    .expect("and must stay optional");
}

/// The port is a `u8` — fPort is one byte on the wire, and clap is where a 300 stops.
#[test]
fn the_fport_flag_is_a_single_byte() {
    let err = ControlRadioCli::command()
        .color(ColorChoice::Never)
        .try_get_matches_from(vec![
            "control.radio",
            "node-send",
            "--serial",
            "s1",
            "--hex",
            "0a",
            "--fport",
            "300",
        ])
        .expect_err("fPort is one byte");
    assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
}
