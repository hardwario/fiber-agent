//! `fiber-factory-reset` — the phase-2 factory-reset executor.
//!
//! Thin wrapper, deliberately: read the two state files, ask
//! [`decide_boot_action`] what they mean, and hand the production
//! [`ResetPlan`] to [`execute`]. Every decision worth testing lives in
//! `fiber_app::libs::factory_reset`, against a `tempfile` root; this file adds
//! only argument parsing, logging and an exit code.
//!
//! ## Why this is a separate binary
//!
//! `fiber_app` runs with `ProtectSystem=strict` and
//! `ReadWritePaths=/data/fiber`. It can arm the ledger — that is phase 1 —
//! and it can reboot, but it cannot wipe the rest of `/data`, and giving the
//! long-running agent that authority just to support one command would be the
//! wrong trade. This executor runs unsandboxed, once, at early boot, and exits.
//!
//! ## What the unit file (meta-fiber, a later task) has to guarantee
//!
//! * **`After=` / `Requires=` the `/data` mount.** If `/data` is not mounted
//!   when this runs, `/data` is an ordinary rootfs directory and the mount
//!   guardrail refuses — correctly, but the reset then does not happen at all.
//!   Ordering is the fix; retrying a destructive operation is not.
//! * **Before the services that own the data**, so nothing is holding files
//!   open in the tree being erased.
//! * **No sandboxing** (`ProtectSystem`, `ReadWritePaths`, `PrivateTmp`): this
//!   process needs the whole of `/data` writable, which is the entire reason it
//!   exists.
//! * **Nothing else may `Requires=` this unit.** It exits non-zero when a
//!   guardrail refuses (see below); that has to stay a visible failure in the
//!   journal, not something that takes other services down with it.
//!
//! ## Exit codes
//!
//! * `0` — nothing to do (no reset armed: every ordinary boot), a stale
//!   request discarded, a dry run, or a wipe that completed with or without
//!   per-entry errors.
//! * `1` — a wipe was armed and did **not** happen: a guardrail refused, the
//!   attempt cap was reached, or the attempt marker could not be written. The
//!   device still holds its data and someone needs to look at it. The ledger is
//!   deliberately left armed in this case, so the request stays visible and is
//!   retried on the next boot rather than being silently forgotten — see
//!   `should_clear_ledger`. The one case that still loses the request is a
//!   `/data` that never mounted at all, because the ledger lives on that
//!   partition; `execute`'s docs spell out why and what bounds it.
//!
//! ## What this binary deliberately does not do
//!
//! It never applies the request's `post_action`. Powering the device off is a
//! separate step for whatever runs after the wipe — the executor runs no
//! external programs at all, and `systemctl poweroff` would be one. The
//! `post_action` is copied into the result file for that step to read.

use std::path::Path;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;

use fiber_app::libs::factory_reset::{
    decide_boot_action, dry_run, execute, should_clear_ledger, BootAction, ResetPlan, ResetRequest,
    ResetStatus, WipeAttempt, LEDGER_PATH,
};

#[derive(Parser)]
#[command(
    name = "fiber-factory-reset",
    version,
    about = "Phase-2 factory reset executor: wipes /data when the phase-1 ledger says to"
)]
struct Cli {
    /// Print what a real run would remove, preserve and recreate, then exit.
    /// Touches nothing on disk — safe on a live device.
    #[arg(long)]
    dry_run: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let plan = ResetPlan::production();
    let ledger_path = Path::new(LEDGER_PATH);

    if cli.dry_run {
        print!("{}", dry_run(&plan).render());
        match ResetRequest::read(ledger_path) {
            Some(request) => println!(
                "armed ledger: request_id={} requested_by={} reason={} post_action={:?}",
                request.request_id, request.requested_by, request.reason, request.post_action
            ),
            None => println!("armed ledger: none at {}", ledger_path.display()),
        }
        match WipeAttempt::read(&plan.marker_path()) {
            Some(marker) => println!(
                "in-progress marker: request_id={} attempts={}",
                marker.request_id, marker.attempts
            ),
            None => println!("in-progress marker: none"),
        }
        return ExitCode::SUCCESS;
    }

    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let action = decide_boot_action(
        ResetRequest::read(ledger_path),
        WipeAttempt::read(&plan.marker_path()),
        now_unix,
    );

    match action {
        // The overwhelmingly common case. Stay quiet and get out of the way of
        // the boot.
        BootAction::Nothing => ExitCode::SUCCESS,

        BootAction::StaleRequest(request) => {
            eprintln!(
                "[factory_reset] WARN: discarding a factory-reset request from {} \
                 (request_id={}, requested_by={}) — too old to act on, and no wipe was started",
                request.requested_at_unix, request.request_id, request.requested_by
            );
            ResetRequest::clear(ledger_path);
            ExitCode::SUCCESS
        }

        BootAction::Wipe(request) => {
            let outcome = execute(&plan, &request);

            // Only a wipe that actually happened un-arms the request. A
            // refusal must leave the ledger alone: it is the only durable
            // record that an operator signed a destructive command, and
            // clearing it would mean the command was neither performed nor
            // remembered nor ever retried. The wipe itself removes
            // /data/fiber, and the ledger with it, so on the ordinary path
            // this call is a no-op.
            if should_clear_ledger(outcome.status) {
                ResetRequest::clear(ledger_path);
            } else {
                eprintln!(
                    "[factory_reset] WARN: leaving the armed ledger at {} in place — this reset \
                     did not happen and must stay visible and retryable",
                    ledger_path.display()
                );
            }

            eprintln!("[factory_reset] {}", outcome.summary());
            match outcome.status {
                ResetStatus::Completed | ResetStatus::CompletedWithErrors => ExitCode::SUCCESS,
                ResetStatus::Failed => ExitCode::FAILURE,
            }
        }
    }
}
