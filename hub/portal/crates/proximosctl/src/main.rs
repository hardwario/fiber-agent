//! `proximosctl` — the ProXimos control CLI.
//!
//! Five subcommands today: the two dispatch groups `control.radio` and `control.ble`,
//! `render`, the configuration renderer of `plan/system/design.md` §7, `first-run-apply`,
//! which writes the portal's first-run transport choice into the overlay (decision #37), and
//! `cluster`, a Server Hub's Client accounts (`plan/control/cluster.md` §7) — the first group
//! that is a client of the agent's control socket rather than a library call. As the monolith lands
//! (`plan/portal/monolith.md` §11) it gains the control-socket client for the rest of the
//! agent's surface; the socket is `/run/proximos/control.sock`, authorized by filesystem
//! permissions — the third of the three mutation dialects, and the reason this is a separate
//! binary rather than a subcommand of the service.
//!
//! Both dispatch groups share one exit-code table at the **table** level rather than the type
//! level, because decision #12's standalone rule binds both crates and neither may depend on the
//! other. This binary is the only place that sees both, so `tests/shared_exit_table.rs` lives here.
//!
//! `render` shares nothing with either and that is deliberate: it runs from a boot oneshot as
//! root before any consumer exists, while the dispatch groups run afterwards as `proximos` and
//! talk to a network server or the BLE stack. It shares this binary because §7 says the renderer
//! is "a subcommand of the in-tree Rust CLI", and because shipping a second 10 MB static binary
//! to run once per boot would be the wrong trade.

use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use control_ble::cli::ControlBle;
use control_radio::cli::ControlRadio;

mod cluster;

/// A plain `fn main()`, deliberately **not** `#[tokio::main]`.
///
/// `security/landlock.md` §4 is binding: `landlock_restrict_self` is per-thread, so the
/// ruleset must be applied while the process is still single-threaded. `#[tokio::main]`
/// expands to a runtime built before the first line of the async body runs, which would
/// leave every worker thread unconfined — silently, with every functional test still
/// passing. So confinement goes first and the runtime is built by hand afterwards.
///
/// Phase 1 leaves the call site empty (CP-3 is pending spike S3), but the shape is
/// load-bearing: filling it in must not require restructuring `main`.
fn main() -> ExitCode {
    confine();

    let cli = Cli::parse();

    // Each dispatch group renders its own result and its own errors, but the *shape* of both is
    // the same: the §6 envelope under `--json`, one stderr line otherwise, and the exit code from
    // the shared table either way. `render` is outside that contract — see its arm.
    match &cli.group {
        // A SYNC PATH, before and without the tokio runtime, and not merely because it
        // happens not to need one. `render` runs from `proximos-render.service`, a
        // `Type=oneshot` at the very top of the boot graph, and it calls `getgrnam(3)` and
        // `getpwnam(3)` — which are not thread-safe. Doing this work on the one thread the
        // process starts with is what makes that safe, and it keeps the renderer entirely
        // outside the async machinery the dispatch groups need.
        Group::Render => render_group(),
        Group::FirstRunApply => first_run_apply(),
        // The agent does the work; this is its control-socket client, sync and runtime-free.
        Group::Cluster(c) => cluster::run(c),
        Group::ControlRadio(radio) => match run_radio(radio) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => report(
                radio.json,
                serde_json::to_value(err.detail()).ok(),
                &err.to_string(),
                err.exit_code(),
            ),
        },
        Group::ControlBle(ble) => match run_ble(ble) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => report(
                ble.json,
                serde_json::to_value(err.detail()).ok(),
                &err.to_string(),
                err.exit_code(),
            ),
        },
    }
}

/// Apply the Landlock ruleset. Empty in phase 1 — see the note on [`main`].
///
/// # What `render` needs from it when CP-3 lands
///
/// Today this is a no-op, so `render` is unaffected. It will not stay that way, and the
/// interaction is worth writing down while it is still cheap to change.
///
/// `confine()` is called **before** `Cli::parse()`, so a filled-in ruleset here is the
/// union of what every subcommand needs — and `render`'s needs are the widest in this
/// binary by a wide margin: `read` on `/usr/share/proximos/templates/`, and
/// `read`/`write`/`create`/`remove` on both `/run/proximos/` (four service
/// subdirectories) and `/data/portal/` (it creates `db.key`). Granting all of that
/// unconditionally would hand `proximosctl control.radio node-add` — the command an
/// operator runs routinely — write access to every rendered config on the device, which is
/// the opposite of what §5.1's per-path table is for.
///
/// **Recommendation, not implemented here** (it is `security/`'s call, and CP-3 is pending
/// spike S3): move the call to `confine(&cli.group)` *after* `Cli::parse()` and narrow per
/// group. That keeps `landlock.md` §4's actual invariant — the ruleset is applied while
/// the process is still single-threaded — because `clap` parsing spawns no threads; the
/// property being protected is single-threadedness, not "the first statement in `main`".
/// The alternative, a separate `proximos-render` binary with its own ruleset, is the
/// cleaner sandbox and the worse trade: it contradicts design.md §7's "a subcommand of the
/// in-tree Rust CLI" and adds a second static binary to the image to run once per boot.
fn confine() {}

/// One failure path for both dispatch groups.
///
/// Under `--json` the envelope goes to **stdout**, so `proximosctl … --json | jq` sees exactly one
/// document whether the command succeeded or failed. Plain mode is one line on stderr.
fn report(as_json: bool, detail: Option<serde_json::Value>, display: &str, code: i32) -> ExitCode {
    if as_json {
        match detail.as_ref().map(serde_json::to_string_pretty) {
            Some(Ok(s)) => println!("{s}"),
            _ => eprintln!("proximosctl: could not render the error as JSON"),
        }
    } else {
        eprintln!("proximosctl: {display}");
    }
    // The exit code is the contract the provisioning wizard and fleet updater read over SSH, so it
    // comes from the taxonomy, never from an ad-hoc number.
    exit_code(code)
}

/// The CLI's own tree. Two dispatch groups and the renderer today; the control-socket
/// subcommands for the rest of the agent's surface join them as the monolith lands.
#[derive(Debug, Parser)]
#[command(
    name = "proximosctl",
    about = "ProXimos device control",
    // The image's release version on a release build, as /health and the BLE device info
    // report it (plan/system/design.md §9.1); control-radio owns the constant because this
    // CLI is its front end.
    version = control_radio::VERSION,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    group: Group,
}

#[derive(Debug, Subcommand)]
enum Group {
    /// Radio control: LoRaWAN and LoRa P2P node and gateway management.
    ///
    /// The dot is deliberate — `control.radio` is the dispatch namespace the plan set,
    /// the MQTT surface and the portal all name, and the CLI spells it the same way.
    #[command(name = "control.radio")]
    ControlRadio(ControlRadio),

    /// BLE control: Beacon telemetry and provisioning, and the phone-facing FB00 service.
    ///
    /// The dot is deliberate — `control.ble` is the dispatch namespace the plan set names.
    #[command(name = "control.ble")]
    ControlBle(ControlBle),

    /// Render the device's runtime configuration into /run/proximos/.
    ///
    /// No dot: this is not a dispatch group. It is one verb, run by
    /// proximos-render.service before every consumer on the device, and it never starts,
    /// stops or reloads a unit.
    ///
    /// Deliberately flagless. Every input is a path, every path has a compiled-in default,
    /// and the four environment variables that override them (PROXIMOS_TEMPLATES,
    /// PROXIMOS_DATA_ROOT, PROXIMOS_RUN_ROOT, PROXIMOS_CONFIG) are the dev-loop and test
    /// seam — proximos-render.service's ExecStart= is `/usr/bin/proximosctl render` and
    /// nothing else, so there is no argv for a path or a secret to hide in.
    Render,

    /// Apply the portal's first-run transport choice to the device overlay (decision #37).
    ///
    /// Run as root by proximos-first-run.service when /data/portal/requested-radio-mode
    /// appears. Writes radio.mode into /data/proximos/config/proximos.config.yaml only while
    /// that file sets none, then removes the request. Exit 0: nothing to do, refused, or
    /// applied with no reboot needed. Exit 3: applied, and the running services were
    /// rendered with another mode, so the unit reboots. Exit 1: failed; the request stays.
    /// Flagless for the same reason `render` is.
    #[command(name = "first-run-apply")]
    FirstRunApply,

    /// A Server Hub's Client Hub accounts: mint, rotate or remove peer-<hostname> on this Hub's
    /// broker and register the Client's gateway (plan/control/cluster.md §7).
    ///
    /// No dot: not a dispatch group of a control crate. The agent does the work, over its
    /// root-only control socket, on the same path as the signed cluster-client-add/-remove.
    Cluster(cluster::Cluster),
}

/// `first-run-apply`'s "applied, reboot into it" exit code. Not from control-radio's
/// taxonomy, for the reason `render_group` gives. The unit's ExecStart= tests for exactly
/// this number, and `tests/first_run_cli.rs` holds the two together.
const FIRST_RUN_REBOOT_EXIT: u8 = 3;

/// Answer the portal's first-run request, one journal line per outcome.
fn first_run_apply() -> ExitCode {
    use proximos_render::first_run::{self, Outcome};

    let paths = proximos_render::RenderPaths::from_env();
    match first_run::apply(&paths) {
        Ok(Outcome::NoRequest) => {
            println!("proximosctl first-run-apply: no request");
            ExitCode::SUCCESS
        }
        Ok(Outcome::Applied { mode, reboot }) => {
            println!(
                "proximosctl first-run-apply: wrote radio.mode: {} to {}",
                mode.as_str(),
                first_run::overlay_path(&paths).display()
            );
            if reboot {
                println!(
                    "proximosctl first-run-apply: the running services use another mode; reboot"
                );
                ExitCode::from(FIRST_RUN_REBOOT_EXIT)
            } else {
                ExitCode::SUCCESS
            }
        }
        Ok(Outcome::Refused { requested, reason }) => {
            // A refusal is an answer, not a failure: exit 0, so the .path unit does not
            // count it towards its trigger limit, and the reason is in the journal.
            eprintln!("proximosctl first-run-apply: refused {requested:?}: {reason}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("proximosctl first-run-apply: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Render, then say what happened, in the shape a `journalctl -u proximos-render` reader
/// needs: one line per file, with skips on stderr so they survive a `StandardOutput=null`.
fn render_group() -> ExitCode {
    let paths = proximos_render::RenderPaths::from_env();
    let default_mode = render_default_mode(control_radio::setmode::default_mode(
        control_radio::setmode::probe_hardware(),
    ));
    match proximos_render::render_all(&paths, default_mode) {
        Ok(report) => {
            println!(
                "proximosctl render: hardware default radio.mode {}",
                default_mode.as_str()
            );
            for p in &report.written {
                println!("proximosctl render: wrote {}", p.display());
            }
            for p in &report.removed {
                println!("proximosctl render: removed stale {}", p.display());
            }
            // Skips are not failures — a Hub in `p2p` mode skipping ChirpStack is the
            // design — but they are the first thing anyone looks for when a unit
            // condition-skips, so they go to stderr with the reason attached.
            for s in &report.skipped {
                eprintln!(
                    "proximosctl render: did not write {}: {}",
                    s.path.display(),
                    s.reason
                );
            }
            // Written, but short of a section its inputs asked for — a client-role Hub
            // whose /data/proximos/cluster/ payload is incomplete (cluster.md §4).
            for s in &report.degraded {
                eprintln!(
                    "proximosctl render: wrote {} incompletely: {}",
                    s.path.display(),
                    s.reason
                );
            }
            if let Some(proximos_render::Ownership::Skipped(why)) = &report.ownership {
                eprintln!("proximosctl render: {why}");
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("proximosctl render: {err}");
            // Exit 1, not a code from control-radio's taxonomy. That table is the contract
            // the provisioning wizard and the fleet updater read for `control.radio`
            // commands, and borrowing a number from it here would imply a meaning this
            // command does not have. The renderer's consumer is systemd, which cares only
            // about zero and non-zero.
            ExitCode::FAILURE
        }
    }
}

/// control-radio's mode as the renderer spells it. The two enums are separate on purpose (the
/// renderer does not depend on control-radio, `proximos_render::Mode`); this binary depends on
/// both and is the one place they meet.
fn render_default_mode(mode: control_radio::Mode) -> proximos_render::Mode {
    match mode {
        control_radio::Mode::Off => proximos_render::Mode::Off,
        control_radio::Mode::Lorawan => proximos_render::Mode::Lorawan,
        control_radio::Mode::P2p => proximos_render::Mode::P2p,
        control_radio::Mode::Dual => proximos_render::Mode::Dual,
    }
}

/// A plain runtime, built by hand after confinement — never `#[tokio::main]`, per the note on
/// `main`. Two worker threads: this is a one-shot CLI, not a server.
fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
}

fn run_radio(cli: &ControlRadio) -> control_radio::Result<()> {
    let rt = runtime().map_err(|e| {
        control_radio::Error::new(
            control_radio::Kind::Transport,
            None,
            control_radio::Component::ControlSocket,
            format!("cannot start the async runtime: {e}"),
        )
    })?;

    rt.block_on(async {
        let config = control_radio::dispatch::load_config()?;

        // Connect only when this invocation actually touches ChirpStack. A P2P-only Hub runs
        // none, and a command that never reaches the LoRaWAN arm must not wait on it.
        let client = if control_radio::dispatch::needs_chirpstack(cli, &config) {
            let tokens = control_radio::lorawan::Tokens::new(control_radio::store::Store::new(
                control_radio::dispatch::store_root(),
            ));
            // `create` runs at boot and right after a mode switch, beside a ChirpStack that is
            // still starting: it waits for the API rather than failing into a unit restart.
            let connected = if matches!(cli.command, control_radio::cli::Command::Create(_)) {
                control_radio::lorawan::tonic_client::TonicChirpStack::connect_waiting(
                    tokens,
                    std::time::Duration::from_secs(60),
                )
                .await
            } else {
                control_radio::lorawan::tonic_client::TonicChirpStack::connect(tokens).await
            };
            match connected {
                Ok(c) => Some(c),
                // `status` must work when things are broken — that is when it is used. A
                // refused connection is reported inside the report (`api_status`), never as
                // exit 20 from the one command whose job is to say what is wrong.
                Err(_) if matches!(cli.command, control_radio::cli::Command::Status(_)) => None,
                Err(e) => return Err(e),
            }
        } else {
            None
        };
        let broker = control_radio::mqtt::RumqttcRetainedTopics::new();
        // Constructing the publisher opens nothing — the connection is made inside the one
        // command that publishes — so a read-only invocation pays nothing for it.
        let publisher = control_radio::mqtt::RumqttcPublisher::new();
        let outcome = control_radio::dispatch::run(
            cli,
            &config,
            client
                .as_ref()
                .map(|c| c as &dyn control_radio::lorawan::client::ChirpStack),
            Some(&broker),
            Some(&publisher),
        )
        .await?;
        render_radio(&outcome, cli.json)
    })
}

fn run_ble(cli: &ControlBle) -> control_ble::Result<()> {
    let rt = runtime().map_err(|e| {
        control_ble::Error::new(
            control_ble::Kind::Transport,
            None,
            control_ble::Component::ControlSocket,
            format!("cannot start the async runtime: {e}"),
        )
    })?;

    rt.block_on(async {
        let config = control_ble::dispatch::load_config()?;
        // Part A: the file-backed harness host. Part B replaces this with the control-socket client,
        // which is what gives the CLI access to the running tasks — until then the commands that
        // need them answer `DaemonDown` rather than answering from nothing.
        let host = Arc::new(control_ble::host::Host::for_harness(
            control_ble::dispatch::fleet_config_path(),
        ));
        let outcome = control_ble::dispatch::run(cli, &config, &host, None).await?;
        render_ble(&outcome, cli.json)
    })
}

fn render_radio(
    outcome: &control_radio::dispatch::Outcome,
    as_json: bool,
) -> control_radio::Result<()> {
    use control_radio::dispatch::Outcome;
    match outcome {
        Outcome::Status(report) => match serde_json::to_value(report.as_ref()) {
            Ok(v) if as_json => print_json(&v),
            Ok(v) => println!("radio_mode: {}\n{v:#}", report.radio_mode),
            Err(e) => eprintln!("proximosctl: could not render status: {e}"),
        },
        Outcome::Json(v) => print_json(v),
        Outcome::Text(t) => println!("{t}"),
    }
    Ok(())
}

/// The §5 output contract, the same one the radio half follows: plain mode *is* the pretty JSON
/// document, with `status` prefixing the mode line. There is no second hand-formatted table to keep
/// in sync.
fn render_ble(outcome: &control_ble::dispatch::Outcome, as_json: bool) -> control_ble::Result<()> {
    use control_ble::dispatch::Outcome;
    match outcome {
        Outcome::Status(report) => match serde_json::to_value(report.as_ref()) {
            Ok(v) if as_json => print_json(&v),
            Ok(v) => println!("ble_mode: {}\n{v:#}", report.ble_mode),
            Err(e) => eprintln!("proximosctl: could not render status: {e}"),
        },
        Outcome::Json(v) => print_json(v),
        Outcome::Text(t) => println!("{t}"),
    }
    Ok(())
}

fn print_json(v: &serde_json::Value) {
    match serde_json::to_string_pretty(v) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("proximosctl: could not render the result: {e}"),
    }
}

/// `ExitCode` is a `u8`; the taxonomy's codes all fit, but clamp explicitly rather than
/// truncating silently if a future variant is added carelessly.
fn exit_code(code: i32) -> ExitCode {
    match u8::try_from(code) {
        Ok(c) => ExitCode::from(c),
        Err(_) => ExitCode::FAILURE,
    }
}
